use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::OutputItem;
use crate::wm::WmHitmap;
use crate::wm::component::{
    EventCtx, HitRegion, HitTarget, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};

const OVERSCAN_ROWS: usize = 8;

fn history_render_range(total: usize, scroll: u32, height: usize) -> (usize, usize, usize) {
    let scroll = (scroll as usize).min(total.saturating_sub(height));
    let start = scroll.saturating_sub(OVERSCAN_ROWS);
    let end = scroll
        .saturating_add(height)
        .saturating_add(OVERSCAN_ROWS)
        .min(total);
    (start, end, scroll)
}

#[derive(Default)]
pub struct HistoryPanelContent {
    pub scroll: u32,
    ordered_revision: Option<u64>,
    completed: Vec<usize>,
}

impl HistoryPanelContent {
    fn update_order(&mut self, snapshots: &[atman_runtime::TaskSnapshot], revision: u64) {
        if self.ordered_revision == Some(revision) {
            return;
        }
        self.completed.clear();
        self.completed.extend(
            snapshots
                .iter()
                .enumerate()
                .filter_map(|(index, snapshot)| (!snapshot.is_running()).then_some(index)),
        );
        self.completed
            .sort_by_key(|&index| std::cmp::Reverse(snapshots[index].ended_at));
        self.ordered_revision = Some(revision);
    }
}

impl WindowComponent for HistoryPanelContent {
    fn render_content(&mut self, area: Rect, frame: &mut Frame, ctx: &RenderCtx) -> Vec<HitRegion> {
        let area = Rect::new(
            area.x + 1,
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        self.update_order(ctx.snapshots, ctx.task_snapshots_revision);
        let mut hitmap = WmHitmap::default();
        render_history_content(
            frame,
            area,
            ctx.snapshots,
            &self.completed,
            ctx.items,
            ctx.handle_index,
            &mut hitmap,
            ctx.hovered_history_row,
            self.scroll,
            ctx.window_id,
        );
        hitmap
            .history_row_rects
            .into_iter()
            .map(|(_, handle, rect)| HitRegion {
                target: HitTarget::HistoryRow(handle),
                rect,
            })
            .collect()
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

#[allow(clippy::too_many_arguments)]
fn render_history_content(
    f: &mut Frame,
    area: Rect,
    snapshots: &[atman_runtime::TaskSnapshot],
    completed: &[usize],
    items: &[OutputItem],
    handle_index: &std::collections::HashMap<String, usize>,
    hitmap: &mut WmHitmap,
    hovered_row: &Option<String>,
    scroll: u32,
    window_id: crate::wm::WindowId,
) {
    let t = crate::theme::theme();
    let bar_color: Color = t.subtle_fg.into();
    let content_bg: Color = t.code_bg.into();
    let hover_bg: Color = t.code_bg.lerp(t.work_hover_bg, 0.3);
    let visible_height = area.height as usize;
    let (render_start, render_end, scroll) =
        history_render_range(completed.len(), scroll, visible_height);

    let mut lines: Vec<Line> = Vec::new();
    for &snapshot_index in &completed[render_start..render_end] {
        let snap = &snapshots[snapshot_index];
        let i = lines.len() + render_start;
        let icon = super::common::status_icon(snap.status);
        let elapsed = super::common::format_elapsed(snap.elapsed_ms());
        let st_color = super::common::status_color(snap.status);
        let visible_i = i
            .checked_sub(scroll)
            .filter(|offset| *offset < visible_height);
        let is_hovered = hovered_row.as_deref() == Some(&snap.source_handle);
        if let Some(visible_i) = visible_i {
            hitmap.history_row_rects.push((
                window_id,
                snap.source_handle.clone(),
                Rect {
                    x: area.x,
                    y: area.y + visible_i as u16,
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
        let kind_icon = crate::wm::WindowContent::Task {
            handle: snap.source_handle.clone(),
            kind: snap.kind,
        }
        .icon();
        let started = format_started_at(snap);
        let summary = task_summary_line(
            snap,
            handle_index
                .get(&snap.source_handle)
                .and_then(|&index| items.get(index)),
        );
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
    if completed.is_empty() {
        let msg = "no completed tasks";
        let total_pad = area.width as usize;
        let left = total_pad.saturating_sub(msg.len()) / 2;
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(left), Style::default()),
            Span::styled(msg, Style::default().fg(t.subtle_fg.into())),
        ]));
    }
    let local_scroll = scroll.saturating_sub(render_start) as u16;
    f.render_widget(Paragraph::new(lines).scroll((local_scroll, 0)), area);
}

fn task_summary_line(snap: &atman_runtime::TaskSnapshot, item: Option<&OutputItem>) -> String {
    use atman_runtime::TaskKind;
    match snap.kind {
        TaskKind::Bash => {
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

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use atman_runtime::{TaskId, TaskKind, TaskSnapshot, TaskStatus};

    use super::{HistoryPanelContent, OVERSCAN_ROWS, history_render_range};

    fn completed(handle: &str, ended_at: Instant) -> TaskSnapshot {
        TaskSnapshot {
            id: TaskId::now(),
            kind: TaskKind::Bash,
            label: handle.to_owned(),
            command: None,
            status: TaskStatus::Ok,
            started_at: ended_at - Duration::from_secs(1),
            ended_at: Some(ended_at),
            source_handle: handle.to_owned(),
            session_id: "session".into(),
            workspace_id: None,
            flow_run_id: None,
            termination: None,
        }
    }

    #[test]
    fn completed_order_rebuilds_only_for_a_new_snapshot_revision() {
        let now = Instant::now();
        let mut snapshots = vec![
            completed("older", now - Duration::from_secs(2)),
            completed("newer", now),
        ];
        let mut panel = HistoryPanelContent::default();

        panel.update_order(&snapshots, 1);
        assert_eq!(panel.completed, vec![1, 0]);

        snapshots[0].ended_at = Some(now + Duration::from_secs(2));
        panel.update_order(&snapshots, 1);
        assert_eq!(panel.completed, vec![1, 0]);

        panel.update_order(&snapshots, 2);
        assert_eq!(panel.completed, vec![0, 1]);
    }

    #[test]
    fn history_window_stays_bounded_past_u16_indices() {
        let height = 25;
        let (start, end, scroll) = history_render_range(100_000, 70_000, height);

        assert_eq!(scroll, 70_000);
        assert_eq!(start, 70_000 - OVERSCAN_ROWS);
        assert_eq!(end, 70_000 + height + OVERSCAN_ROWS);
        assert!(end - start <= height + OVERSCAN_ROWS * 2);
    }
}
