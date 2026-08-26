use crate::tool::{ApprovalLevel, ToolArgs, ToolCtx};
use crate::trust::RiskKind;
use std::collections::BTreeSet;

pub enum ApprovalOutcome {
    Approve {
        authorization: Box<crate::permission::InvocationAuthorization>,
    },
    Deny {
        reason: String,
    },
}

pub fn level_str(level: ApprovalLevel) -> &'static str {
    match level {
        ApprovalLevel::Auto => "auto",
        ApprovalLevel::Approve => "approve",
        ApprovalLevel::Dangerous => "dangerous",
    }
}

fn resolve_provenance(
    ctx: &ToolCtx,
    tool: Option<&dyn crate::tool::Tool>,
    args: &ToolArgs,
) -> Result<crate::permission::ResourceProvenance, String> {
    match tool {
        Some(tool) => tool
            .invocation_provenance(args, ctx)
            .map_err(|e| e.to_string()),
        None => Ok(crate::permission::ResourceProvenance::none()),
    }
}

// Risks only tighten policy, so read-only tools must not inherit mutation risks.
fn intent_risks(
    tier: crate::tool::Tier,
    provenance: &crate::permission::ResourceProvenance,
) -> BTreeSet<RiskKind> {
    let mut risks = provenance.risks.clone();
    if provenance.is_external() {
        risks.insert(RiskKind::WorkspaceExternal);
    }
    if tier == crate::tool::Tier::Four {
        risks.insert(RiskKind::ProcessSpawn);
    }
    if provenance.network {
        risks.insert(RiskKind::Network);
    }
    risks
}

fn args_digest(args_preview: &str) -> String {
    blake3::hash(args_preview.as_bytes()).to_hex().to_string()
}

fn submit_to_broker(
    ctx: &ToolCtx,
    intent: crate::permission::PermissionIntent,
    tier: crate::tool::Tier,
) -> Option<Result<crate::permission::SubmissionOutcome, crate::permission::PermissionError>> {
    let broker = ctx.permission_broker.as_ref()?;
    let identity = ctx.flow_identity.as_ref()?;
    let trust = ctx.trust.as_ref()?;
    // A broker bound to a different registry would authenticate this identity
    // against a foreign authority graph, so refuse rather than mis-authorize.
    let registry = ctx.flow_registry.as_ref()?;
    if !broker.is_for_registry(registry) {
        return None;
    }
    Some(broker.submit(
        Some(identity.session_id.as_str()),
        Some(&identity.run_id),
        intent,
        tier == crate::tool::Tier::Four,
        trust,
    ))
}

/// Reports a queue decision back to the broker so its request leaves the pending
/// set. Best-effort: the broker may already have settled it (terminal cleanup),
/// in which case `AlreadyResolved` is the expected, harmless outcome.
fn emit_approval_result(
    ctx: &ToolCtx,
    run_id: &crate::event::FlowRunId,
    id: &str,
    decision: &crate::session::ApprovalDecision,
    decided_by: &str,
) {
    match decision {
        crate::session::ApprovalDecision::Approve => {
            if let Some(sink) = ctx.events.as_ref() {
                sink.emit(crate::event::Event::ToolApproved {
                    run_id: run_id.clone(),
                    tool_use_id: id.to_string(),
                    decided_by: decided_by.into(),
                });
            }
            if let Some(tx) = &ctx.stream_tx {
                let _ = tx.send(crate::stream::StreamFrame::ToolApproved {
                    run_id: run_id.0.to_string(),
                    tool_use_id: id.to_string(),
                    decided_by: decided_by.into(),
                });
            }
        }
        crate::session::ApprovalDecision::Deny { reason } => {
            if let Some(sink) = ctx.events.as_ref() {
                sink.emit(crate::event::Event::ToolDenied {
                    run_id: run_id.clone(),
                    tool_use_id: id.to_string(),
                    reason: reason.clone(),
                });
            }
            if let Some(tx) = &ctx.stream_tx {
                let _ = tx.send(crate::stream::StreamFrame::ToolDenied {
                    run_id: run_id.0.to_string(),
                    tool_use_id: id.to_string(),
                    reason: reason.clone(),
                });
            }
        }
    }
}

fn settle_broker_request(
    ctx: &ToolCtx,
    request_id: &crate::permission::PermissionRequestId,
    decision: &crate::session::ApprovalDecision,
) {
    let (Some(broker), Some(identity)) =
        (ctx.permission_broker.as_ref(), ctx.flow_identity.as_ref())
    else {
        return;
    };
    let (action, reason) = match decision {
        crate::session::ApprovalDecision::Approve => {
            (crate::permission::PermissionAction::Approve, None)
        }
        crate::session::ApprovalDecision::Deny { reason } => (
            crate::permission::PermissionAction::Deny,
            Some(reason.clone()),
        ),
    };
    let authority = crate::permission::DecisionAuthority::User(
        broker.user_authority(identity.session_id.clone(), None),
    );
    if let Err(error) = broker.resolve(
        request_id,
        &authority,
        action,
        Some(crate::permission::GrantScope::CurrentCall),
        reason,
    ) && error != crate::permission::PermissionError::AlreadyResolved
    {
        crate::notify!(warn, "permission resolve failed: {error}");
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
    use crate::trust::{OutsideBehavior, TrustMode};
    let provenance = match resolve_provenance(ctx, tool, call_args) {
        Ok(provenance) => provenance,
        // The resolver refused to classify the target (relative path escaping a
        // managed workspace). Authorizing an unclassified resource would defeat
        // the gate, so fail closed.
        Err(reason) => {
            return ApprovalOutcome::Deny {
                reason: format!("{name}: blocked — {reason}"),
            };
        }
    };
    let outside_workspace = provenance.is_external();
    if outside_workspace {
        if let Some(trust) = &ctx.trust {
            if trust.mode != TrustMode::Reckless {
                match trust.outside {
                    OutsideBehavior::Deny => {
                        return ApprovalOutcome::Deny {
                            reason: format!(
                                "{name}: blocked — path outside workspace and outside=deny"
                            ),
                        };
                    }
                    OutsideBehavior::Allow => {}
                    OutsideBehavior::Approve => {}
                }
            }
        }
    }
    let permit_provenance = provenance.clone();
    let permit = |manual: bool| {
        crate::permission::InvocationAuthorization::new(id, name, permit_provenance.clone(), manual)
    };
    // A bound broker means this context is under controlled execution, so a
    // missing prompt transport or run identity is an infrastructure gap, not
    // consent. Contexts with no broker at all keep the legacy behaviour.
    let brokered_context = ctx.permission_broker.is_some();
    let Some(run_id) = ctx.flow_run_id.clone() else {
        return if brokered_context {
            ApprovalOutcome::Deny {
                reason: format!(
                    "{name}: blocked — missing run identity under controlled execution"
                ),
            }
        } else {
            ApprovalOutcome::Approve {
                authorization: Box::new(permit(false)),
            }
        };
    };
    let force_manual = outside_workspace
        && ctx
            .trust
            .as_ref()
            .map(|t| t.mode != TrustMode::Reckless && t.outside == OutsideBehavior::Approve)
            .unwrap_or(true);
    let effective_level = if force_manual {
        ApprovalLevel::Dangerous
    } else {
        level
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
    let tier = tool.map(|t| t.tier()).unwrap_or(crate::tool::Tier::Zero);
    let intent = crate::permission::PermissionIntent {
        tool_use_id: id.to_string(),
        tool_name: name.to_string(),
        tier,
        risks: intent_risks(tier, &provenance),
        args_digest: args_digest(&args_preview),
        preview: preview.clone(),
        provenance,
    };
    // The broker owns the policy decision; the legacy queue below is still the
    // only surface that renders a prompt, so a Pending outcome is handed to it.
    let brokered = match submit_to_broker(ctx, intent, tier) {
        Some(Ok(outcome)) => Some(outcome),
        Some(Err(error)) => {
            return ApprovalOutcome::Deny {
                reason: format!("{name}: permission broker rejected the request: {error}"),
            };
        }
        None => None,
    };
    let mut pending_permission = None;
    match brokered {
        Some(crate::permission::SubmissionOutcome::Immediate(immediate)) => {
            use crate::permission::ImmediateAuthorization;
            match *immediate {
                // `force_manual` is the legacy outside-workspace escalation, which the
                // risk projection cannot express as a Deny in Calm/Steady. Honour it so
                // an Auto verdict never silently bypasses that prompt.
                ImmediateAuthorization::Unrestricted if !force_manual => {
                    emit_approval_result(
                        ctx,
                        &run_id,
                        id,
                        &crate::session::ApprovalDecision::Approve,
                        "unrestricted",
                    );
                    return ApprovalOutcome::Approve {
                        authorization: Box::new(permit(false)),
                    };
                }
                ImmediateAuthorization::Auto if !force_manual => {
                    emit_approval_result(
                        ctx,
                        &run_id,
                        id,
                        &crate::session::ApprovalDecision::Approve,
                        "policy",
                    );
                    return ApprovalOutcome::Approve {
                        authorization: Box::new(permit(false)),
                    };
                }
                ImmediateAuthorization::Granted { .. } if !force_manual => {
                    emit_approval_result(
                        ctx,
                        &run_id,
                        id,
                        &crate::session::ApprovalDecision::Approve,
                        "grant",
                    );
                    return ApprovalOutcome::Approve {
                        authorization: Box::new(permit(false)),
                    };
                }
                ImmediateAuthorization::Denied { reason } => {
                    let reason = format!("{name}: {reason}");
                    let decision = crate::session::ApprovalDecision::Deny {
                        reason: reason.clone(),
                    };
                    emit_approval_result(ctx, &run_id, id, &decision, "policy");
                    return ApprovalOutcome::Deny { reason };
                }
                _ => {}
            }
        }
        Some(crate::permission::SubmissionOutcome::Pending(pending)) => {
            pending_permission = Some(pending);
        }
        None => {}
    }
    let Some(approval) = &ctx.approval else {
        if let Some(pending) = pending_permission.as_ref() {
            let reason = "no approval transport under controlled execution".to_string();
            if let Some(broker) = ctx.permission_broker.as_ref() {
                let _ = broker.cancel(&pending.request.request_id, reason.clone());
            }
            return ApprovalOutcome::Deny {
                reason: format!("{name}: blocked — {reason}"),
            };
        }
        return if brokered_context {
            ApprovalOutcome::Deny {
                reason: format!(
                    "{name}: blocked — no approval transport under controlled execution"
                ),
            }
        } else {
            ApprovalOutcome::Approve {
                authorization: Box::new(permit(false)),
            }
        };
    };
    if pending_permission.is_some() && !approval.has_subscribers() {
        if let Some(pending) = pending_permission.as_ref() {
            let reason = "no approval consumer".to_string();
            if let Err(error) = ctx
                .permission_broker
                .as_ref()
                .expect("pending permission requires a broker")
                .cancel(&pending.request.request_id, reason.clone())
                && error != crate::permission::PermissionError::AlreadyResolved
            {
                crate::notify!(warn, "permission cancel failed: {error}");
            }
            let decision = crate::session::ApprovalDecision::Deny {
                reason: reason.clone(),
            };
            emit_approval_result(ctx, &run_id, id, &decision, "system");
            return ApprovalOutcome::Deny { reason };
        }
    }
    let pending = crate::session::PendingApproval {
        tool_use_id: id.to_string(),
        tool_name: name.to_string(),
        args_preview: args_preview.clone(),
        preview: preview.clone(),
        level: effective_level,
        run_id: run_id.clone(),
        emitted_at: chrono::Utc::now(),
        // A broker Pending verdict must not be overridden by the legacy auto ceiling;
        // the registry is only the prompt transport for broker-owned decisions.
        bypass_auto_ceiling: pending_permission.is_some() || force_manual,
    };
    let (ticket, rx) = approval.request_tracked(pending);
    if let Some(sink) = ctx.events.as_ref() {
        sink.emit(crate::event::Event::ToolPendingApproval {
            run_id: run_id.clone(),
            tool_use_id: id.to_string(),
            tool_name: name.to_string(),
            args_preview: args_preview.clone(),
            level: level_str(level).into(),
            preview: preview.clone(),
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
    let decision = match pending_permission {
        // Both sides can settle this call: the user answers the queued prompt, or the
        // broker cancels (flow went terminal). Whichever lands first wins, and the
        // loser is cleaned up so no orphan prompt or unresolved request is left.
        Some(pending) => {
            let request_id = pending.request.request_id.clone();
            let mut resolution = pending.resolution;
            tokio::select! {
                biased;
                brokered = &mut resolution => match brokered {
                    Ok(crate::permission::PermissionResolution::Cancelled { reason }) => {
                        if let Some(ticket) = ticket {
                            approval.cancel(ticket, reason.clone());
                        }
                        crate::session::ApprovalDecision::Deny { reason }
                    }
                    Ok(crate::permission::PermissionResolution::Decision(decision)) => {
                        let approved =
                            decision.action == crate::permission::PermissionAction::Approve;
                        if let Some(ticket) = ticket {
                            approval.cancel(ticket, "resolved by permission broker".to_string());
                        }
                        if approved {
                            crate::session::ApprovalDecision::Approve
                        } else {
                            crate::session::ApprovalDecision::Deny {
                                reason: decision
                                    .reason
                                    .unwrap_or_else(|| "denied by permission broker".into()),
                            }
                        }
                    }
                    Err(_) => {
                        if let Some(ticket) = ticket {
                            approval.cancel(ticket, "permission request dropped");
                        }
                        crate::session::ApprovalDecision::Deny {
                            reason: "permission request dropped".into(),
                        }
                    },
                },
                queued = rx => {
                    let decision = queued.unwrap_or(crate::session::ApprovalDecision::Deny {
                        reason: "approval channel dropped".into(),
                    });
                    settle_broker_request(ctx, &request_id, &decision);
                    decision
                }
            }
        }
        None => rx.await.unwrap_or(crate::session::ApprovalDecision::Deny {
            reason: "approval channel dropped".into(),
        }),
    };
    emit_approval_result(ctx, &run_id, id, &decision, "user");
    match decision {
        crate::session::ApprovalDecision::Approve => ApprovalOutcome::Approve {
            authorization: Box::new(permit(true)),
        },
        crate::session::ApprovalDecision::Deny { reason } => ApprovalOutcome::Deny { reason },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::FlowRunId;
    use crate::flow_authority::EffectiveAuthority;
    use crate::permission::PermissionBroker;
    use crate::session::ApprovalRegistry;
    use crate::tool::{Tier, Tool};
    use crate::tools::agent_ctrl::FlowRegistry;
    use crate::trust::{
        PolicyAction, TierPolicyConfig, TierPolicyOverrides, TrustConfig, TrustMode,
    };
    use std::sync::Arc;

    struct Tier2Tool;

    impl crate::tool::Tool for Tier2Tool {
        fn name(&self) -> &str {
            "probe.tool"
        }

        fn tier(&self) -> Tier {
            Tier::Two
        }

        fn call<'a>(
            &'a self,
            _args: ToolArgs,
            _ctx: &'a ToolCtx,
        ) -> crate::tool::BoxFut<'a, crate::tool::ToolResult> {
            Box::pin(async move { Ok(crate::value::Value::Unit) })
        }
    }

    fn ctx_with_broker(trust: TrustConfig) -> (ToolCtx, Arc<ApprovalRegistry>, Arc<FlowRegistry>) {
        let flows = Arc::new(FlowRegistry::new());
        let broker = PermissionBroker::shared(Arc::clone(&flows));
        let run_id = FlowRunId::now();
        let identity = flows
            .register_root(
                "sess".into(),
                run_id.clone(),
                EffectiveAuthority::root(&trust, true, None),
            )
            .unwrap();
        let approval = Arc::new(ApprovalRegistry::new());
        approval.set_auto_ceiling(ApprovalLevel::Auto);
        let mut ctx = ToolCtx::new();
        ctx.approval = Some(Arc::clone(&approval));
        ctx.permission_broker = Some(broker);
        ctx.flow_registry = Some(Arc::clone(&flows));
        ctx.flow_identity = Some(identity);
        ctx.flow_run_id = Some(run_id);
        ctx.trust = Some(trust);
        (ctx, approval, flows)
    }

    fn deny_tier2() -> TrustConfig {
        TrustConfig {
            mode: TrustMode::Eager,
            tiers: TierPolicyConfig {
                eager: TierPolicyOverrides {
                    tier2: Some(PolicyAction::Deny),
                    ..TierPolicyOverrides::default()
                },
            },
            ..TrustConfig::default()
        }
    }

    /// The broker's policy verdict must reach the caller. Without wiring, the legacy
    /// path would queue a prompt instead of denying outright.
    #[tokio::test]
    async fn broker_policy_denies_without_prompting() {
        let (ctx, approval, _flows) = ctx_with_broker(deny_tier2());
        let outcome = request_approval(
            &ctx,
            "tu1",
            "probe.tool",
            &ToolArgs::default(),
            ApprovalLevel::Approve,
            Some(&Tier2Tool),
        )
        .await;
        assert!(matches!(outcome, ApprovalOutcome::Deny { .. }));
        assert!(approval.list_pending().is_empty());
    }

    #[tokio::test]
    async fn broker_auto_approves_without_prompt_transport() {
        let trust = TrustConfig {
            mode: TrustMode::Eager,
            tiers: TierPolicyConfig {
                eager: TierPolicyOverrides {
                    tier2: Some(PolicyAction::Auto),
                    ..TierPolicyOverrides::default()
                },
            },
            ..TrustConfig::default()
        };
        let (mut ctx, approval, _flows) = ctx_with_broker(trust);
        ctx.approval = None;
        let outcome = request_approval(
            &ctx,
            "tu_auto_headless",
            "probe.tool",
            &ToolArgs::default(),
            ApprovalLevel::Approve,
            Some(&Tier2Tool),
        )
        .await;
        assert!(matches!(outcome, ApprovalOutcome::Approve { .. }));
        assert!(approval.list_pending().is_empty());
    }

    #[tokio::test]
    async fn broker_pending_without_approval_consumer_fails_closed() {
        let trust = TrustConfig {
            mode: TrustMode::Steady,
            ..TrustConfig::default()
        };
        let (ctx, approval, _flows) = ctx_with_broker(trust);
        let broker = ctx.permission_broker.clone().unwrap();
        let gate = tokio::spawn({
            let ctx = ctx.clone();
            async move {
                request_approval(
                    &ctx,
                    "tu_headless",
                    "probe.tool",
                    &ToolArgs::default(),
                    ApprovalLevel::Auto,
                    Some(&Tier2Tool),
                )
                .await
            }
        });
        let request_id = loop {
            if let Some(req) = broker.list().first() {
                break req.request_id.clone();
            }
            tokio::task::yield_now().await;
        };
        let outcome = gate.await.unwrap();
        assert!(matches!(outcome, ApprovalOutcome::Deny { .. }));
        assert!(approval.list_pending().is_empty());
        assert!(matches!(
            broker.get(&request_id).map(|request| request.state),
            Some(crate::permission::PermissionRequestState::Cancelled { .. })
        ));
    }

    /// A user answer on the legacy queue must settle the broker request too,
    /// otherwise pending requests would leak for the life of the run.
    #[tokio::test]
    async fn queue_decision_settles_broker_request() {
        let trust = TrustConfig {
            mode: TrustMode::Steady,
            ..TrustConfig::default()
        };
        let (ctx, approval, _flows) = ctx_with_broker(trust);
        let _approval_updates = approval.subscribe();
        let broker = ctx.permission_broker.clone().unwrap();
        let gate = tokio::spawn({
            let ctx = ctx.clone();
            async move {
                request_approval(
                    &ctx,
                    "tu2",
                    "probe.tool",
                    &ToolArgs::default(),
                    ApprovalLevel::Auto,
                    Some(&Tier2Tool),
                )
                .await
            }
        });
        let request_id = loop {
            if let Some(req) = broker.list().first() {
                break req.request_id.clone();
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(approval.list_pending().len(), 1);
        while !approval.decide("tu2", crate::session::ApprovalDecision::Approve) {
            tokio::task::yield_now().await;
        }
        let outcome = gate.await.unwrap();
        assert!(matches!(outcome, ApprovalOutcome::Approve { .. }));
        assert!(matches!(
            broker.get(&request_id).map(|r| r.state),
            Some(crate::permission::PermissionRequestState::Approved { .. })
        ));
    }

    #[tokio::test]
    async fn approved_write_consumes_one_gate_without_second_prompt() {
        let trust = TrustConfig {
            mode: TrustMode::Steady,
            ..TrustConfig::default()
        };
        let (mut ctx, approval, _flows) = ctx_with_broker(trust);
        let _updates = approval.subscribe();
        ctx.fs_access = crate::fs_access::FsAccessPolicy {
            mode: crate::fs_access::FsAccessMode::ReadOnly,
            workspace: None,
        };
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("approved.txt");
        let args = ToolArgs {
            named: vec![
                ("path".into(), crate::value::Value::Path(target.clone())),
                ("content".into(), crate::value::Value::Str("ok".into())),
            ],
            ..ToolArgs::default()
        };
        let gate = tokio::spawn({
            let ctx = ctx.clone();
            let args = args.clone();
            async move {
                request_approval(
                    &ctx,
                    "write-1",
                    "fs.write",
                    &args,
                    ApprovalLevel::Approve,
                    Some(&crate::tools::fs::FsWrite),
                )
                .await
            }
        });
        while approval.list_pending().is_empty() {
            tokio::task::yield_now().await;
        }
        assert_eq!(approval.list_pending().len(), 1);
        assert!(approval.decide("write-1", crate::session::ApprovalDecision::Approve));
        let ApprovalOutcome::Approve { authorization } = gate.await.unwrap() else {
            panic!("expected approval");
        };
        let call_ctx = ctx.authorized_for(*authorization);
        crate::tools::fs::FsWrite
            .call(args, &call_ctx)
            .await
            .unwrap();

        assert_eq!(tokio::fs::read_to_string(target).await.unwrap(), "ok");
        assert!(approval.list_pending().is_empty());
    }

    /// When the requesting run goes terminal, the broker cancels and the queued
    /// prompt must disappear rather than waiting on a user who has nothing to answer.
    #[tokio::test]
    async fn broker_cancellation_clears_queued_prompt() {
        let trust = TrustConfig {
            mode: TrustMode::Steady,
            ..TrustConfig::default()
        };
        let (ctx, approval, flows) = ctx_with_broker(trust);
        let broker = ctx.permission_broker.clone().unwrap();
        let run_id = ctx.flow_run_id.clone().unwrap();
        let gate = tokio::spawn({
            let ctx = ctx.clone();
            async move {
                request_approval(
                    &ctx,
                    "tu3",
                    "probe.tool",
                    &ToolArgs::default(),
                    ApprovalLevel::Approve,
                    Some(&Tier2Tool),
                )
                .await
            }
        });
        while broker.list().is_empty() {
            tokio::task::yield_now().await;
        }
        flows.mark_terminal(&run_id);
        let outcome = gate.await.unwrap();
        assert!(matches!(outcome, ApprovalOutcome::Deny { .. }));
        assert!(approval.list_pending().is_empty());
    }
}
