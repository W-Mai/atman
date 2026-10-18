//! Interactive controls for a persistent view. Durable acceptance precedes every input ACK.

use super::*;
use atman_tui::{
    TuiCommand, TuiControl, TuiHandle, TuiSubmission,
    control::{BoundControl, ControlReplies, ViewBinding},
};

impl Attachment {
    pub(super) async fn interact_tui(mut self) -> Result<()> {
        ensure!(
            self.config.source() == ConfigHub::global()?.source(),
            "TUI settings require the session configuration directory; select it with ATMAN_CONFIG_DIR or use ATMAN_REPL_NON_INTERACTIVE=1"
        );
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
        let binding = ViewBinding {
            view_id: Uuid::now_v7(),
            revision: 1,
            flow: worker.address().clone(),
        };
        let (controls, mut input) = mpsc::unbounded_channel();
        // Establish the source and interactive transport before starting schedulers.
        let mut handle = TuiHandle::from_flow(&worker, binding.clone(), controls).await?;
        handle.session_name = Some(self.descriptor.title.clone());
        handle.project_root = self
            .descriptor
            .project
            .as_ref()
            .map(|project| project.root().display().to_string());
        handle.form_rx = Some(forms.subscribe());
        handle.trust = worker.context().trust_config();
        let (commands, command_rx) = mpsc::unbounded_channel();
        handle.cmd_rx = Some(command_rx);
        let mut panels = crate::tui_panels::PanelServices::for_scope(
            worker.context(),
            self.executor.clone(),
            commands.clone(),
            vec![],
            self.config.clone(),
            self.descriptor.project.clone(),
        )?;
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
        let ui = atman_tui::run_tui(handle);
        tokio::pin!(ui);
        loop {
            tokio::select! {
                result = &mut ui => return result,
                result = panels.next_completion(), if panels.has_pending_work() => { result?; }
                Some(control) = input.recv() => {
                    let BoundControl { ticket, command, replies } = control;
                    if ticket.binding != binding {
                        let _ = replies.send(TuiCommand::SubmissionResult { result: Err("view is no longer attached to this flow".into()) });
                        continue;
                    }
                    let Some(command) = panels.dispatch(command).await else { continue; };
                    if let Err(error) = self.tui_control(&worker, ticket.request_id, command, &replies, &forms).await {
                        let _ = replies.send(TuiCommand::Toast { message: format!("{error:#}"), level: atman_tui::app::NoteLevel::Error });
                    }
                }
            }
        }
    }

    async fn submit_tui(
        &self,
        worker: &FlowWorker,
        request: Uuid,
        submission: TuiSubmission,
    ) -> Result<()> {
        self.validate_text_input(worker).await?;
        let mut invocation = BTreeMap::new();
        if let Some(reasoning) = submission.reasoning {
            invocation.insert("effort".into(), reasoning.to_string().into());
        }
        self.accept_payload(
            worker,
            request,
            InboxPayload::UserSubmission {
                content: UserContent {
                    text: submission.text,
                    images: submission.images,
                    presentation: submission.presentation,
                },
                invocation,
            },
            DeliveryMode::NextTurn,
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
                let result = if submission.text.trim_start().starts_with(':') {
                    self.command_with_request(worker, &submission.text, request)
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
                let result = forms
                    .submit(&form_id, submission)
                    .await
                    .map_err(|error| error.to_string());
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

#[cfg(test)]
mod tests;
