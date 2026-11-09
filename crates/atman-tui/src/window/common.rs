use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use atman_runtime::TaskKind;

pub(crate) fn detail_command_lines(command: &str, width: usize) -> Vec<Line<'static>> {
    let t = crate::theme::theme();
    let prefix_style = Style::default().fg(t.meta_fg.into()).bg(t.code_bg.into());
    let command_style = Style::default().fg(t.tinted_fg.into()).bg(t.code_bg.into());
    crate::output::wrap_with_prefix(command, width, " command $ ", "           ")
        .into_iter()
        .map(|row| {
            crate::output::line_with_right_pad(
                &row.prefix,
                &row.body,
                width,
                prefix_style,
                command_style,
            )
        })
        .collect()
}

pub(crate) fn detail_section_label(label: &str, width: usize) -> Line<'static> {
    let t = crate::theme::theme();
    let text = format!(" {label}");
    let fill = width.saturating_sub(crate::width::width(text.as_str()));
    let style = Style::default().fg(t.meta_fg.into()).bg(t.code_bg.into());
    Line::from(vec![
        Span::styled(text, style),
        Span::styled(" ".repeat(fill), style),
    ])
}

pub(crate) fn window_text_projection(
    window_id: crate::wm::WindowId,
    surface: &str,
    revision: crate::app::OutputRevision,
    lines: &[Line<'static>],
    area: Rect,
    scroll: u32,
) -> crate::selection::VisibleSelectionProjection {
    WindowTextSource::new(window_id, surface, revision, lines).visible(area, scroll)
}

pub(crate) struct WindowTextSource {
    source: std::sync::Arc<crate::selection::ItemSemanticSource>,
    lines: Vec<(std::ops::Range<usize>, usize)>,
}

impl WindowTextSource {
    pub(crate) fn new(
        window_id: crate::wm::WindowId,
        surface: &str,
        revision: crate::app::OutputRevision,
        lines: &[Line<'static>],
    ) -> Self {
        let mut source = String::new();
        let mut ranges = Vec::with_capacity(lines.len());
        for line in lines {
            if !ranges.is_empty() {
                source.push('\n');
            }
            let start = source.len();
            for span in &line.spans {
                source.push_str(&span.content);
            }
            ranges.push(start..source.len());
        }
        let mut graphemes = crate::selection::SourceGraphemeCursor::new(&source);
        let lines = ranges
            .into_iter()
            .map(|range| {
                let start = graphemes.at(range.start);
                (range, start)
            })
            .collect();
        Self {
            lines,
            source: std::sync::Arc::new(crate::selection::ItemSemanticSource {
                owner_revision: revision,
                source: source.clone(),
                isolated: vec![crate::selection::IsolatedSource {
                    domain: crate::selection::SelectionDomain::Window {
                        window_id,
                        surface: surface.to_owned(),
                    },
                    fragments: vec![crate::selection::CopyFragment::plain_text(source)],
                }],
                ..Default::default()
            }),
        }
    }

    pub(crate) fn visible(
        &self,
        area: Rect,
        scroll: u32,
    ) -> crate::selection::VisibleSelectionProjection {
        let start = usize::try_from(scroll)
            .unwrap_or(usize::MAX)
            .min(self.lines.len());
        let end = start
            .saturating_add(usize::from(area.height))
            .min(self.lines.len());
        let domain = &self.source.isolated[0].domain;
        let mut isolated_atoms = Vec::new();
        for (offset, (range, grapheme_start)) in self.lines[start..end].iter().enumerate() {
            let row = u32::from(area.y).saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
            let text = crate::width::truncate_plain(
                &self.source.source[range.clone()],
                usize::from(area.width),
            );
            let mut graphemes = crate::selection::SourceGraphemeCursor::new(&text);
            let (atoms, _) = crate::selection::atom_runs(
                u16::try_from(row).unwrap_or(u16::MAX),
                area.x,
                &text,
                range.start,
            );
            for mut atom in atoms {
                if atom.cell_width == 0 || atom.cols.start >= area.right() {
                    continue;
                }
                atom.cols.end = atom.cols.end.min(area.right());
                isolated_atoms.push(crate::selection::VisibleIsolatedAtom::Raw {
                    domain: domain.clone(),
                    grapheme_start: grapheme_start + graphemes.at(atom.source.start - range.start),
                    atom: crate::selection::VisibleAtom {
                        screen_row: row,
                        cols: atom.cols,
                        cell_width: atom.cell_width,
                        source: atom.source,
                    },
                });
            }
        }
        let revision = self.source.owner_revision;
        crate::selection::VisibleSelectionProjection {
            structure_revision: revision.layout,
            surfaces: vec![crate::selection::VisibleSurface {
                item_index: usize::MAX,
                revision,
                start_row: u32::from(area.y),
                end_row: u32::from(area.bottom()),
                source: self.source.clone(),
                prose_atoms: Vec::new(),
                code_atoms: Vec::new(),
                isolated_atoms,
            }],
        }
    }

    pub(crate) fn visible_inspector(
        &self,
        area: Rect,
        scroll: u32,
        inspector: Option<&crate::output::InspectorViewport>,
    ) -> crate::selection::VisibleSelectionProjection {
        let mut projection = self.visible(area, scroll);
        let Some(view) = inspector else {
            return projection;
        };
        let columns =
            area.x.saturating_add(view.columns.start)..area.x.saturating_add(view.columns.end);
        for surface in &mut projection.surfaces {
            surface.isolated_atoms.retain(|atom| match atom {
                crate::selection::VisibleIsolatedAtom::Raw { atom, .. } => {
                    let row = atom
                        .screen_row
                        .saturating_sub(u32::from(area.y))
                        .saturating_add(scroll);
                    !view.mask_rows.contains(&row)
                        || atom.cols.end <= columns.start
                        || atom.cols.start >= columns.end
                }
                _ => true,
            });
        }
        // Header and body are at most two contiguous runs of prepared source rows.
        let mut index = 0;
        while index < view.rows.len() {
            let (source, displayed) = view.rows[index];
            let mut end = index + 1;
            while end < view.rows.len() && view.rows[end].0 == source + (end - index) as u32 {
                end += 1;
            }
            let y = u32::from(area.y).saturating_add(displayed.saturating_sub(scroll));
            let mapped = self.visible(
                Rect::new(area.x, y as u16, area.width, (end - index) as u16),
                source,
            );
            if let Some(surface) = projection.surfaces.first_mut() {
                for source in mapped.surfaces {
                    surface
                        .isolated_atoms
                        .extend(source.isolated_atoms.into_iter().filter(|atom| {
                            matches!(atom, crate::selection::VisibleIsolatedAtom::Raw { atom, .. }
                            if atom.cols.start >= columns.start && atom.cols.end <= columns.end)
                        }));
                }
            }
            index = end;
        }
        projection
    }
}
pub(crate) fn render_scrolled_lines(
    f: &mut Frame,
    area: Rect,
    lines: Vec<Line<'static>>,
    scroll: &mut u32,
) {
    let max_scroll = lines
        .len()
        .saturating_sub(area.height as usize)
        .min(u32::MAX as usize) as u32;
    *scroll = (*scroll).min(max_scroll);
    let start = *scroll as usize;
    let end = start.saturating_add(area.height as usize).min(lines.len());
    f.render_widget(Paragraph::new(lines[start.min(end)..end].to_vec()), area);
}

pub(crate) fn render_placeholder(f: &mut Frame, area: Rect, title: &str) {
    let t = crate::theme::theme();
    let msg = format!("no data: {title}");
    let total_pad = area.width as usize;
    let left = total_pad.saturating_sub(msg.len()) / 2;
    let top = area.height as usize / 2;
    let mut lines: Vec<Line> = vec![Line::from(""); top];
    lines.push(Line::from(vec![
        Span::styled(" ".repeat(left), Style::default()),
        Span::styled(msg, Style::default().fg(t.subtle_fg.into())),
    ]));
    f.render_widget(Paragraph::new(lines), area);
}

pub(crate) fn render_task_meta(
    f: &mut Frame,
    area: Rect,
    kind: TaskKind,
    snap: &atman_runtime::TaskSnapshot,
    scroll: &mut u32,
) {
    let t = crate::theme::theme();
    let header = Line::from(vec![
        Span::styled(
            format!(" {} ", status_icon(snap.status)),
            Style::default().fg(status_color(snap.status)),
        ),
        Span::styled(snap.label.clone(), Style::default().fg(t.tinted_fg.into())),
        Span::raw(" "),
        Span::styled(
            snap.status.display_label(),
            Style::default().fg(t.subtle_fg.into()),
        ),
    ]);
    let header_area = Rect { height: 1, ..area };
    f.render_widget(Paragraph::new(header), header_area);

    let body_area = Rect {
        y: area.y + 2,
        height: area.height.saturating_sub(2),
        ..area
    };
    if body_area.height == 0 {
        return;
    }

    let lines: Vec<Line> = vec![
        Line::from(vec![
            Span::styled("kind   ", Style::default().fg(t.subtle_fg.into())),
            Span::styled(kind.label(), Style::default().fg(t.tinted_fg.into())),
        ]),
        Line::from(vec![
            Span::styled("handle ", Style::default().fg(t.subtle_fg.into())),
            Span::styled(
                snap.source_handle.clone(),
                Style::default().fg(t.tinted_fg.into()),
            ),
        ]),
        Line::from(vec![
            Span::styled("elapsed", Style::default().fg(t.subtle_fg.into())),
            Span::raw(" "),
            Span::styled(
                format_elapsed(snap.elapsed_ms()),
                Style::default().fg(t.tinted_fg.into()),
            ),
        ]),
    ];
    let mut lines = lines;
    if let Some(command) = snap.command.as_deref() {
        lines.push(Line::from(""));
        lines.extend(detail_command_lines(command, body_area.width as usize));
    }
    render_scrolled_lines(f, body_area, lines, scroll);
}

pub(crate) fn status_icon(status: atman_runtime::TaskStatus) -> &'static str {
    match status {
        atman_runtime::TaskStatus::Running => "◐",
        atman_runtime::TaskStatus::Killing => "◑",
        atman_runtime::TaskStatus::Ok => "✓",
        atman_runtime::TaskStatus::Err => "✗",
        atman_runtime::TaskStatus::Killed => "⊘",
    }
}

pub(crate) fn status_color(status: atman_runtime::TaskStatus) -> Color {
    let t = crate::theme::theme();
    match status {
        atman_runtime::TaskStatus::Running => t.accent.into(),
        atman_runtime::TaskStatus::Killing => t.warn.into(),
        atman_runtime::TaskStatus::Ok => t.success.into(),
        atman_runtime::TaskStatus::Err => t.error.into(),
        atman_runtime::TaskStatus::Killed => t.subtle_fg.into(),
    }
}

pub(crate) fn format_elapsed(ms: u64) -> String {
    let s = ms / 1000;
    let raw = if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}:{:02}", s / 60, s % 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s % 3600) / 60)
    };
    format!("{:>5}", raw)
}

#[cfg(test)]
mod text_tests {
    use super::*;
    use crate::selection::{VisibleIsolatedAtom, VisibleSelectionProjection};
    use unicode_segmentation::UnicodeSegmentation;

    #[test]
    fn sticky_inspector_selection_keeps_original_grapheme_coordinates() {
        let lines: Vec<_> = (0..80)
            .map(|row| Line::from(format!("tree{row:04}详情{row:04}    ")))
            .collect();
        let text = WindowTextSource::new(
            crate::wm::WindowId(3),
            "workflow",
            Default::default(),
            &lines,
        );
        let inspector = crate::output::Inspector {
            key: ("node".into(), crate::workflow::Section::Arguments),
            top: 3,
            bottom: 80,
            rows: 30,
            header_rows: 3,
            columns: 8..20,
            regions: std::sync::Arc::from([]),
        };
        let mut state = crate::output::InspectorScroll::default();
        let area = Rect::new(5, 4, 20, 10);
        let view = inspector.viewport(&mut state, 0, 0, 20, 10).unwrap();
        assert!(state.scroll(&view, 9));
        let view = inspector.viewport(&mut state, 0, 0, 20, 10).unwrap();
        let mapped = text.visible_inspector(area, 20, Some(&view));
        let surface = &mapped.surfaces[0];
        assert!(std::sync::Arc::ptr_eq(&surface.source, &text.source));
        let raw: Vec<_> = surface
            .isolated_atoms
            .iter()
            .filter_map(|atom| match atom {
                VisibleIsolatedAtom::Raw {
                    atom,
                    grapheme_start,
                    ..
                } => Some((atom, grapheme_start)),
                _ => None,
            })
            .collect();
        for (atom, start) in &raw {
            assert_eq!(
                **start,
                text.source.source[..atom.source.start]
                    .graphemes(true)
                    .count()
            );
        }
        for &(source, displayed) in &view.rows {
            let y = u32::from(area.y) + displayed - 20;
            let selected = raw
                .iter()
                .find(|(atom, _)| atom.screen_row == y && atom.cols.start == area.x + 8)
                .unwrap();
            assert_eq!(
                selected.0.source.start,
                text.lines[source as usize].0.start + 8
            );
            let left = raw
                .iter()
                .find(|(atom, _)| atom.screen_row == y && atom.cols.start == area.x)
                .unwrap();
            assert_eq!(left.0.source.start, text.lines[displayed as usize].0.start);
        }
    }

    #[test]
    fn window_text_viewport_work_does_not_visit_long_hidden_line_tails() {
        let lines = vec![Line::raw("汉a".repeat(50_000)); 3];
        let prepared =
            WindowTextSource::new(crate::wm::WindowId(3), "test", Default::default(), &lines);
        crate::width::take_grapheme_visits();
        let visible = prepared.visible(Rect::new(0, 0, 20, 2), 1);
        assert!(crate::width::take_grapheme_visits() <= 40);
        assert!(visible.visual_points().len() <= 40);
    }

    #[test]
    fn window_text_source_reuses_source_and_preserves_unicode_coordinates() {
        let mut lines = vec![Line::from("prefix 汉 e\u{301} 👩‍💻"); 512];
        lines.extend([Line::from("a\r"), Line::from("汉e\u{301}👩‍💻 abc")]);
        let prepared =
            WindowTextSource::new(crate::wm::WindowId(3), "test", Default::default(), &lines);
        let area = Rect::new(12, 6, 13, 2);
        for scroll in [0, 511, 512, 513, 514, u32::MAX] {
            let projection = prepared.visible(area, scroll);
            assert!(std::sync::Arc::ptr_eq(
                &projection.surfaces[0].source,
                &prepared.source
            ));
            for visible in &projection.surfaces[0].isolated_atoms {
                let VisibleIsolatedAtom::Raw {
                    atom,
                    grapheme_start,
                    ..
                } = visible
                else {
                    unreachable!()
                };
                assert!(atom.cols.start >= area.x && atom.cols.end <= area.right());
                assert_eq!(
                    *grapheme_start,
                    prepared.source.source[..atom.source.start]
                        .graphemes(true)
                        .count()
                );
                for col in (atom.cols.start..atom.cols.end).step_by(usize::from(atom.cell_width)) {
                    let expected = atom.source_at(&prepared.source.source, col).unwrap();
                    let actual = projection.isolated_point_at(atom.screen_row, col).unwrap();
                    assert_eq!(
                        actual.grapheme,
                        prepared.source.source[..expected].graphemes(true).count()
                    );
                }
            }
            assert!(
                projection
                    .visual_points()
                    .iter()
                    .all(|point| point.col < area.right())
            );
        }

        // Raw coordinates are complete: mapping the viewport never needs the full source again.
        let mut projection = prepared.visible(area, 513);
        let expected = projection.visual_points();
        assert!(!expected.is_empty());
        let owner = std::sync::Arc::make_mut(&mut projection.surfaces[0].source);
        owner.source.clear();
        for point in expected {
            let actual = projection.isolated_point_at(point.row, point.col).unwrap();
            assert_eq!(actual, point.point);
        }
        let points = projection.visual_points();
        assert!(!points.is_empty());
        assert_eq!(
            prepared.visible(area, u32::MAX).surfaces[0]
                .isolated_atoms
                .len(),
            0
        );
        let empty = WindowTextSource::new(crate::wm::WindowId(3), "test", Default::default(), &[]);
        let _: VisibleSelectionProjection = empty.visible(area, u32::MAX);
    }
}
