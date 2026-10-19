//! Invocation-bound batch controls over persistent flow instances.

use serde::Deserialize;
use uuid::Uuid;

use crate::RuntimeError;
use crate::flow_runtime::access::PeerControl;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::{Value, ValueJson};

pub struct FlowControlBatch;
pub struct FlowControlStatus;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Batch {
    request_id: Uuid,
    commands: Vec<Command>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Command {
    handle: String,
    expected_revision: u64,
    action: PeerControl,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Status {
    request_id: Uuid,
}

fn failed(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolFailed(format!("flow control: {error}"))
}

fn parse<T: serde::de::DeserializeOwned>(args: ToolArgs) -> Result<T, RuntimeError> {
    let mut names = std::collections::HashSet::new();
    if !args.positional.is_empty() || args.named.iter().any(|(name, _)| !names.insert(name)) {
        return Err(failed("use unique named arguments"));
    }
    serde_json::from_value(Value::Struct(args.named).to_json()?).map_err(failed)
}

impl Tool for FlowControlBatch {
    fn name(&self) -> &str {
        "flow.control_batch"
    }

    fn tier(&self) -> Tier {
        Tier::Four
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Submit persistent flow controls with a stable request_id and one revision-bound command per target. Acceptance survives the caller's pause or closure. Read flow.control_status for per-target outcomes; acceptance is not completion. Identical retries must retain the original commands and request_id. Control grants are required; goals are unchanged.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object", "additionalProperties": false,
            "properties": {
                "request_id": {"type": "string", "format": "uuid"},
                "commands": {"type": "array", "minItems": 1, "items": {
                    "type": "object", "additionalProperties": false,
                    "properties": {
                        "handle": {"type": "string", "description": super::agent_ctrl::instance::HANDLE_DESCRIPTION},
                        "expected_revision": {"type": "integer", "minimum": 0},
                        "action": {"type": "string", "enum": ["pause", "resume", "cancel_turn", "recover", "close"]}
                    },
                    "required": ["handle", "expected_revision", "action"]
                }}
            },
            "required": ["request_id", "commands"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let batch: Batch = parse(args)?;
            if batch.request_id.is_nil() || batch.commands.is_empty() {
                return Err(failed(
                    "a non-nil request_id and at least one command are required",
                ));
            }
            let client = ctx
                .instance_client()?
                .ok_or_else(|| failed("persistent instance ownership is required"))?;
            let mut targets = Vec::with_capacity(batch.commands.len());
            let mut seen = std::collections::HashSet::new();
            for command in batch.commands {
                if command.action == PeerControl::ReleasePause {
                    return Err(failed("release_pause requires an explicit operator action"));
                }
                let target = client.resolve_address(&command.handle).map_err(failed)?;
                if target.session_id.0.is_nil()
                    || target.instance_id.0.0.is_nil()
                    || !seen.insert(target.clone())
                {
                    return Err(failed("commands require unique non-nil target addresses"));
                }
                targets.push((target, command.expected_revision, command.action));
            }
            let record = client
                .control_batch(batch.request_id, targets)
                .await
                .map_err(failed)?;
            Ok(Value::from_json(
                serde_json::to_value(record).map_err(failed)?,
            ))
        })
    }
}

impl Tool for FlowControlStatus {
    fn name(&self) -> &str {
        "flow.control_status"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Read this persistent flow's accepted control batch by request_id, including from later turns. Empty item history means pending; accepted, applied and failed retain durable receipts. Current control grants are required. Reading does not repeat controls or start target execution.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object", "additionalProperties": false,
            "properties": {"request_id": {"type": "string", "format": "uuid"}},
            "required": ["request_id"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let query: Status = parse(args)?;
            let client = ctx
                .instance_client()?
                .ok_or_else(|| failed("persistent instance ownership is required"))?;
            let record = client
                .control_status(query.request_id)
                .await
                .map_err(failed)?
                .ok_or_else(|| failed("control batch does not exist for this flow"))?;
            Ok(Value::from_json(
                serde_json::to_value(record).map_err(failed)?,
            ))
        })
    }
}
