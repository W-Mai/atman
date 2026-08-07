//! Window Manager module — trait definitions and type skeleton.
//!
//! This module defines the architectural types for the future unified WM.
//! Nothing here is wired up yet; existing code continues to use
//! `floating_panels::FloatingPanels` directly.

pub mod component;
pub mod focus;
pub mod hitmap;
pub mod layer;
pub mod modal;
pub mod modal_wrappers;
pub mod window;

pub use component::{
    CloseOutcome, EventCtx, HitRegion, HitTarget, RenderCtx, SizeHint, WmCommand, WmEvent,
    WmEventResult, WindowComponent,
};
pub use focus::FocusState;
pub use hitmap::WmHitmap;
pub use layer::{Layer, LayerKind};
pub use modal::{HitTestResult, ModalComponent, ModalEntry, OutsideClickPolicy};
pub use window::{
    ContentKey, OpenPolicy, WindowCapabilities, WindowContent, WindowId, WindowInstance,
    WindowMode, WindowState,
};
