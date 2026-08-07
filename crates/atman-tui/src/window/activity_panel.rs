use ratatui::Frame;
use ratatui::layout::Rect;

use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

pub struct ActivityPanelContent {
    pub run_id: String,
    pub scroll: u16,
}

impl WindowComponent for ActivityPanelContent {
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
            min: (40, 10),
            max: None,
            preferred: (70, 30),
        }
    }
}
