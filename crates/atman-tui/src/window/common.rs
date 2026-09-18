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
    let text_lines = lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>();
    let source = text_lines.join("\n");
    let domain = crate::selection::SelectionDomain::Window {
        window_id,
        surface: surface.to_owned(),
    };
    let mut isolated_atoms = Vec::new();
    let start = usize::try_from(scroll).unwrap_or(usize::MAX);
    let end = start
        .saturating_add(usize::from(area.height))
        .min(text_lines.len());
    let mut source_offset = text_lines[..start.min(text_lines.len())]
        .iter()
        .map(String::len)
        .fold(0usize, |offset, len| {
            offset.saturating_add(len).saturating_add(1)
        });
    for (offset, text) in text_lines[start..end].iter().enumerate() {
        let row = u32::from(area.y).saturating_add(u32::try_from(offset).unwrap_or(u32::MAX));
        let (atoms, _) = crate::selection::atom_runs(
            u16::try_from(row).unwrap_or(u16::MAX),
            area.x,
            text,
            source_offset,
        );
        isolated_atoms.extend(atoms.into_iter().map(|atom| {
            crate::selection::VisibleIsolatedAtom::Raw {
                domain: domain.clone(),
                atom: crate::selection::VisibleAtom {
                    screen_row: row,
                    cols: atom.cols,
                    cell_width: atom.cell_width,
                    source: atom.source,
                },
            }
        }));
        source_offset = source_offset.saturating_add(text.len()).saturating_add(1);
    }
    crate::selection::VisibleSelectionProjection {
        structure_revision: revision.layout,
        surfaces: vec![crate::selection::VisibleSurface {
            item_index: usize::MAX,
            revision,
            start_row: u32::from(area.y),
            end_row: u32::from(area.y.saturating_add(area.height)),
            source: std::sync::Arc::new(crate::selection::ItemSemanticSource {
                owner_revision: revision,
                source: source.clone(),
                isolated: vec![crate::selection::IsolatedSource {
                    domain,
                    fragments: vec![crate::selection::CopyFragment::plain_text(source)],
                }],
                ..Default::default()
            }),
            prose_atoms: Vec::new(),
            code_atoms: Vec::new(),
            isolated_atoms,
        }],
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
