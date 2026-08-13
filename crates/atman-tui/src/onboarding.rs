use crate::wm::modal::ModalAction;

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{List, ListItem, ListState, Paragraph, Wrap};

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
    pub pending_provider_name: Option<String>,
    provider_was_added: bool,
    pending_model_select: bool,
}

impl Default for OnboardingState {
    fn default() -> Self {
        Self {
            step: OnboardingStep::ProviderSelect,
            selected_action: 0,
            selected_model: 0,
            error: None,
            pending_provider_name: None,
            provider_was_added: false,
            pending_model_select: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnboardingEvent {
    None,
    OpenProviderManager,
    OpenModelManager,
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

    pub fn provider_added(&mut self, provider_name: Option<&str>) {
        self.provider_was_added = true;
        self.pending_provider_name = provider_name.map(String::from);
        self.pending_model_select = true;
    }

    pub fn try_advance_to_model_select(&mut self) {
        if self.pending_model_select {
            self.pending_model_select = false;
            self.pending_provider_name = None;
            self.step = OnboardingStep::ModelSelect;
            self.selected_model = 0;
        }
    }

    pub fn check_provider_manager_closed(&mut self) {
        if self.step == OnboardingStep::ProviderSelect && !self.provider_was_added {
            self.error = Some(
                "No provider added — pick Add provider to try again, or press q to skip"
                    .to_string(),
            );
        }
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
                self.pending_provider_name = None;
                OnboardingEvent::None
            }
            KeyAction::Char('a') => OnboardingEvent::OpenModelManager,
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
                self.error = Some("No models available. Press 'a' to add a model.".to_string());
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
    let providers = atman_runtime::model_registry::all_provider_entries();
    let mut models: Vec<String> = atman_runtime::model_registry::all_model_entries()
        .into_iter()
        .filter_map(|(name, entry)| {
            if entry.enabled == Some(false) {
                return None;
            }
            let provider_name = entry.provider.as_ref()?;
            let provider = providers
                .iter()
                .find(|(n, _)| n == provider_name)
                .map(|(_, e)| e)?;
            let has_key = provider
                .api_key_env
                .as_deref()
                .and_then(|env| std::env::var(env).ok().filter(|v| !v.trim().is_empty()))
                .or_else(|| provider.api_key.clone().filter(|k| !k.is_empty()))
                .or_else(|| match provider.kind.as_str() {
                    "openai" | "openai-compat" => std::env::var("OPENAI_API_KEY")
                        .ok()
                        .filter(|v| !v.is_empty()),
                    "anthropic" => std::env::var("ANTHROPIC_API_KEY")
                        .ok()
                        .filter(|v| !v.is_empty()),
                    _ => None,
                })
                .is_some();
            if has_key { Some(name) } else { None }
        })
        .collect();
    models.sort();
    models
}

impl crate::wm::modal::ModalOverlay for OnboardingState {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        _app: &crate::app::AppState,
        _t: &crate::theme::Theme,
    ) {
        let theme = crate::theme::theme();
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(10),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(2),
            ])
            .split(area);

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

        match self.step {
            OnboardingStep::ProviderSelect => render_provider_step(f, rows[2], self),
            OnboardingStep::ModelSelect => render_model_step(f, rows[2], self),
        }

        let footer = if let Some(error) = self.error.as_deref() {
            Line::from(Span::styled(error, Style::default().fg(theme.error.into())))
        } else {
            match self.step {
                OnboardingStep::ProviderSelect => Line::from(vec![
                    key_span("Enter"),
                    help_span(" add provider  "),
                    key_span("q"),
                    help_span(" skip"),
                ]),
                OnboardingStep::ModelSelect => Line::from(vec![
                    key_span("↑↓/j/k"),
                    help_span(" navigate  "),
                    key_span("a"),
                    help_span(" add model  "),
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

    fn handle_key(
        &mut self,
        action: &crate::keys::KeyAction,
        _app: &mut crate::app::AppState,
        _tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction> {
        self.handle_key(action);
        Some(ModalAction::Consumed)
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        None
    }

    fn title(&self) -> Line<'static> {
        Line::from("Welcome to atman")
    }

    fn icon(&self) -> &str {
        "✦"
    }

    fn accent(&self, t: &crate::theme::Theme) -> ratatui::style::Color {
        t.accent.into()
    }
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
        ListItem::new(
            Line::from(Span::styled(
                "Add provider",
                Style::default().fg(theme.tinted_fg.into()),
            ))
            .alignment(ratatui::layout::Alignment::Center),
        ),
        ListItem::new(
            Line::from(Span::styled(
                "Skip for now",
                Style::default().fg(theme.meta_fg.into()),
            ))
            .alignment(ratatui::layout::Alignment::Center),
        ),
    ];
    let mut list_state = ListState::default().with_selected(Some(state.selected_action));
    crate::wm::shell::render_section_header(f, rows[2], Line::from("Actions"), &theme);
    let list_inner = Rect {
        x: rows[2].x,
        y: rows[2].y + 2,
        width: rows[2].width,
        height: rows[2].height.saturating_sub(2).saturating_sub(1),
    };
    f.render_stateful_widget(
        List::new(actions).highlight_style(selected_button_style()),
        list_inner,
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
    crate::wm::shell::render_section_header(f, rows[2], Line::from("Models"), &theme);
    let list_inner = Rect {
        x: rows[2].x,
        y: rows[2].y + 2,
        width: rows[2].width,
        height: rows[2].height.saturating_sub(2).saturating_sub(1),
    };
    if items.is_empty() {
        let text = if let Some(name) = state.pending_provider_name.as_deref() {
            Line::from(vec![
                help_span("Waiting for models from "),
                Span::styled(name, Style::default().fg(theme.accent.into())),
                help_span("… "),
                key_span("Esc"),
                help_span(" back."),
            ])
        } else {
            Line::from(vec![
                help_span("No configured models found. "),
                key_span("Esc"),
                help_span(" back to add a provider."),
            ])
        };
        f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }), list_inner);
    } else {
        f.render_stateful_widget(
            List::new(items).highlight_style(selected_button_style()),
            list_inner,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KeyAction;

    #[test]
    fn provider_select_skip() {
        let mut state = OnboardingState::default();
        let event = state.handle_key(&KeyAction::Quit);
        assert_eq!(event, OnboardingEvent::Skipped);
    }

    #[test]
    fn provider_select_add() {
        let mut state = OnboardingState::default();
        let event = state.handle_key(&KeyAction::Submit);
        assert_eq!(event, OnboardingEvent::OpenProviderManager);
    }

    #[test]
    fn model_select_esc_back() {
        let mut state = OnboardingState {
            step: OnboardingStep::ModelSelect,
            ..Default::default()
        };
        let event = state.handle_key(&KeyAction::Escape);
        assert_eq!(event, OnboardingEvent::None);
        assert_eq!(state.step, OnboardingStep::ProviderSelect);
    }

    #[test]
    fn model_select_empty_list_enter() {
        let mut state = OnboardingState {
            step: OnboardingStep::ModelSelect,
            ..Default::default()
        };
        let event = state.handle_key(&KeyAction::Submit);
        assert_ne!(event, OnboardingEvent::Completed);
        assert!(state.error.is_some());
    }

    #[test]
    fn provider_added_sets_pending() {
        let mut state = OnboardingState::default();
        state.provider_added(Some("example"));
        assert!(state.pending_model_select);
        assert_eq!(state.step, OnboardingStep::ProviderSelect);
    }

    #[test]
    fn try_advance_to_model_select() {
        let mut cfg = atman_runtime::model_registry::ModelConfig::default();
        cfg.providers.insert(
            "test-provider".into(),
            atman_runtime::model_registry::ProviderEntry {
                name: "test-provider".into(),
                kind: "openai".into(),
                api_key: Some("test-key".into()),
                ..Default::default()
            },
        );
        cfg.models.insert(
            "example-model".into(),
            atman_runtime::model_registry::ModelEntry {
                model: "example-model".into(),
                enabled: Some(true),
                provider: Some("test-provider".into()),
                ..Default::default()
            },
        );
        atman_runtime::model_registry::set_model_config(cfg);

        let mut state = OnboardingState {
            pending_model_select: true,
            ..Default::default()
        };
        state.try_advance_to_model_select();
        assert_eq!(state.step, OnboardingStep::ModelSelect);
    }
}
