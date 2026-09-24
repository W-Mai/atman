pub mod activity;
pub mod approval;
pub mod attachment_store;
pub mod auth_store;
pub mod compaction;
pub mod config_hub;
pub mod config_migration;
pub mod config_provider;
pub mod context_plan;
pub mod cost;
pub mod env;
pub mod error;
pub mod eval;
pub mod event;
pub mod event_log;
pub mod event_writer;
pub mod exec;
pub mod executor;
pub mod flow_authority;
pub mod flow_lint;
pub mod flow_meta;
pub mod flow_registry;
pub mod flow_workspace;
pub mod form;
pub mod fs_access;
pub mod git;
pub mod git_workspace;
pub mod help;
pub mod history_store;
pub mod humanize;
pub mod hunk;
pub mod index;
pub mod injection;
pub mod injection_classifier;
pub mod invocation_env;
pub mod known_models;
pub mod lifecycle;
pub mod mcp;
pub mod mcp_config;
pub mod memory;
pub mod message;
pub mod message_stream;
pub mod meta_commands;
pub mod migration;
pub mod model_registry;
pub mod nodegraph;
pub mod notify;
pub mod oauth;
pub mod oauth_server;
mod panic_capture;

/// Reports whether the current thread is inside an active panic-capture
/// boundary. Panic hooks can use this to skip side effects for a panic that
/// will be caught.
#[doc(hidden)]
pub fn is_panic_capture_active() -> bool {
    panic_capture::is_active()
}

/// Contains a synchronous panic at a fallible external boundary.
///
/// The TUI panic hook leaves the terminal active while this scope catches the panic.
#[doc(hidden)]
pub fn capture_blocking_panic<T>(operation: impl FnOnce() -> T) -> Option<T> {
    panic_capture::blocking(operation).ok()
}

pub mod permission;
pub mod permission_audit;
pub mod project_catalog;
pub mod projection;
pub mod provider;
pub mod provider_lifecycle;
pub mod providers;
pub mod redact;
pub mod rendezvous;
pub mod routing;
pub mod safety;
pub mod sandbox;
pub mod session;
pub mod session_meta;
pub mod session_naming;
pub mod settings_catalog;
pub mod storage;
pub mod stream;
pub(crate) mod streaming;
pub mod submission_queue;
pub mod task_registry;
pub mod templates;
pub mod tool;
pub mod tool_naming;
pub mod tools;
pub mod trust;
pub mod user_input;
pub mod validate;
pub mod value;
pub use watch::WatchHub;
pub mod watch;
pub mod workflow;

pub use attachment_store::AttachmentStore;
pub use context_plan::{
    ContentDigest, ContextCacheObservation, ContextCachePlan, ContextCacheResetReason,
    ContextCallIdentity, ContextCallPurpose, ContextCallScope, ContextEpoch, ContextPlanId,
    ContextPrefixLane, ContextPrefixProfile, ContextPrefixSnapshot, ContextRecord,
    ContextRecordAuthority, ContextRecordBody, ContextRecordRetention, ContextRecordSpec,
    ContextTokenLanes, ContextUsageKey, ContextUsageRecord, ModelContextPlan, TokenUsageSource,
};
pub use cost::{CostSummary, summarize_by_model, summarize_by_provider, total};
pub use env::Env;
pub use error::RuntimeError;
pub use eval::{EvalCtx, eval_expr};
pub use event::{
    Event, EventSink, FlowRunId, FlowStatus, LlmCallStatus, NodeEvent, Observable, TurnId,
};
pub use executor::{Executor, ProviderLifecycleAlreadyAttached, RootInvocation};
pub use hunk::{ApplyError, EditProposal, Hunk, HunkLine};
pub use injection::{Injection, InjectionId, InjectionSource, InjectionState};
pub use invocation_env::InvocationEnv;
pub use message::{ImageData, ImageSource, Message, MessagePart, MessageRole};
pub use projection::message_window::{TranscriptEntry, replay_transcript_from};
pub use provider::{LlmRequest, Provider, ProviderCapabilities, ProviderRegistry, TokenUsage};
pub use provider_lifecycle::{
    ProviderCatalogRefreshOutcome, ProviderLifecycle, ProviderLifecycleError,
    ProviderLifecycleOutcome, ProviderReconcileOutcome, ProviderStateChange,
};
pub use session::{
    CompactReviewDecision, CompactReviewMode, CompactReviewRegistry, ContextSnapshot,
    ContextUsageBucket, PendingCompactReview, Session, SessionId,
};
pub use submission_queue::{
    QueuedSubmission, QueuedSubmissionView, SubmissionId, SubmissionMove, SubmissionQueueError,
};
pub use task_registry::{
    TaskDisplay, TaskEvent, TaskFilter, TaskId, TaskKind, TaskRegistry, TaskSnapshot, TaskStatus,
};
pub use tool::{CancelBehavior, Tier, Tool, ToolArgs, ToolCtx, ToolRegistry, ToolResult};
pub use tool_naming::{ToolNaming, from_wire, to_wire};
pub use validate::{ValidationError, validate};
pub use value::Value;
pub use workflow::{NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind};
