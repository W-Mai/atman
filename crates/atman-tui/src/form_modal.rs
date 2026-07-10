use atman_runtime::form::{FormAnswer, FormKind, PendingForm};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph, Wrap};

use crate::input::InputEditor;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchStatus {
    Pending,
    Answered,
    Cancelled,
}

#[derive(Default)]
pub struct FormModal {
    pub open: bool,
    pub pending: Option<PendingForm>,
    pub cursor: usize,
    pub multi_selected: Vec<bool>,
    pub text_editor: InputEditor,
    pub error: Option<String>,
    pub batch_ids: Vec<String>,
    pub batch_statuses: Vec<BatchStatus>,
    pub batch_index: usize,
    pub pending_switch: Option<String>,
}

impl FormModal {
    pub fn attach(&mut self, form: PendingForm, pending_ids: &[String]) {
        let multi_len = match &form.kind {
            FormKind::MultiSelect { options, .. } => options.len(),
            _ => 0,
        };
        self.multi_selected = vec![false; multi_len];
        self.text_editor = InputEditor::default();
        self.cursor = 0;
        self.error = None;
        for id in pending_ids {
            if !self.batch_ids.contains(id) {
                self.batch_ids.push(id.clone());
                self.batch_statuses.push(BatchStatus::Pending);
            }
        }
        if let Some(idx) = self.batch_ids.iter().position(|id| id == &form.form_id) {
            self.batch_index = idx;
        }
        self.open = true;
        self.pending = Some(form);
    }

    pub fn mark_current(&mut self, status: BatchStatus) {
        if let Some(slot) = self.batch_statuses.get_mut(self.batch_index) {
            *slot = status;
        }
    }

    pub fn switch_to(&mut self, direction: isize) -> Option<String> {
        if self.batch_ids.len() <= 1 {
            return None;
        }
        let len = self.batch_ids.len() as isize;
        let mut cursor = self.batch_index as isize;
        for _ in 0..len {
            cursor = (cursor + direction).rem_euclid(len);
            let i = cursor as usize;
            if i == self.batch_index {
                continue;
            }
            if matches!(self.batch_statuses.get(i), Some(BatchStatus::Pending)) {
                self.batch_index = i;
                return Some(self.batch_ids[i].clone());
            }
        }
        None
    }

    pub fn next_pending_id(&self, direction: isize) -> Option<String> {
        if self.batch_ids.is_empty() {
            return None;
        }
        let len = self.batch_ids.len() as isize;
        if len <= 1 {
            return None;
        }
        let next = (self.batch_index as isize + direction).rem_euclid(len) as usize;
        Some(self.batch_ids[next].clone())
    }

    pub fn close(&mut self) {
        self.open = false;
        self.pending = None;
        self.error = None;
        self.multi_selected.clear();
        self.cursor = 0;
    }

    pub fn end_batch(&mut self) {
        self.close();
        self.batch_ids.clear();
        self.batch_statuses.clear();
        self.batch_index = 0;
    }

    pub fn batch_total(&self) -> usize {
        self.batch_ids.len()
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
        self.mark_current(BatchStatus::Answered);
        if let Some(next) = self.switch_to(1) {
            self.pending_switch = Some(next);
        }
        self.close();
        Some(answer)
    }

    pub fn confirm_no(&mut self) -> Option<FormAnswer> {
        if matches!(
            self.pending.as_ref().map(|p| &p.kind),
            Some(FormKind::Confirm { .. })
        ) {
            self.mark_current(BatchStatus::Answered);
            if let Some(next) = self.switch_to(1) {
                self.pending_switch = Some(next);
            }
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
        self.pending.as_ref()?;
        self.mark_current(BatchStatus::Cancelled);
        self.close();
        if let Some(next) = self.switch_to(1) {
            self.pending_switch = Some(next);
        }
        Some(FormAnswer::Cancelled)
    }

    #[cfg(test)]
    fn attach_test(&mut self, form: PendingForm) {
        let id = form.form_id.clone();
        self.attach(form, &[id]);
    }
}

pub fn render(f: &mut ratatui::Frame, area: Rect, modal: &FormModal) {
    let Some(form) = modal.pending.as_ref() else {
        return;
    };
    let t = crate::theme::theme();
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
    crate::sanitize_widget_edges(f, rect);
    f.render_widget(Clear, rect);
    let title_spans = build_title_spans(form.kind.discriminator(), modal);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(Color::Cyan))
        .style(Style::default().bg(t.code_bg))
        .title(Line::from(title_spans))
        .title_bottom(
            Line::from(Span::styled(
                hint_for(&form.kind),
                Style::default().fg(t.subtle_fg),
            ))
            .right_aligned(),
        );
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    let inner_w = inner.width as usize;
    let prompt_style = Style::default()
        .fg(t.tinted_fg)
        .add_modifier(Modifier::BOLD);
    let dim_style = Style::default().fg(t.subtle_fg);
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        form.kind.prompt().to_string(),
        prompt_style,
    )));
    lines.push(Line::from(""));

    match &form.kind {
        FormKind::Confirm { .. } => {
            let yes_selected = matches!(
                modal.batch_statuses.get(modal.batch_index),
                Some(BatchStatus::Answered)
            );
            let no_selected = matches!(
                modal.batch_statuses.get(modal.batch_index),
                Some(BatchStatus::Cancelled)
            );
            let yes_style = if yes_selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            };
            let no_style = if no_selected {
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Red)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Red)
            };
            lines.push(render_pill(inner_w, "  Yes  ", yes_style, dim_style));
            lines.push(Line::from(""));
            lines.push(render_pill(inner_w, "  No  ", no_style, dim_style));
        }
        FormKind::SingleSelect { options, .. } => {
            for (i, label) in options.iter().enumerate() {
                let is_cursor = i == modal.cursor;
                let row_style = if is_cursor {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(t.tinted_fg)
                };
                let prefix = if is_cursor { "▶ " } else { "  " };
                let text = format!("{prefix}{label}");
                lines.push(render_row_bg(inner_w, &text, row_style, t.code_bg));
            }
        }
        FormKind::MultiSelect {
            options, min, max, ..
        } => {
            for (i, label) in options.iter().enumerate() {
                let checked = modal.multi_selected.get(i).copied().unwrap_or(false);
                let is_cursor = i == modal.cursor;
                let check_glyph = if checked { "✓" } else { " " };
                let row_style = if is_cursor && checked {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else if is_cursor {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else if checked {
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(t.tinted_fg)
                };
                let prefix = if is_cursor { "▶ " } else { "  " };
                let text = format!("{prefix}[{check_glyph}] {label}");
                lines.push(render_row_bg(inner_w, &text, row_style, t.code_bg));
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
                dim_style,
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
            let text_style = if buf.is_empty() {
                dim_style
            } else {
                prompt_style
            };
            for row in display.split('\n') {
                lines.push(Line::from(Span::styled(format!("  {row}"), text_style)));
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
                    dim_style,
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

fn render_row_bg<'a>(width: usize, text: &str, style: Style, bg: Color) -> Line<'a> {
    let text_w = unicode_width::UnicodeWidthStr::width(text);
    let pad = width.saturating_sub(text_w);
    let mut spans: Vec<Span<'a>> = Vec::new();
    spans.push(Span::styled(text.to_string(), style));
    if pad > 0 {
        let bg_fill_style = Style::default().bg(style.bg.unwrap_or(bg));
        spans.push(Span::styled(" ".repeat(pad), bg_fill_style));
    }
    Line::from(spans)
}

fn render_pill<'a>(width: usize, label: &str, style: Style, _dim: Style) -> Line<'a> {
    let label_w = unicode_width::UnicodeWidthStr::width(label);
    let _ = width;
    Line::from(vec![
        Span::styled(label.to_string(), style),
        Span::raw(" ".repeat(label_w.min(2))),
    ])
}

fn build_title_spans(kind_name: &str, modal: &FormModal) -> Vec<Span<'static>> {
    let bold_cyan = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let mut spans: Vec<Span<'static>> = Vec::new();
    spans.push(Span::styled(format!(" form · {kind_name} "), bold_cyan));
    let total = modal.batch_total();
    if total > 1 {
        spans.push(Span::raw(" "));
        for (i, status) in modal.batch_statuses.iter().enumerate() {
            let style = if i == modal.batch_index {
                match status {
                    BatchStatus::Pending => Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                    BatchStatus::Answered => Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                    BatchStatus::Cancelled => {
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                    }
                }
            } else {
                match status {
                    BatchStatus::Pending => Style::default().fg(Color::DarkGray),
                    BatchStatus::Answered => Style::default().fg(Color::Green),
                    BatchStatus::Cancelled => Style::default().fg(Color::Red),
                }
            };
            spans.push(Span::styled("━━━", style));
            if i + 1 < total {
                spans.push(Span::raw(" "));
            }
        }
        spans.push(Span::styled(
            format!(" {}/{} ", modal.batch_index + 1, total),
            Style::default().fg(Color::DarkGray),
        ));
    }
    spans
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
        m.attach_test(mk(FormKind::MultiSelect {
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
        m.attach_test(mk(FormKind::SingleSelect {
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
        m.attach_test(mk(FormKind::Confirm {
            prompt: "sure?".into(),
        }));
        let a = m.submit().unwrap();
        assert!(matches!(a, FormAnswer::Confirmed { value: true }));
        assert!(!m.open);
    }

    #[test]
    fn confirm_no_yields_false() {
        let mut m = FormModal::default();
        m.attach_test(mk(FormKind::Confirm {
            prompt: "sure?".into(),
        }));
        let a = m.confirm_no().unwrap();
        assert!(matches!(a, FormAnswer::Confirmed { value: false }));
    }

    #[test]
    fn multi_select_min_bound_rejects_empty_submit() {
        let mut m = FormModal::default();
        m.attach_test(mk(FormKind::MultiSelect {
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
        m.attach_test(mk(FormKind::MultiSelect {
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
        m.attach_test(mk(FormKind::MultiSelect {
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
        m.attach_test(mk(FormKind::Text {
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
        m.attach_test(mk(FormKind::Confirm { prompt: "?".into() }));
        let a = m.cancel().unwrap();
        assert!(matches!(a, FormAnswer::Cancelled));
        assert!(!m.open);
    }
}
