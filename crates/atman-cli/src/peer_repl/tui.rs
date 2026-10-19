//! Interactive controls for a persistent view. Durable acceptance precedes every input ACK.

use super::*;
mod directory;
use atman_tui::{
    TuiCommand, TuiControl, TuiHandle, TuiSubmission,
    control::{BoundControl, ControlReplies, ViewAddress, ViewBinding},
};

impl Attachment {
    pub(super) async fn interact_tui(
        mut self,
        scopes: Arc<scopes::Scopes>,
        terminal: atman_tui::InheritedTerminal,
        notifications: Vec<atman_runtime::notify::Notification>,
    ) -> Result<()> {
        let catalog = self.session.snapshot().await?;
        let worker = self
            .session
            .open_flow(
                catalog
                    .catalog
                    .default_flow
                    .context("session has no default flow")?,
            )
            .await?;
        let forms = worker.context().forms();
        self.executor.tool_ctx.prompt_resolver = Some(Arc::new(
            atman_runtime::rendezvous::FormPromptResolver::new(forms.clone()),
        ));
        let (controls, input) = mpsc::unbounded_channel();
        let (mut handle, service, commands) = self
            .prepare_tui_view(&worker, controls.clone(), &forms)
            .await?;
        handle.onboarding_recommended =
            self.config.models().is_first_run(&self.config.load_auth()?);
        scopes.welcome(&self, &mut handle).await;
        for row in catalog.catalog.flows {
            let result = async {
                let peer = self.session.open_flow(row.id.clone()).await?;
                if matches!(
                    peer.state().await?.state(),
                    atman_rt::InstanceState::Open(_)
                ) {
                    self.bind(&peer).await?;
                }
                Ok::<_, anyhow::Error>(())
            }
            .await;
            if let Err(error) = result {
                let _ = commands.send(TuiCommand::Toast {
                    message: format!("Flow {} remains detached: {error:#}", row.id),
                    level: atman_tui::app::NoteLevel::Error,
                });
            }
        }
        Workspace {
            scopes,
            controls,
            input,
            forms,
            handle,
            service,
        }
        .run(terminal, notifications)
        .await
    }

    async fn prepare_tui_view(
        &self,
        worker: &FlowWorker,
        controls: mpsc::UnboundedSender<BoundControl>,
        forms: &Arc<atman_runtime::session::FormRegistry>,
    ) -> Result<(TuiHandle, ViewServices, mpsc::UnboundedSender<TuiCommand>)> {
        ensure!(
            worker.address().session_id == self.session.id().into(),
            "view and Session scope differ"
        );
        worker
            .context()
            .bind_model_registry(self.config.models().clone())?;
        let binding = ViewBinding {
            view_id: Uuid::now_v7(),
            revision: 1,
            flow: worker.address().clone().into(),
        };
        let mut handle =
            TuiHandle::from_flow(worker, binding.clone(), controls, self.config.clone()).await?;
        handle.session_id = self.session.id().to_string();
        let mut names = self.session.subscribe_title();
        handle.session_name = names.borrow_and_update().clone();
        handle.session_name_rx = Some(names);
        handle.project_root = self
            .descriptor
            .project
            .as_ref()
            .map(|project| project.root().display().to_string());
        handle.form_rx = Some(forms.subscribe_for(worker.context().deferred_form_inbox()));
        handle.trust = self.config.trust_config()?;
        let (commands, command_rx) = mpsc::unbounded_channel();
        handle.cmd_rx = Some(command_rx);
        let panels = crate::tui_panels::PanelServices::for_scope(
            worker.context(),
            self.executor.clone(),
            commands.clone(),
            vec![],
            self.config.clone(),
            self.descriptor.project.clone(),
        )?;
        Ok((
            handle,
            ViewServices {
                binding,
                project: self
                    .descriptor
                    .project
                    .as_ref()
                    .map(|project| project.root().to_owned()),
                commands: commands.clone(),
                execution: Some(ExecutionServices {
                    scope: self.clone(),
                    worker: worker.clone(),
                    panels,
                }),
            },
            commands,
        ))
    }

    async fn submit_tui(
        &self,
        worker: &FlowWorker,
        request: Uuid,
        submission: TuiSubmission,
    ) -> Result<()> {
        let mut invocation = BTreeMap::new();
        if let Some(reasoning) = submission.reasoning {
            invocation.insert("effort".into(), reasoning.to_string().into());
        }
        self.submit_user(
            worker,
            request,
            UserContent {
                text: submission.text,
                images: submission.images,
                presentation: submission.presentation,
            },
            invocation,
        )
        .await
        .map(|_| ())
    }

    async fn tui_control(
        &self,
        worker: &FlowWorker,
        request: Uuid,
        command: TuiControl,
        replies: &ControlReplies,
        forms: &Arc<atman_runtime::session::FormRegistry>,
    ) -> Result<()> {
        let context = worker.context();
        match command {
            TuiControl::Submit(submission) => {
                if let Some(command) = submission.control_command()
                    && let Some(command) =
                        atman_runtime::meta_commands::AttachmentCommand::from_line(command)
                {
                    let result = async {
                        let command = command.map_err(anyhow::Error::msg)?;
                        let atman_runtime::meta_commands::AttachmentCommand::Add(path) = command
                        else {
                            bail!(
                                "attachment lists and clearing belong to the requesting composer"
                            );
                        };
                        let record = worker.launch_record().await?;
                        self.import_image(worker, record.as_ref(), Path::new(path))
                    }
                    .await;
                    let _ = replies.send(TuiCommand::AttachmentImported {
                        result: result.map_err(|error| format!("{error:#}")),
                    });
                    return Ok(());
                }
                let result = if let Some(command) = submission.control_command() {
                    let command = command.trim();
                    let (name, argument) = command
                        .split_once(char::is_whitespace)
                        .unwrap_or((command, ""));
                    let result = match name {
                        ":sidebar" => crate::sidebar_mode(argument).map(|mode| {
                            if let Some(mode) = mode {
                                let _ = replies.send(TuiCommand::SetSidebar(mode));
                                "Sidebar updated".into()
                            } else {
                                "Use :sidebar on | off".into()
                            }
                        }),
                        ":copy" => {
                            crate::copy_payload(argument, &context.messages_full()).map(|text| {
                                if let Some(text) = text {
                                    let _ = replies.send(TuiCommand::CopyText(text));
                                    String::new()
                                } else {
                                    "Nothing to copy".into()
                                }
                            })
                        }
                        _ => self.command_with_request(worker, command, request).await,
                    };
                    result.map(|message| {
                        if !message.is_empty() {
                            let _ = replies.send(TuiCommand::Toast {
                                message,
                                level: atman_tui::app::NoteLevel::Info,
                            });
                        }
                    })
                } else {
                    self.submit_tui(worker, request, submission).await
                };
                let _ = replies.send(TuiCommand::SubmissionResult {
                    result: result.map_err(|error| format!("{error:#}")),
                });
            }
            TuiControl::GoalControl {
                request_id,
                target,
                goal_id,
                revision,
                action,
            } => {
                let result = async {
                    ensure!(
                        target.is_none(),
                        "select the destination flow before changing its goal"
                    );
                    let expected = match (goal_id, revision) {
                        (Some(id), Some(revision)) => Some(GoalVersion { id, revision }),
                        (None, None) => None,
                        _ => bail!("incomplete goal revision"),
                    };
                    use atman_runtime::goal::GoalAction;
                    let edit = match action {
                        GoalAction::Pause => GoalEdit::Pause {
                            actor: GoalActor::User,
                            reason: GoalPauseReason::UserRequested,
                            summary: String::new(),
                        },
                        GoalAction::Resume => GoalEdit::Resume {
                            actor: GoalActor::User,
                        },
                        GoalAction::Set { text } => GoalEdit::Set {
                            text,
                            replace_completed: false,
                        },
                        GoalAction::Create { text } => GoalEdit::Create { text },
                        GoalAction::Clear => GoalEdit::Clear,
                    };
                    let start_turn = matches!(edit, GoalEdit::Resume { .. });
                    if start_turn {
                        self.bind(worker).await?;
                    }
                    worker
                        .edit_goal(
                            GoalRequest {
                                id: request,
                                sender: SenderIdentity::User(USER),
                                expected,
                                edit,
                                start_turn,
                            },
                            |_| Ok(()),
                        )
                        .await?;
                    Ok::<_, anyhow::Error>(())
                }
                .await
                .map_err(|error| format!("{error:#}"));
                if let Some(request_id) = request_id {
                    let _ = replies.send(TuiCommand::GoalSaved { request_id, result });
                } else {
                    result.map_err(anyhow::Error::msg)?;
                }
            }
            TuiControl::CancelFlow | TuiControl::HardStop => {
                worker
                    .control(
                        worker.state().await?.revision(),
                        Uuid::from_bytes(*b"atman-user-pause").into(),
                        PeerControl::CancelTurn,
                        |_| Ok(()),
                    )
                    .await?;
            }
            TuiControl::FormSubmit {
                request_id,
                form_id,
                submission,
            } => {
                let result = if forms.owns_form(&form_id, &context.deferred_form_inbox()) {
                    forms
                        .submit(&form_id, submission)
                        .await
                        .map_err(|error| error.to_string())
                } else {
                    Err("Form is no longer pending in this flow".into())
                };
                let _ = replies.send(TuiCommand::FormSubmissionResult {
                    request_id,
                    form_id,
                    result,
                });
            }
            TuiControl::ResolvePermission {
                selector,
                expected_revision,
                action,
                grant_scope,
                reason,
            } => {
                use atman_runtime::permission::PermissionSelector;
                let (ids, revisions, group) = match selector {
                    PermissionSelector::RequestIds(ids) => {
                        let revisions = ids
                            .iter()
                            .cloned()
                            .map(|id| (id, expected_revision))
                            .collect();
                        (ids, revisions, None)
                    }
                    PermissionSelector::Group(id) => {
                        (vec![], HashMap::new(), Some((id, expected_revision)))
                    }
                    _ => bail!("invalid permission selector"),
                };
                context.permission_broker().user_resolve(
                    &self.session.id().to_string(),
                    Some(USER.to_string()),
                    ids,
                    &revisions,
                    group,
                    action,
                    grant_scope,
                    reason,
                )?;
            }
            TuiControl::UpdateTrust(mut trust) => {
                trust.theme = context.trust_config().theme;
                context.update_trust(trust, |trust| {
                    self.config
                        .set_trust_config(trust)
                        .map_err(std::io::Error::other)
                })?;
            }
            TuiControl::CompactReviewAccept { review_id, edited } => {
                context.compact_reviews().decide(
                    &review_id,
                    edited.map_or(
                        atman_runtime::CompactReviewDecision::AcceptAsIs,
                        |summary| atman_runtime::CompactReviewDecision::AcceptEdited { summary },
                    ),
                );
            }
            TuiControl::CompactReviewReject { review_id } => {
                context
                    .compact_reviews()
                    .decide(&review_id, atman_runtime::CompactReviewDecision::Reject);
            }
            _ => bail!("This control is not available for persistent flows"),
        }
        Ok(())
    }
}

pub(super) async fn interact_archive(
    view: FlowView,
    descriptor: SessionDescriptor,
    scopes: Arc<scopes::Scopes>,
    terminal: atman_tui::InheritedTerminal,
    notifications: Vec<atman_runtime::notify::Notification>,
) -> Result<()> {
    let (controls, input) = mpsc::unbounded_channel();
    let (handle, service) = archive_attachment(view, descriptor, controls.clone())?;
    Workspace {
        scopes,
        controls,
        input,
        forms: Arc::default(),
        handle,
        service,
    }
    .run(terminal, notifications)
    .await
}

fn archive_attachment(
    view: FlowView,
    descriptor: SessionDescriptor,
    controls: mpsc::UnboundedSender<BoundControl>,
) -> Result<(TuiHandle, ViewServices)> {
    let binding = ViewBinding {
        view_id: Uuid::now_v7(),
        revision: 1,
        flow: view.transcript().flow().clone().into(),
    };
    let mut handle = TuiHandle::from_archive(view, binding.clone(), controls, &descriptor)?;
    let (commands, rx) = mpsc::unbounded_channel();
    handle.cmd_rx = Some(rx);
    Ok((
        handle,
        ViewServices {
            binding,
            project: descriptor
                .project
                .as_ref()
                .map(|project| project.root().to_owned()),
            commands,
            execution: None,
        },
    ))
}

struct Workspace {
    scopes: Arc<scopes::Scopes>,
    controls: mpsc::UnboundedSender<BoundControl>,
    input: mpsc::UnboundedReceiver<BoundControl>,
    forms: Arc<atman_runtime::session::FormRegistry>,
    handle: TuiHandle,
    service: ViewServices,
}

impl Workspace {
    async fn run(
        self,
        terminal: atman_tui::InheritedTerminal,
        notifications: Vec<atman_runtime::notify::Notification>,
    ) -> Result<()> {
        let Self {
            scopes,
            controls,
            mut input,
            forms,
            mut handle,
            service,
        } = self;
        handle
            .boot_toasts
            .extend(atman_tui::boot_animation::toasts(notifications));
        let mut views = HashMap::from([(service.binding.view_id, service)]);
        let mut navigation = tokio::task::JoinSet::new();
        let mut operations = operations::Operations::new();
        let ui = atman_tui::run_tui_ex(handle, Some(terminal));
        tokio::pin!(ui);
        loop {
            // Drain controls already submitted by a closed view before releasing its services.
            if input.is_empty() {
                views.retain(|_, view| !view.can_detach());
            }
            tokio::select! {
                result = &mut ui => return result,
                result = operations.next() => {
                    let ((replies, submitted), result): ((ControlReplies, bool), Result<String>) = result;
                    let (message, level) = match &result {
                        Ok(message) => (message.clone(), atman_tui::app::NoteLevel::Info),
                        Err(error) => (format!("{error:#}"), atman_tui::app::NoteLevel::Error),
                    };
                    let _ = replies.send(TuiCommand::Toast { message, level });
                    if submitted {
                        let _ = replies.send(TuiCommand::SubmissionResult { result: result.map(|_| ()).map_err(|error| format!("{error:#}")) });
                    }
                }
                Some(result) = navigation.join_next() => {
                    let (replies, result): (ControlReplies, Result<Navigation>) = result?;
                    match result {
                        Ok(Navigation::Catalog(page)) => { let _ = replies.send(TuiCommand::FlowCatalog { result: Ok(page) }); }
                        Ok(Navigation::Resolved(flow)) => { let _ = replies.send(TuiCommand::FlowResolved { flow }); }
                        Ok(Navigation::Projects(command)) => { let _ = replies.send(*command); }
                        Ok(Navigation::Focus { flow, placement, handle, service }) => {
                            views.insert(service.binding.view_id, *service);
                            let _ = replies.send(TuiCommand::FocusFlow { flow, placement, attachment: Some(handle) });
                        }
                        Err(error) => { let _ = replies.send(TuiCommand::Toast { message: format!("{error:#}"), level: atman_tui::app::NoteLevel::Error }); }
                    }
                }
                result = next_panel_completion(&mut views) => { result?; }
                Some(control) = input.recv() => {
                    let BoundControl { ticket, command, replies } = control;
                    let Some(view) = views.get_mut(&ticket.binding.view_id).filter(|view| view.binding == ticket.binding) else {
                        let _ = replies.send(TuiCommand::SubmissionResult { result: Err("view is no longer attached to this flow".into()) });
                        continue;
                    };
                    let result = match command {
                        TuiControl::Projects(request) => {
                            let current = view.binding.flow.session_target();
                            let project = view.project.clone();
                            let config = view.execution.as_ref().map(|execution| execution.scope.config.clone());
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let command = crate::tui_projects::request(
                                    scopes.root.clone(), current, project,
                                    config, Some(&scopes.catalog), request,
                                ).await;
                                (replies, Ok(Navigation::Projects(Box::new(command))))
                            });
                            continue;
                        }
                        TuiControl::NewSession => {
                            let Some(execution) = &view.execution else {
                                reject_archive_control(command, &replies);
                                continue;
                            };
                            let seed = (&execution.scope).into();
                            let (scopes, controls, forms) = (scopes.clone(), controls.clone(), forms.clone());
                            navigation.spawn(async move {
                                let result = scopes.create_tui_session(seed, ticket.request_id, controls, forms).await;
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::OpenManagement => {
                            let configuration = view.execution.as_ref().map(|execution| execution.scope.config.clone());
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = async {
                                    let configuration = configuration.map(Ok).unwrap_or_else(ConfigHub::global)?;
                                    scopes.open_management(configuration).await.map(|flow| Navigation::Resolved(flow.into()))
                                }.await;
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::BrowseFlows(request) => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move { let result = scopes.browse(request).await.map(Navigation::Catalog); (replies, result) });
                            continue;
                        }
                        TuiControl::FocusFlow { flow, placement, reuse } => {
                            if let Some(binding) = reuse {
                                if binding.flow == flow && views.get(&binding.view_id).is_some_and(|view| view.binding == binding) {
                                    let _ = replies.send(TuiCommand::FocusFlow { flow, placement, attachment: None });
                                    Ok(())
                                } else { Err(anyhow::anyhow!("view binding is no longer attached")) }
                            } else {
                                let (scopes, controls, forms) = (scopes.clone(), controls.clone(), forms.clone());
                                navigation.spawn(async move { let result = scopes.focus(flow, placement, controls, forms).await; (replies, result) });
                                continue;
                            }
                        }
                        command => {
                            let Some(execution) = view.execution.as_mut() else {
                                reject_archive_control(command, &replies);
                                continue;
                            };
                            let operation = match &command {
                                TuiControl::CompactNow => Some((operations::Operation::Compact, false)),
                                TuiControl::Submit(submission) => submission.control_command().and_then(operations::Operation::parse).map(|operation| (operation, true)),
                                _ => None,
                            };
                            if let Some((operation, submitted)) = operation {
                                match operations.start(&execution.scope, &execution.worker, operation, ticket.request_id, (replies.clone(), submitted)) {
                                    Ok(()) => { let _ = replies.send(TuiCommand::Toast { message: operation.notice().into(), level: atman_tui::app::NoteLevel::Info }); }
                                    Err(error) => {
                                        let message = format!("{error:#}");
                                        let _ = replies.send(TuiCommand::Toast { message: message.clone(), level: atman_tui::app::NoteLevel::Error });
                                        if submitted { let _ = replies.send(TuiCommand::SubmissionResult { result: Err(message) }); }
                                    }
                                }
                                continue;
                            }
                            let Some(command) = execution.panels.dispatch(command).await else { continue; };
                            execution.scope.tui_control(&execution.worker, ticket.request_id, command, &replies, &forms).await
                        },
                    };
                    if let Err(error) = result {
                        let _ = replies.send(TuiCommand::Toast { message: format!("{error:#}"), level: atman_tui::app::NoteLevel::Error });
                    }
                }
            }
        }
    }
}

struct ViewServices {
    binding: ViewBinding,
    project: Option<std::path::PathBuf>,
    commands: mpsc::UnboundedSender<TuiCommand>,
    execution: Option<ExecutionServices>,
}

struct ExecutionServices {
    scope: Attachment,
    worker: FlowWorker,
    panels: crate::tui_panels::PanelServices,
}

impl ViewServices {
    fn can_detach(&self) -> bool {
        self.execution.as_ref().map_or_else(
            || self.commands.is_closed(),
            |execution| execution.panels.can_detach(),
        )
    }
}

fn reject_archive_control(command: TuiControl, replies: &ControlReplies) {
    let error =
        "Archived flow is read-only; select an executable flow for this operation".to_owned();
    let command = match command {
        TuiControl::Submit(_) => TuiCommand::SubmissionResult { result: Err(error) },
        TuiControl::GoalControl {
            request_id: Some(request_id),
            ..
        } => TuiCommand::GoalSaved {
            request_id,
            result: Err(error),
        },
        TuiControl::MutateSettings(request) => TuiCommand::SettingsMutationResult {
            request,
            result: Err(error),
        },
        TuiControl::MutateProvider(request) => TuiCommand::ProviderMutationResult {
            request,
            result: Err(error),
        },
        TuiControl::MutateModel(request) => TuiCommand::ModelMutationResult {
            request,
            result: Err(error),
        },
        _ => TuiCommand::Toast {
            message: error,
            level: atman_tui::app::NoteLevel::Warn,
        },
    };
    let _ = replies.send(command);
}

enum Navigation {
    Resolved(ViewAddress),
    Projects(Box<TuiCommand>),
    Catalog(Box<atman_tui::flow_hub::FlowHubPage>),
    Focus {
        flow: ViewAddress,
        placement: atman_tui::control::FlowPlacement,
        handle: Box<TuiHandle>,
        service: Box<ViewServices>,
    },
}

impl scopes::Scopes {
    async fn create_tui_session(
        self: &Arc<Self>,
        seed: scopes::SessionSeed,
        request: Uuid,
        controls: mpsc::UnboundedSender<BoundControl>,
        forms: Arc<atman_runtime::session::FormRegistry>,
    ) -> Result<Navigation> {
        let flow = self
            .create_session(seed, request, scopes::CreationMode::Interactive)
            .await?;
        let mut navigation = self
            .focus(
                flow.into(),
                atman_tui::control::FlowPlacement::Main,
                controls,
                forms,
            )
            .await?;
        if let Navigation::Focus {
            handle, service, ..
        } = &mut navigation
        {
            let execution = service
                .execution
                .as_ref()
                .context("created session has no execution services")?;
            self.welcome(&execution.scope, handle).await;
        }
        Ok(navigation)
    }

    async fn welcome(&self, scope: &Attachment, handle: &mut TuiHandle) {
        let recent = match self.startup_recent(scope, 5).await {
            Ok(recent) => recent,
            Err(error) => {
                handle.boot_toasts.push(atman_tui::app::ToastNote {
                    id: "session-directory".into(),
                    message: format!("Recent sessions unavailable: {error:#}"),
                    level: atman_tui::app::NoteLevel::Warn,
                    ttl: std::time::Duration::from_secs(10),
                    created: std::time::Instant::now(),
                    position: atman_tui::app::ToastPosition::BottomRight,
                    fading: false,
                    fade_started: None,
                });
                Vec::new()
            }
        };
        handle.startup_card = Some(atman_tui::app::StartupCard {
            version: env!("CARGO_PKG_VERSION").into(),
            recent,
            directory: atman_tui::app::StartupDirectory::Projects,
        });
    }

    async fn browse(
        &self,
        request: atman_tui::flow_hub::HubRequest,
    ) -> Result<Box<atman_tui::flow_hub::FlowHubPage>> {
        use atman_tui::flow_hub::{
            FlowHubPage, HubRequest, PAGE_SIZE, RelationProgress, RelationRange,
        };
        Ok(Box::new(match request {
            HubRequest::Watchers { owner } => {
                let inspected = self.catalog.inspect_flow(&owner, |_| Ok(())).await?;
                FlowHubPage::Watchers(atman_tui::flow_hub::WatchDirectory {
                    title: format!("WatchHub / {}", inspected.descriptor.content.title()),
                    owner,
                    cursor: inspected
                        .journal
                        .cursor
                        .ok_or_else(|| anyhow::anyhow!("flow journal has no creation"))?,
                    trailing_bytes: inspected.journal.trailing_bytes,
                    records: inspected.watchers,
                })
            }
            HubRequest::Sessions { before } => FlowHubPage::Sessions(
                directory::sessions(&self.catalog, &self.root, before, PAGE_SIZE).await?,
            ),
            HubRequest::Flows { session, before } => {
                let overview = self.catalog.overview(session).await?;
                let page = self
                    .catalog
                    .instances(session, before, PAGE_SIZE, |_| Ok(()))
                    .await?;
                FlowHubPage::Flows {
                    session,
                    title: overview
                        .summary
                        .title
                        .unwrap_or_else(|| session.to_string()),
                    default_flow: overview.summary.default_flow,
                    page,
                    relations: self
                        .catalog
                        .relations(session)
                        .await
                        .map_err(|error| error.to_string()),
                }
            }
            HubRequest::LegacyFlows { session, after } => {
                let root = self.root.clone();
                tokio::task::spawn_blocking(move || -> Result<_> {
                    let directory = root.join("sessions").join(session.to_string());
                    let history = atman_runtime::projection::flow::LegacyHistory::inspect(
                        &directory.join("events.jsonl"),
                        session,
                    )?;
                    let all = history.flows();
                    let eligible = all
                        .iter()
                        .filter(|owner| after.as_ref().is_none_or(|after| *owner > after))
                        .cloned()
                        .collect::<Vec<_>>();
                    let next =
                        (eligible.len() > PAGE_SIZE).then(|| eligible[PAGE_SIZE - 1].clone());
                    Ok(FlowHubPage::LegacyFlows {
                        session,
                        title: atman_runtime::session_meta::SessionMeta::load(&directory)
                            .and_then(|meta| meta.title)
                            .unwrap_or_else(|| format!("Legacy session {session}")),
                        flows: eligible.into_iter().take(PAGE_SIZE).collect(),
                        next,
                        total: all.len(),
                    })
                })
                .await??
            }
            HubRequest::Relations {
                session,
                focus,
                range,
            } => {
                let (title, relations, progress) = match &range {
                    RelationRange::Session => {
                        let overview = self.catalog.overview(session).await?;
                        (
                            overview
                                .summary
                                .title
                                .unwrap_or_else(|| session.to_string()),
                            self.catalog.relations(session).await?,
                            None,
                        )
                    }
                    RelationRange::Catalog { before } => {
                        let batch = self.catalog.relation_page(*before, 8, |_| Ok(true)).await?;
                        let mut relations = Vec::new();
                        let mut progress = RelationProgress {
                            scanned: batch.sessions.len(),
                            total: batch.total,
                            next: batch.next,
                            errors: Vec::new(),
                        };
                        for entry in batch.sessions {
                            match entry.relations {
                                Ok(records) => relations.extend(records),
                                Err(error) => progress.errors.push((entry.session_id, error)),
                            }
                        }
                        ("Global".into(), relations, Some(progress))
                    }
                };
                let title = match &focus {
                    Some(flow) => format!(
                        "{title} / links for {}",
                        &flow.instance_id.to_string()[28..]
                    ),
                    None => format!("{title} / all links"),
                };
                FlowHubPage::Relations {
                    session,
                    focus,
                    title,
                    relations,
                    range,
                    progress,
                }
            }
        }))
    }

    async fn focus(
        &self,
        address: ViewAddress,
        placement: atman_tui::control::FlowPlacement,
        controls: mpsc::UnboundedSender<BoundControl>,
        forms: Arc<atman_runtime::session::FormRegistry>,
    ) -> Result<Navigation> {
        let flow = match &address {
            ViewAddress::Instance(flow) => flow,
            ViewAddress::Legacy { session, owner } => {
                let (root, session, owner) = (self.root.clone(), *session, owner.clone());
                let (view, meta) = tokio::task::spawn_blocking(move || -> Result<_> {
                    let directory = root.join("sessions").join(session.to_string());
                    let history = atman_runtime::projection::flow::LegacyHistory::inspect(
                        &directory.join("events.jsonl"),
                        session,
                    )?;
                    Ok((
                        history.read_flow(&owner)?,
                        atman_runtime::session_meta::SessionMeta::load(&directory),
                    ))
                })
                .await??;
                let project = meta.as_ref().and_then(|meta| meta.project_root.clone());
                let binding = ViewBinding {
                    view_id: Uuid::now_v7(),
                    revision: 1,
                    flow: address.clone(),
                };
                let mut handle = TuiHandle::from_legacy(
                    view,
                    binding.clone(),
                    controls,
                    meta.and_then(|meta| meta.title),
                    project.as_ref().map(|path| path.display().to_string()),
                )?;
                let (commands, rx) = mpsc::unbounded_channel();
                handle.cmd_rx = Some(rx);
                return Ok(Navigation::Focus {
                    flow: address,
                    placement,
                    handle: Box::new(handle),
                    service: Box::new(ViewServices {
                        binding,
                        project,
                        commands,
                        execution: None,
                    }),
                });
            }
        };
        if let Some((view, descriptor)) = archived_selection(
            &self.root,
            flow.session_id.0,
            Some(flow.instance_id.clone()),
        )
        .await?
        {
            let (handle, service) = archive_attachment(view, descriptor, controls)?;
            return Ok(Navigation::Focus {
                flow: address,
                placement,
                handle: Box::new(handle),
                service: Box::new(service),
            });
        }
        self.enable_management().await;
        let mut scope = self.open(flow.session_id.0).await?;
        scope.executor.tool_ctx.prompt_resolver = Some(Arc::new(
            atman_runtime::rendezvous::FormPromptResolver::new(forms.clone()),
        ));
        let worker = scope.session.open_flow(flow.instance_id.clone()).await?;
        let (handle, service, _) = scope.prepare_tui_view(&worker, controls, &forms).await?;
        Ok(Navigation::Focus {
            flow: address,
            placement,
            handle: Box::new(handle),
            service: Box::new(service),
        })
    }
}

async fn next_panel_completion(
    views: &mut HashMap<Uuid, ViewServices>,
) -> Result<(), tokio::task::JoinError> {
    let pending: Vec<_> = views
        .values_mut()
        .filter_map(|view| view.execution.as_mut())
        .filter(|execution| execution.panels.has_pending_work())
        .map(|execution| Box::pin(execution.panels.next_completion()))
        .collect();
    if pending.is_empty() {
        std::future::pending().await
    } else {
        futures::future::select_all(pending).await.0
    }
}

#[cfg(test)]
mod tests;
