use std::sync::Arc;

use crate::tool::ToolRegistry;

pub mod fs;

pub fn register_tier_zero(reg: &mut ToolRegistry) {
    reg.register(Arc::new(fs::FsRead));
    reg.register(Arc::new(fs::FsList));
}
