//! Window Manager module — trait definitions, type skeleton, and the
//! floating-panel implementation (`floating` submodule).

pub mod component;
pub mod floating;
pub mod layer;
pub mod layer_stack;
pub mod modal;
pub mod modal_wrappers;
pub mod window;

pub use component::{
    CloseOutcome, EventCtx, HitRegion, HitTarget, RenderCtx, SizeHint, WindowComponent, WmCommand,
    WmEvent, WmEventResult,
};
pub use floating::{
    FocusState, PanelBtn, WindowInstance, WindowManager, WmHitmap, render, render_shadow,
};
pub use layer::{Layer, LayerKind};
pub use layer_stack::LayerStack;
pub use modal::{HitTestResult, ModalComponent, ModalEntry, OutsideClickPolicy};
pub use window::{
    ContentKey, OpenPolicy, WindowCapabilities, WindowContent, WindowId, WindowMode, WindowState,
};
