use std::ops::Range;

use crossterm::event::MouseEventKind;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct MermaidProjectionKey {
    item_id: u64,
    source_generation: u64,
    diagram_width: u16,
    theme_mode: crate::theme::ThemeMode,
}

#[derive(Debug, Default)]
struct SourceLineIndex {
    ranges: Vec<Range<usize>>,
    number_width: usize,
}

impl SourceLineIndex {
    fn rebuild(&mut self, source: &str) {
        let source_start = source.as_ptr() as usize;
        self.ranges = source
            .lines()
            .map(|line| {
                let start = (line.as_ptr() as usize).saturating_sub(source_start);
                start..start.saturating_add(line.len())
            })
            .collect();
        self.number_width = decimal_digits(self.ranges.len()).max(3);
    }

    fn visible_lines(&self, source: &str, scroll: u32, height: u16) -> Vec<Line<'static>> {
        let range = visible_range(self.ranges.len(), scroll, height);
        let t = crate::theme::theme();
        let background = t.code_bg.into();
        self.ranges[range]
            .iter()
            .enumerate()
            .map(|(offset, byte_range)| {
                let line_number = usize::try_from(scroll)
                    .unwrap_or(usize::MAX)
                    .saturating_add(offset)
                    .saturating_add(1);
                let source_line = source.get(byte_range.clone()).unwrap_or_default();
                Line::from(vec![
                    Span::styled(
                        format!("{:>width$} ", line_number, width = self.number_width),
                        Style::default()
                            .fg(t.subtle_fg.into())
                            .bg(background)
                            .add_modifier(Modifier::DIM),
                    ),
                    Span::styled(
                        source_line.to_string(),
                        Style::default().fg(t.tinted_fg.into()).bg(background),
                    ),
                ])
            })
            .collect()
    }
}

#[derive(Debug, Default)]
struct MermaidPanelProjection {
    key: Option<MermaidProjectionKey>,
    diagram_lines: Vec<Line<'static>>,
    content_width: u32,
    source_lines: SourceLineIndex,
    #[cfg(test)]
    rebuild_count: u64,
}

impl MermaidPanelProjection {
    fn update(&mut self, revision: crate::app::OutputRevision, source: &str, diagram_width: u16) {
        let key = MermaidProjectionKey {
            item_id: revision.id,
            source_generation: revision.source_generation,
            diagram_width,
            theme_mode: crate::theme::current_mode(),
        };
        if self.key == Some(key) {
            return;
        }

        self.diagram_lines = crate::mermaid::render_mermaid(source, diagram_width);
        self.content_width = self
            .diagram_lines
            .iter()
            .map(|line| crate::width::spans_width(&line.spans))
            .max()
            .map(|width| u32::try_from(width).unwrap_or(u32::MAX))
            .unwrap_or(0);
        self.source_lines.rebuild(source);
        self.key = Some(key);
        #[cfg(test)]
        {
            self.rebuild_count = self.rebuild_count.wrapping_add(1);
        }
    }

    fn diagram_rows(&self) -> u32 {
        u32::try_from(self.diagram_lines.len()).unwrap_or(u32::MAX)
    }

    fn source_rows(&self) -> u32 {
        u32::try_from(self.source_lines.ranges.len()).unwrap_or(u32::MAX)
    }

    fn visible_diagram(
        &self,
        vertical_scroll: u32,
        height: u16,
        horizontal_scroll: u32,
        width: u16,
    ) -> Vec<Line<'static>> {
        let range = visible_range(self.diagram_lines.len(), vertical_scroll, height);
        let horizontal_scroll = usize::try_from(horizontal_scroll).unwrap_or(usize::MAX);
        let width = width as usize;
        self.diagram_lines[range]
            .iter()
            .map(|line| {
                let text: String = line
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                Line::from(Span::styled(
                    crate::width::trim_display_offset(&text, horizontal_scroll, width),
                    line.spans
                        .first()
                        .map(|span| span.style)
                        .unwrap_or_default(),
                ))
            })
            .collect()
    }
}

pub struct MermaidPanelContent {
    pub item_id: String,
    pub scroll: u32,
    pub h_scroll: u16,
    pub split: bool,
    secondary_scroll: u32,
    projection: MermaidPanelProjection,
}

impl MermaidPanelContent {
    pub fn new(item_id: String) -> Self {
        Self {
            item_id,
            scroll: 0,
            h_scroll: 0,
            split: false,
            secondary_scroll: 0,
            projection: MermaidPanelProjection::default(),
        }
    }
}

impl WindowComponent for MermaidPanelContent {
    fn render_content(&mut self, area: Rect, frame: &mut Frame, ctx: &RenderCtx) -> Vec<HitRegion> {
        let area = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        let item_idx = self
            .item_id
            .strip_prefix("mermaid:")
            .and_then(|value| value.parse::<usize>().ok());
        let Some((source, revision)) = item_idx.and_then(|index| {
            let crate::app::OutputItem::MermaidDiagram { source } = ctx.items.get(index)? else {
                return None;
            };
            Some((source, *ctx.item_revisions.get(index)?))
        }) else {
            super::common::render_placeholder(frame, area, &self.item_id);
            return Vec::new();
        };

        let padding = 2;
        let inner_area = Rect {
            x: area.x + padding,
            y: area.y + 1,
            width: area.width.saturating_sub(padding * 2),
            height: area.height.saturating_sub(2),
        };
        if inner_area.width == 0 || inner_area.height == 0 {
            return Vec::new();
        }

        if self.split {
            let half_width = inner_area.width / 2;
            let source_area = Rect {
                width: half_width,
                ..inner_area
            };
            let diagram_area = Rect {
                x: inner_area.x + half_width,
                width: inner_area.width.saturating_sub(half_width),
                ..inner_area
            };
            self.projection.update(revision, source, diagram_area.width);

            self.scroll = clamp_scroll(
                self.scroll,
                self.projection.source_rows(),
                source_area.height,
            );
            let source_lines =
                self.projection
                    .source_lines
                    .visible_lines(source, self.scroll, source_area.height);
            frame.render_widget(Paragraph::new(source_lines), source_area);

            self.secondary_scroll = clamp_scroll(
                self.secondary_scroll,
                self.projection.diagram_rows(),
                diagram_area.height,
            );
            let diagram_range = visible_range(
                self.projection.diagram_lines.len(),
                self.secondary_scroll,
                diagram_area.height,
            );
            frame.render_widget(
                Paragraph::new(self.projection.diagram_lines[diagram_range].to_vec()),
                diagram_area,
            );

            let divider_x = inner_area.x + half_width;
            for y in inner_area.y..inner_area.y + inner_area.height {
                if let Some(cell) = frame
                    .buffer_mut()
                    .cell_mut(ratatui::layout::Position { x: divider_x, y })
                {
                    cell.set_char('│');
                }
            }
        } else {
            self.projection.update(revision, source, inner_area.width);
            self.scroll = clamp_scroll(
                self.scroll,
                self.projection.diagram_rows(),
                inner_area.height,
            );
            self.secondary_scroll = self.secondary_scroll.min(
                self.projection
                    .content_width
                    .saturating_sub(inner_area.width as u32),
            );
            let lines = self.projection.visible_diagram(
                self.scroll,
                inner_area.height,
                self.secondary_scroll,
                inner_area.width,
            );
            frame.render_widget(Paragraph::new(lines), inner_area);
        }
        Vec::new()
    }

    fn handle_event(&mut self, event: &WmEvent, _ctx: &mut EventCtx) -> WmEventResult {
        match event {
            WmEvent::Mouse(event) if event.kind == MouseEventKind::ScrollLeft => {
                self.secondary_scroll = self.secondary_scroll.saturating_sub(3);
                WmEventResult::Consumed(Vec::new())
            }
            WmEvent::Mouse(event) if event.kind == MouseEventKind::ScrollRight => {
                self.secondary_scroll = self.secondary_scroll.saturating_add(3);
                WmEventResult::Consumed(Vec::new())
            }
            _ => WmEventResult::Ignored,
        }
    }

    fn preferred_size(&self, _viewport: Rect) -> SizeHint {
        SizeHint {
            min: (40, 10),
            max: None,
            preferred: (80, 30),
        }
    }

    fn sync_state(&mut self, scroll: u32, h_scroll: u16, split: bool) {
        self.scroll = scroll;
        self.h_scroll = h_scroll;
        self.split = split;
    }

    fn extract_state(&self) -> (u32, u16, bool) {
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

fn clamp_scroll(scroll: u32, total_rows: u32, viewport_height: u16) -> u32 {
    scroll.min(total_rows.saturating_sub(viewport_height as u32))
}

fn visible_range(total_rows: usize, scroll: u32, viewport_height: u16) -> Range<usize> {
    let start = usize::try_from(scroll)
        .unwrap_or(usize::MAX)
        .min(total_rows);
    let end = start
        .saturating_add(viewport_height as usize)
        .min(total_rows);
    start..end
}

fn decimal_digits(value: usize) -> usize {
    value.max(1).ilog10() as usize + 1
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyModifiers, MouseEvent};

    fn flowchart(nodes: usize) -> String {
        let mut source = String::from("flowchart TD\n");
        for index in 0..nodes {
            source.push_str(&format!("  N{index}[node {index}] --> N{}\n", index + 1));
        }
        source
    }

    fn mouse(kind: MouseEventKind) -> WmEvent {
        WmEvent::Mouse(MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn stable_source_revision_reuses_the_complete_diagram() {
        let source = flowchart(80);
        let revision = crate::app::OutputRevision {
            id: 7,
            source_generation: 11,
            ..Default::default()
        };
        let mut projection = MermaidPanelProjection::default();
        projection.update(revision, &source, 100);
        assert_eq!(projection.rebuild_count, 1);
        for _ in 0..100 {
            projection.update(revision, &source, 100);
        }
        assert_eq!(projection.rebuild_count, 1);

        projection.update(revision, &source, 80);
        assert_eq!(projection.rebuild_count, 2);
        projection.update(
            crate::app::OutputRevision {
                source_generation: 12,
                ..revision
            },
            &source,
            80,
        );
        assert_eq!(projection.rebuild_count, 3);
    }

    #[test]
    fn diagram_projection_materializes_only_top_middle_and_tail_viewports() {
        let source = flowchart(40);
        let revision = crate::app::OutputRevision {
            id: 1,
            source_generation: 1,
            ..Default::default()
        };
        let mut projection = MermaidPanelProjection::default();
        projection.update(revision, &source, 90);
        let height = 7;
        let last = projection.diagram_rows().saturating_sub(height as u32);
        for scroll in [0, projection.diagram_rows() / 2, last] {
            let range = visible_range(projection.diagram_lines.len(), scroll, height);
            let actual = projection.visible_diagram(scroll, height, 0, 90);
            assert_eq!(actual.len(), range.len());
            for (actual, expected) in actual.iter().zip(&projection.diagram_lines[range]) {
                let actual_text: String = actual
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                let expected_text: String = expected
                    .spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect();
                assert_eq!(
                    actual_text,
                    crate::width::trim_display_offset(&expected_text, 0, 90)
                );
            }
        }
    }

    #[test]
    fn source_projection_numbers_only_visible_rows_and_expands_digit_column() {
        let source = (0..1_005)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut index = SourceLineIndex::default();
        index.rebuild(&source);
        let lines = index.visible_lines(&source, 1_000, 5);
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0].spans[0].content.as_ref(), "1001 ");
        assert_eq!(lines[4].spans[0].content.as_ref(), "1005 ");
    }

    #[test]
    fn private_secondary_scroll_exceeds_u16_without_changing_shell_state() {
        let mut content = MermaidPanelContent::new("mermaid:0".into());
        let mut scroll = 0;
        let mut shell_h_scroll = 0;
        let mut ctx = EventCtx {
            scroll: &mut scroll,
            h_scroll: &mut shell_h_scroll,
        };
        for _ in 0..22_000 {
            assert!(matches!(
                content.handle_event(&mouse(MouseEventKind::ScrollRight), &mut ctx),
                WmEventResult::Consumed(_)
            ));
        }
        assert_eq!(content.secondary_scroll, 66_000);
        assert!(content.secondary_scroll > u16::MAX as u32);
        assert_eq!(*ctx.h_scroll, 0);
    }

    #[test]
    #[ignore = "large release-mode Mermaid projection baseline"]
    fn baseline_large_mermaid_projection_rebuild_and_stable_frames() {
        const STABLE_FRAMES: u32 = 1_000;
        const REBUILD_FRAMES: u32 = 16;

        let source = flowchart(500);
        let revision = crate::app::OutputRevision {
            id: 1,
            source_generation: 1,
            ..Default::default()
        };
        let mut projection = MermaidPanelProjection::default();

        let started = std::time::Instant::now();
        projection.update(revision, std::hint::black_box(&source), 100);
        let cold = started.elapsed();
        assert!(projection.diagram_rows() > 500);

        let started = std::time::Instant::now();
        for _ in 0..STABLE_FRAMES {
            projection.update(revision, std::hint::black_box(&source), 100);
            std::hint::black_box(projection.visible_diagram(200, 40, 7, 100));
            std::hint::black_box(projection.source_lines.visible_lines(&source, 200, 40));
        }
        let stable = started.elapsed();

        let started = std::time::Instant::now();
        for generation in 2..REBUILD_FRAMES + 2 {
            projection.update(
                crate::app::OutputRevision {
                    source_generation: u64::from(generation),
                    ..revision
                },
                std::hint::black_box(&source),
                100,
            );
            std::hint::black_box(projection.visible_diagram(200, 40, 7, 100));
        }
        let rebuild = started.elapsed();

        assert_eq!(projection.rebuild_count, u64::from(REBUILD_FRAMES) + 1);
        eprintln!(
            "Mermaid projection baseline: nodes=500 rows={} cold_ms={:.3} stable_us_per_frame={:.3} rebuild_ms_per_frame={:.3}",
            projection.diagram_rows(),
            cold.as_secs_f64() * 1_000.0,
            stable.as_secs_f64() * 1_000_000.0 / f64::from(STABLE_FRAMES),
            rebuild.as_secs_f64() * 1_000.0 / f64::from(REBUILD_FRAMES),
        );
    }
}
