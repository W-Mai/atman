//! Source preparation and atomic destination changes for one retained question window.

use super::*;
use crate::{FlowViewContent, control::ControlTicket, quick_ask::HandoffKind};

type Prepared = (Box<crate::flow_view::FlowViewModel>, Vec<String>);

struct Pending {
    ticket: ControlTicket,
    kind: HandoffKind,
    attachment: Box<TuiHandle>,
    prepared: Option<std::result::Result<Prepared, String>>,
    reported: bool,
}

struct RetiredMailbox {
    binding: ViewBinding,
    commands: Option<mpsc::UnboundedReceiver<TuiCommand>>,
    replies: Option<mpsc::UnboundedReceiver<crate::control::ControlReply>>,
    feedback: Vec<TuiCommand>,
}

#[derive(Default)]
pub(super) struct Handoffs {
    pending: HashMap<uuid::Uuid, Pending>,
    retired: Vec<RetiredMailbox>,
}

impl Handoffs {
    pub(super) fn observe<'a>(&'a mut self, observations: &mut Vec<Observation<'a>>) {
        for pending in self
            .pending
            .values_mut()
            .filter(|pending| pending.prepared.is_none())
        {
            observations.push(Box::pin(async move {
                let source = pending
                    .attachment
                    .flow_view
                    .as_mut()
                    .expect("validated handoff source");
                pending.prepared = Some(match source.next().await {
                    Ok(FlowViewContent::Initial { document, history }) => Ok((document, history)),
                    Ok(_) => Err("Question attachment did not prepare an initial document".into()),
                    Err(error) => Err(error.to_string()),
                });
                None
            }));
        }
        self.retired.retain(|mailbox| {
            mailbox.commands.is_some() || mailbox.replies.is_some() || !mailbox.feedback.is_empty()
        });
        for mailbox in &mut self.retired {
            if mailbox.commands.is_none() && mailbox.replies.is_none() {
                continue;
            }
            observations.push(Box::pin(async move {
                // Old destination replies are consumed to completion; only target-independent
                // feedback survives. A late settings/input/navigation reply cannot mutate the new view.
                let command = tokio::select! {
                    command = recv_optional(mailbox.commands.as_mut()) => {
                        if command.is_none() { mailbox.commands = None; }
                        command
                    },
                    reply = recv_optional(mailbox.replies.as_mut()) => {
                        if reply.is_none() { mailbox.replies = None; }
                        reply.filter(|reply| reply.ticket.binding == mailbox.binding).map(|reply| reply.command)
                    },
                };
                if let Some(command @ TuiCommand::Toast { .. }) = command { mailbox.feedback.push(command); }
                None
            }));
        }
    }
}

impl Workspace {
    pub fn accept_question_handoff(
        &mut self,
        ui: &mut UiState,
        handle: &TuiHandle,
        ticket: Option<&ControlTicket>,
        request_id: uuid::Uuid,
        kind: HandoffKind,
        attachment: Box<TuiHandle>,
    ) {
        let mut attachment = attachment;
        let Some(ticket) = ticket else {
            return;
        };
        let Some(question) = &handle.quick_ask else {
            return;
        };
        let binding = handle
            .control_tx
            .as_ref()
            .and_then(|sender| sender.binding());
        let submission = ui
            .app
            .pending_submission
            .as_ref()
            .map(|pending| &pending.ticket);
        if binding != Some(&ticket.binding)
            || !question.accepts(kind, request_id, ticket, submission)
        {
            return;
        }
        let valid_destination = attachment
            .control_tx
            .as_ref()
            .and_then(|sender| sender.binding())
            .is_some_and(|binding| {
                question.accepts_destination(kind, ticket, binding)
                    && attachment
                        .flow_view
                        .as_ref()
                        .is_some_and(|source| source.address() == binding.flow)
            });
        if !valid_destination {
            ui.app.push_note(
                "Question attachment does not match the requested destination",
                crate::app::NoteLevel::Error,
            );
            return;
        }
        if self
            .questions
            .pending
            .get(&ticket.binding.view_id)
            .is_some_and(|pending| {
                pending.ticket == *ticket && !matches!(pending.prepared, Some(Err(_)))
            })
        {
            return;
        }
        if let Some(sender) = &mut attachment.control_tx {
            sender.bind_work(self.control_work.clone());
        }
        prepare_attachment_source(&mut attachment);
        self.questions.pending.insert(
            ticket.binding.view_id,
            Pending {
                ticket: ticket.clone(),
                kind,
                attachment,
                prepared: None,
                reported: false,
            },
        );
        ui.app.mark_visual_dirty();
    }

    /// Install only complete prepared documents; focus, draft and native geometry stay with the view.
    pub fn install_question_handoffs(
        &mut self,
        ui: &mut UiState,
        editor: &mut InputEditor,
        handle: &mut TuiHandle,
        navigation: Option<&crate::flow_hub::FlowHub>,
    ) {
        for mailbox in &mut self.questions.retired {
            for command in std::mem::take(&mut mailbox.feedback) {
                if self
                    .active_binding
                    .as_ref()
                    .is_some_and(|binding| binding.view_id == mailbox.binding.view_id)
                {
                    apply_view_command(ui, editor, None, command, handle.control_tx.as_ref());
                } else if let Some(view) = self.views.values_mut().find(|view| {
                    view.handle
                        .control_tx
                        .as_ref()
                        .and_then(|sender| sender.binding())
                        .is_some_and(|binding| binding.view_id == mailbox.binding.view_id)
                }) {
                    apply_view_command(
                        &mut view.ui,
                        &mut view.editor,
                        None,
                        command,
                        view.handle.control_tx.as_ref(),
                    );
                }
            }
        }
        let owners: Vec<_> = self
            .questions
            .pending
            .iter()
            .filter(|(_, pending)| pending.prepared.is_some())
            .map(|(owner, _)| *owner)
            .collect();
        for owner in owners {
            let key = if self
                .active_binding
                .as_ref()
                .is_some_and(|binding| binding.view_id == owner)
            {
                self.active.clone()
            } else {
                self.views
                    .iter()
                    .find(|(_, view)| {
                        view.handle
                            .control_tx
                            .as_ref()
                            .and_then(|sender| sender.binding())
                            .is_some_and(|binding| binding.view_id == owner)
                    })
                    .map(|(key, _)| key.clone())
            };
            let Some(key) = key else {
                self.questions.pending.remove(&owner);
                continue;
            };
            if self
                .geometry
                .as_ref()
                .is_some_and(|geometry| geometry.key == key)
            {
                continue;
            }
            let active = self.active.as_ref() == Some(&key);
            if active {
                self.install_question(&key, ui, editor, handle, navigation);
            } else if let Some(mut view) = self.views.remove(&key) {
                let navigation = view.ui.suspended_navigation.take();
                let target = self.install_question(
                    &key,
                    &mut view.ui,
                    &mut view.editor,
                    &mut view.handle,
                    navigation.as_ref(),
                );
                view.ui.suspended_navigation = navigation;
                self.views.insert(target.unwrap_or(key), view);
            } else {
                self.questions.pending.remove(&owner);
            }
        }
    }

    fn install_question(
        &mut self,
        key: &ViewKey,
        ui: &mut UiState,
        editor: &mut InputEditor,
        handle: &mut TuiHandle,
        navigation: Option<&crate::flow_hub::FlowHub>,
    ) -> Option<ViewKey> {
        let owner = handle.control_tx.as_ref()?.binding()?.view_id;
        let pending = self.questions.pending.get_mut(&owner)?;
        let question = handle.quick_ask.as_ref()?;
        let submission = ui
            .app
            .pending_submission
            .as_ref()
            .map(|pending| &pending.ticket);
        if handle
            .control_tx
            .as_ref()
            .and_then(|sender| sender.binding())
            != Some(&pending.ticket.binding)
            || !question.accepts(
                pending.kind,
                pending.ticket.request_id,
                &pending.ticket,
                submission,
            )
        {
            self.questions.pending.remove(&owner);
            return None;
        }
        if let Some(Err(error)) = &pending.prepared {
            if !pending.reported {
                question.set_retry(Some(crate::quick_ask::Retry::Handoff {
                    kind: pending.kind,
                    request_id: pending.ticket.request_id,
                }));
                ui.app.push_note(
                    format!("Question accepted; opening failed: {error}"),
                    crate::app::NoteLevel::Error,
                );
                pending.reported = true;
            }
            return None;
        }
        let accepted = (pending.kind == HandoffKind::Launch).then_some(&pending.ticket);
        if !attachment_can_replace(ui, accepted)
            || !ui.navigation_can_open(navigation)
            || ui.app.transcript_selection_pointer.is_some()
        {
            return None;
        }
        let binding = pending.attachment.control_tx.as_ref()?.binding()?.clone();
        let target = ViewKey {
            flow: binding.flow.clone(),
            placement: key.placement,
        };
        if self.views.contains_key(&target)
            || self
                .active
                .as_ref()
                .is_some_and(|active| active == &target && active != key)
        {
            return None;
        }
        let mut pending = self.questions.pending.remove(&owner)?;
        let (document, history) = pending.prepared.take()?.ok()?;
        if document.flow() != binding.flow.instance() {
            pending.prepared = Some(Err("Question document does not match destination".into()));
            self.questions.pending.insert(owner, pending);
            return None;
        }
        let retained = match (pending.kind, document.cursor()) {
            (
                HandoffKind::Launch,
                Some(atman_runtime::flow_store::position::FlowPosition::Transient { transient }),
            ) => binding
                .flow
                .instance()
                .is_some_and(|flow| transient.flow_id == flow.instance_id.0.0),
            (
                HandoffKind::Keep,
                Some(atman_runtime::flow_store::position::FlowPosition::Durable(cursor)),
            ) => question.retention().is_some_and(|receipt| {
                binding.flow.instance() == Some(&receipt.flow)
                    && &receipt.destination == cursor
                    && ui.app.items.cursor().is_some_and(|position| {
                        matches!(position,
                        atman_runtime::flow_store::position::FlowPosition::Transient { transient }
                            if transient == &receipt.source)
                    })
            }),
            _ => false,
        };
        if !retained {
            pending.prepared = Some(Err(
                "Question publication does not match the requested retention".into(),
            ));
            self.questions.pending.insert(owner, pending);
            return None;
        }
        let mut question = handle.quick_ask.clone()?;
        question.installed(pending.kind, binding.flow.instance()?.clone());
        if pending.kind == HandoffKind::Launch {
            ui.app
                .submission_result(editor, Some(&pending.ticket), Ok(()));
        }
        pending.attachment.quick_ask = Some(question);
        ui.app.question = pending.attachment.quick_ask.clone();
        ui.app.session = pending.attachment.session.clone();
        ui.app.session_id.clone_from(&pending.attachment.session_id);
        ui.app
            .session_dir
            .clone_from(&pending.attachment.session_dir);
        ui.app
            .session_name
            .clone_from(&pending.attachment.session_name);
        ui.app
            .project_root
            .clone_from(&pending.attachment.project_root);
        ui.app.settings = pending.attachment.settings.clone();
        ui.wm
            .modals
            .bind_settings(pending.attachment.settings.clone());
        ui.wm.bind_controls(pending.attachment.control_tx.as_ref());
        ui.app.task_registry = pending.attachment.task_registry.take();
        ui.app.continued_session = pending.attachment.continued_session.clone();
        // Initial content is already prepared and performs no reader or filesystem work here.
        let result = ui
            .app
            .sync_flow_view(FlowViewContent::Initial { document, history });
        let mut old = std::mem::replace(handle, *pending.attachment);
        if let Some(sender) = &handle.control_tx {
            let _ = sender.send(TuiControl::Question(crate::quick_ask::Command::Installed {
                kind: pending.kind,
                request_id: pending.ticket.request_id,
            }));
        }
        self.questions.retired.push(RetiredMailbox {
            binding: pending.ticket.binding,
            commands: old.cmd_rx.take(),
            replies: old.control_reply_rx.take(),
            feedback: Vec::new(),
        });
        refresh_instance_live(ui, editor, handle);
        update_flow_view(ui, handle, result);
        self.rekey_question(key, &target, binding);
        ui.app.mark_visual_dirty();
        Some(target)
    }

    fn rekey_question(&mut self, old: &ViewKey, new: &ViewKey, binding: ViewBinding) {
        if self.active.as_ref() == Some(old) {
            self.active = Some(new.clone());
            self.active_binding = Some(binding);
        }
        if self.main.as_ref() == Some(old) {
            self.main = Some(new.clone());
        }
        for window in &mut self.windows {
            if window.key == *old {
                window.key = new.clone();
            }
        }
        if let Some(mut window) = self.closed_windows.remove(old) {
            window.key = new.clone();
            self.closed_windows.insert(new.clone(), window);
        }
        if let Some(surface) = self.surfaces.remove(old) {
            self.surfaces.insert(new.clone(), surface);
        }
        for key in &mut self.focus_history {
            if key == old {
                *key = new.clone();
            }
        }
        if let Some(geometry) = &mut self.geometry
            && geometry.key == *old
        {
            geometry.key = new.clone();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        composer::PendingSubmission,
        quick_ask::{Phase, QuickAsk},
    };
    use atman_runtime::{
        config_hub::ConfigHub,
        flow_runtime::FlowWorker,
        flow_store::{
            EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, RuntimeStore,
            SessionDescriptor, SessionStore,
            position::FlowPosition,
            transient::{TransientBudget, TransientLimits, TransientRetention},
        },
        source_program::RevisionBundle,
    };
    use uuid::Uuid;

    async fn native_attachment(
        session: &mut SessionStore,
        config: &ConfigHub,
        binding: ViewBinding,
        commands: mpsc::UnboundedSender<crate::control::BoundControl>,
        retention: Option<TransientRetention>,
    ) -> (Box<TuiHandle>, FlowWorker) {
        let descriptor = FlowDescriptor {
            address: binding.flow.instance().unwrap().clone(),
            creation_request: Uuid::new_v4(),
            created_by: None,
            content: EntrySnapshot {
                name: "question".into(),
                path: "question.at".into(),
                bundle: RevisionBundle {
                    entry: "question.at".into(),
                    edges: Default::default(),
                    sources: [("question.at".into(), "flow question() {}".into())].into(),
                },
            }
            .into(),
        };
        let store = match retention {
            Some(retention) => session.create_transient_flow(descriptor, None, retention),
            None => session.create_flow(descriptor),
        }
        .unwrap();
        let worker = FlowWorker::start(store).await.unwrap();
        worker
            .context()
            .bind_model_registry(config.models().clone())
            .unwrap();
        let handle = TuiHandle::from_flow(&worker, binding, commands, config.clone())
            .await
            .unwrap();
        (Box::new(handle), worker)
    }

    fn draft(
        question: QuickAsk,
        commands: mpsc::UnboundedSender<crate::control::BoundControl>,
        config: &ConfigHub,
    ) -> TuiHandle {
        let binding = ViewBinding {
            view_id: Uuid::new_v4(),
            revision: 1,
            flow: question.address(),
        };
        TuiHandle::from_question_draft(question, binding, commands, config.clone().into(), None)
            .unwrap()
    }

    fn ui(handle: &TuiHandle) -> UiState {
        let mut app = AppState::new(handle.session_id.clone(), None);
        app.question = handle.quick_ask.clone();
        UiState::new(app)
    }

    #[tokio::test]
    async fn handoffs_preserve_window_drafts_and_hidden_reply_ownership() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigHub::from_config_dir(directory.path().join("config"));
        let (commands, mut incoming) = mpsc::unbounded_channel();
        let question = crate::quick_ask::tests::question();
        let source = question.source.clone();
        let root = RuntimeStore::open(directory.path()).unwrap();
        let mut session = root
            .create_session(SessionDescriptor {
                id: source.flow.session_id.0,
                project: None,
                configuration: Default::default(),
                title: "Question".into(),
            })
            .unwrap();
        let mut handle = draft(question, commands.clone(), &config);
        assert!(
            handle.session.is_none()
                && handle.flow_view.is_none()
                && handle.initial_items.is_empty()
        );
        let original = handle
            .control_tx
            .as_ref()
            .unwrap()
            .binding()
            .unwrap()
            .clone();
        let ticket = ControlTicket {
            history_head: None,
            binding: original.clone(),
            request_id: Uuid::new_v4(),
        };
        let mut ui = ui(&handle);
        let mut editor = InputEditor::default();
        editor.insert_str("question");
        ui.app.pending_submission = Some(PendingSubmission {
            ticket: ticket.clone(),
            draft: editor.prepare_submission(),
            quote: None,
            consumes_quote: false,
            images: vec![],
        });
        let mut workspace = Workspace::new(&handle);
        let key = ViewKey {
            flow: original.flow.clone(),
            placement: FlowPlacement::Floating,
        };
        let rect = Rect::new(7, 3, 90, 24);
        workspace.active = Some(key.clone());
        workspace.main = None;
        workspace.windows.push(Window {
            key: key.clone(),
            history: Default::default(),
            rect,
            minimum_size: (72, 18),
            preferred_size: Some((72, 18)),
            restore: Some(Rect::new(5, 2, 80, 20)),
            hover: None,
            last_titlebar_click: None,
        });
        workspace.focus_history.push(key.clone());
        let native = ViewBinding {
            view_id: original.view_id,
            revision: 2,
            flow: FlowRef {
                session_id: source.flow.session_id,
                instance_id: FlowInstanceId::now(),
            }
            .into(),
        };
        let retention = TransientRetention::new(
            TransientBudget::new(64 * 1024 * 1024).unwrap(),
            Uuid::new_v4(),
            native.flow.instance().unwrap().instance_id.0.0,
            TransientLimits::default(),
        )
        .unwrap();
        let (mut attachment, worker) = native_attachment(
            &mut session,
            &config,
            native.clone(),
            commands.clone(),
            Some(retention),
        )
        .await;
        assert!(worker.is_transient());
        assert!(worker.context().storage_directory().is_none());
        assert!(
            session.projection().flows.is_empty(),
            "draft launch does not register persistent history"
        );
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        attachment.flow_view = Some(
            attachment.flow_view.take().unwrap().prepare_with_limit(
                attachment.session.clone(),
                attachment
                    .session
                    .as_ref()
                    .map(|session| session.output_store()),
                true,
                gate.clone(),
            ),
        );
        let document = ui.app.items.identity();
        workspace.accept_question_handoff(
            &mut ui,
            &handle,
            Some(&ticket),
            ticket.request_id,
            HandoffKind::Launch,
            attachment,
        );
        workspace
            .questions
            .pending
            .get_mut(&original.view_id)
            .unwrap()
            .prepared = Some(Err("injected reader preparation failure".into()));
        workspace.install_question_handoffs(&mut ui, &mut editor, &mut handle, None);
        assert!(
            ui.app.pending_submission.is_some(),
            "accepted input stays owned until installation"
        );
        assert_eq!(
            handle.quick_ask.as_ref().unwrap().retry(),
            Some(crate::quick_ask::Retry::Handoff {
                kind: HandoffKind::Launch,
                request_id: ticket.request_id
            })
        );
        ui.app.question_intent = Some(crate::quick_ask::Intent::Action(
            crate::quick_ask::presentation::Action::Retry,
        ));
        workspace.question_intent(&mut ui, &mut editor, &mut handle);
        let retry = incoming.recv().await.unwrap();
        assert_eq!(retry.ticket.binding, original);
        assert!(
            matches!(retry.command, TuiControl::Question(crate::quick_ask::Command::Retry(crate::quick_ask::Retry::Handoff { kind: HandoffKind::Launch, request_id })) if request_id == ticket.request_id)
        );
        let pending = workspace
            .questions
            .pending
            .get_mut(&original.view_id)
            .unwrap();
        pending.prepared = None;
        pending.reported = false;
        workspace.install_question_handoffs(&mut ui, &mut editor, &mut handle, None);
        assert_eq!(
            ui.app.items.identity(),
            document,
            "preparation keeps the complete displayed document"
        );
        assert_eq!(
            handle.control_tx.as_ref().unwrap().binding(),
            Some(&original)
        );
        gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(3), async {
            while workspace.questions.pending[&original.view_id]
                .prepared
                .is_none()
            {
                workspace.next_background().await;
            }
        })
        .await
        .unwrap();
        ui.app.queue_recovery_ticket = Some(ticket.clone());
        workspace.install_question_handoffs(&mut ui, &mut editor, &mut handle, None);
        assert_eq!(
            handle.control_tx.as_ref().unwrap().binding(),
            Some(&original),
            "retained controls defer installation"
        );
        ui.app.queue_recovery_ticket = None;
        editor.insert_str("\nnewer draft");
        ui.app.pending_quote = Some("newer quote".into());
        workspace.install_question_handoffs(&mut ui, &mut editor, &mut handle, None);
        assert_eq!(handle.control_tx.as_ref().unwrap().binding(), Some(&native));
        assert_eq!(ui.app.items.flow(), native.flow.instance());
        assert!(matches!(
            ui.app.items.cursor(),
            Some(FlowPosition::Transient { .. })
        ));
        assert!(ui.app.pending_submission.is_none());
        assert_eq!(editor.buf(), "question\nnewer draft");
        assert_eq!(ui.app.pending_quote.as_deref(), Some("newer quote"));
        assert_eq!(workspace.windows[0].rect, rect);
        assert_eq!(workspace.windows[0].restore, Some(Rect::new(5, 2, 80, 20)));
        assert_eq!(workspace.windows[0].key.flow, native.flow);
        assert!(matches!(
            handle.quick_ask.as_ref().unwrap().phase(),
            Phase::Transient(_)
        ));
        assert_eq!(handle.quick_ask.as_ref().unwrap().source, source);
        assert_eq!(workspace.questions.retired[0].binding, original);
        let installed = incoming.recv().await.unwrap();
        assert_eq!(installed.ticket.binding, native);
        assert!(
            matches!(installed.command, TuiControl::Question(crate::quick_ask::Command::Installed { kind: HandoffKind::Launch, request_id }) if request_id == ticket.request_id)
        );

        let keep = handle
            .control_tx
            .as_ref()
            .unwrap()
            .send_tracked(TuiControl::TermResize {
                handle: "question".into(),
                cols: 100,
                rows: 40,
            })
            .unwrap()
            .unwrap();
        let request = incoming.recv().await.unwrap();
        assert!(handle.quick_ask.as_mut().unwrap().begin_keep(keep.clone()));
        let kept = ViewBinding {
            view_id: native.view_id,
            revision: 3,
            flow: native.flow.clone(),
        };
        let (attachment, durable_worker) =
            native_attachment(&mut session, &config, kept.clone(), commands.clone(), None).await;
        assert!(!durable_worker.is_transient());
        let FlowPosition::Transient { transient } = ui.app.items.cursor().unwrap().clone() else {
            panic!("question must still be transient");
        };
        handle.quick_ask.as_ref().unwrap().set_retention(
            atman_runtime::flow_store::retention::RetentionReceipt {
                request: keep.request_id,
                flow: durable_worker.address().clone(),
                source: transient,
                destination: durable_worker
                    .snapshot()
                    .await
                    .unwrap()
                    .cursor
                    .require_durable()
                    .unwrap()
                    .clone(),
                outputs_digest: format!("blake3:{}", "0".repeat(64)),
            },
        );
        let other = draft(
            QuickAsk::new(source.clone(), "vendor/other".into()),
            commands,
            &config,
        );
        let other_key = ViewKey {
            flow: other
                .control_tx
                .as_ref()
                .unwrap()
                .binding()
                .unwrap()
                .flow
                .clone(),
            placement: FlowPlacement::Main,
        };
        workspace.views.insert(
            other_key.clone(),
            AttachedView {
                ui: self::ui(&other),
                editor: InputEditor::default(),
                handle: other,
            },
        );
        workspace
            .select_cached(&mut ui, &mut editor, &mut handle, &other_key)
            .unwrap();
        workspace.main = Some(other_key.clone());
        request
            .replies
            .send(TuiCommand::QuestionHandoff {
                request_id: keep.request_id,
                kind: HandoffKind::Keep,
                attachment,
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                workspace.next_background().await;
                workspace.install_question_handoffs(&mut ui, &mut editor, &mut handle, None);
                if workspace.views.values().any(|view| {
                    view.handle
                        .quick_ask
                        .as_ref()
                        .is_some_and(|question| matches!(question.phase(), Phase::Kept(_)))
                }) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            workspace.active.as_ref(),
            Some(&other_key),
            "background Keep cannot steal focus"
        );
        let kept_key = ViewKey {
            flow: kept.flow.clone(),
            placement: FlowPlacement::Floating,
        };
        let kept_view = &workspace.views[&kept_key];
        assert!(matches!(
            kept_view.ui.app.items.cursor(),
            Some(FlowPosition::Durable(_))
        ));
        assert_eq!(kept_view.editor.buf(), "question\nnewer draft");
        assert_eq!(
            kept_view.ui.app.pending_quote.as_deref(),
            Some("newer quote")
        );
        assert_eq!(workspace.windows[0].key, kept_key);
        assert_eq!(workspace.windows[0].rect, rect);
        assert_eq!(kept_view.handle.quick_ask.as_ref().unwrap().source, source);
        assert!(
            workspace
                .questions
                .retired
                .iter()
                .any(|mailbox| mailbox.binding == native),
            "old reply channels remain owned until drained"
        );
        worker.shutdown().await.unwrap();
        durable_worker.shutdown().await.unwrap();
    }
}
