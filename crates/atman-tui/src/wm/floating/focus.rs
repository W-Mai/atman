use crate::wm::WindowId;

use super::WindowInstance;

#[derive(Debug, Default, Clone)]
pub struct FocusState {
    pub active: Option<WindowId>,
    history: Vec<WindowId>,
}

impl FocusState {
    pub fn focus(&mut self, id: WindowId) {
        self.history.retain(|h| *h != id);
        self.history.insert(0, id);
        self.active = Some(id);
    }

    pub fn blur(&mut self) {
        self.active = None;
    }

    pub fn remove_and_refocus(&mut self, id: WindowId, panels: &[WindowInstance]) {
        self.history.retain(|h| *h != id);
        let live: std::collections::HashSet<WindowId> = panels.iter().map(|p| p.id).collect();
        self.active = self.history.iter().find(|h| live.contains(h)).copied();
    }
}
