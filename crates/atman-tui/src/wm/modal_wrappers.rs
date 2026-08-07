//! Skeleton for modal wrappers — to be filled in when individual modals
//! are migrated to `ModalComponent`.
//!
//! Each existing modal (provider_manager, onboarding, alias_manager, etc.)
//! will get a wrapper that implements `ModalComponent`. For now this is
//! just a placeholder to verify the trait is usable.

use crate::wm::component::{EventCtx, RenderCtx, WmEvent, WmEventResult};
use crate::wm::{CloseOutcome, ModalComponent, OutsideClickPolicy};
use ratatui::Frame;
use ratatui::layout::Rect;

/// Thin wrapper for any modal with a known rect.
/// Will be replaced by per-modal wrappers during migration.
pub struct GenericModalWrapper {
    pub rect: Option<Rect>,
}

impl ModalComponent for GenericModalWrapper {
    fn render(&mut self, _frame: &mut Frame, _area: Rect, _ctx: &RenderCtx) {}

    fn handle_event(&mut self, _event: &WmEvent, _ctx: &mut EventCtx) -> WmEventResult {
        WmEventResult::Ignored
    }

    fn preferred_rect(&self, viewport: Rect) -> Rect {
        self.rect.unwrap_or(viewport)
    }

    fn can_close(&self) -> CloseOutcome {
        CloseOutcome::Close
    }

    fn outside_click_policy(&self) -> OutsideClickPolicy {
        OutsideClickPolicy::Consume
    }
}
