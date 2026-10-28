//! Named metadata edits over the invocation-bound flow client.

use serde::Deserialize;
use uuid::Uuid;

use crate::flow_store::name::NameSource;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::tool_schedule::{AccessMode, ResourceAccess, ResourceKey, SchedulingClaims};
use crate::{RuntimeError, Value, ValueJson};

pub struct FlowUpdate;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    name: String,
    handle: Option<String>,
    expected_name_revision: Option<u64>,
    request_id: Option<Uuid>,
    #[serde(default)]
    automatic: bool,
}

fn failed(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolFailed(format!("flow.update: {error}"))
}

impl Tool for FlowUpdate {
    fn name(&self) -> &str {
        "flow.update"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Update a persistent flow's display name. Omit handle for the current flow. Requires control access to another flow. Names persist independently of entry, session title, Goal, and execution state. expected_name_revision from flow.current/status protects concurrent edits; request_id makes retries idempotent. automatic is a conditional, best-effort update of the current flow's provisional name and requires expected_name_revision.",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object", "additionalProperties": false,
            "properties": {
                "name": {"type": "string", "minLength": 1},
                "handle": {"type": "string", "description": super::agent_ctrl::instance::HANDLE_DESCRIPTION},
                "expected_name_revision": {"type": "integer", "minimum": 0},
                "request_id": {"type": "string", "format": "uuid"},
                "automatic": {"type": "boolean", "default": false}
            }, "required": ["name"]
        })
    }
    fn scheduling_claims(
        &self,
        args: &ToolArgs,
        ctx: &ToolCtx,
    ) -> Result<SchedulingClaims, RuntimeError> {
        let client = ctx
            .instance_client()?
            .ok_or_else(|| failed("persistent flow required"))?;
        let address = match args.named("handle") {
            Some(Value::Str(handle)) => client.resolve_address(handle).map_err(failed)?,
            None => client.source().clone(),
            _ => return Err(failed("handle must be a string")),
        };
        Ok(SchedulingClaims::declared([ResourceAccess::new(
            ResourceKey::Flow(format!("{}:{}", address.session_id, address.instance_id)),
            AccessMode::Exclusive,
        )]))
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let mut fields = std::collections::HashSet::new();
            if !args.positional.is_empty()
                || args.named.iter().any(|(field, _)| !fields.insert(field))
            {
                return Err(failed("use unique named arguments"));
            }
            let update: Update =
                serde_json::from_value(Value::Struct(args.named).to_json()?).map_err(failed)?;
            let client = ctx
                .instance_client()?
                .ok_or_else(|| failed("persistent flow required"))?;
            let target = match update.handle {
                Some(handle) => client.resolve_address(&handle).map_err(failed)?,
                None => client.source().clone(),
            };
            if update.automatic
                && (&target != client.source() || update.expected_name_revision.is_none())
            {
                return Err(failed(
                    "automatic updates require the current flow and expected_name_revision",
                ));
            }
            let result = client
                .update_name(
                    target,
                    update.request_id.unwrap_or_else(Uuid::now_v7),
                    update.expected_name_revision,
                    update.name.trim().to_owned(),
                    if update.automatic {
                        NameSource::Automatic
                    } else {
                        NameSource::Explicit
                    },
                )
                .await;
            match result {
                Ok(record) => Ok(Value::from_json(
                    serde_json::json!({ "updated": true, "name": record.snapshot.name, "naming": record.snapshot, "receipt": record.receipt }),
                )),
                Err(error) if update.automatic => Ok(Value::from_json(
                    serde_json::json!({ "updated": false, "error": error.to_string() }),
                )),
                Err(error) => Err(failed(error)),
            }
        })
    }
}
