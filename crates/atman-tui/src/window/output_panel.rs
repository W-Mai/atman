use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;

use crate::app::{Disclosure, OutputItem};
use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

pub struct OutputPanelContent {
    item: OutputItem,
    scroll: u32,
}

impl OutputPanelContent {
    pub fn new(mut item: OutputItem) -> Self {
        match &mut item {
            OutputItem::DiffPreview { expanded, .. }
            | OutputItem::Bash { expanded, .. }
            | OutputItem::Terminal { expanded, .. }
            | OutputItem::SubAgentActivity { expanded, .. } => *expanded = true,
            OutputItem::Thinking { disclosure, .. }
            | OutputItem::CompactionSummary { disclosure, .. } => {
                *disclosure = Disclosure::Full;
            }
            _ => {}
        }
        Self { item, scroll: 0 }
    }
}

impl WindowComponent for OutputPanelContent {
    fn render_content(
        &mut self,
        area: Rect,
        frame: &mut Frame,
        _ctx: &RenderCtx,
    ) -> Vec<HitRegion> {
        let area = Rect::new(
            area.x.saturating_add(1),
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        let expanded_tools = HashSet::new();
        let render_ctx = crate::output::RenderCtx {
            expanded_tools: &expanded_tools,
            messages: &[],
            panel_width: area.width,
            hovered_thinking_idx: None,
            hovered_output_node: None,
            animation_frame: 0,
        };
        let lines = crate::output::render_item(&self.item, &render_ctx);
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
