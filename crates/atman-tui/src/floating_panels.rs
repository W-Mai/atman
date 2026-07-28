use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use atman_runtime::{TaskKind, TaskSnapshot};

use crate::app::OutputItem;
use crate::task_panel::{ActivityNode, ActivityStatus};

#[derive(Debug, Clone)]
pub struct FloatingPanel {
    pub id: String,
    pub kind: PanelKind,
    pub title: String,
    pub rect: Rect,
    pub z: u32,
    pub maximized: bool,
    pub prev_rect: Option<Rect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelKind {
    Task(TaskKind),
    History,
    Activity,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelBtn {
    Close,
    Maximize,
    Resize,
}

impl PanelKind {
    fn icon(self) -> &'static str {
        match self {
            PanelKind::Task(kind) => task_kind_icon(kind),
            PanelKind::History => "⊞",
            PanelKind::Activity => "▸",
        }
    }
}

pub fn task_kind_icon(kind: TaskKind) -> &'static str {
    match kind {
        TaskKind::Bash => "$",
        TaskKind::Terminal => "▶",
        TaskKind::Flow => "⬡",
        TaskKind::Agent => "✦",
        TaskKind::Subflow => "↳",
        TaskKind::Dispatch => "≡",
    }
}

#[derive(Debug, Default, Clone)]
pub struct FloatingPanels {
    pub panels: Vec<FloatingPanel>,
    pub focused: Option<String>,
    z_counter: u32,
}

impl FloatingPanels {
    pub fn open(&mut self, id: &str, kind: PanelKind, title: &str, canvas: Rect) {
        self.open_with_size(id, kind, title, canvas, 48, 20);
    }

    pub fn open_with_size(
        &mut self,
        id: &str,
        kind: PanelKind,
        title: &str,
        canvas: Rect,
        w: u16,
        h: u16,
    ) {
        if self.panels.iter().any(|p| p.id == id) {
            self.focused = Some(id.to_string());
            self.bring_to_front(id);
            return;
        }
        self.z_counter += 1;
        let offset = (self.panels.len() as u16) * 3;
        let w = w.min(canvas.width.saturating_sub(4));
        let h = h.min(canvas.height.saturating_sub(4));
        let panel = FloatingPanel {
            id: id.to_string(),
            kind,
            title: title.to_string(),
            rect: Rect {
                x: canvas.x + 2 + offset,
                y: canvas.y + 1 + offset,
                width: w,
                height: h,
            },
            z: self.z_counter,
            maximized: false,
            prev_rect: None,
        };
        self.panels.push(panel);
        self.focused = Some(id.to_string());
    }

    pub fn close(&mut self, id: &str) {
        self.panels.retain(|p| p.id != id);
        if self.focused.as_deref() == Some(id) {
            self.focused = self.panels.last().map(|p| p.id.clone());
        }
    }

    pub fn focus(&mut self, id: &str) {
        self.focused = Some(id.to_string());
        self.bring_to_front(id);
    }

    fn bring_to_front(&mut self, id: &str) {
        self.z_counter += 1;
        let z = self.z_counter;
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            p.z = z;
        }
    }

    pub fn toggle_maximize(&mut self, id: &str, canvas: Rect) {
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            if p.maximized {
                if let Some(prev) = p.prev_rect.take() {
                    p.rect = prev;
                }
                p.maximized = false;
            } else {
                p.prev_rect = Some(p.rect);
                p.rect = Rect {
                    x: canvas.x + 1,
                    y: canvas.y + 1,
                    width: canvas.width.saturating_sub(2),
                    height: canvas.height.saturating_sub(2),
                };
                p.maximized = true;
            }
            self.bring_to_front(id);
        }
    }

    pub fn move_panel(&mut self, id: &str, x: u16, y: u16, canvas: Rect) {
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            let x = x.min(canvas.x + canvas.width.saturating_sub(20));
            let y = y.min(canvas.y + canvas.height.saturating_sub(3));
            p.rect.x = x.max(canvas.x);
            p.rect.y = y.max(canvas.y);
        }
    }

    pub fn resize_panel(&mut self, id: &str, w: u16, h: u16, canvas: Rect) {
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            let max_w = canvas.x + canvas.width - p.rect.x;
            let max_h = canvas.y + canvas.height - p.rect.y;
            p.rect.width = w.min(max_w).max(20);
            p.rect.height = h.min(max_h).max(6);
        }
    }

    pub fn hit_test_titlebar(&self, col: u16, row: u16) -> Option<&FloatingPanel> {
        // draggable = panel rect + shadow ring (2 cols left/right, 1 row top/bottom)
        // EXCEPT content area and button hit areas
        self.panels
            .iter()
            .filter(|p| {
                // expanded rect including shadow ring
                let sx0 = p.rect.x.saturating_sub(2);
                let sx1 = p.rect.x + p.rect.width + 1;
                let sy0 = p.rect.y.saturating_sub(1);
                let sy1 = p.rect.y + p.rect.height + 1;
                col >= sx0 && col <= sx1 && row >= sy0 && row <= sy1
            })
            .filter(|p| {
                // exclude content area: starts at y+2 (same as content_area.y)
                let cx0 = p.rect.x + 2;
                let cx1 = p.rect.x + p.rect.width.saturating_sub(2);
                let cy0 = p.rect.y + 2;
                let cy1 = p.rect.y + p.rect.height.saturating_sub(1);
                !(col >= cx0 && col < cx1 && row >= cy0 && row < cy1)
            })
            .max_by_key(|p| p.z)
    }

    pub fn hit_test_close(&self, col: u16, row: u16) -> Option<String> {
        self.panels
            .iter()
            .find(|p| col >= p.rect.x && col < p.rect.x + 3 && row == p.rect.y + 2)
            .map(|p| p.id.clone())
    }

    pub fn hit_test_maximize(&self, col: u16, row: u16) -> Option<String> {
        self.panels
            .iter()
            .find(|p| col >= p.rect.x && col < p.rect.x + 3 && row == p.rect.y + 3)
            .map(|p| p.id.clone())
    }

    pub fn hit_test_resize(&self, col: u16, row: u16) -> Option<String> {
        // 3x3 area around the resize button at (x+w-1, y+h-1)
        self.panels
            .iter()
            .find(|p| {
                let cx = p.rect.x + p.rect.width.saturating_sub(1);
                let cy = p.rect.y + p.rect.height.saturating_sub(1);
                col >= cx.saturating_sub(1)
                    && col <= cx + 1
                    && row >= cy.saturating_sub(1)
                    && row <= cy + 1
            })
            .map(|p| p.id.clone())
    }

    pub fn hit_test_btn(&self, col: u16, row: u16) -> Option<(String, PanelBtn)> {
        if let Some(id) = self.hit_test_close(col, row) {
            return Some((id, PanelBtn::Close));
        }
        if let Some(id) = self.hit_test_maximize(col, row) {
            return Some((id, PanelBtn::Maximize));
        }
        if let Some(id) = self.hit_test_resize(col, row) {
            return Some((id, PanelBtn::Resize));
        }
        None
    }

    pub fn hit_test_panel(&self, col: u16, row: u16) -> Option<&FloatingPanel> {
        self.panels
            .iter()
            .filter(|p| {
                col >= p.rect.x
                    && col < p.rect.x + p.rect.width
                    && row >= p.rect.y
                    && row < p.rect.y + p.rect.height
            })
            .max_by_key(|p| p.z)
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render(
    f: &mut Frame,
    _canvas: Rect,
    panels: &FloatingPanels,
    snapshots: &[TaskSnapshot],
    items: &[OutputItem],
    activity_nodes: &[ActivityNode],
    hovered_btn: &Option<(String, PanelBtn)>,
    hovered_history_row: &Option<String>,
) -> FloatingPanelHitmap {
    let mut all_hitmap = FloatingPanelHitmap::default();
    let mut sorted: Vec<&FloatingPanel> = panels.panels.iter().collect();
    sorted.sort_by_key(|p| p.z);

    let t = crate::theme::theme();
    for panel in &sorted {
        let is_focused = panels.focused.as_deref() == Some(panel.id.as_str());
        let btn_hover = hovered_btn
            .as_ref()
            .and_then(|(id, btn)| if id == &panel.id { Some(*btn) } else { None });
        let panel_bg: Color = if is_focused {
            t.modal_bg.lerp(t.highlight_bg, 0.15)
        } else {
            t.modal_bg.into()
        };

        // panel background
        let buf_area = f.buffer_mut().area;
        let sanitize_rect = Rect {
            x: panel.rect.x.saturating_sub(2),
            y: panel.rect.y.saturating_sub(1),
            width: panel.rect.width + 4,
            height: panel.rect.height + 2,
        }
        .intersection(buf_area);
        crate::sanitize_widget_edges(f, sanitize_rect);
        f.render_widget(Clear, panel.rect);
        f.render_widget(
            Block::default().style(Style::default().bg(panel_bg)),
            panel.rect,
        );

        // title row
        let title_line = Line::from(vec![
            Span::styled(
                format!(" {} ", panel.kind.icon()),
                Style::default().fg(t.heading.into()).bg(panel_bg),
            ),
            Span::styled(
                truncate(&panel.title, panel.rect.width as usize - 6),
                Style::default().fg(t.tinted_fg.into()).bg(panel_bg),
            ),
        ]);
        f.render_widget(
            Paragraph::new(title_line),
            Rect {
                x: panel.rect.x,
                y: panel.rect.y,
                width: panel.rect.width.saturating_sub(2),
                height: 1,
            },
        );

        // buttons
        // ✕ at (x, y+2), ▢ at (x, y+3), ⇲ at (x+w-3, y+h-1)
        if panel.rect.height >= 6 {
            let hover_bg = t.user_msg_bg.into();

            // ✕ close
            let btn_area = Rect {
                x: panel.rect.x,
                y: panel.rect.y + 2,
                width: 3,
                height: 1,
            };
            let (close_fg, close_bg) = if btn_hover == Some(PanelBtn::Close) {
                (t.error.into(), hover_bg)
            } else {
                (t.error.into(), panel_bg)
            };
            f.render_widget(Clear, btn_area);
            f.render_widget(
                Block::default().style(Style::default().bg(close_bg)),
                btn_area,
            );
            f.render_widget(
                Paragraph::new(Line::from(vec![Span::styled(
                    "✕",
                    Style::default().fg(close_fg).bg(close_bg),
                )]))
                .alignment(ratatui::layout::Alignment::Center),
                btn_area,
            );

            // ▢ maximize
            let btn_area = Rect {
                x: panel.rect.x,
                y: panel.rect.y + 3,
                width: 3,
                height: 1,
            };
            let (max_fg, max_bg) = if btn_hover == Some(PanelBtn::Maximize) {
                (t.tinted_fg.into(), hover_bg)
            } else {
                (t.subtle_fg.into(), panel_bg)
            };
            f.render_widget(Clear, btn_area);
            f.render_widget(
                Block::default().style(Style::default().bg(max_bg)),
                btn_area,
            );
            f.render_widget(
                Paragraph::new(Line::from(vec![Span::styled(
                    "▢",
                    Style::default().fg(max_fg).bg(max_bg),
                )]))
                .alignment(ratatui::layout::Alignment::Center),
                btn_area,
            );

            // ⇲ resize handle
            let btn_area = Rect {
                x: panel.rect.x + panel.rect.width.saturating_sub(3),
                y: panel.rect.y + panel.rect.height.saturating_sub(1),
                width: 3,
                height: 1,
            };
            let (res_fg, res_bg) = if btn_hover == Some(PanelBtn::Resize) {
                (t.tinted_fg.into(), hover_bg)
            } else {
                (t.subtle_fg.into(), panel_bg)
            };
            f.render_widget(Clear, btn_area);
            f.render_widget(
                Block::default().style(Style::default().bg(res_bg)),
                btn_area,
            );
            f.render_widget(
                Paragraph::new(Line::from(vec![Span::styled(
                    "⇲",
                    Style::default().fg(res_fg).bg(res_bg),
                )]))
                .alignment(ratatui::layout::Alignment::Center),
                btn_area,
            );

            // show dimensions on resize hover
            if btn_hover == Some(PanelBtn::Resize) {
                let inner_cols = panel.rect.width.saturating_sub(8);
                let inner_rows = panel.rect.height.saturating_sub(5);
                let dim_text = format!("{inner_cols}×{inner_rows}");
                let dim_w = dim_text.chars().count() as u16 + 2;
                let dim_area = Rect {
                    x: btn_area.x.saturating_sub(dim_w),
                    y: btn_area.y,
                    width: dim_w,
                    height: 1,
                };
                let buf_area = f.buffer_mut().area;
                let dim_area = dim_area.intersection(buf_area);
                if dim_area.width > 0 {
                    f.render_widget(
                        Paragraph::new(Line::from(vec![Span::styled(
                            format!(" {dim_text}"),
                            Style::default().fg(t.subtle_fg.into()).bg(panel_bg),
                        )])),
                        dim_area,
                    );
                }
            }
        }

        // content area: left 3, right 3, top 2, bottom 1
        let content_area = Rect {
            x: panel.rect.x + 3,
            y: panel.rect.y + 2,
            width: panel.rect.width.saturating_sub(6),
            height: panel.rect.height.saturating_sub(3),
        };

        let mut panel_hitmap = FloatingPanelHitmap::default();
        if content_area.height > 0 && content_area.width > 0 {
            let content_bg: Color = t.code_bg.into();
            f.render_widget(Clear, content_area);
            f.render_widget(
                Block::default().style(Style::default().bg(content_bg)),
                content_area,
            );
            render_panel_content(
                f,
                content_area,
                panel,
                snapshots,
                items,
                activity_nodes,
                hovered_btn,
                hovered_history_row,
                &mut panel_hitmap,
            );
            all_hitmap
                .history_row_rects
                .append(&mut panel_hitmap.history_row_rects);
        }

        // shadow after panel+content render
        render_shadow(f, panel.rect, &t);
    }

    all_hitmap
}

fn render_shadow(f: &mut Frame, rect: Rect, t: &crate::theme::Theme) {
    let buf = f.buffer_mut();
    let area = *buf.area();
    let bg_target: Color = t.modal_bg.into();

    let bg_ratio = 0.5;
    let fg_ratio = 0.4;

    let darken = |buf: &mut ratatui::buffer::Buffer, x: u16, y: u16| {
        if x < area.x || x >= area.x + area.width || y < area.y || y >= area.y + area.height {
            return;
        }
        let cell = &mut buf[(x, y)];
        cell.bg = lerp_color(cell.bg, bg_target, bg_ratio);
        cell.fg = lerp_color(cell.fg, bg_target, fg_ratio);
    };

    let max_x = area.x.saturating_add(area.width).saturating_sub(1);
    let max_y = area.y.saturating_add(area.height).saturating_sub(1);

    let top_y = rect.y.saturating_sub(1);
    let bot_y = (rect.y + rect.height).min(max_y);
    let lx0 = rect.x.saturating_sub(2);
    let lx1 = rect.x.saturating_sub(1);
    let rx0 = (rect.x + rect.width).min(max_x);
    let rx1 = (rect.x + rect.width + 1).min(max_x);

    // darken all ring cells
    for x in lx0..=rx1 {
        darken(buf, x, top_y);
    }
    for x in lx0..=rx1 {
        darken(buf, x, bot_y);
    }
    for y in top_y..=bot_y {
        darken(buf, rx0, y);
        darken(buf, rx1, y);
    }
    for y in top_y.saturating_add(1)..=bot_y {
        darken(buf, lx0, y);
        darken(buf, lx1, y);
    }
}

fn lerp_color(a: Color, b: Color, t: f64) -> Color {
    match (a, b) {
        (Color::Rgb(ar, ag, ab), Color::Rgb(br, bg, bb)) => {
            let r = (ar as f64 + (br as f64 - ar as f64) * t) as u8;
            let g = (ag as f64 + (bg as f64 - ag as f64) * t) as u8;
            let b = (ab as f64 + (bb as f64 - ab as f64) * t) as u8;
            Color::Rgb(r, g, b)
        }
        _ => b,
    }
}

#[allow(clippy::too_many_arguments)]
fn render_panel_content(
    f: &mut Frame,
    area: Rect,
    panel: &FloatingPanel,
    snapshots: &[TaskSnapshot],
    items: &[OutputItem],
    activity_nodes: &[ActivityNode],
    hovered_btn: &Option<(String, PanelBtn)>,
    hovered_history_row: &Option<String>,
    hitmap_out: &mut FloatingPanelHitmap,
) {
    let _hovered_btn = hovered_btn;
    if area.height == 0 || area.width == 0 {
        return;
    }

    let area = Rect::new(area.x + 1, area.y, area.width - 2, area.height - 1);

    match panel.kind {
        PanelKind::History => {
            render_history_content(f, area, snapshots, items, hitmap_out, hovered_history_row)
        }
        PanelKind::Activity => {
            let parts: Vec<&str> = panel.id.splitn(2, ':').collect();
            if parts.len() == 2 {
                let node = activity_nodes
                    .iter()
                    .find(|n| n.run_id == parts[0] && n.node_id == parts[1]);
                if let Some(node) = node {
                    render_activity_content(f, area, node);
                    return;
                }
            }
            render_placeholder(f, area, &panel.title);
        }
        PanelKind::Task(kind) => {
            let snap = snapshots.iter().find(|s| s.source_handle == panel.id);
            if snap.is_none() {
                render_placeholder(f, area, &panel.title);
                return;
            }
            let snap = snap.unwrap();
            match kind {
                TaskKind::Bash => {
                    let item = items.iter().rev().find(|it| match it {
                        OutputItem::Bash { handle, .. } => *handle == panel.id,
                        _ => false,
                    });
                    if let Some(OutputItem::Bash { output, done, .. }) = item {
                        render_bash_content(f, area, snap, output, *done);
                    } else {
                        render_task_meta(f, area, kind, snap);
                    }
                }
                TaskKind::Terminal => {
                    let item = items.iter().rev().find(|it| match it {
                        OutputItem::Terminal { handle, .. } => *handle == panel.id,
                        _ => false,
                    });
                    if let Some(OutputItem::Terminal {
                        screen,
                        accumulated_bytes,
                        done,
                        ..
                    }) = item
                    {
                        render_terminal_content(f, area, snap, screen, accumulated_bytes, *done);
                    } else {
                        render_task_meta(f, area, kind, snap);
                    }
                }
                _ => render_task_meta(f, area, kind, snap),
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct FloatingPanelHitmap {
    pub history_row_rects: Vec<(String, Rect)>,
}

fn task_summary_line(snap: &TaskSnapshot, items: &[OutputItem]) -> String {
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

fn format_started_at(snap: &TaskSnapshot) -> String {
    if let Some(ts) = snap.id.0.get_timestamp() {
        let (secs, _nanos) = ts.to_unix();
        if let Some(dt) = chrono::DateTime::from_timestamp(secs as i64, 0) {
            return dt.format("%H:%M:%S").to_string();
        }
    }
    String::new()
}

fn render_history_content(
    f: &mut Frame,
    area: Rect,
    snapshots: &[TaskSnapshot],
    items: &[OutputItem],
    hitmap: &mut FloatingPanelHitmap,
    hovered_row: &Option<String>,
) {
    let t = crate::theme::theme();
    let bar_color: Color = t.subtle_fg.into();
    let content_bg: Color = t.code_bg.into();
    let hover_bg: Color = t.code_bg.lerp(t.highlight_bg, 0.3);
    let mut done: Vec<&TaskSnapshot> = snapshots.iter().filter(|s| !s.is_running()).collect();
    done.sort_by_key(|b| std::cmp::Reverse(b.ended_at));

    let mut lines: Vec<Line> = Vec::new();
    for (i, snap) in done.iter().enumerate() {
        let icon = status_icon(snap.status);
        let elapsed = format_elapsed(snap.elapsed_ms());
        let st_color = status_color(snap.status);
        let row_y = area.y + i as u16;
        let is_hovered = hovered_row.as_deref() == Some(&snap.source_handle);
        hitmap.history_row_rects.push((
            snap.source_handle.clone(),
            Rect {
                x: area.x,
                y: row_y,
                width: area.width,
                height: 1,
            },
        ));
        let row_bg = if is_hovered { hover_bg } else { content_bg };
        let bar = if is_hovered { "▌" } else { "▎" };
        let label_fg = if is_hovered {
            t.tinted_fg.into()
        } else {
            t.subtle_fg.into()
        };
        let time_fg: Color = t.meta_fg.into();
        let kind_icon = task_kind_icon(snap.kind);
        let started = format_started_at(snap);
        let summary = task_summary_line(snap, items);
        let prefix_len: u16 = 2 + 2 + 2;
        let suffix_len: u16 =
            1 + started.chars().count() as u16 + 1 + elapsed.chars().count() as u16 + 1;
        let content_max = area.width.saturating_sub(prefix_len + suffix_len) as usize;
        let label_text = if summary.is_empty() {
            snap.label.clone()
        } else {
            format!("{} · {}", snap.label, summary)
        };
        let label = truncate(&label_text, content_max);
        let label_actual = label.chars().count() as u16;
        let pad = area.width - prefix_len - label_actual - suffix_len;
        lines.push(Line::from(vec![
            Span::styled(format!("{bar} "), Style::default().fg(bar_color).bg(row_bg)),
            Span::styled(
                format!("{kind_icon} "),
                Style::default().fg(st_color).bg(row_bg),
            ),
            Span::styled(format!("{icon} "), Style::default().fg(st_color).bg(row_bg)),
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
    f.render_widget(Paragraph::new(lines), area);
}

fn render_task_meta(f: &mut Frame, area: Rect, kind: TaskKind, snap: &TaskSnapshot) {
    let t = crate::theme::theme();
    let header = Line::from(vec![
        Span::styled(
            format!(" {} ", status_icon(snap.status)),
            Style::default().fg(status_color(snap.status)),
        ),
        Span::styled(&snap.label, Style::default().fg(t.tinted_fg.into())),
        Span::raw(" "),
        Span::styled(
            format!("{:?}", snap.status),
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
            Span::styled(&snap.source_handle, Style::default().fg(t.tinted_fg.into())),
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
    f.render_widget(Paragraph::new(lines), body_area);
}

fn render_bash_content(f: &mut Frame, area: Rect, snap: &TaskSnapshot, output: &str, done: bool) {
    let t = crate::theme::theme();
    let header = Line::from(vec![
        Span::styled(
            format!(" {} ", status_icon(snap.status)),
            Style::default().fg(status_color(snap.status)),
        ),
        Span::styled(&snap.label, Style::default().fg(t.tinted_fg.into())),
        Span::raw(" "),
        Span::styled(
            if done {
                "done".into()
            } else {
                format_elapsed(snap.elapsed_ms())
            },
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

    let all_lines: Vec<&str> = output.lines().collect();
    let max_visible = body_area.height as usize;
    let start = all_lines.len().saturating_sub(max_visible);
    let visible: Vec<Line> = all_lines[start..]
        .iter()
        .map(|l| {
            Line::from(Span::styled(
                *l,
                Style::default().fg(t.tinted_fg.into()).bg(t.code_bg.into()),
            ))
        })
        .collect();
    f.render_widget(Paragraph::new(visible), body_area);
}

fn render_terminal_content(
    f: &mut Frame,
    area: Rect,
    snap: &TaskSnapshot,
    screen: &atman_runtime::tools::term::TerminalScreen,
    _accumulated_bytes: &[u8],
    _done: bool,
) {
    let t = crate::theme::theme();
    let header = Line::from(vec![
        Span::styled(
            format!(" {} ", status_icon(snap.status)),
            Style::default().fg(status_color(snap.status)),
        ),
        Span::styled(&snap.label, Style::default().fg(t.tinted_fg.into())),
        Span::raw(" "),
        Span::styled(
            format_elapsed(snap.elapsed_ms()),
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
    if body_area.height == 0 || screen.cells.is_empty() {
        return;
    }

    let cols = screen.cols as usize;
    let total_rows = screen.rows as usize;
    let max_rows = (body_area.height as usize).min(total_rows);
    let start_row = total_rows.saturating_sub(max_rows);
    let bg: Color = t.code_bg.into();
    let mut lines: Vec<Line> = Vec::with_capacity(max_rows);
    for row in start_row..total_rows {
        let mut spans: Vec<Span> = Vec::with_capacity(cols);
        for col in 0..cols {
            let idx = row * cols + col;
            if idx >= screen.cells.len() {
                spans.push(Span::raw(" "));
                continue;
            }
            let cell = &screen.cells[idx];
            let style = crate::output::cell_style_for_viewer(cell, bg);
            let text = if cell.chars.is_empty() {
                " ".to_string()
            } else {
                cell.chars.clone()
            };
            spans.push(Span::styled(text, style));
        }
        lines.push(Line::from(spans));
    }
    f.render_widget(Paragraph::new(lines), body_area);
}

fn render_activity_content(f: &mut Frame, area: Rect, node: &ActivityNode) {
    let t = crate::theme::theme();
    let (kind_icon, kind_color) = crate::task_panel::node_kind_glyph(&node.kind);
    let icon = match node.status {
        ActivityStatus::Running => "◐",
        ActivityStatus::Ok => "✓",
        ActivityStatus::Err => "✗",
        ActivityStatus::Cancelled => "⊘",
    };
    let color: Color = match node.status {
        ActivityStatus::Running => t.accent.into(),
        ActivityStatus::Ok => t.success.into(),
        ActivityStatus::Err => t.error.into(),
        ActivityStatus::Cancelled => t.subtle_fg.into(),
    };
    let elapsed = node
        .ended_at
        .map(|e| e.duration_since(node.started_at).as_millis() as u64)
        .unwrap_or_else(|| node.started_at.elapsed().as_millis() as u64);
    let elapsed_str = format_elapsed(elapsed);

    let header = Line::from(vec![
        Span::styled(format!(" {kind_icon} "), Style::default().fg(kind_color)),
        Span::styled(&node.label, Style::default().fg(t.tinted_fg.into())),
        Span::raw(" "),
        Span::styled(icon.to_string(), Style::default().fg(color)),
        Span::raw(" "),
        Span::styled(&elapsed_str, Style::default().fg(t.subtle_fg.into())),
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

    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled("status ", Style::default().fg(t.subtle_fg.into())),
        Span::styled(format!("{:?}", node.status), Style::default().fg(color)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("run_id ", Style::default().fg(t.subtle_fg.into())),
        Span::styled(&node.run_id, Style::default().fg(t.tinted_fg.into())),
    ]));
    lines.push(Line::from(vec![
        Span::styled("node_id", Style::default().fg(t.subtle_fg.into())),
        Span::raw(" "),
        Span::styled(&node.node_id, Style::default().fg(t.tinted_fg.into())),
    ]));
    f.render_widget(Paragraph::new(lines), body_area);
}

fn render_placeholder(f: &mut Frame, area: Rect, title: &str) {
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

fn status_icon(status: atman_runtime::TaskStatus) -> &'static str {
    match status {
        atman_runtime::TaskStatus::Running => "◐",
        atman_runtime::TaskStatus::Ok => "✓",
        atman_runtime::TaskStatus::Err => "✗",
        atman_runtime::TaskStatus::Killed => "⊘",
    }
}

fn status_color(status: atman_runtime::TaskStatus) -> Color {
    let t = crate::theme::theme();
    match status {
        atman_runtime::TaskStatus::Running => t.accent.into(),
        atman_runtime::TaskStatus::Ok => t.success.into(),
        atman_runtime::TaskStatus::Err => t.error.into(),
        atman_runtime::TaskStatus::Killed => t.subtle_fg.into(),
    }
}

fn format_elapsed(ms: u64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{t}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas() -> Rect {
        Rect::new(0, 0, 100, 40)
    }

    #[test]
    fn open_creates_panel_and_focuses() {
        let mut fp = FloatingPanels::default();
        fp.open(
            "bg_1",
            PanelKind::Task(TaskKind::Bash),
            "cargo build",
            canvas(),
        );
        assert_eq!(fp.panels.len(), 1);
        assert_eq!(fp.focused, Some("bg_1".into()));
    }

    #[test]
    fn open_existing_focuses_without_duplicate() {
        let mut fp = FloatingPanels::default();
        fp.open(
            "bg_1",
            PanelKind::Task(TaskKind::Bash),
            "cargo build",
            canvas(),
        );
        fp.open(
            "bg_1",
            PanelKind::Task(TaskKind::Bash),
            "cargo build",
            canvas(),
        );
        assert_eq!(fp.panels.len(), 1);
    }

    #[test]
    fn close_removes_and_refocuses() {
        let mut fp = FloatingPanels::default();
        fp.open("a", PanelKind::Task(TaskKind::Bash), "a", canvas());
        fp.open("b", PanelKind::Task(TaskKind::Bash), "b", canvas());
        fp.close("b");
        assert_eq!(fp.panels.len(), 1);
        assert_eq!(fp.focused, Some("a".into()));
    }

    #[test]
    fn focus_brings_to_front() {
        let mut fp = FloatingPanels::default();
        fp.open("a", PanelKind::Task(TaskKind::Bash), "a", canvas());
        fp.open("b", PanelKind::Task(TaskKind::Bash), "b", canvas());
        let z_b_before = fp.panels.iter().find(|p| p.id == "b").unwrap().z;
        fp.focus("a");
        let z_a = fp.panels.iter().find(|p| p.id == "a").unwrap().z;
        assert!(z_a > z_b_before);
    }

    #[test]
    fn toggle_maximize_swaps_rect() {
        let mut fp = FloatingPanels::default();
        fp.open("a", PanelKind::Task(TaskKind::Bash), "a", canvas());
        let orig = fp.panels[0].rect;
        fp.toggle_maximize("a", canvas());
        assert!(fp.panels[0].maximized);
        assert_ne!(fp.panels[0].rect, orig);
        fp.toggle_maximize("a", canvas());
        assert!(!fp.panels[0].maximized);
        assert_eq!(fp.panels[0].rect, orig);
    }

    #[test]
    fn hit_test_titlebar_finds_topmost() {
        let mut fp = FloatingPanels::default();
        fp.open("a", PanelKind::Task(TaskKind::Bash), "a", canvas());
        fp.open("b", PanelKind::Task(TaskKind::Bash), "b", canvas());
        let b = fp.panels.iter().find(|p| p.id == "b").unwrap().clone();
        let hit = fp.hit_test_titlebar(b.rect.x + 2, b.rect.y);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().id, "b");
    }

    #[test]
    fn move_panel_clamps_to_canvas() {
        let mut fp = FloatingPanels::default();
        fp.open("a", PanelKind::Task(TaskKind::Bash), "a", canvas());
        fp.move_panel("a", 200, 200, canvas());
        let p = &fp.panels[0];
        assert!(p.rect.x < 100);
        assert!(p.rect.y < 40);
    }

    #[test]
    fn hit_test_titlebar_includes_shadow_ring() {
        let mut fp = FloatingPanels::default();
        fp.open("a", PanelKind::Task(TaskKind::Bash), "a", canvas());
        let p = fp.panels[0].clone();
        // shadow ring: 2 cols left of panel
        let hit = fp.hit_test_titlebar(p.rect.x - 1, p.rect.y + 1);
        assert!(hit.is_some(), "should hit shadow ring left of panel");
        // shadow ring: 1 row above panel
        let hit = fp.hit_test_titlebar(p.rect.x + 5, p.rect.y - 1);
        assert!(hit.is_some(), "should hit shadow ring above panel");
        // shadow ring: 2 cols right of panel
        let hit = fp.hit_test_titlebar(p.rect.x + p.rect.width, p.rect.y + 1);
        assert!(hit.is_some(), "should hit shadow ring right of panel");
    }

    #[test]
    fn hit_test_titlebar_excludes_content() {
        let mut fp = FloatingPanels::default();
        fp.open("a", PanelKind::Task(TaskKind::Bash), "a", canvas());
        let p = fp.panels[0].clone();
        // content area: x+2..x+w-2, y+3..y+h-2
        let hit = fp.hit_test_titlebar(p.rect.x + 3, p.rect.y + 4);
        assert!(hit.is_none(), "should not hit content area");
    }

    #[test]
    fn hit_test_titlebar_first_content_row_is_draggable() {
        // Content area starts at y+2 (panel.rect.y + 2). The titlebar
        // exclusion must cover y+2 so that clicks on the first content row
        // (e.g. first history row) are not captured as drag.
        let mut fp = FloatingPanels::default();
        fp.open("a", PanelKind::History, "History", canvas());
        let p = fp.panels[0].clone();
        // first content row is at y+2, x+3 (inside content x range)
        let hit = fp.hit_test_titlebar(p.rect.x + 3, p.rect.y + 2);
        assert!(
            hit.is_none(),
            "first content row y+2 must be excluded from titlebar"
        );
        // second content row at y+3 is excluded
        let hit2 = fp.hit_test_titlebar(p.rect.x + 3, p.rect.y + 3);
        assert!(hit2.is_none(), "second content row y+3 is excluded");
        // title bar at y+0 is draggable
        let hit3 = fp.hit_test_titlebar(p.rect.x + 3, p.rect.y);
        assert!(hit3.is_some(), "title bar y+0 is draggable");
    }

    #[test]
    fn hit_test_close_at_correct_position() {
        let mut fp = FloatingPanels::default();
        fp.open("a", PanelKind::Task(TaskKind::Bash), "a", canvas());
        let p = fp.panels[0].clone();
        // ✕ at (x..x+3, y+2) — 3-wide area
        let hit = fp.hit_test_close(p.rect.x + 1, p.rect.y + 2);
        assert!(hit.is_some());
        // also hits at x+0 (padding)
        let hit_pad = fp.hit_test_close(p.rect.x, p.rect.y + 2);
        assert!(hit_pad.is_some(), "should hit at padding position too");
        // not at x+3
        let miss = fp.hit_test_close(p.rect.x + 3, p.rect.y + 2);
        assert!(miss.is_none(), "should not hit beyond 3-wide area");
    }

    #[test]
    fn hit_test_resize_at_correct_position() {
        let mut fp = FloatingPanels::default();
        fp.open("a", PanelKind::Task(TaskKind::Bash), "a", canvas());
        let p = fp.panels[0].clone();
        // ⇲ at (x+w-1, y+h-1) — 3x3 area
        let cx = p.rect.x + p.rect.width - 1;
        let cy = p.rect.y + p.rect.height - 1;
        // center
        assert!(fp.hit_test_resize(cx, cy).is_some());
        // surrounding
        assert!(fp.hit_test_resize(cx - 1, cy).is_some());
        assert!(fp.hit_test_resize(cx + 1, cy).is_some());
        assert!(fp.hit_test_resize(cx, cy - 1).is_some());
        assert!(fp.hit_test_resize(cx, cy + 1).is_some());
        // too far
        assert!(fp.hit_test_resize(cx - 2, cy).is_none());
        assert!(fp.hit_test_resize(cx, cy + 2).is_none());
    }

    #[test]
    fn resize_panel_min_size() {
        let mut fp = FloatingPanels::default();
        fp.open("a", PanelKind::Task(TaskKind::Bash), "a", canvas());
        let c = canvas();
        fp.resize_panel("a", 5, 3, c);
        assert_eq!(fp.panels[0].rect.width, 20);
        assert_eq!(fp.panels[0].rect.height, 6);
    }

    #[test]
    fn shadow_border_not_overlapped_by_panel() {
        // shadow border is at lx0 (rect.x - 2), which is outside panel.rect
        // panel.rect starts at rect.x, so rect.x - 2 is NOT inside panel
        let mut fp = FloatingPanels::default();
        fp.open("a", PanelKind::Task(TaskKind::Bash), "a", canvas());
        let p = fp.panels[0].clone();
        // shadow top border y = rect.y - 1, panel starts at rect.y
        // so shadow top is NOT inside panel
        assert!(p.rect.y > 0, "panel should not be at y=0 for shadow test");
        // shadow left border x = rect.x - 2, panel starts at rect.x
        assert!(p.rect.x >= 2, "panel should not be at x<2 for shadow test");
    }
}
