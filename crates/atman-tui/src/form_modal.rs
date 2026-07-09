use atman_runtime::form::{FormAnswer, FormKind, PendingForm};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use crate::input::InputEditor;

#[derive(Default)]
pub struct FormModal {
    pub open: bool,
    pub pending: Option<PendingForm>,
    pub cursor: usize,
    pub multi_selected: Vec<bool>,
    pub text_editor: InputEditor,
    pub error: Option<String>,
}

impl FormModal {
    pub fn attach(&mut self, form: PendingForm) {
        let multi_len = match &form.kind {
            FormKind::MultiSelect { options, .. } => options.len(),
            _ => 0,
        };
        self.multi_selected = vec![false; multi_len];
        self.text_editor = InputEditor::default();
        self.cursor = 0;
        self.error = None;
        self.open = true;
        self.pending = Some(form);
    }

    pub fn close(&mut self) {
        self.open = false;
        self.pending = None;
        self.error = None;
        self.multi_selected.clear();
        self.cursor = 0;
    }

    pub fn active_form_id(&self) -> Option<&str> {
        self.pending.as_ref().map(|p| p.form_id.as_str())
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let len = match self.pending.as_ref().map(|p| &p.kind) {
            Some(FormKind::SingleSelect { options, .. }) => options.len(),
            Some(FormKind::MultiSelect { options, .. }) => options.len(),
            _ => return,
        };
        if len == 0 {
            return;
        }
        let cur = self.cursor as isize + delta;
        self.cursor = cur.rem_euclid(len as isize) as usize;
        self.error = None;
    }

    pub fn toggle_current(&mut self) {
        if matches!(
            self.pending.as_ref().map(|p| &p.kind),
            Some(FormKind::MultiSelect { .. })
        ) {
            if let Some(flag) = self.multi_selected.get_mut(self.cursor) {
                *flag = !*flag;
                self.error = None;
            }
        }
    }

    // Returns the answer to send + closes the modal, or None if validation
    // rejected the submit (in which case `error` is set for the render pass).
    pub fn submit(&mut self) -> Option<FormAnswer> {
        let kind = self.pending.as_ref().map(|p| p.kind.clone())?;
        let answer = match kind {
            FormKind::Confirm { .. } => FormAnswer::Confirmed { value: true },
            FormKind::SingleSelect { options, .. } => {
                let label = options.get(self.cursor)?.clone();
                FormAnswer::Selected {
                    index: self.cursor,
                    label,
                }
            }
            FormKind::MultiSelect {
                options, min, max, ..
            } => {
                let count = self.multi_selected.iter().filter(|&&b| b).count();
                if let Some(m) = min
                    && count < m
                {
                    self.error = Some(format!("Select at least {m}"));
                    return None;
                }
                if let Some(m) = max
                    && count > m
                {
                    self.error = Some(format!("Select at most {m}"));
                    return None;
                }
                let indices: Vec<usize> = self
                    .multi_selected
                    .iter()
                    .enumerate()
                    .filter_map(|(i, &b)| b.then_some(i))
                    .collect();
                let labels: Vec<String> = indices
                    .iter()
                    .filter_map(|&i| options.get(i).cloned())
                    .collect();
                FormAnswer::MultiSelected { indices, labels }
            }
            FormKind::Text { .. } => FormAnswer::TextEntered {
                text: self.text_editor.buf().to_string(),
            },
        };
        self.close();
        Some(answer)
    }

    pub fn confirm_no(&mut self) -> Option<FormAnswer> {
        if matches!(
            self.pending.as_ref().map(|p| &p.kind),
            Some(FormKind::Confirm { .. })
        ) {
            self.close();
            Some(FormAnswer::Confirmed { value: false })
        } else {
            None
        }
    }

    pub fn cancel(&mut self) -> Option<FormAnswer> {
        if !self.open {
            return None;
        }
        self.close();
        Some(FormAnswer::Cancelled)
    }
}

pub fn render(f: &mut ratatui::Frame, area: Rect, modal: &FormModal) {
    let Some(form) = modal.pending.as_ref() else {
        return;
    };
    let outer_width = (area.width.saturating_mul(3) / 4).clamp(50, 100);
    let content_lines = estimate_height(&form.kind, &modal.multi_selected);
    let outer_height = (content_lines + 6).min(area.height.saturating_sub(4).max(6));
    let x = area.x + (area.width.saturating_sub(outer_width)) / 2;
    let y = area.y + (area.height.saturating_sub(outer_height)) / 2;
    let rect = Rect {
        x,
        y,
        width: outer_width,
        height: outer_height,
    };
    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .title(Span::styled(
            format!(" form · {} ", form.kind.discriminator()),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ))
        .title_bottom(
            Line::from(Span::styled(
                hint_for(&form.kind),
                Style::default().fg(Color::DarkGray),
            ))
            .right_aligned(),
        );
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        form.kind.prompt().to_string(),
        Style::default().add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    match &form.kind {
        FormKind::Confirm { .. } => {
            lines.push(Line::from(vec![
                Span::styled(
                    "  [Y]es ",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("   "),
                Span::styled("[N]o ", Style::default().fg(Color::Red)),
            ]));
        }
        FormKind::SingleSelect { options, .. } => {
            for (i, label) in options.iter().enumerate() {
                let marker = if i == modal.cursor { "▶ " } else { "  " };
                let style = if i == modal.cursor {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::styled(marker.to_string(), style),
                    Span::styled(label.clone(), style),
                ]));
            }
        }
        FormKind::MultiSelect {
            options, min, max, ..
        } => {
            for (i, label) in options.iter().enumerate() {
                let checked = modal.multi_selected.get(i).copied().unwrap_or(false);
                let box_glyph = if checked { "[x]" } else { "[ ]" };
                let cursor_prefix = if i == modal.cursor { "▶ " } else { "  " };
                let row_style = if i == modal.cursor {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::from(vec![
                    Span::styled(cursor_prefix.to_string(), row_style),
                    Span::styled(format!("{box_glyph} "), row_style),
                    Span::styled(label.clone(), row_style),
                ]));
            }
            let count = modal.multi_selected.iter().filter(|&&b| b).count();
            let bounds = match (min, max) {
                (Some(m), Some(mx)) => format!(" (min {m}, max {mx})"),
                (Some(m), None) => format!(" (min {m})"),
                (None, Some(mx)) => format!(" (max {mx})"),
                (None, None) => String::new(),
            };
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                format!("  {count} selected{bounds}"),
                Style::default().fg(Color::DarkGray),
            )));
        }
        FormKind::Text {
            placeholder,
            multiline,
            ..
        } => {
            let buf = modal.text_editor.buf();
            let display: String = if buf.is_empty() {
                placeholder.clone().unwrap_or_default()
            } else {
                buf.to_string()
            };
            let placeholder_style = if buf.is_empty() {
                Style::default().fg(Color::DarkGray)
            } else {
                Style::default()
            };
            for row in display.split('\n') {
                lines.push(Line::from(Span::styled(
                    format!("  {row}"),
                    placeholder_style,
                )));
            }
            if buf.is_empty() {
                lines.push(Line::from(Span::styled(
                    "  ▏",
                    Style::default().add_modifier(Modifier::SLOW_BLINK),
                )));
            }
            if *multiline {
                lines.push(Line::from(Span::styled(
                    "  (Ctrl+Enter to submit multi-line input)",
                    Style::default().fg(Color::DarkGray),
                )));
            }
        }
    }
    if let Some(err) = &modal.error {
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            format!("  ! {err}"),
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        )));
    }
    let para = Paragraph::new(lines)
        .alignment(Alignment::Left)
        .wrap(Wrap { trim: false });
    f.render_widget(para, inner);
}

fn estimate_height(kind: &FormKind, _multi: &[bool]) -> u16 {
    match kind {
        FormKind::Confirm { .. } => 3,
        FormKind::SingleSelect { options, .. } => options.len().min(12) as u16 + 2,
        FormKind::MultiSelect { options, .. } => options.len().min(12) as u16 + 3,
        FormKind::Text { multiline, .. } => {
            if *multiline {
                6
            } else {
                3
            }
        }
    }
}

fn hint_for(kind: &FormKind) -> &'static str {
    match kind {
        FormKind::Confirm { .. } => " y/enter · yes  n · no  esc · cancel ",
        FormKind::SingleSelect { .. } => " ↑↓/jk · move  enter · pick  esc · cancel ",
        FormKind::MultiSelect { .. } => {
            " ↑↓/jk · move  space · toggle  enter · submit  esc · cancel "
        }
        FormKind::Text { .. } => " enter · submit  esc · cancel ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::event::FlowRunId;

    fn mk(kind: FormKind) -> PendingForm {
        PendingForm {
            form_id: "f".into(),
            run_id: FlowRunId::now(),
            tool_use_id: "t".into(),
            kind,
            emitted_at: chrono::Utc::now(),
        }
    }

    #[test]
    fn attach_resets_state_per_kind() {
        let mut m = FormModal::default();
        m.attach(mk(FormKind::MultiSelect {
            prompt: "?".into(),
            options: vec!["a".into(), "b".into(), "c".into()],
            min: None,
            max: None,
        }));
        assert_eq!(m.multi_selected.len(), 3);
        assert!(m.open);
    }

    #[test]
    fn move_cursor_wraps() {
        let mut m = FormModal::default();
        m.attach(mk(FormKind::SingleSelect {
            prompt: "?".into(),
            options: vec!["a".into(), "b".into()],
        }));
        m.move_cursor(-1);
        assert_eq!(m.cursor, 1);
        m.move_cursor(1);
        assert_eq!(m.cursor, 0);
    }

    #[test]
    fn confirm_submit_yields_true() {
        let mut m = FormModal::default();
        m.attach(mk(FormKind::Confirm {
            prompt: "sure?".into(),
        }));
        let a = m.submit().unwrap();
        assert!(matches!(a, FormAnswer::Confirmed { value: true }));
        assert!(!m.open);
    }

    #[test]
    fn confirm_no_yields_false() {
        let mut m = FormModal::default();
        m.attach(mk(FormKind::Confirm {
            prompt: "sure?".into(),
        }));
        let a = m.confirm_no().unwrap();
        assert!(matches!(a, FormAnswer::Confirmed { value: false }));
    }

    #[test]
    fn multi_select_min_bound_rejects_empty_submit() {
        let mut m = FormModal::default();
        m.attach(mk(FormKind::MultiSelect {
            prompt: "?".into(),
            options: vec!["a".into(), "b".into()],
            min: Some(1),
            max: None,
        }));
        assert!(m.submit().is_none());
        assert!(m.error.is_some());
        assert!(m.open, "modal stays open after failed submit");
    }

    #[test]
    fn multi_select_max_bound_rejects_overfull_submit() {
        let mut m = FormModal::default();
        m.attach(mk(FormKind::MultiSelect {
            prompt: "?".into(),
            options: vec!["a".into(), "b".into(), "c".into()],
            min: None,
            max: Some(1),
        }));
        m.cursor = 0;
        m.toggle_current();
        m.cursor = 1;
        m.toggle_current();
        assert!(m.submit().is_none());
        assert!(m.error.is_some());
    }

    #[test]
    fn multi_select_valid_submit_returns_indices_and_labels() {
        let mut m = FormModal::default();
        m.attach(mk(FormKind::MultiSelect {
            prompt: "?".into(),
            options: vec!["a".into(), "b".into(), "c".into()],
            min: None,
            max: None,
        }));
        m.cursor = 0;
        m.toggle_current();
        m.cursor = 2;
        m.toggle_current();
        let a = m.submit().unwrap();
        match a {
            FormAnswer::MultiSelected { indices, labels } => {
                assert_eq!(indices, vec![0, 2]);
                assert_eq!(labels, vec!["a", "c"]);
            }
            _ => panic!("expected multi selected"),
        }
    }

    #[test]
    fn text_submit_returns_editor_buf() {
        let mut m = FormModal::default();
        m.attach(mk(FormKind::Text {
            prompt: "?".into(),
            placeholder: None,
            multiline: false,
        }));
        m.text_editor.insert_str("hi there");
        let a = m.submit().unwrap();
        assert!(matches!(a, FormAnswer::TextEntered { text } if text == "hi there"));
    }

    #[test]
    fn cancel_from_open_returns_cancelled() {
        let mut m = FormModal::default();
        m.attach(mk(FormKind::Confirm { prompt: "?".into() }));
        let a = m.cancel().unwrap();
        assert!(matches!(a, FormAnswer::Cancelled));
        assert!(!m.open);
    }
}
