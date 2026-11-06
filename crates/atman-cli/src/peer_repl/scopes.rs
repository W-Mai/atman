//! Session runtimes retained independently of view selection.

use super::*;
use atman_runtime::flow_runtime::catalog::SessionCatalog;

pub(super) struct SessionSeed {
    pub config: ConfigHub,
    pub project: Option<atman_runtime::storage::ProjectContext>,
    pub directory: Option<std::path::PathBuf>,
    pub prompt_resolver: Option<Arc<dyn atman_runtime::rendezvous::PromptResolver>>,
}

impl From<&Attachment> for SessionSeed {
    fn from(scope: &Attachment) -> Self {
        Self {
            config: scope.config.clone(),
            project: scope.project.clone(),
            directory: None,
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
    pub(super) startup_directory: Result<std::path::PathBuf, String>,
    pub(super) root: std::path::PathBuf,
    pub(super) host: RuntimeHost,
    permissions: Option<Arc<PermissionClients>>,
    prompts: Arc<prompts::Prompts>,
    loaded: tokio::sync::Mutex<HashMap<Uuid, Attachment>>,
    management:
        tokio::sync::OnceCell<Result<atman_runtime::flow_runtime::host::ProfileHandle, String>>,
    pub catalog: SessionCatalog,
    pub(super) browse: super::tui::browse::DetailsCache,
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
            startup_directory: std::env::current_dir()
                .and_then(|path| path.canonicalize())
                .map_err(|error| format!("Startup directory unavailable: {error}")),
            catalog: SessionCatalog::new(Some(host.clone()), Some(root.to_owned())),
            host,
            permissions,
            prompts,
            loaded: Default::default(),
            management: Default::default(),
            browse: Default::default(),
        }
    }

    pub async fn enable_management(&self) {
        let _ = self.profile().await;
    }

    pub(super) async fn profile(
        &self,
    ) -> Result<&atman_runtime::flow_runtime::host::ProfileHandle> {
        self.management
            .get_or_init(|| async {
                let root = self.root.clone();
                let result = async {
                    let store = tokio::task::spawn_blocking(move || {
                        ProfileStore::open(&RuntimeStore::open(&root)?)
                    })
                    .await??;
                    let profile = self.host.attach_profile(store)?;
                    self.host.start_control_service()?;
                    Ok::<_, anyhow::Error>(profile)
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
                        directory: None,
                        prompt_resolver: None,
                    },
                    descriptor.id,
                    descriptor,
                    CreationMode::Deferred,
                    None,
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
        self.open_selected(id, None).await
    }

    pub async fn open_flow_scope(
        &self,
        flow: &atman_runtime::flow_store::FlowRef,
    ) -> Result<Attachment> {
        self.open_selected(flow.session_id.0, Some(flow.instance_id.clone()))
            .await
    }

    async fn open_selected(
        &self,
        id: Uuid,
        instance: Option<FlowInstanceId>,
    ) -> Result<Attachment> {
        self.enable_management().await;
        // Only navigation loads serialize. Existing views retain their own immutable scope.
        let mut loaded = self.loaded.lock().await;
        if let Some(scope) = loaded.get(&id) {
            let scope = scope.clone();
            drop(loaded);
            let catalog = scope.session.snapshot().await?.catalog;
            let project = execution::selected_project(&catalog, instance.as_ref())?;
            return scope.for_project(project).await;
        }
        let scope = Attachment::open_selected(
            &self.root,
            id,
            self.host.clone(),
            self.permissions.clone(),
            self.prompts.clone(),
            instance,
        )
        .await?;
        loaded.insert(id, scope.clone());
        Ok(scope)
    }

    pub async fn remove_session(
        self: &Arc<Self>,
        request: atman_tui::flow_hub::SessionRemovalRequest,
    ) -> Result<()> {
        let scopes = self.clone();
        tokio::spawn(async move {
            let request = match request {
                atman_tui::flow_hub::SessionRemovalRequest::Persistent(request) => request,
                atman_tui::flow_hub::SessionRemovalRequest::Legacy(request) => {
                    let root = scopes.root.clone();
                    // The review action explicitly confirms the old executor has stopped.
                    return tokio::task::spawn_blocking(move || {
                        RuntimeStore::open(&root)?.delete_legacy_session(&request, true)
                    })
                    .await?;
                }
            };
            let mut loaded = scopes.loaded.lock().await;
            let id = request.session_id;
            let result = scopes.host.delete_session(&scopes.root, request).await;
            // Eviction follows accepted removal even if the requesting view disappears.
            if scopes.host.session(id).is_err() {
                loaded.remove(&id);
            }
            result
        })
        .await?
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
        let id = subscription.source.session_id.0;
        if !recorded {
            self.host.open_session(&self.root, id).await?;
        }
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
        let project = source.project.as_ref().map(|p| p.root().to_owned());
        let rows = tokio::task::spawn_blocking(move || {
            let mut rows = Vec::new();
            for id in ids.into_iter().filter(|id| *id != current) {
                let Ok(read) = RuntimeStore::inspect_session(&root, id) else {
                    continue;
                };
                if read.projection.default_flow.is_none()
                    || read.projection.project.as_ref().map(|p| p.root()) != project.as_deref()
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
        self.initialize_session_entry(source, request, descriptor, mode, None)
            .await
    }

    pub(super) async fn initialize_session_entry(
        &self,
        source: SessionSeed,
        request: Uuid,
        descriptor: SessionDescriptor,
        mode: CreationMode,
        continuation: Option<super::continuation::HistorySeed>,
    ) -> Result<atman_runtime::flow_store::FlowRef> {
        if matches!(mode, CreationMode::Interactive) {
            self.enable_management().await;
        }
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
                Some(prepare_session_entry(
                    &source.config,
                    &descriptor,
                    descriptor.project.as_ref(),
                    None,
                    continuation.is_none(),
                    source.directory.as_deref(),
                )?)
            }
        };
        let session = self
            .host
            .create_session(&self.root, descriptor.clone())
            .await?;
        let catalog = session.snapshot().await?;
        ensure!(
            catalog.catalog.descriptor == descriptor,
            "session creation identity conflicts with another scope"
        );
        let registered = catalog
            .catalog
            .flows
            .iter()
            .find(|row| row.id == address.instance_id);
        let project = registered.map_or_else(
            || catalog.catalog.project.clone(),
            |row| row.project.clone(),
        );
        let retained = if registered.is_some() {
            let worker = session.open_flow(address.instance_id.clone()).await?;
            let entry = worker.descriptor().await?;
            ensure!(
                entry.creation_request == request && entry.created_by.is_none(),
                "initial flow creation identity conflicts with the request"
            );
            if let Some(record) = worker.launch_record().await? {
                if let Some(directory) = &source.directory {
                    ensure!(
                        record
                            .input
                            .execution
                            .as_ref()
                            .and_then(|entry| entry.authority.workspace_root.as_ref())
                            == Some(directory),
                        "session creation identity conflicts with its working directory"
                    );
                }
                ensure!(
                    record
                        .input
                        .continued_from
                        .as_ref()
                        .map(|source| (source.session_id, &source.owner))
                        == continuation
                            .as_ref()
                            .map(|seed| (seed.source.session_id, &seed.source.owner)),
                    "session creation identity conflicts with its history source"
                );
                return Ok(address);
            }
            Some(entry)
        } else {
            None
        };
        let NewSessionEntry {
            entry,
            start,
            trust,
            authority,
        } = match prepared.filter(|_| retained.is_none() && project == descriptor.project) {
            Some(prepared) => prepared,
            None => prepare_session_entry(
                &source.config,
                &descriptor,
                project.as_ref(),
                retained.as_ref().map(FlowDescriptor::entry).transpose()?,
                continuation.is_none(),
                source.directory.as_deref(),
            )?,
        };
        let descriptor = retained.unwrap_or(FlowDescriptor {
            address: address.clone(),
            creation_request: request,
            created_by: None,
            content: entry.into(),
        });
        let inherited = continuation
            .as_ref()
            .map(|seed| seed.prepare())
            .transpose()?;
        let input = LaunchInput {
            name: None,
            display_label: None,
            scope: None,
            workspace: None,
            initial_goal: None,
            inherited_outputs: inherited
                .as_ref()
                .map(|(_, outputs)| outputs.clone())
                .unwrap_or_default(),
            continued_from: continuation.as_ref().map(|seed| seed.source.clone()),
            inherited_context: continuation
                .as_ref()
                .map(|seed| seed.transcript.context.clone())
                .unwrap_or_default(),
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
                session_start: continuation.is_none(),
                first_turn: atman_rt::TurnId(request.into()),
                first_run: atman_rt::RunId(request.into()),
                authority: authority.clone(),
            }),
        };
        if let CreationMode::Interactive = mode {
            let mut scope = match loaded.get(&request) {
                Some(scope) => scope.clone(),
                None => {
                    Attachment::open_selected(
                        &self.root,
                        request,
                        self.host.clone(),
                        self.permissions.clone(),
                        self.prompts.clone(),
                        registered.map(|row| row.id.clone()),
                    )
                    .await?
                }
            }
            .for_project(project)
            .await?;
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
                .launch_flow_at(descriptor, input, catalog.cursor, |_, _| Ok(()))
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

fn prepare_session_entry(
    config: &ConfigHub,
    descriptor: &SessionDescriptor,
    project: Option<&atman_runtime::storage::ProjectContext>,
    retained: Option<&atman_runtime::flow_store::EntrySnapshot>,
    session_start: bool,
    directory: Option<&Path>,
) -> Result<NewSessionEntry> {
    use atman_runtime::flow_store::{EntrySnapshot, launch::EntryStart};
    let roots = descriptor.source_roots_for(project);
    let (program, name) = match retained {
        Some(entry) => (
            load_program_from_bundle(&entry.bundle, &entry.path, &roots)?,
            entry.name.clone(),
        ),
        None => {
            let (program, name, _, _) = crate::resolve_slash_command_from(
                "/agent",
                config.config_dir(),
                project.map(|project| project.root()),
            )?;
            (program, name)
        }
    };
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
        directory
            .map(Path::to_path_buf)
            .or_else(|| project.map(|project| project.root().to_path_buf())),
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
    let start = if session_start && startup.try_exists()? {
        let program = atman_runtime::source_program::load_program(&startup, &roots)?;
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
    use atman_runtime::flow_store::{EntrySnapshot, FlowDescriptor, FlowRef, FlowTransition};

    #[tokio::test]
    async fn prepared_initial_entry_reuses_retained_source_and_project_after_reopening() {
        use atman_runtime::{source_program::load_program, storage::ProjectContext};
        for mode in [CreationMode::Interactive, CreationMode::Deferred] {
            let data = tempfile::tempdir().unwrap();
            let config = ConfigHub::from_config_dir(data.path().join("config"));
            let a = data.path().join("a");
            let b = data.path().join("b");
            let c = data.path().join("c");
            for path in [config.config_dir(), &a, &b, &c] {
                std::fs::create_dir_all(path).unwrap();
            }
            std::fs::write(config.config_dir().join("config.toml"), "").unwrap();
            std::fs::create_dir_all(config.config_dir().join("commands")).unwrap();
            std::fs::write(
                config.config_dir().join("commands/agent.at"),
                "flow broken(",
            )
            .unwrap();
            std::fs::write(
                config.config_dir().join("on_session_start.at"),
                "flow startup() -> string { return final.answer(message: \"ready\") }",
            )
            .unwrap();
            for (path, label) in [(&a, "ALPHA"), (&b, "BETA"), (&c, "GAMMA")] {
                std::fs::write(path.join("AGENTS.md"), format!("# scope-rule\n{label}")).unwrap();
            }
            let original = ProjectContext::resolve(&config, &a, data.path()).unwrap();
            let captured = ProjectContext::resolve(&config, &b, data.path()).unwrap();
            let current = ProjectContext::resolve(&config, &c, data.path()).unwrap();
            let request = Uuid::now_v7();
            let descriptor = SessionDescriptor {
                id: request,
                title: "New session".into(),
                project: Some(original.clone()),
                configuration: SessionConfiguration::Atman(config.source()),
            };
            let path = b.join("main.at");
            let helper = b.join("helper.at");
            std::fs::write(&helper, "pub flow tag() -> string { return \"retained\" }").unwrap();
            std::fs::write(
                &path,
                r#"
                use "./helper.at"::tag
                flow main(input: string) -> string {
                    contract { invocation { user_message: input } }
                    tag = tag().await
                    rule = rule.fetch(name: "scope-rule")
                    return final.answer(message: tag + "|" + rule + "|" + input)
                }
            "#,
            )
            .unwrap();
            let program =
                load_program(&path, &descriptor.source_roots_for(Some(&captured))).unwrap();
            let entry = FlowDescriptor {
                address: FlowRef {
                    session_id: request.into(),
                    instance_id: atman_rt::FlowInstanceId(request.into()),
                },
                creation_request: request,
                created_by: None,
                content: EntrySnapshot {
                    path: program.entry_path().unwrap().to_owned(),
                    name: "main".into(),
                    bundle: program.revision_bundle(),
                }
                .into(),
            };
            let store = RuntimeStore::open(data.path()).unwrap();
            let mut writer = store.create_session(descriptor.clone()).unwrap();
            writer
                .set_project(&writer.cursor().clone(), Some(captured.clone()))
                .unwrap();
            let prepared = writer.create_flow(entry.clone()).unwrap();
            writer
                .set_project(&writer.cursor().clone(), Some(current.clone()))
                .unwrap();
            let mut selected = entry.clone();
            selected.address.instance_id = FlowInstanceId::now();
            selected.creation_request = Uuid::now_v7();
            let selected_id = selected.address.instance_id.clone();
            let selected_flow = writer.create_flow(selected).unwrap();
            writer
                .select_default(&writer.cursor().clone(), selected_id.clone())
                .unwrap();
            drop(selected_flow);
            drop(prepared);
            drop(writer);
            std::fs::remove_file(path).unwrap();
            std::fs::remove_file(helper).unwrap();
            let host = RuntimeHost::default();
            let (notices, _received) = mpsc::unbounded_channel();
            let scopes = Arc::new(Scopes::empty(
                data.path(),
                host.clone(),
                None,
                Arc::new(prompts::Prompts::new(notices)),
            ));
            // The default view may be bound to another instance while this entry remains Prepared.
            let opened = scopes.open(request).await.unwrap();
            assert_eq!(opened.project.as_ref(), Some(&current));
            let seed = || SessionSeed {
                config: config.clone(),
                project: Some(original.clone()),
                directory: None,
                prompt_resolver: None,
            };
            let address = scopes.create_session(seed(), request, mode).await.unwrap();
            assert_eq!(address, entry.address);
            let worker = host.flow(&address).await.unwrap();
            assert_eq!(worker.descriptor().await.unwrap(), entry);
            assert_eq!(worker.project().await.unwrap().as_ref(), Some(&captured));
            let record = worker.launch_record().await.unwrap().unwrap();
            assert_eq!(
                record
                    .input
                    .execution
                    .as_ref()
                    .unwrap()
                    .authority
                    .workspace_root
                    .as_deref(),
                Some(captured.root()),
            );
            if matches!(mode, CreationMode::Deferred) {
                assert!(worker.context().messages_full().is_empty());
                assert_eq!(worker.snapshot().await.unwrap().pending.len(), 1);
                assert!(
                    host.session(request)
                        .unwrap()
                        .scheduling_status(address.instance_id.clone())
                        .await
                        .unwrap()
                        .is_none()
                );
            }
            let scope = scopes.open_flow_scope(&address).await.unwrap();
            scope.bind(&worker).await.unwrap();
            host.wait_quiescent().await.unwrap();
            let before = worker.snapshot().await.unwrap().cursor;
            // Once activated, retries do not reload startup or enqueue another greeting.
            std::fs::write(
                config.config_dir().join("on_session_start.at"),
                "flow broken(",
            )
            .unwrap();
            assert_eq!(
                scopes.create_session(seed(), request, mode).await.unwrap(),
                address
            );
            assert_eq!(worker.snapshot().await.unwrap().cursor, before);
            std::fs::remove_file(config.config_dir().join("on_session_start.at")).unwrap();
            scope
                .command(&worker, ":invoke {\"input\":\"hello\"}")
                .await
                .unwrap();
            host.wait_quiescent().await.unwrap();
            let messages = worker.context().messages_full();
            let answer = messages.last().unwrap().text_concat();
            assert!(
                answer.contains("retained|")
                    && answer.contains("BETA")
                    && answer.ends_with("|hello"),
                "{answer}"
            );
            assert!(!answer.contains("ALPHA") && !answer.contains("GAMMA"));
            assert_eq!(
                messages
                    .iter()
                    .filter(|message| message.text_concat() == "ready")
                    .count(),
                1
            );
            let catalog = host.session(request).unwrap().snapshot().await.unwrap();
            assert_eq!(catalog.catalog.descriptor, descriptor);
            assert_eq!(catalog.catalog.project.as_ref(), Some(&current));
            assert_eq!(catalog.catalog.flows.len(), 2);
            assert_eq!(catalog.catalog.default_flow.as_ref(), Some(&selected_id));
            host.shutdown().await.unwrap();
        }
    }

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
            .select_default(
                session.snapshot().await.unwrap().cursor,
                selected_address.instance_id.clone(),
            )
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
