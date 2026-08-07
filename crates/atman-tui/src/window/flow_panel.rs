use ratatui::Frame;
use ratatui::layout::Rect;

use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WmEvent, WmEventResult, WindowComponent,
};

pub struct FlowPanelContent {
    pub handle: String,
    pub scroll: u16,
    pub expanded_tools: std::collections::HashSet<String>,
}

impl WindowComponent for FlowPanelContent {
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

    fn wants_background_updates(&self) -> bool {
        true
    }
}
