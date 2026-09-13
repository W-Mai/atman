use std::collections::{HashMap, HashSet};

use ratatui::Frame;
use ratatui::layout::Rect;

use crate::app::AppState;
use crate::wm::layer::LayerKind;
use crate::wm::{ModalEntry, ModalKind};

pub struct LayerStack {
    pub layers: Vec<LayerKind>,
    pub modal_stack: Vec<ModalEntry>,
}

impl LayerStack {
    pub fn new() -> Self {
        Self {
            layers: vec![
                LayerKind::Base,
                LayerKind::Docked,
                LayerKind::Floating,
                LayerKind::Modal,
                LayerKind::Blocking,
                LayerKind::Toast,
            ],
            modal_stack: Vec::new(),
        }
    }

    pub fn sync_from_kinds(&mut self, open: &[ModalKind]) {
        let open_set: HashSet<_> = open.iter().copied().collect();
        let saved_focus: HashMap<_, _> = self
            .modal_stack
            .iter()
            .map(|entry| (entry.kind, entry.pre_modal_focus))
            .collect();

        self.modal_stack
            .retain(|entry| open_set.contains(&entry.kind));
        for &kind in open {
            if self.modal_stack.iter().all(|entry| entry.kind != kind) {
                self.modal_stack.push(ModalEntry {
                    kind,
                    pre_modal_focus: None,
                });
            }
        }
        if let Some(root) = self.modal_stack.first_mut()
            && root.pre_modal_focus.is_none()
        {
            root.pre_modal_focus = saved_focus.get(&root.kind).cloned().flatten();
        }
    }

    pub fn dispatch_key(&self) -> Option<ModalKind> {
        self.modal_stack.last().map(|entry| entry.kind)
    }

    pub fn render_blocking(&self, f: &mut Frame, area: Rect, app: &AppState) {
        if let Some(ref msg) = app.modal_notification {
            crate::render_notify_modal(f, area, msg);
        }
    }

    pub fn render_toasts(&self, f: &mut Frame, area: Rect, app: &AppState) {
        crate::render_toast_notes(f, area, &app.toasts);
    }

    pub fn dispatch_mouse(&self) -> bool {
        false
    }
}

impl Default for LayerStack {
    fn default() -> Self {
        Self::new()
    }
}
