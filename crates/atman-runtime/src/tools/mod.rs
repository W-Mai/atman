use std::sync::Arc;

use crate::tool::ToolRegistry;

pub mod fs;
pub mod memory_stubs;

pub fn register_tier_zero(reg: &mut ToolRegistry) {
    reg.register(Arc::new(fs::FsRead));
    reg.register(Arc::new(fs::FsList));
    reg.register(Arc::new(memory_stubs::FetchConfessions));
}
