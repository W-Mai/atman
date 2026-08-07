use ratatui::layout::Rect;

use super::{PanelBtn, WindowInstance, WindowManager};

#[derive(Debug, Default)]
pub struct WmHitmap {
    // Each entry is (panel_id, ...) so hit regions can be attributed to the
    // panel that produced them. Consumers must not fire a region owned by a
    // lower-z panel that the topmost panel would block.
    pub history_row_rects: Vec<(String, String, Rect)>, // (panel_id, handle, rect)
    pub workflow_node_rects: Vec<(String, usize, String, Rect)>, // (panel_id, idx, path, rect)
    pub mcp_row_rects: Vec<(String, String, Rect)>,     // (panel_id, name, rect)
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

    pub fn hit_test_minimize(&self, col: u16, row: u16) -> Option<String> {
        self.panels
            .iter()
            .filter(|p| col >= p.rect.x && col < p.rect.x + 3 && row == p.rect.y + 2)
            .max_by_key(|p| p.z)
            .map(|p| p.id.clone())
    }

    pub fn hit_test_maximize(&self, col: u16, row: u16) -> Option<String> {
        self.panels
            .iter()
            .filter(|p| col >= p.rect.x && col < p.rect.x + 3 && row == p.rect.y + 3)
            .max_by_key(|p| p.z)
            .map(|p| p.id.clone())
    }

    pub fn hit_test_close(&self, col: u16, row: u16) -> Option<String> {
        self.panels
            .iter()
            .filter(|p| {
                p.rect.height >= 8 && col >= p.rect.x && col < p.rect.x + 3 && row == p.rect.y + 5
            })
            .max_by_key(|p| p.z)
            .map(|p| p.id.clone())
    }

    pub fn hit_test_resize(&self, col: u16, row: u16) -> Option<String> {
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

    pub fn hit_test_panel(&self, col: u16, row: u16) -> Option<&WindowInstance> {
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
