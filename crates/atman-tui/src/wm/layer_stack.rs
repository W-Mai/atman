use crate::wm::layer::LayerKind;

pub struct LayerStack {
    pub layers: Vec<LayerKind>,
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
        }
    }

    pub fn dispatch_key(&self) -> bool {
        false
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
