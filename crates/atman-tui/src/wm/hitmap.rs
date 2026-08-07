use super::component::HitRegion;
use super::window::WindowId;

/// Aggregated hitmap from all floating windows. Used by the WM for
/// top-down mouse dispatch with click swallowing.
#[derive(Debug, Default)]
pub struct WmHitmap {
    /// (window_id, regions) pairs, ordered by z (low to high).
    pub entries: Vec<(WindowId, Vec<HitRegion>)>,
}

impl WmHitmap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&mut self, id: WindowId, regions: Vec<HitRegion>) {
        self.entries.push((id, regions));
    }

    /// Find the topmost window whose region contains the given point.
    /// Searches from highest z to lowest.
    pub fn topmost_at(&self, col: u16, row: u16) -> Option<WindowId> {
        self.entries.iter().rev().find_map(|(id, regions)| {
            if regions.iter().any(|r| {
                col >= r.rect.x
                    && col < r.rect.x + r.rect.width
                    && row >= r.rect.y
                    && row < r.rect.y + r.rect.height
            }) {
                Some(*id)
            } else {
                None
            }
        })
    }

    /// Check if any window's regions contain the point (for click swallowing).
    pub fn any_contains(&self, col: u16, row: u16) -> bool {
        self.entries.iter().any(|(_, regions)| {
            regions.iter().any(|r| {
                col >= r.rect.x
                    && col < r.rect.x + r.rect.width
                    && row >= r.rect.y
                    && row < r.rect.y + r.rect.height
            })
        })
    }
}
