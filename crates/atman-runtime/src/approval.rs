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
    original_request_id: Option<crate::permission::PermissionRequestId>,
) -> Result<crate::permission::SubmissionOutcome, String> {
    let broker = ctx
        .permission_broker
        .as_ref()
        .ok_or_else(|| "missing permission broker".to_string())?;
    let identity = ctx
        .flow_identity
        .as_ref()
        .ok_or_else(|| "missing flow identity".to_string())?;
    let trust = ctx
        .trust
        .as_ref()
        .ok_or_else(|| "missing trust snapshot".to_string())?;
    // A broker bound to a different registry would authenticate this identity
    // against a foreign authority graph, so refuse rather than mis-authorize.
    let registry = ctx
        .flow_registry
        .as_ref()
        .ok_or_else(|| "missing flow registry".to_string())?;
    if !broker.is_for_registry(registry) {
        return Err("permission broker and flow registry mismatch".into());
    }
    broker
        .submit_with_original_request_id(
            Some(identity.session_id.as_str()),
            Some(&identity.run_id),
            intent,
            tier == crate::tool::Tier::Four,
            original_request_id,
            trust,
        )
        .map_err(|error| error.to_string())
}

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
    decision: crate::session::ApprovalDecision,
) -> crate::session::ApprovalDecision {
    let (Some(broker), Some(identity)) =
        (ctx.permission_broker.as_ref(), ctx.flow_identity.as_ref())
    else {
        return crate::session::ApprovalDecision::Deny {
            reason: "permission broker identity unavailable".into(),
        };
    };
    let (action, grant_scope, reason) = match &decision {
        crate::session::ApprovalDecision::Approve => (
            crate::permission::PermissionAction::Approve,
            Some(crate::permission::GrantScope::CurrentCall),
            None,
        ),
        crate::session::ApprovalDecision::Deny { reason } => (
            crate::permission::PermissionAction::Deny,
            None,
            Some(reason.clone()),
        ),
    };
    let authority = crate::permission::DecisionAuthority::User(
        broker.user_authority(identity.session_id.clone(), None),
    );
    match broker.resolve(request_id, &authority, action, grant_scope, reason) {
        Ok(crate::permission::ResolveOutcome::Resolved(resolved)) if resolved.action == action => {
            decision
        }
        Ok(_) | Err(crate::permission::PermissionError::AlreadyResolved) => {
            crate::session::ApprovalDecision::Deny {
                reason: "permission request was already settled".into(),
            }
        }
        Err(error) => {
            crate::notify!(warn, "permission resolve failed: {error}");
            crate::session::ApprovalDecision::Deny {
                reason: format!("permission resolve failed: {error}"),
            }
        }
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
    request_approval_with_additional_risks(ctx, id, name, call_args, level, tool, []).await
}

struct ApprovalSubmissionContext<I> {
    additional_risks: I,
    original_request_id: Option<crate::permission::PermissionRequestId>,
}

pub async fn request_approval_with_additional_risks(
    ctx: &ToolCtx,
    id: &str,
    name: &str,
    call_args: &ToolArgs,
    level: ApprovalLevel,
    tool: Option<&dyn crate::tool::Tool>,
    additional_risks: impl IntoIterator<Item = RiskKind>,
) -> ApprovalOutcome {
    request_approval_with_context(
        ctx,
        id,
        name,
        call_args,
        level,
        tool,
        ApprovalSubmissionContext {
            additional_risks,
            original_request_id: None,
        },
    )
    .await
}

pub async fn request_sandbox_relaxation_approval(
    ctx: &ToolCtx,
    id: &str,
    name: &str,
    call_args: &ToolArgs,
    level: ApprovalLevel,
    tool: Option<&dyn crate::tool::Tool>,
) -> ApprovalOutcome {
    let Some(authorization) = ctx.invocation_authorization() else {
        return ApprovalOutcome::Deny {
            reason: "sandbox relaxation requires the strict invocation authorization".into(),
        };
    };
    if !authorization.is_for_call(id, name) {
        return ApprovalOutcome::Deny {
            reason: "sandbox relaxation authorization does not match this invocation".into(),
        };
    }
    request_approval_with_context(
        ctx,
        id,
        name,
        call_args,
        level,
        tool,
        ApprovalSubmissionContext {
            additional_risks: [RiskKind::SandboxViolation],
            original_request_id: Some(authorization.request_id().clone()),
        },
    )
    .await
}

async fn request_approval_with_context(
    ctx: &ToolCtx,
    id: &str,
    name: &str,
    call_args: &ToolArgs,
    level: ApprovalLevel,
    tool: Option<&dyn crate::tool::Tool>,
    submission: ApprovalSubmissionContext<impl IntoIterator<Item = RiskKind>>,
) -> ApprovalOutcome {
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
    let permit_provenance = provenance.clone();
    let permit = |request_id, manual: bool| {
        crate::permission::InvocationAuthorization::new(
            request_id,
            id,
            name,
            permit_provenance.clone(),
            manual,
        )
    };
    let Some(run_id) = ctx.flow_run_id.clone() else {
        return ApprovalOutcome::Deny {
            reason: format!("{name}: blocked — missing run identity"),
        };
    };
    let effective_level = level;
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
    let mut risks = intent_risks(tier, &provenance);
    risks.extend(submission.additional_risks);
    let intent = crate::permission::PermissionIntent {
        tool_use_id: id.to_string(),
        tool_name: name.to_string(),
        tier,
        risks,
        args_digest: args_digest(&args_preview),
        preview: preview.clone(),
        provenance,
    };
    // The broker owns the policy decision; the legacy queue below is still the
    // only surface that renders a prompt, so a Pending outcome is handed to it.
    let brokered = match submit_to_broker(ctx, intent, tier, submission.original_request_id) {
        Ok(outcome) => outcome,
        Err(error) => {
            return ApprovalOutcome::Deny {
                reason: format!("{name}: permission broker rejected the request: {error}"),
            };
        }
    };
    let mut approved_request_id = None;
    let pending_permission = match brokered {
        crate::permission::SubmissionOutcome::Immediate(immediate) => {
            use crate::permission::ImmediateAuthorization;
            let request_id = immediate.request.request_id.clone();
            match immediate.authorization {
                ImmediateAuthorization::Unrestricted => {
                    emit_approval_result(
                        ctx,
                        &run_id,
                        id,
                        &crate::session::ApprovalDecision::Approve,
                        "unrestricted",
                    );
                    return ApprovalOutcome::Approve {
                        authorization: Box::new(permit(request_id.clone(), false)),
                    };
                }
                ImmediateAuthorization::Auto => {
                    emit_approval_result(
                        ctx,
                        &run_id,
                        id,
                        &crate::session::ApprovalDecision::Approve,
                        "policy",
                    );
                    return ApprovalOutcome::Approve {
                        authorization: Box::new(permit(request_id.clone(), false)),
                    };
                }
                ImmediateAuthorization::Granted { .. } => {
                    emit_approval_result(
                        ctx,
                        &run_id,
                        id,
                        &crate::session::ApprovalDecision::Approve,
                        "grant",
                    );
                    return ApprovalOutcome::Approve {
                        authorization: Box::new(permit(request_id.clone(), false)),
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
            }
        }
        crate::permission::SubmissionOutcome::Pending(pending) => Some(pending),
    };
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
        return ApprovalOutcome::Deny {
            reason: format!("{name}: blocked — no approval transport under controlled execution"),
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
            approved_request_id = Some(request_id.clone());
            let mut resolution = pending.resolution;
            tokio::select! {
                biased;
                queued = rx => {
                    let decision = queued.unwrap_or(crate::session::ApprovalDecision::Deny {
                        reason: "approval channel dropped".into(),
                    });
                    settle_broker_request(ctx, &request_id, decision)
                }
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
            }
        }
        None => rx.await.unwrap_or(crate::session::ApprovalDecision::Deny {
            reason: "approval channel dropped".into(),
        }),
    };
    emit_approval_result(ctx, &run_id, id, &decision, "user");
    match decision {
        crate::session::ApprovalDecision::Approve => ApprovalOutcome::Approve {
            authorization: Box::new(permit(
                approved_request_id.expect("approved broker request has an id"),
                true,
            )),
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
        PolicyAction, RiskPolicyConfig, RiskPolicyOverrides, TierPolicyConfig, TierPolicyOverrides,
        TrustConfig, TrustMode,
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

    async fn assert_gate_denied(ctx: &ToolCtx, expected: &str) {
        let outcome = request_approval(
            ctx,
            "tu_fail_closed",
            "probe.tool",
            &ToolArgs::default(),
            ApprovalLevel::Approve,
            Some(&Tier2Tool),
        )
        .await;
        let ApprovalOutcome::Deny { reason } = outcome else {
            panic!("expected fail-closed denial");
        };
        assert!(reason.contains(expected), "unexpected denial: {reason}");
    }

    #[tokio::test]
    async fn no_broker_fails_closed_instead_of_legacy_auto_approve() {
        let (mut ctx, approval, _flows) = ctx_with_broker(TrustConfig::default());
        ctx.permission_broker = None;
        assert_gate_denied(&ctx, "missing permission broker").await;
        assert!(approval.list_pending().is_empty());
    }

    #[tokio::test]
    async fn missing_flow_identity_fails_closed() {
        let (mut ctx, approval, _flows) = ctx_with_broker(TrustConfig::default());
        ctx.flow_identity = None;
        assert_gate_denied(&ctx, "missing flow identity").await;
        assert!(approval.list_pending().is_empty());
    }

    #[tokio::test]
    async fn missing_trust_snapshot_fails_closed() {
        let (mut ctx, approval, _flows) = ctx_with_broker(TrustConfig::default());
        ctx.trust = None;
        assert_gate_denied(&ctx, "missing trust snapshot").await;
        assert!(approval.list_pending().is_empty());
    }

    #[tokio::test]
    async fn missing_flow_registry_fails_closed() {
        let (mut ctx, approval, _flows) = ctx_with_broker(TrustConfig::default());
        ctx.flow_registry = None;
        assert_gate_denied(&ctx, "missing flow registry").await;
        assert!(approval.list_pending().is_empty());
    }

    #[tokio::test]
    async fn broker_registry_mismatch_fails_closed() {
        let (mut ctx, approval, _flows) = ctx_with_broker(TrustConfig::default());
        ctx.flow_registry = Some(Arc::new(FlowRegistry::new()));
        assert_gate_denied(&ctx, "broker and flow registry mismatch").await;
        assert!(approval.list_pending().is_empty());
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
    async fn sandbox_relaxation_requires_strict_invocation_authorization() {
        let (ctx, _approval, _flows) = ctx_with_broker(TrustConfig::default());
        let broker = ctx.permission_broker.clone().unwrap();
        let outcome = request_sandbox_relaxation_approval(
            &ctx,
            "relaxed",
            "probe.tool",
            &ToolArgs::default(),
            ApprovalLevel::Approve,
            Some(&Tier2Tool),
        )
        .await;
        assert!(matches!(
            outcome,
            ApprovalOutcome::Deny { reason }
                if reason.contains("strict invocation authorization")
        ));
        assert!(broker.list().is_empty());
    }

    #[tokio::test]
    async fn sandbox_relaxation_rejects_authorization_for_another_call() {
        let (ctx, _approval, _flows) = ctx_with_broker(TrustConfig::default());
        let broker = ctx.permission_broker.clone().unwrap();
        let authorization = crate::permission::InvocationAuthorization::new(
            crate::permission::PermissionRequestId::now(),
            "other",
            "probe.tool",
            crate::permission::ResourceProvenance::none(),
            false,
        );
        let outcome = request_sandbox_relaxation_approval(
            &ctx.authorized_for(authorization),
            "relaxed",
            "probe.tool",
            &ToolArgs::default(),
            ApprovalLevel::Approve,
            Some(&Tier2Tool),
        )
        .await;
        assert!(matches!(
            outcome,
            ApprovalOutcome::Deny { reason } if reason.contains("does not match")
        ));
        assert!(broker.list().is_empty());
    }

    #[tokio::test]
    async fn sandbox_relaxation_rejects_nonexistent_original_request() {
        let (ctx, _approval, _flows) = ctx_with_broker(TrustConfig::default());
        let broker = ctx.permission_broker.clone().unwrap();
        let authorization = crate::permission::InvocationAuthorization::new(
            crate::permission::PermissionRequestId::now(),
            "relaxed",
            "probe.tool",
            crate::permission::ResourceProvenance::none(),
            false,
        );
        let outcome = request_sandbox_relaxation_approval(
            &ctx.authorized_for(authorization),
            "relaxed",
            "probe.tool",
            &ToolArgs::default(),
            ApprovalLevel::Approve,
            Some(&Tier2Tool),
        )
        .await;
        assert!(matches!(
            outcome,
            ApprovalOutcome::Deny { reason } if reason.contains("not found")
        ));
        assert!(broker.list().is_empty());
    }

    #[tokio::test]
    async fn only_explicit_sandbox_relaxation_records_request_lineage() {
        let trust = TrustConfig {
            mode: TrustMode::Eager,
            tiers: TierPolicyConfig {
                eager: TierPolicyOverrides {
                    tier2: Some(PolicyAction::Auto),
                    ..TierPolicyOverrides::default()
                },
            },
            risks: RiskPolicyConfig {
                eager: RiskPolicyOverrides {
                    sandbox_violation: Some(PolicyAction::Auto),
                    ..RiskPolicyOverrides::default()
                },
            },
            ..TrustConfig::default()
        };
        let (ctx, _approval, _flows) = ctx_with_broker(trust);
        let broker = ctx.permission_broker.clone().unwrap();
        let parent = request_approval(
            &ctx,
            "relaxed",
            "probe.tool",
            &ToolArgs::default(),
            ApprovalLevel::Approve,
            Some(&Tier2Tool),
        )
        .await;
        let ApprovalOutcome::Approve { authorization } = parent else {
            panic!("expected parent approval");
        };
        let parent_request_id = authorization.request_id().clone();
        let nested_ctx = ctx.authorized_for(*authorization);

        let nested = request_approval(
            &nested_ctx,
            "nested",
            "probe.tool",
            &ToolArgs::default(),
            ApprovalLevel::Approve,
            Some(&Tier2Tool),
        )
        .await;
        assert!(matches!(nested, ApprovalOutcome::Approve { .. }));
        let relaxed = request_sandbox_relaxation_approval(
            &nested_ctx,
            "relaxed",
            "probe.tool",
            &ToolArgs::default(),
            ApprovalLevel::Approve,
            Some(&Tier2Tool),
        )
        .await;
        assert!(matches!(relaxed, ApprovalOutcome::Approve { .. }));

        let requests = broker.list();
        let nested_request = requests
            .iter()
            .find(|request| request.intent.tool_use_id == "nested")
            .expect("nested request record");
        assert_eq!(nested_request.original_request_id, None);
        let relaxed_request = requests
            .iter()
            .find(|request| request.original_request_id.is_some())
            .expect("relaxed request record");
        assert_eq!(
            relaxed_request.original_request_id.as_ref(),
            Some(&parent_request_id)
        );
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

    #[tokio::test]
    async fn queued_approve_after_terminal_cancellation_fails_closed() {
        let trust = TrustConfig {
            mode: TrustMode::Steady,
            ..TrustConfig::default()
        };
        let (ctx, _approval, flows) = ctx_with_broker(trust);
        let broker = ctx.permission_broker.clone().unwrap();
        let identity = ctx.flow_identity.as_ref().unwrap();
        let outcome = broker
            .submit(
                Some(&identity.session_id),
                Some(&identity.run_id),
                crate::permission::PermissionIntent::minimal("race", "probe.tool", Tier::Two),
                false,
                ctx.trust.as_ref().unwrap(),
            )
            .unwrap();
        let crate::permission::SubmissionOutcome::Pending(pending) = outcome else {
            panic!("expected pending request");
        };
        let request_id = pending.request.request_id.clone();
        flows.mark_terminal(&identity.run_id);

        let decision =
            settle_broker_request(&ctx, &request_id, crate::session::ApprovalDecision::Approve);

        assert!(matches!(
            decision,
            crate::session::ApprovalDecision::Deny { .. }
        ));
        assert!(matches!(
            broker.get(&request_id).map(|request| request.state),
            Some(crate::permission::PermissionRequestState::Cancelled { .. })
        ));
    }

    #[tokio::test]
    async fn queued_denial_preserves_reason_and_settles_without_a_grant_scope() {
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
                    "tu_deny",
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
        while !approval.decide(
            "tu_deny",
            crate::session::ApprovalDecision::Deny {
                reason: "operator denied".into(),
            },
        ) {
            tokio::task::yield_now().await;
        }

        let outcome = gate.await.unwrap();
        assert!(matches!(
            outcome,
            ApprovalOutcome::Deny { ref reason } if reason == "operator denied"
        ));
        assert!(matches!(
            broker.get(&request_id).map(|request| request.state),
            Some(crate::permission::PermissionRequestState::Denied { .. })
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
        let ApprovalOutcome::Approve { authorization } = outcome else {
            panic!("expected approval");
        };
        assert_eq!(authorization.request_id(), &request_id);
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
