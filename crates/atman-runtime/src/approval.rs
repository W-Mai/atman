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
    let mut targets = provenance.authorized_targets().peekable();
    let external_non_temp = provenance.is_external()
        && (targets.peek().is_none()
            || targets.any(|target| !crate::fs_access::is_temp_path(target)));
    if external_non_temp {
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
        .submit(
            Some(identity.session_id.as_str()),
            Some(&identity.run_id),
            intent,
            tier == crate::tool::Tier::Four,
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

async fn defer_after_ancestor_timeout(
    broker: &crate::permission::PermissionBroker,
    request_id: &crate::permission::PermissionRequestId,
    target_run_id: &crate::event::FlowRunId,
) -> Result<bool, crate::permission::PermissionError> {
    tokio::time::sleep(crate::permission::ANCESTOR_OFFER_TIMEOUT).await;
    broker.defer_timed_out_target(request_id, target_run_id)
}

#[cfg(test)]
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

pub async fn authorize_tool_invocation(
    ctx: &ToolCtx,
    id: &str,
    name: &str,
    call_args: &ToolArgs,
    tool: &dyn crate::tool::Tool,
) -> Result<ToolCtx, String> {
    if tool.invocation_plane() == crate::tool::InvocationPlane::PermissionControl {
        let broker = ctx
            .permission_broker
            .as_ref()
            .ok_or_else(|| format!("{name}: permission broker is missing"))?;
        let actor = ctx
            .flow_identity
            .as_ref()
            .ok_or_else(|| format!("{name}: permission identity is missing"))?;
        broker
            .authenticate_control_actor(actor)
            .map_err(|error| format!("{name}: permission control rejected the actor: {error}"))?;
        return Ok(ctx.clone());
    }
    match request_approval(
        ctx,
        id,
        name,
        call_args,
        tool.approval_level(call_args, ctx),
        Some(tool),
    )
    .await
    {
        ApprovalOutcome::Approve { authorization } => Ok(ctx.authorized_for(*authorization)),
        ApprovalOutcome::Deny { reason } => Err(reason),
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

pub async fn request_approval_with_additional_risks(
    ctx: &ToolCtx,
    id: &str,
    name: &str,
    call_args: &ToolArgs,
    level: ApprovalLevel,
    tool: Option<&dyn crate::tool::Tool>,
    additional_risks: impl IntoIterator<Item = RiskKind>,
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
    let permit = |request_id, execution_boundary| {
        crate::permission::InvocationAuthorization::new(
            request_id,
            id,
            name,
            permit_provenance.clone(),
            execution_boundary,
        )
    };
    let Some(run_id) = ctx.flow_run_id.clone() else {
        return ApprovalOutcome::Deny {
            reason: format!("{name}: blocked — missing run identity"),
        };
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
    let mut risks = intent_risks(tier, &provenance);
    risks.extend(additional_risks);
    let intent = crate::permission::PermissionIntent {
        tool_use_id: id.to_string(),
        tool_name: name.to_string(),
        tier,
        risks,
        args_digest: args_digest(&args_preview),
        preview: preview.clone(),
        provenance,
    };
    let brokered = match submit_to_broker(ctx, intent, tier) {
        Ok(outcome) => outcome,
        Err(error) => {
            return ApprovalOutcome::Deny {
                reason: format!("{name}: permission broker rejected the request: {error}"),
            };
        }
    };
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
                        authorization: Box::new(permit(
                            request_id.clone(),
                            crate::permission::ExecutionBoundary::Direct,
                        )),
                    };
                }
                ImmediateAuthorization::Auto { execution_boundary } => {
                    emit_approval_result(
                        ctx,
                        &run_id,
                        id,
                        &crate::session::ApprovalDecision::Approve,
                        "policy",
                    );
                    return ApprovalOutcome::Approve {
                        authorization: Box::new(permit(request_id.clone(), execution_boundary)),
                    };
                }
                ImmediateAuthorization::Granted { grant } => {
                    emit_approval_result(
                        ctx,
                        &run_id,
                        id,
                        &crate::session::ApprovalDecision::Approve,
                        "grant",
                    );
                    return ApprovalOutcome::Approve {
                        authorization: Box::new(permit(
                            request_id.clone(),
                            grant.execution_boundary,
                        )),
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
    let mut pending_permission = pending_permission;
    let mut broker_resolution = None;
    if let Some(pending) = pending_permission.as_mut() {
        while let crate::permission::PermissionRequestState::Pending {
            target: crate::permission::ApprovalTarget::Flow(target_run_id),
        } = &pending.request.state
        {
            let target_run_id = target_run_id.clone();
            tokio::select! {
                changed = pending.target_changes.changed() => {
                    if changed.is_err() {
                        return ApprovalOutcome::Deny {
                            reason: format!("{name}: permission target transport closed"),
                        };
                    }
                }
                brokered = &mut pending.resolution => {
                    broker_resolution = Some(brokered);
                    break;
                }
                result = defer_after_ancestor_timeout(
                    ctx.permission_broker
                        .as_ref()
                        .expect("pending permission requires a broker"),
                    &pending.request.request_id,
                    &target_run_id,
                ) => {
                    if let Err(error) = result {
                        return ApprovalOutcome::Deny {
                            reason: format!("{name}: permission timeout escalation failed: {error}"),
                        };
                    }
                }
            }
            pending.request.state = crate::permission::PermissionRequestState::Pending {
                target: pending.target_changes.borrow().clone(),
            };
        }
    }
    if let Some(result) = broker_resolution {
        let (decision, execution_boundary) = match result {
            Ok(crate::permission::PermissionResolution::Decision(decision))
                if decision.action == crate::permission::PermissionAction::Approve =>
            {
                (
                    crate::session::ApprovalDecision::Approve,
                    decision.execution_boundary,
                )
            }
            Ok(crate::permission::PermissionResolution::Decision(decision)) => (
                crate::session::ApprovalDecision::Deny {
                    reason: decision
                        .reason
                        .unwrap_or_else(|| "denied by permission broker".into()),
                },
                crate::permission::ExecutionBoundary::Sandboxed,
            ),
            Ok(crate::permission::PermissionResolution::Cancelled { reason }) => (
                crate::session::ApprovalDecision::Deny { reason },
                crate::permission::ExecutionBoundary::Sandboxed,
            ),
            Err(_) => (
                crate::session::ApprovalDecision::Deny {
                    reason: "permission request dropped".into(),
                },
                crate::permission::ExecutionBoundary::Sandboxed,
            ),
        };
        emit_approval_result(ctx, &run_id, id, &decision, "broker");
        return match decision {
            crate::session::ApprovalDecision::Approve => ApprovalOutcome::Approve {
                authorization: Box::new(permit(
                    pending_permission
                        .as_ref()
                        .expect("pending permission")
                        .request
                        .request_id
                        .clone(),
                    execution_boundary,
                )),
            },
            crate::session::ApprovalDecision::Deny { reason } => ApprovalOutcome::Deny { reason },
        };
    }
    let Some(pending) = pending_permission else {
        return ApprovalOutcome::Deny {
            reason: format!("{name}: blocked — permission request transport unavailable"),
        };
    };
    let broker = ctx
        .permission_broker
        .as_ref()
        .expect("pending permission requires a broker");
    if !broker.has_clients() {
        let reason = "no permission client".to_string();
        if let Err(error) = broker.cancel(&pending.request.request_id, reason.clone())
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
    let request_id = pending.request.request_id.clone();
    let (decision, execution_boundary) = match pending.resolution.await {
        Ok(crate::permission::PermissionResolution::Decision(decision))
            if decision.action == crate::permission::PermissionAction::Approve =>
        {
            (
                crate::session::ApprovalDecision::Approve,
                decision.execution_boundary,
            )
        }
        Ok(crate::permission::PermissionResolution::Decision(decision)) => (
            crate::session::ApprovalDecision::Deny {
                reason: decision
                    .reason
                    .unwrap_or_else(|| "denied by permission broker".into()),
            },
            crate::permission::ExecutionBoundary::Sandboxed,
        ),
        Ok(crate::permission::PermissionResolution::Cancelled { reason }) => (
            crate::session::ApprovalDecision::Deny { reason },
            crate::permission::ExecutionBoundary::Sandboxed,
        ),
        Err(_) => (
            crate::session::ApprovalDecision::Deny {
                reason: "permission request dropped".into(),
            },
            crate::permission::ExecutionBoundary::Sandboxed,
        ),
    };
    emit_approval_result(ctx, &run_id, id, &decision, "user");
    match decision {
        crate::session::ApprovalDecision::Approve => ApprovalOutcome::Approve {
            authorization: Box::new(permit(request_id, execution_boundary)),
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

    struct ProcessTool;

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

    impl crate::tool::Tool for ProcessTool {
        fn name(&self) -> &str {
            "process.tool"
        }

        fn tier(&self) -> Tier {
            Tier::Two
        }

        fn invocation_provenance(
            &self,
            _args: &ToolArgs,
            _ctx: &ToolCtx,
        ) -> Result<crate::permission::ResourceProvenance, crate::error::RuntimeError> {
            Ok(crate::permission::ResourceProvenance::none()
                .with_risk(crate::trust::RiskKind::ProcessSpawn))
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
        std::mem::forget(broker.register_client());
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

    fn ctx_without_permission_client(trust: TrustConfig) -> ToolCtx {
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
        let mut ctx = ToolCtx::new()
            .with_permission_broker(broker)
            .with_flow_registry(flows)
            .with_trust(trust)
            .with_anchors(None, Some(run_id), None);
        ctx.flow_identity = Some(identity);
        ctx
    }

    fn resolve_as_user(
        broker: &Arc<PermissionBroker>,
        request_id: crate::permission::PermissionRequestId,
        action: crate::permission::PermissionAction,
        reason: Option<String>,
    ) {
        let expected = std::collections::HashMap::from([(
            request_id.clone(),
            broker.get(&request_id).unwrap().revision,
        )]);
        broker
            .user_resolve(
                "sess",
                Some("test-user".into()),
                vec![request_id],
                &expected,
                None,
                action,
                (action == crate::permission::PermissionAction::Approve)
                    .then_some(crate::permission::GrantScope::CurrentCall),
                reason,
            )
            .unwrap();
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

    #[test]
    fn temp_targets_do_not_add_workspace_external_risk() {
        let workspace = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::new().with_workspace(crate::git_workspace::WorkspaceBinding {
            workspace_id: "test".into(),
            repository_root: workspace.path().to_path_buf(),
            path: workspace.path().to_path_buf(),
            branch: None,
        });
        let provenance = crate::permission::ResourceProvenance::for_ctx(&ctx)
            .with_cwd(&ctx, Some(scratch.path()))
            .unwrap();

        let risks = intent_risks(Tier::Four, &provenance);

        assert!(risks.contains(&crate::trust::RiskKind::ProcessSpawn));
        assert!(!risks.contains(&crate::trust::RiskKind::WorkspaceExternal));
    }

    #[test]
    fn non_temp_external_targets_keep_workspace_external_risk() {
        let workspace = tempfile::tempdir().unwrap();
        let ctx = ToolCtx::new().with_workspace(crate::git_workspace::WorkspaceBinding {
            workspace_id: "test".into(),
            repository_root: workspace.path().to_path_buf(),
            path: workspace.path().to_path_buf(),
            branch: None,
        });
        let provenance = crate::permission::ResourceProvenance::for_ctx(&ctx)
            .with_cwd(&ctx, Some(std::path::Path::new("/etc")))
            .unwrap();

        let risks = intent_risks(Tier::Four, &provenance);

        assert!(risks.contains(&crate::trust::RiskKind::WorkspaceExternal));
    }

    #[tokio::test(start_paused = true)]
    async fn ancestor_offer_waits_exactly_thirty_seconds_before_deferring() {
        let flows = Arc::new(FlowRegistry::new());
        let trust = TrustConfig {
            mode: TrustMode::Steady,
            ..TrustConfig::default()
        };
        let root = flows
            .register_root(
                "sess".into(),
                FlowRunId::now(),
                EffectiveAuthority::root(&trust, true, None),
            )
            .unwrap();
        let requester = flows
            .register_child(
                &root.run_id,
                FlowRunId::now(),
                crate::flow_authority::InvocationKind::InlineSubflow,
                true,
                crate::flow_authority::ChildWorkspaceAuthority::Inherit,
            )
            .unwrap();
        let broker = PermissionBroker::shared(Arc::clone(&flows));
        let pending = match broker
            .submit(
                Some(&requester.session_id),
                Some(&requester.run_id),
                crate::permission::PermissionIntent {
                    tool_use_id: "call-1".into(),
                    tool_name: "probe.tool".into(),
                    tier: Tier::Two,
                    risks: Default::default(),
                    args_digest: "sha256:test".into(),
                    preview: None,
                    provenance: crate::permission::ResourceProvenance::none(),
                },
                false,
                &trust,
            )
            .unwrap()
        {
            crate::permission::SubmissionOutcome::Pending(pending) => pending,
            _ => panic!("expected pending"),
        };
        let request_id = pending.request.request_id.clone();
        let root_run_id = root.run_id.clone();
        let waiter = tokio::spawn({
            let broker = Arc::clone(&broker);
            async move { defer_after_ancestor_timeout(&broker, &request_id, &root_run_id).await }
        });

        tokio::time::advance(std::time::Duration::from_secs(29)).await;
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        assert!(matches!(
            broker.get(&pending.request.request_id).unwrap().state,
            crate::permission::PermissionRequestState::Pending {
                target: crate::permission::ApprovalTarget::Flow(_)
            }
        ));
        tokio::time::advance(std::time::Duration::from_secs(1)).await;
        assert!(waiter.await.unwrap().unwrap());
        assert_eq!(
            broker.get(&pending.request.request_id).unwrap().state,
            crate::permission::PermissionRequestState::Pending {
                target: crate::permission::ApprovalTarget::User
            }
        );
    }

    #[tokio::test]
    async fn ancestor_can_approve_descendant_before_user_prompt() {
        let trust = TrustConfig {
            mode: TrustMode::Steady,
            ..TrustConfig::default()
        };
        let flows = Arc::new(FlowRegistry::new());
        let root = flows
            .register_root(
                "sess".into(),
                FlowRunId::now(),
                EffectiveAuthority::root(&trust, true, None),
            )
            .unwrap();
        let child = flows
            .register_child(
                &root.run_id,
                FlowRunId::now(),
                crate::flow_authority::InvocationKind::InlineSubflow,
                true,
                crate::flow_authority::ChildWorkspaceAuthority::Inherit,
            )
            .unwrap();
        let broker = PermissionBroker::shared(Arc::clone(&flows));
        let approval = Arc::new(ApprovalRegistry::new());
        let _approval_watch = approval.subscribe();
        let mut ctx = ToolCtx::new()
            .with_approval(Arc::clone(&approval))
            .with_permission_broker(Arc::clone(&broker))
            .with_flow_registry(Arc::clone(&flows))
            .with_trust(trust)
            .with_anchors(None, Some(child.run_id.clone()), None);
        ctx.flow_identity = Some(child);
        let request = tokio::spawn(async move {
            request_approval(
                &ctx,
                "ancestor-approve",
                "probe.tool",
                &ToolArgs::default(),
                ApprovalLevel::Approve,
                Some(&Tier2Tool),
            )
            .await
        });
        let request_id = loop {
            if let Some(permission) = broker.list().into_iter().next() {
                break permission.request_id;
            }
            tokio::task::yield_now().await;
        };
        assert!(approval.list_pending().is_empty());
        let authority =
            crate::permission::DecisionAuthority::Flow(broker.flow_authority(root).unwrap());
        broker
            .resolve(
                &request_id,
                &authority,
                crate::permission::PermissionAction::Approve,
                Some(crate::permission::GrantScope::CurrentCall),
                None,
            )
            .unwrap();
        assert!(matches!(
            request.await.unwrap(),
            ApprovalOutcome::Approve { .. }
        ));
        assert!(approval.list_pending().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_ancestor_offer_reaches_user_prompt() {
        let trust = TrustConfig {
            mode: TrustMode::Steady,
            ..TrustConfig::default()
        };
        let flows = Arc::new(FlowRegistry::new());
        let root = flows
            .register_root(
                "sess".into(),
                FlowRunId::now(),
                EffectiveAuthority::root(&trust, true, None),
            )
            .unwrap();
        let child = flows
            .register_child(
                &root.run_id,
                FlowRunId::now(),
                crate::flow_authority::InvocationKind::InlineSubflow,
                true,
                crate::flow_authority::ChildWorkspaceAuthority::Inherit,
            )
            .unwrap();
        let broker = PermissionBroker::shared(Arc::clone(&flows));
        let approval = Arc::new(ApprovalRegistry::new());
        let _client = broker.register_client();
        let mut ctx = ToolCtx::new()
            .with_approval(Arc::clone(&approval))
            .with_permission_broker(Arc::clone(&broker))
            .with_flow_registry(Arc::clone(&flows))
            .with_trust(trust)
            .with_anchors(None, Some(child.run_id.clone()), None);
        ctx.flow_identity = Some(child);
        let request = tokio::spawn(async move {
            request_approval(
                &ctx,
                "ancestor-timeout",
                "probe.tool",
                &ToolArgs::default(),
                ApprovalLevel::Approve,
                Some(&Tier2Tool),
            )
            .await
        });
        let request_id = loop {
            if let Some(permission) = broker.list().into_iter().next() {
                break permission.request_id;
            }
            tokio::task::yield_now().await;
        };
        tokio::time::advance(crate::permission::ANCESTOR_OFFER_TIMEOUT).await;
        tokio::task::yield_now().await;
        assert!(matches!(
            broker.get(&request_id).unwrap().state,
            crate::permission::PermissionRequestState::Pending {
                target: crate::permission::ApprovalTarget::User
            }
        ));
        let expected = std::collections::HashMap::from([(
            request_id.clone(),
            broker.get(&request_id).unwrap().revision,
        )]);
        broker
            .user_resolve(
                "sess",
                Some("test-user".into()),
                vec![request_id],
                &expected,
                None,
                crate::permission::PermissionAction::Approve,
                Some(crate::permission::GrantScope::CurrentCall),
                None,
            )
            .unwrap();
        assert!(approval.list_pending().is_empty());
        assert!(matches!(
            request.await.unwrap(),
            ApprovalOutcome::Approve { .. }
        ));
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
    async fn broker_pending_without_approval_consumer_fails_closed() {
        let trust = TrustConfig {
            mode: TrustMode::Steady,
            ..TrustConfig::default()
        };
        let ctx = ctx_without_permission_client(trust);
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
        assert!(matches!(
            broker.get(&request_id).map(|request| request.state),
            Some(crate::permission::PermissionRequestState::Cancelled { .. })
        ));
    }

    #[tokio::test]
    async fn last_permission_client_disconnect_cancels_waiting_invocation() {
        let trust = TrustConfig {
            mode: TrustMode::Steady,
            ..TrustConfig::default()
        };
        let ctx = ctx_without_permission_client(trust);
        let broker = ctx.permission_broker.clone().unwrap();
        let first = broker.register_client();
        let second = broker.register_client();
        let gate = tokio::spawn({
            let ctx = ctx.clone();
            async move {
                request_approval(
                    &ctx,
                    "client-disconnect",
                    "probe.tool",
                    &ToolArgs::default(),
                    ApprovalLevel::Approve,
                    Some(&Tier2Tool),
                )
                .await
            }
        });
        let request_id = loop {
            if let Some(request) = broker.list().first() {
                break request.request_id.clone();
            }
            tokio::task::yield_now().await;
        };
        drop(first);
        assert!(matches!(
            broker.get(&request_id).map(|request| request.state),
            Some(crate::permission::PermissionRequestState::Pending { .. })
        ));
        drop(second);
        assert!(matches!(gate.await.unwrap(), ApprovalOutcome::Deny { .. }));
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
        resolve_as_user(
            &broker,
            request_id.clone(),
            crate::permission::PermissionAction::Deny,
            Some("operator denied".into()),
        );
        assert!(approval.list_pending().is_empty());

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

    #[tokio::test]
    async fn broker_user_decision_settles_waiting_invocation() {
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
        assert!(approval.list_pending().is_empty());
        resolve_as_user(
            &broker,
            request_id.clone(),
            crate::permission::PermissionAction::Approve,
            None,
        );
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
    async fn process_authorization_carries_selected_execution_boundary() {
        let (ctx, _approval, _flows) = ctx_with_broker(TrustConfig {
            mode: TrustMode::Steady,
            ..TrustConfig::default()
        });
        let broker = ctx.permission_broker.clone().unwrap();
        let gate = tokio::spawn({
            let ctx = ctx.clone();
            async move {
                request_approval(
                    &ctx,
                    "process-user",
                    "process.tool",
                    &ToolArgs::default(),
                    ApprovalLevel::Approve,
                    Some(&ProcessTool),
                )
                .await
            }
        });
        let request_id = loop {
            if let Some(request) = broker.list().first() {
                break request.request_id.clone();
            }
            tokio::task::yield_now().await;
        };
        resolve_as_user(
            &broker,
            request_id,
            crate::permission::PermissionAction::Approve,
            None,
        );
        let ApprovalOutcome::Approve { authorization } = gate.await.unwrap() else {
            panic!("expected user approval");
        };
        assert_eq!(
            authorization.execution_boundary(),
            crate::permission::ExecutionBoundary::Direct
        );

        let (ctx, _approval, _flows) = ctx_with_broker(TrustConfig {
            mode: TrustMode::Eager,
            escalation: crate::trust::EscalationPolicy::Allow,
            ..TrustConfig::default()
        });
        let ApprovalOutcome::Approve { authorization } = request_approval(
            &ctx,
            "process-eager-allow",
            "process.tool",
            &ToolArgs::default(),
            ApprovalLevel::Approve,
            Some(&ProcessTool),
        )
        .await
        else {
            panic!("expected eager allow authorization");
        };
        assert_eq!(
            authorization.execution_boundary(),
            crate::permission::ExecutionBoundary::Direct
        );
    }

    #[tokio::test]
    async fn approved_write_consumes_one_gate_without_second_prompt() {
        let trust = TrustConfig {
            mode: TrustMode::Steady,
            ..TrustConfig::default()
        };
        let (mut ctx, approval, _flows) = ctx_with_broker(trust);
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
        let broker = ctx.permission_broker.clone().unwrap();
        let request_id = loop {
            if let Some(request) = broker.list().first() {
                break request.request_id.clone();
            }
            tokio::task::yield_now().await;
        };
        assert!(approval.list_pending().is_empty());
        resolve_as_user(
            &broker,
            request_id,
            crate::permission::PermissionAction::Approve,
            None,
        );
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
