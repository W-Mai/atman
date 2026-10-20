//! Explicit cancellation groups owned by the invoking persistent Flow.

use serde::{Deserialize, de::DeserializeOwned};
use uuid::Uuid;

use crate::RuntimeError;
use crate::flow_store::{
    FlowRef,
    task_scope::{ScopeGoal, TaskScopeDefinition, TaskScopeRef},
};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::{Value, ValueJson};

pub enum TaskScopeTool {
    Create,
    Cancel,
    List,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Create {
    id: Uuid,
    name: String,
    goal: Option<Goal>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Goal {
    id: String,
    objective: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Cancel {
    id: Uuid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct List {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Reference {
    owner: Owner,
    id: Uuid,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Owner {
    session_id: Uuid,
    instance_id: Uuid,
}

fn failed(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolFailed(format!("flow.scope: {error}"))
}

fn parse<T: DeserializeOwned>(args: ToolArgs) -> Result<T, RuntimeError> {
    let mut names = std::collections::HashSet::new();
    if !args.positional.is_empty() || args.named.iter().any(|(name, _)| !names.insert(name)) {
        return Err(failed("use unique named arguments"));
    }
    serde_json::from_value(Value::Struct(args.named).to_json()?).map_err(failed)
}

pub(super) fn reference(args: &ToolArgs) -> Result<Option<TaskScopeRef>, RuntimeError> {
    if args
        .named
        .iter()
        .filter(|(name, _)| name == "scope")
        .count()
        > 1
    {
        return Err(failed("scope must be provided once"));
    }
    let Some(value) = args.named("scope") else {
        return Ok(None);
    };
    let reference: Reference = serde_json::from_value(value.to_json()?).map_err(failed)?;
    let scope = TaskScopeRef {
        owner: FlowRef {
            session_id: reference.owner.session_id.into(),
            instance_id: crate::flow_store::FlowInstanceId::new(reference.owner.instance_id.into()),
        },
        id: reference.id,
    };
    scope.validate().map_err(failed)?;
    Ok(Some(scope))
}

pub(super) fn reference_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object", "additionalProperties": false,
        "description": "Explicit task-scope reference returned by flow.scope.create/list. Omit for independent work.",
        "properties": {
            "id": {"type": "string", "format": "uuid"},
            "owner": {
                "type": "object", "additionalProperties": false,
                "properties": {
                    "session_id": {"type": "string", "format": "uuid"},
                    "instance_id": {"type": "string", "format": "uuid"}
                },
                "required": ["session_id", "instance_id"]
            }
        },
        "required": ["owner", "id"]
    })
}

impl Tool for TaskScopeTool {
    fn name(&self) -> &str {
        match self {
            Self::Create => "flow.scope.create",
            Self::Cancel => "flow.scope.cancel",
            Self::List => "flow.scope.list",
        }
    }

    fn tier(&self) -> Tier {
        match self {
            Self::Cancel => Tier::Two,
            _ => Tier::Zero,
        }
    }

    fn description(&self) -> Option<&str> {
        Some(match self {
            Self::Create => {
                "Create a cancellation group owned by this flow. Supply a stable non-nil UUID id and name; identical retries return the original record. Optional goal must match the current goal's id and objective; replacing or clearing that goal cancels the group. No peer joins implicitly. Use the returned reference for explicit scoped work."
            }
            Self::Cancel => {
                "Cancel an owned task scope by id. Explicit members close, unread scoped messages are invalidated, and pending scoped reply waits end. Independent peers and consumed history are preserved. Cancellation is permanent; retries retain the original receipt."
            }
            Self::List => {
                "List this flow's owned task scopes, complete references, goal bindings, and cancellation records without starting work."
            }
        })
    }

    fn input_schema(&self) -> serde_json::Value {
        match self {
            Self::Create => serde_json::json!({
                "type": "object", "additionalProperties": false,
                "properties": {
                    "id": {"type": "string", "format": "uuid"},
                    "name": {"type": "string", "minLength": 1},
                    "goal": {
                        "type": "object", "additionalProperties": false,
                        "properties": {
                            "id": {"type": "string"},
                            "objective": {"type": "string"}
                        },
                        "required": ["id", "objective"]
                    }
                },
                "required": ["id", "name"]
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
            let describe = |record: crate::flow_store::task_scope::TaskScopeRecord| {
                serde_json::json!({
                    "reference": TaskScopeRef {
                        owner: client.source().clone(), id: record.created.definition.id
                    },
                    "record": record,
                })
            };
            let value = match self {
                Self::Create => {
                    let Create { id, name, goal } = parse(args)?;
                    describe(
                        client
                            .create_task_scope(TaskScopeDefinition {
                                id,
                                name,
                                goal: goal.map(|goal| ScopeGoal {
                                    id: goal.id,
                                    objective: goal.objective,
                                }),
                            })
                            .await
                            .map_err(failed)?,
                    )
                }
                Self::Cancel => {
                    let Cancel { id } = parse(args)?;
                    serde_json::to_value(client.cancel_task_scope(id).await.map_err(failed)?)
                        .map_err(failed)?
                }
                Self::List => {
                    let List {} = parse(args)?;
                    serde_json::Value::Array(
                        client
                            .task_scopes()
                            .await
                            .map_err(failed)?
                            .into_iter()
                            .map(describe)
                            .collect(),
                    )
                }
            };
            Ok(Value::from_json(value))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn scoped_tools_reject_unbound_invocations_without_legacy_fallback() {
        let context = ToolCtx::new();
        for tool in [
            &TaskScopeTool::Create as &dyn Tool,
            &TaskScopeTool::List,
            &TaskScopeTool::Cancel,
            &crate::tools::agent_ctrl::FlowInterject,
        ] {
            let error = tool
                .call(
                    ToolArgs {
                        positional: vec![],
                        named: vec![
                            ("handle".into(), Value::Str("root".into())),
                            ("text".into(), Value::Str("scoped work".into())),
                            ("scope".into(), Value::Unit),
                        ],
                    },
                    &context,
                )
                .await
                .unwrap_err();
            assert!(error.to_string().contains("persistent instance"), "{error}");
        }
    }
}
