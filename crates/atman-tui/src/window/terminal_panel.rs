use ratatui::Frame;
use ratatui::layout::Rect;

use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

pub struct TerminalPanelContent {
    pub handle: String,
    pub scroll: u16,
}

impl WindowComponent for TerminalPanelContent {
    fn render_content(&mut self, area: Rect, frame: &mut Frame, ctx: &RenderCtx) -> Vec<HitRegion> {
        let area = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        let snap = ctx
            .snapshots
            .iter()
            .find(|s| s.source_handle == self.handle);
        let item = ctx.items.iter().rev().find(|it| match it {
            crate::app::OutputItem::Terminal { handle, .. } => *handle == self.handle,
            _ => false,
        });
        use crate::app::OutputItem;
        if let Some(OutputItem::Terminal {
            screen,
            accumulated_bytes,
            done,
            ..
        }) = item
        {
            if let Some(snap) = snap {
                crate::wm::floating::content::render_terminal_content(
                    frame,
                    area,
                    snap,
                    screen,
                    accumulated_bytes,
                    *done,
                );
            } else {
                crate::wm::floating::content::render_terminal_screen(
                    frame,
                    area,
                    &self.handle,
                    screen,
                );
            }
        } else if let Some(snap) = snap {
            crate::wm::floating::content::render_task_meta(
                frame,
                area,
                atman_runtime::TaskKind::Terminal,
                snap,
            );
        } else {
            crate::wm::floating::content::render_placeholder(frame, area, &self.handle);
        }
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
