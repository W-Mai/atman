use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::completion;
use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

pub struct CheatsheetPanelContent {
    pub scroll: u16,
}

impl WindowComponent for CheatsheetPanelContent {
    fn render_content(
        &mut self,
        area: Rect,
        frame: &mut Frame,
        _ctx: &RenderCtx,
    ) -> Vec<HitRegion> {
        let lines = completion::cheatsheet_lines();
        let max_scroll = (lines.len() as u16).saturating_sub(area.height);
        self.scroll = self.scroll.min(max_scroll);
        let scroll = self.scroll;
        let visible: Vec<Line> = lines.into_iter().skip(scroll as usize).collect();
        frame.render_widget(Paragraph::new(visible), area);
        Vec::new()
    }

    fn handle_event(&mut self, _event: &WmEvent, _ctx: &mut EventCtx) -> WmEventResult {
        WmEventResult::Ignored
    }

    fn sync_state(&mut self, scroll: u16, _h_scroll: u16, _split: bool) {
        self.scroll = scroll;
    }

    fn preferred_size(&self, _viewport: Rect) -> SizeHint {
        SizeHint {
            min: (40, 10),
            max: None,
            preferred: (60, 24),
        }
    }
}
