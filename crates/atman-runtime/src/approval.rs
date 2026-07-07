use crate::tool::{ApprovalLevel, ToolArgs, ToolCtx};

pub enum ApprovalOutcome {
    Approve,
    Deny { reason: String },
}

pub fn level_str(level: ApprovalLevel) -> &'static str {
    match level {
        ApprovalLevel::Auto => "auto",
        ApprovalLevel::Approve => "approve",
        ApprovalLevel::Dangerous => "dangerous",
    }
}

pub async fn request_approval(
    ctx: &ToolCtx,
    id: &str,
    name: &str,
    call_args: &ToolArgs,
    level: ApprovalLevel,
    tool: Option<&dyn crate::tool::Tool>,
) -> ApprovalOutcome {
    let Some(approval) = &ctx.approval else {
        return ApprovalOutcome::Approve;
    };
    let Some(run_id) = ctx.flow_run_id.clone() else {
        return ApprovalOutcome::Approve;
    };
    let args_preview: String = format!("{:?}", call_args.named)
        .chars()
        .take(4000)
        .collect();
    let preview = if level == ApprovalLevel::Auto {
        None
    } else {
        match tool {
            Some(t) => t.preview_call(call_args, ctx).await,
            None => None,
        }
    };
    let pending = crate::session::PendingApproval {
        tool_use_id: id.to_string(),
        tool_name: name.to_string(),
        args_preview: args_preview.clone(),
        preview: preview.clone(),
        level,
        run_id: run_id.clone(),
        emitted_at: chrono::Utc::now(),
    };
    let rx = approval.request(pending);
    if let Some(sink) = ctx.events.as_ref() {
        sink.emit(crate::event::Event::ToolPendingApproval {
            seq: 0,
            run_id: run_id.clone(),
            tool_use_id: id.to_string(),
            tool_name: name.to_string(),
            args_preview: args_preview.clone(),
            level: level_str(level).into(),
            preview: preview.clone(),
            ts: chrono::Utc::now(),
        });
    }
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::ToolPendingApproval {
            run_id: run_id.0.to_string(),
            tool_use_id: id.to_string(),
            tool_name: name.to_string(),
            args_preview,
            level: level_str(level).into(),
            preview: preview.clone(),
        });
    }
    let decision = rx.await.unwrap_or(crate::session::ApprovalDecision::Deny {
        reason: "approval channel dropped".into(),
    });
    match decision {
        crate::session::ApprovalDecision::Approve => {
            if let Some(sink) = ctx.events.as_ref() {
                sink.emit(crate::event::Event::ToolApproved {
                    seq: 0,
                    run_id: run_id.clone(),
                    tool_use_id: id.to_string(),
                    decided_by: "user".into(),
                    ts: chrono::Utc::now(),
                });
            }
            if let Some(tx) = &ctx.stream_tx {
                let _ = tx.send(crate::stream::StreamFrame::ToolApproved {
                    run_id: run_id.0.to_string(),
                    tool_use_id: id.to_string(),
                    decided_by: "user".into(),
                });
            }
            ApprovalOutcome::Approve
        }
        crate::session::ApprovalDecision::Deny { reason } => {
            if let Some(sink) = ctx.events.as_ref() {
                sink.emit(crate::event::Event::ToolDenied {
                    seq: 0,
                    run_id: run_id.clone(),
                    tool_use_id: id.to_string(),
                    reason: reason.clone(),
                    ts: chrono::Utc::now(),
                });
            }
            if let Some(tx) = &ctx.stream_tx {
                let _ = tx.send(crate::stream::StreamFrame::ToolDenied {
                    run_id: run_id.0.to_string(),
                    tool_use_id: id.to_string(),
                    reason: reason.clone(),
                });
            }
            ApprovalOutcome::Deny { reason }
        }
    }
}
