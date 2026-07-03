use std::sync::Arc;

use crate::tool::ToolRegistry;

pub mod bash;
pub mod fs;
pub mod memory;
pub mod memory_stubs;
pub mod stdlib;
pub mod web;

pub fn register_tier_zero(reg: &mut ToolRegistry) {
    reg.register(Arc::new(fs::FsRead));
    reg.register(Arc::new(fs::FsList));
    reg.register(Arc::new(fs::FsWrite));
    reg.register(Arc::new(memory_stubs::FetchConfessions));
    reg.register(Arc::new(stdlib::ShellQuote));
    reg.register(Arc::new(stdlib::ComposeEmailPreview));
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

pub fn register_memory(
    reg: &mut ToolRegistry,
    todo_store: Arc<crate::memory::todo::TodoStore>,
    confession_store: Arc<crate::memory::confession::ConfessionStore>,
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
}
