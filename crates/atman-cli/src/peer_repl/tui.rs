//! Interactive controls for a persistent view. Durable acceptance precedes every input ACK.

use super::*;
mod directory;
mod new_session;
mod session_choices;
mod workspaces;
use atman_tui::{
    TuiCommand, TuiControl, TuiHandle, TuiSubmission,
    control::{BoundControl, ControlReplies, ViewAddress, ViewBinding},
};

impl Attachment {
    pub(super) async fn continued_session(
        &self,
    ) -> Result<Option<atman_runtime::flow_store::FlowRef>> {
        // CLI continuation sessions retain their initial entry identity even if the default flow changes.
        let initial = atman_rt::FlowInstanceId(self.session.id().into());
        if !self
            .session
            .snapshot()
            .await?
            .catalog
            .flows
            .iter()
            .any(|flow| flow.id == initial)
        {
            return Ok(None);
        }
        let worker = self.session.open_flow(initial).await?;
        let Some(source) = worker
            .launch_record()
            .await?
            .and_then(|record| record.input.continued_from)
        else {
            return Ok(None);
        };
        ensure!(
            atman_runtime::flow_store::history::LegacyArchive::continuation_id(
                source.session_id,
                &source.owner
            )? == self.session.id(),
            "historical provenance does not belong to this continuation session"
        );
        // Indexed recovery retains no original Session directory to delete.
        let root = self
            .data_root
            .canonicalize()
            .unwrap_or_else(|_| self.data_root.clone());
        if source
            .source
            .path
            .starts_with(root.join("runtime-v1/.index-recovery"))
        {
            return Ok(None);
        }
        Ok(Some(worker.address().clone()))
    }

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
        self = self.for_worker(&worker).await?;
        let forms = worker.context().forms();
        self.executor.tool_ctx.prompt_resolver = Some(Arc::new(
            atman_runtime::rendezvous::FormPromptResolver::new(forms.clone()),
        ));
        let (controls, input) = mpsc::unbounded_channel();
        let (mut handle, service, _) = self
            .prepare_tui_view(&worker, controls.clone(), &forms)
            .await?;
        handle.onboarding_recommended =
            self.config.models().is_first_run(&self.config.load_auth()?);
        scopes.welcome(&self, &mut handle).await;
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
        ensure!(
            worker.project().await? == self.project,
            "view and execution project differ"
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
        let continuation = self.continued_session().await;
        handle.project_root = self
            .project
            .as_ref()
            .map(|project| project.root().display().to_string());
        handle.form_rx = Some(forms.subscribe_for(worker.context().deferred_form_inbox()));
        handle.trust = self.config.trust_config()?;
        let (commands, command_rx) = mpsc::unbounded_channel();
        handle.cmd_rx = Some(command_rx);
        match continuation {
            Ok(owner) => handle.continued_session = owner,
            Err(error) => {
                let _ = commands.send(TuiCommand::Toast {
                    message: format!("Old session cleanup unavailable: {error:#}"),
                    level: atman_tui::app::NoteLevel::Warn,
                });
            }
        }
        let panels = crate::tui_panels::PanelServices::for_scope(
            worker.context(),
            self.executor.clone(),
            commands.clone(),
            vec![],
            self.config.clone(),
            self.project.clone(),
        )?;
        // Main and floating conversations attach the same retained session schedulers.
        // Subscribe the view first so any newly admitted work is observed normally.
        for message in self.restore_scheduling().await? {
            let _ = commands.send(TuiCommand::Toast {
                message,
                level: atman_tui::app::NoteLevel::Error,
            });
        }
        Ok((
            handle,
            ViewServices {
                new_session: None,
                historical_directory: None,
                binding,
                project: self
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
            TuiControl::EditInbox(edit) => {
                let request_id = edit.id;
                let result = worker
                    .edit_input(*edit, |edit| {
                        if edit.sender == SenderIdentity::User(USER) {
                            Ok(())
                        } else {
                            Err(std::io::Error::new(
                                std::io::ErrorKind::PermissionDenied,
                                "input belongs to another user",
                            ))
                        }
                    })
                    .await
                    .map_err(|error| error.to_string());
                let _ = replies.send(TuiCommand::InboxEdited { request_id, result });
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
                        atman_runtime::flow_store::control::ControlRequest {
                            id: request,
                            sender: SenderIdentity::User(USER),
                            target: worker.address().clone(),
                            expected_revision: worker.state().await?.revision(),
                            pause_key: Uuid::from_bytes(*b"atman-user-pause").into(),
                            action: PeerControl::CancelTurn,
                        },
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
            TuiControl::TermResize { handle, rows, cols } => {
                use atman_rt::Value;
                use atman_runtime::tool::{Tool, ToolArgs, ToolCtx};
                ensure!(rows > 0 && cols > 0, "terminal dimensions must be positive");
                let ctx = ToolCtx::new()
                    .with_session_id(context.id().to_string())
                    .with_term_registry(worker.resources().terminals().clone());
                atman_runtime::tools::term::TermResize
                    .call(
                        ToolArgs {
                            positional: vec![],
                            named: vec![
                                ("handle".into(), Value::Str(handle)),
                                ("rows".into(), Value::Int(i64::from(rows))),
                                ("cols".into(), Value::Int(i64::from(cols))),
                                ("wait_ms".into(), Value::Int(0)),
                            ],
                        },
                        &ctx,
                    )
                    .await?;
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
    archive: ArchivedSelection,
    scopes: Arc<scopes::Scopes>,
    terminal: atman_tui::InheritedTerminal,
    notifications: Vec<atman_runtime::notify::Notification>,
) -> Result<()> {
    let (controls, input) = mpsc::unbounded_channel();
    let (handle, service) = archive_attachment(archive, controls.clone())?;
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
    archive: ArchivedSelection,
    controls: mpsc::UnboundedSender<BoundControl>,
) -> Result<(TuiHandle, ViewServices)> {
    let ArchivedSelection {
        view,
        title,
        project,
    } = archive;
    let binding = ViewBinding {
        view_id: Uuid::now_v7(),
        revision: 1,
        flow: view.transcript().flow().clone().into(),
    };
    let mut handle = TuiHandle::from_archive(
        view,
        binding.clone(),
        controls,
        title,
        project
            .as_ref()
            .map(|project| project.root().display().to_string()),
    )?;
    let (commands, rx) = mpsc::unbounded_channel();
    handle.cmd_rx = Some(rx);
    Ok((
        handle,
        ViewServices {
            new_session: None,
            historical_directory: project.as_ref().map(|project| project.root().to_owned()),
            binding,
            project: project.as_ref().map(|project| project.root().to_owned()),
            commands,
            execution: None,
        },
    ))
}

pub(super) async fn interact_legacy(
    view: LegacyFlowView,
    meta: Option<atman_runtime::session_meta::SessionMeta>,
    scopes: Arc<scopes::Scopes>,
    terminal: atman_tui::InheritedTerminal,
    notifications: Vec<atman_runtime::notify::Notification>,
) -> Result<()> {
    let (controls, input) = mpsc::unbounded_channel();
    let (handle, service) = legacy_attachment(view, meta, controls.clone())?;
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

fn legacy_attachment(
    view: LegacyFlowView,
    meta: Option<atman_runtime::session_meta::SessionMeta>,
    controls: mpsc::UnboundedSender<BoundControl>,
) -> Result<(TuiHandle, ViewServices)> {
    let project = meta.as_ref().and_then(|meta| meta.project_root.clone());
    let historical_directory = meta
        .as_ref()
        .and_then(|meta| meta.start_path.clone())
        .or_else(|| project.clone());
    let binding = ViewBinding {
        view_id: Uuid::now_v7(),
        revision: 1,
        flow: ViewAddress::Legacy {
            session: view.session_id(),
            owner: view.owner().clone(),
        },
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
    Ok((
        handle,
        ViewServices {
            new_session: None,
            historical_directory,
            binding,
            project,
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
        let searches = atman_runtime::history_search::SearchService::default();
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
                        Ok(Navigation::Applied) => {}
                        Ok(Navigation::Catalog(page)) => { let _ = replies.send(TuiCommand::FlowCatalog { result: Ok(page) }); }
                        Ok(Navigation::Resolved(flow)) => { let _ = replies.send(TuiCommand::FlowResolved { flow }); }
                        Ok(Navigation::Reply(command)) => { let _ = replies.send(*command); }
                        Ok(Navigation::Created { flow, handle, service }) => {
                            views.insert(service.binding.view_id, *service);
                            let _ = replies.send(TuiCommand::SessionCreated { flow, attachment: handle });
                        }
                        Ok(Navigation::Focus { flow, placement, handle, service }) => {
                            views.insert(service.binding.view_id, *service);
                            let _ = replies.send(TuiCommand::FocusFlow { flow, placement, attachment: Some(handle) });
                        }
                        Ok(Navigation::Continued { flow, handle, service }) => {
                            views.insert(service.binding.view_id, *service);
                            let _ = replies.send(TuiCommand::SubmissionContinued { flow, attachment: handle });
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
                        TuiControl::Submit(submission) if view.execution.is_none() && submission.control_command().is_none() => {
                            let address = view.binding.flow.clone();
                            let (scopes, controls, forms) = (scopes.clone(), controls.clone(), forms.clone());
                            navigation.spawn(async move {
                                let result = scopes.continue_tui(address, ticket.request_id, submission, controls, forms).await;
                                let result = result.or_else(|error| Ok(Navigation::Reply(Box::new(TuiCommand::SubmissionResult { result: Err(format!("{error:#}")) }))));
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::ArchiveFlow(change) => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = scopes.archive_flow(*change).await.map(Navigation::Catalog);
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::SelectDefaultFlow(change) => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = scopes.select_default_flow(*change).await.map(Navigation::Catalog);
                                (replies, result)
                            });
                            continue;
                        }
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
                                (replies, Ok(Navigation::Reply(Box::new(command))))
                            });
                            continue;
                        }
                        TuiControl::NewProjectSession { fingerprint } => {
                            let (scopes, controls, forms) = (scopes.clone(), controls.clone(), forms.clone());
                            navigation.spawn(async move {
                                let root = scopes.root.clone();
                                let result = async {
                                    let seed = tokio::task::spawn_blocking(move || new_session::project_seed(&root, &fingerprint, ConfigHub::global()?)).await??;
                                    scopes.create_tui_session(seed, ticket.request_id, controls, forms).await
                                }.await.or_else(|error: anyhow::Error| Ok(Navigation::Reply(Box::new(TuiCommand::NewSessionFailed { request: ticket.request_id, error: format!("{error:#}") }))));
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::NewSession => {
                            match scopes.new_session_plan(view, ticket.request_id).await {
                                Ok(plan) => {
                                    if let Some(selection) = plan.direct() {
                                        match plan.seed(selection, &scopes.root) {
                                            Ok(seed) => {
                                                let (scopes, controls, forms) = (scopes.clone(), controls.clone(), forms.clone());
                                                navigation.spawn(async move {
                                                    let result = scopes.create_tui_session(seed, ticket.request_id, controls, forms).await;
                                                    (replies, result)
                                                });
                                                continue;
                                            }
                                            Err(error) => Err(error),
                                        }
                                    } else {
                                        let _ = replies.send(TuiCommand::NewSessionLocations(plan.locations.clone()));
                                        view.new_session = Some(plan);
                                        Ok(())
                                    }
                                }
                                Err(error) => Err(error),
                            }
                        }
                        TuiControl::CreateSession { request, selection } => {
                            let result = match view.new_session.as_ref().filter(|plan| plan.locations.id == request) {
                                Some(plan) => selection.map(|selection| plan.seed(selection, &scopes.root)).transpose(),
                                None => Err(anyhow::anyhow!("New Session choices have expired; open New Session again")),
                            };
                            match result {
                                Ok(Some(seed)) => {
                                    let (scopes, controls, forms) = (scopes.clone(), controls.clone(), forms.clone());
                                    navigation.spawn(async move {
                                        let result = scopes.create_tui_session(seed, request, controls, forms).await;
                                        let result = result.or_else(|error| Ok(Navigation::Reply(Box::new(TuiCommand::NewSessionFailed { request, error: format!("{error:#}") }))));
                                        (replies, result)
                                    });
                                }
                                Ok(None) => view.new_session = None,
                                Err(error) => { let _ = replies.send(TuiCommand::NewSessionFailed { request, error: format!("{error:#}") }); }
                            }
                            continue;
                        }
                        TuiControl::ListSessionChoices => {
                            let scopes = scopes.clone();
                            let current = view.binding.flow.clone();
                            navigation.spawn(async move {
                                let result = scopes.session_choices(current).await.map_err(|error| format!("{error:#}"));
                                (replies, Ok(Navigation::Reply(Box::new(TuiCommand::SessionChoices { result }))))
                            });
                            continue;
                        }
                        TuiControl::SearchHistory { request, cancellation } => {
                            use atman_runtime::history_search::SearchSource;
                            let root = scopes.root.clone();
                            let source = match &view.binding.flow {
                                ViewAddress::Instance(flow) => SearchSource::Instance(flow.clone()),
                                ViewAddress::Legacy { session, .. } => SearchSource::Legacy(*session),
                            };
                            let searches = searches.clone();
                            navigation.spawn(async move {
                                let result = searches.search(root, source, request, cancellation).await
                                    .map_err(|error| format!("{error:#}"));
                                (replies, Ok(Navigation::Reply(Box::new(TuiCommand::HistorySearchResults { result }))))
                            });
                            continue;
                        }
                        TuiControl::OpenLegacySession(session) => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = scopes.resolve_legacy_session(session).await.map(Navigation::Resolved);
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::RecoverIndexedHistory(review) => {
                            let root = scopes.root.clone();
                            navigation.spawn(async move {
                                let result = tokio::task::spawn_blocking(move || -> Result<_> {
                                    let configuration = ConfigHub::global()?;
                                    let session = atman_runtime::history_index::recover(&root, &review, &configuration)?;
                                    let read = RuntimeStore::inspect_session(&root, session)?;
                                    let instance_id = read.projection.default_flow.context("Recovered history has no default flow")?;
                                    Ok(Navigation::Resolved(atman_runtime::flow_store::FlowRef { session_id: session.into(), instance_id }.into()))
                                }).await.map_err(anyhow::Error::from).and_then(|result| result);
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::RenameSessionChoice(change) => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = scopes.rename_session_choice(*change).await.map_err(|error| format!("{error:#}"));
                                (replies, Ok(Navigation::Reply(Box::new(TuiCommand::SessionChoiceRenamed { result }))))
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
                        TuiControl::ChangeSessionProject(change) => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = scopes.change_session_project(*change).await.map(Navigation::Catalog);
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::SubmitControls(request) => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = scopes.submit_controls(*request).await.map(Navigation::Catalog);
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::RetryControls(id) => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = scopes.retry_controls(id).await.map(Navigation::Catalog);
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::UpdateManagement(change) => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = scopes.update_management(*change).await.map(Navigation::Catalog);
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::SelectManagement(change) => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = scopes.select_management(*change).await.map(Navigation::Catalog);
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::SetWorkspaceRetention(change) => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = async {
                                    let request = atman_tui::flow_hub::HubRequest::WorkspaceRecord { location: change.location.clone(), id: change.expected.id.clone() };
                                    workspaces::retain(*change).await?;
                                    scopes.browse(request).await.context("Retention saved, but reading updated details failed; refresh to inspect the current state").map(Navigation::Catalog)
                                }.await;
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::ReleaseWorkspace(change) => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = async {
                                    let request = atman_tui::flow_hub::HubRequest::WorkspaceRecord { location: change.location.clone(), id: change.expected.id.clone() };
                                    workspaces::release(*change).await?;
                                    scopes.browse(request).await.context("Checkout removal committed, but reading updated details failed; refresh to inspect the current state").map(Navigation::Catalog)
                                }.await;
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::RemoveSession(request) => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = async {
                                    scopes.remove_session(*request).await?;
                                    let _ = replies.send(TuiCommand::Toast {
                                        message: "Session deleted".into(),
                                        level: atman_tui::app::NoteLevel::Info,
                                    });
                                    scopes.browse(atman_tui::flow_hub::HubRequest::Sessions { before: None }).await
                                        .context("Session deletion committed, but refreshing the directory failed; retry or refresh")
                                        .map(Navigation::Catalog)
                                }.await;
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::CancelWatcher { owner, id } => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = async {
                                    scopes.cancel_watcher(owner.clone(), id).await?;
                                    scopes.browse(atman_tui::flow_hub::HubRequest::Watchers { owner }).await
                                        .context("Cancellation committed, but reading updated details failed")
                                        .map(Navigation::Catalog)
                                }.await;
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::ReplaceWatcher { owner, expected, replacement } => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = async {
                                    scopes.replace_watcher(owner.clone(), expected, *replacement).await?;
                                    scopes.browse(atman_tui::flow_hub::HubRequest::Watchers { owner }).await
                                        .context("Replacement committed, but reading updated details failed")
                                        .map(Navigation::Catalog)
                                }.await;
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::RegisterWatcher { owner, registration } => {
                            let scopes = scopes.clone();
                            navigation.spawn(async move {
                                let result = async {
                                    match registration {
                                        atman_tui::flow_hub::WatchDefinition::Flow(subscription) => scopes.register_watcher(owner.clone(), subscription).await?,
                                        atman_tui::flow_hub::WatchDefinition::Process(watch) => scopes.register_process_watcher(owner.clone(), watch).await?,
                                    }
                                    scopes.browse(atman_tui::flow_hub::HubRequest::Watchers { owner }).await
                                        .context("Registration committed, but reading updated details failed")
                                        .map(Navigation::Catalog)
                                }.await;
                                (replies, result)
                            });
                            continue;
                        }
                        TuiControl::FocusFlow { flow, placement, reuse } => {
                            let reuse = reuse.map(|binding| {
                                views.get(&binding.view_id)
                                    .filter(|view| view.binding == binding && binding.flow == flow)
                                    .map(ViewServices::can_reuse)
                            });
                            match reuse {
                                Some(None) => Err(anyhow::anyhow!("view binding is no longer attached")),
                                Some(Some(true)) => {
                                    let _ = replies.send(TuiCommand::FocusFlow { flow, placement, attachment: None });
                                    Ok(())
                                }
                                _ => {
                                    let (scopes, controls, forms) = (scopes.clone(), controls.clone(), forms.clone());
                                    navigation.spawn(async move { let result = scopes.focus(flow, placement, controls, forms).await; (replies, result) });
                                    continue;
                                }
                            }
                        }
                        command => {
                            let Some(execution) = view.execution.as_mut() else {
                                reject_archive_control(command, &replies);
                                continue;
                            };
                            if matches!(command, TuiControl::TermResize { .. }) {
                                let scope = execution.scope.clone();
                                let worker = execution.worker.clone();
                                let forms = forms.clone();
                                navigation.spawn(async move {
                                    let result = scope.tui_control(&worker, ticket.request_id, command, &replies, &forms).await.map(|()| Navigation::Applied);
                                    (replies, result)
                                });
                                continue;
                            }
                            let operation = match &command {
                                TuiControl::CompactNow => Some((operations::Operation::Compact, false)),
                                TuiControl::AutoNameSession => Some((operations::Operation::NameSession, false)),
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
    new_session: Option<new_session::Plan>,
    historical_directory: Option<std::path::PathBuf>,
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
    fn can_reuse(&self) -> bool {
        self.execution
            .as_ref()
            .is_none_or(|execution| !execution.worker.is_stopped())
    }

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
        TuiControl::EditInbox(edit) => TuiCommand::InboxEdited {
            request_id: edit.id,
            result: Err(error),
        },
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
    Applied,
    Resolved(ViewAddress),
    Reply(Box<TuiCommand>),
    Catalog(Box<atman_tui::flow_hub::FlowHubPage>),
    Created {
        flow: ViewAddress,
        handle: Box<TuiHandle>,
        service: Box<ViewServices>,
    },
    Continued {
        flow: ViewAddress,
        handle: Box<TuiHandle>,
        service: Box<ViewServices>,
    },
    Focus {
        flow: ViewAddress,
        placement: atman_tui::control::FlowPlacement,
        handle: Box<TuiHandle>,
        service: Box<ViewServices>,
    },
}

impl scopes::Scopes {
    pub(super) async fn continuation_removal(
        &self,
        owner: atman_runtime::flow_store::FlowRef,
    ) -> Result<Box<atman_tui::flow_hub::SessionRemovalPage>> {
        let source = self
            .host
            .flow(&owner)
            .await?
            .launch_record()
            .await?
            .and_then(|record| record.input.continued_from)
            .context("this session has no inherited legacy history")?;
        let mut page = directory::removal(
            &self.root,
            atman_tui::project_hub::SessionTarget::Legacy(source.session_id.to_string()),
        )
        .await?;
        let atman_tui::flow_hub::SessionRemovalRequest::Legacy(request) = &page.request else {
            bail!("historical source is not an original session");
        };
        ensure!(
            request.directory.join("events.jsonl") == source.source.path,
            "original history belongs to another storage location; no local session was selected for deletion"
        );
        let root = self.root.clone();
        let cleanup = page.cleanup;
        tokio::task::spawn_blocking(move || -> Result<()> {
            if cleanup {
                ensure!(RuntimeStore::pending_legacy_removals(&root)?.contains(&source.session_id),
                    "the original session has already been deleted");
            } else {
                source.source.verify().context(
                    "original history changed since continuation; inspect it separately before deleting"
                )?;
            }
            Ok(())
        }).await??;
        page.continuation = Some(owner);
        Ok(page)
    }

    async fn continue_tui(
        self: &Arc<Self>,
        source: ViewAddress,
        request: Uuid,
        submission: TuiSubmission,
        controls: mpsc::UnboundedSender<BoundControl>,
        forms: Arc<atman_runtime::session::FormRegistry>,
    ) -> Result<Navigation> {
        let flow = self.continue_history(source).await?;
        let Navigation::Focus {
            flow,
            handle,
            service,
            ..
        } = self
            .focus(
                flow.into(),
                atman_tui::control::FlowPlacement::Main,
                controls,
                forms,
            )
            .await?
        else {
            bail!("continuation did not attach an executable view");
        };
        let execution = service
            .execution
            .as_ref()
            .context("continuation has no execution services")?;
        // Acceptance is the last fallible operation; UI attachment failures must not invite duplicate input.
        execution
            .scope
            .submit_tui(&execution.worker, request, submission)
            .await?;
        Ok(Navigation::Continued {
            flow,
            handle,
            service,
        })
    }

    async fn change_session_project(
        &self,
        change: atman_tui::flow_hub::SessionProjectChange,
    ) -> Result<Box<atman_tui::flow_hub::FlowHubPage>> {
        let session = self.host.open_session(&self.root, change.session).await?;
        let catalog = session.snapshot().await?.catalog;
        let project = match change.project {
            Some(path) => {
                ensure!(path.is_absolute(), "Enter an absolute project directory");
                ensure!(
                    path.is_dir(),
                    "Project directory does not exist: {}",
                    path.display()
                );
                let SessionConfiguration::Atman(source) = &catalog.descriptor.configuration else {
                    bail!(
                        "Session needs a recorded Atman configuration to resolve project storage"
                    );
                };
                Some(atman_runtime::storage::ProjectContext::resolve(
                    &ConfigHub::from_source(source)?,
                    &path.canonicalize()?,
                    &self.root,
                )?)
            }
            None => None,
        };
        session.set_project(change.expected, project).await?;
        self.browse(atman_tui::flow_hub::HubRequest::Flows {
            session: change.session,
            before: None,
        })
        .await
        .context("Project association saved, but refreshing the directory failed; retry or refresh")
    }

    async fn select_management(
        &self,
        change: atman_tui::flow_hub::ManagementSelection,
    ) -> Result<Box<atman_tui::flow_hub::FlowHubPage>> {
        self.profile()
            .await?
            .select(
                change.request_id,
                change.expected,
                change.session_id,
                vec![],
            )
            .await?;
        self.browse(atman_tui::flow_hub::HubRequest::Management { grant: None })
            .await
            .context("Management saved, but reading updated details failed; refresh to inspect the current state")
    }

    async fn update_management(
        &self,
        change: atman_runtime::flow_store::profile::ReplaceManagement,
    ) -> Result<Box<atman_tui::flow_hub::FlowHubPage>> {
        self.profile().await?.replace(change).await?;
        self.browse(atman_tui::flow_hub::HubRequest::Management { grant: None })
            .await
            .context("Management saved, but reading updated details failed; refresh to inspect the current state")
    }

    async fn create_tui_session(
        self: &Arc<Self>,
        mut seed: scopes::SessionSeed,
        request: Uuid,
        controls: mpsc::UnboundedSender<BoundControl>,
        forms: Arc<atman_runtime::session::FormRegistry>,
    ) -> Result<Navigation> {
        seed.prompt_resolver = Some(Arc::new(
            atman_runtime::rendezvous::FormPromptResolver::new(forms.clone()),
        ));
        let flow = self
            .create_session(seed, request, scopes::CreationMode::Interactive)
            .await?;
        let Navigation::Focus {
            flow,
            mut handle,
            service,
            ..
        } = self
            .focus(
                flow.into(),
                atman_tui::control::FlowPlacement::Main,
                controls,
                forms,
            )
            .await?
        else {
            bail!("created session has no view attachment")
        };
        let execution = service
            .execution
            .as_ref()
            .context("created session has no execution services")?;
        self.welcome(&execution.scope, &mut handle).await;
        Ok(Navigation::Created {
            flow,
            handle,
            service,
        })
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

    async fn submit_controls(
        &self,
        request: atman_tui::flow_hub::OperatorControls,
    ) -> Result<Box<atman_tui::flow_hub::FlowHubPage>> {
        use atman_runtime::flow_store::{
            control::{ControlAction, ControlRequest},
            inbox::SenderIdentity,
            profile::ControlBatchRequest,
        };
        let sender = SenderIdentity::User(super::USER);
        anyhow::ensure!(
            !request.id.is_nil() && !request.commands.is_empty(),
            "Control batch needs an identity and targets"
        );
        let mut targets = std::collections::HashSet::new();
        let mut commands = Vec::with_capacity(request.commands.len());
        for command in request.commands {
            anyhow::ensure!(
                !command.target.session_id.0.is_nil()
                    && !command.target.instance_id.0.0.is_nil()
                    && targets.insert(command.target.clone()),
                "Control targets must be distinct valid instances"
            );
            let pause_key = match (command.action, command.pause_key) {
                (ControlAction::ReleasePause, Some(key)) if !key.is_nil() => key,
                (ControlAction::ReleasePause, _) => {
                    anyhow::bail!("Release pause requires a selected pause key")
                }
                (_, Some(_)) => anyhow::bail!("Only Release pause accepts a pause key"),
                (_, None) => uuid::Uuid::from_bytes(*b"atman-user-pause"),
            };
            commands.push(ControlRequest {
                id: request.id,
                sender: sender.clone(),
                target: command.target,
                expected_revision: command.expected_revision,
                pause_key: pause_key.into(),
                action: command.action,
            });
        }
        self.profile().await?;
        self.host
            .submit_controls(
                ControlBatchRequest {
                    id: request.id,
                    sender,
                    commands,
                },
                |_| Ok(()),
            )
            .await?;
        self.browse(atman_tui::flow_hub::HubRequest::ControlBatch {
            request_id: request.id,
        })
        .await
        .context(
            "Controls accepted, but reading results failed; retry retains the original request",
        )
    }

    async fn operator_control_record(
        &self,
        id: Uuid,
    ) -> Result<atman_runtime::flow_store::profile::ControlBatchRecord> {
        self.profile()
            .await?
            .control_batch(
                atman_runtime::flow_store::inbox::SenderIdentity::User(super::USER),
                id,
            )
            .await?
            .ok_or_else(|| anyhow::anyhow!("Control batch not found"))
    }

    async fn retry_controls(&self, id: Uuid) -> Result<Box<atman_tui::flow_hub::FlowHubPage>> {
        let record = self.operator_control_record(id).await?;
        self.host
            .submit_controls(
                atman_runtime::flow_store::profile::ControlBatchRequest {
                    id: record.id,
                    sender: record.sender,
                    commands: record.items.into_iter().map(|item| item.request).collect(),
                },
                |_| Ok(()),
            )
            .await?;
        self.browse(atman_tui::flow_hub::HubRequest::ControlBatch { request_id: id })
            .await
    }

    async fn select_default_flow(
        &self,
        change: atman_tui::flow_hub::FlowCatalogSelection,
    ) -> Result<Box<atman_tui::flow_hub::FlowHubPage>> {
        let session = self
            .host
            .open_session(&self.root, change.flow.session_id.0)
            .await?;
        session
            .select_default(change.expected, change.flow.instance_id.clone())
            .await?;
        self.browse(atman_tui::flow_hub::HubRequest::FlowDetails { owner: change.flow }).await
            .context("Default flow saved, but reading updated details failed; refresh to inspect the current state")
    }

    async fn archive_flow(
        &self,
        change: atman_tui::flow_hub::FlowArchiveChange,
    ) -> Result<Box<atman_tui::flow_hub::FlowHubPage>> {
        let selection = change.selection;
        let session = self
            .host
            .open_session(&self.root, selection.flow.session_id.0)
            .await?;
        session
            .set_flow_archived(
                selection.expected,
                selection.flow.instance_id.clone(),
                change.archived,
            )
            .await?;
        self.browse(atman_tui::flow_hub::HubRequest::FlowDetails { owner: selection.flow }).await
            .context("Flow visibility saved, but reading updated details failed; refresh to inspect the current state")
    }

    async fn browse(
        &self,
        request: atman_tui::flow_hub::HubRequest,
    ) -> Result<Box<atman_tui::flow_hub::FlowHubPage>> {
        use atman_tui::flow_hub::{
            FlowHubPage, HubRequest, PAGE_SIZE, RelationProgress, RelationRange,
        };
        Ok(Box::new(match request {
            HubRequest::SessionProject { session } => {
                let summary = self.catalog.overview(session).await?.summary;
                ensure!(
                    summary.trailing_bytes.is_none_or(|bytes| bytes == 0),
                    "Session journal has an incomplete tail"
                );
                FlowHubPage::SessionProject(Box::new(atman_tui::flow_hub::SessionProjectPage::new(
                    session,
                    summary.catalog.context("Session catalog is unavailable")?,
                    summary.title.unwrap_or_else(|| session.to_string()),
                    summary.project_root,
                )))
            }
            HubRequest::FlowDetails { owner } => {
                let summary = self.catalog.overview(owner.session_id.0).await?.summary;
                let expected = summary.catalog.context("Session catalog is unavailable")?;
                ensure!(
                    summary.trailing_bytes.is_none_or(|bytes| bytes == 0),
                    "Session journal has an incomplete tail"
                );
                let inspected = self.catalog.inspect_flow(&owner, |_| Ok(())).await?;
                FlowHubPage::FlowDetails(Box::new(atman_tui::flow_hub::FlowDetailsPage {
                    is_default: summary.default_flow.as_ref() == Some(&owner.instance_id),
                    selection: atman_tui::flow_hub::FlowCatalogSelection {
                        flow: owner,
                        expected,
                    },
                    title: inspected.descriptor.content.title().into(),
                    recorded_state: inspected.state.state().clone(),
                    archived: inspected.archived,
                }))
            }
            HubRequest::ControlHistory { before } => FlowHubPage::ControlHistory(
                self.profile()
                    .await?
                    .control_batches(
                        atman_runtime::flow_store::inbox::SenderIdentity::User(super::USER),
                        before,
                        PAGE_SIZE,
                    )
                    .await?,
            ),
            HubRequest::ControlBatch { request_id } => {
                let record = self.operator_control_record(request_id).await?;
                let issues = self.host.subscribe_controls().borrow().issues.clone();
                FlowHubPage::ControlBatch(Box::new(atman_tui::flow_hub::ControlBatchPage {
                    record,
                    issues,
                }))
            }
            HubRequest::Management { grant } => {
                let view = self.profile().await?.snapshot().await?;
                FlowHubPage::Management(Box::new(
                    atman_tui::flow_hub::ManagementPage::new(view, grant)
                        .map_err(anyhow::Error::msg)?,
                ))
            }
            HubRequest::Overview { after } => {
                let page = self
                    .catalog
                    .overview_page(after.clone(), 8, |_| Ok(true), |_| Ok(()))
                    .await?;
                FlowHubPage::Overview(atman_tui::flow_hub::OverviewDirectory::new(after, page))
            }
            HubRequest::SessionOverview { session, after } => {
                ensure!(
                    after
                        .as_ref()
                        .is_none_or(|cursor| cursor.session_id == session),
                    "Overview cursor belongs to another session"
                );
                let overview = self.catalog.overview(session).await?;
                let title = overview
                    .summary
                    .title
                    .unwrap_or_else(|| "This session".into());
                let page = self
                    .catalog
                    .overview_page(after.clone(), 8, |id| Ok(id == session), |_| Ok(()))
                    .await?;
                FlowHubPage::Overview(
                    atman_tui::flow_hub::OverviewDirectory::new(after, page)
                        .for_session(session, title),
                )
            }
            HubRequest::FlowWorkspace { owner } => {
                FlowHubPage::FlowWorkspace(workspaces::binding(&self.catalog, owner).await?)
            }
            HubRequest::Workspaces { after } => FlowHubPage::Workspaces(
                workspaces::directory(&self.catalog, self.root.clone(), after).await?,
            ),
            HubRequest::Workspace { location } => {
                FlowHubPage::Workspace(workspaces::inspect(location).await?)
            }
            HubRequest::WorkspaceRecord { location, id } => {
                let snapshot = workspaces::inspect(location.clone()).await?;
                let workspace = snapshot
                    .workspaces
                    .into_iter()
                    .find(|workspace| workspace.record.id == id)
                    .context("Workspace is no longer registered; refresh the project directory")?;
                FlowHubPage::WorkspaceRecord(Box::new(atman_tui::flow_hub::WorkspaceRecordPage {
                    location,
                    workspace,
                }))
            }
            HubRequest::Inbox {
                owner,
                side,
                before,
            } => FlowHubPage::Inbox(
                self.catalog
                    .inbox_page(&owner, side, before, PAGE_SIZE, |_| Ok(()))
                    .await?,
            ),
            HubRequest::WatchCatalog { owner, after } => {
                let page = self
                    .catalog
                    .watch_page(after.clone(), 8, |_| Ok(true), |_| Ok(()))
                    .await?;
                FlowHubPage::Watchers(atman_tui::flow_hub::WatchDirectory::catalog(
                    owner, after, page,
                ))
            }
            HubRequest::ProcessSources { owner } => {
                let sources = self.process_watch_sources(&owner).await?;
                FlowHubPage::ProcessSources { owner, sources }
            }
            HubRequest::Watchers { owner } => {
                let inspected = self.catalog.inspect_flow(&owner, |_| Ok(())).await?;
                FlowHubPage::Watchers(atman_tui::flow_hub::WatchDirectory::flow(
                    owner,
                    atman_runtime::flow_runtime::catalog::WatchCatalogSnapshot {
                        title: inspected.descriptor.content.title().into(),
                        cursor: inspected
                            .journal
                            .cursor
                            .ok_or_else(|| anyhow::anyhow!("flow journal has no creation"))?,
                        trailing_bytes: inspected.journal.trailing_bytes,
                        records: inspected.watchers,
                    },
                ))
            }
            HubRequest::Sessions { before } => FlowHubPage::Sessions(
                directory::sessions(&self.catalog, &self.root, before, PAGE_SIZE).await?,
            ),
            HubRequest::SessionRemoval { session } => {
                FlowHubPage::SessionRemoval(directory::removal(&self.root, session).await?)
            }
            HubRequest::ContinuationRemoval { owner } => {
                FlowHubPage::SessionRemoval(self.continuation_removal(owner).await?)
            }
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
            HubRequest::IndexedHistory { source } => {
                let root = self.root.clone();
                let review = tokio::task::spawn_blocking(move || {
                    atman_runtime::history_index::review(&root, &source)
                })
                .await??;
                FlowHubPage::IndexedHistory(Box::new(review))
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
                let (view, meta) =
                    legacy_selection(&self.root, *session, Some(owner.clone())).await?;
                let (handle, service) = legacy_attachment(view, meta, controls)?;
                return Ok(Navigation::Focus {
                    flow: address,
                    placement,
                    handle: Box::new(handle),
                    service: Box::new(service),
                });
            }
        };
        if let Some(archive) = archived_selection(
            &self.root,
            flow.session_id.0,
            Some(flow.instance_id.clone()),
        )
        .await?
        {
            let (handle, service) = archive_attachment(archive, controls)?;
            return Ok(Navigation::Focus {
                flow: address,
                placement,
                handle: Box::new(handle),
                service: Box::new(service),
            });
        }
        let mut scope = self.open_flow_scope(flow).await?;
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
