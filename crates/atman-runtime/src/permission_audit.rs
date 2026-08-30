use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::event::FlowRunId;
use crate::permission::{
    ApprovalTarget, DecisionActor, EscalationHop, ExecutionBoundary, GrantScope, GroupOwner,
    PermissionDecision, PermissionGrant, PermissionGroup, PermissionGroupId, PermissionRequest,
    PermissionRequestId, PermissionRequestState,
};
use crate::tool::Tier;
use crate::trust::{ExecutionPolicy, PolicyResolution, RiskKind, TrustConfig};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionProjectionActor {
    Policy {
        policy_version: String,
        rule_id: String,
    },
    Flow {
        session_id: String,
        run_id: FlowRunId,
    },
    User {
        session_id: String,
        principal_id: Option<String>,
    },
    System {
        component: String,
    },
    UnknownLegacy {
        label: String,
    },
}

impl From<&DecisionActor> for PermissionProjectionActor {
    fn from(actor: &DecisionActor) -> Self {
        match actor {
            DecisionActor::Policy {
                policy_version,
                rule_id,
            } => Self::Policy {
                policy_version: policy_version.clone(),
                rule_id: rule_id.clone(),
            },
            DecisionActor::Flow { session_id, run_id } => Self::Flow {
                session_id: session_id.clone(),
                run_id: run_id.clone(),
            },
            DecisionActor::User {
                session_id,
                principal_id,
            } => Self::User {
                session_id: session_id.clone(),
                principal_id: principal_id.clone(),
            },
            DecisionActor::System { component } => Self::System {
                component: component.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionAuditTarget {
    Flow { run_id: FlowRunId },
    User,
}

impl From<&ApprovalTarget> for PermissionAuditTarget {
    fn from(target: &ApprovalTarget) -> Self {
        match target {
            ApprovalTarget::Flow(run_id) => Self::Flow {
                run_id: run_id.clone(),
            },
            ApprovalTarget::User => Self::User,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionPolicyReference {
    pub snapshot_id: String,
    pub rule_id: String,
}

impl PermissionPolicyReference {
    pub fn capture(
        policy: &TrustConfig,
        tier: Tier,
        risks: &BTreeSet<RiskKind>,
        execution_policy: ExecutionPolicy,
        effective: PolicyResolution,
    ) -> Self {
        let bytes = serde_json::to_vec(policy).expect("TrustConfig is serializable");
        let digest = blake3::hash(&bytes).to_hex().to_string();
        let risk_names = risks
            .iter()
            .map(|risk| format!("{risk:?}"))
            .collect::<Vec<_>>();
        let policy_resolution = policy.resolve_policy_resolution(tier, risks.iter().copied());
        Self {
            snapshot_id: format!("blake3:{digest}"),
            rule_id: format!(
                "mode={:?};tier={tier:?};risks={};escalation={:?};escalation_step={:?};policy_result={:?};effective_result={:?};execution={execution_policy:?}",
                policy.mode,
                risk_names.join(","),
                policy.escalation,
                policy_resolution.escalation,
                policy_resolution.action,
                effective.action,
            ),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionProvenanceSummary {
    pub cwd: Option<String>,
    pub path: Option<String>,
    pub path_origin: Option<String>,
    pub workspace_id: Option<String>,
    pub workspace_root: Option<String>,
    pub repository_root: Option<String>,
    pub network: bool,
    pub risks: BTreeSet<String>,
    pub targets: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionEscalationAuditHop {
    pub target: PermissionAuditTarget,
    pub actor: Option<PermissionProjectionActor>,
    pub action: Option<String>,
    pub reason: Option<String>,
    pub at: DateTime<Utc>,
}

impl From<&EscalationHop> for PermissionEscalationAuditHop {
    fn from(hop: &EscalationHop) -> Self {
        Self {
            target: (&hop.target).into(),
            actor: hop.actor.as_ref().map(Into::into),
            action: hop
                .action
                .map(|action| format!("{action:?}").to_lowercase()),
            reason: hop.reason.clone(),
            at: hop.at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionAuditScope {
    CurrentCall,
    ChildRunSameTool {
        run_id: FlowRunId,
        tool_name: String,
    },
    ChildRunSamePathRule {
        run_id: FlowRunId,
        tool_name: String,
        workspace_relative_path: String,
    },
}

impl From<&GrantScope> for PermissionAuditScope {
    fn from(scope: &GrantScope) -> Self {
        match scope {
            GrantScope::CurrentCall => Self::CurrentCall,
            GrantScope::ChildRunSameTool { run_id, tool_name } => Self::ChildRunSameTool {
                run_id: run_id.clone(),
                tool_name: tool_name.clone(),
            },
            GrantScope::ChildRunSamePathRule {
                run_id,
                tool_name,
                workspace_relative_path,
            } => Self::ChildRunSamePathRule {
                run_id: run_id.clone(),
                tool_name: tool_name.clone(),
                workspace_relative_path: workspace_relative_path.clone(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionRequestAudit {
    pub request_id: Option<PermissionRequestId>,
    #[serde(default)]
    pub revision: u64,
    pub session_id: String,
    pub requesting_run_id: FlowRunId,
    pub parent_run_id: Option<FlowRunId>,
    pub root_run_id: FlowRunId,
    pub tool_use_id: String,
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_intent: Option<crate::message::ToolCallIntent>,
    pub tier: Tier,
    /// Process boundary selected for this approval. Pending user approvals
    /// report the boundary that an approval will grant; denied and non-process
    /// requests have no execution boundary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_boundary: Option<ExecutionBoundary>,
    #[serde(default)]
    pub provenance: PermissionProvenanceSummary,
    pub target: PermissionAuditTarget,
    pub group_ids: Vec<PermissionGroupId>,
    pub policy: PermissionPolicyReference,
    pub escalation_path: Vec<PermissionEscalationAuditHop>,
    pub decision_id: Option<String>,
    pub actor: Option<PermissionProjectionActor>,
    pub scope: Option<PermissionAuditScope>,
    pub reason: Option<String>,
    pub at: DateTime<Utc>,
}

impl PermissionRequestAudit {
    pub fn from_request(
        request: &PermissionRequest,
        group_ids: Vec<PermissionGroupId>,
        decision: Option<&PermissionDecision>,
        at: DateTime<Utc>,
    ) -> Self {
        let provenance = &request.intent.provenance;
        let target = match &request.state {
            PermissionRequestState::Pending { target } => target,
            _ => request
                .escalation_path
                .last()
                .map(|hop| &hop.target)
                .expect("request has target"),
        };
        Self {
            request_id: Some(request.request_id.clone()),
            revision: request.revision,
            session_id: request.session_id.clone(),
            requesting_run_id: request.requesting_run_id.clone(),
            parent_run_id: request.parent_run_id.clone(),
            root_run_id: request.root_run_id.clone(),
            tool_use_id: request.intent.tool_use_id.clone(),
            tool: request.intent.tool_name.clone(),
            call_intent: request.intent.call_intent.clone(),
            tier: request.intent.tier,
            execution_boundary: request
                .intent
                .risks
                .contains(&RiskKind::ProcessSpawn)
                .then(|| match decision {
                    Some(decision)
                        if decision.action == crate::permission::PermissionAction::Approve =>
                    {
                        Some(decision.execution_boundary)
                    }
                    Some(_) => None,
                    None if matches!(request.state, PermissionRequestState::Pending { .. }) => {
                        Some(if matches!(target, ApprovalTarget::User) {
                            ExecutionBoundary::Direct
                        } else {
                            ExecutionBoundary::Sandboxed
                        })
                    }
                    None => None,
                })
                .flatten(),
            provenance: PermissionProvenanceSummary {
                cwd: provenance
                    .cwd
                    .as_ref()
                    .map(|path| path.display().to_string()),
                path: provenance
                    .path
                    .as_ref()
                    .map(|path| path.display().to_string()),
                path_origin: provenance.path_origin.map(|origin| format!("{origin:?}")),
                workspace_id: provenance.workspace_id.clone(),
                workspace_root: provenance
                    .workspace_root
                    .as_ref()
                    .map(|path| path.display().to_string()),
                repository_root: provenance
                    .repository_root
                    .as_ref()
                    .map(|path| path.display().to_string()),
                network: provenance.network,
                risks: provenance
                    .risks
                    .iter()
                    .map(|risk| format!("{risk:?}"))
                    .collect(),
                targets: provenance
                    .authorized_targets()
                    .map(|path| path.display().to_string())
                    .collect(),
            },
            target: target.into(),
            group_ids,
            policy: request.policy_reference.clone(),
            escalation_path: request.escalation_path.iter().map(Into::into).collect(),
            decision_id: decision.map(|decision| decision.decision_id.to_string()),
            actor: decision
                .map(|decision| (&decision.actor).into())
                .or_else(|| {
                    request
                        .escalation_path
                        .iter()
                        .rev()
                        .find_map(|hop| hop.actor.as_ref().map(Into::into))
                }),
            scope: decision.and_then(|decision| decision.grant_scope.as_ref().map(Into::into)),
            reason: decision
                .and_then(|decision| decision.reason.clone())
                .or_else(|| {
                    request
                        .escalation_path
                        .iter()
                        .rev()
                        .find_map(|hop| hop.reason.clone())
                })
                .or_else(|| match &request.state {
                    PermissionRequestState::Cancelled { reason } => Some(reason.clone()),
                    _ => None,
                }),
            at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PermissionGroupAuditOwner {
    Flow { run_id: FlowRunId },
    User { session_id: String },
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionGroupAudit {
    pub group_id: PermissionGroupId,
    pub owner: PermissionGroupAuditOwner,
    pub label: String,
    pub request_ids: Vec<PermissionRequestId>,
    pub revision: u64,
    pub at: DateTime<Utc>,
}

impl PermissionGroupAudit {
    pub fn from_group(group: &PermissionGroup, session_id: &str, at: DateTime<Utc>) -> Self {
        let owner = match &group.owner {
            GroupOwner::Flow(run_id) => PermissionGroupAuditOwner::Flow {
                run_id: run_id.clone(),
            },
            GroupOwner::User => PermissionGroupAuditOwner::User {
                session_id: session_id.to_owned(),
            },
            GroupOwner::System => PermissionGroupAuditOwner::System,
        };
        Self {
            group_id: group.group_id.clone(),
            owner,
            label: group.label.clone(),
            request_ids: group.request_ids.iter().cloned().collect(),
            revision: group.revision,
            at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PermissionGrantAudit {
    pub grant_id: crate::permission::PermissionGrantId,
    pub request_id: PermissionRequestId,
    pub requesting_run_id: FlowRunId,
    pub actor: PermissionProjectionActor,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_boundary: Option<ExecutionBoundary>,
    pub scope: PermissionAuditScope,
    pub reason: Option<String>,
    pub at: DateTime<Utc>,
}

impl PermissionGrantAudit {
    pub fn from_grant(grant: &PermissionGrant, reason: Option<String>, at: DateTime<Utc>) -> Self {
        Self::from_grant_with_actor(grant, (&grant.actor).into(), reason, at)
    }

    pub fn from_grant_with_actor(
        grant: &PermissionGrant,
        actor: PermissionProjectionActor,
        reason: Option<String>,
        at: DateTime<Utc>,
    ) -> Self {
        Self {
            grant_id: grant.grant_id.clone(),
            request_id: grant.request_id.clone(),
            requesting_run_id: grant.requesting_run_id.clone(),
            actor,
            execution_boundary: grant
                .requirement
                .risks
                .contains(&RiskKind::ProcessSpawn)
                .then_some(grant.execution_boundary),
            scope: (&grant.scope).into(),
            reason,
            at,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", content = "payload", rename_all = "snake_case")]
pub enum PermissionAuditRecord {
    RequestCreated(PermissionRequestAudit),
    RequestTargeted(PermissionRequestAudit),
    RequestDeferred(PermissionRequestAudit),
    RequestApproved(PermissionRequestAudit),
    RequestDenied(PermissionRequestAudit),
    RequestCancelled(PermissionRequestAudit),
    GroupCreated(PermissionGroupAudit),
    GroupUpdated(PermissionGroupAudit),
    GroupResolved(PermissionGroupAudit),
    GrantCreated(PermissionGrantAudit),
    GrantExpired(PermissionGrantAudit),
    UnrestrictedExecution(PermissionRequestAudit),
}

#[derive(Clone)]
pub struct PermissionAuditProjector {
    sink: crate::event::EventSink,
    stream: tokio::sync::broadcast::Sender<crate::stream::StreamFrame>,
}

impl PermissionAuditProjector {
    pub fn new(
        sink: crate::event::EventSink,
        stream: tokio::sync::broadcast::Sender<crate::stream::StreamFrame>,
    ) -> Self {
        Self { sink, stream }
    }

    pub fn emit(&self, record: PermissionAuditRecord) {
        self.sink.emit(record.clone().into());
        let _ = self.stream.send(record.into());
    }
}

trait AuditAnchor {
    fn stream_anchor(&self) -> String;
}

impl AuditAnchor for PermissionRequestAudit {
    fn stream_anchor(&self) -> String {
        self.requesting_run_id.to_string()
    }
}

impl AuditAnchor for PermissionGroupAudit {
    fn stream_anchor(&self) -> String {
        match &self.owner {
            PermissionGroupAuditOwner::Flow { run_id } => run_id.to_string(),
            PermissionGroupAuditOwner::User { session_id } => format!("user:{session_id}"),
            PermissionGroupAuditOwner::System => "system".into(),
        }
    }
}

impl AuditAnchor for PermissionGrantAudit {
    fn stream_anchor(&self) -> String {
        self.requesting_run_id.to_string()
    }
}

macro_rules! audit_conversions {
    ($(($record:ident, $event:ident, $frame:ident)),+ $(,)?) => {
        impl From<PermissionAuditRecord> for crate::event::Event {
            fn from(record: PermissionAuditRecord) -> Self {
                match record {
                    $(PermissionAuditRecord::$record(payload) => Self::$event { payload },)+
                }
            }
        }

        impl From<PermissionAuditRecord> for crate::stream::StreamFrame {
            fn from(record: PermissionAuditRecord) -> Self {
                match record {
                    $(PermissionAuditRecord::$record(payload) => Self::$frame {
                        run_id: payload.stream_anchor(),
                        payload,
                    },)+
                }
            }
        }
    };
}

audit_conversions!(
    (
        RequestCreated,
        PermissionRequestCreated,
        PermissionRequestCreated
    ),
    (
        RequestTargeted,
        PermissionRequestTargeted,
        PermissionRequestTargeted
    ),
    (
        RequestDeferred,
        PermissionRequestDeferred,
        PermissionRequestDeferred
    ),
    (
        RequestApproved,
        PermissionRequestApproved,
        PermissionRequestApproved
    ),
    (
        RequestDenied,
        PermissionRequestDenied,
        PermissionRequestDenied
    ),
    (
        RequestCancelled,
        PermissionRequestCancelled,
        PermissionRequestCancelled
    ),
    (GroupCreated, PermissionGroupCreated, PermissionGroupCreated),
    (GroupUpdated, PermissionGroupUpdated, PermissionGroupUpdated),
    (
        GroupResolved,
        PermissionGroupResolved,
        PermissionGroupResolved
    ),
    (GrantCreated, PermissionGrantCreated, PermissionGrantCreated),
    (GrantExpired, PermissionGrantExpired, PermissionGrantExpired),
    (
        UnrestrictedExecution,
        UnrestrictedExecution,
        UnrestrictedExecution
    ),
);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{EventEnvelope, FlowRunId};
    use crate::permission::{PermissionGrantId, PermissionGroupId, PermissionRequestId};
    use crate::stream::frame_run_id;

    fn request(run_id: &FlowRunId) -> PermissionRequestAudit {
        PermissionRequestAudit {
            request_id: Some(PermissionRequestId::now()),
            revision: 1,
            session_id: "session".into(),
            requesting_run_id: run_id.clone(),
            parent_run_id: None,
            root_run_id: run_id.clone(),
            tool_use_id: "tool-use".into(),
            tool: "fs.read".into(),
            call_intent: None,
            tier: Tier::Two,
            execution_boundary: None,
            provenance: PermissionProvenanceSummary::default(),
            target: PermissionAuditTarget::User,
            group_ids: Vec::new(),
            policy: PermissionPolicyReference {
                snapshot_id: "snapshot".into(),
                rule_id: "rule".into(),
            },
            escalation_path: Vec::new(),
            decision_id: None,
            actor: None,
            scope: None,
            reason: None,
            at: Utc::now(),
        }
    }

    #[test]
    fn legacy_request_audit_has_unknown_execution_boundary() {
        let mut value = serde_json::to_value(request(&FlowRunId::now())).unwrap();
        value.as_object_mut().unwrap().remove("execution_boundary");

        let decoded: PermissionRequestAudit = serde_json::from_value(value).unwrap();

        assert_eq!(decoded.execution_boundary, None);
    }

    #[test]
    fn s8_actor_origin_matrix_remains_distinguishable_after_roundtrip() {
        let run_id = FlowRunId::now();
        let actors = [
            PermissionProjectionActor::Policy {
                policy_version: "snapshot".into(),
                rule_id: "automatic:tier".into(),
            },
            PermissionProjectionActor::Flow {
                session_id: "session".into(),
                run_id: run_id.clone(),
            },
            PermissionProjectionActor::User {
                session_id: "session".into(),
                principal_id: Some("operator".into()),
            },
            PermissionProjectionActor::Policy {
                policy_version: "snapshot".into(),
                rule_id: "policy:deny".into(),
            },
            PermissionProjectionActor::System {
                component: "permission.run_cleanup".into(),
            },
            PermissionProjectionActor::UnknownLegacy {
                label: "legacy approver unavailable".into(),
            },
        ];
        let roundtripped = actors
            .iter()
            .map(|actor| {
                serde_json::from_str::<PermissionProjectionActor>(
                    &serde_json::to_string(actor).unwrap(),
                )
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(roundtripped, actors);
        assert!(
            matches!(roundtripped[0], PermissionProjectionActor::Policy { ref rule_id, .. } if rule_id == "automatic:tier")
        );
        assert!(matches!(
            roundtripped[1],
            PermissionProjectionActor::Flow { .. }
        ));
        assert!(matches!(
            roundtripped[2],
            PermissionProjectionActor::User { .. }
        ));
        assert!(
            matches!(roundtripped[3], PermissionProjectionActor::Policy { ref rule_id, .. } if rule_id == "policy:deny")
        );
        assert!(matches!(
            roundtripped[4],
            PermissionProjectionActor::System { .. }
        ));
        assert!(matches!(
            roundtripped[5],
            PermissionProjectionActor::UnknownLegacy { .. }
        ));
    }

    #[test]
    fn s8_all_permission_audit_variants_roundtrip_and_keep_event_frame_mappings() {
        let run_id = FlowRunId::now();
        let request = request(&run_id);
        let group = PermissionGroupAudit {
            group_id: PermissionGroupId(uuid::Uuid::now_v7()),
            owner: PermissionGroupAuditOwner::Flow {
                run_id: run_id.clone(),
            },
            label: "group".into(),
            request_ids: vec![request.request_id.clone().unwrap()],
            revision: 1,
            at: Utc::now(),
        };
        let grant = PermissionGrantAudit {
            grant_id: PermissionGrantId(uuid::Uuid::now_v7()),
            request_id: request.request_id.clone().unwrap(),
            requesting_run_id: run_id.clone(),
            actor: PermissionProjectionActor::System {
                component: "test".into(),
            },
            execution_boundary: None,
            scope: PermissionAuditScope::CurrentCall,
            reason: None,
            at: Utc::now(),
        };
        let cases = [
            (
                PermissionAuditRecord::RequestCreated(request.clone()),
                "permission_request_created",
            ),
            (
                PermissionAuditRecord::RequestTargeted(request.clone()),
                "permission_request_targeted",
            ),
            (
                PermissionAuditRecord::RequestDeferred(request.clone()),
                "permission_request_deferred",
            ),
            (
                PermissionAuditRecord::RequestApproved(request.clone()),
                "permission_request_approved",
            ),
            (
                PermissionAuditRecord::RequestDenied(request.clone()),
                "permission_request_denied",
            ),
            (
                PermissionAuditRecord::RequestCancelled(request.clone()),
                "permission_request_cancelled",
            ),
            (
                PermissionAuditRecord::GroupCreated(group.clone()),
                "permission_group_created",
            ),
            (
                PermissionAuditRecord::GroupUpdated(group.clone()),
                "permission_group_updated",
            ),
            (
                PermissionAuditRecord::GroupResolved(group),
                "permission_group_resolved",
            ),
            (
                PermissionAuditRecord::GrantCreated(grant.clone()),
                "permission_grant_created",
            ),
            (
                PermissionAuditRecord::GrantExpired(grant),
                "permission_grant_expired",
            ),
            (
                PermissionAuditRecord::UnrestrictedExecution(request),
                "unrestricted_execution",
            ),
        ];

        for (seq, (record, expected_kind)) in cases.into_iter().enumerate() {
            let event: crate::event::Event = record.clone().into();
            let frame: crate::stream::StreamFrame = record.into();
            assert_eq!(crate::event_writer::event_kind(&event), expected_kind);
            let run_id_text = run_id.to_string();
            assert_eq!(
                crate::event_writer::extract_anchors(&event).1.as_deref(),
                Some(run_id_text.as_str())
            );
            assert_eq!(frame_run_id(&frame), Some(run_id_text.as_str()));
            let json = serde_json::to_string(&EventEnvelope::new(seq as u64 + 1, event)).unwrap();
            let roundtrip: EventEnvelope = serde_json::from_str(&json).unwrap();
            assert_eq!(
                crate::event_writer::event_kind(&roundtrip.event),
                expected_kind
            );
        }
    }
}
