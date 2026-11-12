//! Native history menus retain immutable selections and use the shared form and composer.

use super::*;
use crate::composer::branches::RecoveryKey;
use crate::{
    ControlSender, UiState,
    app::{AppState, OutputItem},
    input::InputEditor,
};
use atman_runtime::flow_store::{
    history_operation::HistoryOperationStatus,
    source_capture::{ConversationBranchId, ConversationCutoff, SourceSelection},
};

#[derive(Clone)]
pub(crate) struct CapturedTurn {
    head: HistoryHead,
    source: SourceSelection,
    cutoff: Box<ConversationCutoff>,
    label: String,
}

pub(crate) enum Intent {
    Turn(CapturedTurn),
    Turns,
    Branches,
}

#[derive(Clone)]
enum Action {
    Close,
    Turn(CapturedTurn),
    Turns(usize),
    Review {
        head: HistoryHead,
        target: HistoryOperationTarget,
    },
    Send(Command),
    Draft(RecoveryKey),
    Overview,
}

struct Menu {
    id: Uuid,
    options: Vec<Action>,
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) pending: Pending,
    pub(crate) intent: Option<Intent>,
    menu: Option<Menu>,
    last: Option<Command>,
    operation: Option<Box<HistoryOperationRecord>>,
    recoveries: Vec<RecoveryKey>,
    candidates: Vec<(Uuid, Vec<Uuid>)>,
    recovery_requested: std::collections::HashSet<(Uuid, Uuid)>,
}

pub(crate) fn capture_turn(app: &AppState, index: usize) -> Option<CapturedTurn> {
    if app.question.is_some() || app.conversation_closed {
        return None;
    }
    let cutoff = app.items.user_turn_boundary(index)?.clone();
    let head = app.items.snapshot()?.status.history_head.clone()?;
    let source = app.items.source_selection()?;
    let text = match app.items.get(index)? {
        OutputItem::UserTurn { text, presentation }
        | OutputItem::Interjection { text, presentation } => presentation
            .as_ref()
            .map_or(text.as_str(), |value| value.prompt.as_str()),
        _ => return None,
    };
    let label = crate::width::truncate(text, 64).replace(['\n', '\r'], " ");
    Some(CapturedTurn {
        head,
        source,
        cutoff: Box::new(cutoff),
        label,
    })
}

pub(crate) fn update(ui: &mut UiState, editor: &mut InputEditor, controls: Option<&ControlSender>) {
    let mut state = std::mem::take(&mut ui.app.conversation_history);
    let result = (|| {
        if let Some(result) = state.pending.response.take() {
            state.response(ui, editor, controls, result)?;
        }
        if let Some((id, selected)) = ui.wm.modals.form_modal.take_history_action()
            && state.menu.as_ref().is_some_and(|menu| menu.id == id)
            && let Some(menu) = state.menu.take()
            && let Some(action) = selected.and_then(|index| menu.options.get(index)).cloned()
        {
            state.action(ui, editor, controls, action)?;
        }
        if let Some(intent) = state.intent.take() {
            if ui.app.question.is_some() || ui.app.items.is_archive() || ui.app.conversation_closed
            {
                return Err("Open an executable conversation for history actions".into());
            }
            if state.pending.running {
                state.progress(ui)?;
            } else {
                match intent {
                    Intent::Turn(turn) => state.turn(ui, turn)?,
                    Intent::Turns => state.turns(ui, ui.app.items.len())?,
                    Intent::Branches => state.send(ui, controls, Command::Pending, true)?,
                }
            }
        }
        Ok::<(), String>(())
    })();
    if let Err(error) = result {
        ui.app.push_note(error, crate::app::NoteLevel::Error);
    }
    ui.app.conversation_history = state;
}

impl State {
    fn visible(&self, ui: &UiState) -> bool {
        self.menu
            .as_ref()
            .is_some_and(|menu| ui.wm.modals.form_modal.owns_history(menu.id))
    }

    fn show(
        &mut self,
        ui: &mut UiState,
        prompt: String,
        choices: Vec<(String, Action)>,
        loading: bool,
    ) -> Result<(), String> {
        if self.visible(ui) {
            ui.wm.modals.form_modal.close();
        }
        let id = Uuid::now_v7();
        ui.wm.modals.form_modal.open_history(
            id,
            prompt,
            choices.iter().map(|(label, _)| label.clone()).collect(),
            loading,
        )?;
        self.menu = Some(Menu {
            id,
            options: choices.into_iter().map(|(_, action)| action).collect(),
        });
        Ok(())
    }

    fn send(
        &mut self,
        ui: &mut UiState,
        controls: Option<&ControlSender>,
        command: Command,
        visible: bool,
    ) -> Result<(), String> {
        if self.pending.running && !matches!(command, Command::Abort(_)) {
            return Err("A history request is already pending".into());
        }
        let controls = controls.ok_or("History controls are unavailable")?;
        if ui.app.question.is_some() || ui.app.items.is_archive() {
            return Err("Open an executable conversation for history actions".into());
        }
        let ticket = controls
            .send_tracked(crate::TuiControl::ConversationHistory(command.clone()))
            .map_err(|error| error.to_string())?
            .ok_or("History requires a native view")?;
        self.pending.ticket = Some(ticket);
        self.pending.running = true;
        self.last = Some(command);
        if visible {
            self.show(
                ui,
                "Preparing".into(),
                vec![("Hide".into(), Action::Close)],
                true,
            )?;
        }
        Ok(())
    }

    fn turn(&mut self, ui: &mut UiState, turn: CapturedTurn) -> Result<(), String> {
        self.show(
            ui,
            turn.label.clone(),
            vec![
                (
                    "↶ Revert to here".into(),
                    Action::Review {
                        head: turn.head,
                        target: HistoryOperationTarget::Revert {
                            observed: turn.source,
                            cutoff: turn.cutoff,
                            branch: ConversationBranchId::random(),
                        },
                    },
                ),
                ("⑂ Branches".into(), Action::Overview),
                ("Close".into(), Action::Close),
            ],
            false,
        )
    }

    fn turns(&mut self, ui: &mut UiState, before: usize) -> Result<(), String> {
        let mut choices = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut next = 0;
        for index in (0..before.min(ui.app.items.len())).rev().take(512) {
            next = index;
            if let Some(turn) = capture_turn(&ui.app, index)
                && let ConversationCutoff::BeforeUserTurn { input, .. } = &*turn.cutoff
                && seen.insert(*input)
            {
                choices.push((turn.label.clone(), Action::Turn(turn)));
                if choices.len() == 32 {
                    break;
                }
            }
        }
        if next > 0 {
            choices.push(("Earlier".into(), Action::Turns(next)));
        }
        choices.push(("Close".into(), Action::Close));
        self.show(
            ui,
            "Revert to here · files unchanged".into(),
            choices,
            false,
        )
    }

    fn action(
        &mut self,
        ui: &mut UiState,
        editor: &mut InputEditor,
        controls: Option<&ControlSender>,
        action: Action,
    ) -> Result<(), String> {
        match action {
            Action::Close => Ok(()),
            Action::Turn(turn) => self.turn(ui, turn),
            Action::Turns(before) => self.turns(ui, before),
            Action::Review { head, target } => self.send(
                ui,
                controls,
                Command::Review {
                    id: Uuid::now_v7(),
                    expected: head,
                    target,
                },
                true,
            ),
            Action::Send(command) => self.send(ui, controls, command, true),
            Action::Draft(key) => ui.app.exchange_recovered_draft(editor, &key),
            Action::Overview => self.send(ui, controls, Command::Pending, true),
        }
    }

    fn draft_choices(&self, ui: &UiState) -> Vec<(String, Action)> {
        let mut choices = Vec::new();
        for key in &self.recoveries {
            if let Some(selected) = ui.app.recovered_draft_selected(key) {
                let ordinal = self
                    .candidates
                    .iter()
                    .find(|(id, _)| *id == key.operation)
                    .and_then(|(_, inputs)| inputs.iter().position(|input| *input == key.input))
                    .map(|index| format!(" {}", index + 1))
                    .unwrap_or_default();
                choices.push((
                    format!(
                        "{}{}",
                        if selected {
                            "Return to draft"
                        } else {
                            "Edit recovered input"
                        },
                        ordinal
                    ),
                    Action::Draft(key.clone()),
                ));
            }
        }
        if let Some(record) = &self.operation
            && matches!(record.status, HistoryOperationStatus::Applied { .. })
            && let Some((operation, inputs)) = self
                .candidates
                .iter()
                .find(|(id, _)| *id == record.request.id)
        {
            for (index, input) in inputs.iter().enumerate() {
                if !self
                    .recoveries
                    .iter()
                    .any(|key| key.operation == *operation && key.input == *input)
                {
                    choices.push((
                        format!("Recover input {}", index + 1),
                        Action::Send(Command::Recover {
                            operation: *operation,
                            input: *input,
                        }),
                    ));
                }
            }
        }
        choices
    }

    fn progress(&mut self, ui: &mut UiState) -> Result<(), String> {
        if self.pending.running
            && !matches!(
                self.last,
                Some(Command::Confirm(_) | Command::Status(_) | Command::Abort(_))
            )
        {
            return self.show(
                ui,
                "Preparing".into(),
                vec![("Hide".into(), Action::Close)],
                true,
            );
        }
        let mut choices = vec![("Hide".into(), Action::Close)];
        let label = if let Some(record) = &self.operation {
            match &record.status {
                HistoryOperationStatus::Preparing
                | HistoryOperationStatus::Stopping
                | HistoryOperationStatus::Ready { .. } => {
                    choices.push((
                        "Abort history change".into(),
                        Action::Send(Command::Abort(record.request.id)),
                    ));
                    if !self.pending.running {
                        choices.push((
                            "Resume history change".into(),
                            Action::Send(Command::Confirm(Box::new(record.request.clone()))),
                        ));
                    }
                    if record.status == HistoryOperationStatus::Stopping {
                        "Stopping · waiting for execution cleanup"
                    } else {
                        "Preparing history"
                    }
                }
                HistoryOperationStatus::Applied { .. } => {
                    choices.push(("Branches".into(), Action::Overview));
                    "History changed · files unchanged"
                }
                HistoryOperationStatus::Aborted { .. } => {
                    choices.push(("Branches".into(), Action::Overview));
                    "History change aborted · cancellation remains"
                }
            }
        } else {
            "Preparing"
        };
        choices.extend(self.draft_choices(ui));
        self.show(ui, label.into(), choices, self.pending.running)
    }

    fn response(
        &mut self,
        ui: &mut UiState,
        editor: &mut InputEditor,
        controls: Option<&ControlSender>,
        result: Result<Response, String>,
    ) -> Result<(), String> {
        let visible = self.visible(ui);
        match result {
            Err(error) => {
                if !visible {
                    ui.app.push_note(error, crate::app::NoteLevel::Error);
                    return Ok(());
                }
                let mut choices = vec![("Close".into(), Action::Close)];
                if let Some(Command::Confirm(request)) = &self.last {
                    choices.push((
                        "Check operation".into(),
                        Action::Send(Command::Status(request.id)),
                    ));
                } else if let Some(command) = &self.last {
                    choices.push(("Retry".into(), Action::Send(command.clone())));
                }
                self.show(ui, error, choices, false)
            }
            Ok(Response::Branches(page)) => {
                if !visible {
                    return Ok(());
                }
                let offset = match self.last {
                    Some(Command::Branches { offset, .. }) => offset,
                    _ => return Ok(()),
                };
                let mut choices = Vec::new();
                for (index, branch) in page.branches.iter().enumerate() {
                    let ordinal = offset + index as u64 + 1;
                    if branch.id == page.head.branch {
                        choices.push((format!("● {ordinal} · current"), Action::Close));
                    } else {
                        choices.push((
                            format!("⑂ {ordinal} · restore"),
                            Action::Review {
                                head: page.head.clone(),
                                target: HistoryOperationTarget::Restore { branch: branch.id },
                            },
                        ));
                    }
                }
                choices.extend(self.draft_choices(ui));
                if let Some(offset) = page.next_offset {
                    choices.push((
                        "Next".into(),
                        Action::Send(Command::Branches {
                            expected: page.head.clone(),
                            offset,
                        }),
                    ));
                }
                if offset > 0 {
                    choices.push((
                        "Previous".into(),
                        Action::Send(Command::Branches {
                            expected: page.head.clone(),
                            offset: offset.saturating_sub(32),
                        }),
                    ));
                }
                choices.push(("Revert to here".into(), Action::Turns(ui.app.items.len())));
                choices.push(("Close".into(), Action::Close));
                self.show(ui, format!("Branches · {}", page.total), choices, false)
            }
            Ok(Response::Review(review)) => {
                if !visible {
                    return Ok(());
                }
                self.candidates
                    .retain(|(id, _)| self.recoveries.iter().any(|key| key.operation == *id));
                self.candidates
                    .push((review.request.id, review.recoverable_inputs));
                let name = if matches!(review.request.target, HistoryOperationTarget::Revert { .. })
                {
                    "Revert"
                } else {
                    "Restore"
                };
                let mut prompt = format!("{name} · files unchanged");
                if let Some(turns) = review.affected_turns {
                    prompt.push_str(&format!(
                        "\n{turns} {} leave the active branch",
                        if turns == 1 {
                            "turn will"
                        } else {
                            "turns will"
                        }
                    ));
                }
                let confirmation = if review.request.active.is_some() {
                    format!("Stop and {}", name.to_lowercase())
                } else {
                    name.into()
                };
                if review.request.active.is_some() {
                    prompt.push_str("\nCurrent work will stop");
                }
                if review.request.goal.is_some() {
                    prompt.push_str("\nGoal will pause");
                }
                if !review.request.queued.is_empty() {
                    prompt.push_str(&format!(
                        "\n{} queued inputs stay held",
                        review.request.queued.len()
                    ));
                }
                self.show(
                    ui,
                    prompt,
                    vec![
                        ("Cancel".into(), Action::Close),
                        (
                            confirmation,
                            Action::Send(Command::Confirm(Box::new(review.request))),
                        ),
                    ],
                    false,
                )
            }
            Ok(Response::Operation(record)) => {
                if let Some(record) = record {
                    let recover = if matches!(record.status, HistoryOperationStatus::Applied { .. })
                    {
                        match &record.request.target {
                            HistoryOperationTarget::Revert { cutoff, .. } => match &**cutoff {
                                ConversationCutoff::BeforeUserTurn { input, .. } => {
                                    Some((record.request.id, *input))
                                }
                                _ => None,
                            },
                            _ => None,
                        }
                    } else {
                        None
                    };
                    self.operation = Some(record);
                    if !self.pending.running
                        && let Some((operation, input)) = recover
                        && self.recovery_requested.insert((operation, input))
                    {
                        return self.send(
                            ui,
                            controls,
                            Command::Recover { operation, input },
                            visible,
                        );
                    }
                    if visible {
                        self.progress(ui)?;
                    }
                } else if visible {
                    if matches!(self.last, Some(Command::Pending)) {
                        let head = ui
                            .app
                            .items
                            .snapshot()
                            .and_then(|snapshot| snapshot.status.history_head.clone())
                            .ok_or("Conversation is still loading")?;
                        self.send(
                            ui,
                            controls,
                            Command::Branches {
                                expected: head,
                                offset: 0,
                            },
                            true,
                        )?;
                    } else {
                        self.show(
                            ui,
                            "Operation unavailable · no new change was started".into(),
                            vec![
                                ("Branches".into(), Action::Overview),
                                ("Close".into(), Action::Close),
                            ],
                            false,
                        )?;
                    }
                }
                Ok(())
            }
            Ok(Response::Recovered(mut recovered)) => {
                let Some(Command::Recover { operation, input }) = self.last else {
                    crate::render_work::discard(recovered);
                    return Ok(());
                };
                if recovered.operation != operation || recovered.input != input {
                    crate::render_work::discard(recovered);
                    return Err("Recovered input identity changed".into());
                }
                self.candidates.retain(|(id, _)| *id != operation);
                self.candidates
                    .push((operation, std::mem::take(&mut recovered.recoverable_inputs)));
                let flow = self
                    .pending
                    .ticket
                    .as_ref()
                    .and_then(|ticket| ticket.binding.flow.instance())
                    .cloned()
                    .ok_or("Original input view is unavailable")?;
                let key = ui.app.retain_recovered_draft(editor, flow, *recovered);
                if !self.recoveries.contains(&key) {
                    self.recoveries.push(key);
                }
                if visible {
                    self.progress(ui)?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{keys::KeyAction, wm::modal::ModalOverlay};

    #[tokio::test]
    async fn history_forms_keep_menu_identity_and_hidden_observation() {
        let mut ui = UiState::new(AppState::new("history menus".into(), None));
        let mut editor = InputEditor::default();
        let mut state = State::default();
        state
            .show(
                &mut ui,
                "old".into(),
                vec![("Close".into(), Action::Close)],
                false,
            )
            .unwrap();
        ui.wm
            .modals
            .form_modal
            .handle_key(&KeyAction::Submit, &mut ui.app, None);
        state
            .show(
                &mut ui,
                "new".into(),
                vec![("Hide".into(), Action::Close)],
                true,
            )
            .unwrap();
        let current = state.menu.as_ref().unwrap().id;
        ui.app.conversation_history = state;
        update(&mut ui, &mut editor, None);
        assert!(ui.wm.modals.form_modal.owns_history(current));
        assert_eq!(
            ui.app.conversation_history.menu.as_ref().unwrap().id,
            current
        );
        assert!(ui.wm.modals.form_modal.history_loading());
        ui.wm
            .modals
            .form_modal
            .handle_key(&KeyAction::Escape, &mut ui.app, None);
        update(&mut ui, &mut editor, None);
        assert!(!ui.wm.modals.form_modal.history_loading());
        ui.app.conversation_history.pending.response = Some(Ok(Response::Operation(None)));
        update(&mut ui, &mut editor, None);
        assert!(
            !ui.wm.modals.form_modal.open,
            "a hidden read cannot reopen the menu"
        );
        assert!(editor.is_untouched());
        let flow = atman_runtime::flow_store::FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
        };
        let operation = Uuid::now_v7();
        let input = Uuid::now_v7();
        let head = HistoryHead::initial(&flow, Uuid::now_v7());
        ui.app.conversation_history.last = Some(Command::Recover { operation, input });
        ui.app.conversation_history.pending.ticket = Some(crate::control::ControlTicket {
            binding: crate::control::ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: flow.into(),
            },
            request_id: Uuid::now_v7(),
            history_head: Some(head.clone()),
        });
        let mut recovered = InputEditor::default();
        recovered.insert_str("hidden recovery");
        ui.app.conversation_history.pending.response =
            Some(Ok(Response::Recovered(Box::new(PreparedRecovery {
                operation,
                input,
                head,
                editor: recovered,
                quote: None,
                recoverable_inputs: vec![input],
            }))));
        update(&mut ui, &mut editor, None);
        assert!(!ui.wm.modals.form_modal.open);
        assert!(
            editor.is_untouched(),
            "an uninstalled recovery stays outside the current draft"
        );
        assert_eq!(ui.app.conversation_history.recoveries.len(), 1);
        assert_eq!(
            ui.app.conversation_history.candidates,
            vec![(operation, vec![input])]
        );
    }
}
