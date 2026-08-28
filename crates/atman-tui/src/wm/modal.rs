use ratatui::Frame;
use ratatui::layout::Rect;
use tokio::sync::mpsc;

use super::component::HitRegion;
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModalKind {
    Form,
    CompactReview,
    SessionSwitcher,
    HistorySearch,
    ProviderManager,
    AliasManager,
    ModelPicker,
    ModelManager,
    Onboarding,
    Palette,
    ThemePicker,
    TrustModePicker,
}

#[derive(Default)]
pub struct ModalManager {
    pub palette: crate::palette::CommandPalette,
    pub form_modal: crate::form_modal::FormModal,
    pub provider_manager: crate::provider_manager::ProviderManager,
    pub alias_manager: crate::alias_manager::AliasManager,
    pub model_picker: crate::model_picker::ModelPicker,
    pub model_manager: crate::model_manager::ModelManager,
    pub session_switcher: crate::session_switcher::SessionSwitcher,
    pub history_search: crate::history_search_modal::HistorySearchModal,
    pub onboarding: crate::onboarding::OnboardingState,
    pub onboarding_open: bool,
    pub compact_review: Option<crate::compact_review_modal::CompactReviewModal>,
    pub theme_picker_open: bool,
    pub trust_mode_picker_open: bool,
    pub trust_draft: Option<atman_runtime::trust::TrustConfig>,
}

impl ModalManager {
    pub fn cursor_visible(&self, kind: ModalKind) -> bool {
        match kind {
            ModalKind::ProviderManager => self.provider_manager.in_form,
            ModalKind::ModelManager => self.model_manager.has_text_focus(),
            ModalKind::Palette
            | ModalKind::HistorySearch
            | ModalKind::Form
            | ModalKind::AliasManager => true,
            _ => false,
        }
    }

    pub fn any_open(&self) -> bool {
        self.palette.open
            || self.form_modal.open
            || self.provider_manager.open
            || self.alias_manager.open
            || self.model_picker.open
            || self.model_manager.open
            || self.session_switcher.open
            || self.history_search.open
            || self.onboarding_open
            || self.compact_review.is_some()
            || self.theme_picker_open
            || self.trust_mode_picker_open
    }

    pub fn open_kinds(&self) -> Vec<ModalKind> {
        let mut kinds = Vec::new();
        if self.form_modal.open {
            kinds.push(ModalKind::Form);
        }
        if self.compact_review.is_some() {
            kinds.push(ModalKind::CompactReview);
        }
        if self.session_switcher.open {
            kinds.push(ModalKind::SessionSwitcher);
        }
        if self.history_search.open {
            kinds.push(ModalKind::HistorySearch);
        }
        if self.provider_manager.open {
            kinds.push(ModalKind::ProviderManager);
        }
        if self.alias_manager.open {
            kinds.push(ModalKind::AliasManager);
        }
        if self.model_picker.open {
            kinds.push(ModalKind::ModelPicker);
        }
        if self.model_manager.open {
            kinds.push(ModalKind::ModelManager);
        }
        if self.onboarding_open {
            kinds.push(ModalKind::Onboarding);
        }
        if self.palette.open {
            kinds.push(ModalKind::Palette);
        }
        if self.theme_picker_open {
            kinds.push(ModalKind::ThemePicker);
        }
        if self.trust_mode_picker_open {
            kinds.push(ModalKind::TrustModePicker);
        }
        kinds
    }
}

/// Result of a modal hit-test.
#[derive(Debug, Clone)]
pub enum HitTestResult {
    /// Click is inside the modal. Returns hit regions for dispatch.
    Inside(Vec<HitRegion>),
    /// Click is outside the modal.
    Outside,
}

/// What happens when the user clicks outside the modal's rect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutsideClickPolicy {
    /// Swallow the click — it doesn't reach lower layers, but the modal
    /// stays open. This is the default.
    Consume,
    /// Close the modal on outside click.
    Close,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModalEntry {
    pub kind: ModalKind,
    pub pre_modal_focus: Option<crate::wm::WindowId>,
}

/// Side effect a modal wants to carry out once it closes (or as it consumes a
/// key event). `Some` means the key was consumed; `None` means it fell through.
#[derive(Debug, Clone)]
pub enum ModalAction {
    Consumed,
    Dispatched(crate::palette::PaletteEntryId),
    OpenModelManager(String),
    OpenAliasForModel(String),
}

pub trait ModalOverlay {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: ratatui::layout::Rect,
        app: &crate::app::AppState,
        t: &crate::theme::Theme,
    );
    fn handle_key(
        &mut self,
        action: &crate::keys::KeyAction,
        app: &mut crate::app::AppState,
        tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> Option<ModalAction>;
    fn cursor_position(&self) -> Option<(u16, u16)>;
    fn title(&self) -> ratatui::text::Line<'static>;
    fn icon(&self) -> &str;
    fn accent(&self, t: &crate::theme::Theme) -> ratatui::style::Color;
    fn handle_paste(&mut self, _text: &str) {}
}

// ── ModalManager dispatch methods ──

impl ModalManager {
    pub fn dispatch_paste(
        &mut self,
        text: &str,
        _app: &mut crate::app::AppState,
        _tx: Option<&mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        let kinds = self.open_kinds();
        for kind in kinds.iter().rev() {
            let handled = match kind {
                ModalKind::ProviderManager => {
                    if self.provider_manager.in_form {
                        self.provider_manager.handle_paste(text);
                        true
                    } else {
                        false
                    }
                }
                ModalKind::ModelManager => {
                    self.model_manager.handle_paste(text);
                    true
                }
                ModalKind::AliasManager => {
                    self.alias_manager.handle_paste(text);
                    true
                }
                ModalKind::Form => {
                    self.form_modal.handle_paste(text);
                    true
                }
                _ => false,
            };
            if handled {
                break;
            }
        }
    }

    /// Render the topmost modal's content inside the shell-provided area.
    pub fn render_top(
        &mut self,
        kind: ModalKind,
        f: &mut Frame,
        area: Rect,
        app: &mut crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        match kind {
            ModalKind::Palette => self.palette.render_content(f, area, app, t),
            ModalKind::Form => self.form_modal.render_content(f, area, app, t),
            ModalKind::CompactReview => {
                if let Some(m) = self.compact_review.as_mut() {
                    m.render_content(f, area, app, t);
                }
            }
            ModalKind::SessionSwitcher => self.session_switcher.render_content(f, area, app, t),
            ModalKind::HistorySearch => self.history_search.render_content(f, area, app, t),
            ModalKind::ProviderManager => self.provider_manager.render_content(f, area, app, t),
            ModalKind::AliasManager => self.alias_manager.render_content(f, area, app, t),
            ModalKind::ModelPicker => self.model_picker.render_content(f, area, app, t),
            ModalKind::ModelManager => self.model_manager.render_content(f, area, app, t),
            ModalKind::Onboarding => self.onboarding.render_content(f, area, app, t),
            ModalKind::ThemePicker => self.render_theme_picker_content(f, area, app, t),
            ModalKind::TrustModePicker => self.render_trust_mode_picker_content(f, area, app, t),
        }
    }

    /// Handle a key for the topmost modal. Returns (consumed, carried action).
    /// `consumed` is true if the key was handled and should not fall through.
    /// `carried` is the modal's side-effect (e.g. a palette entry to dispatch).
    pub fn handle_key_top(
        &mut self,
        kind: ModalKind,
        action: &crate::keys::KeyAction,
        app: &mut crate::app::AppState,
        tx: Option<&mpsc::UnboundedSender<crate::TuiControl>>,
    ) -> (bool, Option<ModalAction>) {
        match kind {
            ModalKind::ModelPicker => {
                if self.model_picker.open {
                    self.model_picker.handle_key(action);
                    if let Some(model) = self.model_picker.picked.take() {
                        if let Some(tx) = tx {
                            let _ = tx.send(crate::TuiControl::SwitchModel {
                                model: model.clone(),
                            });
                        }
                        app.context.model = model.clone();
                        app.push_toast(
                            format!("model switched to {model}"),
                            crate::app::NoteLevel::Success,
                            std::time::Duration::from_secs(3),
                            crate::app::ToastPosition::TopRight,
                        );
                    }
                }
                (true, None)
            }
            ModalKind::ModelManager => {
                if self.model_manager.open {
                    let mm_action = crate::wm::modal::ModalOverlay::handle_key(
                        &mut self.model_manager,
                        action,
                        app,
                        tx,
                    );
                    if let Some(ModalAction::OpenAliasForModel(model)) = mm_action {
                        self.alias_manager.open_form_with_model(&model);
                    }
                    (true, None)
                } else {
                    (false, None)
                }
            }
            ModalKind::Onboarding => {
                if self.onboarding_open {
                    match self.onboarding.handle_key(action) {
                        crate::onboarding::OnboardingEvent::None => {}
                        crate::onboarding::OnboardingEvent::OpenProviderManager => {
                            self.provider_manager.open_add();
                        }
                        crate::onboarding::OnboardingEvent::OpenModelManager => {
                            self.model_manager.open();
                        }
                        crate::onboarding::OnboardingEvent::Completed => {
                            self.onboarding_open = false;
                            app.onboarding_skipped = true;
                            app.hints_dismissed = false;
                            app.save_ui_state();
                            app.push_toast(
                                "Setup complete — smart model configured".to_string(),
                                crate::app::NoteLevel::Success,
                                std::time::Duration::from_secs(4),
                                crate::app::ToastPosition::TopRight,
                            );
                        }
                        crate::onboarding::OnboardingEvent::Skipped => {
                            self.onboarding_open = false;
                            app.onboarding_skipped = true;
                            app.save_ui_state();
                            app.push_toast(
                                "You can configure atman in ~/.config/atman/config.toml"
                                    .to_string(),
                                crate::app::NoteLevel::Warn,
                                std::time::Duration::from_secs(5),
                                crate::app::ToastPosition::TopRight,
                            );
                        }
                    }
                }
                (true, None)
            }
            ModalKind::ProviderManager => {
                if self.provider_manager.open {
                    let pm_action = self.provider_manager.handle_key(action, tx);
                    if let Some(ModalAction::OpenModelManager(name)) = pm_action {
                        self.model_manager.open_with_provider(&name);
                    }
                    if self.provider_manager.add_just_completed {
                        self.provider_manager.add_just_completed = false;
                        if self.onboarding_open {
                            let name = self.provider_manager.last_added_name.take();
                            self.onboarding.provider_added(name.as_deref());
                        }
                    }
                    if self.provider_manager.refresh_just_triggered {
                        self.provider_manager.refresh_just_triggered = false;
                        app.push_toast(
                            "refreshing models…",
                            crate::app::NoteLevel::Info,
                            std::time::Duration::from_secs(2),
                            crate::app::ToastPosition::TopRight,
                        );
                    }
                    if self.provider_manager.test_just_triggered {
                        self.provider_manager.test_just_triggered = false;
                        app.push_toast(
                            "testing endpoint…",
                            crate::app::NoteLevel::Info,
                            std::time::Duration::from_secs(5),
                            crate::app::ToastPosition::TopRight,
                        );
                    }
                    if self.onboarding_open && !self.provider_manager.open {
                        self.onboarding.check_provider_manager_closed();
                    }
                }
                (true, None)
            }
            ModalKind::AliasManager => {
                if self.alias_manager.open {
                    self.alias_manager.handle_key(action, tx);
                }
                (true, None)
            }
            ModalKind::ThemePicker => {
                if self.theme_picker_open {
                    self.handle_theme_picker_key(action, app);
                }
                (true, None)
            }
            ModalKind::TrustModePicker => {
                if self.trust_mode_picker_open {
                    self.handle_trust_mode_picker_key(action, app, tx);
                }
                (true, None)
            }
            ModalKind::Palette => {
                if self.palette.open {
                    let result = self.palette.handle_key(action, app, tx);
                    let consumed = result.is_some();
                    (consumed, result)
                } else {
                    (true, None)
                }
            }
            ModalKind::HistorySearch => {
                let result = if self.history_search.open {
                    self.history_search.handle_key(action, app, tx)
                } else {
                    None
                };
                (result.is_some(), result)
            }
            ModalKind::Form => {
                let result = if self.form_modal.open {
                    self.form_modal.handle_key(action, app, tx)
                } else {
                    None
                };
                (result.is_some(), result)
            }
            ModalKind::CompactReview => {
                let result = if let Some(m) = &mut self.compact_review {
                    m.handle_key(action, app, tx)
                } else {
                    None
                };
                (result.is_some(), result)
            }
            ModalKind::SessionSwitcher => {
                let result = if self.session_switcher.open {
                    self.session_switcher.handle_key(action, app, tx)
                } else {
                    None
                };
                (result.is_some(), result)
            }
        }
    }

    /// Get cursor position for the topmost modal.
    pub fn cursor_position(&self, kind: ModalKind) -> Option<(u16, u16)> {
        match kind {
            ModalKind::Palette => self.palette.cursor_position(),
            ModalKind::HistorySearch => self.history_search.cursor_position(),
            ModalKind::Form => self.form_modal.cursor_position(),
            ModalKind::AliasManager => self.alias_manager.cursor_position(),
            _ => None,
        }
    }

    /// Compute the modal's centered rect within the viewport.
    pub fn compute_rect(&self, kind: ModalKind, canvas: Rect) -> Rect {
        match kind {
            ModalKind::Palette => {
                let w = canvas.width.saturating_sub(4).clamp(40, 80);
                let desired = 4 + self.palette.display_len() as u16 + 2;
                let h = canvas.height.saturating_sub(4).min(desired).max(6);
                center_rect(canvas, w, h)
            }
            ModalKind::Form => {
                let outer_width = (canvas.width.saturating_mul(3) / 4).clamp(50, 100);
                let content_lines = self
                    .form_modal
                    .pending
                    .as_ref()
                    .map(|f| {
                        crate::form_modal::estimate_height(&f.kind, &self.form_modal.multi_selected)
                    })
                    .unwrap_or(6);
                let outer_height = (content_lines + 6).min(canvas.height.saturating_sub(4).max(6));
                center_rect(canvas, outer_width, outer_height)
            }
            ModalKind::CompactReview => {
                let w = canvas.width.saturating_sub(4).clamp(60, 140);
                let h = canvas.height.saturating_sub(4).clamp(16, 40);
                center_rect(canvas, w, h)
            }
            ModalKind::SessionSwitcher => {
                let w = canvas
                    .width
                    .saturating_sub(4)
                    .clamp(72, crate::session_switcher::SESSION_SWITCHER_WIDTH);
                let desired =
                    4 + (self.session_switcher.rows.len().max(1) as u16).saturating_mul(3) + 3;
                let h = canvas.height.saturating_sub(4).min(desired).max(10);
                center_rect(canvas, w, h)
            }
            ModalKind::HistorySearch => {
                let w = canvas.width.saturating_sub(4).clamp(70, 140);
                let h = canvas.height.saturating_sub(4).clamp(20, 42);
                center_rect(canvas, w, h)
            }
            ModalKind::ProviderManager => {
                let w = canvas.width.saturating_sub(4).clamp(50, 70);
                let h = if self.provider_manager.in_form {
                    canvas.height.saturating_sub(2).clamp(20, 30)
                } else {
                    canvas.height.saturating_sub(2).clamp(10, 24)
                };
                center_rect(canvas, w, h)
            }
            ModalKind::AliasManager => {
                if self.alias_manager.show_form {
                    let w = canvas.width.saturating_sub(4).clamp(60, 84);
                    let h = canvas.height.saturating_sub(2).clamp(14, 24);
                    center_rect(canvas, w, h)
                } else {
                    let w = canvas.width.saturating_sub(4).clamp(40, 60);
                    let h = canvas.height.saturating_sub(2).clamp(8, 20);
                    center_rect(canvas, w, h)
                }
            }
            ModalKind::ModelPicker => {
                let w = canvas.width.saturating_sub(4).clamp(48, 74);
                let h = canvas.height.saturating_sub(2).clamp(10, 22);
                center_rect(canvas, w, h)
            }
            ModalKind::ModelManager => {
                let w = canvas.width.saturating_sub(4).clamp(50, 80);
                let h = canvas.height.saturating_sub(2).clamp(10, 24);
                center_rect(canvas, w, h)
            }
            ModalKind::Onboarding => {
                if canvas.width < 60 || canvas.height < 20 {
                    canvas
                } else {
                    let w = canvas.width.saturating_sub(4).clamp(54, 82);
                    let h = canvas.height.saturating_sub(2).clamp(18, 30);
                    center_rect(canvas, w, h)
                }
            }
            ModalKind::ThemePicker => {
                let h = 5u16 + 4;
                let w = 70u16.min(canvas.width);
                center_rect(canvas, w, h)
            }
            ModalKind::TrustModePicker => {
                let h = atman_runtime::trust::TrustMode::all().len() as u16 + 4;
                let w = 70u16.min(canvas.width);
                center_rect(canvas, w, h)
            }
        }
    }

    /// Title for the modal shell header.
    pub fn title_for(&self, kind: ModalKind) -> ratatui::text::Line<'static> {
        let t = crate::theme::theme();
        use ratatui::style::{Modifier, Style};
        use ratatui::text::{Line, Span};
        match kind {
            ModalKind::Palette => Line::from(Span::styled(
                "Command Palette (Esc to close)",
                Style::default().fg(t.tinted_fg.into()),
            )),
            ModalKind::Form => {
                let title = self
                    .form_modal
                    .pending
                    .as_ref()
                    .map(|f| match f.kind.discriminator() {
                        "text" => "Text Input",
                        "confirm" => "Confirm",
                        "select" => "Select",
                        _ => "Form",
                    })
                    .unwrap_or("Form");
                Line::from(Span::styled(
                    title.to_string(),
                    Style::default().fg(t.tinted_fg.into()),
                ))
            }
            ModalKind::CompactReview => {
                if let Some(m) = &self.compact_review {
                    Line::from(Span::styled(
                        format!(
                            "Review Compaction — slice {}..{} ({} msgs, ~{} tokens)",
                            m.pending.range_start,
                            m.pending.range_end,
                            m.pending.slice_count,
                            m.pending.tokens_before,
                        ),
                        Style::default().fg(t.warn.into()),
                    ))
                } else {
                    Line::default()
                }
            }
            ModalKind::SessionSwitcher => {
                let title = if self.session_switcher.rename_mode {
                    format!(
                        " Rename · {}▏ · Enter save · Esc cancel ",
                        self.session_switcher.rename_buf
                    )
                } else if self.session_switcher.delete_armed.is_some() {
                    " Delete? · d again to confirm · any other key cancels ".to_string()
                } else if self.session_switcher.filter_mode {
                    format!(
                        " Filter · {}▏ · Esc/Enter done ",
                        self.session_switcher.filter
                    )
                } else {
                    format!(" Sessions · {} ", self.session_switcher.scope.label())
                };
                let color = if self.session_switcher.rename_mode {
                    t.warn
                } else if self.session_switcher.delete_armed.is_some() {
                    t.error
                } else {
                    t.accent
                };
                Line::from(Span::styled(
                    title,
                    Style::default()
                        .fg(color.into())
                        .add_modifier(Modifier::BOLD),
                ))
            }
            ModalKind::HistorySearch => Line::from(vec![
                Span::styled(
                    "Search History · ",
                    Style::default()
                        .fg(t.accent.into())
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    self.history_search.scope.label().to_string(),
                    Style::default()
                        .fg(t.accent.into())
                        .add_modifier(Modifier::BOLD),
                ),
            ]),
            ModalKind::ProviderManager => Line::from(Span::styled(
                "Provider Manager",
                Style::default().fg(t.tinted_fg.into()),
            )),
            ModalKind::AliasManager => Line::from(Span::styled(
                if self.alias_manager.show_form {
                    "Alias Form"
                } else {
                    "Aliases"
                },
                Style::default().fg(t.tinted_fg.into()),
            )),
            ModalKind::ModelPicker => Line::from(Span::styled(
                "Switch Model",
                Style::default().fg(t.tinted_fg.into()),
            )),
            ModalKind::ModelManager => Line::from(Span::styled(
                "Model Manager",
                Style::default().fg(t.tinted_fg.into()),
            )),
            ModalKind::Onboarding => Line::from(Span::styled(
                "Welcome to atman",
                Style::default().fg(t.tinted_fg.into()),
            )),
            ModalKind::ThemePicker => Line::from(Span::styled(
                "Theme",
                Style::default().fg(t.tinted_fg.into()),
            )),
            ModalKind::TrustModePicker => Line::from(Span::styled(
                "Trust Mode",
                Style::default().fg(t.tinted_fg.into()),
            )),
        }
    }

    /// Icon for the modal shell header.
    pub fn icon_for(&self, kind: ModalKind) -> &'static str {
        match kind {
            ModalKind::Palette => "⌘",
            ModalKind::Form => "✎",
            ModalKind::CompactReview => "◫",
            ModalKind::SessionSwitcher => "▣",
            ModalKind::HistorySearch => "⌕",
            ModalKind::ProviderManager => "⚙",
            ModalKind::AliasManager => "@",
            ModalKind::ModelPicker => "\u{25C6}",
            ModalKind::ModelManager => "\u{25C6}",
            ModalKind::Onboarding => "\u{2726}",
            ModalKind::ThemePicker => "◐",
            ModalKind::TrustModePicker => "⚡",
        }
    }

    /// Accent color for the modal shell header.
    pub fn accent_for(&self, kind: ModalKind, t: &crate::theme::Theme) -> ratatui::style::Color {
        match kind {
            ModalKind::CompactReview => t.warn.into(),
            ModalKind::SessionSwitcher => {
                if self.session_switcher.rename_mode {
                    t.warn.into()
                } else if self.session_switcher.delete_armed.is_some() {
                    t.error.into()
                } else {
                    t.accent.into()
                }
            }
            ModalKind::HistorySearch => match self.history_search.scope {
                crate::history_search_modal::HistorySearchScope::Session => t.accent.into(),
                crate::history_search_modal::HistorySearchScope::Project => t.warn.into(),
            },
            _ => t.accent.into(),
        }
    }

    // ── Theme picker ──

    fn render_theme_picker_content(
        &self,
        f: &mut Frame,
        area: Rect,
        app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        use ratatui::style::{Modifier, Style};
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{List, ListItem, ListState};

        let themes = [
            ("default", "calm / steady / eager / reckless"),
            ("wuxia", "守拙 / 行云 / 破竹 / 逍遥"),
            ("animal", "🦔 hedgehog / 🐱 cat / 🐶 dog / 🦡 honey-badger"),
            ("weather", "🌧 drizzle / ☀️ clear / ⛈ storm / 🌪 tornado"),
            ("drink", "💧 water / ☕ coffee / ☕ espresso / 🧪 bleach"),
        ];
        let items: Vec<ListItem> = themes
            .iter()
            .map(|(id, desc)| {
                let is_current = app.trust.theme.to_string() == *id;
                let marker = if is_current { "  ← current" } else { "" };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        format!(" {:<10}", id),
                        Style::default().add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("  {}{}", desc, marker)),
                ]))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(app.picker_selected.min(items.len() - 1)));
        f.render_stateful_widget(
            List::new(items).highlight_style(
                Style::default()
                    .fg(t.tinted_fg.into())
                    .add_modifier(Modifier::BOLD),
            ),
            area,
            &mut state,
        );
    }

    fn handle_theme_picker_key(
        &mut self,
        action: &crate::keys::KeyAction,
        app: &mut crate::app::AppState,
    ) {
        let themes = [
            atman_runtime::trust::Theme::Default,
            atman_runtime::trust::Theme::Wuxia,
            atman_runtime::trust::Theme::Animal,
            atman_runtime::trust::Theme::Weather,
            atman_runtime::trust::Theme::Drink,
        ];
        let max = themes.len();
        match action {
            crate::keys::KeyAction::Escape => {
                self.theme_picker_open = false;
            }
            crate::keys::KeyAction::HistoryUp | crate::keys::KeyAction::CursorLeft => {
                app.picker_selected = app.picker_selected.checked_sub(1).unwrap_or(max - 1);
            }
            crate::keys::KeyAction::HistoryDown | crate::keys::KeyAction::CursorRight => {
                app.picker_selected = (app.picker_selected + 1) % max;
            }
            crate::keys::KeyAction::Submit | crate::keys::KeyAction::Char('\r') => {
                app.trust.theme = themes[app.picker_selected.min(max - 1)];
                self.theme_picker_open = false;
                app.save_ui_state();
            }
            crate::keys::KeyAction::Quit => app.should_quit = true,
            _ => {}
        }
    }

    // ── Trust mode picker ──

    fn render_trust_mode_picker_content(
        &self,
        f: &mut Frame,
        area: Rect,
        app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        use ratatui::style::{Modifier, Style};
        use ratatui::text::{Line, Span};
        use ratatui::widgets::{List, ListItem, ListState};

        let draft = self.trust_draft.as_ref().unwrap_or(&app.trust);
        let modes = atman_runtime::trust::TrustMode::all();
        let items: Vec<ListItem> = modes
            .iter()
            .map(|&m| {
                let d = app.trust.theme.display(m);
                let color = match d.color {
                    atman_runtime::trust::ModeColor::Cyan => t.accent.into(),
                    atman_runtime::trust::ModeColor::Green => t.success.into(),
                    atman_runtime::trust::ModeColor::Yellow => t.warn.into(),
                    atman_runtime::trust::ModeColor::Orange => {
                        ratatui::style::Color::Rgb(208, 135, 22)
                    }
                    atman_runtime::trust::ModeColor::Red => t.error.into(),
                };
                let marker = if m == app.trust.mode {
                    "← current"
                } else {
                    ""
                };
                let matrix = if m == atman_runtime::trust::TrustMode::Reckless {
                    "T0–T4: unrestricted".to_owned()
                } else {
                    let config = atman_runtime::trust::TrustConfig {
                        mode: m,
                        ..draft.clone()
                    };
                    let tiers = [
                        atman_runtime::tool::Tier::Zero,
                        atman_runtime::tool::Tier::One,
                        atman_runtime::tool::Tier::Two,
                        atman_runtime::tool::Tier::Three,
                        atman_runtime::tool::Tier::Four,
                    ];
                    let actions = tiers
                        .iter()
                        .map(|tier| format!("{:?}", config.resolve_tier(*tier)))
                        .collect::<Vec<_>>();
                    let risks = [
                        (
                            "x",
                            config.resolve_risk(atman_runtime::trust::RiskKind::WorkspaceExternal),
                        ),
                        (
                            "n",
                            config.resolve_risk(atman_runtime::trust::RiskKind::Network),
                        ),
                        (
                            "i",
                            config.resolve_risk(atman_runtime::trust::RiskKind::Irreversible),
                        ),
                        (
                            "w",
                            config.resolve_risk(atman_runtime::trust::RiskKind::FilesystemWrite),
                        ),
                        (
                            "p",
                            config.resolve_risk(atman_runtime::trust::RiskKind::ProcessSpawn),
                        ),
                        (
                            "r",
                            config.resolve_risk(atman_runtime::trust::RiskKind::RepositoryMutation),
                        ),
                    ];
                    let risk_summary = risks
                        .iter()
                        .map(|(key, action)| format!("{key}:{action:?}"))
                        .collect::<Vec<_>>()
                        .join(" ");
                    format!(
                        "T0–T4 {} · risks {risk_summary} · escalation {}",
                        actions.join("/"),
                        config.escalation.label()
                    )
                };
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {} ", d.emoji), Style::default().fg(color)),
                    Span::styled(
                        format!("{:<14}", d.name),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(format!("  {} · {}  {}", d.description, matrix, marker)),
                ]))
            })
            .collect();
        let mut state = ListState::default();
        state.select(Some(app.picker_selected.min(items.len() - 1)));
        f.render_stateful_widget(
            List::new(items).highlight_style(
                Style::default()
                    .bg(t.highlight_bg.into())
                    .add_modifier(Modifier::BOLD),
            ),
            area,
            &mut state,
        );
    }

    fn handle_trust_mode_picker_key(
        &mut self,
        action: &crate::keys::KeyAction,
        app: &mut crate::app::AppState,
        tx: Option<&mpsc::UnboundedSender<crate::TuiControl>>,
    ) {
        let modes = atman_runtime::trust::TrustMode::all();
        let max = modes.len();
        let draft = self.trust_draft.get_or_insert_with(|| app.trust.clone());
        match action {
            crate::keys::KeyAction::Escape => {
                self.trust_mode_picker_open = false;
            }
            crate::keys::KeyAction::HistoryUp | crate::keys::KeyAction::CursorLeft => {
                app.picker_selected = app.picker_selected.checked_sub(1).unwrap_or(max - 1);
            }
            crate::keys::KeyAction::HistoryDown | crate::keys::KeyAction::CursorRight => {
                app.picker_selected = (app.picker_selected + 1) % max;
            }
            crate::keys::KeyAction::Char(c @ '0'..='4')
                if modes[app.picker_selected.min(max - 1)]
                    == atman_runtime::trust::TrustMode::Eager =>
            {
                let tier = match c {
                    '0' => atman_runtime::tool::Tier::Zero,
                    '1' => atman_runtime::tool::Tier::One,
                    '2' => atman_runtime::tool::Tier::Two,
                    '3' => atman_runtime::tool::Tier::Three,
                    _ => atman_runtime::tool::Tier::Four,
                };
                let current = draft.resolve_tier(tier);
                let slot = match tier {
                    atman_runtime::tool::Tier::Zero => &mut draft.tiers.eager.tier0,
                    atman_runtime::tool::Tier::One => &mut draft.tiers.eager.tier1,
                    atman_runtime::tool::Tier::Two => &mut draft.tiers.eager.tier2,
                    atman_runtime::tool::Tier::Three => &mut draft.tiers.eager.tier3,
                    atman_runtime::tool::Tier::Four => &mut draft.tiers.eager.tier4,
                };
                *slot = Some(next_policy_action(current));
                if let Some(tx) = tx {
                    let _ = tx.send(crate::TuiControl::UpdateTrust(draft.clone()));
                }
            }
            crate::keys::KeyAction::Char(c @ ('n' | 'w' | 'i' | 'x' | 'p' | 'r'))
                if modes[app.picker_selected.min(max - 1)]
                    == atman_runtime::trust::TrustMode::Eager =>
            {
                let risk = match c {
                    'n' => atman_runtime::trust::RiskKind::Network,
                    'w' => atman_runtime::trust::RiskKind::FilesystemWrite,
                    'i' => atman_runtime::trust::RiskKind::Irreversible,
                    'x' => atman_runtime::trust::RiskKind::WorkspaceExternal,
                    'p' => atman_runtime::trust::RiskKind::ProcessSpawn,
                    _ => atman_runtime::trust::RiskKind::RepositoryMutation,
                };
                let current = draft.resolve_risk(risk);
                let slot = match c {
                    'n' => &mut draft.risks.eager.network,
                    'w' => &mut draft.risks.eager.filesystem_write,
                    'i' => &mut draft.risks.eager.irreversible,
                    'x' => &mut draft.risks.eager.outside_workspace,
                    'p' => &mut draft.risks.eager.process_spawn,
                    _ => &mut draft.risks.eager.repository_mutation,
                };
                *slot = Some(next_policy_action(current));
                if let Some(tx) = tx {
                    let _ = tx.send(crate::TuiControl::UpdateTrust(draft.clone()));
                }
            }
            crate::keys::KeyAction::Char('e')
                if modes[app.picker_selected.min(max - 1)]
                    == atman_runtime::trust::TrustMode::Eager =>
            {
                draft.escalation = draft.escalation.next();
                if let Some(tx) = tx {
                    let _ = tx.send(crate::TuiControl::UpdateTrust(draft.clone()));
                }
            }
            crate::keys::KeyAction::Submit | crate::keys::KeyAction::Char('\r') => {
                let new_mode = modes[app.picker_selected.min(max - 1)];
                let prev = app.trust.mode;
                self.trust_mode_picker_open = false;
                if new_mode != prev {
                    draft.mode = new_mode;
                    if let Some(tx) = tx {
                        let _ = tx.send(crate::TuiControl::UpdateTrust(draft.clone()));
                    }
                    let display = app.trust.theme.display(new_mode);
                    if let Some(warning) = new_mode.warning(&display) {
                        app.push_note(&warning, crate::app::NoteLevel::Warn);
                    }
                }
            }
            crate::keys::KeyAction::Quit => app.should_quit = true,
            _ => {}
        }
    }
}

// ── Helpers ──

fn next_policy_action(
    action: atman_runtime::trust::PolicyAction,
) -> atman_runtime::trust::PolicyAction {
    match action {
        atman_runtime::trust::PolicyAction::Auto => atman_runtime::trust::PolicyAction::Ask,
        atman_runtime::trust::PolicyAction::Ask => atman_runtime::trust::PolicyAction::Deny,
        atman_runtime::trust::PolicyAction::Deny => atman_runtime::trust::PolicyAction::Auto,
    }
}

fn center_rect(canvas: Rect, w: u16, h: u16) -> Rect {
    Rect {
        x: canvas.x + canvas.width.saturating_sub(w) / 2,
        y: canvas.y + canvas.height.saturating_sub(h) / 2,
        width: w,
        height: h,
    }
}
