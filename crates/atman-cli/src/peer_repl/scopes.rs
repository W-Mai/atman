//! Session runtimes retained independently of view selection.

use super::*;
use atman_runtime::flow_runtime::catalog::SessionCatalog;

pub(super) struct SessionSeed {
    pub config: ConfigHub,
    pub project: Option<atman_runtime::storage::ProjectContext>,
    pub prompt_resolver: Option<Arc<dyn atman_runtime::rendezvous::PromptResolver>>,
}

impl From<&Attachment> for SessionSeed {
    fn from(scope: &Attachment) -> Self {
        Self {
            config: scope.config.clone(),
            project: scope.descriptor.project.clone(),
            prompt_resolver: scope.executor.tool_ctx.prompt_resolver.clone(),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum CreationMode {
    Interactive,
    Deferred,
}

pub(super) struct Scopes {
    pub(super) root: std::path::PathBuf,
    host: RuntimeHost,
    permissions: Option<Arc<PermissionClients>>,
    prompts: Arc<prompts::Prompts>,
    loaded: tokio::sync::Mutex<HashMap<Uuid, Attachment>>,
    pub catalog: SessionCatalog,
}

impl Scopes {
    pub fn new(root: &Path, initial: Attachment) -> Self {
        let mut scopes = Self::empty(
            root,
            initial.host.clone(),
            initial.permissions.clone(),
            initial.prompts.clone(),
        );
        scopes
            .loaded
            .get_mut()
            .insert(initial.session.id(), initial);
        scopes
    }

    pub fn empty(
        root: &Path,
        host: RuntimeHost,
        permissions: Option<Arc<PermissionClients>>,
        prompts: Arc<prompts::Prompts>,
    ) -> Self {
        Self {
            root: root.to_owned(),
            catalog: SessionCatalog::new(Some(host.clone()), Some(root.to_owned())),
            host,
            permissions,
            prompts,
            loaded: Default::default(),
        }
    }

    pub async fn open(&self, id: Uuid) -> Result<Attachment> {
        // Only navigation loads serialize. Existing views retain their own immutable scope.
        let mut loaded = self.loaded.lock().await;
        if let Some(scope) = loaded.get(&id) {
            return Ok(scope.clone());
        }
        let scope = Attachment::open(
            &self.root,
            id,
            self.host.clone(),
            self.permissions.clone(),
            self.prompts.clone(),
        )
        .await?;
        loaded.insert(id, scope.clone());
        Ok(scope)
    }

    pub async fn startup_recent(
        &self,
        source: &Attachment,
        limit: usize,
    ) -> Result<Vec<atman_tui::app::StartupSessionEntry>> {
        let ids = self.catalog.ids().await?;
        let root = self.root.clone();
        let current = source.session.id();
        let project = source
            .descriptor
            .project
            .as_ref()
            .map(|p| p.root().to_owned());
        let rows = tokio::task::spawn_blocking(move || {
            let mut rows = Vec::new();
            for id in ids.into_iter().filter(|id| *id != current) {
                let Ok(read) = RuntimeStore::inspect_session(&root, id) else {
                    continue;
                };
                if read.projection.default_flow.is_none()
                    || read
                        .projection
                        .descriptor
                        .project
                        .as_ref()
                        .map(|p| p.root())
                        != project.as_deref()
                {
                    continue;
                }
                rows.push((read.activity_time(), id));
            }
            rows.sort_unstable_by(|left, right| right.cmp(left));
            rows
        })
        .await?;
        let mut recent = Vec::new();
        for (activity, id) in rows {
            if recent.len() == limit {
                break;
            }
            let Ok(view) = self.catalog.overview(id).await else {
                continue;
            };
            let Some(instance_id) = view.summary.default_flow else {
                continue;
            };
            let age = activity
                .map(|time| {
                    crate::format_age(
                        std::time::SystemTime::now()
                            .duration_since(time)
                            .unwrap_or_default()
                            .as_secs(),
                    )
                })
                .unwrap_or_else(|| "Recorded".into());
            let activity = match view.summary.flow_count {
                Some(1) => format!("{age}  ·  1 flow"),
                Some(count) => format!("{age}  ·  {count} flows"),
                None => age,
            };
            recent.push(atman_tui::app::StartupSessionEntry {
                target: atman_tui::app::StartupSessionTarget::Flow(
                    atman_runtime::flow_store::FlowRef {
                        session_id: id.into(),
                        instance_id,
                    },
                ),
                short_id: id.to_string().chars().take(8).collect(),
                goal: view.default_goal.or(view.summary.title),
                project: view
                    .summary
                    .project_root
                    .as_deref()
                    .map(crate::short_project_path),
                activity,
            });
        }
        Ok(recent)
    }

    /// Accepted creation outlives its caller and requires no existing session.
    pub async fn create_session(
        self: &Arc<Self>,
        source: SessionSeed,
        request: Uuid,
        mode: CreationMode,
    ) -> Result<atman_runtime::flow_store::FlowRef> {
        let scopes = self.clone();
        tokio::spawn(async move { scopes.create_session_owned(source, request, mode).await })
            .await?
    }

    async fn create_session_owned(
        &self,
        source: SessionSeed,
        request: Uuid,
        mode: CreationMode,
    ) -> Result<atman_runtime::flow_store::FlowRef> {
        use atman_runtime::flow_runtime::launch::EntryExecution;
        use atman_runtime::flow_store::{
            FlowDescriptor, FlowRef,
            launch::{EntryBinding, EntryRequest, LaunchInput},
        };
        ensure!(
            !request.is_nil(),
            "session creation requires a request identity"
        );
        let address = FlowRef {
            session_id: request.into(),
            instance_id: atman_rt::FlowInstanceId(request.into()),
        };
        let descriptor = SessionDescriptor {
            id: request,
            title: "New session".into(),
            project: source.project,
            configuration: SessionConfiguration::Atman(source.config.source()),
        };
        let mut loaded = self.loaded.lock().await;
        let prepared = if loaded.contains_key(&request) {
            None
        } else {
            let root = self.root.clone();
            let expected = descriptor.clone();
            let exists = tokio::task::spawn_blocking(move || {
                match RuntimeStore::inspect_session(&root, request) {
                    Ok(existing) if existing.projection.descriptor == expected => Ok(true),
                    Ok(_) => Err(std::io::Error::other(
                        "session creation identity conflicts with another scope",
                    )),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                    Err(error) => Err(error),
                }
            })
            .await??;
            if exists {
                None
            } else {
                Some(prepare_new_session(&source.config, &descriptor)?)
            }
        };
        let session = match self.host.session(request) {
            Ok(session) => session,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let root = self.root.clone();
                let expected = descriptor.clone();
                let session = tokio::task::spawn_blocking(move || {
                    let store = RuntimeStore::open(&root)?;
                    let session = match store.create_session(expected.clone()) {
                        Ok(session) => session,
                        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                            store.open_session(expected.id)?
                        }
                        Err(error) => return Err(error),
                    };
                    if session.projection().descriptor != expected {
                        return Err(std::io::Error::other(
                            "session creation identity conflicts with another scope",
                        ));
                    }
                    Ok(session)
                })
                .await??;
                self.host.attach_session_with_redactor(
                    session,
                    atman_daemon::bootstrap::build_redactor(Some(source.config.config_dir())),
                )?
            }
            Err(error) => return Err(error.into()),
        };
        let catalog = session.snapshot().await?;
        ensure!(
            catalog.catalog.descriptor == descriptor,
            "session creation identity conflicts with another scope"
        );
        if catalog
            .catalog
            .flows
            .iter()
            .any(|row| row.id == address.instance_id)
        {
            let worker = session.open_flow(address.instance_id.clone()).await?;
            if worker.launch_record().await?.is_some() {
                return Ok(address);
            }
        }
        let NewSessionEntry {
            entry,
            start,
            trust,
            authority,
        } = match prepared {
            Some(prepared) => prepared,
            None => prepare_new_session(&source.config, &descriptor)?,
        };
        let descriptor = FlowDescriptor {
            address: address.clone(),
            creation_request: request,
            created_by: None,
            content: entry.into(),
        };
        let input = LaunchInput {
            workspace: None,
            initial_goal: None,
            inherited_context: vec![],
            result_route: None,
            request: EntryRequest {
                request_id: request,
                sender: SenderIdentity::Host(USER),
                recipient: address.clone(),
                arguments: Default::default(),
                invocation: Default::default(),
                start,
            },
            execution: Some(EntryBinding {
                user_routes: true,
                session_start: true,
                first_turn: atman_rt::TurnId(request.into()),
                first_run: atman_rt::RunId(request.into()),
                authority: authority.clone(),
            }),
        };
        if let CreationMode::Interactive = mode {
            let mut scope = match loaded.get(&request) {
                Some(scope) => scope.clone(),
                None => {
                    Attachment::open(
                        &self.root,
                        request,
                        self.host.clone(),
                        self.permissions.clone(),
                        self.prompts.clone(),
                    )
                    .await?
                }
            };
            scope.executor.tool_ctx.prompt_resolver = source.prompt_resolver;
            // Retain the scoped prompt transport before scheduling startup work.
            loaded.insert(request, scope.clone());
            self.host
                .launch_entry(
                    descriptor,
                    input,
                    EntryExecution {
                        executor: scope.executor.clone(),
                        authority,
                        trust,
                        yield_interval: YIELD,
                        catalog: catalog.cursor,
                    },
                    |_, _| Ok(()),
                )
                .await?;
        } else {
            self.host
                .launch_flow(descriptor, input, |_, _| Ok(()))
                .await?;
        }
        Ok(address)
    }
}

struct NewSessionEntry {
    entry: atman_runtime::flow_store::EntrySnapshot,
    start: atman_runtime::flow_store::launch::EntryStart,
    trust: atman_runtime::trust::TrustConfig,
    authority: EffectiveAuthority,
}

fn prepare_new_session(
    config: &ConfigHub,
    descriptor: &SessionDescriptor,
) -> Result<NewSessionEntry> {
    use atman_runtime::flow_store::{EntrySnapshot, launch::EntryStart};
    let (program, name, _, _) = crate::resolve_slash_command_from(
        "/agent",
        config.config_dir(),
        descriptor.project.as_ref().map(|project| project.root()),
    )?;
    let flow = program
        .entry_flow(&name)
        .and_then(|id| program.flow(&id))
        .context("default agent entry is unavailable")?;
    ensure!(
        atman_runtime::validate::invocation_message_parameter(flow)?.is_some(),
        "default agent entry requires an invocation user_message parameter"
    );
    let trust = config.trust_config()?;
    let authority = EffectiveAuthority::root(
        &trust,
        contract_allows_shell(flow.contract.as_ref()),
        descriptor
            .project
            .as_ref()
            .map(|project| project.root().to_path_buf()),
    );
    let entry = EntrySnapshot {
        name,
        path: program
            .entry_path()
            .context("default source path is unavailable")?
            .to_owned(),
        bundle: program.revision_bundle(),
    };
    let startup = config.config_dir().join("on_session_start.at");
    let start = if startup.try_exists()? {
        let program =
            atman_runtime::source_program::load_program(&startup, &descriptor.source_roots())?;
        match program.entry_file().flows.first() {
            Some(flow) => EntryStart::Command {
                entry: Box::new(EntrySnapshot {
                    name: flow.name.name.clone(),
                    path: startup,
                    bundle: program.revision_bundle(),
                }),
                arguments: Default::default(),
                invocation: Default::default(),
            },
            None => EntryStart::Waiting,
        }
    } else {
        EntryStart::Waiting
    };
    Ok(NewSessionEntry {
        entry,
        start,
        trust,
        authority,
    })
}
