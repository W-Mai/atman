//! Durable notifications addressed to the invoking persistent Flow.

use chrono::{DateTime, Utc};
use serde::{Deserialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::RuntimeError;
use crate::flow_store::timer::Timer;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::{Value, ValueJson};

pub enum TimerTool {
    Schedule,
    Cancel,
    List,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Schedule {
    id: Uuid,
    due_at: DateTime<Utc>,
    content: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cancel {
    id: Uuid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {}

fn failed(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolFailed(format!("timer: {error}"))
}

fn parse<T: DeserializeOwned>(args: ToolArgs) -> Result<T, RuntimeError> {
    let mut names = std::collections::HashSet::new();
    if !args.positional.is_empty() || args.named.iter().any(|(name, _)| !names.insert(name)) {
        return Err(failed("use unique named arguments"));
    }
    serde_json::from_value(Value::Struct(args.named).to_json()?).map_err(failed)
}

impl Tool for TimerTool {
    fn name(&self) -> &str {
        match self {
            Self::Schedule => "timer.schedule",
            Self::Cancel => "timer.cancel",
            Self::List => "timer.list",
        }
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(match self {
            Self::Schedule => {
                "Schedule one durable notification for this flow at an RFC 3339 timestamp with an explicit timezone. Supply a stable non-nil UUID id; identical retries retain the original deadline and status. Notification content enters the flow inbox at the deadline, survives turn end and restart, and waits for execution pause to be released. Past deadlines are delivered promptly. No external command is run."
            }
            Self::Cancel => {
                "Cancel this flow's pending timer by id. Already accepted notifications remain in the inbox; cancellation never retracts consumed input."
            }
            Self::List => {
                "List this flow's durable timer registrations, deadlines, content, and scheduled/fired/cancelled status. Listing does not trigger deadlines or consume notifications."
            }
        })
    }

    fn input_schema(&self) -> serde_json::Value {
        match self {
            Self::Schedule => serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "id": {"type": "string", "format": "uuid"},
                    "due_at": {"type": "string", "format": "date-time"},
                    "content": {"description": "JSON notification content delivered to this flow."}
                },
                "required": ["id", "due_at", "content"]
            }),
            Self::Cancel => serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {"id": {"type": "string", "format": "uuid"}},
                "required": ["id"]
            }),
            Self::List => serde_json::json!({
                "type": "object", "additionalProperties": false, "properties": {}
            }),
        }
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let client = ctx
                .instance_client()?
                .ok_or_else(|| failed("persistent instance ownership is required"))?;
            let result = match self {
                Self::Schedule => {
                    let Schedule {
                        id,
                        due_at,
                        content,
                    } = parse(args)?;
                    serde_json::to_value(
                        client
                            .schedule_timer(Timer {
                                id,
                                due_at,
                                content,
                            })
                            .await
                            .map_err(failed)?,
                    )
                }
                Self::Cancel => {
                    let Cancel { id } = parse(args)?;
                    serde_json::to_value(client.cancel_timer(id).await.map_err(failed)?)
                }
                Self::List => {
                    let List {} = parse(args)?;
                    serde_json::to_value(client.timers().await.map_err(failed)?)
                }
            }
            .map_err(failed)?;
            Ok(Value::from_json(result))
        })
    }
}
