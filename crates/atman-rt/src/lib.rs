//! Portable flow program types and statement execution control.
#![no_std]

extern crate alloc;

pub mod ast;
pub mod engine;
pub mod env;
pub mod ops;
pub mod pattern;
pub mod value;

pub use ast::File as Program;
pub use engine::{
    Engine, HostFuture, LoopExit, LoopHost, Preflight, StatementExecution, StatementHost,
    StatementOutcome, run_loop, run_when,
};
pub use env::Env;
pub use ops::{EvalError, HostValueOps, ValueError, eval_binary, eval_literal, eval_unary};
pub use pattern::{PatternBindError, PatternValue, bind_pattern};
pub use value::{HostPayload, Value};
