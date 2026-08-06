use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap};

use crate::keys::KeyAction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingStep {
    ProviderSelect,
    ModelSelect,
}

pub struct OnboardingState {
    pub step: OnboardingStep,
    pub selected_model: usize,
    pub error: Option<String>,
}

impl Default for OnboardingState {
    fn default() -> Self {
        Self {
            step: OnboardingStep::ProviderSelect,
            selected_model: 0,
            error: None,
        }
    }
}

pub enum OnboardingEvent {
    None,
    OpenProviderManager,
    Completed,
    Skipped,
}

impl OnboardingState {
    pub fn handle_key(&mut self, action: &KeyAction) -> OnboardingEvent {
        self.error = None;
        match self.step {
            OnboardingStep::ProviderSelect => self.handle_provider_key(action),
            OnboardingStep::ModelSelect => self.handle_model_key(action),
        }
    }

    pub fn provider_added(&mut self) {
        self.step = OnboardingStep::ModelSelect;
        self.selected_model = 0;
    }

    fn handle_provider_key(&mut self, action: &KeyAction) -> OnboardingEvent {
        match action {
            KeyAction::Char('q') | KeyAction::Quit => OnboardingEvent::Skipped,
            KeyAction::Submit => OnboardingEvent::OpenProviderManager,
            _ => OnboardingEvent::None,
        }
    }

    fn handle_model_key(&mut self, action: &KeyAction) -> OnboardingEvent {
        let models = selectable_models();
        let len = models.len();
        match action {
            KeyAction::Escape => {
                self.step = OnboardingStep::ProviderSelect;
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
            KeyAction::Submit if len > 0 => match self.write_config(&models) {
                Ok(()) => OnboardingEvent::Completed,
                Err(e) => {
                    self.error = Some(format!("setup failed: {e}"));
                    OnboardingEvent::None
                }
            },
            KeyAction::Submit => {
                self.error = Some("Add a provider first".to_string());
                OnboardingEvent::None
            }
            _ => OnboardingEvent::None,
        }
    }

    fn write_config(&self, models: &[String]) -> anyhow::Result<()> {
        let model = models
            .get(self.selected_model)
            .ok_or_else(|| anyhow::anyhow!("no model selected"))?;
        atman_runtime::model_registry::add_alias_to_config("smart", model)?;
        Ok(())
    }
}

fn selectable_models() -> Vec<String> {
    let mut models: Vec<String> = atman_runtime::model_registry::all_model_entries()
        .into_iter()
        .filter_map(|(name, entry)| {
            if entry.enabled != Some(false) && entry.context_budget.unwrap_or(0) > 0 {
                Some(name)
            } else {
                None
            }
        })
        .collect();
    models.sort();
    models
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
            Constraint::Length(10),
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
        OnboardingStep::ProviderSelect => render_provider_step(f, rows[1]),
        OnboardingStep::ModelSelect => render_model_step(f, rows[1], state),
    }

    let footer = state.error.as_deref().unwrap_or(match state.step {
        OnboardingStep::ProviderSelect => "Enter add provider · q skip",
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

fn render_provider_step(f: &mut ratatui::Frame, area: Rect) {
    let theme = crate::theme::theme();
    let lines = vec![
        Line::from("1. Add a provider"),
        Line::from(""),
        Line::from(Span::styled(
            "Press Enter to open Provider Manager.",
            Style::default().fg(theme.tinted_fg.into()),
        )),
        Line::from(Span::styled(
            "The setup flow uses the same provider presets and form as Manage Providers.",
            Style::default().fg(theme.meta_fg.into()),
        )),
    ];
    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn render_model_step(f: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let theme = crate::theme::theme();
    let models = selectable_models();
    let items: Vec<ListItem> = models
        .iter()
        .enumerate()
        .map(|(i, model)| {
            let style = if i == state.selected_model {
                Style::default()
                    .fg(theme.accent.into())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let info = atman_runtime::model_registry::model_info(model);
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {model}"), style),
                Span::styled(
                    format!(
                        " — {}",
                        atman_runtime::humanize::format_count(info.context_budget)
                    ),
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
    if items.is_empty() {
        f.render_widget(
            Paragraph::new("No configured models found. Press Esc and add a provider first."),
            list_area,
        );
    } else {
        f.render_stateful_widget(List::new(items), list_area, &mut list_state);
    }
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
