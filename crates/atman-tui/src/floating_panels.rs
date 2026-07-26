use std::collections::HashMap;

use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};
use ratatui::Frame;

use atman_runtime::{TaskKind, TaskSnapshot};

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
}

impl PanelKind {
    fn icon(self) -> &'static str {
        match self {
            PanelKind::Task(TaskKind::Bash) => "$",
            PanelKind::Task(TaskKind::Terminal) => "▶",
            PanelKind::Task(TaskKind::Flow) => "⬡",
            PanelKind::Task(TaskKind::Agent) => "✦",
            PanelKind::Task(TaskKind::Subflow) => "↳",
            PanelKind::Task(TaskKind::Dispatch) => "≡",
            PanelKind::History => "⊞",
        }
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
        if self.panels.iter().any(|p| p.id == id) {
            self.focused = Some(id.to_string());
            self.bring_to_front(id);
            return;
        }
        self.z_counter += 1;
        let offset = (self.panels.len() as u16) * 3;
        let w = 48.min(canvas.width.saturating_sub(4));
        let h = 20.min(canvas.height.saturating_sub(4));
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

    pub fn hit_test_titlebar(&self, col: u16, row: u16) -> Option<&FloatingPanel> {
        self.panels
            .iter()
            .filter(|p| {
                row == p.rect.y
                    && col >= p.rect.x
                    && col < p.rect.x + p.rect.width
            })
            .max_by_key(|p| p.z)
    }

    pub fn hit_test_close(&self, col: u16, row: u16) -> Option<String> {
        self.panels.iter()
            .find(|p| {
                row == p.rect.y && col >= p.rect.x + 1 && col <= p.rect.x + 2
            })
            .map(|p| p.id.clone())
    }

    pub fn hit_test_maximize(&self, col: u16, row: u16) -> Option<String> {
        self.panels.iter()
            .find(|p| {
                row == p.rect.y + 1 && col >= p.rect.x + 1 && col <= p.rect.x + 2
            })
            .map(|p| p.id.clone())
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

pub fn render(
    f: &mut Frame,
    canvas: Rect,
    panels: &FloatingPanels,
    snapshots: &[TaskSnapshot],
) {
    let mut sorted: Vec<&FloatingPanel> = panels.panels.iter().collect();
    sorted.sort_by_key(|p| p.z);

    for panel in &sorted {
        let is_focused = panels.focused.as_deref() == Some(panel.id.as_str());
        let border_color = if is_focused {
            Color::LightBlue
        } else {
            Color::DarkGray
        };

        f.render_widget(Clear, panel.rect);

        let title_line = Line::from(vec![
            Span::styled(
                format!(" {} ", panel.kind.icon()),
                Style::default().fg(Color::Yellow),
            ),
            Span::styled(
                truncate(&panel.title, panel.rect.width as usize - 6),
                Style::default().fg(if is_focused {
                    Color::White
                } else {
                    Color::Gray
                }),
            ),
        ]);

        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(border_color))
            .title(title_line);

        let inner = block.inner(panel.rect);
        f.render_widget(block, panel.rect);

        // Vertical button bar on left border: × (close) at row 0, ▢ (maximize) at row 1
        if panel.rect.height >= 3 {
            let bx = panel.rect.x + 1;
            let by = panel.rect.y;
            f.render_widget(
                Paragraph::new(Line::from(vec![Span::styled(
                    "×",
                    Style::default().fg(Color::Red),
                )])),
                Rect {
                    x: bx,
                    y: by,
                    width: 2,
                    height: 1,
                },
            );
            f.render_widget(
                Paragraph::new(Line::from(vec![Span::styled(
                    "▢",
                    Style::default().fg(Color::DarkGray),
                )])),
                Rect {
                    x: bx,
                    y: by + 1,
                    width: 2,
                    height: 1,
                },
            );
        }

        render_panel_content(f, inner, panel, snapshots);
    }
}

fn render_panel_content(
    f: &mut Frame,
    area: Rect,
    panel: &FloatingPanel,
    snapshots: &[TaskSnapshot],
) {
    if area.height == 0 || area.width == 0 {
        return;
    }

    match panel.kind {
        PanelKind::History => render_history_content(f, area, snapshots),
        PanelKind::Task(kind) => {
            let snap = snapshots.iter().find(|s| s.source_handle == panel.id);
            if let Some(snap) = snap {
                render_task_content(f, area, kind, snap);
            } else {
                render_placeholder(f, area, &panel.title);
            }
        }
    }
}

fn render_history_content(f: &mut Frame, area: Rect, snapshots: &[TaskSnapshot]) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        " Tool execution history",
        Style::default().fg(Color::DarkGray),
    )]));
    lines.push(Line::from(""));
    for snap in snapshots.iter() {
        let icon = status_icon(snap.status);
        let elapsed = if snap.is_running() {
            format_elapsed(snap.elapsed_ms())
        } else {
            "done".into()
        };
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {icon} "),
                Style::default().fg(status_color(snap.status)),
            ),
            Span::styled(
                truncate(&snap.label, area.width as usize - 12),
                Style::default().fg(Color::White),
            ),
            Span::raw(" "),
            Span::styled(elapsed, Style::default().fg(Color::DarkGray)),
        ]));
    }
    f.render_widget(Paragraph::new(lines), area);
}

fn render_task_content(
    f: &mut Frame,
    area: Rect,
    kind: TaskKind,
    snap: &TaskSnapshot,
) {
    let mut lines: Vec<Line> = Vec::new();
    lines.push(Line::from(vec![
        Span::styled(" kind: ", Style::default().fg(Color::Yellow)),
        Span::raw(kind.label()),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" label: ", Style::default().fg(Color::Yellow)),
        Span::raw(&snap.label),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" status: ", Style::default().fg(Color::Yellow)),
        Span::styled(
            format!("{:?}", snap.status),
            Style::default().fg(status_color(snap.status)),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" handle: ", Style::default().fg(Color::Yellow)),
        Span::raw(&snap.source_handle),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" elapsed: ", Style::default().fg(Color::Yellow)),
        Span::raw(format_elapsed(snap.elapsed_ms())),
    ]));
    f.render_widget(Paragraph::new(lines), area);
}

fn render_placeholder(f: &mut Frame, area: Rect, title: &str) {
    f.render_widget(
        Paragraph::new(format!(" (no data: {title})")),
        area,
    );
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
    match status {
        atman_runtime::TaskStatus::Running => Color::LightBlue,
        atman_runtime::TaskStatus::Ok => Color::Green,
        atman_runtime::TaskStatus::Err => Color::Red,
        atman_runtime::TaskStatus::Killed => Color::DarkGray,
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
        fp.open("bg_1", PanelKind::Task(TaskKind::Bash), "cargo build", canvas());
        assert_eq!(fp.panels.len(), 1);
        assert_eq!(fp.focused, Some("bg_1".into()));
    }

    #[test]
    fn open_existing_focuses_without_duplicate() {
        let mut fp = FloatingPanels::default();
        fp.open("bg_1", PanelKind::Task(TaskKind::Bash), "cargo build", canvas());
        fp.open("bg_1", PanelKind::Task(TaskKind::Bash), "cargo build", canvas());
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
}
