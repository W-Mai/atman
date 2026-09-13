use ratatui::Frame;
use ratatui::layout::Rect;

use crate::wm::WmHitmap;
use crate::wm::component::{
    EventCtx, HitRegion, HitTarget, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

#[derive(Default)]
pub struct McpPanelContent {
    pub scroll: u32,
    projection: crate::mcp_manager::McpPanelProjection,
}

impl WindowComponent for McpPanelContent {
    fn render_content(&mut self, area: Rect, frame: &mut Frame, ctx: &RenderCtx) -> Vec<HitRegion> {
        let area = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        let mut hitmap = WmHitmap::default();
        crate::mcp_manager::render_panel(
            frame,
            area,
            &mut self.scroll,
            ctx.mcp_servers,
            ctx.expanded_mcp_servers,
            ctx.mcp_selected,
            ctx.hovered_mcp_row,
            &mut hitmap,
            ctx.mcp_browser,
            ctx.window_id,
            &mut self.projection,
        );
        let mut regions: Vec<_> = hitmap
            .mcp_row_rects
            .into_iter()
            .map(|(_, id, rect)| HitRegion {
                target: HitTarget::McpRow(id),
                rect,
            })
            .collect();
        regions.extend(
            hitmap
                .mcp_action_rects
                .into_iter()
                .map(|(_, action, rect)| HitRegion {
                    target: HitTarget::McpAction(action),
                    rect,
                }),
        );
        regions
    }

    fn handle_event(&mut self, _event: &WmEvent, _ctx: &mut EventCtx) -> WmEventResult {
        WmEventResult::Ignored
    }

    fn sync_state(&mut self, scroll: u32, _h_scroll: u16, _split: bool) {
        self.scroll = scroll;
    }

    fn extract_state(&self) -> (u32, u16, bool) {
        (self.scroll, 0, false)
    }

    fn preferred_size(&self, _viewport: Rect) -> SizeHint {
        SizeHint {
            min: (40, 10),
            max: None,
            preferred: (70, 30),
        }
    }
}
