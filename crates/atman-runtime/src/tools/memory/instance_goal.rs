//! Goal tools address durable instances through the same authorized peer client.

use super::*;
use crate::flow_store::goal::GoalVersion;
use crate::memory::goal::GoalEdit;
use crate::tools::agent_ctrl::instance::{self, Target};

pub struct MemoryGoalSchedule;

impl Tool for MemoryGoalSchedule {
    fn name(&self) -> &str {
        "memory.goal.schedule"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Propose this persistent flow's next turn from an explicit entry policy. Requires the current active goal version. Ordinary input takes priority; a changed goal declines the proposal. This does not deliver a final answer or resume execution.",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "goal_id": { "type": "string" },
                "expected_revision": { "type": "integer", "minimum": 1 },
                "message": { "type": "string", "description": "Instructions for the next turn, recorded as internal runtime input." }
            },
            "required": ["goal_id", "expected_revision", "message"],
            "additionalProperties": false
        })
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let client = ctx.flow_client().ok_or_else(|| {
                goal_error("scheduling requires a persistent instance host binding")
            })?;
            let (id, revision) = goal_expected(&args, true)?.expect("required goal version");
            let acceptance = client
                .schedule_goal(
                    GoalVersion {
                        id: id.into(),
                        revision,
                    },
                    required_string(&args, "message")?,
                )
                .await
                .map_err(goal_error)?;
            Ok(Value::from_json(
                serde_json::json!({ "accepted": acceptance.is_some(), "acceptance": acceptance }),
            ))
        })
    }
}

pub(super) fn target<'a>(
    args: &ToolArgs,
    ctx: &'a ToolCtx,
) -> Result<Option<Target<'a>>, RuntimeError> {
    let Some(client) = ctx.instance_client()? else {
        return Ok(None);
    };
    let address = client.source();
    let handle = match args.named("target_handle") {
        None => address.instance_id.to_string(),
        Some(Value::Str(handle)) => handle.clone(),
        _ => return Err(goal_error("target_handle must be a string")),
    };
    instance::resolve(&handle, ctx)
}

pub(super) async fn get(target: Target<'_>) -> ToolResult {
    let view = target
        .client
        .read_goal(target.address.clone())
        .await
        .map_err(goal_error)?;
    Ok(Value::from_json(serde_json::json!({
        "goal": view.goal,
        "flow": instance::status_value(&view.execution, &target.address.instance_id.to_string()),
    })))
}

async fn edit(target: Target<'_>, args: &ToolArgs, edit: GoalEdit) -> ToolResult {
    let expected = goal_expected(args, false)?.map(|(id, revision)| GoalVersion {
        id: id.into(),
        revision,
    });
    let logical_request = match args.named("request_id") {
        None => uuid::Uuid::now_v7(),
        Some(Value::Str(id)) => uuid::Uuid::parse_str(id).map_err(goal_error)?,
        _ => return Err(goal_error("request_id must be a UUID")),
    };
    let request = if args.named("request_id").is_some() {
        stable_storage_request_id(logical_request, &target.address, expected.as_ref(), &edit)?
    } else {
        logical_request
    };
    let committed = target
        .client
        .edit_goal(target.address, request, expected, edit)
        .await
        .map_err(goal_error)?;
    Ok(Value::from_json(serde_json::json!({
        "goal": committed.goal, "request_id": logical_request, "receipt": committed.receipt,
    })))
}

fn stable_storage_request_id(
    logical_request: uuid::Uuid,
    target: &crate::flow_store::FlowRef,
    expected: Option<&GoalVersion>,
    edit: &GoalEdit,
) -> Result<uuid::Uuid, RuntimeError> {
    let mut hash = blake3::Hasher::new();
    hash.update(b"atman.memory.goal.edit\0");
    hash.update(logical_request.as_bytes());
    hash.update(target.session_id.0.as_bytes());
    hash.update(target.instance_id.to_string().as_bytes());
    hash.update(&serde_json::to_vec(&(expected, edit)).map_err(goal_error)?);
    Ok(uuid::Uuid::from_bytes(
        hash.finalize().as_bytes()[..16]
            .try_into()
            .expect("blake3 digest has at least 16 bytes"),
    ))
}

pub(super) async fn set(
    target: Target<'_>,
    args: &ToolArgs,
    ctx: &ToolCtx,
    text: String,
) -> ToolResult {
    // Preserve request intent across retries; the owner resolves it against the acknowledged revision.
    edit(
        target,
        args,
        GoalEdit::Set {
            text,
            replace_completed: ctx.goal_resume_allowed,
        },
    )
    .await
}

pub(super) async fn control(
    target: Target<'_>,
    args: &ToolArgs,
    ctx: &ToolCtx,
    resume: bool,
) -> ToolResult {
    goal_expected(args, true)?;
    let change = if resume {
        if &target.address == target.client.source() && !ctx.goal_resume_allowed {
            return Err(goal_error(
                "self resume requires new user or authorized input",
            ));
        }
        GoalEdit::Resume {
            actor: GoalActor::Agent,
        }
    } else {
        let reason = match required_string(args, "reason")?.as_str() {
            "completed" => GoalPauseReason::Completed,
            "needs_input" => GoalPauseReason::NeedsInput,
            "blocked" => GoalPauseReason::Blocked,
            "user_requested" => GoalPauseReason::UserRequested,
            _ => return Err(goal_error("invalid pause reason")),
        };
        let summary = required_string(args, "summary")?;
        if summary.trim().is_empty() {
            return Err(goal_error("pause summary is required"));
        }
        GoalEdit::Pause {
            actor: GoalActor::Agent,
            reason,
            summary,
        }
    };
    edit(target, args, change).await
}

pub(super) async fn clear(target: Target<'_>, args: &ToolArgs) -> ToolResult {
    edit(target, args, GoalEdit::Clear).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn goal_tool_request_identity_is_stable_per_logical_payload() {
        let target = crate::flow_store::FlowRef {
            session_id: uuid::Uuid::now_v7().into(),
            instance_id: crate::flow_store::FlowInstanceId::now(),
        };
        let logical = uuid::Uuid::now_v7();
        let first = GoalEdit::Set {
            text: "first".into(),
            replace_completed: false,
        };
        let second = GoalEdit::Set {
            text: "second".into(),
            replace_completed: false,
        };
        let first_id = stable_storage_request_id(logical, &target, None, &first).unwrap();
        assert_eq!(
            first_id,
            stable_storage_request_id(logical, &target, None, &first).unwrap()
        );
        assert_ne!(
            first_id,
            stable_storage_request_id(logical, &target, None, &second).unwrap()
        );
    }
}
