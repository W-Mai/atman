//! Persistent-instance routing for the shared flow tool surface.

use atman_rt::{InstanceActivity, InstanceState, TurnPhase};
use uuid::Uuid;

use super::{RuntimeError, ToolArgs, ToolCtx, ToolResult, Value, ValueJson};
use crate::flow_runtime::access::{FlowClient, PeerControl};
use crate::flow_store::FlowRef;
use crate::flow_store::inbox::DeliveryMode;

pub(in crate::tools) const HANDLE_DESCRIPTION: &str = "Flow handle. Persistent instances use an instance UUID in the current session or session UUID/instance UUID across sessions.";

fn request_id(args: &ToolArgs) -> Result<Uuid, RuntimeError> {
    match args.named("request_id") {
        None => Ok(Uuid::now_v7()),
        Some(Value::Str(id)) => Uuid::parse_str(id).map_err(failed),
        _ => Err(failed("request_id must be a UUID string")),
    }
}

pub(in crate::tools) struct Target<'a> {
    pub(in crate::tools) client: &'a FlowClient,
    pub(in crate::tools) address: FlowRef,
}

/// Resolve from invocation identity, never by probing a legacy registry after an instance error.
pub(in crate::tools) fn resolve<'a>(
    handle: &str,
    ctx: &'a ToolCtx,
) -> Result<Option<Target<'a>>, RuntimeError> {
    let Some(client) = ctx.instance_client()? else {
        return Ok(None);
    };
    let address = client.resolve_address(handle).map_err(failed)?;
    Ok(Some(Target { client, address }))
}

impl Target<'_> {
    pub fn resource_key(&self) -> String {
        format!("{}:{}", self.address.session_id, self.address.instance_id)
    }

    pub async fn status(self, handle: String) -> ToolResult {
        let snapshot = self.client.inspect(self.address).await.map_err(failed)?;
        Ok(Value::from_json(status_value(&snapshot, &handle)))
    }

    pub async fn output(self) -> Result<String, RuntimeError> {
        Ok(self
            .client
            .read_output(self.address)
            .await
            .map_err(failed)?
            .iter()
            .map(|record| record.publication.message.text_concat())
            .collect::<Vec<_>>()
            .join("\n"))
    }

    pub async fn result(self, turn: Option<crate::event::TurnId>) -> ToolResult {
        let record = self
            .client
            .read_result(self.address, turn)
            .await
            .map_err(failed)?;
        Ok(Value::from_json(
            serde_json::json!({ "completion": record }),
        ))
    }

    pub async fn cancel(self, args: &ToolArgs) -> ToolResult {
        self.control(args, PeerControl::CancelTurn).await
    }

    pub async fn close(self, args: &ToolArgs) -> ToolResult {
        self.control(args, PeerControl::Close).await
    }

    async fn control(self, args: &ToolArgs, action: PeerControl) -> ToolResult {
        let revision = match args.named("expected_revision") {
            Some(Value::Int(n)) if *n >= 0 => *n as u64,
            _ => {
                return Err(failed(
                    "expected_revision from flow.status is required for instance control",
                ));
            }
        };
        let request_id = request_id(args)?;
        let receipt = self
            .client
            .control(request_id, self.address, revision, action)
            .await
            .map_err(failed)?;
        Ok(Value::from_json(
            serde_json::json!({ "request_id": request_id, "receipt": receipt }),
        ))
    }

    pub async fn interject(self, args: &ToolArgs, text: String) -> ToolResult {
        let scope = crate::tools::task_scope::reference(args)?;
        match args.named("level") {
            None => (),
            Some(Value::Str(level)) if level == "l1_nudge" => (),
            Some(Value::Str(level)) if level == "l4_hard_stop" => {
                if scope.is_some() {
                    return Err(failed(
                        "scope applies to messages, not execution cancellation",
                    ));
                }
                return self.cancel(args).await;
            }
            _ => {
                return Err(failed(
                    "persistent interjection requires l1_nudge; execution cancellation uses flow.kill",
                ));
            }
        }
        if args.named("redirect_target").is_some() {
            return Err(failed(
                "persistent interjection does not redirect an instance entry",
            ));
        }
        let delivery = match args.named("delivery") {
            None => DeliveryMode::AtBoundary,
            Some(Value::Str(mode)) if mode == "at_boundary" => DeliveryMode::AtBoundary,
            Some(Value::Str(mode)) if mode == "next_turn" => DeliveryMode::NextTurn,
            _ => return Err(failed("delivery must be at_boundary or next_turn")),
        };
        let request_id = request_id(args)?;
        let attempt = self
            .client
            .send(self.address, request_id, text, delivery, scope)
            .await
            .map_err(failed)?;
        Ok(Value::from_json(serde_json::json!({
            "request_id": request_id,
            "delivery": attempt.record.status,
            "scope": attempt.record.intent.request.scope,
            "pending_reason": attempt.pending_reason.map(|error| error.to_string()),
        })))
    }
}

pub(in crate::tools) fn status_value(
    snapshot: &atman_rt::FlowInstance<crate::event::AtmanUuid>,
    handle: &str,
) -> serde_json::Value {
    let status = match snapshot.state() {
        InstanceState::Prepared => "prepared",
        InstanceState::Open(InstanceActivity::Idle) => "idle",
        InstanceState::Open(InstanceActivity::Interrupted { .. }) => "interrupted",
        InstanceState::Open(InstanceActivity::Active(active)) => match active.phase {
            TurnPhase::Running => "running",
            TurnPhase::Waiting(_) => "waiting",
            TurnPhase::Finishing => "finishing",
            TurnPhase::Cancelling => "cancelling",
        },
        InstanceState::Closing { .. } => "closing",
        InstanceState::Closed => "closed",
    };
    // Pause causes are separate from execution phase and Goal automation.
    serde_json::json!({
        "handle": handle,
        "address": snapshot.address(),
        "status": status,
        "state": snapshot.state(),
        "revision": snapshot.revision(),
        "paused": !snapshot.pauses().is_empty(),
    })
}

fn failed(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolFailed(format!("flow instance: {error}"))
}
