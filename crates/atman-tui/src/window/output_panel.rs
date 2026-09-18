use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use crate::app::OutputItem;
use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

pub struct OutputPanelContent {
    item_index: usize,
    tool_use_id: String,
    scroll: u32,
}

impl OutputPanelContent {
    pub fn new(item_index: usize, tool_use_id: String) -> Self {
        Self {
            item_index,
            tool_use_id,
            scroll: 0,
        }
    }
}

impl WindowComponent for OutputPanelContent {
    fn render_content(&mut self, area: Rect, frame: &mut Frame, ctx: &RenderCtx) -> Vec<HitRegion> {
        let area = Rect::new(
            area.x.saturating_add(1),
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        let Some(OutputItem::ToolDispatch { calls }) = ctx.items.get(self.item_index) else {
            return Vec::new();
        };
        let Some(item) = calls
            .iter()
            .find(|call| call.id == self.tool_use_id)
            .and_then(|call| call.detail.as_deref())
        else {
            return Vec::new();
        };
        let expanded_tools = HashSet::new();
        let render_ctx = crate::output::RenderCtx {
            expanded_tools: &expanded_tools,
            messages: &[],
            panel_width: area.width,
            hovered_thinking_idx: None,
            hovered_output_node: None,
            animation_frame: ctx.animation_frame,
        };
        let lines = crate::output::render_item(item, &render_ctx);
        let max_scroll = (lines.len() as u32).saturating_sub(area.height as u32);
        self.scroll = self.scroll.min(max_scroll);
        let visible = lines
            .into_iter()
            .skip(self.scroll as usize)
            .collect::<Vec<_>>();
        frame.render_widget(Paragraph::new(visible), area);
        Vec::new()
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

    fn preferred_size(&self, viewport: Rect) -> SizeHint {
        SizeHint {
            min: (40, 12),
            max: None,
            preferred: (viewport.width, viewport.height),
        }
    }
}
