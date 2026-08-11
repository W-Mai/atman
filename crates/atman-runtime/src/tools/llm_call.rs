use crate::error::RuntimeError;
use crate::eval::llm_args::parse_llm_args_from_toolargs;
use crate::eval::llm_dispatch::dispatch_llm;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};

pub struct LlmCallTool;

impl Tool for LlmCallTool {
    fn name(&self) -> &str {
        "llm.call"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let Some(registry) = ctx.registry.as_deref() else {
                return Err(RuntimeError::ToolFailed(
                    "llm.call: no tool registry available".into(),
                ));
            };
            let llm_args = parse_llm_args_from_toolargs(&args, registry)?;
            Ok(dispatch_llm(llm_args, ctx, None).await)
        })
    }
}
