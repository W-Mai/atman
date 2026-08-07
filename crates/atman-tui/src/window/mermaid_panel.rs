use ratatui::Frame;
use ratatui::layout::Rect;

use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WmEvent, WmEventResult, WindowComponent,
};

pub struct MermaidPanelContent {
    pub item_id: String,
    pub scroll: u16,
    pub h_scroll: u16,
    pub split: bool,
}

impl WindowComponent for MermaidPanelContent {
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
            preferred: (80, 30),
        }
    }

    fn title_suffix(&self) -> Option<String> {
        if self.split {
            Some("Tab: diagram".into())
        } else {
            Some("Tab: split".into())
        }
    }
}
