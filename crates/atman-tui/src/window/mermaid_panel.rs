use ratatui::Frame;
use ratatui::layout::Rect;

use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

pub struct MermaidPanelContent {
    pub item_id: String,
    pub scroll: u16,
    pub h_scroll: u16,
    pub split: bool,
}

impl WindowComponent for MermaidPanelContent {
    fn render_content(&mut self, area: Rect, frame: &mut Frame, ctx: &RenderCtx) -> Vec<HitRegion> {
        let area = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        let item_idx: Option<usize> = self
            .item_id
            .strip_prefix("mermaid:")
            .and_then(|s| s.parse().ok());
        if let Some(idx) = item_idx
            && let Some(crate::app::OutputItem::MermaidDiagram { source }) = ctx.items.get(idx)
        {
            let pad = 2u16;
            let inner_area = Rect {
                x: area.x + pad,
                y: area.y + 1,
                width: area.width.saturating_sub(pad * 2),
                height: area.height.saturating_sub(2),
            };
            if inner_area.width == 0 || inner_area.height == 0 {
                return Vec::new();
            }
            if self.split {
                let half_w = inner_area.width / 2;
                let left_area = Rect {
                    width: half_w,
                    ..inner_area
                };
                let right_area = Rect {
                    x: inner_area.x + half_w,
                    width: inner_area.width.saturating_sub(half_w),
                    ..inner_area
                };

                let t = crate::theme::theme();
                let src_bg: ratatui::style::Color = t.code_bg.into();

                let src_lines: Vec<ratatui::text::Line<'static>> = source
                    .lines()
                    .enumerate()
                    .map(|(i, l)| {
                        ratatui::text::Line::from(vec![
                            ratatui::text::Span::styled(
                                format!("{:>3} ", i + 1),
                                ratatui::style::Style::default()
                                    .fg(t.subtle_fg.into())
                                    .bg(src_bg)
                                    .add_modifier(ratatui::style::Modifier::DIM),
                            ),
                            ratatui::text::Span::styled(
                                l.to_string(),
                                ratatui::style::Style::default()
                                    .fg(t.tinted_fg.into())
                                    .bg(src_bg),
                            ),
                        ])
                    })
                    .collect();
                let src_max = (src_lines.len() as u16).saturating_sub(left_area.height);
                self.scroll = self.scroll.min(src_max);
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(src_lines).scroll((self.scroll, 0)),
                    left_area,
                );

                let diagram_lines = crate::mermaid::render_mermaid(source, right_area.width);
                let diag_max = (diagram_lines.len() as u16).saturating_sub(right_area.height);
                self.h_scroll = self.h_scroll.min(diag_max);
                frame.render_widget(
                    ratatui::widgets::Paragraph::new(diagram_lines).scroll((self.h_scroll, 0)),
                    right_area,
                );

                let divider_x = inner_area.x + half_w;
                for y in inner_area.y..inner_area.y + inner_area.height {
                    if let Some(cell) = frame
                        .buffer_mut()
                        .cell_mut(ratatui::layout::Position { x: divider_x, y })
                    {
                        cell.set_char('│');
                    }
                }
            } else {
                let lines = crate::mermaid::render_mermaid(source, inner_area.width);
                let max_scroll = (lines.len() as u16).saturating_sub(inner_area.height);
                self.scroll = self.scroll.min(max_scroll);
                let content_w = lines
                    .iter()
                    .map(|l| crate::width::spans_width(&l.spans))
                    .max()
                    .unwrap_or(0) as u16;
                let max_h_scroll = content_w.saturating_sub(inner_area.width);
                self.h_scroll = self.h_scroll.min(max_h_scroll);
                let scroll = self.scroll;
                let h_scroll = self.h_scroll;
                let visible_w = inner_area.width as usize;
                let scrolled_lines: Vec<ratatui::text::Line> = lines
                    .iter()
                    .skip(scroll as usize)
                    .map(|l| {
                        let line_str: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                        let trimmed = crate::width::trim_display_offset(
                            &line_str,
                            h_scroll as usize,
                            visible_w,
                        );
                        ratatui::text::Line::from(ratatui::text::Span::styled(
                            trimmed,
                            l.spans.first().map(|s| s.style).unwrap_or_default(),
                        ))
                    })
                    .collect();
                let p = ratatui::widgets::Paragraph::new(scrolled_lines);
                frame.render_widget(p, inner_area);
            }
        } else {
            super::common::render_placeholder(frame, area, &self.item_id);
        }
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

    fn sync_state(&mut self, scroll: u16, h_scroll: u16, split: bool) {
        self.scroll = scroll;
        self.h_scroll = h_scroll;
        self.split = split;
    }

    fn extract_state(&self) -> (u16, u16, bool) {
        (self.scroll, self.h_scroll, self.split)
    }

    fn title_suffix(&self) -> Option<String> {
        if self.split {
            Some("Tab: diagram".into())
        } else {
            Some("Tab: split".into())
        }
    }
}
