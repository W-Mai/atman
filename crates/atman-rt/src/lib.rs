//! Portable flow program types and statement execution control.
#![no_std]

extern crate alloc;

pub mod ast;
pub mod engine;
pub mod env;
pub mod pattern;

pub use ast::File as Program;
pub use engine::{
    Engine, HostFuture, Preflight, StatementExecution, StatementHost, StatementOutcome,
};
pub use env::Env;
pub use pattern::{PatternBindError, PatternValue, bind_pattern};
