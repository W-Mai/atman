//! Private RPC observation and commands over the native terminal's existing runtime.

use std::{io, path::PathBuf, sync::Arc};

use atman_daemon::host_directory::{HostPublication, HostRecord, HostRegistration, HostRole};
use atman_daemon::{DaemonState, EmbeddedRpcServer, SharedHostServices, SharedPrompts};
use atman_runtime::{flow_runtime::host::RuntimeHost, permission::PermissionClients};
use serde_json::Value;
use uuid::Uuid;

use super::prompts::Prompts;

pub(super) struct RpcHost {
    pub state: Arc<DaemonState>,
    transport: Arc<std::sync::Mutex<Option<Transport>>>,
    closing: Arc<tokio::sync::Mutex<()>>,
    closed: Arc<std::sync::OnceLock<Result<(), String>>>,
}

struct Transport {
    registration: Arc<HostRegistration>,
    publication: HostPublication,
    server: EmbeddedRpcServer,
    maintenance: atman_daemon::host_maintenance::HostMaintenance,
}

pub(super) fn state(
    data: PathBuf,
    host: RuntimeHost,
    permissions: Arc<PermissionClients>,
    prompts: Arc<Prompts>,
) -> Arc<DaemonState> {
    let generation = host.generation();
    let original_prompts = prompts.clone();
    let original_host = host.clone();
    let shared_prompts = SharedPrompts::new(prompts, move |id, value, session| {
        let prompts = original_prompts.clone();
        let host = original_host.clone();
        Box::pin(async move {
            if let Some(result) = prompts.reply_in_session(id, &value, session)? {
                return Ok(result);
            }
            answer_native_interaction(&host, &prompts, id, value, session).await
        })
    });
    Arc::new(DaemonState::with_shared_host(
        data,
        generation.to_string(),
        SharedHostServices {
            host,
            permissions,
            operator: super::USER,
            prompts: Some(shared_prompts),
        },
    ))
}

pub(super) async fn start(
    data: PathBuf,
    host: RuntimeHost,
    state: Arc<DaemonState>,
) -> anyhow::Result<RpcHost> {
    let token = tokio::task::spawn_blocking(|| {
        let path = atman_daemon::config::default_config_path()?;
        Ok::<_, anyhow::Error>(atman_daemon::config::DaemonConfig::load_or_init(&path)?.auth_token)
    })
    .await??;
    let generation = host.generation();
    state.configure_host_routing(token.clone())?;
    state.prepare_peer_transport().await?;
    let server = EmbeddedRpcServer::bind(state.clone(), token).await?;
    let record = HostRecord {
        generation,
        process_id: std::process::id(),
        role: HostRole::Tui,
        presentation: atman_proto::hosts::HostPresentation::Attached,
        endpoint: server.endpoint(),
        sessions: host.sessions(),
    };
    let registration = Arc::new(
        tokio::task::spawn_blocking(move || HostRegistration::register(&data, record)).await??,
    );
    let publication = HostPublication::start(registration.clone(), host);
    let maintenance = atman_daemon::host_maintenance::HostMaintenance::start(&state);
    Ok(RpcHost {
        state,
        transport: Arc::new(std::sync::Mutex::new(Some(Transport {
            registration,
            publication,
            server,
            maintenance,
        }))),
        closing: Default::default(),
        closed: Default::default(),
    })
}

impl RpcHost {
    pub async fn claim(&self, session: Uuid) -> io::Result<()> {
        self.state
            .claim_session(session, tokio_util::sync::CancellationToken::new())
            .await
            .map(|_| ())
    }
    pub async fn retire(&self) -> io::Result<()> {
        self.state
            .set_presentation(atman_proto::hosts::HostPresentation::Detached);
        let registration = self
            .transport
            .lock()
            .unwrap()
            .as_ref()
            .map(|transport| transport.registration.clone());
        let Some(registration) = registration else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || {
            registration.publish_presentation(atman_proto::hosts::HostPresentation::Detached)
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Listener shutdown releases observers and discovery, without stopping the runtime owner.
    pub async fn shutdown(&self) -> io::Result<()> {
        let closing = self.closing.clone().lock_owned().await;
        if let Some(result) = self.closed.get() {
            return result.clone().map_err(io::Error::other);
        }
        let transport = self.transport.clone();
        let closed = self.closed.clone();
        tokio::spawn(async move {
            let _closing = closing;
            let transport = transport.lock().unwrap().take();
            let result = match transport {
                None => Ok(()),
                Some(Transport {
                    registration,
                    publication,
                    server,
                    maintenance,
                }) => {
                    let maintained = maintenance.shutdown().await;
                    let published = publication.shutdown().await;
                    let result = server.shutdown().await;
                    let retired = tokio::task::spawn_blocking(move || drop(registration))
                        .await
                        .map_err(io::Error::other);
                    maintained.and(published).and(result).and(retired)
                }
            }
            .map_err(|error| error.to_string());
            let _ = closed.set(result.clone());
            result
        })
        .await
        .map_err(io::Error::other)?
        .map_err(io::Error::other)
    }
}

pub(super) async fn answer_native_interaction(
    host: &RuntimeHost,
    prompts: &Prompts,
    id: Uuid,
    value: Value,
    session: Option<Uuid>,
) -> io::Result<bool> {
    let Some(session) = session else {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "native interaction replies require their original Session",
        ));
    };
    let source = match host.session(session) {
        Ok(source) => source,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let form_id = id.to_string();
    let mut registries = prompts.native_forms();
    let mut target = None;
    let mut compact = None;
    for observation in source.observe().await?.loaded {
        let Some(worker) = source.loaded_flow(&observation.execution.address().instance_id) else {
            continue;
        };
        let context = worker.context();
        let forms = context.forms();
        if !registries
            .iter()
            .any(|registry| Arc::ptr_eq(registry, &forms))
        {
            registries.push(forms);
        }
        let inbox = context.deferred_form_inbox();
        for forms in &registries {
            if !forms.owns_form(&form_id, &inbox) {
                continue;
            }
            if target.is_some() || compact.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "interaction identity is ambiguous in this Session",
                ));
            }
            target = Some((forms.clone(), inbox.clone()));
        }
        let reviews = context.compact_reviews();
        if reviews
            .list_pending()
            .is_some_and(|review| review.review_id == form_id)
        {
            if target.is_some() || compact.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "interaction identity is ambiguous in this Session",
                ));
            }
            compact = Some((context, reviews));
        }
    }
    if let Some((forms, inbox)) = target {
        let submission: atman_runtime::form::FormSubmission =
            serde_json::from_value(value).map_err(io::Error::other)?;
        if !forms.owns_form(&form_id, &inbox) {
            return Ok(false);
        }
        return forms.submit(&form_id, submission).await;
    }
    if let Some((context, reviews)) = compact {
        let decision: CompactDecision = serde_json::from_value(value).map_err(io::Error::other)?;
        return context
            .apply_instance_effect(move || reviews.decide(&form_id, decision.into()))
            .await;
    }
    Ok(false)
}

#[derive(serde::Deserialize)]
#[serde(tag = "decision", rename_all = "snake_case", deny_unknown_fields)]
enum CompactDecision {
    AcceptAsIs,
    AcceptEdited { summary: String },
    Reject,
}

impl From<CompactDecision> for atman_runtime::CompactReviewDecision {
    fn from(decision: CompactDecision) -> Self {
        match decision {
            CompactDecision::AcceptAsIs => Self::AcceptAsIs,
            CompactDecision::AcceptEdited { summary } => Self::AcceptEdited { summary },
            CompactDecision::Reject => Self::Reject,
        }
    }
}
