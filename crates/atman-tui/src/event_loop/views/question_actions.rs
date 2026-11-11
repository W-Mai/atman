//! Question entry and actions use retained native views and their original control mailboxes.

use super::*;
use crate::quick_ask::{
    Command, Intent, Operation, Phase, QuickAsk, presentation::Action as QuestionAction,
};
use anyhow::Context;
use atman_runtime::flow_store::source_capture::SourceSelection;

fn source(ui: &UiState) -> Result<SourceSelection> {
    if let Some(question) = &ui.app.question {
        return Ok(question.captured_source());
    }
    ui.app
        .items
        .source_selection()
        .context("Open a loaded executable conversation before asking")
}

pub(super) fn running(ui: &UiState) -> bool {
    ui.app.items.snapshot().is_some_and(|snapshot| {
        matches!(
            snapshot.status.execution.state(),
            atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(_))
                | atman_rt::InstanceState::Closing { active: Some(_) }
        )
    })
}

impl Workspace {
    pub fn question_intent(
        &mut self,
        ui: &mut UiState,
        editor: &mut InputEditor,
        handle: &mut TuiHandle,
    ) {
        self.update_question_sources(ui);
        if let Some(action) = ui.wm.modals.form_modal.take_question_action() {
            ui.app.question_intent = Some(Intent::Action(action));
        }
        if let Some((id, accepted)) = ui.wm.modals.form_modal.take_question_bring()
            && let Some(bring) = ui.app.question_bring.take().filter(|bring| bring.id == id)
            && accepted
        {
            let target = self
                .views
                .iter()
                .find(|(_, view)| {
                    view.handle
                        .control_tx
                        .as_ref()
                        .and_then(|sender| sender.binding())
                        == Some(&bring.target)
                        && source(&view.ui).is_ok_and(|source| source == bring.source)
                })
                .map(|(key, _)| key.clone());
            if let Some(target) = target {
                if self.select_cached(ui, editor, handle, &target).is_ok() {
                    if target.placement == FlowPlacement::Floating {
                        self.open_window(target);
                    }
                    append_quote(editor, &bring.text);
                }
            } else {
                ui.app.push_note(
                    "Source changed during review; answer retained in Quick Ask",
                    crate::app::NoteLevel::Warn,
                );
            }
        }
        let result = match ui.app.question_intent.take() {
            Some(Intent::Actions) => (|| {
                let question = ui
                    .app
                    .question
                    .as_ref()
                    .context("Open Quick Ask to use question actions")?;
                let phase = question.phase();
                let binding = handle
                    .control_tx
                    .as_ref()
                    .and_then(|sender| sender.binding())
                    .context("Question is unbound")?;
                let snapshot = action_snapshot(ui, question, binding, &phase);
                let options = QuestionAction::ALL
                    .into_iter()
                    .filter(|action| snapshot.enabled(*action))
                    .map(|action| (action, snapshot.hint(action).to_owned()))
                    .collect();
                ui.wm
                    .modals
                    .form_modal
                    .open_question_actions(options)
                    .map_err(anyhow::Error::msg)
            })(),
            Some(Intent::Open {
                selection,
                fresh,
                source,
            }) => self.open_question(ui, editor, handle, selection, fresh, source),
            Some(Intent::Action(action)) => self.question_action(ui, editor, handle, action),
            None => Ok(()),
        };
        if let Err(error) = result {
            ui.app
                .push_note(error.to_string(), crate::app::NoteLevel::Error);
        }
        let discarded: Vec<_> = self
            .views
            .iter()
            .filter(|(_, view)| view.ui.app.question_discarded)
            .map(|(key, _)| key.clone())
            .collect();
        for key in discarded {
            self.forget_question(&key);
        }
        if ui.app.question_discarded
            && let Some(key) = self.active.clone()
        {
            if let Some(fallback) = self
                .main
                .clone()
                .filter(|fallback| fallback != &key)
                .or_else(|| self.views.keys().find(|other| *other != &key).cloned())
            {
                if self.select_cached(ui, editor, handle, &fallback).is_ok() {
                    self.forget_question(&key);
                }
            } else {
                self.main = None;
                self.remember_window(&key);
                self.show_navigation(ui, handle);
            }
        }
    }

    fn update_question_sources(&mut self, ui: &mut UiState) {
        use crate::quick_ask::presentation::SourceState;
        let observations: Vec<_> = std::iter::once(&*ui)
            .chain(self.views.values().map(|view| &view.ui))
            .filter(|view| view.app.question.is_none())
            .filter_map(|view| source(view).ok())
            .collect();
        for view in std::iter::once(ui).chain(self.views.values_mut().map(|view| &mut view.ui)) {
            let Some(question) = &view.app.question else {
                continue;
            };
            let captured = question.captured_source();
            let next = match observations
                .iter()
                .find(|source| source.flow == captured.flow)
            {
                Some(source) if source.branch != captured.branch => SourceState::BranchChanged,
                Some(source)
                    if source.cursor != captured.cursor
                        && !matches!(question.phase(), Phase::Draft) =>
                {
                    SourceState::Updated
                }
                Some(_) => SourceState::Current,
                None => SourceState::Unavailable,
            };
            if view.app.question_source_state != next {
                view.app.question_source_state = next;
                view.app.mark_visual_dirty();
            }
        }
    }

    fn forget_question(&mut self, key: &ViewKey) {
        self.views.remove(key);
        self.windows.retain(|window| window.key != *key);
        self.closed_windows.remove(key);
        self.surfaces.remove(key);
        self.focus_history.retain(|previous| previous != key);
        if self.main.as_ref() == Some(key) {
            self.main = None;
        }
    }

    fn open_question(
        &mut self,
        ui: &mut UiState,
        editor: &mut InputEditor,
        handle: &mut TuiHandle,
        selection: Option<String>,
        fresh: bool,
        selected_source: Option<SourceSelection>,
    ) -> Result<()> {
        anyhow::ensure!(ui.app.question_open.is_none(), "Question window is opening");
        let source = selected_source.map_or_else(|| source(ui), Ok)?;
        if !fresh {
            if handle
                .quick_ask
                .as_ref()
                .is_some_and(|question| question.has_source(&source))
            {
                if let Some(selection) = selection {
                    append_quote(editor, &selection);
                }
                return Ok(());
            }
            let key = self
                .focus_history
                .iter()
                .rev()
                .chain(self.views.keys())
                .find(|key| {
                    self.views.get(*key).is_some_and(|view| {
                        view.handle
                            .quick_ask
                            .as_ref()
                            .is_some_and(|question| question.has_source(&source))
                            && !view.ui.app.question_discarded
                    })
                })
                .cloned();
            if let Some(key) = key {
                self.select_cached(ui, editor, handle, &key)?;
                if key.placement == FlowPlacement::Floating {
                    self.open_window(key);
                }
                if let Some(selection) = selection {
                    append_quote(editor, &selection);
                }
                return Ok(());
            }
        }
        let question = match ui
            .app
            .question_requested
            .as_ref()
            .filter(|question| !fresh && selection.is_none() && question.has_source(&source))
        {
            Some(question) => question.clone(),
            None => {
                let (_, model) = ui
                    .app
                    .input_model()
                    .context("Select a model before asking")?;
                let question = QuickAsk::new(source, model);
                question.initialize(
                    ui.app.input_reasoning.clone(),
                    selection,
                    ui.app
                        .question
                        .as_ref()
                        .map(QuickAsk::label)
                        .or_else(|| {
                            ui.app
                                .items
                                .snapshot()
                                .map(|snapshot| snapshot.display_title.clone())
                        })
                        .unwrap_or_else(|| "Source".into()),
                );
                question
            }
        };
        let ticket = handle
            .control_tx
            .as_ref()
            .context("Question controls are unavailable")?
            .send_tracked(TuiControl::Question(Command::Open(question.clone())))
            .map_err(|_| anyhow::anyhow!("Question control queue is closed"))?
            .context("Question requires a native view")?;
        ui.app.question_open = Some(ticket);
        ui.app.question_requested = Some(question);
        ui.app.mark_visual_dirty();
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn question_opened(
        &mut self,
        ui: &mut UiState,
        editor: &mut InputEditor,
        handle: &mut TuiHandle,
        ticket: Option<&crate::control::ControlTicket>,
        request: uuid::Uuid,
        mut attachment: Box<TuiHandle>,
        focus: bool,
    ) -> Result<()> {
        let Some(ticket) = ticket.filter(|ticket| {
            ticket.request_id == request && ui.app.question_open.as_ref() == Some(*ticket)
        }) else {
            return Ok(());
        };
        let question = ui
            .app
            .question_requested
            .as_ref()
            .context("Question opening ownership is missing")?;
        anyhow::ensure!(
            attachment.quick_ask.as_ref().is_some_and(
                |attached| attached.id == question.id && attached.has_source(&question.source)
            ),
            "Question attachment changed source"
        );
        let binding = attachment
            .control_tx
            .as_ref()
            .and_then(|sender| sender.binding())
            .context("Question attachment is unbound")?
            .clone();
        anyhow::ensure!(
            binding.flow == question.address(),
            "Question attachment address mismatch"
        );
        let _ = ticket;
        if let Some(sender) = &mut attachment.control_tx {
            sender.bind_work(self.control_work.clone());
        }
        let preferences = ui
            .app
            .preferences
            .as_ref()
            .map(|binding| binding.store.clone())
            .unwrap_or_else(crate::states::PreferenceStore::start);
        let (mut next, question_editor) = attach_view(&mut attachment, &preferences)?;
        next.app.image_paste.work = ui.app.image_paste.work.clone();
        next.app.clipboard_copies.service = ui.app.clipboard_copies.service.clone();
        next.app.project_sidebar = ui.app.project_sidebar.clone();
        let key = ViewKey {
            flow: binding.flow,
            placement: FlowPlacement::Floating,
        };
        self.views.insert(
            key.clone(),
            AttachedView {
                ui: next,
                editor: question_editor,
                handle: *attachment,
            },
        );
        ui.app.question_open = None;
        ui.app.question_requested = None;
        if focus && ui.navigation_can_open(None) {
            self.select_cached(ui, editor, handle, &key)?;
            self.open_window(key);
        }
        ui.app.mark_visual_dirty();
        Ok(())
    }

    fn question_action(
        &mut self,
        ui: &mut UiState,
        editor: &mut InputEditor,
        handle: &mut TuiHandle,
        action: QuestionAction,
    ) -> Result<()> {
        let question = ui
            .app
            .question
            .clone()
            .context("Question view is unavailable")?;
        let binding = handle
            .control_tx
            .as_ref()
            .and_then(|sender| sender.binding())
            .context("Question is unbound")?;
        let phase = question.phase();
        let snapshot = action_snapshot(ui, &question, binding, &phase);
        anyhow::ensure!(snapshot.enabled(action), "{}", snapshot.hint(action));
        anyhow::ensure!(
            !question.retention_pending()
                || matches!(
                    action,
                    QuestionAction::Retry
                        | QuestionAction::Keep
                        | QuestionAction::Copy
                        | QuestionAction::Source
                ),
            "Retry pending Keep before changing this question"
        );
        match action {
            QuestionAction::Retry => {
                let retry = question.retry().context("No operation to retry")?;
                let sender = handle
                    .control_tx
                    .as_ref()
                    .context("Question controls are unavailable")?;
                question.start_retry();
                if sender
                    .send(TuiControl::Question(Command::Retry(retry)))
                    .is_err()
                {
                    question.set_retry(Some(retry));
                    anyhow::bail!("Question control queue is closed");
                }
                ui.app.mark_visual_dirty();
                return Ok(());
            }
            QuestionAction::NewAsk => {
                return self.open_question(ui, editor, handle, None, true, None);
            }
            QuestionAction::Copy => {
                let text = answer(ui).context("No answer to copy")?;
                crate::key_handler::copy_text(&mut ui.app, &text, "copied answer".into());
                return Ok(());
            }
            QuestionAction::Source | QuestionAction::Bring => {
                let target = self
                    .focus_history
                    .iter()
                    .rev()
                    .chain(self.views.keys())
                    .find(|key| {
                        key.flow.instance() == Some(&question.source.flow)
                            && self
                                .views
                                .get(*key)
                                .is_some_and(|view| source(&view.ui).is_ok())
                    })
                    .cloned()
                    .context("Source branch is not open; the answer remains available to Copy")?;
                let text = if action == QuestionAction::Bring {
                    Some(answer(ui).context("No answer to bring")?)
                } else {
                    None
                };
                let view = &self.views[&target];
                let target_source = source(&view.ui)?;
                if let Some(text) = &text
                    && !question.has_source(&target_source)
                {
                    let id = uuid::Uuid::new_v4();
                    let target_binding = view
                        .handle
                        .control_tx
                        .as_ref()
                        .and_then(|sender| sender.binding())
                        .context("Source is unbound")?
                        .clone();
                    ui.wm
                        .modals
                        .form_modal
                        .open_question_bring(id, &target_source.flow.instance_id.to_string())
                        .map_err(anyhow::Error::msg)?;
                    ui.app.question_bring = Some(crate::quick_ask::PendingBring {
                        id,
                        target: target_binding,
                        source: target_source,
                        text: text.clone(),
                    });
                    ui.app.mark_visual_dirty();
                    return Ok(());
                }
                self.select_cached(ui, editor, handle, &target)?;
                if target.placement == FlowPlacement::Floating {
                    self.open_window(target);
                }
                if let Some(text) = text {
                    append_quote(editor, &text);
                }
                ui.app.mark_visual_dirty();
                return Ok(());
            }
            QuestionAction::Stop => {
                anyhow::ensure!(
                    !matches!(question.phase(), Phase::Draft),
                    "Question has not started"
                );
                handle
                    .control_tx
                    .as_ref()
                    .context("Question controls are unavailable")?
                    .send(TuiControl::CancelFlow)
                    .map_err(|_| anyhow::anyhow!("Question control queue is closed"))?;
                return Ok(());
            }
            _ => {}
        }
        let operation = match action {
            QuestionAction::Refresh => Operation::Refresh,
            QuestionAction::Keep => Operation::Keep,
            QuestionAction::Discard => Operation::Discard,
            _ => unreachable!(),
        };
        anyhow::ensure!(
            question.pending().is_none(),
            "Question operation is pending"
        );
        if operation != Operation::Discard {
            anyhow::ensure!(
                !running(ui) && ui.app.pending_submission.is_none(),
                "Wait for this response before changing question context or retention"
            );
        }
        let ticket = handle
            .control_tx
            .as_ref()
            .context("Question controls are unavailable")?
            .send_tracked(TuiControl::Question(Command::Operate(
                operation,
                ui.app.input_reasoning.clone(),
            )))
            .map_err(|_| anyhow::anyhow!("Question control queue is closed"))?
            .context("Question requires a native view")?;
        anyhow::ensure!(
            question.begin(operation, ticket),
            "Question operation is unavailable"
        );
        ui.app.mark_visual_dirty();
        Ok(())
    }
}

fn answer(ui: &UiState) -> Option<String> {
    super::super::selection_action_payload(ui)
        .map(|payload| match payload {
            crate::selection::CopyPayload::Markdown(text)
            | crate::selection::CopyPayload::PlainText(text)
            | crate::selection::CopyPayload::Preview(text) => text,
        })
        .or_else(|| ui.app.items.copy_message())
}

fn action_snapshot<'a>(
    ui: &UiState,
    question: &QuickAsk,
    binding: &'a ViewBinding,
    phase: &'a Phase,
) -> crate::quick_ask::presentation::Snapshot<'a> {
    crate::quick_ask::presentation::Snapshot {
        binding,
        document: ui.app.items.identity(),
        phase,
        source: "",
        source_detail: "Return to source",
        captured: "",
        source_state: ui.app.question_source_state,
        running: running(ui),
        has_answer: ui.app.items.has_copy_message(),
        retry: question.retry().is_some(),
        frozen: question.retention_pending(),
        operation: question_operation(ui, question),
        frame: ui.app.animation_frame,
    }
}

fn question_operation(
    ui: &UiState,
    question: &QuickAsk,
) -> Option<crate::quick_ask::presentation::Operation> {
    if question.retry().is_some() {
        return None;
    }
    if question.retrying() {
        return Some(crate::quick_ask::presentation::Operation::Retry);
    }
    question
        .pending()
        .map(|operation| match operation {
            Operation::Refresh => crate::quick_ask::presentation::Operation::Refresh,
            Operation::Keep => crate::quick_ask::presentation::Operation::Keep,
            Operation::Discard => crate::quick_ask::presentation::Operation::Discard,
        })
        .or_else(|| {
            ui.app
                .pending_submission
                .as_ref()
                .map(|_| crate::quick_ask::presentation::Operation::Launch)
        })
}

fn append_quote(editor: &mut InputEditor, text: &str) {
    editor.move_end();
    if !editor.buf().is_empty() {
        editor.insert_str("\n\n");
    }
    editor.insert_str(&crate::selection_menu::quote_text(text));
}

pub(super) fn render_chrome(
    frame: &mut Frame,
    area: Rect,
    ui: &mut UiState,
    binding: &ViewBinding,
) -> Rect {
    let Some(question) = ui.app.question.clone() else {
        ui.app.question_chrome.clear();
        return area;
    };
    let phase = question.phase();
    let label = question.label();
    let captured = question.capture_time();
    let mut detail = question.source_detail();
    if detail.is_empty() {
        detail = format!(
            "{} · {}",
            question.source.flow.instance_id, question.source.flow.session_id
        );
    }
    let operation = question_operation(ui, &question);
    let snapshot = crate::quick_ask::presentation::Snapshot {
        binding,
        document: ui.app.items.identity(),
        phase: &phase,
        source: &label,
        source_detail: &detail,
        captured: if matches!(phase, Phase::Draft) {
            ""
        } else {
            &captured
        },
        source_state: ui.app.question_source_state,
        running: running(ui),
        has_answer: ui.app.items.has_copy_message(),
        retry: question.retry().is_some(),
        frozen: question.retention_pending(),
        operation,
        frame: ui.app.animation_frame,
    };
    ui.app
        .question_chrome
        .render(frame, area, &snapshot, ui.app.question_pointer, None)
        .body
}
