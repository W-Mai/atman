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
    pub selected_action: usize,
    pub selected_model: usize,
    pub error: Option<String>,
}

impl Default for OnboardingState {
    fn default() -> Self {
        Self {
            step: OnboardingStep::ProviderSelect,
            selected_action: 0,
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
        let actions = 2;
        match action {
            KeyAction::Char('q') | KeyAction::Quit => OnboardingEvent::Skipped,
            KeyAction::HistoryUp | KeyAction::Char('k') => {
                self.selected_action = if self.selected_action == 0 {
                    actions - 1
                } else {
                    self.selected_action - 1
                };
                OnboardingEvent::None
            }
            KeyAction::HistoryDown | KeyAction::Char('j') => {
                self.selected_action = (self.selected_action + 1) % actions;
                OnboardingEvent::None
            }
            KeyAction::Submit => {
                if self.selected_action == 0 {
                    OnboardingEvent::OpenProviderManager
                } else {
                    OnboardingEvent::Skipped
                }
            }
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
            Constraint::Length(1),
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

    f.render_widget(Paragraph::new(""), rows[1]);

    match state.step {
        OnboardingStep::ProviderSelect => render_provider_step(f, rows[2], state),
        OnboardingStep::ModelSelect => render_model_step(f, rows[2], state),
    }

    let footer = if let Some(error) = state.error.as_deref() {
        Line::from(Span::styled(error, Style::default().fg(theme.error.into())))
    } else {
        match state.step {
            OnboardingStep::ProviderSelect => Line::from(vec![
                key_span("Enter"),
                help_span(" add provider  "),
                key_span("q"),
                help_span(" skip"),
            ]),
            OnboardingStep::ModelSelect => Line::from(vec![
                key_span("↑↓/j/k"),
                help_span(" navigate  "),
                key_span("Enter"),
                help_span(" finish  "),
                key_span("Esc"),
                help_span(" back"),
            ]),
        }
    };
    f.render_widget(
        Paragraph::new(footer)
            .wrap(Wrap { trim: true })
            .alignment(ratatui::layout::Alignment::Right),
        rows[3],
    );
}

fn render_provider_step(f: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let theme = crate::theme::theme();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);

    let intro = vec![
        Line::from(Span::styled(
            "1. Add a provider",
            Style::default()
                .fg(theme.tinted_fg.into())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "Use the same presets and API key form as Provider Manager.",
            Style::default().fg(theme.meta_fg.into()),
        )),
    ];
    f.render_widget(Paragraph::new(intro), rows[0]);

    let actions = vec![
        ListItem::new(Line::from(Span::styled(
            "Add provider",
            Style::default().fg(theme.tinted_fg.into()),
        )).alignment(ratatui::layout::Alignment::Center)),
        ListItem::new(Line::from(Span::styled(
            "Skip for now",
            Style::default().fg(theme.meta_fg.into()),
        )).alignment(ratatui::layout::Alignment::Center)),
    ];
    let mut list_state = ListState::default().with_selected(Some(state.selected_action));
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border.into()))
        .title(" Actions ");
    f.render_stateful_widget(
        List::new(actions)
            .block(block)
            .highlight_style(selected_button_style()),
        rows[2],
        &mut list_state,
    );

    f.render_widget(
        Paragraph::new(Line::from(vec![
            help_span("Next, choose the model that becomes "),
            Span::styled("smart", Style::default().fg(theme.accent.into())),
            help_span("."),
        ]))
        .alignment(ratatui::layout::Alignment::Center),
        rows[3],
    );
}

fn render_model_step(f: &mut ratatui::Frame, area: Rect, state: &OnboardingState) {
    let theme = crate::theme::theme();
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(3),
        ])
        .split(area);

    let intro = vec![
        Line::from(Span::styled(
            "2. Choose your default model",
            Style::default()
                .fg(theme.tinted_fg.into())
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            "This model will be saved as the smart alias.",
            Style::default().fg(theme.meta_fg.into()),
        )),
    ];
    f.render_widget(Paragraph::new(intro), rows[0]);

    let models = selectable_models();
    let items: Vec<ListItem> = models
        .iter()
        .enumerate()
        .map(|(i, model)| {
            let selected = i == state.selected_model;
            let style = if selected {
                Style::default()
                    .fg(theme.accent.into())
                    .bg(theme.highlight_bg.into())
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(theme.tinted_fg.into())
            };
            let info = atman_runtime::model_registry::model_info(model);
            let prefix = if selected { "›" } else { " " };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {prefix} {model}"), style),
                Span::styled(
                    format!(
                        "  {} ctx",
                        atman_runtime::humanize::format_count(info.context_budget)
                    ),
                    Style::default().fg(theme.meta_fg.into()),
                ),
            ]))
        })
        .collect();
    let selected = if items.is_empty() {
        None
    } else {
        Some(state.selected_model.min(items.len().saturating_sub(1)))
    };
    let mut list_state = ListState::default().with_selected(selected);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme.border.into()))
        .title(" Models ");
    if items.is_empty() {
        f.render_widget(
            Paragraph::new(Line::from(vec![
                help_span("No configured models found. "),
                key_span("Esc"),
                help_span(" back to add a provider."),
            ]))
            .block(block)
            .wrap(Wrap { trim: true }),
            rows[2],
        );
    } else {
        f.render_stateful_widget(
            List::new(items)
                .block(block)
                .highlight_style(selected_button_style()),
            rows[2],
            &mut list_state,
        );
    }

    f.render_widget(
        Paragraph::new(Line::from(vec![
            key_span("Enter"),
            help_span(" saves "),
            Span::styled("smart", Style::default().fg(theme.accent.into())),
            help_span(" for future runs."),
        ]))
        .alignment(ratatui::layout::Alignment::Center),
        rows[3],
    );
}

fn key_span(text: &'static str) -> Span<'static> {
    let theme = crate::theme::theme();
    Span::styled(
        format!(" {text} "),
        Style::default()
            .fg(theme.accent.into())
            .bg(theme.panel_bg.into())
            .add_modifier(Modifier::BOLD),
    )
}

fn help_span(text: &'static str) -> Span<'static> {
    let theme = crate::theme::theme();
    Span::styled(text, Style::default().fg(theme.subtle_fg.into()))
}

fn selected_button_style() -> Style {
    let theme = crate::theme::theme();
    Style::default()
        .fg(theme.accent.into())
        .bg(theme.panel_bg.into())
        .add_modifier(Modifier::BOLD)
}
