//! Daemon-owned persistent session catalogs. Reading or selecting a flow never installs a scheduler.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::{Context, Result};
use atman_proto::{JsonRpcError, flows::*, methods};
use atman_rt::FlowInstanceId;
use atman_runtime::flow_runtime::{
    FlowWorker,
    access::PeerControl,
    host::{HostServices, RuntimeHost, SessionHandle},
};
use atman_runtime::flow_store::{FlowRef, RuntimeStore, SessionDescriptor};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use uuid::Uuid;

use crate::DaemonState;

pub(crate) mod events;
mod execution;

type CatalogSlot = Arc<OnceCell<SessionHandle>>;

pub(crate) struct SessionService {
    data_dir: PathBuf,
    host: RuntimeHost,
    _permission_clients: Arc<atman_runtime::permission::PermissionClients>,
    catalogs: Mutex<HashMap<Uuid, CatalogSlot>>,
    executors: Mutex<HashMap<Uuid, Arc<OnceCell<atman_runtime::Executor>>>>,
}

impl SessionService {
    fn new(data_dir: PathBuf, redactor: Option<Arc<atman_runtime::redact::Redactor>>) -> Self {
        let permission_clients = Arc::new(atman_runtime::permission::PermissionClients::default());
        Self {
            data_dir,
            host: RuntimeHost::with_services(HostServices {
                redactor,
                permission_clients: Arc::downgrade(&permission_clients),
            }),
            _permission_clients: permission_clients,
            catalogs: Default::default(),
            executors: Default::default(),
        }
    }

    async fn attach(&self, id: Uuid, create: Option<SessionDescriptor>) -> Result<SessionHandle> {
        anyhow::ensure!(!id.is_nil(), "session identity must not be nil");
        let slot = self.catalogs.lock().unwrap().entry(id).or_default().clone();
        let handle = slot
            .get_or_try_init(|| async {
                let data = self.data_dir.clone();
                let requested = create.clone();
                let store = tokio::task::spawn_blocking(move || -> Result<_> {
                    let root = RuntimeStore::open(&data)?;
                    let store = match requested {
                        Some(descriptor) => match root.create_session(descriptor) {
                            Ok(store) => store,
                            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                                root.open_session(id)?
                            }
                            Err(error) => return Err(error.into()),
                        },
                        None => root.open_session(id)?,
                    };
                    Ok(store)
                })
                .await??;
                if let Some(expected) = &create {
                    anyhow::ensure!(
                        &store.projection().descriptor == expected,
                        "session creation conflicts with its recorded descriptor"
                    );
                }
                Ok::<_, anyhow::Error>(self.host.attach_session(store)?)
            })
            .await?
            .clone();
        if let Some(expected) = create {
            anyhow::ensure!(
                handle.snapshot().await?.catalog.descriptor == expected,
                "session creation conflicts with its recorded descriptor"
            );
        }
        Ok(handle)
    }

    fn session(&self, id: Uuid) -> Result<SessionHandle> {
        self.host
            .session(id)
            .context("open the persistent session before accessing its flows")
    }
}

pub(crate) fn handles(method: &str) -> bool {
    matches!(
        method,
        methods::SESSION_CREATE
            | methods::SESSION_OPEN
            | methods::SESSION_FLOWS
            | methods::SESSION_SELECT_FLOW
            | methods::FLOW_GET
            | methods::FLOW_CONTROL
            | methods::FLOW_LAUNCH
            | methods::FLOW_BIND
            | methods::FLOW_INVOKE
            | methods::FLOW_OUTPUT
            | methods::FLOW_EVENTS
    )
}

pub(crate) async fn dispatch(
    state: Arc<DaemonState>,
    method: String,
    params: Value,
    principal: &str,
) -> Result<Value, JsonRpcError> {
    authorize_operator(principal)?;
    let request = Request::parse(&method, params)?;
    // Accepted management work survives a disconnected RPC waiter.
    tokio::spawn(async move { dispatch_owned(&state, request).await })
        .await
        .map_err(|error| JsonRpcError::internal(error.to_string()))?
        .map_err(|error| JsonRpcError::application(error.to_string()))
}

fn authorize_operator(principal: &str) -> Result<(), JsonRpcError> {
    // Both authenticated transports represent this one local operator. Unknown principals must not acquire storage ownership.
    if principal != crate::state::LOCAL_OPERATOR {
        return Err(JsonRpcError::application(
            "permission denied for persistent session management",
        ));
    }
    Ok(())
}

pub(crate) async fn context(
    state: &DaemonState,
    flow: FlowAddress,
    principal: &str,
) -> Result<Arc<atman_runtime::Session>, JsonRpcError> {
    Ok(worker(state, flow, principal).await?.context())
}

pub(crate) async fn worker(
    state: &DaemonState,
    flow: FlowAddress,
    principal: &str,
) -> Result<FlowWorker, JsonRpcError> {
    authorize_operator(principal)?;
    if flow.session_id.0.is_nil() || flow.instance_id.is_nil() {
        return Err(JsonRpcError::invalid_params(
            "session and flow identities must not be nil",
        ));
    }
    let service = state.session_service.get().ok_or_else(|| {
        JsonRpcError::application("open the persistent session before accessing its flows")
    })?;
    service
        .host
        .flow(&core_address(flow))
        .await
        .map_err(|error| JsonRpcError::application(error.to_string()))
}

enum Request {
    Create(CreateSessionRequest),
    Open(SessionRequest),
    List(ListFlowsRequest),
    Select(FlowRequest),
    Get(FlowRequest),
    Control(ControlFlowRequest),
    Launch(LaunchFlowRequest),
    Bind(FlowRequest),
    Invoke(InvokeFlowRequest),
    Output(ReadFlowOutputRequest),
    Events(ReadFlowEventsRequest),
}

impl Request {
    fn parse(method: &str, params: Value) -> Result<Self, JsonRpcError> {
        let request = match method {
            methods::SESSION_CREATE => Self::Create(decode(params)?),
            methods::SESSION_OPEN => Self::Open(decode(params)?),
            methods::SESSION_FLOWS => Self::List(decode(params)?),
            methods::SESSION_SELECT_FLOW => Self::Select(decode(params)?),
            methods::FLOW_GET => Self::Get(decode(params)?),
            methods::FLOW_CONTROL => Self::Control(decode(params)?),
            methods::FLOW_LAUNCH => Self::Launch(decode(params)?),
            methods::FLOW_BIND => Self::Bind(decode(params)?),
            methods::FLOW_INVOKE => Self::Invoke(decode(params)?),
            methods::FLOW_OUTPUT => Self::Output(decode(params)?),
            methods::FLOW_EVENTS => Self::Events(decode(params)?),
            _ => return Err(JsonRpcError::method_not_found(method)),
        };
        let (session, instance) = match &request {
            Self::Create(request) => {
                if request.title.trim().is_empty() {
                    return Err(JsonRpcError::invalid_params(
                        "session title must not be empty",
                    ));
                }
                (request.session_id.0, None)
            }
            Self::Open(request) => (request.session_id.0, None),
            Self::List(request) => {
                if !(1..=50).contains(&request.limit) {
                    return Err(JsonRpcError::invalid_params(
                        "instance page limit must be between 1 and 50",
                    ));
                }
                (request.session_id.0, request.before)
            }
            Self::Select(request) | Self::Get(request) | Self::Bind(request) => {
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::Control(request) => (request.flow.session_id.0, Some(request.flow.instance_id)),
            Self::Launch(request) => {
                if request.request_id.is_nil()
                    || request.entry.trim().is_empty()
                    || !std::path::Path::new(&request.path).is_absolute()
                {
                    return Err(JsonRpcError::invalid_params(
                        "launch requires a request UUID, an absolute source path, and an entry name",
                    ));
                }
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::Invoke(request) => {
                if request.request_id.is_nil() {
                    return Err(JsonRpcError::invalid_params(
                        "request identity must not be nil",
                    ));
                }
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::Output(request) => {
                if !(1..=50).contains(&request.limit) {
                    return Err(JsonRpcError::invalid_params(
                        "output page limit must be between 1 and 50",
                    ));
                }
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
            Self::Events(request) => {
                if !(1..=128).contains(&request.limit) {
                    return Err(JsonRpcError::invalid_params(
                        "event page limit must be between 1 and 128",
                    ));
                }
                (request.flow.session_id.0, Some(request.flow.instance_id))
            }
        };
        if session.is_nil() || instance.is_some_and(|id| id.is_nil()) {
            return Err(JsonRpcError::invalid_params(
                "session and flow identities must not be nil",
            ));
        }
        Ok(request)
    }
}

fn service(state: &DaemonState) -> &SessionService {
    state.session_service.get_or_init(|| {
        let config_dir = state
            .launcher()
            .and_then(|launcher| launcher.config_dir.clone());
        SessionService::new(
            state.data_dir().to_path_buf(),
            crate::bootstrap::build_redactor(config_dir.as_deref()),
        )
    })
}

pub(crate) async fn shutdown(state: &DaemonState) -> Result<()> {
    // Initialize even an unused service so an already accepted RPC cannot install a fresh host after shutdown.
    let service = service(state);
    let result = service.host.shutdown().await;
    service.executors.lock().unwrap().clear();
    result.map_err(Into::into)
}

async fn dispatch_owned(state: &Arc<DaemonState>, request: Request) -> Result<Value> {
    anyhow::ensure!(!state.is_stopping(), "daemon is stopping");
    let service = service(state);
    match request {
        Request::Launch(request) => service.launch(state, request).await,
        Request::Bind(request) => service.bind(state, request).await,
        Request::Invoke(request) => service.invoke(request).await,
        Request::Output(request) => service.output(request).await,
        Request::Events(request) => service.events(request).await,
        Request::Create(request) => {
            let config_dir = state
                .launcher()
                .and_then(|launcher| launcher.config_dir.clone());
            let hub = crate::bootstrap::resolve_config_hub(config_dir.as_deref())?;
            let project = request
                .project_root
                .map(|root| {
                    atman_runtime::storage::ProjectContext::resolve(
                        &hub,
                        std::path::Path::new(&root),
                        state.data_dir(),
                    )
                })
                .transpose()?;
            let descriptor = SessionDescriptor {
                id: request.session_id.0,
                title: request.title,
                configuration: atman_runtime::flow_store::SessionConfiguration::Atman(hub.source()),
                project,
            };
            describe_session(&service.attach(descriptor.id, Some(descriptor)).await?).await
        }
        Request::Open(request) => {
            describe_session(&service.attach(request.session_id.0, None).await?).await
        }
        Request::List(request) => {
            let page = service
                .session(request.session_id.0)?
                .instances(
                    request.before.map(|id| FlowInstanceId(id.into())),
                    request.limit,
                    |_| Ok(()),
                )
                .await?;
            let instances = page.instances.into_iter().map(|row| json!({
                "flow": address(&row.address), "entry": row.entry,
                "execution": row.state.as_ref().map(execution), "unavailable": row.unavailable,
            })).collect::<Vec<_>>();
            Ok(
                json!({"instances": instances, "next": page.next, "total": page.total, "catalog": page.catalog}),
            )
        }
        Request::Select(request) => {
            let flow = core_address(request.flow);
            let session = service.session(flow.session_id.0)?;
            session.select_default(flow.instance_id).await?;
            describe_session(&session).await
        }
        Request::Get(request) => {
            let flow = core_address(request.flow);
            let worker = service.host.flow(&flow).await?;
            let view = worker.status().await?;
            let scheduler = service
                .session(flow.session_id.0)?
                .scheduling_status(flow.instance_id.clone())
                .await?;
            let scheduler = scheduler.map(|status| scheduling(&status.borrow()));
            Ok(
                json!({"flow": address(&flow), "execution": execution(&view.execution), "goal": view.goal, "last_completion": view.completion, "cursor": view.cursor, "scheduler": scheduler}),
            )
        }
        Request::Control(request) => {
            let worker = service.host.flow(&core_address(request.flow)).await?;
            let control = match request.action {
                FlowControl::Pause => PeerControl::Pause,
                FlowControl::Resume => PeerControl::Resume,
                FlowControl::CancelTurn => PeerControl::CancelTurn,
                FlowControl::Recover => PeerControl::Recover,
            };
            // Stable across process restarts, distinct from peer-generated controller keys.
            let user_pause = Uuid::from_bytes(*b"atman-user-pause").into();
            let receipt = worker
                .control(request.expected_revision, user_pause, control, |_| Ok(()))
                .await?;
            Ok(json!({"receipt": receipt}))
        }
    }
}

fn decode<T: DeserializeOwned>(params: Value) -> Result<T, JsonRpcError> {
    serde_json::from_value(params).map_err(|error| JsonRpcError::invalid_params(error.to_string()))
}

async fn describe_session(session: &SessionHandle) -> Result<Value> {
    let snapshot = session.snapshot().await?;
    Ok(json!({
        "session_id": session.id(), "title": snapshot.catalog.descriptor.title,
        "project_root": snapshot.catalog.descriptor.project.as_ref().map(|project| project.root()),
        "default_flow": snapshot.catalog.default_flow, "flow_count": snapshot.catalog.flows.len(),
        "catalog": snapshot.cursor,
    }))
}

fn core_address(address: FlowAddress) -> FlowRef {
    FlowRef {
        session_id: address.session_id.0.into(),
        instance_id: FlowInstanceId(address.instance_id.into()),
    }
}

fn address(flow: &FlowRef) -> FlowAddress {
    FlowAddress {
        session_id: atman_proto::SessionId(flow.session_id.0),
        instance_id: flow.instance_id.0.0,
    }
}

fn execution(state: &atman_rt::FlowInstance<atman_runtime::event::AtmanUuid>) -> Value {
    json!({"revision": state.revision(), "state": state.state(), "pause_count": state.pauses().len(), "can_start_turn": state.can_start_turn()})
}

fn scheduling(status: &atman_runtime::flow_runtime::scheduler::SchedulingStatus) -> Value {
    use atman_runtime::flow_runtime::scheduler::SchedulingStatus;
    match status {
        SchedulingStatus::Running => json!({"status": "running"}),
        SchedulingStatus::Stopped => json!({"status": "stopped"}),
        SchedulingStatus::Failed(error) => json!({"status": "failed", "error": error}),
    }
}
