//! Goal tools address durable instances through the same authorized peer client.

use super::*;
use crate::flow_store::goal::GoalVersion;
use crate::memory::goal::GoalEdit;
use crate::tools::agent_ctrl::instance::{self, Target};

pub(super) fn target<'a>(
    args: &ToolArgs,
    ctx: &'a ToolCtx,
) -> Result<Option<Target<'a>>, RuntimeError> {
    let Some(address) = ctx
        .flow_identity
        .as_ref()
        .and_then(|identity| identity.instance.as_ref())
    else {
        return Ok(None);
    };
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
    let request = match args.named("request_id") {
        None => uuid::Uuid::now_v7(),
        Some(Value::Str(id)) => uuid::Uuid::parse_str(id).map_err(goal_error)?,
        _ => return Err(goal_error("request_id must be a UUID")),
    };
    let committed = target
        .client
        .edit_goal(target.address, request, expected, edit)
        .await
        .map_err(goal_error)?;
    Ok(Value::from_json(serde_json::json!({
        "goal": committed.goal, "request_id": request, "receipt": committed.receipt,
    })))
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
