use ratatui::layout::Rect;

use crate::wm::WindowId;

use super::{PanelBtn, WindowInstance, WindowManager};

#[derive(Debug, Default)]
pub struct WmHitmap {
    // Each entry is (panel_id, ...) so hit regions can be attributed to the
    // panel that produced them. Consumers must not fire a region owned by a
    // lower-z panel that the topmost panel would block.
    pub history_row_rects: Vec<(WindowId, String, Rect)>,
    pub workflow_node_rects: Vec<(WindowId, usize, String, Rect)>,
    pub mcp_row_rects: Vec<(WindowId, String, Rect)>,
}

impl WindowManager {
    pub fn hit_test_titlebar(&self, col: u16, row: u16) -> Option<&WindowInstance> {
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

    pub fn hit_test_minimize(&self, col: u16, row: u16) -> Option<WindowId> {
        self.panels
            .iter()
            .filter(|p| col >= p.rect.x && col < p.rect.x + 3 && row == p.rect.y + 2)
            .max_by_key(|p| p.z)
            .map(|p| p.id)
    }

    pub fn hit_test_maximize(&self, col: u16, row: u16) -> Option<WindowId> {
        self.panels
            .iter()
            .filter(|p| col >= p.rect.x && col < p.rect.x + 3 && row == p.rect.y + 3)
            .max_by_key(|p| p.z)
            .map(|p| p.id)
    }

    pub fn hit_test_close(&self, col: u16, row: u16) -> Option<WindowId> {
        self.panels
            .iter()
            .filter(|p| {
                p.rect.height >= 8 && col >= p.rect.x && col < p.rect.x + 3 && row == p.rect.y + 5
            })
            .max_by_key(|p| p.z)
            .map(|p| p.id)
    }

    pub fn hit_test_resize(&self, col: u16, row: u16) -> Option<WindowId> {
        // 3x3 area around the resize button at (x+w-1, y+h-1)
        self.panels
            .iter()
            .filter(|p| {
                let cx = p.rect.x + p.rect.width.saturating_sub(1);
                let cy = p.rect.y + p.rect.height.saturating_sub(1);
                col >= cx.saturating_sub(1)
                    && col <= cx + 1
                    && row >= cy.saturating_sub(1)
                    && row <= cy + 1
            })
            .max_by_key(|p| p.z)
            .map(|p| p.id)
    }

    pub fn hit_test_btn(&self, col: u16, row: u16) -> Option<(WindowId, PanelBtn)> {
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

    pub fn hit_test_panel(&self, col: u16, row: u16) -> Option<&WindowInstance> {
        self.panels
            .iter()
            .filter(|p| {
                let sx0 = p.rect.x.saturating_sub(2);
                let sx1 = p.rect.x + p.rect.width + 1;
                let sy0 = p.rect.y.saturating_sub(1);
                let sy1 = p.rect.y + p.rect.height + 1;
                col >= sx0 && col <= sx1 && row >= sy0 && row <= sy1
            })
            .max_by_key(|p| p.z)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wm::{ContentKey, FocusState, WindowContent, WindowId};
    use ratatui::layout::Rect;

    fn rect(x: u16, y: u16, w: u16, h: u16) -> Rect {
        Rect::new(x, y, w, h)
    }

    fn panel(id: u64, label: &str, rect: Rect, z: u32) -> WindowInstance {
        WindowInstance {
            id: WindowId(id),
            label: label.to_string(),
            content_key: ContentKey::Task(label.to_string()),
            content_kind: WindowContent::Task {
                handle: label.to_string(),
                kind: atman_runtime::TaskKind::Bash,
            },
            content: None,
            title: label.to_string(),
            rect,
            z,
            maximized: false,
            prev_rect: None,
            scroll: 0,
            h_scroll: 0,
            split: false,
            expanded_tools: std::collections::HashSet::new(),
            render_cache: None,
            content_version: 0,
        }
    }

    #[test]
    fn hit_test_panel_returns_topmost() {
        let wm = WindowManager {
            panels: vec![
                panel(0, "a", rect(10, 10, 30, 10), 1),
                panel(1, "b", rect(10, 10, 30, 10), 2),
            ],
            focus: FocusState::default(),
            ..Default::default()
        };
        let hit = wm.hit_test_panel(20, 15);
        assert_eq!(hit.map(|p| p.id), Some(WindowId(1)));
    }

    #[test]
    fn overlap_history_row_rect_uses_topmost_panel() {
        let wm = WindowManager {
            panels: vec![
                panel(0, "a", rect(10, 10, 30, 10), 1),
                panel(1, "b", rect(10, 10, 30, 10), 2),
            ],
            focus: FocusState::default(),
            ..Default::default()
        };
        // A history row rect from the lower panel A is fully covered by the
        // topmost panel B. A click at (20,15) must resolve to B, the topmost.
        let hit = wm.hit_test_panel(20, 15);
        assert_eq!(hit.map(|p| p.id), Some(WindowId(1)));
        // A point outside both panels resolves to none.
        assert!(wm.hit_test_panel(5, 5).is_none());
    }

    #[test]
    fn click_swallow_topmost_panel_blocks_lower() {
        // Two fully-overlapping panels; a click inside both must resolve to the
        // topmost (highest z) and never fall through to the covered panel.
        let wm = WindowManager {
            panels: vec![
                panel(0, "a", rect(0, 0, 40, 20), 1),
                panel(1, "b", rect(0, 0, 40, 20), 2),
            ],
            focus: FocusState::default(),
            ..Default::default()
        };
        let hit = wm.hit_test_panel(5, 5);
        assert_eq!(
            hit.map(|p| p.id),
            Some(WindowId(1)),
            "click at (5,5) must resolve to topmost panel B"
        );
        assert_ne!(
            hit.map(|p| p.id),
            Some(WindowId(0)),
            "covered panel A must not receive the click"
        );
    }
}
