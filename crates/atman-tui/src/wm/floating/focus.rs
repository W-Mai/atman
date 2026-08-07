use super::WindowInstance;

#[derive(Debug, Default, Clone)]
pub struct FocusState {
    pub active: Option<String>,
    history: Vec<String>,
}

impl FocusState {
    pub fn focus(&mut self, id: &str) {
        self.history.retain(|h| h != id);
        self.history.insert(0, id.to_string());
        self.active = Some(id.to_string());
    }

    pub fn blur(&mut self) {
        self.active = None;
    }

    pub fn remove_and_refocus(&mut self, id: &str, panels: &[WindowInstance]) {
        self.history.retain(|h| h != id);
        let live: std::collections::HashSet<&str> = panels.iter().map(|p| p.id.as_str()).collect();
        self.active = self
            .history
            .iter()
            .find(|h| live.contains(h.as_str()))
            .cloned();
    }
}
