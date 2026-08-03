use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use atman_runtime::message::Message;
use atman_runtime::workflow::WorkflowGraph;
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
    pub scroll: u16,
    pub h_scroll: u16,
    pub split: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelKind {
    Task(TaskKind),
    History,
    Activity,
    Mermaid,
    Cheatsheet,
    Mcp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanelBtn {
    Minimize,
    Maximize,
    Close,
    Resize,
}

impl PanelKind {
    fn icon(self) -> &'static str {
        match self {
            PanelKind::Task(kind) => task_kind_icon(kind),
            PanelKind::History => "⊞",
            PanelKind::Activity => "▸",
            PanelKind::Mermaid => "◇",
            PanelKind::Cheatsheet => "?",
            PanelKind::Mcp => "⚡",
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

// Content area is panel minus padding: width-8, height-5.
// 88×29 panel → 80×24 content.
const DEFAULT_PANEL_W: u16 = 88;
const DEFAULT_PANEL_H: u16 = 29;

impl FloatingPanels {
    pub fn open(&mut self, id: &str, kind: PanelKind, title: &str, canvas: Rect) {
        self.open_with_size(id, kind, title, canvas, 0, 0, false);
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open_with_size(
        &mut self,
        id: &str,
        kind: PanelKind,
        title: &str,
        canvas: Rect,
        w: u16,
        h: u16,
        maximized: bool,
    ) {
        let (w, h) = if w == 0 || h == 0 {
            (DEFAULT_PANEL_W, DEFAULT_PANEL_H)
        } else {
            (w, h)
        };
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            p.kind = kind;
            p.title = title.to_string();
            if !p.maximized && !maximized {
                let w = w.min(canvas.width.saturating_sub(4));
                let h = h.min(canvas.height.saturating_sub(4));
                p.rect.width = w;
                p.rect.height = h;
            }
            if maximized && !p.maximized {
                p.prev_rect = Some(p.rect);
                p.rect = maximized_rect(canvas);
                p.maximized = true;
            }
            self.focused = Some(id.to_string());
            self.bring_to_front(id);
            return;
        }
        self.z_counter += 1;
        let offset = (self.panels.len() as u16) * 3;
        let rect = if maximized {
            maximized_rect(canvas)
        } else {
            let w = w.min(canvas.width.saturating_sub(4));
            let h = h.min(canvas.height.saturating_sub(4));
            Rect {
                x: canvas.x + 2 + offset,
                y: canvas.y + 1 + offset,
                width: w,
                height: h,
            }
        };
        let prev_rect = if maximized {
            Some(default_rect(canvas))
        } else {
            None
        };
        let panel = FloatingPanel {
            id: id.to_string(),
            kind,
            title: title.to_string(),
            rect,
            z: self.z_counter,
            maximized,
            prev_rect,
            scroll: 0,
            h_scroll: 0,
            split: false,
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
                p.rect = p.prev_rect.take().unwrap_or_else(|| default_rect(canvas));
                p.maximized = false;
            } else {
                p.prev_rect = Some(p.rect);
                p.rect = maximized_rect(canvas);
                p.maximized = true;
            }
            self.bring_to_front(id);
        }
    }

    pub fn move_panel(&mut self, id: &str, x: u16, y: u16, _canvas: Rect) {
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            p.rect.x = x;
            p.rect.y = y;
        }
    }

    pub fn resize_panel(&mut self, id: &str, w: u16, h: u16, _canvas: Rect) {
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            p.rect.width = w.max(20);
            p.rect.height = h.max(6);
        }
    }

    pub fn unmaximize(&mut self, id: &str, canvas: Rect) {
        if let Some(p) = self.panels.iter_mut().find(|p| p.id == id) {
            if p.maximized {
                p.rect = p.prev_rect.take().unwrap_or_else(|| default_rect(canvas));
                p.maximized = false;
            }
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

    pub fn hit_test_minimize(&self, col: u16, row: u16) -> Option<String> {
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

    pub fn hit_test_close(&self, col: u16, row: u16) -> Option<String> {
        self.panels
            .iter()
            .find(|p| {
                p.rect.height >= 8 && col >= p.rect.x && col < p.rect.x + 3 && row == p.rect.y + 5
            })
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
        if let Some(id) = self.hit_test_minimize(col, row) {
            return Some((id, PanelBtn::Minimize));
        }
        if let Some(id) = self.hit_test_maximize(col, row) {
            return Some((id, PanelBtn::Maximize));
        }
        if let Some(id) = self.hit_test_close(col, row) {
            return Some((id, PanelBtn::Close));
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

fn maximized_rect(canvas: Rect) -> Rect {
    let w = canvas.width.saturating_sub(8);
    let h = canvas.height.saturating_sub(4);
    let x = canvas.x + 4;
    let y = canvas.y + 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

fn default_rect(canvas: Rect) -> Rect {
    let w = DEFAULT_PANEL_W.min(canvas.width.saturating_sub(4));
    let h = DEFAULT_PANEL_H.min(canvas.height.saturating_sub(4));
    let x = canvas.x + (canvas.width.saturating_sub(w)) / 2;
    let y = canvas.y + (canvas.height.saturating_sub(h)) / 2;
    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

#[allow(clippy::too_many_arguments)]
pub fn render(
    f: &mut Frame,
    _canvas: Rect,
    panels: &mut FloatingPanels,
    snapshots: &[TaskSnapshot],
    items: &[OutputItem],
    activity_nodes: &[ActivityNode],
    hovered_btn: &Option<(String, PanelBtn)>,
    hovered_history_row: &Option<String>,
    animation_frame: u32,
    panel_close_armed: Option<(&str, bool)>,
    max_canvas: Rect,
    mcp_servers: &[atman_runtime::mcp::McpServerStatus],
    expanded_mcp_servers: &std::collections::HashSet<String>,
    mcp_selected: usize,
    hovered_mcp_row: &Option<String>,
    mcp_browser: &crate::mcp_manager::McpBrowserState<'_>,
) -> FloatingPanelHitmap {
    let mut all_hitmap = FloatingPanelHitmap::default();
    let mut sorted: Vec<usize> = (0..panels.panels.len()).collect();
    sorted.sort_by_key(|&i| panels.panels[i].z);

    // Window resizes must update maximized rects every frame.
    for panel in &mut panels.panels {
        if panel.maximized {
            panel.rect = maximized_rect(max_canvas);
        }
    }

    let t = crate::theme::theme();
    for &idx in &sorted {
        let panel = &mut panels.panels[idx];
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
        let title_text = if panel.kind == PanelKind::Mermaid {
            let hint = if panel.split {
                "Tab: diagram"
            } else {
                "Tab: split"
            };
            format!("{}  · {}", panel.title, hint)
        } else {
            panel.title.clone()
        };
        let title_line = Line::from(vec![
            Span::styled(
                format!(" {} ", panel.kind.icon()),
                Style::default().fg(t.heading.into()).bg(panel_bg),
            ),
            Span::styled(
                crate::width::truncate(&title_text, panel.rect.width as usize - 6),
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

        // buttons: minimize(y+2), maximize(y+3), close(y+5, only if killable)
        if panel.rect.height >= 6 {
            let hover_bg = t.user_msg_bg.into();

            // — minimize (yellow)
            let btn_area = Rect {
                x: panel.rect.x,
                y: panel.rect.y + 2,
                width: 3,
                height: 1,
            };
            let (min_fg, min_bg) = if btn_hover == Some(PanelBtn::Minimize) {
                (Color::Rgb(180, 130, 0), hover_bg)
            } else {
                (Color::Rgb(200, 150, 20), panel_bg)
            };
            f.render_widget(Clear, btn_area);
            f.render_widget(
                Block::default().style(Style::default().bg(min_bg)),
                btn_area,
            );
            f.render_widget(
                Paragraph::new(Line::from(vec![Span::styled(
                    "—",
                    Style::default().fg(min_fg).bg(min_bg),
                )]))
                .alignment(ratatui::layout::Alignment::Center),
                btn_area,
            );

            // ▢ maximize (green, highlighted when maximized)
            let btn_area = Rect {
                x: panel.rect.x,
                y: panel.rect.y + 3,
                width: 3,
                height: 1,
            };
            let is_max = panel.maximized;
            let (max_fg, max_bg) = if btn_hover == Some(PanelBtn::Maximize) || is_max {
                (Color::Rgb(40, 200, 80), hover_bg)
            } else {
                (Color::Rgb(80, 180, 100), panel_bg)
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

            // ✕ close (red, two-click kill, only for killable running task panels)
            if panel.rect.height >= 8 {
                let killable = match &panel.kind {
                    PanelKind::Task(_) => snapshots
                        .iter()
                        .any(|s| s.source_handle == panel.id && s.is_running()),
                    _ => false,
                };
                if killable {
                    let btn_area = Rect {
                        x: panel.rect.x,
                        y: panel.rect.y + 5,
                        width: 3,
                        height: 1,
                    };
                    let armed = panel_close_armed
                        .as_ref()
                        .is_some_and(|(id, expired)| id == &panel.id && !expired);
                    let (close_fg, close_bg) = if btn_hover == Some(PanelBtn::Close) || armed {
                        (t.error.into(), hover_bg)
                    } else {
                        (Color::Rgb(220, 90, 90), panel_bg)
                    };
                    f.render_widget(Clear, btn_area);
                    f.render_widget(
                        Block::default().style(Style::default().bg(close_bg)),
                        btn_area,
                    );
                    let glyph = "✕";
                    let mod_add = if armed {
                        Modifier::BOLD
                    } else {
                        Modifier::empty()
                    };
                    f.render_widget(
                        Paragraph::new(Line::from(vec![Span::styled(
                            glyph,
                            Style::default()
                                .fg(close_fg)
                                .bg(close_bg)
                                .add_modifier(mod_add),
                        )]))
                        .alignment(ratatui::layout::Alignment::Center),
                        btn_area,
                    );
                }
            }

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
                animation_frame,
                mcp_servers,
                expanded_mcp_servers,
                mcp_selected,
                hovered_mcp_row,
                mcp_browser,
            );
            all_hitmap
                .history_row_rects
                .append(&mut panel_hitmap.history_row_rects);
            all_hitmap
                .workflow_node_rects
                .append(&mut panel_hitmap.workflow_node_rects);
            all_hitmap
                .mcp_row_rects
                .append(&mut panel_hitmap.mcp_row_rects);
        }

        // shadow after panel+content render
        render_shadow(f, panel.rect, &t);
    }

    all_hitmap
}

pub fn render_shadow(f: &mut Frame, rect: Rect, t: &crate::theme::Theme) {
    use ratatui::buffer::CellDiffOption;
    let buf = f.buffer_mut();
    let area = *buf.area();

    let sanitize = |buf: &mut ratatui::buffer::Buffer, x: u16, y: u16| {
        if x < area.x || x >= area.x + area.width || y < area.y || y >= area.y + area.height {
            return;
        }
        let cell = &mut buf[(x, y)];
        if crate::width::width(cell.symbol()) > 1 || cell.symbol().is_empty() {
            cell.set_symbol(" ");
            cell.set_diff_option(CellDiffOption::None);
        }
    };

    let shadow = t.shadow.into();
    let factor = lerp_color(Color::Rgb(255, 255, 255), shadow, 0.6);
    let darken = |buf: &mut ratatui::buffer::Buffer, x: u16, y: u16| {
        if x < area.x || x >= area.x + area.width || y < area.y || y >= area.y + area.height {
            return;
        }
        let cell = &mut buf[(x, y)];
        if matches!(cell.bg, Color::Reset) {
            return;
        }
        cell.bg = multiply_color(cell.bg, factor);
    };

    let max_x = area.x.saturating_add(area.width).saturating_sub(1);
    let max_y = area.y.saturating_add(area.height).saturating_sub(1);

    let top_y = rect.y.saturating_sub(1);
    let bot_y = (rect.y + rect.height).min(max_y);
    let lx0 = rect.x.saturating_sub(2);
    let lx1 = rect.x.saturating_sub(1);
    let rx0 = (rect.x + rect.width).min(max_x);
    let rx1 = (rect.x + rect.width + 1).min(max_x);

    // sanitize wide glyphs in shadow ring
    for x in lx0..=rx1 {
        sanitize(buf, x, top_y);
        sanitize(buf, x, bot_y);
    }
    for y in top_y..=bot_y {
        sanitize(buf, lx0, y);
        sanitize(buf, lx1, y);
        sanitize(buf, rx0, y);
        sanitize(buf, rx1, y);
    }

    // spiral shadow: top row, bottom row, right 2 cols, left 2 cols (offset to avoid corner overlap)
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

pub fn lerp_color(a: Color, b: Color, t: f64) -> Color {
    let mode = crate::theme::current_mode();
    fn to_rgb(c: Color, mode: crate::theme::ThemeMode) -> (u8, u8, u8) {
        use crate::theme::ThemeMode;
        match c {
            Color::Rgb(r, g, b) => (r, g, b),
            Color::Cyan => (40, 180, 180),
            Color::DarkGray => (96, 96, 96),
            Color::Gray => (128, 128, 128),
            Color::Green => (70, 175, 70),
            Color::Yellow => (190, 175, 55),
            Color::Red => (190, 65, 65),
            Color::Blue => (0, 0, 200),
            Color::Magenta => (200, 0, 200),
            Color::White => (240, 240, 240),
            Color::Black => (16, 16, 16),
            // Reset = terminal default; fg is light in dark mode, dark in light mode
            Color::Reset => match mode {
                ThemeMode::Dark => (220, 220, 220),
                ThemeMode::Light => (40, 40, 40),
            },
            _ => (128, 128, 128),
        }
    }
    let (ar, ag, ab) = to_rgb(a, mode);
    let (br, bg, bb) = to_rgb(b, mode);
    let t = t.clamp(0.0, 1.0);
    Color::Rgb(
        (ar as f64 + (br as f64 - ar as f64) * t).round() as u8,
        (ag as f64 + (bg as f64 - ag as f64) * t).round() as u8,
        (ab as f64 + (bb as f64 - ab as f64) * t).round() as u8,
    )
}

/// Multiply blend: `result = a * b / 255`. Always darkens (or keeps same).
/// `shadow` color acts as a brightness factor — e.g. (128,128,128) = 50% darken.
pub fn multiply_color(a: Color, b: Color) -> Color {
    let mode = crate::theme::current_mode();
    fn to_rgb(c: Color, mode: crate::theme::ThemeMode) -> (u8, u8, u8) {
        use crate::theme::ThemeMode;
        match c {
            Color::Rgb(r, g, b) => (r, g, b),
            Color::Cyan => (40, 180, 180),
            Color::DarkGray => (96, 96, 96),
            Color::Gray => (128, 128, 128),
            Color::Green => (70, 175, 70),
            Color::Yellow => (190, 175, 55),
            Color::Red => (190, 65, 65),
            Color::Blue => (0, 0, 200),
            Color::Magenta => (200, 0, 200),
            Color::White => (240, 240, 240),
            Color::Black => (16, 16, 16),
            Color::Reset => match mode {
                ThemeMode::Dark => (220, 220, 220),
                ThemeMode::Light => (40, 40, 40),
            },
            _ => (128, 128, 128),
        }
    }
    let (ar, ag, ab) = to_rgb(a, mode);
    let (br, bg, bb) = to_rgb(b, mode);
    Color::Rgb(
        ((ar as u32 * br as u32) / 255) as u8,
        ((ag as u32 * bg as u32) / 255) as u8,
        ((ab as u32 * bb as u32) / 255) as u8,
    )
}

fn darken_cell(buf: &mut ratatui::buffer::Buffer, area: Rect, x: u16, y: u16, ratio: f64) {
    if x < area.x || x >= area.x + area.width || y < area.y || y >= area.y + area.height {
        return;
    }
    let cell = &mut buf[(x, y)];
    if matches!(cell.bg, Color::Reset) {
        return;
    }
    let t = crate::theme::theme();
    let shadow = t.shadow.into();
    let factor = lerp_color(Color::Rgb(255, 255, 255), shadow, ratio);
    cell.bg = multiply_color(cell.bg, factor);
}

/// Top-to-bottom gradient: strongest at `rect.y`, fading to 0 over `rows` rows.
pub fn render_top_fade(f: &mut Frame, rect: Rect, rows: u16) {
    let buf = f.buffer_mut();
    let area = *buf.area();
    let rows = rows.min(rect.height);
    for ry in 0..rows {
        let ratio = 0.8 * (1.0 - ry as f64 / rows as f64);
        let y = rect.y + ry;
        for x in rect.x..rect.x + rect.width {
            darken_cell(buf, area, x, y, ratio);
        }
    }
}

/// Bottom-to-top gradient: strongest at `rect.y + rect.height - 1`,
/// fading to 0 over `rows` rows upward.
pub fn render_bottom_fade(f: &mut Frame, rect: Rect, rows: u16) {
    let buf = f.buffer_mut();
    let area = *buf.area();
    let rows = rows.min(rect.height);
    for ry in 0..rows {
        let ratio = 0.8 * (1.0 - ry as f64 / rows as f64);
        let y = rect.y + rect.height - 1 - ry;
        for x in rect.x..rect.x + rect.width {
            darken_cell(buf, area, x, y, ratio);
        }
    }
}

/// Input box shadow: sides + bottom fade with rounded corners.
pub fn render_input_shadow(f: &mut Frame, rect: Rect) {
    let buf = f.buffer_mut();
    let area = *buf.area();
    let h_cols = 4u16.min(rect.width / 2);
    let v_rows = 2u16;
    let max_ratio = 0.6;

    // iterate over the L-shaped shadow region (left + right + bottom)
    // and compute ratio from elliptical distance to the input box edge
    let offset_y = 1u16;
    let shadow_y0 = rect.y + offset_y;
    let shadow_y1 = rect.y + rect.height + v_rows + offset_y;
    let shadow_x0 = rect.x.saturating_sub(h_cols);
    let shadow_x1 = rect.x + rect.width + h_cols;

    for y in shadow_y0..shadow_y1 {
        for x in shadow_x0..shadow_x1 {
            // skip the input box interior (shifted down by offset_y)
            if x >= rect.x
                && x < rect.x + rect.width
                && y >= rect.y + offset_y
                && y < rect.y + rect.height + offset_y
            {
                continue;
            }

            // distance to nearest input box edge (shifted down by offset_y)
            let dx = if x < rect.x {
                rect.x.saturating_sub(x)
            } else if x >= rect.x + rect.width {
                x.saturating_sub(rect.x + rect.width)
            } else {
                0
            };
            let dy = if y >= rect.y + rect.height + offset_y {
                y.saturating_sub(rect.y + rect.height + offset_y)
            } else if y < rect.y + offset_y {
                (rect.y + offset_y).saturating_sub(y)
            } else {
                0
            };

            // normalize: horizontal distance relative to h_cols,
            // vertical distance relative to v_rows
            let nx = dx as f64 / h_cols as f64;
            let ny = dy as f64 / v_rows as f64;

            // elliptical falloff shaped like a quarter circle.
            // cells beyond 0.8 radius are cut to create the rounded gap.
            let dist = (nx * nx + ny * ny).sqrt();
            if dist > 0.8 {
                continue;
            }
            let ratio = max_ratio * (1.0 - dist / 0.8);
            if ratio > 0.0 {
                darken_cell(buf, area, x, y, ratio);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_panel_content(
    f: &mut Frame,
    area: Rect,
    panel: &mut FloatingPanel,
    snapshots: &[TaskSnapshot],
    items: &[OutputItem],
    activity_nodes: &[ActivityNode],
    hovered_btn: &Option<(String, PanelBtn)>,
    hovered_history_row: &Option<String>,
    hitmap_out: &mut FloatingPanelHitmap,
    animation_frame: u32,
    mcp_servers: &[atman_runtime::mcp::McpServerStatus],
    expanded_mcp_servers: &std::collections::HashSet<String>,
    mcp_selected: usize,
    hovered_mcp_row: &Option<String>,
    mcp_browser: &crate::mcp_manager::McpBrowserState<'_>,
) {
    let _hovered_btn = hovered_btn;
    if area.height == 0 || area.width == 0 {
        return;
    }

    let area = Rect::new(area.x + 1, area.y, area.width - 2, area.height);

    match panel.kind {
        PanelKind::History => render_history_content(
            f,
            area,
            snapshots,
            items,
            hitmap_out,
            hovered_history_row,
            panel.scroll,
        ),
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
        PanelKind::Mermaid => {
            let item_idx: Option<usize> = panel
                .id
                .strip_prefix("mermaid:")
                .and_then(|s| s.parse().ok());
            if let Some(idx) = item_idx
                && let Some(OutputItem::MermaidDiagram { source }) = items.get(idx)
            {
                let pad = 2u16;
                let inner_area = Rect {
                    x: area.x + pad,
                    y: area.y + 1,
                    width: area.width.saturating_sub(pad * 2),
                    height: area.height.saturating_sub(2),
                };
                if inner_area.width == 0 || inner_area.height == 0 {
                    return;
                }
                if panel.split {
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
                    let src_bg: Color = t.code_bg.into();

                    let src_lines: Vec<Line<'static>> = source
                        .lines()
                        .enumerate()
                        .map(|(i, l)| {
                            Line::from(vec![
                                Span::styled(
                                    format!("{:>3} ", i + 1),
                                    Style::default()
                                        .fg(t.subtle_fg.into())
                                        .bg(src_bg)
                                        .add_modifier(Modifier::DIM),
                                ),
                                Span::styled(
                                    l.to_string(),
                                    Style::default().fg(t.tinted_fg.into()).bg(src_bg),
                                ),
                            ])
                        })
                        .collect();
                    let src_max = (src_lines.len() as u16).saturating_sub(left_area.height);
                    panel.scroll = panel.scroll.min(src_max);
                    f.render_widget(
                        ratatui::widgets::Paragraph::new(src_lines).scroll((panel.scroll, 0)),
                        left_area,
                    );

                    let diagram_lines = crate::mermaid::render_mermaid(source, right_area.width);
                    let diag_max = (diagram_lines.len() as u16).saturating_sub(right_area.height);
                    panel.h_scroll = panel.h_scroll.min(diag_max);
                    f.render_widget(
                        ratatui::widgets::Paragraph::new(diagram_lines).scroll((panel.h_scroll, 0)),
                        right_area,
                    );

                    let divider_x = inner_area.x + half_w;
                    for y in inner_area.y..inner_area.y + inner_area.height {
                        if let Some(cell) = f
                            .buffer_mut()
                            .cell_mut(ratatui::layout::Position { x: divider_x, y })
                        {
                            cell.set_char('│');
                        }
                    }
                } else {
                    let lines = crate::mermaid::render_mermaid(source, inner_area.width);
                    let max_scroll = (lines.len() as u16).saturating_sub(inner_area.height);
                    panel.scroll = panel.scroll.min(max_scroll);
                    let content_w = lines
                        .iter()
                        .map(|l| crate::width::spans_width(&l.spans))
                        .max()
                        .unwrap_or(0) as u16;
                    let max_h_scroll = content_w.saturating_sub(inner_area.width);
                    panel.h_scroll = panel.h_scroll.min(max_h_scroll);
                    let scroll = panel.scroll;
                    let h_scroll = panel.h_scroll;
                    let visible_w = inner_area.width as usize;
                    let scrolled_lines: Vec<Line> = lines
                        .iter()
                        .skip(scroll as usize)
                        .map(|l| {
                            let line_str: String =
                                l.spans.iter().map(|s| s.content.as_ref()).collect();
                            let trimmed = crate::width::trim_display_offset(
                                &line_str,
                                h_scroll as usize,
                                visible_w,
                            );
                            Line::from(Span::styled(
                                trimmed,
                                l.spans.first().map(|s| s.style).unwrap_or_default(),
                            ))
                        })
                        .collect();
                    let p = ratatui::widgets::Paragraph::new(scrolled_lines);
                    f.render_widget(p, inner_area);
                }
            } else {
                render_placeholder(f, area, &panel.title);
            }
        }
        PanelKind::Cheatsheet => {
            let lines = crate::completion::cheatsheet_lines();
            let max_scroll = (lines.len() as u16).saturating_sub(area.height);
            panel.scroll = panel.scroll.min(max_scroll);
            let scroll = panel.scroll;
            let visible: Vec<Line> = lines.into_iter().skip(scroll as usize).collect();
            let p = ratatui::widgets::Paragraph::new(visible);
            f.render_widget(p, area);
        }
        PanelKind::Mcp => {
            crate::mcp_manager::render_panel(
                f,
                area,
                &mut panel.scroll,
                mcp_servers,
                expanded_mcp_servers,
                mcp_selected,
                hovered_mcp_row,
                hitmap_out,
                mcp_browser,
            );
        }
        PanelKind::Task(kind) => {
            let snap = snapshots.iter().find(|s| s.source_handle == panel.id);
            match kind {
                TaskKind::Bash => {
                    let item = items.iter().rev().find(|it| match it {
                        OutputItem::Bash { handle, .. } => *handle == panel.id,
                        _ => false,
                    });
                    if let Some(OutputItem::Bash { output, done, .. }) = item {
                        if let Some(snap) = snap {
                            render_bash_content(f, area, snap, output, *done);
                        } else {
                            render_bash_screen(f, area, &panel.title, output, *done);
                        }
                    } else if let Some(snap) = snap {
                        render_task_meta(f, area, kind, snap);
                    } else {
                        render_placeholder(f, area, &panel.title);
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
                        if let Some(snap) = snap {
                            render_terminal_content(
                                f,
                                area,
                                snap,
                                screen,
                                accumulated_bytes,
                                *done,
                            );
                        } else {
                            render_terminal_screen(f, area, &panel.title, screen);
                        }
                    } else if let Some(snap) = snap {
                        render_task_meta(f, area, kind, snap);
                    } else {
                        render_placeholder(f, area, &panel.title);
                    }
                }
                TaskKind::Agent => {
                    let item_idx = items
                        .iter()
                        .enumerate()
                        .rev()
                        .find(|(_, it)| match it {
                            OutputItem::SubAgentActivity { handle, .. } => *handle == panel.id,
                            _ => false,
                        })
                        .map(|(i, _)| i);
                    let item = item_idx.and_then(|i| items.get(i));
                    if let Some(OutputItem::SubAgentActivity {
                        handle,
                        goal,
                        model,
                        status,
                        output,
                        iteration,
                        done,
                        messages,
                        workflow_graph,
                        expanded_nodes,
                        ..
                    }) = item
                    {
                        render_sub_agent_panel(
                            f,
                            area,
                            handle,
                            goal,
                            model,
                            status,
                            output,
                            *iteration,
                            *done,
                            messages,
                            workflow_graph,
                            expanded_nodes,
                            &mut panel.scroll,
                            animation_frame,
                            hitmap_out,
                            item_idx.unwrap(),
                        );
                    } else if let Some(snap) = snap {
                        render_task_meta(f, area, kind, snap);
                    } else {
                        render_placeholder(f, area, &panel.title);
                    }
                }
                _ => {
                    let wp_idx = items.iter().enumerate().rev().find(|(_, it)| {
                        if let OutputItem::WorkflowPanel { graph, .. } = it {
                            graph.root.iter().any(|node| {
                                matches!(
                                    &node.kind,
                                    atman_runtime::workflow::WorkflowNodeKind::Flow { run_id, .. }
                                        if run_id == &panel.id
                                )
                            })
                        } else {
                            false
                        }
                    });
                    if let Some((panel_idx, _)) = wp_idx
                        && let Some(OutputItem::WorkflowPanel {
                            graph,
                            expanded_nodes,
                            ..
                        }) = items.get(panel_idx)
                    {
                        let render_width = area.width.max(300);
                        let (lines, regions) = crate::output::render_workflow_panel_with_regions(
                            graph,
                            expanded_nodes,
                            true,
                            false,
                            animation_frame,
                            render_width,
                            crate::output::MAX_COLLAPSED_BODY_ROWS,
                        );
                        let max_scroll = (lines.len() as u16).saturating_sub(area.height);
                        let content_w = lines
                            .iter()
                            .map(|l| crate::width::spans_width(&l.spans))
                            .max()
                            .unwrap_or(0) as u16;
                        let max_h_scroll = content_w.saturating_sub(area.width);
                        panel.scroll = panel.scroll.min(max_scroll);
                        panel.h_scroll = panel.h_scroll.min(max_h_scroll);
                        let scroll = panel.scroll;
                        let h_scroll = panel.h_scroll;
                        for r in &regions {
                            let row0 = area.y as u32 + r.start_row.saturating_sub(scroll as u32);
                            let row1 = area.y as u32 + r.end_row.saturating_sub(scroll as u32);
                            let col0 = area.x + r.col_start.saturating_sub(h_scroll);
                            let col1 = area.x + r.col_end.saturating_sub(h_scroll);
                            if col1 > col0 && row1 > row0 {
                                hitmap_out.workflow_node_rects.push((
                                    panel_idx,
                                    r.path_key.clone(),
                                    Rect {
                                        x: col0,
                                        y: row0 as u16,
                                        width: col1.saturating_sub(col0),
                                        height: (row1.saturating_sub(row0)) as u16,
                                    },
                                ));
                            }
                        }
                        let p = ratatui::widgets::Paragraph::new(lines)
                            .scroll((panel.scroll, panel.h_scroll));
                        f.render_widget(p, area);
                    } else {
                        render_placeholder(f, area, &panel.title);
                    }
                }
            }
        }
    }
}

#[derive(Debug, Default)]
pub struct FloatingPanelHitmap {
    pub history_row_rects: Vec<(String, Rect)>,
    pub workflow_node_rects: Vec<(usize, String, Rect)>,
    pub mcp_row_rects: Vec<(String, Rect)>,
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
    "--:--:--".to_string()
}

fn render_history_content(
    f: &mut Frame,
    area: Rect,
    snapshots: &[TaskSnapshot],
    items: &[OutputItem],
    hitmap: &mut FloatingPanelHitmap,
    hovered_row: &Option<String>,
    scroll: u16,
) {
    let t = crate::theme::theme();
    let bar_color: Color = t.subtle_fg.into();
    let content_bg: Color = t.code_bg.into();
    let hover_bg: Color = t.code_bg.lerp(t.highlight_bg, 0.3);
    let mut done: Vec<&TaskSnapshot> = snapshots.iter().filter(|s| !s.is_running()).collect();
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
        let kind_icon = task_kind_icon(snap.kind);
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

#[allow(clippy::too_many_arguments)]
fn render_sub_agent_panel(
    f: &mut Frame,
    area: Rect,
    handle: &str,
    goal: &str,
    model: &str,
    status: &str,
    _output: &str,
    iteration: u64,
    done: bool,
    messages: &[Message],
    workflow_graph: &WorkflowGraph,
    expanded_nodes: &HashSet<String>,
    scroll: &mut u16,
    animation_frame: u32,
    hitmap_out: &mut FloatingPanelHitmap,
    item_idx: usize,
) {
    let t = crate::theme::theme();
    let label_style = Style::default().fg(t.subtle_fg.into());
    let value_style = Style::default().fg(t.tinted_fg.into());
    let accent_style = Style::default().fg(t.accent.into());

    // Section 1: Header table
    let icon = match status {
        "ok" => "✓",
        "err" => "✗",
        "killed" => "⊘",
        _ if done => "✓",
        _ => "◐",
    };
    let iter_str = if done {
        String::new()
    } else {
        format!(" (iter {iteration})")
    };

    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::styled(
        format!(" {icon} {handle}{iter_str}"),
        accent_style,
    ));
    lines.push(Line::from(vec![
        Span::styled(" goal:   ", label_style),
        Span::styled(goal.to_string(), value_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" model:  ", label_style),
        Span::styled(model.to_string(), value_style),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" status: ", label_style),
        Span::styled(status.to_string(), value_style),
    ]));
    lines.push(Line::from(""));

    // Section 2: Workflow graph — reuse the same renderer as the main transcript
    let wf_offset = lines.len() as u32;
    let (wf_lines, regions) = crate::output::render_workflow_panel_with_regions(
        workflow_graph,
        expanded_nodes,
        true,
        false,
        animation_frame,
        area.width.max(300),
        crate::output::MAX_COLLAPSED_BODY_ROWS,
    );
    lines.extend(wf_lines);
    lines.push(Line::from(""));

    // Section 3: Sub-document flow — convert messages to OutputItems and
    // reuse build_lines so the rendering is identical to the main transcript.
    let items: Vec<OutputItem> = messages
        .iter()
        .map(|msg| match msg.role {
            atman_runtime::message::MessageRole::User => OutputItem::UserTurn {
                text: msg.text_concat(),
            },
            atman_runtime::message::MessageRole::Assistant => OutputItem::AssistantMd {
                md: msg.text_concat(),
                streaming: false,
                retried: false,
            },
            atman_runtime::message::MessageRole::Tool => {
                let content: String = msg
                    .parts
                    .iter()
                    .filter_map(|p| {
                        if let atman_runtime::message::MessagePart::ToolResult { content, .. } = p {
                            Some(content.as_str())
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                OutputItem::SystemNote {
                    text: content,
                    level: crate::app::NoteLevel::Info,
                }
            }
            _ => OutputItem::SystemNote {
                text: msg.text_concat(),
                level: crate::app::NoteLevel::Info,
            },
        })
        .collect();

    let empty_tools = HashSet::new();
    let render_ctx = crate::output::RenderCtx {
        expanded_tools: &empty_tools,
        messages,
        animation_frame,
        panel_width: area.width,
        hovered_thinking_idx: None,
    };
    let doc_lines = crate::output::build_lines(&items, &render_ctx);
    lines.extend(doc_lines);

    let max_scroll = (lines.len() as u16).saturating_sub(area.height);
    *scroll = (*scroll).min(max_scroll);

    for r in &regions {
        let row0 = area.y as u32 + (r.start_row + wf_offset).saturating_sub(*scroll as u32);
        let row1 = area.y as u32 + (r.end_row + wf_offset).saturating_sub(*scroll as u32);
        let col0 = area.x + r.col_start;
        let col1 = area.x + r.col_end;
        if col1 > col0 && row1 > row0 {
            hitmap_out.workflow_node_rects.push((
                item_idx,
                r.path_key.clone(),
                Rect {
                    x: col0,
                    y: row0 as u16,
                    width: col1 - col0,
                    height: (row1 - row0) as u16,
                },
            ));
        }
    }

    f.render_widget(Paragraph::new(lines).scroll((*scroll, 0)), area);
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
            if cell.wide_continuation {
                continue;
            }
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

fn render_terminal_screen(
    f: &mut Frame,
    area: Rect,
    title: &str,
    screen: &atman_runtime::tools::term::TerminalScreen,
) {
    let t = crate::theme::theme();
    let header = Line::from(vec![
        Span::styled(" ✓ ", Style::default().fg(t.success.into())),
        Span::styled(title, Style::default().fg(t.tinted_fg.into())),
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
            if cell.wide_continuation {
                continue;
            }
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

fn render_bash_screen(f: &mut Frame, area: Rect, title: &str, output: &str, done: bool) {
    let t = crate::theme::theme();
    let icon = if done { "✓" } else { "◐" };
    let icon_color = if done { t.success } else { t.accent };
    let header = Line::from(vec![
        Span::styled(format!(" {icon} "), Style::default().fg(icon_color.into())),
        Span::styled(title, Style::default().fg(t.tinted_fg.into())),
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
    fn move_panel_no_clamp() {
        let mut fp = FloatingPanels::default();
        fp.open("a", PanelKind::Task(TaskKind::Bash), "a", canvas());
        fp.move_panel("a", 200, 200, canvas());
        let p = &fp.panels[0];
        assert_eq!(p.rect.x, 200);
        assert_eq!(p.rect.y, 200);
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
        let hit = fp.hit_test_close(p.rect.x + 1, p.rect.y + 5);
        assert!(hit.is_some());
        let hit_pad = fp.hit_test_close(p.rect.x, p.rect.y + 5);
        assert!(hit_pad.is_some(), "should hit at padding position too");
        let miss = fp.hit_test_close(p.rect.x + 3, p.rect.y + 5);
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
