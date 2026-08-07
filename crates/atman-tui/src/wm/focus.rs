use super::window::WindowId;

/// Focus state for floating windows. Tracks active window and a focus
/// history stack (most-recent-first) for correct refocus after close.
#[derive(Debug, Clone, Default)]
pub struct FocusState {
    pub active: Option<WindowId>,
    history: Vec<WindowId>,
}

impl FocusState {
    pub fn focus(&mut self, id: WindowId) {
        self.history.retain(|&h| h != id);
        self.history.insert(0, id);
        self.active = Some(id);
    }

    pub fn blur(&mut self) {
        self.active = None;
    }

    /// Remove a window from focus history and select the next focusable
    /// candidate by history (not by Vec position).
    pub fn remove_and_refocus<F>(&mut self, id: WindowId, is_focusable: F)
    where
        F: Fn(WindowId) -> bool,
    {
        self.history.retain(|&h| h != id);
        self.active = self
            .history
            .iter()
            .copied()
            .find(|&h| is_focusable(h));
    }

    /// Iterate focus history (most-recent-first).
    pub fn history(&self) -> &[WindowId] {
        &self.history
    }
}
