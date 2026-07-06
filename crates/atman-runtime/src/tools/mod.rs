use std::sync::Arc;

use crate::tool::ToolRegistry;

pub mod bash;
pub mod fs;
pub mod git;
pub mod hunk;
pub mod memory;
pub mod memory_stubs;
pub mod preview;
pub mod stdlib;
pub mod test;
pub mod web;

pub fn register_tier_zero(reg: &mut ToolRegistry) {
    register_tier_zero_with_rules(reg, memory_stubs::FetchRule::new());
}

pub fn register_tier_zero_with_rules(reg: &mut ToolRegistry, fetch_rule: memory_stubs::FetchRule) {
    reg.register(Arc::new(fs::FsRead));
    reg.register(Arc::new(fs::FsList));
    reg.register(Arc::new(fs::FsWrite));
    reg.register(Arc::new(memory_stubs::FetchConfessions));
    reg.register(Arc::new(fetch_rule));
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
    reg.register(Arc::new(stdlib::ExtractToolUses));
    reg.register(Arc::new(stdlib::DispatchAll));
    reg.register(Arc::new(stdlib::ListMap));
    reg.register(Arc::new(stdlib::ListFilter));
    reg.register(Arc::new(stdlib::ListFind));
    reg.register(Arc::new(stdlib::ListAny));
    reg.register(Arc::new(stdlib::ListAll));
    reg.register(Arc::new(stdlib::ListReduce));
    reg.register(Arc::new(git::GitDiff));
    reg.register(Arc::new(test::TestRun));
    reg.register(Arc::new(hunk::FsEdit));
    reg.register(Arc::new(hunk::HunkApply));
    reg.register(Arc::new(hunk::HunkReview));
}

pub fn register_shell(reg: &mut ToolRegistry) {
    reg.register(Arc::new(bash::BashExec));
}

pub fn register_web(reg: &mut ToolRegistry, config: web::WebConfig) {
    reg.register(Arc::new(web::WebFetch::new(config)));
}

pub fn register_web_search(reg: &mut ToolRegistry, provider: Arc<dyn web::SearchProvider>) {
    reg.register(Arc::new(web::WebSearch::new(provider)));
}

pub fn register_preview(reg: &mut ToolRegistry, config: preview::PreviewConfig) {
    reg.register(Arc::new(preview::PreviewPush::new(config)));
}

pub fn register_memory(
    reg: &mut ToolRegistry,
    todo_store: Arc<crate::memory::todo::TodoStore>,
    confession_store: Arc<crate::memory::confession::ConfessionStore>,
    goal_store: Arc<crate::memory::goal::GoalStore>,
) {
    reg.register(Arc::new(memory::MemoryTodoSet {
        store: todo_store.clone(),
    }));
    reg.register(Arc::new(memory::MemoryTodoDone { store: todo_store }));
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
}

pub fn register_spec_memory(
    reg: &mut ToolRegistry,
    spec_store: Arc<crate::memory::spec::SpecStore>,
) {
    reg.register(Arc::new(memory::MemorySpecStatus {
        store: spec_store.clone(),
    }));
    reg.register(Arc::new(memory::MemorySpecUpdate {
        store: spec_store.clone(),
    }));
    reg.register(Arc::new(memory::MemorySpecDeviate { store: spec_store }));
}
