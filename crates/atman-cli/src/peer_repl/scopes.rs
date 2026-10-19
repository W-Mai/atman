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
    management:
        tokio::sync::OnceCell<Result<atman_runtime::flow_runtime::host::ProfileHandle, String>>,
    pub catalog: SessionCatalog,
}

impl Scopes {
    #[cfg(test)]
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
            management: Default::default(),
        }
    }

    pub async fn enable_management(&self) {
        let _ = self.profile().await;
    }

    async fn profile(&self) -> Result<&atman_runtime::flow_runtime::host::ProfileHandle> {
        self.management
            .get_or_init(|| async {
                let root = self.root.clone();
                let result = async {
                    let store = tokio::task::spawn_blocking(move || {
                        ProfileStore::open(&RuntimeStore::open(&root)?)
                    })
                    .await??;
                    Ok::<_, anyhow::Error>(self.host.attach_profile(store)?)
                }
                .await;
                if let Err(error) = &result {
                    atman_runtime::notify!(warn, "profile management unavailable: {error}");
                }
                // Save ownership before awaiting validation so cancellation cannot orphan it.
                result.map_err(|error| format!("{error:#}"))
            })
            .await
            .as_ref()
            .map_err(|error| anyhow::anyhow!(error.clone()))
    }

    /// Explicit operator navigation initializes only missing entry data, never scheduling or grants.
    pub async fn open_management(
        self: &Arc<Self>,
        configuration: ConfigHub,
    ) -> Result<atman_runtime::flow_store::FlowRef> {
        let scopes = self.clone();
        tokio::spawn(async move {
            let profile = scopes.profile().await?;
            let view = profile
                .bootstrap(SessionConfiguration::Atman(configuration.source()))
                .await?;
            ensure!(
                view.unavailable.is_none(),
                "management profile unavailable: {}",
                view.unavailable.as_deref().unwrap_or_default()
            );
            let descriptor = view.profile.management
                .context("management binding is absent")?.session;
            let catalog = scopes.catalog.overview(descriptor.id).await?;
            let initial = atman_rt::FlowInstanceId(descriptor.id.into());
            let address = if let Some(instance_id) = catalog.summary.default_flow
                .filter(|id| id != &initial)
            {
                atman_runtime::flow_store::FlowRef {
                    session_id: descriptor.id.into(), instance_id,
                }
            } else {
                let SessionConfiguration::Atman(source) = &descriptor.configuration else {
                    bail!("management session requires an Atman configuration source to initialize its entry");
                };
                scopes.initialize_session_entry(
                    SessionSeed {
                        config: ConfigHub::from_source(source)?,
                        project: descriptor.project.clone(),
                        prompt_resolver: None,
                    },
                    descriptor.id,
                    descriptor,
                    CreationMode::Deferred,
                ).await?
            };
            let current = profile.snapshot().await?;
            ensure!(
                current.unavailable.is_none()
                    && current.profile.management.as_ref().is_some_and(|binding|
                        binding.session.id == address.session_id.0),
                "management binding changed during navigation"
            );
            Ok(address)
        }).await?
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

    pub async fn cancel_watcher(
        &self,
        owner: atman_runtime::flow_store::FlowRef,
        id: Uuid,
    ) -> Result<()> {
        // UI controls are authenticated operator requests; they never borrow a model turn lease.
        let worker = self.host.flow(&owner).await?;
        worker
            .unwatch(id, move |actual| {
                if actual == &owner {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "watcher owner differs from selected flow",
                    ))
                }
            })
            .await?;
        Ok(())
    }

    pub async fn register_process_watcher(
        &self,
        owner: atman_runtime::flow_store::FlowRef,
        watch: atman_runtime::flow_store::process_watch::ProcessWatch,
    ) -> Result<()> {
        self.host
            .flow(&owner)
            .await?
            .watch_process_source(watch, move |actual| {
                if actual == &owner {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "watcher owner differs from selected flow",
                    ))
                }
            })
            .await?;
        Ok(())
    }

    pub async fn process_watch_sources(
        &self,
        owner: &atman_runtime::flow_store::FlowRef,
    ) -> Result<Vec<atman_runtime::flow_runtime::ProcessWatchSource>> {
        Ok(self
            .host
            .session(owner.session_id.0)?
            .process_watch_sources(owner.instance_id.clone())
            .await?)
    }

    pub async fn register_watcher(
        &self,
        owner: atman_runtime::flow_store::FlowRef,
        subscription: atman_runtime::flow_store::subscription::FlowSubscription,
    ) -> Result<()> {
        use atman_runtime::flow_store::subscription::SubscriptionPurpose;
        ensure!(
            subscription.purpose == SubscriptionPurpose::Watcher && subscription.reply.is_none(),
            "watcher controls cannot create internal result routes"
        );
        self.prepare_watch_source(&owner, &subscription).await?;
        self.host
            .watch_events(owner.clone(), subscription, move |actual| {
                if actual == &owner {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "watcher owner differs from selected flow",
                    ))
                }
            })
            .await?;
        Ok(())
    }

    pub async fn replace_watcher(
        &self,
        owner: atman_runtime::flow_store::FlowRef,
        expected: atman_runtime::flow_store::watch_edit::WatchDefinition,
        replacement: atman_runtime::flow_store::watch_edit::WatchDefinition,
    ) -> Result<()> {
        if let atman_runtime::flow_store::watch_edit::WatchDefinition::Flow(subscription) =
            &replacement
        {
            self.prepare_watch_source(&owner, subscription).await?;
        }
        self.host
            .replace_watcher(owner.clone(), expected, replacement, move |actual| {
                if actual == &owner {
                    Ok(())
                } else {
                    Err(std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        "watcher owner differs from selected flow",
                    ))
                }
            })
            .await?;
        Ok(())
    }

    async fn prepare_watch_source(
        &self,
        owner: &atman_runtime::flow_store::FlowRef,
        subscription: &atman_runtime::flow_store::subscription::FlowSubscription,
    ) -> Result<()> {
        let recorded = self
            .host
            .flow(owner)
            .await?
            .flow_subscriptions()
            .await?
            .iter()
            .any(|record| record.subscription.id == subscription.id);
        // Share the attachment lock with navigation, without building providers or scheduling turns.
        let _loaded = self.loaded.lock().await;
        let id = subscription.source.session_id.0;
        match self.host.session(id) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && !recorded => {
                let root = self.root.clone();
                let (session, redactor) = tokio::task::spawn_blocking(move || {
                    let session = RuntimeStore::open(&root)?.open_session(id)?;
                    let redactor = match &session.projection().descriptor.configuration {
                        SessionConfiguration::Atman(source) => {
                            let config = ConfigHub::from_source(source)?;
                            atman_daemon::bootstrap::build_redactor(Some(config.config_dir()))
                        }
                        _ => None,
                    };
                    Ok::<_, anyhow::Error>((session, redactor))
                })
                .await??;
                self.host.attach_session_with_redactor(session, redactor)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && recorded => {}
            Err(error) => return Err(error.into()),
        }
        drop(_loaded);
        Ok(())
    }

    pub async fn startup_recent(
        &self,
        source: &Attachment,
        limit: usize,
    ) -> Result<Vec<atman_tui::app::StartupSessionEntry>> {
        enum Candidate {
            Instance(Uuid),
            Legacy(crate::SessionRow),
        }
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
                rows.push((
                    read.activity_time(),
                    id.to_string(),
                    Candidate::Instance(id),
                ));
            }
            if let Some(project) = project {
                let query =
                    atman_runtime::session_meta::SessionDiscoveryQuery::current_project(&project);
                for row in crate::list_recent_sessions(&root, limit, Some(&query))? {
                    if Uuid::parse_str(&row.sid).is_ok()
                        && std::fs::symlink_metadata(root.join("sessions").join(&row.sid))
                            .is_ok_and(|metadata| metadata.file_type().is_dir())
                    {
                        rows.push((Some(row.mtime), row.sid.clone(), Candidate::Legacy(row)));
                    }
                }
            }
            rows.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
            Ok::<_, anyhow::Error>(rows)
        })
        .await??;
        let mut recent = Vec::new();
        for (activity, _, candidate) in rows {
            if recent.len() == limit {
                break;
            }
            let id = match candidate {
                Candidate::Instance(id) => id,
                Candidate::Legacy(row) => {
                    let root = self.root.clone();
                    recent.push(
                        tokio::task::spawn_blocking(move || {
                            crate::legacy_startup_entry(&root, row, std::time::SystemTime::now())
                        })
                        .await?,
                    );
                    continue;
                }
            };
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
        let descriptor = SessionDescriptor {
            id: request,
            title: "New session".into(),
            project: source.project.clone(),
            configuration: SessionConfiguration::Atman(source.config.source()),
        };
        self.initialize_session_entry(source, request, descriptor, mode)
            .await
    }

    async fn initialize_session_entry(
        &self,
        source: SessionSeed,
        request: Uuid,
        descriptor: SessionDescriptor,
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

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::flow_store::FlowTransition;

    #[tokio::test]
    async fn watcher_registration_attaches_storage_without_models_and_retains_retry_identity() {
        use atman_runtime::flow_store::{
            access::{AccessTarget, FlowAction, FlowGrant},
            subscription::{FlowEventFilter, FlowSubscription, SubscriptionPurpose},
        };
        let data = tempfile::tempdir().unwrap();
        let config = ConfigHub::from_config_dir(data.path().join("config"));
        std::fs::create_dir_all(config.config_dir()).unwrap();
        std::fs::write(config.config_dir().join("config.toml"), "").unwrap();
        let host = RuntimeHost::default();
        let (notices, _received) = mpsc::unbounded_channel();
        let scopes = Arc::new(Scopes::empty(
            data.path(),
            host.clone(),
            None,
            Arc::new(prompts::Prompts::new(notices)),
        ));
        let owner = scopes.open_management(config).await.unwrap();
        let recipient = host.flow(&owner).await.unwrap();
        let before = recipient.snapshot().await.unwrap();
        let mut descriptor = recipient.descriptor().await.unwrap();
        let source_id = Uuid::now_v7();
        descriptor.address.session_id = source_id.into();
        descriptor.creation_request = Uuid::now_v7();
        let source = descriptor.address.clone();
        let mut stored = RuntimeStore::open(data.path())
            .unwrap()
            .create_session(SessionDescriptor {
                id: source_id,
                project: None,
                configuration: Default::default(),
                title: "Detached source".into(),
            })
            .unwrap();
        let mut flow = stored.create_flow(descriptor).unwrap();
        flow.transition(flow.state().revision(), FlowTransition::Activated)
            .unwrap();
        drop(flow);
        drop(stored);
        assert!(host.session(source_id).is_err());
        let subscription = FlowSubscription {
            id: Uuid::now_v7(),
            source: source.clone(),
            filter: FlowEventFilter::FinalAnswers,
            once: false,
            purpose: SubscriptionPurpose::Watcher,
            expires_at: None,
            reply: None,
        };
        assert!(
            scopes
                .register_watcher(owner.clone(), subscription.clone())
                .await
                .is_err()
        );
        assert!(recipient.flow_subscriptions().await.unwrap().is_empty());
        let session = host.session(owner.session_id.0).unwrap();
        let grant = Uuid::now_v7();
        session
            .grant_access(
                session.snapshot().await.unwrap().cursor,
                FlowGrant {
                    id: grant,
                    grantee: owner.clone(),
                    target: AccessTarget::Flow(source),
                    actions: [FlowAction::Watch].into(),
                },
            )
            .await
            .unwrap();
        scopes
            .register_watcher(owner.clone(), subscription.clone())
            .await
            .unwrap();
        let registered = recipient.snapshot().await.unwrap();
        assert_eq!(registered.state, before.state);
        assert_eq!(registered.pending.len(), before.pending.len());
        assert!(scopes.loaded.lock().await.is_empty());
        assert!(
            host.session(source_id)
                .unwrap()
                .scheduling_status(subscription.source.instance_id.clone())
                .await
                .unwrap()
                .is_none()
        );
        let original =
            atman_runtime::flow_store::watch_edit::WatchDefinition::Flow(subscription.clone());
        let subscription = FlowSubscription {
            id: Uuid::now_v7(),
            once: true,
            ..subscription
        };
        let edited =
            atman_runtime::flow_store::watch_edit::WatchDefinition::Flow(subscription.clone());
        scopes
            .replace_watcher(owner.clone(), original.clone(), edited.clone())
            .await
            .unwrap();
        let registered = recipient.snapshot().await.unwrap();
        assert_eq!(registered.state, before.state);
        assert_eq!(registered.pending, before.pending);
        assert!(scopes.loaded.lock().await.is_empty());
        session
            .revoke_access(session.snapshot().await.unwrap().cursor, grant)
            .await
            .unwrap();
        scopes
            .register_watcher(owner.clone(), subscription.clone())
            .await
            .unwrap();
        assert_eq!(
            recipient.snapshot().await.unwrap().cursor,
            registered.cursor
        );
        let mut conflict = subscription.clone();
        conflict.once = !subscription.once;
        assert!(
            scopes
                .register_watcher(owner.clone(), conflict)
                .await
                .is_err()
        );
        scopes
            .cancel_watcher(owner.clone(), subscription.id)
            .await
            .unwrap();
        let cancelled = recipient.snapshot().await.unwrap().cursor;
        scopes
            .register_watcher(owner.clone(), subscription.clone())
            .await
            .unwrap();
        assert_eq!(recipient.snapshot().await.unwrap().cursor, cancelled);
        scopes
            .replace_watcher(owner.clone(), original.clone(), edited.clone())
            .await
            .unwrap();
        assert_eq!(recipient.snapshot().await.unwrap().cursor, cancelled);
        host.shutdown().await.unwrap();
        let source_path = data
            .path()
            .join("runtime-v1/sessions")
            .join(source_id.to_string());
        std::fs::rename(source_path, data.path().join("unavailable-source")).unwrap();
        let restored_host = RuntimeHost::default();
        restored_host
            .attach_session(
                RuntimeStore::open(data.path())
                    .unwrap()
                    .open_session(owner.session_id.0)
                    .unwrap(),
            )
            .unwrap();
        let (notices, _received) = mpsc::unbounded_channel();
        let restored = Scopes::empty(
            data.path(),
            restored_host.clone(),
            None,
            Arc::new(prompts::Prompts::new(notices)),
        );
        restored
            .register_watcher(owner.clone(), subscription)
            .await
            .unwrap();
        restored
            .replace_watcher(owner.clone(), original, edited)
            .await
            .unwrap();
        assert!(restored_host.session(source_id).is_err());
        assert_eq!(
            restored_host
                .flow(&owner)
                .await
                .unwrap()
                .snapshot()
                .await
                .unwrap()
                .cursor,
            cancelled
        );
        restored_host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn projectless_management_entry_can_bind_without_model_work() {
        let data = tempfile::tempdir().unwrap();
        let config = ConfigHub::from_config_dir(data.path().join("config"));
        std::fs::create_dir_all(config.config_dir()).unwrap();
        std::fs::write(config.config_dir().join("config.toml"), "").unwrap();
        let host = RuntimeHost::default();
        let (notices, _received) = mpsc::unbounded_channel();
        let scopes = Arc::new(Scopes::empty(
            data.path(),
            host.clone(),
            None,
            Arc::new(prompts::Prompts::new(notices)),
        ));
        let flow = scopes.open_management(config.clone()).await.unwrap();
        let scope = scopes.open(flow.session_id.0).await.unwrap();
        let worker = host.flow(&flow).await.unwrap();
        scope.bind(&worker).await.unwrap();
        assert!(worker.context().messages_full().is_empty());
        let mut selected = worker.descriptor().await.unwrap();
        selected.address.instance_id = atman_rt::FlowInstanceId(Uuid::now_v7().into());
        selected.creation_request = Uuid::now_v7();
        let selected_address = selected.address.clone();
        let session = host.session(flow.session_id.0).unwrap();
        session.create_flow(selected).await.unwrap();
        session
            .select_default(selected_address.instance_id.clone())
            .await
            .unwrap();
        assert_eq!(
            scopes.open_management(config).await.unwrap(),
            selected_address
        );
        assert!(
            session
                .scheduling_status(selected_address.instance_id)
                .await
                .unwrap()
                .is_none()
        );
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn management_navigation_retains_identity_configuration_and_execution_gates() {
        let data = tempfile::tempdir().unwrap();
        let config = ConfigHub::from_config_dir(data.path().join("config"));
        std::fs::create_dir_all(config.config_dir()).unwrap();
        std::fs::write(config.config_dir().join("config.toml"), "").unwrap();
        std::fs::write(
            config.config_dir().join("on_session_start.at"),
            "flow startup() -> string { return final.answer(message: \"ready\") }",
        )
        .unwrap();
        let host = RuntimeHost::default();
        let (notices, _received) = mpsc::unbounded_channel();
        let scopes = Arc::new(Scopes::empty(
            data.path(),
            host.clone(),
            None,
            Arc::new(prompts::Prompts::new(notices)),
        ));
        let (first, second) = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::join!(
                scopes.open_management(config.clone()),
                scopes.open_management(config.clone())
            )
        })
        .await
        .unwrap();
        let flow = first.unwrap();
        assert_eq!(flow, second.unwrap());
        let profile = scopes.profile().await.unwrap();
        let initial_profile = profile.snapshot().await.unwrap();
        let binding = initial_profile.profile.management.as_ref().unwrap();
        assert_eq!(binding.session.id, flow.session_id.0);
        assert!(binding.session.project.is_none());
        assert!(binding.grants.is_empty());
        assert_eq!(
            binding.session.configuration,
            SessionConfiguration::Atman(config.source())
        );
        let session = host.session(flow.session_id.0).unwrap();
        let worker = host.flow(&flow).await.unwrap();
        let snapshot = worker.snapshot().await.unwrap();
        assert_eq!(snapshot.pending.len(), 1);
        assert!(worker.context().messages_full().is_empty());
        assert!(
            session
                .scheduling_status(flow.instance_id.clone())
                .await
                .unwrap()
                .is_none()
        );
        assert_eq!(session.snapshot().await.unwrap().catalog.flows.len(), 1);
        let pause = Uuid::now_v7().into();
        worker
            .transition(
                snapshot.state.revision(),
                FlowTransition::ExecutionPaused { key: pause },
            )
            .await
            .unwrap();
        let paused = worker.snapshot().await.unwrap();
        // A different caller configuration and deleted source cannot reinitialize a committed entry.
        std::fs::remove_file(config.config_dir().join("commands/agent.at")).unwrap();
        let other_config = ConfigHub::from_config_dir(data.path().join("other-config"));
        assert_eq!(
            scopes.open_management(other_config.clone()).await.unwrap(),
            flow
        );
        assert_eq!(worker.snapshot().await.unwrap().cursor, paused.cursor);
        assert_eq!(
            profile.snapshot().await.unwrap().profile.cursor,
            initial_profile.profile.cursor
        );
        assert!(!other_config.config_dir().exists());
        worker
            .transition(paused.state.revision(), FlowTransition::CloseRequested)
            .await
            .unwrap();
        worker
            .transition(
                worker.state().await.unwrap().revision(),
                FlowTransition::Closed,
            )
            .await
            .unwrap();
        let closed = worker.snapshot().await.unwrap();
        assert_eq!(scopes.open_management(config.clone()).await.unwrap(), flow);
        assert_eq!(worker.snapshot().await.unwrap().cursor, closed.cursor);
        host.shutdown().await.unwrap();
        drop(worker);
        drop(session);
        drop(scopes);

        let host = RuntimeHost::default();
        let (notices, _received) = mpsc::unbounded_channel();
        let scopes = Arc::new(Scopes::empty(
            data.path(),
            host.clone(),
            None,
            Arc::new(prompts::Prompts::new(notices)),
        ));
        assert_eq!(scopes.open_management(config).await.unwrap(), flow);
        assert_eq!(
            host.flow(&flow)
                .await
                .unwrap()
                .snapshot()
                .await
                .unwrap()
                .cursor,
            closed.cursor
        );
        assert_eq!(
            RuntimeStore::session_ids(data.path()).unwrap(),
            [flow.session_id.0]
        );
        host.shutdown().await.unwrap();
    }
}
