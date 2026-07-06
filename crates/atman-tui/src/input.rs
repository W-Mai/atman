use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

pub fn input_paragraph<'a>(input: &'a str, streaming: bool) -> Paragraph<'a> {
    let prompt_style = if streaming {
        Style::default().add_modifier(Modifier::DIM)
    } else {
        Style::default().add_modifier(Modifier::BOLD)
    };
    let line = Line::from(vec![
        Span::styled("atman> ", prompt_style),
        Span::raw(input),
        Span::styled("▏", Style::default().add_modifier(Modifier::SLOW_BLINK)),
    ]);
    Paragraph::new(line).wrap(Wrap { trim: false })
}

#[derive(Default)]
pub struct InputEditor {
    buf: String,
    history: Vec<String>,
    history_idx: Option<usize>,
    stashed: Option<String>,
}

impl InputEditor {
    pub fn buf(&self) -> &str {
        &self.buf
    }

    pub fn push_char(&mut self, c: char) {
        self.reset_history_view();
        self.buf.push(c);
    }

    pub fn backspace(&mut self) {
        self.reset_history_view();
        self.buf.pop();
    }

    pub fn clear(&mut self) -> String {
        self.reset_history_view();
        std::mem::take(&mut self.buf)
    }

    pub fn prefill(&mut self, prefix: &str) {
        self.reset_history_view();
        if !self.buf.starts_with(prefix) {
            let existing = std::mem::take(&mut self.buf);
            self.buf.push_str(prefix);
            self.buf.push_str(&existing);
        }
    }

    pub fn submit(&mut self) -> Option<String> {
        let line = std::mem::take(&mut self.buf);
        self.history_idx = None;
        self.stashed = None;
        if line.trim().is_empty() {
            return None;
        }
        if self.history.last().is_none_or(|prev| prev != &line) {
            self.history.push(line.clone());
        }
        Some(line)
    }

    pub fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        let new_idx = match self.history_idx {
            None => {
                self.stashed = Some(self.buf.clone());
                self.history.len() - 1
            }
            Some(0) => 0,
            Some(i) => i - 1,
        };
        self.history_idx = Some(new_idx);
        self.buf = self.history[new_idx].clone();
    }

    pub fn history_down(&mut self) {
        let Some(i) = self.history_idx else {
            return;
        };
        if i + 1 >= self.history.len() {
            self.history_idx = None;
            self.buf = self.stashed.take().unwrap_or_default();
        } else {
            self.history_idx = Some(i + 1);
            self.buf = self.history[i + 1].clone();
        }
    }

    fn reset_history_view(&mut self) {
        if self.history_idx.is_some() {
            self.history_idx = None;
            self.stashed = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn submit_returns_line_and_records_history() {
        let mut ed = InputEditor::default();
        ed.push_char('h');
        ed.push_char('i');
        assert_eq!(ed.submit().as_deref(), Some("hi"));
        assert_eq!(ed.buf(), "");
    }

    #[test]
    fn submit_empty_is_none_and_not_recorded() {
        let mut ed = InputEditor::default();
        assert!(ed.submit().is_none());
        ed.history_up();
        assert_eq!(ed.buf(), "");
    }

    #[test]
    fn history_up_down_walks_prior_lines() {
        let mut ed = InputEditor::default();
        "one".chars().for_each(|c| ed.push_char(c));
        ed.submit();
        "two".chars().for_each(|c| ed.push_char(c));
        ed.submit();
        "wip".chars().for_each(|c| ed.push_char(c));
        ed.history_up();
        assert_eq!(ed.buf(), "two");
        ed.history_up();
        assert_eq!(ed.buf(), "one");
        ed.history_down();
        assert_eq!(ed.buf(), "two");
        ed.history_down();
        assert_eq!(ed.buf(), "wip");
    }

    #[test]
    fn prefill_prepends_prefix_when_absent() {
        let mut ed = InputEditor::default();
        "foo".chars().for_each(|c| ed.push_char(c));
        ed.prefill("/nudge ");
        assert_eq!(ed.buf(), "/nudge foo");
    }

    #[test]
    fn prefill_is_idempotent_when_prefix_already_present() {
        let mut ed = InputEditor::default();
        "/nudge stop".chars().for_each(|c| ed.push_char(c));
        ed.prefill("/nudge ");
        assert_eq!(ed.buf(), "/nudge stop");
    }

    #[test]
    fn deduplicates_consecutive_identical_submissions() {
        let mut ed = InputEditor::default();
        "hi".chars().for_each(|c| ed.push_char(c));
        ed.submit();
        "hi".chars().for_each(|c| ed.push_char(c));
        ed.submit();
        ed.history_up();
        assert_eq!(ed.buf(), "hi");
        ed.history_up();
        assert_eq!(ed.buf(), "hi");
    }
}
