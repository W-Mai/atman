//! Embeddable Atman language VM, including source compilation and flow execution.
//! Embedding requires an allocator, pointer-width atomics, and a host future executor.
#![no_std]

extern crate alloc;

pub mod ast;
pub mod cancel;
pub mod engine;
pub mod env;
pub mod expr;
pub mod fanout;
pub mod fix;
pub mod identity;
pub mod lifecycle;
pub mod lint;
pub mod list;
pub mod ops;
#[cfg(feature = "syntax")]
pub mod parse;
pub mod pattern;
pub mod print;
pub mod program;
pub mod redirect;
pub mod route;
pub mod status;
pub mod validate;
pub mod value;
pub mod vm;
pub mod watch;

pub use cancel::race_cancel;
pub use engine::{
    CallArgumentError, Engine, FlowArgs, FlowExecution, FlowOutcome, HostFuture, LoopExit,
    LoopHost, Preflight, StatementExecution, StatementHost, StatementOutcome, bind_call_arguments,
    run_loop, run_when,
};
pub use env::Env;
pub use expr::{
    ExpressionEffect, ExpressionHost, eval_dynamic_fanout, eval_expr, eval_fanout, is_type_name,
};
pub use fanout::join_fanout_all;
pub use identity::{IdSource, RunId, TurnId};
pub use lifecycle::{FlowEndFact, FlowLifecycle, FlowStartFact, StartedFlow};
pub use lint::{LintHit, LintRule, lint_file};
pub use list::{ListIntrinsic, eval_list_intrinsic};
pub use ops::{EvalError, HostValueOps, ValueError, eval_binary, eval_literal, eval_unary};
#[cfg(feature = "syntax")]
pub use parse::{ParseError, parse_file};
pub use pattern::{PatternBindError, PatternValue, bind_pattern};
pub use print::print_file;
pub use program::{LinkedProgram as Program, Source, SourceResolver};
pub use redirect::{RedirectOutcome, run_redirects};
pub use route::{RouteMatch, resolve_route};
pub use status::{FlowTermination, classify_outcome, classify_result};
pub use validate::{LanguageValidationError, LanguageValidationReport, validate_flow};
pub use value::{FlowFuture, HostPayload, Value};
pub use vm::{FlowCall, FlowDriveMode, Vm, VmCallError, VmEmbedding, VmHost};
pub use watch::{WatchObservation, WatchRules, WatchState, WatchWarning};
