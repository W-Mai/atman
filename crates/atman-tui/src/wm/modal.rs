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
}

impl ModalKind {
    pub fn is_open(self, flags: &crate::app::ModalOpenFlags) -> bool {
        match self {
            Self::Form => flags.form,
            Self::CompactReview => flags.compact_review,
            Self::SessionSwitcher => flags.session_switcher,
            Self::HistorySearch => flags.history_search,
            Self::ProviderManager => flags.provider_manager,
            Self::AliasManager => flags.alias_manager,
            Self::ModelPicker => flags.model_picker,
            Self::Onboarding => flags.onboarding,
            Self::Palette => flags.palette,
            Self::ThemePicker => flags.theme_picker,
        }
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
