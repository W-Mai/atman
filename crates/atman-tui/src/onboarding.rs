use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::input::InputEditor;
use crate::keys::KeyAction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingStep {
    ProviderSelect,
    ApiKeyEntry,
    ModelSelect,
}

pub struct OnboardingState {
    pub step: OnboardingStep,
    pub selected_provider: usize,
    pub selected_model: usize,
    pub input: InputEditor,
    pub error: Option<String>,
}

impl Default for OnboardingState {
    fn default() -> Self {
        Self {
            step: OnboardingStep::ProviderSelect,
            selected_provider: 0,
            selected_model: 0,
            input: InputEditor::default(),
            error: None,
        }
    }
}

pub enum OnboardingEvent {
    None,
    Completed,
    Skipped,
}

impl OnboardingState {
    pub fn handle_key(&mut self, action: &KeyAction) -> OnboardingEvent {
        self.error = None;
        match self.step {
            OnboardingStep::ProviderSelect => self.handle_provider_key(action),
            OnboardingStep::ApiKeyEntry => self.handle_input_key(action),
            OnboardingStep::ModelSelect => self.handle_model_key(action),
        }
    }

    fn handle_provider_key(&mut self, action: &KeyAction) -> OnboardingEvent {
        let presets = atman_runtime::model_registry::PROVIDER_PRESETS;
        match action {
            KeyAction::Char('q') | KeyAction::Quit => OnboardingEvent::Skipped,
            KeyAction::HistoryUp | KeyAction::Char('k') => {
                self.selected_provider = self
                    .selected_provider
                    .checked_sub(1)
                    .unwrap_or(presets.len().saturating_sub(1));
                OnboardingEvent::None
            }
            KeyAction::HistoryDown | KeyAction::Char('j') => {
                self.selected_provider = (self.selected_provider + 1) % presets.len().max(1);
                OnboardingEvent::None
            }
            KeyAction::Submit => {
                self.step = OnboardingStep::ApiKeyEntry;
                self.input = InputEditor::default();
                let preset = &presets[self.selected_provider.min(presets.len() - 1)];
                if !preset.needs_api_key && !preset.base_url.is_empty() {
                    self.input.insert_str(preset.base_url);
                }
                OnboardingEvent::None
            }
            _ => OnboardingEvent::None,
        }
    }

    fn handle_input_key(&mut self, action: &KeyAction) -> OnboardingEvent {
        match action {
            KeyAction::Escape => {
                self.step = OnboardingStep::ProviderSelect;
                OnboardingEvent::None
            }
            KeyAction::Backspace => {
                self.input.backspace();
                OnboardingEvent::None
            }
            KeyAction::CursorLeft => {
                self.input.move_left();
                OnboardingEvent::None
            }
            KeyAction::CursorRight => {
                self.input.move_right();
                OnboardingEvent::None
            }
            KeyAction::CursorHome => {
                self.input.move_home();
                OnboardingEvent::None
            }
            KeyAction::CursorEnd => {
                self.input.move_end();
                OnboardingEvent::None
            }
            KeyAction::Char(c) => {
                self.input.insert_char(*c);
                OnboardingEvent::None
            }
            KeyAction::Submit => {
                let preset =
                    &atman_runtime::model_registry::PROVIDER_PRESETS[self.selected_provider];
                if preset.needs_api_key && self.input.buf().trim().is_empty() {
                    self.error = Some("API key is required for this provider".to_string());
                    return OnboardingEvent::None;
                }
                if preset.models.is_empty() {
                    self.error = Some("Custom/Ollama model catalog is empty for now — use Manage Providers after setup".to_string());
                    return OnboardingEvent::None;
                }
                self.step = OnboardingStep::ModelSelect;
                self.selected_model = 0;
                OnboardingEvent::None
            }
            _ => OnboardingEvent::None,
        }
    }

    fn handle_model_key(&mut self, action: &KeyAction) -> OnboardingEvent {
        let preset = &atman_runtime::model_registry::PROVIDER_PRESETS[self.selected_provider];
        let len = preset.models.len();
        match action {
            KeyAction::Escape => {
                self.step = OnboardingStep::ApiKeyEntry;
                OnboardingEvent::None
            }
            KeyAction::HistoryUp | KeyAction::Char('k') if len > 0 => {
                self.selected_model = self.selected_model.checked_sub(1).unwrap_or(len - 1);
                OnboardingEvent::None
            }
            KeyAction::HistoryDown | KeyAction::Char('j') if len > 0 => {
                self.selected_model = (self.selected_model + 1) % len;
                OnboardingEvent::None
            }
            KeyAction::Submit if len > 0 => match self.write_config() {
                Ok(()) => OnboardingEvent::Completed,
                Err(e) => {
                    self.error = Some(format!("setup failed: {e}"));
                    OnboardingEvent::None
                }
            },
            _ => OnboardingEvent::None,
        }
    }

    fn write_config(&self) -> anyhow::Result<()> {
        let preset = &atman_runtime::model_registry::PROVIDER_PRESETS[self.selected_provider];
        let model = &preset.models[self.selected_model];
        atman_runtime::model_registry::upsert_model_config(
            &format!("{}/{}", preset.name.to_ascii_lowercase(), model.id),
            preset.provider_type,
            preset.needs_api_key.then(|| self.input.buf().trim()),
            Some(preset.base_url),
            model.context_budget,
            model.description.to_ascii_lowercase().contains("thinking"),
        )?;
        atman_runtime::model_registry::add_alias_to_config(
            "smart",
            &format!("{}/{}", preset.name.to_ascii_lowercase(), model.id),
        )?;
        Ok(())
    }
}

pub fn render(f: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let w = area.width.saturating_sub(4).clamp(54, 82);
    let h = area.height.saturating_sub(2).clamp(18, 30);
    let rect = Rect {
        x: area.x + area.width.saturating_sub(w) / 2,
        y: area.y + area.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    };

    crate::sanitize_widget_edges(f, rect);
    f.render_widget(Clear, rect);

    let theme = crate::theme::theme();
    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.accent.into()))
        .title(Span::styled(
            " Welcome to atman ",
            Style::default()
                .fg(theme.accent.into())
                .add_modifier(Modifier::BOLD),
        ));
    let inner = outer.inner(rect);
    f.render_widget(outer, rect);

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(7),
            Constraint::Min(0),
            Constraint::Length(2),
        ])
        .split(inner);

    let banner: Vec<Line> = crate::output::STARTUP_BANNER
        .iter()
        .map(|row| {
            Line::from(Span::styled(
                *row,
                Style::default()
                    .fg(theme.accent.into())
                    .add_modifier(Modifier::BOLD),
            ))
        })
        .chain(std::iter::once(Line::from("")))
        .chain(std::iter::once(Line::from(Span::styled(
            "Welcome to atman — let's get set up",
            Style::default().fg(theme.meta_fg.into()),
        ))))
        .collect();
    f.render_widget(
        Paragraph::new(banner).alignment(ratatui::layout::Alignment::Center),
        rows[0],
    );

    match state.step {
        OnboardingStep::ProviderSelect => render_provider_step(f, rows[1], state),
        OnboardingStep::ApiKeyEntry => render_input_step(f, rows[1], state),
        OnboardingStep::ModelSelect => render_model_step(f, rows[1], state),
    }

    let footer = state.error.as_deref().unwrap_or(match state.step {
        OnboardingStep::ProviderSelect => "↑↓/j/k navigate · Enter select · q skip",
        OnboardingStep::ApiKeyEntry => "Enter confirm · Esc back",
        OnboardingStep::ModelSelect => "↑↓/j/k navigate · Enter finish · Esc back",
    });
    let style = if state.error.is_some() {
        Style::default().fg(theme.error.into())
    } else {
        Style::default().fg(theme.meta_fg.into())
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(footer, style))).wrap(Wrap { trim: true }),
        rows[2],
    );
}

fn render_provider_step(f: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let theme = crate::theme::theme();
    let presets = atman_runtime::model_registry::PROVIDER_PRESETS;
    let items: Vec<ListItem> = presets
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let style = if i == state.selected_provider {
                Style::default()
                    .fg(theme.accent.into())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {}", p.name), style),
                Span::styled(
                    format!(" — {}", p.description),
                    Style::default().fg(theme.meta_fg.into()),
                ),
            ]))
        })
        .collect();
    let mut list_state = ListState::default().with_selected(Some(state.selected_provider));
    f.render_widget(
        Paragraph::new("1. Choose your provider"),
        Rect { height: 1, ..area },
    );
    let list_area = Rect {
        y: area.y + 2,
        height: area.height.saturating_sub(2),
        ..area
    };
    f.render_stateful_widget(List::new(items), list_area, &mut list_state);
}

fn render_input_step(f: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let theme = crate::theme::theme();
    let preset = &atman_runtime::model_registry::PROVIDER_PRESETS[state.selected_provider];
    let title = if preset.needs_api_key {
        "2. Enter API key"
    } else {
        "2. Confirm base URL"
    };
    let help = preset.key_url.unwrap_or(preset.base_url);
    let display = if preset.needs_api_key {
        mask_secret(state.input.buf())
    } else {
        state.input.buf().to_string()
    };
    let lines = vec![
        Line::from(title),
        Line::from(Span::styled(
            format!("Provider: {}", preset.name),
            Style::default().fg(theme.meta_fg.into()),
        )),
        Line::from(Span::styled(
            format!("Get one at: {help}"),
            Style::default().fg(theme.meta_fg.into()),
        )),
        Line::from(""),
        Line::from(Span::styled(
            format!("  {display}"),
            Style::default().fg(theme.accent.into()),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn render_model_step(f: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let theme = crate::theme::theme();
    let preset = &atman_runtime::model_registry::PROVIDER_PRESETS[state.selected_provider];
    let items: Vec<ListItem> = preset
        .models
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let style = if i == state.selected_model {
                Style::default()
                    .fg(theme.accent.into())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {}", m.id), style),
                Span::styled(
                    format!(" — {}", m.description),
                    Style::default().fg(theme.meta_fg.into()),
                ),
            ]))
        })
        .collect();
    let mut list_state = ListState::default().with_selected(Some(state.selected_model));
    f.render_widget(
        Paragraph::new("3. Choose your default model"),
        Rect { height: 1, ..area },
    );
    let list_area = Rect {
        y: area.y + 2,
        height: area.height.saturating_sub(4),
        ..area
    };
    f.render_stateful_widget(List::new(items), list_area, &mut list_state);
    let note_area = Rect {
        y: area.y + area.height.saturating_sub(2),
        height: 2,
        ..area
    };
    f.render_widget(
        Paragraph::new("This will be set as your smart model."),
        note_area,
    );
}

fn mask_secret(s: &str) -> String {
    if s.is_empty() {
        "sk-…".to_string()
    } else if s.len() <= 6 {
        "*".repeat(s.len())
    } else {
        format!("{}{}", &s[..3], "*".repeat(s.len().saturating_sub(3)))
    }
}
