use crate::error::RuntimeError;
use crate::help;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct HelpShow;

impl Tool for HelpShow {
    fn name(&self) -> &str {
        "help.show"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Show built-in help documentation. Pass `topic` to get specific docs: \
             'index' (default), 'tools', 'cli', 'config', 'mcp', 'dsl', 'features'. \
             Content is version-synced — dynamic topics are generated from the live \
             ToolRegistry and META_COMMANDS, config/mcp topics are read from /// doc \
             comments via the documented crate. Returns markdown text.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "topic": {
                    "type": "string",
                    "default": "index",
                    "enum": ["index", "tools", "cli", "config", "mcp", "dsl", "features"],
                    "description": "Help topic to display. Defaults to 'index' (topic list)."
                }
            }
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let topic = match args.named("topic") {
                Some(Value::Str(s)) => s.as_str().to_string(),
                _ => "index".to_string(),
            };
            match help::topic_content(&topic, ctx) {
                Some(content) => Ok(Value::Str(content)),
                None => {
                    let available: Vec<&str> = help::TOPICS.iter().map(|t| t.id).collect();
                    Err(RuntimeError::ToolFailed(format!(
                        "unknown help topic '{topic}'. available: {}",
                        available.join(", ")
                    )))
                }
            }
        })
    }
}
