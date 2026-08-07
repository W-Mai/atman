use std::collections::HashSet;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Clear};

use atman_runtime::{TaskKind, TaskSnapshot};

use crate::app::OutputItem;
use crate::task_panel::ActivityNode;
use crate::wm::{ContentKey, OpenPolicy};

pub mod content;
pub mod focus;
pub mod hitmap;
pub mod shadow;
pub mod shell;

pub use focus::FocusState;
pub use hitmap::WmHitmap;
pub use shadow::{
    lerp_color, multiply_color, render_bottom_fade, render_input_shadow, render_shadow,
    render_top_fade,
};

pub struct WindowInstance {
    pub id: String,
    pub content_key: ContentKey,
    pub kind: PanelKind,
    pub content: Option<Box<dyn crate::wm::WindowComponent>>,
    pub title: String,
    pub rect: Rect,
    pub z: u32,
    pub maximized: bool,
    pub prev_rect: Option<Rect>,
    pub scroll: u16,
    pub h_scroll: u16,
    pub split: bool,
    pub expanded_tools: HashSet<String>,
    pub render_cache: Option<PanelRenderCache>,
}

/// Cached render output for sub-agent / workflow floating panels.
/// On a cache hit we skip workflow graph traversal, flatten_message, and
/// build_lines — just draw the stored lines and recompute hitmap rects from
/// the stored regions using the *current* scroll/area.
#[derive(Clone, Debug)]
pub struct PanelRenderCache {
    pub(crate) items_version: u64,
    pub(crate) expanded_version: u64,
    pub(crate) width: u16,
    /// None when the panel is done/idle → cache survives across frames.
    /// Some(frame) when running → invalidated every animation tick.
    pub(crate) animation_frame: Option<u32>,
    pub(crate) messages_len: usize,
    pub(crate) workflow_expanded: bool,
    pub(crate) expanded_tools_len: usize,

    pub(crate) lines: Vec<Line<'static>>,
    pub(crate) regions: Vec<crate::output::NodeRegion>,
    pub(crate) wf_offset: u32,
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
    }
}

#[derive(Default)]
pub struct WindowManager {
    pub panels: Vec<WindowInstance>,
    pub focus: FocusState,
    z_counter: u32,
}

const DEFAULT_PANEL_W: u16 = 88;
const DEFAULT_PANEL_H: u16 = 29;

impl WindowManager {
    pub fn focused(&self) -> Option<&str> {
        self.focus.active.as_deref()
    }

    pub fn open(
        &mut self,
        id: &str,
        content_key: ContentKey,
        kind: PanelKind,
        title: &str,
        canvas: Rect,
    ) {
        self.open_with_size(
            id,
            content_key,
            OpenPolicy::ReuseExisting,
            kind,
            title,
            canvas,
            0,
            0,
            false,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn open_with_size(
        &mut self,
        id: &str,
        content_key: ContentKey,
        policy: OpenPolicy,
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
        let reuse = match policy {
            OpenPolicy::ReuseExisting => self
                .panels
                .iter()
                .find(|p| p.content_key == content_key)
                .map(|p| p.id.clone()),
            OpenPolicy::AlwaysNew => None,
        };
        if let Some(existing_id) = reuse {
            let p = self
                .panels
                .iter_mut()
                .find(|p| p.id == existing_id)
                .unwrap();
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
            self.focus.focus(&existing_id);
            self.bring_to_front(&existing_id);
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
        let panel = WindowInstance {
            id: id.to_string(),
            content_key,
            kind,
            content: None,
            title: title.to_string(),
            rect,
            z: self.z_counter,
            maximized,
            prev_rect,
            scroll: 0,
            h_scroll: 0,
            split: false,
            expanded_tools: HashSet::new(),
            render_cache: None,
        };
        self.panels.push(panel);
        self.focus.focus(id);
    }

    pub fn close(&mut self, id: &str) {
        self.panels.retain(|p| p.id != id);
        self.focus.remove_and_refocus(id, &self.panels);
    }

    pub fn focus(&mut self, id: &str) {
        self.focus.focus(id);
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

    pub fn cycle_focus(&mut self, forward: bool) {
        if self.panels.len() < 2 {
            return;
        }
        let mut sorted: Vec<&WindowInstance> = self.panels.iter().collect();
        sorted.sort_by_key(|p| std::cmp::Reverse(p.z));
        let current_idx = sorted
            .iter()
            .position(|p| Some(p.id.as_str()) == self.focus.active.as_deref());
        let next_idx = match current_idx {
            Some(i) => {
                if forward {
                    (i + 1) % sorted.len()
                } else {
                    (i + sorted.len() - 1) % sorted.len()
                }
            }
            None => 0,
        };
        if let Some(target) = sorted.get(next_idx) {
            let id = target.id.clone();
            self.focus.focus(&id);
            self.bring_to_front(&id);
        }
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
    panels: &mut WindowManager,
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
    items_version: u64,
    expanded_version: u64,
) -> WmHitmap {
    let mut all_hitmap = WmHitmap::default();
    let mut sorted: Vec<usize> = (0..panels.panels.len()).collect();
    sorted.sort_by_key(|&i| panels.panels[i].z);

    for panel in &mut panels.panels {
        if panel.maximized {
            panel.rect = maximized_rect(max_canvas);
        }
    }

    let t = crate::theme::theme();
    for &idx in &sorted {
        let panel = &mut panels.panels[idx];
        let is_focused = panels.focus.active.as_deref() == Some(panel.id.as_str());
        let btn_hover = hovered_btn
            .as_ref()
            .and_then(|(id, btn)| if id == &panel.id { Some(*btn) } else { None });

        shell::render_shell(
            f,
            panel,
            is_focused,
            btn_hover,
            panel_close_armed,
            snapshots,
            &t,
        );

        let content_area = Rect {
            x: panel.rect.x + 3,
            y: panel.rect.y + 2,
            width: panel.rect.width.saturating_sub(6),
            height: panel.rect.height.saturating_sub(3),
        };

        let mut panel_hitmap = WmHitmap::default();
        if content_area.height > 0 && content_area.width > 0 {
            let content_bg: Color = t.code_bg.into();
            f.render_widget(Clear, content_area);
            f.render_widget(
                Block::default().style(Style::default().bg(content_bg)),
                content_area,
            );
            content::render_panel_content(
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
                items_version,
                expanded_version,
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

        shadow::render_shadow(f, panel.rect, &t);
    }

    all_hitmap
}

#[cfg(test)]
mod tests {
    use super::*;

    fn canvas() -> Rect {
        Rect::new(0, 0, 100, 40)
    }

    #[test]
    fn open_creates_panel_and_focuses() {
        let mut fp = WindowManager::default();
        fp.open(
            "bg_1",
            ContentKey::Task("bg_1".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "cargo build",
            canvas(),
        );
        assert_eq!(fp.panels.len(), 1);
        assert_eq!(fp.focused(), Some("bg_1"));
    }

    #[test]
    fn open_existing_focuses_without_duplicate() {
        let mut fp = WindowManager::default();
        fp.open(
            "bg_1",
            ContentKey::Task("bg_1".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "cargo build",
            canvas(),
        );
        fp.open(
            "bg_1",
            ContentKey::Task("bg_1".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "cargo build",
            canvas(),
        );
        assert_eq!(fp.panels.len(), 1);
    }

    #[test]
    fn close_removes_and_refocuses() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "a",
            canvas(),
        );
        fp.open(
            "b",
            ContentKey::Task("b".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "b",
            canvas(),
        );
        fp.close("b");
        assert_eq!(fp.panels.len(), 1);
        assert_eq!(fp.focused(), Some("a"));
    }

    #[test]
    fn focus_brings_to_front() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "a",
            canvas(),
        );
        fp.open(
            "b",
            ContentKey::Task("b".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "b",
            canvas(),
        );
        let z_b_before = fp.panels.iter().find(|p| p.id == "b").unwrap().z;
        fp.focus("a");
        let z_a = fp.panels.iter().find(|p| p.id == "a").unwrap().z;
        assert!(z_a > z_b_before);
    }

    #[test]
    fn toggle_maximize_swaps_rect() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "a",
            canvas(),
        );
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
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "a",
            canvas(),
        );
        fp.open(
            "b",
            ContentKey::Task("b".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "b",
            canvas(),
        );
        let b = fp.panels.iter().find(|p| p.id == "b").unwrap().clone();
        let hit = fp.hit_test_titlebar(b.rect.x + 2, b.rect.y);
        assert!(hit.is_some());
        assert_eq!(hit.unwrap().id, "b");
    }

    #[test]
    fn move_panel_no_clamp() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "a",
            canvas(),
        );
        fp.move_panel("a", 200, 200, canvas());
        let p = &fp.panels[0];
        assert_eq!(p.rect.x, 200);
        assert_eq!(p.rect.y, 200);
    }

    #[test]
    fn hit_test_titlebar_includes_shadow_ring() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "a",
            canvas(),
        );
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
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "a",
            canvas(),
        );
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
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::History,
            PanelKind::History,
            "History",
            canvas(),
        );
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
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "a",
            canvas(),
        );
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
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "a",
            canvas(),
        );
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
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "a",
            canvas(),
        );
        let c = canvas();
        fp.resize_panel("a", 5, 3, c);
        assert_eq!(fp.panels[0].rect.width, 20);
        assert_eq!(fp.panels[0].rect.height, 6);
    }

    #[test]
    fn shadow_border_not_overlapped_by_panel() {
        // shadow border is at lx0 (rect.x - 2), which is outside panel.rect
        // panel.rect starts at rect.x, so rect.x - 2 is NOT inside panel
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "a",
            canvas(),
        );
        let p = fp.panels[0].clone();
        // shadow top border y = rect.y - 1, panel starts at rect.y
        // so shadow top is NOT inside panel
        assert!(p.rect.y > 0, "panel should not be at y=0 for shadow test");
        // shadow left border x = rect.x - 2, panel starts at rect.x
        assert!(p.rect.x >= 2, "panel should not be at x<2 for shadow test");
    }

    #[test]
    fn close_uses_history_not_panels_last() {
        let mut fp = WindowManager::default();
        fp.open(
            "a",
            ContentKey::Task("a".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "a",
            canvas(),
        );
        fp.open(
            "b",
            ContentKey::Task("b".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "b",
            canvas(),
        );
        fp.open(
            "c",
            ContentKey::Task("c".to_string()),
            PanelKind::Task(TaskKind::Bash),
            "c",
            canvas(),
        );
        // focus order: a was focused first, then b, then c.
        // Now re-focus "a" so it's most-recent in history.
        fp.focus("a");
        // close "c" (the last-opened). Focus should go to "a" (most recent in
        // history), not "b" (panels.last()).
        fp.close("c");
        assert_eq!(fp.panels.len(), 2);
        assert_eq!(
            fp.focused(),
            Some("a"),
            "close should refocus by history, not panels.last()"
        );
    }
}
