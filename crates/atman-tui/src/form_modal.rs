use crate::input::InputEditor;
use crate::keys::KeyAction;
use crate::wm::modal::ModalAction;
use atman_runtime::form::{FormAnswer, FormKind, FormQuestion, FormSubmission, PendingForm};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FormPhase {
    #[default]
    Editing,
    FinalConfirm,
}

#[derive(Debug, Clone)]
pub enum SubmitOutcome {
    Submit {
        form_id: String,
        submission: FormSubmission,
    },
    None,
}

#[derive(Default)]
pub struct FormModal {
    pub open: bool,
    pub pending: Option<PendingForm>,
    pub current_index: usize,
    pub draft_answers: Vec<Option<FormAnswer>>,
    pub phase: FormPhase,
    pub confirm_focus: usize,
    pub multi_selected: Vec<bool>,
    pub text_editor: InputEditor,
    pub error: Option<String>,
    pub last_input_rect: Option<Rect>,
    pub scroll: u16,
}

impl FormModal {
    pub fn attach(&mut self, form: PendingForm) {
        let questions = questions(&form);
        self.pending = Some(form);
        self.open = true;
        self.current_index = 0;
        self.draft_answers = vec![None; questions.len()];
        self.phase = FormPhase::Editing;
        self.confirm_focus = 0;
        self.error = None;
        self.scroll = 0;
        self.reset_question_state();
    }

    pub fn close(&mut self) {
        self.open = false;
        self.pending = None;
        self.current_index = 0;
        self.draft_answers.clear();
        self.phase = FormPhase::Editing;
        self.confirm_focus = 0;
        self.multi_selected.clear();
        self.text_editor = InputEditor::default();
        self.error = None;
        self.scroll = 0;
    }

    pub fn active_form_id(&self) -> Option<&str> {
        self.pending.as_ref().map(|p| p.form_id.as_str())
    }

    fn questions(&self) -> &[FormQuestion] {
        self.pending.as_ref().map_or(&[], |p| &p.form.questions)
    }

    fn current_kind(&self) -> Option<&FormKind> {
        self.questions()
            .get(self.current_index)
            .map(|q| &q.kind)
            .or_else(|| self.pending.as_ref().map(|p| &p.kind))
    }

    fn reset_question_state(&mut self) {
        self.multi_selected = match self.current_kind() {
            Some(FormKind::MultiSelect { options, .. }) => {
                let mut selected = vec![false; options.len()];
                if let Some(Some(FormAnswer::MultiSelected { indices, .. })) =
                    self.draft_answers.get(self.current_index)
                {
                    for &index in indices {
                        if let Some(value) = selected.get_mut(index) {
                            *value = true;
                        }
                    }
                }
                selected
            }
            _ => Vec::new(),
        };
        self.text_editor = InputEditor::default();
        if let Some(Some(FormAnswer::TextEntered { text })) =
            self.draft_answers.get(self.current_index)
        {
            self.text_editor.insert_str(text);
        }
    }

    fn answer_current(&self) -> Option<FormAnswer> {
        match self.current_kind()? {
            FormKind::Confirm { .. } => Some(FormAnswer::Confirmed {
                value: self.confirm_focus == 0,
            }),
            FormKind::SingleSelect { options, .. } => {
                options
                    .get(self.confirm_focus)
                    .cloned()
                    .map(|label| FormAnswer::Selected {
                        index: self.confirm_focus,
                        label,
                    })
            }
            FormKind::MultiSelect {
                options, min, max, ..
            } => {
                let indices: Vec<_> = self
                    .multi_selected
                    .iter()
                    .enumerate()
                    .filter_map(|(i, b)| b.then_some(i))
                    .collect();
                if min.is_some_and(|m| indices.len() < m) {
                    return None;
                }
                if max.is_some_and(|m| indices.len() > m) {
                    return None;
                }
                let labels = indices
                    .iter()
                    .filter_map(|&i| options.get(i).cloned())
                    .collect();
                Some(FormAnswer::MultiSelected { indices, labels })
            }
            FormKind::Text { .. } => Some(FormAnswer::TextEntered {
                text: self.text_editor.buf().to_string(),
            }),
        }
    }

    fn commit_current(&mut self) -> bool {
        let Some(answer) = self.answer_current() else {
            if let Some(FormKind::MultiSelect { min, max, .. }) = self.current_kind() {
                let count = self.multi_selected.iter().filter(|b| **b).count();
                self.error = if min.is_some_and(|m| count < m) {
                    min.map(|m| format!("Select at least {m}"))
                } else {
                    max.map(|m| format!("Select at most {m}"))
                };
            }
            return false;
        };
        if self.current_index >= self.draft_answers.len() {
            self.draft_answers.resize(self.current_index + 1, None);
        }
        self.draft_answers[self.current_index] = Some(answer);
        self.error = None;
        true
    }

    pub fn move_question(&mut self, delta: isize) {
        if self.phase != FormPhase::Editing || self.questions().is_empty() {
            return;
        }
        if !self.commit_current() {
            return;
        }
        let len = self.questions().len() as isize;
        self.current_index = (self.current_index as isize + delta).rem_euclid(len) as usize;
        self.confirm_focus = self.draft_answers[self.current_index]
            .as_ref()
            .and_then(|a| match a {
                FormAnswer::Selected { index, .. } => Some(*index),
                _ => None,
            })
            .unwrap_or(0);
        self.reset_question_state();
        self.scroll = 0;
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let Some(kind) = self.current_kind() else {
            return;
        };
        let len = match kind {
            FormKind::SingleSelect { options, .. } | FormKind::MultiSelect { options, .. } => {
                options.len()
            }
            FormKind::Confirm { .. } => 2,
            FormKind::Text { .. } => return,
        };
        if len > 0 {
            self.confirm_focus =
                (self.confirm_focus as isize + delta).rem_euclid(len as isize) as usize;
            self.error = None;
        }
    }

    pub fn toggle_current(&mut self) {
        if matches!(self.current_kind(), Some(FormKind::MultiSelect { .. })) {
            if let Some(selected) = self.multi_selected.get_mut(self.confirm_focus) {
                *selected = !*selected;
                self.error = None;
            }
        }
    }

    pub fn submit(&mut self) -> SubmitOutcome {
        if self.phase == FormPhase::Editing {
            if self.current_index + 1 < self.questions().len() {
                self.move_question(1);
                return SubmitOutcome::None;
            }
            if !self.commit_current() {
                return SubmitOutcome::None;
            }
            self.phase = FormPhase::FinalConfirm;
            self.confirm_focus = 0;
            self.last_input_rect = None;
            self.scroll = 0;
            return SubmitOutcome::None;
        }
        if self.confirm_focus == 1 {
            return self.reject();
        }
        let Some(form_id) = self.active_form_id().map(str::to_owned) else {
            return SubmitOutcome::None;
        };
        let answers = self.draft_answers.iter().filter_map(Clone::clone).collect();
        self.close();
        SubmitOutcome::Submit {
            form_id,
            submission: FormSubmission::Submitted { answers },
        }
    }

    pub fn reject(&mut self) -> SubmitOutcome {
        let Some(form_id) = self.active_form_id().map(str::to_owned) else {
            return SubmitOutcome::None;
        };
        self.close();
        SubmitOutcome::Submit {
            form_id,
            submission: FormSubmission::Rejected,
        }
    }

    pub fn cancel(&mut self) -> SubmitOutcome {
        if !self.open {
            SubmitOutcome::None
        } else {
            self.reject()
        }
    }

    #[cfg(test)]
    fn attach_test(&mut self, form: PendingForm) {
        self.attach(form);
    }
}

impl crate::wm::modal::ModalOverlay for FormModal {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        _app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        let Some(kind) = self.current_kind().cloned() else {
            return;
        };
        let inner = Rect {
            x: area.x,
            y: area.y.saturating_add(2),
            width: area.width,
            height: area.height.saturating_sub(3),
        };
        let total = self.questions().len().max(1);
        let filled = (area.width as usize * (self.current_index + 1).min(total)) / total;
        let progress_line = Line::from(
            (0..area.width as usize)
                .map(|i| {
                    Span::styled(
                        if i < filled { "━" } else { "·" },
                        Style::default().fg(if i < filled {
                            t.accent.into()
                        } else {
                            t.panel_bg.into()
                        }),
                    )
                })
                .collect::<Vec<_>>(),
        );
        f.render_widget(
            Paragraph::new(progress_line),
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 1,
            },
        );
        let hint = if self.phase == FormPhase::FinalConfirm {
            " ←→ · choose  enter/y · confirm  n/esc · reject "
        } else {
            hint_for(&kind)
        };
        f.render_widget(
            Paragraph::new(hint).alignment(Alignment::Right),
            Rect {
                x: area.x,
                y: area.y + area.height.saturating_sub(1),
                width: area.width,
                height: 1,
            },
        );
        let mut lines = if self.phase == FormPhase::FinalConfirm {
            vec![Line::from(Span::styled(
                "  Submit all answers?",
                Style::default()
                    .fg(t.tinted_fg.into())
                    .add_modifier(Modifier::BOLD),
            ))]
        } else {
            vec![
                Line::from(Span::styled(
                    kind.prompt().to_owned(),
                    Style::default()
                        .fg(t.tinted_fg.into())
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
            ]
        };
        if self.phase == FormPhase::FinalConfirm {
            let yes = if self.confirm_focus == 0 {
                "[ Yes ]"
            } else {
                "  Yes  "
            };
            let no = if self.confirm_focus == 1 {
                "[ No ]"
            } else {
                "  No  "
            };
            lines.push(Line::from(format!("  {yes}   {no}")));
        } else {
            match kind {
                FormKind::Confirm { .. } => lines.push(Line::from("  [ Yes ]   No")),
                FormKind::SingleSelect { options, .. } | FormKind::MultiSelect { options, .. } => {
                    let is_multi =
                        matches!(self.current_kind(), Some(FormKind::MultiSelect { .. }));
                    for (i, label) in options.iter().enumerate() {
                        lines.push(Line::from(Span::styled(
                            format!(
                                " {}{}{}",
                                if i == self.confirm_focus {
                                    "▶ "
                                } else {
                                    "  "
                                },
                                if is_multi && self.multi_selected.get(i).copied().unwrap_or(false)
                                {
                                    "[✓] "
                                } else if is_multi {
                                    "[ ] "
                                } else {
                                    ""
                                },
                                label
                            ),
                            Style::default().fg(if i == self.confirm_focus {
                                t.accent.into()
                            } else {
                                t.tinted_fg.into()
                            }),
                        )));
                    }
                }
                FormKind::Text { placeholder, .. } => {
                    let text = if self.text_editor.buf().is_empty() {
                        placeholder.unwrap_or_default()
                    } else {
                        self.text_editor.buf().to_owned()
                    };
                    lines.push(Line::from(Span::styled(
                        text,
                        Style::default().fg(t.tinted_fg.into()),
                    )));
                    self.last_input_rect = Some(Rect {
                        x: inner.x,
                        y: inner.y + 2,
                        width: inner.width,
                        height: 1,
                    });
                }
            }
        }
        if let Some(error) = &self.error {
            lines.push(Line::from(Span::styled(
                format!("! {error}"),
                Style::default().fg(t.error.into()),
            )));
        }
        let height = lines.len() as u16;
        self.scroll = self.scroll.min(height.saturating_sub(inner.height));
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            inner,
        );
    }

    fn handle_key(
        &mut self,
        action: &KeyAction,
        _app: &mut crate::app::AppState,
        tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        if !self.open {
            return None;
        }
        let outcome = match action {
            KeyAction::Escape => Some(self.cancel()),
            KeyAction::Submit => Some(self.submit()),
            KeyAction::Char('y') | KeyAction::Char('Y')
                if self.phase == FormPhase::FinalConfirm =>
            {
                Some(self.submit())
            }
            KeyAction::Char('n') | KeyAction::Char('N')
                if self.phase == FormPhase::FinalConfirm =>
            {
                Some(self.reject())
            }
            KeyAction::Tab if self.phase == FormPhase::Editing => {
                self.move_question(1);
                None
            }
            KeyAction::BackTab if self.phase == FormPhase::FinalConfirm => {
                self.phase = FormPhase::Editing;
                self.current_index = self.questions().len().saturating_sub(1);
                self.reset_question_state();
                self.last_input_rect = None;
                self.scroll = 0;
                None
            }
            KeyAction::BackTab if self.phase == FormPhase::Editing => {
                self.move_question(-1);
                None
            }
            KeyAction::CursorLeft if self.phase == FormPhase::FinalConfirm => {
                self.confirm_focus = 0;
                None
            }
            KeyAction::CursorRight if self.phase == FormPhase::FinalConfirm => {
                self.confirm_focus = 1;
                None
            }
            KeyAction::HistoryUp | KeyAction::Char('k') => {
                self.move_cursor(-1);
                None
            }
            KeyAction::HistoryDown | KeyAction::Char('j') => {
                self.move_cursor(1);
                None
            }
            KeyAction::Char(' ') => {
                self.toggle_current();
                None
            }
            KeyAction::PageUp | KeyAction::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(3);
                None
            }
            KeyAction::PageDown | KeyAction::ScrollDown => {
                self.scroll = self.scroll.saturating_add(3);
                None
            }
            KeyAction::Backspace
            | KeyAction::Delete
            | KeyAction::DeleteWordBackward
            | KeyAction::CursorLeft
            | KeyAction::CursorRight
            | KeyAction::CursorHome
            | KeyAction::CursorEnd
            | KeyAction::Char(_)
            | KeyAction::Newline
                if matches!(self.current_kind(), Some(FormKind::Text { .. })) =>
            {
                self.text_editor.handle_key(action);
                None
            }
            _ => None,
        };
        if let Some(Some(SubmitOutcome::Submit {
            form_id,
            submission,
        })) = outcome.map(Some)
        {
            if let Some(tx) = tx {
                let _ = tx.send(crate::TuiControl::FormSubmit {
                    form_id,
                    submission,
                });
            }
        }
        Some(ModalAction::Consumed)
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        self.last_input_rect
            .map(|r| (r.x + self.text_editor.cursor_display_col() as u16, r.y))
    }
    fn title(&self) -> Line<'static> {
        Line::from(" form ")
    }
    fn icon(&self) -> &str {
        "📋"
    }
    fn accent(&self, t: &crate::theme::Theme) -> ratatui::style::Color {
        t.accent.into()
    }
}

fn questions(form: &PendingForm) -> Vec<FormQuestion> {
    if form.form.questions.is_empty() {
        vec![FormQuestion {
            id: "question".into(),
            kind: form.kind.clone(),
        }]
    } else {
        form.form.questions.clone()
    }
}

pub fn estimate_height(kind: &FormKind, width: u16) -> u16 {
    let width = width.max(1) as usize;
    let prompt = crate::width::width(kind.prompt()).max(1).div_ceil(width) as u16;
    prompt
        + match kind {
            FormKind::Confirm { .. } => 3,
            FormKind::SingleSelect { options, .. } | FormKind::MultiSelect { options, .. } => {
                options.len() as u16 + 2
            }
            FormKind::Text { multiline, .. } => {
                if *multiline {
                    4
                } else {
                    2
                }
            }
        }
}

fn hint_for(kind: &FormKind) -> &'static str {
    match kind {
        FormKind::Confirm { .. } => " ←→/jk · move  enter · next ",
        FormKind::SingleSelect { .. } => " ↑↓/jk · move  enter · next ",
        FormKind::MultiSelect { .. } => " ↑↓/jk · move  space · toggle  tab · next ",
        FormKind::Text { .. } => " tab · next  enter · next ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wm::modal::ModalOverlay;
    use atman_runtime::event::FlowRunId;
    fn mk_questions(kinds: Vec<FormKind>) -> PendingForm {
        PendingForm {
            form_id: "f".into(),
            run_id: FlowRunId::now(),
            tool_use_id: "t".into(),
            kind: kinds[0].clone(),
            form: atman_runtime::form::CompositeForm {
                questions: kinds
                    .into_iter()
                    .enumerate()
                    .map(|(i, kind)| FormQuestion {
                        id: i.to_string(),
                        kind,
                    })
                    .collect(),
            },
            emitted_at: chrono::Utc::now(),
        }
    }
    #[test]
    fn attach_preserves_composite_questions() {
        let mut m = FormModal::default();
        m.attach_test(mk_questions(vec![
            FormKind::Text {
                prompt: "a".into(),
                placeholder: None,
                multiline: false,
            },
            FormKind::Confirm { prompt: "b".into() },
        ]));
        assert_eq!(m.draft_answers.len(), 2);
    }
    #[test]
    fn tab_navigates_without_registry_order() {
        let mut m = FormModal::default();
        m.attach_test(mk_questions(vec![
            FormKind::SingleSelect {
                prompt: "a".into(),
                options: vec!["x".into()],
            },
            FormKind::Confirm { prompt: "b".into() },
        ]));
        m.handle_key(&KeyAction::Tab, &mut crate::app::AppState::default(), None);
        assert_eq!(m.current_index, 1);
    }
    #[test]
    fn final_yes_submits_all_drafts_once() {
        let mut m = FormModal::default();
        m.attach_test(mk_questions(vec![
            FormKind::SingleSelect {
                prompt: "a".into(),
                options: vec!["x".into()],
            },
            FormKind::Confirm { prompt: "b".into() },
        ]));
        m.submit();
        m.submit();
        assert!(
            matches!(m.submit(), SubmitOutcome::Submit { submission: FormSubmission::Submitted { answers }, .. } if answers.len() == 2)
        );
    }
    #[test]
    fn final_confirmation_keys_control_focus_and_submission() {
        let mut m = FormModal::default();
        m.attach_test(mk_questions(vec![FormKind::Text {
            prompt: "last question".into(),
            placeholder: None,
            multiline: false,
        }]));
        m.submit();
        assert_eq!(m.phase, FormPhase::FinalConfirm);
        m.handle_key(
            &KeyAction::CursorRight,
            &mut crate::app::AppState::default(),
            None,
        );
        assert_eq!(m.confirm_focus, 1);
        let out = m.handle_key(
            &KeyAction::Submit,
            &mut crate::app::AppState::default(),
            None,
        );
        assert!(matches!(out, Some(ModalAction::Consumed)));
        assert!(!m.open);

        let mut m = FormModal::default();
        m.attach_test(mk_questions(vec![FormKind::Text {
            prompt: "last question".into(),
            placeholder: None,
            multiline: false,
        }]));
        m.submit();
        m.handle_key(
            &KeyAction::CursorLeft,
            &mut crate::app::AppState::default(),
            None,
        );
        assert_eq!(m.confirm_focus, 0);
        assert!(matches!(
            m.submit(),
            SubmitOutcome::Submit {
                submission: FormSubmission::Submitted { .. },
                ..
            }
        ));
    }

    #[test]
    fn backtab_from_final_confirmation_returns_to_last_question() {
        let mut m = FormModal::default();
        m.attach_test(mk_questions(vec![
            FormKind::Text {
                prompt: "first".into(),
                placeholder: None,
                multiline: false,
            },
            FormKind::Text {
                prompt: "last".into(),
                placeholder: None,
                multiline: false,
            },
        ]));
        m.submit();
        m.submit();
        assert_eq!(m.phase, FormPhase::FinalConfirm);
        m.handle_key(
            &KeyAction::BackTab,
            &mut crate::app::AppState::default(),
            None,
        );
        assert_eq!(m.phase, FormPhase::Editing);
        assert_eq!(m.current_index, 1);
        assert_eq!(m.current_kind().unwrap().prompt(), "last");
    }

    #[test]
    fn final_no_and_escape_reject_one_request() {
        for action in [KeyAction::Char('n'), KeyAction::Escape] {
            let mut m = FormModal::default();
            m.attach_test(mk_questions(vec![FormKind::Confirm { prompt: "a".into() }]));
            m.submit();
            let out = if matches!(action, KeyAction::Escape) {
                m.cancel()
            } else {
                m.reject()
            };
            assert!(matches!(
                out,
                SubmitOutcome::Submit {
                    submission: FormSubmission::Rejected,
                    ..
                }
            ));
        }
    }

    #[test]
    fn final_confirmation_backtab_returns_to_last_question() {
        let mut m = FormModal::default();
        m.attach_test(mk_questions(vec![
            FormKind::Confirm {
                prompt: "first".into(),
            },
            FormKind::Confirm {
                prompt: "last".into(),
            },
        ]));
        m.submit();
        m.submit();
        assert_eq!(m.phase, FormPhase::FinalConfirm);
        assert_eq!(m.current_index, 1);

        m.handle_key(
            &KeyAction::BackTab,
            &mut crate::app::AppState::default(),
            None,
        );
        assert_eq!(m.phase, FormPhase::Editing);
        assert_eq!(m.current_index, 1);
        assert_eq!(m.current_kind().unwrap().prompt(), "last");
    }

    #[test]
    fn final_confirmation_arrows_choose_submission() {
        for (arrow, expected) in [
            (
                KeyAction::CursorLeft,
                FormSubmission::Submitted {
                    answers: vec![
                        FormAnswer::Confirmed { value: true },
                        FormAnswer::Confirmed { value: true },
                    ],
                },
            ),
            (KeyAction::CursorRight, FormSubmission::Rejected),
        ] {
            let mut m = FormModal::default();
            m.attach_test(mk_questions(vec![
                FormKind::Confirm {
                    prompt: "first".into(),
                },
                FormKind::Confirm {
                    prompt: "last".into(),
                },
            ]));
            m.submit();
            m.submit();
            let mut app = crate::app::AppState::default();
            m.handle_key(&arrow, &mut app, None);
            let out = m.submit();
            assert!(
                matches!(out, SubmitOutcome::Submit { submission, .. } if submission == expected)
            );
        }
    }
}
