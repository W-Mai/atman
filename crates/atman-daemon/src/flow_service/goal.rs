//! Goal controls and explicit resume inputs share the persistent instance writer.

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use atman_proto::{GoalRequest, JsonRpcError, methods};
use atman_runtime::flow_store::{
    goal::{GoalRequest as StoredRequest, GoalVersion},
    inbox::SenderIdentity,
};
use atman_runtime::memory::goal::{GoalActor, GoalEdit, GoalPauseReason};
use serde_json::{Value, json};

use super::{OPERATOR, address, authorize_operator, core_address, execution, resolve_target};
use crate::DaemonState;

pub(crate) async fn dispatch(
    state: Arc<DaemonState>,
    method: String,
    request: GoalRequest,
    principal: &str,
) -> Result<Value, JsonRpcError> {
    authorize_operator(principal)?;
    if request.session_id.0.is_nil()
        || request.instance_id.is_some_and(|id| id.is_nil())
        || request.target_handle.is_some()
    {
        return Err(JsonRpcError::invalid_params(
            "persistent goals require a session and optional instance UUID, not a legacy target handle",
        ));
    }
    if method != methods::GOAL_GET
        && (request.instance_id.is_none()
            || request.request_id.is_none_or(|id| id.is_nil())
            || request.goal_id.is_some() != request.expected_revision.is_some())
    {
        return Err(JsonRpcError::invalid_params(
            "persistent goal changes require instance_id, request_id and a paired goal_id/expected_revision",
        ));
    }
    // The single-writer operation survives a disconnected caller.
    tokio::spawn(async move { apply(&state, &method, request).await })
        .await
        .map_err(|error| JsonRpcError::internal(error.to_string()))?
        .map_err(|error| JsonRpcError::application(error.to_string()))
}

async fn apply(state: &DaemonState, method: &str, request: GoalRequest) -> Result<Value> {
    ensure!(!state.is_stopping(), "daemon is stopping");
    let service = state
        .session_service
        .get()
        .context("open the persistent session before accessing its flows")?;
    let flow = core_address(
        resolve_target(
            state,
            &request.session_id,
            request.instance_id,
            crate::state::LOCAL_OPERATOR,
        )
        .await
        .map_err(|error| anyhow::anyhow!(error.message))?
        .context("persistent session is not attached")?,
    );
    let worker = service.host.flow(&flow).await?;
    let committed = if method == methods::GOAL_GET {
        None
    } else {
        let edit = match method {
            methods::GOAL_PAUSE => GoalEdit::Pause {
                actor: GoalActor::User,
                reason: GoalPauseReason::UserRequested,
                summary: String::new(),
            },
            methods::GOAL_RESUME => GoalEdit::Resume {
                actor: GoalActor::User,
            },
            methods::GOAL_CLEAR => GoalEdit::Clear,
            methods::GOAL_SET => GoalEdit::Set {
                text: request.text.unwrap_or_default(),
                replace_completed: false,
            },
            _ => anyhow::bail!("unknown goal operation"),
        };
        Some(
            worker
                .edit_goal(
                    StoredRequest {
                        expected_head: None,
                        id: request.request_id.context("request_id is required")?,
                        sender: SenderIdentity::User(OPERATOR),
                        expected: request
                            .goal_id
                            .zip(request.expected_revision)
                            .map(|(id, revision)| GoalVersion { id, revision }),
                        edit,
                        start_turn: method == methods::GOAL_RESUME,
                    },
                    |_| Ok(()),
                )
                .await?,
        )
    };
    let view = worker.goal().await?;
    // A fast turn may already have changed the goal again; keep the command receipt separate.
    Ok(json!({"flow": address(&flow), "goal": view.goal,
        "execution": execution(&view.execution), "committed": committed}))
}
