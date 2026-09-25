//! Portable flow program types and statement execution control.
#![no_std]

extern crate alloc;

pub mod ast;
pub mod cancel;
pub mod engine;
pub mod env;
pub mod expr;
pub mod lifecycle;
pub mod ops;
pub mod pattern;
pub mod status;
pub mod value;

pub use ast::File as Program;
pub use cancel::race_cancel;
pub use engine::{
    Engine, FlowArgs, FlowExecution, FlowOutcome, HostFuture, LoopExit, LoopHost, Preflight,
    StatementExecution, StatementHost, StatementOutcome, run_loop, run_when,
};
pub use env::Env;
pub use expr::{ExpressionEffect, ExpressionHost, eval_expr};
pub use lifecycle::{FlowEndFact, FlowLifecycle, FlowStartFact, StartedFlow};
pub use ops::{EvalError, HostValueOps, ValueError, eval_binary, eval_literal, eval_unary};
pub use pattern::{PatternBindError, PatternValue, bind_pattern};
pub use status::{FlowTermination, classify_outcome, classify_result};
pub use value::{HostPayload, Value};
