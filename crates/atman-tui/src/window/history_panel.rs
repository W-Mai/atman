use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::OutputItem;
use crate::wm::component::{
    EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};
use crate::wm::floating::WmHitmap;

pub struct HistoryPanelContent {
    pub scroll: u16,
}

impl WindowComponent for HistoryPanelContent {
    fn render_content(&mut self, area: Rect, frame: &mut Frame, ctx: &RenderCtx) -> Vec<HitRegion> {
        let area = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        let mut dummy = WmHitmap::default();
        render_history_content(
            frame,
            area,
            ctx.snapshots,
            ctx.items,
            &mut dummy,
            &None,
            self.scroll,
        );
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
            preferred: (70, 30),
        }
    }
}

fn render_history_content(
    f: &mut Frame,
    area: Rect,
    snapshots: &[atman_runtime::TaskSnapshot],
    items: &[OutputItem],
    hitmap: &mut WmHitmap,
    hovered_row: &Option<String>,
    scroll: u16,
) {
    let t = crate::theme::theme();
    let bar_color: Color = t.subtle_fg.into();
    let content_bg: Color = t.code_bg.into();
    let hover_bg: Color = t.code_bg.lerp(t.highlight_bg, 0.3);
    let mut done: Vec<&atman_runtime::TaskSnapshot> =
        snapshots.iter().filter(|s| !s.is_running()).collect();
    done.sort_by_key(|b| std::cmp::Reverse(b.ended_at));

    let visible_height = area.height;
    let max_scroll = done.len().saturating_sub(visible_height as usize) as u16;
    let scroll = scroll.min(max_scroll);

    let mut lines: Vec<Line> = Vec::new();
    for (i, snap) in done.iter().enumerate() {
        let icon = status_icon(snap.status);
        let elapsed = format_elapsed(snap.elapsed_ms());
        let st_color = status_color(snap.status);
        let visible_i = (i as u16).saturating_sub(scroll);
        let row_y = area.y + visible_i;
        let is_hovered = hovered_row.as_deref() == Some(&snap.source_handle);
        if visible_i < visible_height {
            hitmap.history_row_rects.push((
                snap.source_handle.clone(),
                Rect {
                    x: area.x,
                    y: row_y,
                    width: area.width,
                    height: 1,
                },
            ));
        }
        let row_bg = if is_hovered { hover_bg } else { content_bg };
        let bar = if is_hovered { "▌" } else { "▎" };
        let label_fg = if is_hovered {
            t.tinted_fg.into()
        } else {
            t.subtle_fg.into()
        };
        let time_fg: Color = t.meta_fg.into();
        let kind_icon = crate::wm::floating::task_kind_icon(snap.kind);
        let started = format_started_at(snap);
        let summary = task_summary_line(snap, items);
        let bar_str = format!("{bar} ");
        let kind_str = format!("{kind_icon} ");
        let icon_str = format!("{icon} ");
        let prefix_w: u16 = crate::width::width(bar_str.as_str()) as u16
            + crate::width::width(kind_str.as_str()) as u16
            + crate::width::width(icon_str.as_str()) as u16;
        let suffix_w: u16 = crate::width::width(started.as_str()) as u16
            + 1
            + crate::width::width(elapsed.as_str()) as u16
            + 1;
        let content_max = area.width.saturating_sub(prefix_w + suffix_w + 1) as usize;
        let label_text = if summary.is_empty() {
            snap.label.clone()
        } else {
            format!("{} · {}", snap.label, summary)
        };
        let label = crate::width::truncate(&label_text, content_max);
        let label_w = crate::width::width(label.as_str()) as u16;
        let pad = area
            .width
            .saturating_sub(prefix_w)
            .saturating_sub(label_w)
            .saturating_sub(suffix_w)
            .max(1);
        lines.push(Line::from(vec![
            Span::styled(bar_str, Style::default().fg(bar_color).bg(row_bg)),
            Span::styled(kind_str, Style::default().fg(st_color).bg(row_bg)),
            Span::styled(icon_str, Style::default().fg(st_color).bg(row_bg)),
            Span::styled(label, Style::default().fg(label_fg).bg(row_bg)),
            Span::styled(" ".repeat(pad as usize), Style::default().bg(row_bg)),
            Span::styled(started, Style::default().fg(time_fg).bg(row_bg)),
            Span::styled(" ", Style::default().bg(row_bg)),
            Span::styled(elapsed, Style::default().fg(label_fg).bg(row_bg)),
            Span::styled(" ", Style::default().bg(row_bg)),
        ]));
    }
    if done.is_empty() {
        let msg = "no completed tasks";
        let total_pad = area.width as usize;
        let left = total_pad.saturating_sub(msg.len()) / 2;
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(left), Style::default()),
            Span::styled(msg, Style::default().fg(t.subtle_fg.into())),
        ]));
    }
    f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
}

fn task_summary_line(snap: &atman_runtime::TaskSnapshot, items: &[OutputItem]) -> String {
    use atman_runtime::TaskKind;
    match snap.kind {
        TaskKind::Bash => {
            let item = items.iter().rev().find(|it| match it {
                OutputItem::Bash { handle, .. } => *handle == snap.source_handle,
                _ => false,
            });
            if let Some(OutputItem::Bash { output, .. }) = item {
                for line in output.lines().rev() {
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        return trimmed.to_string();
                    }
                }
            }
            String::new()
        }
        TaskKind::Terminal => {
            let item = items.iter().rev().find(|it| match it {
                OutputItem::Terminal { handle, .. } => *handle == snap.source_handle,
                _ => false,
            });
            if let Some(OutputItem::Terminal { screen, .. }) = item {
                let cols = screen.cols as usize;
                let rows = screen.rows as usize;
                if cols > 0 && rows > 0 {
                    for r in (0..rows).rev() {
                        let start = r * cols;
                        let end = start + cols;
                        let line: String = screen
                            .cells
                            .get(start..end)
                            .map(|cells| cells.iter().map(|c| c.chars.as_str()).collect::<String>())
                            .unwrap_or_default();
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            return trimmed.to_string();
                        }
                    }
                }
            }
            String::new()
        }
        _ => String::new(),
    }
}

fn format_started_at(snap: &atman_runtime::TaskSnapshot) -> String {
    if let Some(ts) = snap.id.0.get_timestamp() {
        let (secs, _nanos) = ts.to_unix();
        if let Some(dt) = chrono::DateTime::from_timestamp(secs as i64, 0) {
            return dt
                .with_timezone(&chrono::Local)
                .format("%H:%M:%S")
                .to_string();
        }
    }
    "--:--:--".to_string()
}

fn status_icon(status: atman_runtime::TaskStatus) -> &'static str {
    match status {
        atman_runtime::TaskStatus::Running => "◐",
        atman_runtime::TaskStatus::Killing => "◑",
        atman_runtime::TaskStatus::Ok => "✓",
        atman_runtime::TaskStatus::Err => "✗",
        atman_runtime::TaskStatus::Killed => "⊘",
    }
}

fn status_color(status: atman_runtime::TaskStatus) -> Color {
    let t = crate::theme::theme();
    match status {
        atman_runtime::TaskStatus::Running => t.accent.into(),
        atman_runtime::TaskStatus::Killing => t.warn.into(),
        atman_runtime::TaskStatus::Ok => t.success.into(),
        atman_runtime::TaskStatus::Err => t.error.into(),
        atman_runtime::TaskStatus::Killed => t.subtle_fg.into(),
    }
}

fn format_elapsed(ms: u64) -> String {
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
