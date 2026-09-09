use std::sync::Arc;

use crate::tool::ToolRegistry;

pub mod agent_ctrl;
pub mod anchor;
pub mod anchor_fs;
pub mod bash_bg;
pub mod context;
pub mod final_answer;
pub mod flow_check;
pub mod flow_list;
pub mod flow_source;
pub mod form;
pub mod fs;
pub mod git;
pub mod git_branch;
pub mod git_history;
pub mod git_ops;
pub mod git_workspace;
pub mod git_worktree;
pub mod help;
pub mod hunk;
pub mod image;
pub mod llm_call;
pub mod llm_classify;
pub mod llm_extract;
pub mod llm_generate_branches;
pub mod mcp;
pub mod memory;
pub mod memory_stubs;
pub mod permission;
pub mod plan;
pub mod preview;
pub mod session;
pub mod sleep;
pub mod stdlib;
pub mod task_ops;
pub mod term;
pub mod test;
pub mod tool_output;
pub mod web;

pub fn register_tier_zero(reg: &ToolRegistry) {
    register_tier_zero_with_rules(reg, memory_stubs::RuleFetch::new());
}

pub fn register_tier_zero_with_rules(reg: &ToolRegistry, rule_fetch: memory_stubs::RuleFetch) {
    reg.register(Arc::new(anchor::AnchorRead));
    reg.register(Arc::new(anchor::AnchorEdit));
    reg.register(Arc::new(anchor::AnchorWrite));
    reg.register(Arc::new(anchor::AnchorUndo));
    reg.register(Arc::new(fs::FsRead));
    reg.register(Arc::new(image::ImageRead));
    reg.register(Arc::new(mcp::McpStatus));
    reg.register(Arc::new(mcp::McpTools));
    reg.register(Arc::new(mcp::McpAwait));
    reg.register(Arc::new(mcp::McpCall));
    reg.register(Arc::new(tool_output::OutputRead));
    reg.register(Arc::new(fs::FsList));
    reg.register(Arc::new(fs::FsWrite));
    reg.register(Arc::new(fs::FsEdit));
    reg.register(Arc::new(fs::FsGrep));
    reg.register(Arc::new(rule_fetch));
    reg.register(Arc::new(stdlib::ShellQuote));
    reg.register(Arc::new(stdlib::ToJsonString));
    reg.register(Arc::new(stdlib::ComposeEmailPreview));
    reg.register(Arc::new(stdlib::RenderPromptXml));
    reg.register(Arc::new(stdlib::RenderPromptMarkdown));
    reg.register(Arc::new(stdlib::RenderPromptTerse));
    reg.register(Arc::new(stdlib::EstimateTokens));
    reg.register(Arc::new(stdlib::FindCompactRange));
    reg.register(Arc::new(stdlib::ReplaceMessagesRange));
    reg.register(Arc::new(stdlib::Len));
    reg.register(Arc::new(stdlib::Head));
    reg.register(Arc::new(stdlib::Tail));
    reg.register(Arc::new(stdlib::IsEmpty));
    reg.register(Arc::new(stdlib::Concat));
    reg.register(Arc::new(stdlib::TextConcat));
    reg.register(Arc::new(final_answer::FinalAnswer));
    reg.register(Arc::new(final_answer::ExtractFinalAnswer));
    reg.register(Arc::new(stdlib::ExtractToolUses));
    reg.register(Arc::new(stdlib::DispatchAll));
    reg.register(Arc::new(context::ContextRecordAppend));
    reg.register(Arc::new(permission::PermissionList));
    reg.register(Arc::new(permission::PermissionGet));
    reg.register(Arc::new(permission::PermissionGroupTool));
    reg.register(Arc::new(permission::PermissionUngroup));
    reg.register(Arc::new(permission::PermissionApprove));
    reg.register(Arc::new(permission::PermissionDeny));
    reg.register(Arc::new(permission::PermissionDefer));
    reg.register(Arc::new(permission::PermissionBatch));
    reg.register(Arc::new(stdlib::MessageUser));
    reg.register(Arc::new(stdlib::MessageAssistant));
    reg.register(Arc::new(stdlib::MessageSystem));
    reg.register(Arc::new(stdlib::MessageTool));
    reg.register(Arc::new(git::GitDiff));
    reg.register(Arc::new(git_ops::GitInit));
    reg.register(Arc::new(git_ops::GitShow));
    reg.register(Arc::new(git_ops::GitLog));
    reg.register(Arc::new(git_ops::GitStatus));
    reg.register(Arc::new(test::TestRun));
    reg.register(Arc::new(hunk::FsEdit));
    reg.register(Arc::new(hunk::HunkApply));
    reg.register(Arc::new(hunk::HunkReview));
    reg.register(Arc::new(agent_ctrl::AgentSpawn));
    reg.register(Arc::new(agent_ctrl::AgentStatus));
    reg.register(Arc::new(agent_ctrl::AgentOutput));
    reg.register(Arc::new(agent_ctrl::AgentKill));
    reg.register(Arc::new(agent_ctrl::FlowInterject));
    reg.register(Arc::new(form::FormAsk));
    reg.register(Arc::new(session::SessionPush));
    reg.register(Arc::new(sleep::Sleep));
    reg.register(Arc::new(help::HelpShow));
    reg.register(Arc::new(flow_list::FlowList));
    reg.register(Arc::new(flow_list::FlowInstances));
    reg.register(Arc::new(flow_list::FlowSearch));
    reg.register(Arc::new(flow_list::FlowDescribe));
    reg.register(Arc::new(flow_check::FlowCheck));
    reg.register(Arc::new(llm_call::LlmCallTool));
    reg.register(Arc::new(llm_classify::LlmClassifyTool));
    reg.register(Arc::new(llm_extract::LlmExtractTool));
    reg.register(Arc::new(llm_generate_branches::LlmGenerateBranchesTool));
}

pub fn register_git_ops(reg: &ToolRegistry) {
    reg.register(Arc::new(git_ops::GitAdd));
    reg.register(Arc::new(git_ops::GitCommit));
    reg.register(Arc::new(git_ops::GitFetch));
    reg.register(Arc::new(git_ops::GitBranch));
    reg.register(Arc::new(git_ops::GitPush));
    reg.register(Arc::new(git_history::GitRestore));
    reg.register(Arc::new(git_history::GitRevert));
    reg.register(Arc::new(git_history::GitTagList));
    reg.register(Arc::new(git_history::GitTagCreate));
    reg.register(Arc::new(git_branch::GitBranchList));
    reg.register(Arc::new(git_branch::GitBranchCreate));
    reg.register(Arc::new(git_branch::GitBranchSwitch));
    reg.register(Arc::new(git_branch::GitBranchRename));
    reg.register(Arc::new(git_branch::GitBranchDelete));
    reg.register(Arc::new(git_branch::GitRemoteList));
    reg.register(Arc::new(git_worktree::GitWorktreeAdd));
    reg.register(Arc::new(git_worktree::GitWorktreeList));
    reg.register(Arc::new(git_worktree::GitWorktreeRemove));
    reg.register(Arc::new(git_worktree::GitWorktreePrune));
    reg.register(Arc::new(git_worktree::GitWorktreeLock));
    reg.register(Arc::new(git_worktree::GitWorktreeUnlock));
    reg.register(Arc::new(git_workspace::GitWorkspaceCreate));
    reg.register(Arc::new(git_workspace::GitWorkspaceList));
    reg.register(Arc::new(git_workspace::GitWorkspaceGet));
    reg.register(Arc::new(git_workspace::GitWorkspaceRelease));
    reg.register(Arc::new(git_workspace::GitWorkspaceRetain));
    reg.register(Arc::new(git_workspace::GitWorkspacePrune));
}

pub fn register_watch(reg: &ToolRegistry) {
    reg.register(Arc::new(crate::watch::Watch));
    reg.register(Arc::new(crate::watch::WatcherList));
    reg.register(Arc::new(crate::watch::WatcherUnwatch));
    reg.register(Arc::new(crate::watch::WaitForWatcher));
    reg.register(Arc::new(crate::watch::HasPendingInjections));
}

pub fn register_bash_bg(reg: &ToolRegistry) -> Arc<bash_bg::BgRegistry> {
    let registry = Arc::new(bash_bg::BgRegistry::new());
    reg.register(Arc::new(bash_bg::BashSpawn));
    reg.register(Arc::new(bash_bg::BashStatus));
    reg.register(Arc::new(bash_bg::BashOutput));
    reg.register(Arc::new(bash_bg::BashKill));
    reg.register(Arc::new(bash_bg::BashList));
    registry
}

pub fn register_bash_bg_with_task_registry(
    reg: &ToolRegistry,
    task_registry: crate::task_registry::TaskRegistry,
) -> Arc<bash_bg::BgRegistry> {
    let registry = Arc::new(bash_bg::BgRegistry::new().with_task_registry(task_registry));
    reg.register(Arc::new(bash_bg::BashSpawn));
    reg.register(Arc::new(bash_bg::BashStatus));
    reg.register(Arc::new(bash_bg::BashOutput));
    reg.register(Arc::new(bash_bg::BashKill));
    reg.register(Arc::new(bash_bg::BashList));
    registry
}

pub fn register_web(reg: &ToolRegistry, config: web::WebConfig) {
    reg.register(Arc::new(web::WebFetch::new(config)));
}

pub fn register_web_search(reg: &ToolRegistry, config: &web::SearchConfig) {
    if let Some(provider) = web::build_search_provider(config) {
        reg.register(Arc::new(web::WebSearch::new(provider)));
    }
}

pub fn register_terminal(reg: &ToolRegistry) -> Arc<term::TermRegistry> {
    let registry = Arc::new(term::TermRegistry::new());
    reg.register(Arc::new(term::TermSpawn));
    reg.register(Arc::new(term::TermInput));
    reg.register(Arc::new(term::TermCapture));
    reg.register(Arc::new(term::TermFind));
    reg.register(Arc::new(term::TermResize));
    reg.register(Arc::new(term::TermKill));
    reg.register(Arc::new(term::TermList));
    registry
}

pub fn register_terminal_with_task_registry(
    reg: &ToolRegistry,
    task_registry: crate::task_registry::TaskRegistry,
) -> Arc<term::TermRegistry> {
    let registry = Arc::new(term::TermRegistry::new().with_task_registry(task_registry));
    reg.register(Arc::new(term::TermSpawn));
    reg.register(Arc::new(term::TermInput));
    reg.register(Arc::new(term::TermCapture));
    reg.register(Arc::new(term::TermFind));
    reg.register(Arc::new(term::TermResize));
    reg.register(Arc::new(term::TermKill));
    reg.register(Arc::new(term::TermList));
    registry
}

pub fn register_preview(reg: &ToolRegistry, config: preview::PreviewConfig) {
    reg.register(Arc::new(preview::PreviewPush::new(config)));
}

pub fn register_memory(
    reg: &ToolRegistry,
    todo_store: Arc<crate::memory::todo::TodoStore>,
    confession_store: Arc<crate::memory::confession::ConfessionStore>,
    goal_store: Arc<crate::memory::goal::GoalStore>,
    plan_store: Arc<crate::memory::plan::PlanStore>,
) {
    reg.register(Arc::new(memory::MemoryTodoSet {
        store: todo_store.clone(),
    }));
    reg.register(Arc::new(memory::MemoryTodoDone {
        store: todo_store.clone(),
    }));
    reg.register(Arc::new(memory::MemoryTodoCancel {
        store: todo_store.clone(),
    }));
    reg.register(Arc::new(memory::MemoryTodoDelete {
        store: todo_store.clone(),
    }));
    reg.register(Arc::new(memory::MemoryTodoList { store: todo_store }));
    reg.register(Arc::new(memory::MemoryConfess {
        store: confession_store.clone(),
    }));
    reg.register(Arc::new(memory::MemoryFetchConfessions {
        store: confession_store,
    }));
    reg.register(Arc::new(memory::MemoryGoalGet {
        store: goal_store.clone(),
    }));
    reg.register(Arc::new(memory::MemoryGoalSet {
        store: goal_store.clone(),
    }));
    reg.register(Arc::new(memory::MemoryGoalClear { store: goal_store }));
    reg.register(Arc::new(memory::MemoryRecentTurns));
    reg.register(Arc::new(memory::MemoryHistorySearch));
    reg.register(Arc::new(memory::MemoryHistoryRead));
    reg.register(Arc::new(memory::MemoryHistoryCount));
    reg.register(Arc::new(plan::PlanWrite {
        store: plan_store.clone(),
    }));
    reg.register(Arc::new(plan::PlanRead {
        store: plan_store.clone(),
    }));
    reg.register(Arc::new(plan::PlanTick { store: plan_store }));
}

pub fn register_spec_memory(reg: &ToolRegistry, spec_store: Arc<crate::memory::spec::SpecStore>) {
    reg.register(Arc::new(memory::MemorySpecStatus {
        store: spec_store.clone(),
    }));
    reg.register(Arc::new(memory::MemorySpecMaterialize {
        store: spec_store.clone(),
    }));
    reg.register(Arc::new(memory::MemorySpecUpdate {
        store: spec_store.clone(),
    }));
    reg.register(Arc::new(memory::MemorySpecDeviate { store: spec_store }));
}
