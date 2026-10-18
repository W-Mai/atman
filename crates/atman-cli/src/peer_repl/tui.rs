//! Interactive controls for a persistent view. Durable acceptance precedes every input ACK.

use super::*;
use atman_tui::{
    TuiCommand, TuiControl, TuiHandle, TuiSubmission,
    control::{BoundControl, ControlReplies, ViewBinding},
};

impl Attachment {
    pub(super) async fn interact_tui(
        mut self,
        root: &Path,
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
        let (controls, mut input) = mpsc::unbounded_channel();
        let (mut handle, service, commands) = self
            .prepare_tui_view(&worker, controls.clone(), &forms)
            .await?;
        handle.onboarding_recommended =
            self.config.models().is_first_run(&self.config.load_auth()?);
        handle
            .boot_toasts
            .extend(atman_tui::boot_animation::toasts(notifications));
        let mut views = HashMap::from([(service.binding.view_id, service)]);
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
        let scopes = Arc::new(scopes::Scopes::new(root, self));
        let mut navigation = tokio::task::JoinSet::new();
        let ui = atman_tui::run_tui_ex(handle, Some(terminal));
        tokio::pin!(ui);
        loop {
            // Drain controls already submitted by a closed view before releasing its services.
            if input.is_empty() {
                views.retain(|_, view| !view.panels.can_detach());
            }
            tokio::select! {
                result = &mut ui => return result,
                Some(result) = navigation.join_next() => {
                    let (replies, result): (ControlReplies, Result<Navigation>) = result?;
                    match result {
                        Ok(Navigation::Catalog(page)) => { let _ = replies.send(TuiCommand::FlowCatalog { result: Ok(page) }); }
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
                    let worker = view.worker.clone();
                    let scope = view.scope.clone();
                    let Some(command) = view.panels.dispatch(command).await else { continue; };
                    let result = match command {
                        TuiControl::NewSession => {
                            let (scopes, controls, forms) = (scopes.clone(), controls.clone(), forms.clone());
                            navigation.spawn(async move {
                                let result = scopes.create_tui_session((&scope).into(), ticket.request_id, controls, forms).await;
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
                        command => scope.tui_control(&worker, ticket.request_id, command, &replies, &forms).await,
                    };
                    if let Err(error) = result {
                        let _ = replies.send(TuiCommand::Toast { message: format!("{error:#}"), level: atman_tui::app::NoteLevel::Error });
                    }
                }
            }
        }
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
            flow: worker.address().clone(),
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
                scope: self.clone(),
                binding,
                worker: worker.clone(),
                panels,
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
                    self.command_with_request(worker, command, request)
                        .await
                        .map(|message| {
                            let _ = replies.send(TuiCommand::Toast {
                                message,
                                level: atman_tui::app::NoteLevel::Info,
                            });
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

struct ViewServices {
    scope: Attachment,
    binding: ViewBinding,
    worker: FlowWorker,
    panels: crate::tui_panels::PanelServices,
}

enum Navigation {
    Catalog(Box<atman_tui::flow_hub::FlowHubPage>),
    Focus {
        flow: atman_runtime::flow_store::FlowRef,
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
                flow,
                atman_tui::control::FlowPlacement::Main,
                controls,
                forms,
            )
            .await?;
        if let Navigation::Focus {
            handle, service, ..
        } = &mut navigation
        {
            let recent = match self.startup_recent(&service.scope, 5).await {
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
                directory: atman_tui::app::StartupDirectory::Sessions,
            });
        }
        Ok(navigation)
    }

    async fn browse(
        &self,
        request: atman_tui::flow_hub::HubRequest,
    ) -> Result<Box<atman_tui::flow_hub::FlowHubPage>> {
        use atman_tui::flow_hub::{FlowHubPage, HubRequest, PAGE_SIZE};
        Ok(Box::new(match request {
            HubRequest::Sessions { before } => {
                FlowHubPage::Sessions(self.catalog.page(before, PAGE_SIZE, |_| Ok(true)).await?)
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
                }
            }
        }))
    }

    async fn focus(
        &self,
        flow: atman_runtime::flow_store::FlowRef,
        placement: atman_tui::control::FlowPlacement,
        controls: mpsc::UnboundedSender<BoundControl>,
        forms: Arc<atman_runtime::session::FormRegistry>,
    ) -> Result<Navigation> {
        let mut scope = self.open(flow.session_id.0).await?;
        scope.executor.tool_ctx.prompt_resolver = Some(Arc::new(
            atman_runtime::rendezvous::FormPromptResolver::new(forms.clone()),
        ));
        let worker = scope.session.open_flow(flow.instance_id.clone()).await?;
        let (handle, service, _) = scope.prepare_tui_view(&worker, controls, &forms).await?;
        Ok(Navigation::Focus {
            flow,
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
        .filter(|view| view.panels.has_pending_work())
        .map(|view| Box::pin(view.panels.next_completion()))
        .collect();
    if pending.is_empty() {
        std::future::pending().await
    } else {
        futures::future::select_all(pending).await.0
    }
}

#[cfg(test)]
mod tests;
