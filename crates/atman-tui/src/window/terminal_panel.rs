use ratatui::Frame;
use ratatui::layout::Rect;

use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WmEvent, WmEventResult, WindowComponent,
};

pub struct TerminalPanelContent {
    pub handle: String,
    pub scroll: u16,
}

impl WindowComponent for TerminalPanelContent {
    fn render_content(
        &mut self,
        _area: Rect,
        _frame: &mut Frame,
        _ctx: &RenderCtx,
    ) -> Vec<HitRegion> {
        Vec::new()
    }

    fn handle_event(&mut self, _event: &WmEvent, _ctx: &mut EventCtx) -> WmEventResult {
        WmEventResult::Ignored
    }

    fn preferred_size(&self, _viewport: Rect) -> SizeHint {
        SizeHint {
            min: (20, 6),
            max: None,
            preferred: (88, 29),
        }
    }
}
