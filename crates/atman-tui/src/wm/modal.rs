use ratatui::Frame;
use ratatui::layout::Rect;

use super::component::{EventCtx, HitRegion, RenderCtx, WmEvent, WmEventResult};
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModalKind {
    Form,
    CompactReview,
    SessionSwitcher,
    HistorySearch,
    ProviderManager,
    AliasManager,
    ModelPicker,
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
    pub session_switcher: crate::session_switcher::SessionSwitcher,
    pub history_search: crate::history_search_modal::HistorySearchModal,
    pub onboarding: crate::onboarding::OnboardingState,
    pub onboarding_open: bool,
    pub compact_review: Option<crate::compact_review_modal::CompactReviewModal>,
    pub theme_picker_open: bool,
    pub trust_mode_picker_open: bool,
}

impl ModalManager {
    pub fn any_open(&self) -> bool {
        self.palette.open
            || self.form_modal.open
            || self.provider_manager.open
            || self.alias_manager.open
            || self.model_picker.open
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

/// A modal component — blocks input to lower layers.
///
/// Modals are stacked: opening a new modal pushes, closing pops.
/// Only the topmost modal receives keyboard/mouse events.
pub trait ModalComponent: Send {
    /// Render the modal. Return hit regions for mouse dispatch.
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &RenderCtx);

    /// Handle a key or mouse event. Return Consumed/Ignored.
    fn handle_event(&mut self, event: &WmEvent, ctx: &mut EventCtx) -> WmEventResult;

    /// Called when this modal is pushed onto the stack.
    fn on_open(&mut self) {}

    /// Called when this modal is popped from the stack.
    fn on_close(&mut self) {}

    /// Whether closing should be blocked (unsaved changes, etc.).
    fn can_close(&self) -> super::component::CloseOutcome {
        super::component::CloseOutcome::Close
    }

    /// Preferred rect relative to viewport.
    fn preferred_rect(&self, viewport: Rect) -> Rect;

    /// Hit-test a mouse position. Returns Inside with regions, or Outside.
    fn hit_test(&self, col: u16, row: u16, viewport: Rect) -> HitTestResult {
        let rect = self.preferred_rect(viewport);
        if col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
        {
            HitTestResult::Inside(Vec::new())
        } else {
            HitTestResult::Outside
        }
    }

    /// Policy for clicks outside the modal rect.
    fn outside_click_policy(&self) -> OutsideClickPolicy {
        OutsideClickPolicy::Consume
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModalEntry {
    pub kind: ModalKind,
    pub pre_modal_focus: Option<crate::wm::WindowId>,
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
    ) -> bool;
    fn cursor_position(&self) -> Option<(u16, u16)>;
    fn title(&self) -> ratatui::text::Line<'static>;
    fn icon(&self) -> &str;
    fn accent(&self, t: &crate::theme::Theme) -> ratatui::style::Color;
}
