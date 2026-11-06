pub(crate) mod preparation;

use std::sync::Arc;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::widgets::Paragraph;
use ratatui::{style::Style, text::Line};

use crate::app::OutputItem;
use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

pub struct OutputPanelContent {
    snapshot: Option<Arc<OutputItem>>,
    item_index: usize,
    tool_use_id: String,
    scroll: u32,
    selection_projection: Option<crate::selection::VisibleSelectionProjection>,
    preparation: preparation::Preparation,
    retry_rect: Option<Rect>,
    retry_hover: bool,
}

impl OutputPanelContent {
    pub fn new(item_index: usize, tool_use_id: String) -> Self {
        Self {
            snapshot: None,
            item_index,
            tool_use_id,
            scroll: 0,
            selection_projection: None,
            preparation: preparation::Preparation::default(),
            retry_rect: None,
            retry_hover: false,
        }
    }

    pub(crate) fn snapshot(mut item: Box<OutputItem>) -> Self {
        crate::output::set_detail_expanded(&mut item, true);
        let mut panel = Self::new(0, String::new());
        panel.snapshot = Some(Arc::from(item));
        panel
    }
}

impl WindowComponent for OutputPanelContent {
    fn has_active_animation(&self) -> bool {
        self.preparation.is_animating()
    }

    fn render_content(&mut self, area: Rect, frame: &mut Frame, ctx: &RenderCtx) -> Vec<HitRegion> {
        let area = Rect::new(
            area.x.saturating_add(1),
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        // The retained result still owns this source while the old viewport is dropped.
        self.selection_projection = None;
        self.retry_rect = None;
        if area.is_empty() {
            return Vec::new();
        }
        let Some(_) = self.snapshot.as_deref().or_else(|| {
            let OutputItem::ToolDispatch { calls } = ctx.items.get(self.item_index)? else {
                return None;
            };
            calls
                .iter()
                .find(|call| call.id == self.tool_use_id)
                .and_then(|call| call.detail.as_deref())
        }) else {
            self.preparation.clear();
            return Vec::new();
        };
        let key = preparation::Key {
            revision: if self.snapshot.is_some() {
                Default::default()
            } else {
                ctx.item_revisions
                    .get(self.item_index)
                    .copied()
                    .unwrap_or_default()
            },
            window: ctx.window_id,
            width: area.width,
            theme: crate::theme::theme(),
            preferences: crate::render_preferences::RenderPreferences::current(),
        };
        self.preparation.update(key, || {
            self.snapshot
                .clone()
                .map(crate::flow_view::OutputCapture::from)
                .unwrap_or_else(|| {
                    ctx.items
                        .capture(self.item_index)
                        .expect("existing tool source")
                        .detail(self.tool_use_id.clone())
                })
        });
        if let Some(ready) = &self.preparation.ready {
            let max_scroll = ready
                .lines
                .len()
                .saturating_sub(usize::from(area.height))
                .min(u32::MAX as usize) as u32;
            self.scroll = self.scroll.min(max_scroll);
            let start = self.scroll as usize;
            let end = start
                .saturating_add(usize::from(area.height))
                .min(ready.lines.len());
            let mut visible = ready.lines[start..end].to_vec();
            crate::output::patch_animation_lines(
                &mut visible,
                &ready.paint,
                ctx.animation_frame,
                start,
            );
            self.selection_projection = ready
                .text
                .as_ref()
                .map(|text| text.visible(area, self.scroll));
            frame.render_widget(Paragraph::new(visible), area);
        }
        if let Some(error) = self.preparation.error() {
            let style = Style::default().fg(key.theme.subtle_fg.into());
            frame.render_widget(
                Paragraph::new(Line::styled(error.to_owned(), style)),
                Rect { height: 1, ..area },
            );
            if area.height > 1 {
                let rect = Rect::new(area.x, area.y + 1, area.width.min(11), 1);
                self.retry_rect = Some(rect);
                let style = if self.retry_hover {
                    style.bg(key.theme.work_hover_bg.into())
                } else {
                    style
                };
                frame.render_widget(Paragraph::new(Line::styled(" Retry [R] ", style)), rect);
            }
        } else if self.preparation.is_pending() {
            let line = if self.preparation.ready.is_none() {
                crate::loading::line("Preparing", ctx.animation_frame, area.width)
            } else {
                Line::styled(
                    crate::loading::glyph(ctx.animation_frame),
                    Style::default().fg(key.theme.subtle_fg.into()),
                )
            };
            frame.render_widget(Paragraph::new(line), Rect { height: 1, ..area });
        }
        Vec::new()
    }

    fn selection_projection(&self) -> Option<crate::selection::VisibleSelectionProjection> {
        self.selection_projection.clone()
    }

    fn handle_event(&mut self, event: &WmEvent, _ctx: &mut EventCtx) -> WmEventResult {
        use crossterm::event::{MouseButton, MouseEventKind};
        if let WmEvent::Mouse(mouse) = event {
            self.retry_hover = self
                .retry_rect
                .is_some_and(|rect| rect.contains((mouse.column, mouse.row).into()));
        }
        if self.preparation.error().is_some()
            && (matches!(event, WmEvent::Key(crate::keys::KeyAction::Char('r' | 'R')))
                || matches!(event, WmEvent::Mouse(mouse) if self.retry_hover && mouse.kind == MouseEventKind::Down(MouseButton::Left)))
        {
            self.preparation.retry();
            return WmEventResult::Consumed(Vec::new());
        }
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

impl Drop for OutputPanelContent {
    fn drop(&mut self) {
        self.selection_projection = None;
        preparation::discard(self.snapshot.take());
    }
}
