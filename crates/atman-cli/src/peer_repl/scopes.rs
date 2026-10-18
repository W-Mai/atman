//! Session runtimes retained independently of view selection.

use super::*;
use atman_runtime::flow_runtime::catalog::SessionCatalog;

pub(super) struct Scopes {
    root: std::path::PathBuf,
    host: RuntimeHost,
    permissions: Option<Arc<PermissionClients>>,
    prompts: Arc<prompts::Prompts>,
    loaded: tokio::sync::Mutex<HashMap<Uuid, Attachment>>,
    pub catalog: SessionCatalog,
}

impl Scopes {
    pub fn new(root: &Path, initial: Attachment) -> Self {
        Self {
            root: root.to_owned(),
            host: initial.host.clone(),
            permissions: initial.permissions.clone(),
            prompts: initial.prompts.clone(),
            catalog: SessionCatalog::new(Some(initial.host.clone()), Some(root.to_owned())),
            loaded: tokio::sync::Mutex::new(HashMap::from([(initial.session.id(), initial)])),
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

    /// Accepted creation outlives its view. Configuration and project come from the requesting scope.
    pub async fn create_session(
        self: &Arc<Self>,
        source: Attachment,
        request: Uuid,
    ) -> Result<atman_runtime::flow_store::FlowRef> {
        let scopes = self.clone();
        tokio::spawn(async move { scopes.create_session_owned(source, request).await }).await?
    }

    async fn create_session_owned(
        &self,
        source: Attachment,
        request: Uuid,
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
            project: source.descriptor.project.clone(),
            configuration: source.descriptor.configuration.clone(),
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
                Some(prepare_new_session(&source)?)
            }
        };
        let mut scope = if let Some(scope) = loaded.get(&request) {
            scope.clone()
        } else {
            let root = self.root.clone();
            let expected = descriptor.clone();
            tokio::task::spawn_blocking(move || {
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
                Ok(())
            })
            .await??;
            let scope = Attachment::open(
                &self.root,
                request,
                self.host.clone(),
                self.permissions.clone(),
                self.prompts.clone(),
            )
            .await?;
            loaded.insert(request, scope.clone());
            scope
        };
        ensure!(
            scope.descriptor == descriptor,
            "session creation identity conflicts with another scope"
        );
        let catalog = scope.session.snapshot().await?;
        if catalog
            .catalog
            .flows
            .iter()
            .any(|row| row.id == address.instance_id)
        {
            let worker = scope.session.open_flow(address.instance_id.clone()).await?;
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
            None => prepare_new_session(&scope)?,
        };
        scope.executor.tool_ctx.prompt_resolver = source.executor.tool_ctx.prompt_resolver.clone();
        // Retain the scoped prompt transport before scheduling startup work.
        loaded.insert(request, scope.clone());
        self.host
            .launch_entry(
                FlowDescriptor {
                    address: address.clone(),
                    creation_request: request,
                    created_by: None,
                    content: entry.into(),
                },
                LaunchInput {
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
                },
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
        Ok(address)
    }
}

struct NewSessionEntry {
    entry: atman_runtime::flow_store::EntrySnapshot,
    start: atman_runtime::flow_store::launch::EntryStart,
    trust: atman_runtime::trust::TrustConfig,
    authority: EffectiveAuthority,
}

fn prepare_new_session(scope: &Attachment) -> Result<NewSessionEntry> {
    use atman_runtime::flow_store::{EntrySnapshot, launch::EntryStart};
    let (program, name, _, _) = crate::resolve_slash_command_from(
        "/agent",
        scope.config.config_dir(),
        scope
            .descriptor
            .project
            .as_ref()
            .map(|project| project.root()),
    )?;
    let flow = program
        .entry_flow(&name)
        .and_then(|id| program.flow(&id))
        .context("default agent entry is unavailable")?;
    ensure!(
        atman_runtime::validate::invocation_message_parameter(flow)?.is_some(),
        "default agent entry requires an invocation user_message parameter"
    );
    let trust = scope.config.trust_config()?;
    let authority = EffectiveAuthority::root(
        &trust,
        contract_allows_shell(flow.contract.as_ref()),
        scope
            .descriptor
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
    let startup = scope.config.config_dir().join("on_session_start.at");
    let start = if startup.try_exists()? {
        let program = atman_runtime::source_program::load_program(
            &startup,
            &scope.descriptor.source_roots(),
        )?;
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
