use ratatui::layout::Rect;

/// Semantic layer ordering. The integer value determines render and dispatch
/// priority: lower = renders first (bottom), higher = dispatch first (top).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LayerKind {
    Base,
    Docked,
    Floating,
    Modal,
    Blocking,
    Toast,
}

impl LayerKind {
    /// Render order: lowest first.
    pub fn render_order(self) -> u8 {
        self as u8
    }

    /// Dispatch order: highest first.
    pub fn dispatch_priority(self) -> u8 {
        self as u8
    }

    /// Whether this layer captures keyboard events.
    pub fn captures_keyboard(self) -> bool {
        matches!(self, LayerKind::Modal | LayerKind::Blocking)
    }
}

/// A layer in the stack. In v1, FloatingLayer holds the window list;
/// other layers are thin wrappers during migration.
#[derive(Debug)]
pub struct Layer {
    pub kind: LayerKind,
}

impl Layer {
    pub fn new(kind: LayerKind) -> Self {
        Self { kind }
    }
}

/// Check if a point is inside a rect. Matches the convention used throughout
/// the codebase (`rect_contains`).
pub fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}
