use crate::wm::modal::ModalAction;

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::keys::KeyAction;
use crate::settings_form::{Action, render_action, render_record};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum ModelFocus {
    #[default]
    List,
    Add,
    Back,
    Finish,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hit {
    Provider(usize),
    Model(usize),
    Add,
    Back,
    Finish,
}

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
    model_focus: ModelFocus,
    models: Vec<String>,
    model_offset: usize,
    visible_models: usize,
    hits: Vec<(Hit, Rect)>,
    list_rect: Option<Rect>,
    hovered: Option<Hit>,
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
            model_focus: ModelFocus::List,
            models: Vec::new(),
            model_offset: 0,
            visible_models: 0,
            hits: Vec::new(),
            list_rect: None,
            hovered: None,
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
        let event = match self.step {
            OnboardingStep::ProviderSelect => self.handle_provider_key(action),
            OnboardingStep::ModelSelect => self.handle_model_key(action),
        };
        self.clear_geometry();
        event
    }

    fn clear_geometry(&mut self) {
        self.hits.clear();
        self.list_rect = None;
        self.hovered = None;
    }

    pub fn handle_mouse(&mut self, event: &MouseEvent) -> OnboardingEvent {
        if matches!(event.kind, MouseEventKind::Down(MouseButton::Left))
            && self.step == OnboardingStep::ModelSelect
        {
            self.refresh_models();
        }
        let point = (event.column, event.row).into();
        let hit = self
            .hits
            .iter()
            .find_map(|(hit, rect)| rect.contains(point).then_some(*hit));
        match event.kind {
            MouseEventKind::Moved => self.hovered = hit,
            MouseEventKind::Down(MouseButton::Left) => {
                self.hovered = hit;
                match hit {
                    Some(Hit::Provider(index)) => {
                        self.selected_action = index;
                        return self.handle_key(&KeyAction::Submit);
                    }
                    Some(Hit::Model(index)) => {
                        self.selected_model = index;
                        self.model_focus = ModelFocus::List;
                        self.error = None;
                    }
                    Some(Hit::Add) => return self.handle_key(&KeyAction::Char('a')),
                    Some(Hit::Back) => return self.handle_key(&KeyAction::Escape),
                    Some(Hit::Finish) => {
                        self.model_focus = ModelFocus::Finish;
                        return self.handle_key(&KeyAction::Submit);
                    }
                    None => {}
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                if self.list_rect.is_some_and(|rect| rect.contains(point)) =>
            {
                self.model_focus = ModelFocus::List;
                return self.handle_key(&if event.kind == MouseEventKind::ScrollUp {
                    KeyAction::HistoryUp
                } else {
                    KeyAction::HistoryDown
                });
            }
            _ => {}
        }
        OnboardingEvent::None
    }

    fn refresh_models(&mut self) {
        let models = selectable_models();
        if self.models != models {
            let selected = self.models.get(self.selected_model);
            self.selected_model = selected
                .and_then(|selected| models.iter().position(|name| name == selected))
                .unwrap_or(self.selected_model.min(models.len().saturating_sub(1)));
            self.models = models;
            self.clear_geometry();
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
            self.model_focus = ModelFocus::List;
            self.model_offset = 0;
            self.clear_geometry();
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
            KeyAction::HistoryUp | KeyAction::Char('k') | KeyAction::BackTab => {
                self.selected_action = if self.selected_action == 0 {
                    actions - 1
                } else {
                    self.selected_action - 1
                };
                OnboardingEvent::None
            }
            KeyAction::HistoryDown | KeyAction::Char('j') | KeyAction::Tab => {
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
        self.refresh_models();
        let len = self.models.len();
        match action {
            KeyAction::Escape => {
                self.step = OnboardingStep::ProviderSelect;
                self.pending_provider_name = None;
                OnboardingEvent::None
            }
            KeyAction::Char('a') => OnboardingEvent::OpenModelManager,
            KeyAction::Tab | KeyAction::BackTab => {
                use ModelFocus::{Add, Back, Finish, List};
                self.model_focus = match (self.model_focus, action) {
                    (List, KeyAction::Tab) | (Back, KeyAction::BackTab) => Add,
                    (Add, KeyAction::Tab) | (Finish, KeyAction::BackTab) => Back,
                    (Back, KeyAction::Tab) | (List, KeyAction::BackTab) => Finish,
                    _ => List,
                };
                OnboardingEvent::None
            }
            KeyAction::HistoryUp | KeyAction::Char('k') if len > 0 => {
                self.model_focus = ModelFocus::List;
                self.selected_model = self.selected_model.checked_sub(1).unwrap_or(len - 1);
                OnboardingEvent::None
            }
            KeyAction::HistoryDown | KeyAction::Char('j') if len > 0 => {
                self.model_focus = ModelFocus::List;
                self.selected_model = (self.selected_model + 1) % len;
                OnboardingEvent::None
            }
            KeyAction::PageUp | KeyAction::PageDown if len > 0 => {
                self.model_focus = ModelFocus::List;
                self.selected_model = if *action == KeyAction::PageUp {
                    self.selected_model
                        .saturating_sub(self.visible_models.max(1))
                } else {
                    self.selected_model
                        .saturating_add(self.visible_models.max(1))
                        .min(len - 1)
                };
                OnboardingEvent::None
            }
            KeyAction::Home | KeyAction::CursorHome if len > 0 => {
                self.model_focus = ModelFocus::List;
                self.selected_model = 0;
                OnboardingEvent::None
            }
            KeyAction::End | KeyAction::CursorEnd if len > 0 => {
                self.model_focus = ModelFocus::List;
                self.selected_model = len - 1;
                OnboardingEvent::None
            }
            KeyAction::Submit if self.model_focus == ModelFocus::Add => {
                OnboardingEvent::OpenModelManager
            }
            KeyAction::Submit if self.model_focus == ModelFocus::Back => {
                self.handle_model_key(&KeyAction::Escape)
            }
            KeyAction::Submit if len > 0 => match self.write_config(&self.models) {
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
        atman_runtime::config_hub::ConfigHub::global()?.bind_default_model(model)?;
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
        self.render(f, area);
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

impl OnboardingState {
    fn render(&mut self, f: &mut ratatui::Frame, area: Rect) {
        let theme = crate::theme::theme();
        self.hits.clear();
        self.list_rect = None;
        if self.step == OnboardingStep::ModelSelect {
            self.refresh_models();
        }
        let width = area.width.saturating_sub(4).min(76);
        let top = u16::from(area.height >= 16);
        let mut content = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + top,
            width,
            area.height.saturating_sub(top * 2),
        );
        if content.width == 0 || content.height == 0 {
            return;
        }
        if content.width >= 70 && content.height >= 27 {
            let height = crate::output::STARTUP_BANNER.len() as u16;
            let banner = crate::output::STARTUP_BANNER
                .iter()
                .map(|row| Line::from(*row))
                .collect::<Vec<_>>();
            f.render_widget(
                Paragraph::new(banner)
                    .alignment(Alignment::Center)
                    .style(Style::default().fg(theme.accent.into())),
                Rect::new(content.x, content.y, content.width, height),
            );
            content.y += height + 1;
            content.height -= height + 1;
        }
        let intro_height = if content.height >= 13 { 3 } else { 1 }.min(content.height);
        let help_height = u16::from(content.height >= 9);
        let error_height = if let Some(error) = &self.error {
            crate::width::word_wrap(error, content.width as usize)
                .len()
                .min(3)
                .min(
                    content
                        .height
                        .saturating_sub(intro_height + help_height + 6)
                        as usize,
                ) as u16
        } else {
            0
        };
        let intro = Rect::new(content.x, content.y, content.width, intro_height);
        let body = Rect::new(
            content.x,
            intro.bottom(),
            content.width,
            content
                .height
                .saturating_sub(intro_height + error_height + help_height),
        );
        let error_rect = Rect::new(content.x, body.bottom(), content.width, error_height);
        let help_rect = Rect::new(content.x, error_rect.bottom(), content.width, help_height);
        let (title, description) = match self.step {
            OnboardingStep::ProviderSelect => (
                "1 / 2  Connect a provider",
                "Add API credentials or sign in.",
            ),
            OnboardingStep::ModelSelect => (
                "2 / 2  Choose your default model",
                "Save this model as the smart alias.",
            ),
        };
        let mut intro_lines = vec![Line::from(Span::styled(
            crate::width::truncate(title, intro.width as usize),
            Style::default()
                .fg(theme.tinted_fg.into())
                .add_modifier(Modifier::BOLD),
        ))];
        if intro_height > 1 {
            intro_lines.push(Line::from(Span::styled(
                crate::width::truncate(description, intro.width as usize),
                Style::default().fg(theme.subtle_fg.into()),
            )));
        }
        f.render_widget(Paragraph::new(intro_lines), intro);
        match self.step {
            OnboardingStep::ProviderSelect => self.render_provider_step(f, body, &theme),
            OnboardingStep::ModelSelect => self.render_model_step(f, body, &theme),
        }
        if let Some(error) = &self.error {
            f.render_widget(
                Paragraph::new(error.as_str())
                    .style(Style::default().fg(theme.error.into()))
                    .wrap(Wrap { trim: true }),
                error_rect,
            );
        }
        let help = match self.step {
            OnboardingStep::ProviderSelect if width >= 46 => {
                "↑↓ / Tab: choose · Enter: select · q: skip"
            }
            OnboardingStep::ProviderSelect => "↑↓ / Tab: choose · Enter: select",
            OnboardingStep::ModelSelect if width >= 56 => {
                "↑↓: models · PgUp/PgDn: page · Tab: actions · Enter: select"
            }
            OnboardingStep::ModelSelect => "↑↓: models · Tab: actions",
        };
        f.render_widget(
            Paragraph::new(crate::width::truncate(help, help_rect.width as usize))
                .style(Style::default().fg(theme.subtle_fg.into())),
            help_rect,
        );
    }

    fn render_provider_step(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        theme: &crate::theme::Theme,
    ) {
        let height = 3.min(area.height / 2);
        let gap = u16::from(area.height >= 7);
        for (index, (label, shortcut)) in [("Add provider", "Enter"), ("Skip for now", "q")]
            .into_iter()
            .enumerate()
        {
            let rect = Rect::new(
                area.x,
                area.y + index as u16 * (height + gap),
                area.width,
                height,
            );
            let hit = Hit::Provider(index);
            render_action(
                f,
                rect,
                &Action {
                    label,
                    shortcut: if index == self.selected_action || index == 1 {
                        shortcut
                    } else {
                        ""
                    },
                    focused: index == self.selected_action,
                    hovered: self.hovered == Some(hit),
                    ..Default::default()
                },
                theme,
            );
            self.hits.push((hit, rect));
        }
        let hint_y = area.y + height * 2 + gap + 1;
        if hint_y < area.bottom() {
            f.render_widget(
                Paragraph::new("Provider settings can be changed at any time.")
                    .style(Style::default().fg(theme.subtle_fg.into()))
                    .wrap(Wrap { trim: true }),
                Rect::new(
                    area.x + 2.min(area.width),
                    hint_y,
                    area.width.saturating_sub(4),
                    area.bottom() - hint_y,
                ),
            );
        }
    }

    fn render_model_step(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        theme: &crate::theme::Theme,
    ) {
        let action_height = 3.min(area.height);
        let gap = u16::from(area.height >= 8);
        let list_height = area.height.saturating_sub(action_height + gap);
        let show_position = list_height >= 4;
        let list = Rect::new(
            area.x,
            area.y,
            area.width,
            list_height.saturating_sub(u16::from(show_position)),
        );
        self.list_rect = Some(list);
        self.visible_models = (list.height / 3) as usize;
        let count = self.models.len();
        self.selected_model = self.selected_model.min(count.saturating_sub(1));
        self.model_offset = self
            .model_offset
            .min(count.saturating_sub(self.visible_models));
        if self.selected_model < self.model_offset {
            self.model_offset = self.selected_model;
        } else if self.selected_model >= self.model_offset + self.visible_models {
            self.model_offset = self
                .selected_model
                .saturating_sub(self.visible_models.saturating_sub(1));
        }
        if count == 0 {
            let message = self.pending_provider_name.as_deref().map_or_else(
                || "No configured models. Use Add to configure one.".to_owned(),
                |name| format!("Waiting for models from {name}…"),
            );
            f.render_widget(
                Paragraph::new(message)
                    .style(Style::default().fg(theme.subtle_fg.into()))
                    .wrap(Wrap { trim: true }),
                Rect::new(
                    list.x + 2.min(list.width),
                    list.y + u16::from(list.height > 2),
                    list.width.saturating_sub(4),
                    list.height.saturating_sub(u16::from(list.height > 2)),
                ),
            );
        }
        for (slot, index) in (self.model_offset..count)
            .take(self.visible_models)
            .enumerate()
        {
            let rect = Rect::new(list.x, list.y + slot as u16 * 3, list.width, 3);
            let hit = Hit::Model(index);
            let bg = render_record(
                f,
                rect,
                self.selected_model == index,
                self.hovered == Some(hit),
                theme,
            );
            let model = &self.models[index];
            let text_width = rect.width.saturating_sub(4);
            let info = atman_runtime::model_registry::model_info(model);
            let metadata = format!(
                "  {} ctx",
                atman_runtime::humanize::format_count(info.context_budget)
            );
            let metadata_width = crate::width::width(&metadata);
            let show_metadata = text_width as usize >= metadata_width + 24;
            let name_width = (text_width as usize).saturating_sub(if show_metadata {
                metadata_width
            } else {
                0
            });
            let mut spans = vec![Span::styled(
                crate::width::truncate(model, name_width),
                Style::default().fg(theme.tinted_fg.into()).bg(bg),
            )];
            if show_metadata {
                spans.push(Span::styled(
                    metadata,
                    Style::default().fg(theme.subtle_fg.into()).bg(bg),
                ));
            }
            f.render_widget(
                Paragraph::new(Line::from(spans)),
                Rect::new(rect.x + 2.min(rect.width), rect.y + 1, text_width, 1),
            );
            self.hits.push((hit, rect));
        }
        if show_position && count > 0 {
            let position = format!(
                "{}–{} / {} models",
                self.model_offset + 1,
                (self.model_offset + self.visible_models).min(count),
                count
            );
            f.render_widget(
                Paragraph::new(position)
                    .alignment(Alignment::Right)
                    .style(Style::default().fg(theme.subtle_fg.into())),
                Rect::new(area.x, list.bottom(), area.width, 1),
            );
        }
        let action_y = area.bottom().saturating_sub(action_height);
        let action_gap = u16::from(area.width >= 5);
        let button_width = area.width.saturating_sub(action_gap * 2) / 3;
        let compact = button_width < 16;
        let narrow = area.width < 31;
        let add_width = if compact && !narrow {
            button_width.saturating_sub(1)
        } else {
            button_width
        };
        let back_width = if compact && !narrow {
            button_width + 1
        } else {
            button_width
        };
        let widths = [
            add_width,
            back_width,
            area.width
                .saturating_sub(add_width + back_width + action_gap * 2),
        ];
        let mut action_x = area.x;
        for (index, (hit, focus, label, shortcut)) in [
            (
                Hit::Add,
                ModelFocus::Add,
                if narrow {
                    "+"
                } else if compact {
                    "Add"
                } else {
                    "Add model"
                },
                "a",
            ),
            (
                Hit::Back,
                ModelFocus::Back,
                if narrow { "←" } else { "Back" },
                "Esc",
            ),
            (
                Hit::Finish,
                ModelFocus::Finish,
                if narrow {
                    "✓"
                } else if compact {
                    "Save"
                } else {
                    "Finish"
                },
                if compact { "↵" } else { "Enter" },
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let rect = Rect::new(action_x, action_y, widths[index], action_height);
            action_x += widths[index] + action_gap;
            render_action(
                f,
                rect,
                &Action {
                    label,
                    shortcut,
                    focused: self.model_focus == focus,
                    hovered: self.hovered == Some(hit),
                    disabled: hit == Hit::Finish && count == 0,
                },
                theme,
            );
            self.hits.push((hit, rect));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::KeyAction;

    struct ModelConfigReset;

    fn render(state: &mut OnboardingState, width: u16, height: u16) {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| state.render(f, f.area())).unwrap();
    }

    fn click(rect: Rect, bottom: bool) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + rect.width / 2,
            row: if bottom { rect.bottom() - 1 } else { rect.y },
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    fn set_models(count: usize) {
        let mut config = atman_runtime::model_registry::ModelConfig::default();
        config.providers.insert(
            "example".into(),
            atman_runtime::model_registry::ProviderEntry {
                name: "example".into(),
                kind: "openai".into(),
                api_key: Some("test-key".into()),
                ..Default::default()
            },
        );
        for index in 0..count {
            let model = format!("model-{index:02}");
            config.models.insert(
                model.clone(),
                atman_runtime::model_registry::ModelEntry {
                    model,
                    provider: Some("example".into()),
                    ..Default::default()
                },
            );
        }
        atman_runtime::model_registry::set_model_config(config);
    }

    #[test]
    fn padded_actions_share_keyboard_and_full_record_mouse_targets() {
        let mut state = OnboardingState::default();
        render(&mut state, 40, 14);
        let add = state
            .hits
            .iter()
            .find(|(hit, _)| *hit == Hit::Provider(0))
            .unwrap()
            .1;
        assert_eq!(add.height, 3);
        assert_eq!(
            state.handle_mouse(&click(add, true)),
            OnboardingEvent::OpenProviderManager
        );
        assert!(state.hits.is_empty());
        assert_eq!(state.handle_mouse(&click(add, true)), OnboardingEvent::None);
        state.handle_key(&KeyAction::Tab);
        assert_eq!(
            state.handle_key(&KeyAction::Submit),
            OnboardingEvent::Skipped
        );
        render(&mut state, 40, 14);
        let skip = state
            .hits
            .iter()
            .find(|(hit, _)| *hit == Hit::Provider(1))
            .unwrap()
            .1;
        assert_eq!(
            state.handle_mouse(&click(skip, false)),
            OnboardingEvent::Skipped
        );
    }

    #[test]
    fn model_selection_stays_visible_and_hits_follow_the_rendered_page() {
        let _lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap();
        let _reset = ModelConfigReset;
        set_models(20);
        let mut state = OnboardingState {
            step: OnboardingStep::ModelSelect,
            ..Default::default()
        };
        for width in [24, 40, 80] {
            for height in [12, 18, 34] {
                state.selected_model = 0;
                render(&mut state, width, height);
                state.handle_key(&KeyAction::End);
                render(&mut state, width, height);
                assert!(state.hits.iter().any(|(hit, _)| *hit == Hit::Model(19)));
                for (_, rect) in &state.hits {
                    assert_eq!(rect.height, 3);
                    assert!(rect.right() <= width && rect.bottom() <= height);
                }
                let (Hit::Model(index), rect) = state
                    .hits
                    .iter()
                    .find(|(hit, _)| matches!(hit, Hit::Model(_)))
                    .copied()
                    .unwrap()
                else {
                    unreachable!()
                };
                assert!(index > 0);
                assert_eq!(
                    state.handle_mouse(&click(rect, true)),
                    OnboardingEvent::None
                );
                assert_eq!(state.selected_model, index);
            }
        }
        state.handle_key(&KeyAction::Tab);
        assert_eq!(
            state.handle_key(&KeyAction::Submit),
            OnboardingEvent::OpenModelManager
        );
        state.handle_key(&KeyAction::Tab);
        assert_eq!(state.handle_key(&KeyAction::Submit), OnboardingEvent::None);
        assert_eq!(state.step, OnboardingStep::ProviderSelect);
        assert!(state.hits.is_empty());
    }

    #[test]
    fn catalog_refresh_keeps_selected_model_identity_and_invalidates_old_hits() {
        let _lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap();
        let _reset = ModelConfigReset;
        set_models(4);
        let mut state = OnboardingState {
            step: OnboardingStep::ModelSelect,
            selected_model: 2,
            ..Default::default()
        };
        render(&mut state, 40, 18);
        let old_rect = state
            .hits
            .iter()
            .find(|(hit, _)| matches!(hit, Hit::Model(_)))
            .unwrap()
            .1;
        set_models(7);
        assert_eq!(
            state.handle_mouse(&click(old_rect, false)),
            OnboardingEvent::None
        );
        assert_eq!(state.models[state.selected_model], "model-02");
        assert!(state.hits.is_empty());
    }

    impl Drop for ModelConfigReset {
        fn drop(&mut self) {
            atman_runtime::model_registry::set_model_config(
                atman_runtime::model_registry::ModelConfig::default(),
            );
        }
    }

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
        let _lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap();
        let _reset = ModelConfigReset;
        atman_runtime::model_registry::set_model_config(
            atman_runtime::model_registry::ModelConfig::default(),
        );
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
        let _lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap();
        let _reset = ModelConfigReset;
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
