use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use uuid::Uuid;

use crate::event::FlowRunId;
use crate::flow_authority::{FlowExecutionState, FlowIdentity};
use crate::tool::{PathOrigin, Tier};
use crate::tools::agent_ctrl::FlowRegistry;
use crate::trust::{ExecutionPolicy, PolicyAction, RiskKind, TrustConfig};

macro_rules! permission_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn now() -> Self {
                Self(Uuid::now_v7())
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
}

permission_id!(PermissionRequestId);
permission_id!(PermissionDecisionId);
permission_id!(PermissionGrantId);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceProvenance {
    pub cwd: Option<std::path::PathBuf>,
    pub path: Option<std::path::PathBuf>,
    pub path_origin: Option<PathOrigin>,
    pub workspace_id: Option<String>,
    pub workspace_root: Option<std::path::PathBuf>,
    pub repository_root: Option<std::path::PathBuf>,
    pub network: bool,
    pub risks: BTreeSet<RiskKind>,
    extra_targets: Vec<std::path::PathBuf>,
}

impl ResourceProvenance {
    pub fn none() -> Self {
        Self::default()
    }

    pub fn for_ctx(ctx: &crate::tool::ToolCtx) -> Self {
        Self {
            workspace_id: ctx.workspace.as_ref().map(|w| w.workspace_id.clone()),
            workspace_root: ctx.workspace.as_ref().map(|w| w.path.clone()),
            repository_root: ctx.workspace.as_ref().map(|w| w.repository_root.clone()),
            ..Self::default()
        }
    }

    pub fn with_cwd(
        mut self,
        ctx: &crate::tool::ToolCtx,
        explicit: Option<&std::path::Path>,
    ) -> Result<Self, crate::error::RuntimeError> {
        let resolved = ctx.resolve_cwd_with_origin(explicit)?;
        self.cwd = Some(resolved.path);
        self.path_origin = Some(resolved.origin);
        Ok(self)
    }

    pub fn with_path(
        mut self,
        ctx: &crate::tool::ToolCtx,
        path: &std::path::Path,
    ) -> Result<Self, crate::error::RuntimeError> {
        let resolved = ctx.resolve_path_with_origin(path)?;
        self.path = Some(resolved.path);
        self.path_origin = Some(resolved.origin);
        Ok(self)
    }

    pub fn with_network(mut self) -> Self {
        self.network = true;
        self
    }

    pub fn with_risk(mut self, risk: RiskKind) -> Self {
        self.risks.insert(risk);
        self
    }

    pub fn with_extra_target(
        mut self,
        ctx: &crate::tool::ToolCtx,
        path: &std::path::Path,
    ) -> Result<Self, crate::error::RuntimeError> {
        let resolved = ctx.resolve_path_with_origin(path)?;
        // Any external target makes the whole invocation external.
        if matches!(resolved.origin, PathOrigin::ExplicitExternal) {
            self.path_origin = Some(resolved.origin);
        }
        self.extra_targets.push(resolved.path);
        Ok(self)
    }

    /// Every resource this provenance authorizes, primary and extra alike. The
    /// permit check compares against this set, so a target the tool never
    /// declared can never be written under another target's approval.
    pub fn authorized_targets(&self) -> impl Iterator<Item = &std::path::Path> {
        self.path
            .as_deref()
            .into_iter()
            .chain(self.cwd.as_deref())
            .chain(self.extra_targets.iter().map(|p| p.as_path()))
    }

    pub fn is_external(&self) -> bool {
        matches!(self.path_origin, Some(PathOrigin::ExplicitExternal))
    }

    pub fn is_unbound(&self) -> bool {
        matches!(self.path_origin, Some(PathOrigin::Unbound))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionIntent {
    pub tool_use_id: String,
    pub tool_name: String,
    pub tier: Tier,
    pub risks: BTreeSet<RiskKind>,
    pub args_digest: String,
    pub preview: Option<String>,
    pub provenance: ResourceProvenance,
}

impl PermissionIntent {
    pub fn minimal(
        tool_use_id: impl Into<String>,
        tool_name: impl Into<String>,
        tier: Tier,
    ) -> Self {
        Self {
            tool_use_id: tool_use_id.into(),
            tool_name: tool_name.into(),
            tier,
            risks: BTreeSet::new(),
            args_digest: String::new(),
            preview: None,
            provenance: ResourceProvenance::none(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorityRequirement {
    pub tier: Tier,
    pub risks: BTreeSet<RiskKind>,
    pub shell: bool,
}

impl AuthorityRequirement {
    pub fn from_intent(intent: &PermissionIntent, shell: bool) -> Self {
        Self {
            tier: intent.tier,
            risks: intent.risks.clone(),
            shell,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalTarget {
    Flow(FlowRunId),
    User,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionRequestState {
    Evaluating,
    Pending { target: ApprovalTarget },
    Approved { decision_id: PermissionDecisionId },
    Denied { decision_id: PermissionDecisionId },
    Cancelled { reason: String },
}

impl PermissionRequestState {
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Approved { .. } | Self::Denied { .. } | Self::Cancelled { .. }
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionActor {
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionAction {
    Approve,
    Deny,
    Defer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GrantScope {
    CurrentCall,
    ChildRunSameTool {
        run_id: FlowRunId,
        tool_name: String,
    },
    SamePathRuleUnsupported,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscalationHop {
    pub target: ApprovalTarget,
    pub actor: Option<DecisionActor>,
    pub action: Option<PermissionAction>,
    pub reason: Option<String>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRequest {
    pub request_id: PermissionRequestId,
    pub session_id: String,
    pub requesting_run_id: FlowRunId,
    pub parent_run_id: Option<FlowRunId>,
    pub root_run_id: FlowRunId,
    pub intent: PermissionIntent,
    pub requirement: AuthorityRequirement,
    pub original_request_id: Option<PermissionRequestId>,
    pub state: PermissionRequestState,
    pub escalation_path: Vec<EscalationHop>,
    pub requested_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionDecision {
    pub decision_id: PermissionDecisionId,
    pub request_id: PermissionRequestId,
    pub actor: DecisionActor,
    pub action: PermissionAction,
    pub grant_scope: Option<GrantScope>,
    pub reason: Option<String>,
    pub decided_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionGrant {
    pub grant_id: PermissionGrantId,
    pub request_id: PermissionRequestId,
    pub session_id: String,
    pub requesting_run_id: FlowRunId,
    pub requirement: AuthorityRequirement,
    pub scope: GrantScope,
    pub actor: DecisionActor,
    pub granted_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImmediateAuthorization {
    Unrestricted,
    Auto,
    Denied { reason: String },
    Granted { grant: PermissionGrant },
}

#[derive(Debug)]
pub struct ImmediateSubmission {
    pub request: PermissionRequest,
    pub authorization: ImmediateAuthorization,
}

#[derive(Debug)]
pub enum SubmissionOutcome {
    Immediate(Box<ImmediateSubmission>),
    Pending(Box<PendingPermission>),
}

/// Unforgeable authorization for one invocation and its exact resources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvocationAuthorization {
    request_id: PermissionRequestId,
    tool_use_id: String,
    tool_name: String,
    provenance: ResourceProvenance,
    /// True when a human or an ancestor answered, false when policy or an
    /// existing grant authorized it without asking.
    manual: bool,
}

impl InvocationAuthorization {
    pub(crate) fn new(
        request_id: PermissionRequestId,
        tool_use_id: impl Into<String>,
        tool_name: impl Into<String>,
        provenance: ResourceProvenance,
        manual: bool,
    ) -> Self {
        Self {
            request_id,
            tool_use_id: tool_use_id.into(),
            tool_name: tool_name.into(),
            provenance,
            manual,
        }
    }

    pub fn request_id(&self) -> &PermissionRequestId {
        &self.request_id
    }

    pub fn tool_name(&self) -> &str {
        &self.tool_name
    }

    pub fn tool_use_id(&self) -> &str {
        &self.tool_use_id
    }

    pub fn was_manual(&self) -> bool {
        self.manual
    }

    // Prefix matching would turn a directory permit into blanket subtree access.
    pub fn covers(&self, operation: &str, target: &std::path::Path) -> bool {
        if self.tool_name != operation {
            return false;
        }
        let canonical = crate::fs_access::canonicalize_stable(target);
        self.provenance
            .authorized_targets()
            .any(|authorized| canonical == authorized)
    }

    pub fn is_for_call(&self, tool_use_id: &str, tool_name: &str) -> bool {
        self.tool_use_id == tool_use_id && self.tool_name == tool_name
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionResolution {
    Decision(PermissionDecision),
    Cancelled { reason: String },
}

#[derive(Debug)]
pub struct PendingPermission {
    pub request: PermissionRequest,
    pub resolution: oneshot::Receiver<PermissionResolution>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveOutcome {
    Resolved(PermissionDecision),
    Deferred(PermissionDecision),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionError {
    MissingIdentity,
    IdentityMismatch,
    RequestNotFound,
    AlreadyResolved,
    ActorNotAuthorized,
    ActorNotRunning,
    PermissionManagementRequired,
    GrantExceedsAuthority,
    UnsupportedGrantScope,
    NoEscalationTarget,
}

impl std::fmt::Display for PermissionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::MissingIdentity => "permission identity is missing",
            Self::IdentityMismatch => "permission identity does not match the authority graph",
            Self::RequestNotFound => "permission request was not found",
            Self::AlreadyResolved => "permission request is already resolved",
            Self::ActorNotAuthorized => "decision actor is not authorized for this request",
            Self::ActorNotRunning => "decision actor is not running",
            Self::PermissionManagementRequired => {
                "decision actor lacks permission-management authority"
            }
            Self::GrantExceedsAuthority => "requested grant exceeds actor authority",
            Self::UnsupportedGrantScope => "path-rule grants require structured provenance",
            Self::NoEscalationTarget => "permission request has no remaining escalation target",
        };
        f.write_str(message)
    }
}

impl std::error::Error for PermissionError {}

struct RequestEntry {
    request: PermissionRequest,
    responder: Option<oneshot::Sender<PermissionResolution>>,
}

struct SubmissionContext {
    target_authority: ApprovalAuthority,
    original_request_id: Option<PermissionRequestId>,
}

#[derive(Default)]
struct BrokerState {
    requests: HashMap<PermissionRequestId, RequestEntry>,
    grants: Vec<PermissionGrant>,
}

#[derive(Debug, Clone)]
pub struct FlowDecisionAuthority {
    identity: Arc<FlowIdentity>,
}

#[derive(Debug, Clone)]
pub struct UserDecisionAuthority {
    broker_id: Uuid,
    session_id: String,
    principal_id: Option<String>,
}

#[derive(Debug, Clone)]
pub enum DecisionAuthority {
    Flow(FlowDecisionAuthority),
    User(UserDecisionAuthority),
}

#[derive(Debug, Clone)]
pub enum ApprovalAuthority {
    Flow(FlowDecisionAuthority),
    User(UserDecisionAuthority),
}

pub struct PermissionBroker {
    broker_id: Uuid,
    flows: Arc<FlowRegistry>,
    state: Mutex<BrokerState>,
    /// Keeps the registry-observed cleanup hook alive for exactly this broker's lifetime.
    observer: Mutex<Option<Arc<dyn crate::tools::agent_ctrl::FlowTerminalObserver>>>,
}

/// Bridges registry terminal transitions into the broker without keeping the broker
/// alive: the registry stores only a `Weak`, so a dropped broker deregisters itself.
struct TerminalCleanup {
    broker: std::sync::Weak<PermissionBroker>,
}

impl crate::tools::agent_ctrl::FlowTerminalObserver for TerminalCleanup {
    fn flow_became_terminal(&self, session_id: &str, run_id: &FlowRunId) {
        if let Some(broker) = self.broker.upgrade() {
            broker.cancel_for_run_locked(session_id, run_id, "requesting flow is terminal");
        }
    }
}

impl PermissionBroker {
    /// Creates a broker without terminal-driven cleanup. Deliberately private: only
    /// [`PermissionBroker::shared`] returns a broker in production, because a
    /// non-observing broker never wakes pending awaiters on terminal transitions
    /// unless some caller drives an `expire_terminal` sweep.
    fn new(flows: Arc<FlowRegistry>) -> Self {
        Self {
            broker_id: Uuid::now_v7(),
            flows,
            state: Mutex::new(BrokerState::default()),
            observer: Mutex::new(None),
        }
    }

    /// Wires terminal-driven cleanup. Only shared brokers can observe the registry,
    /// because the observer must not keep the broker alive.
    pub fn shared(flows: Arc<FlowRegistry>) -> Arc<Self> {
        let broker = Arc::new(Self::new(Arc::clone(&flows)));
        let observer: Arc<dyn crate::tools::agent_ctrl::FlowTerminalObserver> =
            Arc::new(TerminalCleanup {
                broker: Arc::downgrade(&broker),
            });
        // The registry holds a Weak to the trait object, so the Arc must live in the
        // broker itself to stay upgradable for the broker's lifetime.
        flows.register_terminal_observer(Arc::downgrade(&observer));
        broker.observer.lock().unwrap().replace(observer);
        broker
    }

    /// True when this broker arbitrates `flows`. Callers that already hold a
    /// broker must confirm it is bound to the registry they are about to submit
    /// against, otherwise identity authentication would run against a different
    /// authority graph.
    pub fn is_for_registry(&self, flows: &Arc<FlowRegistry>) -> bool {
        Arc::ptr_eq(&self.flows, flows)
    }

    pub fn flow_authority(
        &self,
        identity: Arc<FlowIdentity>,
    ) -> Result<FlowDecisionAuthority, PermissionError> {
        self.authenticate_registered_identity(&identity)?;
        Ok(FlowDecisionAuthority { identity })
    }

    pub(crate) fn user_authority(
        &self,
        session_id: impl Into<String>,
        principal_id: Option<String>,
    ) -> UserDecisionAuthority {
        UserDecisionAuthority {
            broker_id: self.broker_id,
            session_id: session_id.into(),
            principal_id,
        }
    }

    pub fn submit(
        &self,
        session_id: Option<&str>,
        run_id: Option<&FlowRunId>,
        intent: PermissionIntent,
        shell: bool,
        policy: &TrustConfig,
    ) -> Result<SubmissionOutcome, PermissionError> {
        self.submit_with_original_request_id(session_id, run_id, intent, shell, None, policy)
    }

    pub(crate) fn submit_with_original_request_id(
        &self,
        session_id: Option<&str>,
        run_id: Option<&FlowRunId>,
        intent: PermissionIntent,
        shell: bool,
        original_request_id: Option<PermissionRequestId>,
        policy: &TrustConfig,
    ) -> Result<SubmissionOutcome, PermissionError> {
        let session_id = session_id.ok_or(PermissionError::MissingIdentity)?;
        let target = ApprovalAuthority::User(self.user_authority(session_id, None));
        self.submit_to_with_context(
            Some(session_id),
            run_id,
            intent,
            shell,
            SubmissionContext {
                target_authority: target,
                original_request_id,
            },
            policy,
        )
    }

    pub fn submit_to(
        &self,
        session_id: Option<&str>,
        run_id: Option<&FlowRunId>,
        intent: PermissionIntent,
        shell: bool,
        target_authority: ApprovalAuthority,
        policy: &TrustConfig,
    ) -> Result<SubmissionOutcome, PermissionError> {
        self.submit_to_with_context(
            session_id,
            run_id,
            intent,
            shell,
            SubmissionContext {
                target_authority,
                original_request_id: None,
            },
            policy,
        )
    }

    fn submit_to_with_context(
        &self,
        session_id: Option<&str>,
        run_id: Option<&FlowRunId>,
        intent: PermissionIntent,
        shell: bool,
        context: SubmissionContext,
        policy: &TrustConfig,
    ) -> Result<SubmissionOutcome, PermissionError> {
        // Liveness checks and the pending insert must observe the same lifecycle
        // snapshot, otherwise a run could go terminal between them and leave an
        // orphan pending request that terminal cleanup already walked past.
        self.flows.with_lifecycle_arbitration(|| {
            self.submit_to_locked(session_id, run_id, intent, shell, context, policy)
        })
    }

    fn submit_to_locked(
        &self,
        session_id: Option<&str>,
        run_id: Option<&FlowRunId>,
        intent: PermissionIntent,
        shell: bool,
        context: SubmissionContext,
        policy: &TrustConfig,
    ) -> Result<SubmissionOutcome, PermissionError> {
        let requirement = AuthorityRequirement::from_intent(&intent, shell);
        let identity = self.authenticate_requester(session_id, run_id, &requirement)?;
        let target = self.authenticate_target(&identity, context.target_authority)?;
        let (execution_policy, action) = identity.effective_authority.constrain_policy(
            policy,
            intent.tier,
            intent.risks.iter().copied(),
        );
        let mut state = self.state.lock().unwrap();
        self.remove_terminal_grants(&mut state);
        if let Some(original_request_id) = context.original_request_id.as_ref() {
            let original = state
                .requests
                .get(original_request_id)
                .ok_or(PermissionError::RequestNotFound)?;
            if original.request.session_id != identity.session_id
                || original.request.requesting_run_id != identity.run_id
                || original.request.intent.tool_use_id != intent.tool_use_id
                || original.request.intent.tool_name != intent.tool_name
            {
                return Err(PermissionError::IdentityMismatch);
            }
        }
        let request_id = PermissionRequestId::now();
        let now = Utc::now();
        let immediate = if execution_policy == ExecutionPolicy::Unrestricted {
            Some(ImmediateAuthorization::Unrestricted)
        } else {
            match action {
                PolicyAction::Auto => Some(ImmediateAuthorization::Auto),
                PolicyAction::Deny => Some(ImmediateAuthorization::Denied {
                    reason: "permission policy denied the invocation".into(),
                }),
                PolicyAction::Ask => {
                    matching_grant(&state.grants, &identity, &intent, &requirement)
                        .map(|grant| ImmediateAuthorization::Granted { grant })
                }
            }
        };
        let request_state = match &immediate {
            Some(ImmediateAuthorization::Denied { .. }) => PermissionRequestState::Denied {
                decision_id: PermissionDecisionId::now(),
            },
            Some(_) => PermissionRequestState::Approved {
                decision_id: PermissionDecisionId::now(),
            },
            None => PermissionRequestState::Pending {
                target: target.clone(),
            },
        };
        let request = PermissionRequest {
            request_id: request_id.clone(),
            session_id: identity.session_id.clone(),
            requesting_run_id: identity.run_id.clone(),
            parent_run_id: identity.parent_run_id.clone(),
            root_run_id: identity.root_run_id.clone(),
            intent,
            requirement,
            original_request_id: context.original_request_id,
            state: request_state,
            escalation_path: vec![EscalationHop {
                target,
                actor: None,
                action: None,
                reason: None,
                at: now,
            }],
            requested_at: now,
        };
        if let Some(authorization) = immediate {
            state.requests.insert(
                request_id,
                RequestEntry {
                    request: request.clone(),
                    responder: None,
                },
            );
            return Ok(SubmissionOutcome::Immediate(Box::new(
                ImmediateSubmission {
                    request,
                    authorization,
                },
            )));
        }

        let (responder, resolution) = oneshot::channel();
        state.requests.insert(
            request_id,
            RequestEntry {
                request: request.clone(),
                responder: Some(responder),
            },
        );
        Ok(SubmissionOutcome::Pending(Box::new(PendingPermission {
            request,
            resolution,
        })))
    }

    pub fn resolve(
        &self,
        request_id: &PermissionRequestId,
        authority: &DecisionAuthority,
        action: PermissionAction,
        grant_scope: Option<GrantScope>,
        reason: Option<String>,
    ) -> Result<ResolveOutcome, PermissionError> {
        // Holding the lifecycle arbitration across the liveness check and the decision
        // commit makes terminal-vs-approve a single linearization point: either this
        // decision commits before the run is terminal, or terminal cleanup runs first
        // and this call observes a cancelled request.
        self.flows.with_lifecycle_arbitration(|| {
            self.resolve_locked(request_id, authority, action, grant_scope, reason)
        })
    }

    fn resolve_locked(
        &self,
        request_id: &PermissionRequestId,
        authority: &DecisionAuthority,
        action: PermissionAction,
        grant_scope: Option<GrantScope>,
        reason: Option<String>,
    ) -> Result<ResolveOutcome, PermissionError> {
        let mut state = self.state.lock().unwrap();
        let request = state
            .requests
            .get(request_id)
            .ok_or(PermissionError::RequestNotFound)?
            .request
            .clone();
        if request.state.is_terminal() {
            return Err(PermissionError::AlreadyResolved);
        }
        if matches!(
            self.flows.execution_state(&request.requesting_run_id),
            None | Some(FlowExecutionState::Terminal)
        ) {
            let reason = "requesting flow is terminal".to_owned();
            cancel_entry(state.requests.get_mut(request_id).unwrap(), reason);
            state.grants.retain(|grant| {
                grant.session_id != request.session_id
                    || grant.requesting_run_id != request.requesting_run_id
            });
            return Err(PermissionError::ActorNotRunning);
        }

        let actor = self.validate_authority(&request, authority, action, grant_scope.as_ref())?;
        let target = match &request.state {
            PermissionRequestState::Pending { target } => target.clone(),
            PermissionRequestState::Evaluating => return Err(PermissionError::ActorNotAuthorized),
            _ => return Err(PermissionError::AlreadyResolved),
        };
        if !actor_matches_target(&actor, &target) {
            return Err(PermissionError::ActorNotAuthorized);
        }

        let decision = PermissionDecision {
            decision_id: PermissionDecisionId::now(),
            request_id: request_id.clone(),
            actor: actor.clone(),
            action,
            grant_scope: grant_scope.clone(),
            reason: reason.clone(),
            decided_at: Utc::now(),
        };
        let entry = state.requests.get_mut(request_id).unwrap();
        entry.request.escalation_path.push(EscalationHop {
            target,
            actor: Some(actor.clone()),
            action: Some(action),
            reason: reason.clone(),
            at: decision.decided_at,
        });

        if action == PermissionAction::Defer {
            if matches!(
                entry.request.state,
                PermissionRequestState::Pending {
                    target: ApprovalTarget::User
                }
            ) {
                let reason = reason.unwrap_or_else(|| "user deferred without a fallback".into());
                cancel_entry(entry, reason);
                return Ok(ResolveOutcome::Deferred(decision));
            }
            let next_target = ApprovalTarget::User;
            entry.request.state = PermissionRequestState::Pending {
                target: next_target.clone(),
            };
            entry.request.escalation_path.push(EscalationHop {
                target: next_target,
                actor: None,
                action: None,
                reason: None,
                at: Utc::now(),
            });
            return Ok(ResolveOutcome::Deferred(decision));
        }

        let persistent_grant = if action == PermissionAction::Approve {
            grant_scope
                .clone()
                .filter(|scope| !matches!(scope, GrantScope::CurrentCall))
                .map(|scope| PermissionGrant {
                    grant_id: PermissionGrantId::now(),
                    request_id: request_id.clone(),
                    session_id: request.session_id.clone(),
                    requesting_run_id: request.requesting_run_id.clone(),
                    requirement: request.requirement.clone(),
                    scope,
                    actor,
                    granted_at: decision.decided_at,
                })
        } else {
            None
        };
        let entry = state.requests.get_mut(request_id).unwrap();
        entry.request.state = if action == PermissionAction::Approve {
            PermissionRequestState::Approved {
                decision_id: decision.decision_id.clone(),
            }
        } else {
            PermissionRequestState::Denied {
                decision_id: decision.decision_id.clone(),
            }
        };
        let responder = entry.responder.take();
        if let Some(grant) = persistent_grant
            && !state
                .grants
                .iter()
                .any(|existing| equivalent_grant(existing, &grant))
        {
            state.grants.push(grant);
        }
        if let Some(responder) = responder {
            let _ = responder.send(PermissionResolution::Decision(decision.clone()));
        }
        Ok(ResolveOutcome::Resolved(decision))
    }

    pub fn cancel(
        &self,
        request_id: &PermissionRequestId,
        reason: impl Into<String>,
    ) -> Result<(), PermissionError> {
        let mut state = self.state.lock().unwrap();
        let entry = state
            .requests
            .get_mut(request_id)
            .ok_or(PermissionError::RequestNotFound)?;
        if entry.request.state.is_terminal() || entry.responder.is_none() {
            return Err(PermissionError::AlreadyResolved);
        }
        cancel_entry(entry, reason.into());
        Ok(())
    }

    pub fn cancel_for_run(&self, session_id: &str, run_id: &FlowRunId, reason: &str) -> usize {
        self.flows
            .with_lifecycle_arbitration(|| self.cancel_for_run_locked(session_id, run_id, reason))
    }

    /// Cancels a run's pending requests and revokes its grants. Callers must already
    /// hold the lifecycle arbitration; this only takes the broker state lock.
    fn cancel_for_run_locked(&self, session_id: &str, run_id: &FlowRunId, reason: &str) -> usize {
        let mut state = self.state.lock().unwrap();
        let mut cancelled = 0;
        for entry in state.requests.values_mut() {
            if entry.request.session_id == session_id
                && entry.request.requesting_run_id == *run_id
                && !entry.request.state.is_terminal()
                && entry.responder.is_some()
            {
                cancel_entry(entry, reason.to_owned());
                cancelled += 1;
            }
        }
        state
            .grants
            .retain(|grant| grant.session_id != session_id || grant.requesting_run_id != *run_id);
        cancelled
    }

    pub fn expire_terminal(&self, reason: &str) -> usize {
        self.flows
            .with_lifecycle_arbitration(|| self.expire_terminal_locked(reason))
    }

    fn expire_terminal_locked(&self, reason: &str) -> usize {
        let terminal_runs: HashSet<_> = {
            let state = self.state.lock().unwrap();
            state
                .requests
                .values()
                .filter(|entry| {
                    !entry.request.state.is_terminal()
                        && matches!(
                            self.flows.execution_state(&entry.request.requesting_run_id),
                            None | Some(FlowExecutionState::Terminal)
                        )
                })
                .map(|entry| {
                    (
                        entry.request.session_id.clone(),
                        entry.request.requesting_run_id.clone(),
                    )
                })
                .collect()
        };
        terminal_runs
            .iter()
            .map(|(session_id, run_id)| self.cancel_for_run_locked(session_id, run_id, reason))
            .sum()
    }

    pub fn expire_before(&self, deadline: DateTime<Utc>, reason: &str) -> usize {
        let mut state = self.state.lock().unwrap();
        let mut expired = 0;
        for entry in state.requests.values_mut() {
            if entry.request.requested_at <= deadline
                && !entry.request.state.is_terminal()
                && entry.responder.is_some()
            {
                cancel_entry(entry, reason.to_owned());
                expired += 1;
            }
        }
        expired
    }

    pub fn get(&self, request_id: &PermissionRequestId) -> Option<PermissionRequest> {
        self.state
            .lock()
            .unwrap()
            .requests
            .get(request_id)
            .map(|entry| entry.request.clone())
    }

    pub fn list(&self) -> Vec<PermissionRequest> {
        let mut requests: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .requests
            .values()
            .map(|entry| entry.request.clone())
            .collect();
        requests.sort_by_key(|request| request.requested_at);
        requests
    }

    pub fn grants(&self) -> Vec<PermissionGrant> {
        self.state.lock().unwrap().grants.clone()
    }

    pub fn find_matching_grant(
        &self,
        identity: &FlowIdentity,
        intent: &PermissionIntent,
        shell: bool,
    ) -> Option<PermissionGrant> {
        let requirement = AuthorityRequirement::from_intent(intent, shell);
        self.flows.with_lifecycle_arbitration(|| {
            let mut state = self.state.lock().unwrap();
            self.remove_terminal_grants(&mut state);
            matching_grant(&state.grants, identity, intent, &requirement)
        })
    }

    fn authenticate_requester(
        &self,
        session_id: Option<&str>,
        run_id: Option<&FlowRunId>,
        requirement: &AuthorityRequirement,
    ) -> Result<Arc<FlowIdentity>, PermissionError> {
        let session_id = session_id.ok_or(PermissionError::MissingIdentity)?;
        let run_id = run_id.ok_or(PermissionError::MissingIdentity)?;
        let identity = self
            .flows
            .lookup_run(run_id)
            .ok_or(PermissionError::MissingIdentity)?;
        if identity.session_id != session_id {
            return Err(PermissionError::IdentityMismatch);
        }
        self.authenticate_registered_identity(&identity)?;
        if !matches!(identity.execution_state(), FlowExecutionState::Running) {
            return Err(PermissionError::ActorNotRunning);
        }
        if !authority_contains(&identity.effective_authority, requirement) {
            return Err(PermissionError::GrantExceedsAuthority);
        }
        Ok(identity)
    }

    fn authenticate_registered_identity(
        &self,
        identity: &Arc<FlowIdentity>,
    ) -> Result<(), PermissionError> {
        let registered = self
            .flows
            .lookup_run(&identity.run_id)
            .ok_or(PermissionError::MissingIdentity)?;
        if !Arc::ptr_eq(&registered, identity) {
            return Err(PermissionError::IdentityMismatch);
        }
        Ok(())
    }

    fn authenticate_target(
        &self,
        requester: &FlowIdentity,
        authority: ApprovalAuthority,
    ) -> Result<ApprovalTarget, PermissionError> {
        match authority {
            ApprovalAuthority::Flow(authority) => {
                self.authenticate_registered_identity(&authority.identity)?;
                let target = authority.identity;
                if target.session_id != requester.session_id
                    || !self
                        .flows
                        .is_strict_ancestor(&target.run_id, &requester.run_id)
                {
                    return Err(PermissionError::ActorNotAuthorized);
                }
                if !matches!(target.execution_state(), FlowExecutionState::Running) {
                    return Err(PermissionError::ActorNotRunning);
                }
                if !target.effective_authority.permission_management {
                    return Err(PermissionError::PermissionManagementRequired);
                }
                Ok(ApprovalTarget::Flow(target.run_id.clone()))
            }
            ApprovalAuthority::User(authority) => {
                if authority.broker_id != self.broker_id
                    || authority.session_id != requester.session_id
                {
                    return Err(PermissionError::ActorNotAuthorized);
                }
                Ok(ApprovalTarget::User)
            }
        }
    }

    fn validate_authority(
        &self,
        request: &PermissionRequest,
        authority: &DecisionAuthority,
        action: PermissionAction,
        scope: Option<&GrantScope>,
    ) -> Result<DecisionActor, PermissionError> {
        if action != PermissionAction::Approve && scope.is_some() {
            return Err(PermissionError::ActorNotAuthorized);
        }
        if matches!(scope, Some(GrantScope::SamePathRuleUnsupported)) {
            return Err(PermissionError::UnsupportedGrantScope);
        }
        match authority {
            DecisionAuthority::Flow(authority) => {
                self.authenticate_registered_identity(&authority.identity)?;
                let identity = &authority.identity;
                if identity.session_id != request.session_id
                    || !self
                        .flows
                        .is_strict_ancestor(&identity.run_id, &request.requesting_run_id)
                {
                    return Err(PermissionError::ActorNotAuthorized);
                }
                if !matches!(identity.execution_state(), FlowExecutionState::Running) {
                    return Err(PermissionError::ActorNotRunning);
                }
                if !identity.effective_authority.permission_management {
                    return Err(PermissionError::PermissionManagementRequired);
                }
                if !authority_contains(&identity.effective_authority, &request.requirement) {
                    return Err(PermissionError::GrantExceedsAuthority);
                }
                validate_same_tool_scope(request, scope)?;
                Ok(DecisionActor::Flow {
                    session_id: identity.session_id.clone(),
                    run_id: identity.run_id.clone(),
                })
            }
            DecisionAuthority::User(authority) => {
                if authority.broker_id != self.broker_id
                    || authority.session_id != request.session_id
                {
                    return Err(PermissionError::ActorNotAuthorized);
                }
                validate_same_tool_scope(request, scope)?;
                Ok(DecisionActor::User {
                    session_id: authority.session_id.clone(),
                    principal_id: authority.principal_id.clone(),
                })
            }
        }
    }

    fn remove_terminal_grants(&self, state: &mut BrokerState) {
        state.grants.retain(|grant| {
            !matches!(
                self.flows.execution_state(&grant.requesting_run_id),
                None | Some(FlowExecutionState::Terminal)
            )
        });
    }
}

fn cancel_entry(entry: &mut RequestEntry, reason: String) {
    entry.request.state = PermissionRequestState::Cancelled {
        reason: reason.clone(),
    };
    if let Some(responder) = entry.responder.take() {
        let _ = responder.send(PermissionResolution::Cancelled { reason });
    }
}

fn validate_same_tool_scope(
    request: &PermissionRequest,
    scope: Option<&GrantScope>,
) -> Result<(), PermissionError> {
    if let Some(GrantScope::ChildRunSameTool { run_id, tool_name }) = scope
        && (run_id != &request.requesting_run_id || tool_name != &request.intent.tool_name)
    {
        return Err(PermissionError::GrantExceedsAuthority);
    }
    Ok(())
}

fn matching_grant(
    grants: &[PermissionGrant],
    identity: &FlowIdentity,
    intent: &PermissionIntent,
    requirement: &AuthorityRequirement,
) -> Option<PermissionGrant> {
    grants
        .iter()
        .find(|grant| {
            grant.session_id == identity.session_id
                && grant.requesting_run_id == identity.run_id
                && requirement_contains(&grant.requirement, requirement)
                && matches!(
                    &grant.scope,
                    GrantScope::ChildRunSameTool { run_id, tool_name }
                        if run_id == &identity.run_id && tool_name == &intent.tool_name
                )
        })
        .cloned()
}

fn equivalent_grant(left: &PermissionGrant, right: &PermissionGrant) -> bool {
    left.session_id == right.session_id
        && left.requesting_run_id == right.requesting_run_id
        && left.requirement == right.requirement
        && left.scope == right.scope
}

fn requirement_contains(grant: &AuthorityRequirement, requested: &AuthorityRequirement) -> bool {
    grant.tier == requested.tier
        && requested.risks.is_subset(&grant.risks)
        && (!requested.shell || grant.shell)
}

fn actor_matches_target(actor: &DecisionActor, target: &ApprovalTarget) -> bool {
    matches!(
        (actor, target),
        (DecisionActor::Flow { run_id, .. }, ApprovalTarget::Flow(target_run)) if run_id == target_run
    ) || matches!(
        (actor, target),
        (DecisionActor::User { .. }, ApprovalTarget::User)
    )
}

fn authority_contains(
    authority: &crate::flow_authority::EffectiveAuthority,
    requirement: &AuthorityRequirement,
) -> bool {
    let tier_index = match requirement.tier {
        Tier::Zero => 0,
        Tier::One => 1,
        Tier::Two => 2,
        Tier::Three => 3,
        Tier::Four => 4,
    };
    authority.allowed_tiers[tier_index]
        && requirement.risks.is_subset(&authority.allowed_risks)
        && (!requirement.shell || authority.shell)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_authority::{ChildWorkspaceAuthority, EffectiveAuthority, InvocationKind};
    use crate::trust::{EscalationPolicy, TrustMode};

    fn authority(permission_management: bool) -> EffectiveAuthority {
        EffectiveAuthority {
            execution_policy: ExecutionPolicy::Controlled,
            allowed_tiers: [true; 5],
            allowed_risks: BTreeSet::from([
                RiskKind::WorkspaceExternal,
                RiskKind::SandboxViolation,
                RiskKind::Network,
                RiskKind::Irreversible,
                RiskKind::FilesystemWrite,
                RiskKind::ProcessSpawn,
                RiskKind::RepositoryMutation,
            ]),
            tier_ceiling: [PolicyAction::Auto; 5],
            risk_ceiling: [PolicyAction::Auto; 7],
            shell: true,
            permission_management,
            workspace_root: None,
        }
    }

    fn register_root(
        flows: &Arc<FlowRegistry>,
        session_id: &str,
        permission_management: bool,
    ) -> Arc<FlowIdentity> {
        flows
            .register_root(
                session_id.into(),
                FlowRunId::now(),
                authority(permission_management),
            )
            .unwrap()
    }

    fn child(flows: &Arc<FlowRegistry>, parent: &FlowIdentity) -> Arc<FlowIdentity> {
        flows
            .register_child(
                &parent.run_id,
                FlowRunId::now(),
                InvocationKind::InlineSubflow,
                true,
                ChildWorkspaceAuthority::Inherit,
            )
            .unwrap()
    }

    fn ask_policy() -> TrustConfig {
        TrustConfig {
            mode: TrustMode::Steady,
            ..TrustConfig::default()
        }
    }

    fn intent() -> PermissionIntent {
        PermissionIntent {
            tool_use_id: "call-1".into(),
            tool_name: "bash.spawn".into(),
            tier: Tier::Two,
            risks: BTreeSet::new(),
            args_digest: "sha256:test".into(),
            preview: None,
            provenance: ResourceProvenance::none(),
        }
    }

    fn submit_to_flow(
        broker: &PermissionBroker,
        requester: &FlowIdentity,
        target: Arc<FlowIdentity>,
    ) -> Result<SubmissionOutcome, PermissionError> {
        let target = ApprovalAuthority::Flow(broker.flow_authority(target)?);
        broker.submit_to(
            Some(&requester.session_id),
            Some(&requester.run_id),
            intent(),
            false,
            target,
            &ask_policy(),
        )
    }

    fn submit_user(
        broker: &PermissionBroker,
        requester: &FlowIdentity,
        intent: PermissionIntent,
    ) -> PendingPermission {
        let SubmissionOutcome::Pending(pending) = broker
            .submit(
                Some(&requester.session_id),
                Some(&requester.run_id),
                intent,
                false,
                &ask_policy(),
            )
            .unwrap()
        else {
            panic!("expected pending request");
        };
        *pending
    }

    fn user_decision(broker: &PermissionBroker, session_id: &str) -> DecisionAuthority {
        DecisionAuthority::User(broker.user_authority(session_id, None))
    }

    fn assert_immediate_is_auditable(broker: &PermissionBroker, submission: &ImmediateSubmission) {
        assert_eq!(
            broker.get(&submission.request.request_id),
            Some(submission.request.clone())
        );
        assert!(!matches!(
            submission.request.state,
            PermissionRequestState::Pending { .. }
        ));
    }

    #[test]
    fn sandbox_lineage_rejects_cross_run_and_cross_session_requests() {
        let flows = Arc::new(FlowRegistry::default());
        let broker = PermissionBroker::new(Arc::clone(&flows));
        let original_requester = register_root(&flows, "session", true);
        let original = submit_user(&broker, &original_requester, intent());
        let original_request_id = original.request.request_id.clone();
        let same_session_other_run = child(&flows, &original_requester);
        let other_session = register_root(&flows, "other", true);

        for requester in [same_session_other_run, other_session] {
            let before = broker.list().len();
            assert!(matches!(
                broker.submit_with_original_request_id(
                    Some(&requester.session_id),
                    Some(&requester.run_id),
                    intent(),
                    false,
                    Some(original_request_id.clone()),
                    &ask_policy(),
                ),
                Err(PermissionError::IdentityMismatch)
            ));
            assert_eq!(broker.list().len(), before);
        }
    }

    #[test]
    fn sandbox_lineage_rejects_another_tool_invocation() {
        let flows = Arc::new(FlowRegistry::default());
        let broker = PermissionBroker::new(Arc::clone(&flows));
        let requester = register_root(&flows, "session", true);
        let original = submit_user(&broker, &requester, intent());
        let mut unrelated = intent();
        unrelated.tool_name = "term.spawn".into();
        let before = broker.list().len();

        assert!(matches!(
            broker.submit_with_original_request_id(
                Some(&requester.session_id),
                Some(&requester.run_id),
                unrelated,
                false,
                Some(original.request.request_id.clone()),
                &ask_policy(),
            ),
            Err(PermissionError::IdentityMismatch)
        ));
        assert_eq!(broker.list().len(), before);
    }

    #[test]
    fn invalid_flow_targets_do_not_create_requests() {
        let cases = [
            "self",
            "sibling",
            "cross-session",
            "terminal",
            "no-management",
        ];
        for case in cases {
            let flows = Arc::new(FlowRegistry::default());
            let root = register_root(&flows, "session", case != "no-management");
            let requester = child(&flows, &root);
            let target = match case {
                "self" => Arc::clone(&requester),
                "sibling" => child(&flows, &root),
                "cross-session" => register_root(&flows, "other", true),
                "terminal" => {
                    flows.mark_terminal(&root.run_id);
                    Arc::clone(&root)
                }
                "no-management" => Arc::clone(&root),
                _ => unreachable!(),
            };
            let broker = PermissionBroker::new(flows);

            assert!(
                submit_to_flow(&broker, &requester, target).is_err(),
                "{case}"
            );
            assert!(broker.list().is_empty(), "{case} allocated a request");
        }
    }

    #[test]
    fn running_managing_strict_ancestor_can_be_initial_target() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let requester = child(&flows, &root);
        let broker = PermissionBroker::new(flows);

        let SubmissionOutcome::Pending(pending) =
            submit_to_flow(&broker, &requester, root).unwrap()
        else {
            panic!("expected pending request");
        };
        assert!(matches!(
            pending.request.state,
            PermissionRequestState::Pending {
                target: ApprovalTarget::Flow(_)
            }
        ));
    }

    #[test]
    fn unrestricted_still_requires_authenticated_identity() {
        let broker = PermissionBroker::new(Arc::new(FlowRegistry::default()));
        let policy = TrustConfig {
            mode: TrustMode::Reckless,
            ..TrustConfig::default()
        };

        assert!(matches!(
            broker.submit(None, None, intent(), false, &policy),
            Err(PermissionError::MissingIdentity)
        ));
    }

    #[test]
    fn deny_is_immediate_and_persists_terminal_request() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(flows);
        let policy = TrustConfig {
            mode: TrustMode::Eager,
            escalation: EscalationPolicy::Deny,
            ..TrustConfig::default()
        };

        let outcome = broker
            .submit(
                Some(&requester.session_id),
                Some(&requester.run_id),
                intent(),
                false,
                &policy,
            )
            .unwrap();

        let SubmissionOutcome::Immediate(submission) = outcome else {
            panic!("expected immediate denial");
        };
        assert!(matches!(
            submission.authorization,
            ImmediateAuthorization::Denied { .. }
        ));
        assert_immediate_is_auditable(&broker, &submission);
        assert!(matches!(
            submission.request.state,
            PermissionRequestState::Denied { .. }
        ));
    }

    #[test]
    fn submit_uses_each_policy_snapshot_without_cross_request_state() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(Arc::clone(&flows));
        let auto = TrustConfig {
            mode: TrustMode::Eager,
            escalation: EscalationPolicy::Allow,
            ..TrustConfig::default()
        };
        let ask = ask_policy();
        let deny = TrustConfig {
            mode: TrustMode::Eager,
            escalation: EscalationPolicy::Deny,
            ..TrustConfig::default()
        };
        let unrestricted = TrustConfig {
            mode: TrustMode::Reckless,
            ..TrustConfig::default()
        };
        let unrestricted_requester = flows
            .register_root(
                "session".into(),
                FlowRunId::now(),
                EffectiveAuthority::root(&unrestricted, true, None),
            )
            .unwrap();

        let SubmissionOutcome::Immediate(auto_submission) = broker
            .submit(
                Some(&requester.session_id),
                Some(&requester.run_id),
                intent(),
                false,
                &auto,
            )
            .unwrap()
        else {
            panic!("expected immediate auto authorization");
        };
        assert!(matches!(
            auto_submission.authorization,
            ImmediateAuthorization::Auto
        ));
        assert_immediate_is_auditable(&broker, &auto_submission);

        let SubmissionOutcome::Pending(pending) = broker
            .submit(
                Some(&requester.session_id),
                Some(&requester.run_id),
                intent(),
                false,
                &ask,
            )
            .unwrap()
        else {
            panic!("expected pending request");
        };

        let SubmissionOutcome::Immediate(denied_submission) = broker
            .submit(
                Some(&requester.session_id),
                Some(&requester.run_id),
                intent(),
                false,
                &deny,
            )
            .unwrap()
        else {
            panic!("expected immediate denial");
        };
        assert!(matches!(
            denied_submission.authorization,
            ImmediateAuthorization::Denied { .. }
        ));
        assert_immediate_is_auditable(&broker, &denied_submission);

        let SubmissionOutcome::Immediate(unrestricted_submission) = broker
            .submit(
                Some(&unrestricted_requester.session_id),
                Some(&unrestricted_requester.run_id),
                intent(),
                false,
                &unrestricted,
            )
            .unwrap()
        else {
            panic!("expected immediate unrestricted authorization");
        };
        assert!(matches!(
            unrestricted_submission.authorization,
            ImmediateAuthorization::Unrestricted
        ));
        assert_immediate_is_auditable(&broker, &unrestricted_submission);
        assert_eq!(broker.list().len(), 4);
        assert_eq!(
            broker
                .list()
                .into_iter()
                .filter(|request| matches!(request.state, PermissionRequestState::Pending { .. }))
                .count(),
            1
        );

        broker
            .cancel(&pending.request.request_id, "test cleanup")
            .unwrap();
        assert!(matches!(
            broker.get(&pending.request.request_id).unwrap().state,
            PermissionRequestState::Cancelled { .. }
        ));
        assert_eq!(broker.list().len(), 4);
    }

    #[test]
    fn broker_bound_user_authority_cannot_be_reused() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let first = PermissionBroker::new(Arc::clone(&flows));
        let second = PermissionBroker::new(flows);
        let pending = submit_user(&second, &requester, intent());
        let foreign = user_decision(&first, "session");

        assert!(matches!(
            second.resolve(
                &pending.request.request_id,
                &foreign,
                PermissionAction::Approve,
                None,
                None,
            ),
            Err(PermissionError::ActorNotAuthorized)
        ));
    }

    #[test]
    fn cancellation_wakes_waiter_with_fail_closed_resolution() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(flows);
        let pending = submit_user(&broker, &requester, intent());

        broker
            .cancel(&pending.request.request_id, "flow stopped")
            .unwrap();

        assert_eq!(
            pending.resolution.blocking_recv().unwrap(),
            PermissionResolution::Cancelled {
                reason: "flow stopped".into()
            }
        );
    }

    #[test]
    fn terminal_run_cleanup_cancels_pending_and_removes_grants() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(Arc::clone(&flows));
        let first = submit_user(&broker, &requester, intent());
        let authority = user_decision(&broker, "session");
        let scope = GrantScope::ChildRunSameTool {
            run_id: requester.run_id.clone(),
            tool_name: "bash.spawn".into(),
        };
        broker
            .resolve(
                &first.request.request_id,
                &authority,
                PermissionAction::Approve,
                Some(scope),
                None,
            )
            .unwrap();
        let pending = submit_user(
            &broker,
            &requester,
            PermissionIntent {
                tool_name: "fs.write".into(),
                ..intent()
            },
        );

        flows.mark_terminal(&requester.run_id);
        assert_eq!(broker.expire_terminal("flow stopped"), 1);
        assert!(broker.grants().is_empty());
        assert!(matches!(
            pending.resolution.blocking_recv().unwrap(),
            PermissionResolution::Cancelled { .. }
        ));
    }

    #[test]
    fn same_tool_grant_covers_narrower_risk_but_not_escalation() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(flows);
        let mut approved = intent();
        approved.risks.insert(RiskKind::Network);
        let pending = submit_user(&broker, &requester, approved);
        let scope = GrantScope::ChildRunSameTool {
            run_id: requester.run_id.clone(),
            tool_name: "bash.spawn".into(),
        };
        broker
            .resolve(
                &pending.request.request_id,
                &user_decision(&broker, "session"),
                PermissionAction::Approve,
                Some(scope),
                None,
            )
            .unwrap();

        let SubmissionOutcome::Immediate(granted_submission) = broker
            .submit(
                Some(&requester.session_id),
                Some(&requester.run_id),
                intent(),
                false,
                &ask_policy(),
            )
            .unwrap()
        else {
            panic!("expected immediate grant authorization");
        };
        assert!(matches!(
            granted_submission.authorization,
            ImmediateAuthorization::Granted { .. }
        ));
        assert_immediate_is_auditable(&broker, &granted_submission);
        let mut escalated = intent();
        escalated.risks.insert(RiskKind::Network);
        escalated.risks.insert(RiskKind::Irreversible);
        assert!(matches!(
            broker.submit(
                Some(&requester.session_id),
                Some(&requester.run_id),
                escalated,
                false,
                &ask_policy(),
            ),
            Ok(SubmissionOutcome::Pending(_))
        ));
    }

    #[test]
    fn current_call_is_not_persisted_and_equivalent_grants_are_deduplicated() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(flows);
        let authority = user_decision(&broker, "session");

        let current = submit_user(&broker, &requester, intent());
        broker
            .resolve(
                &current.request.request_id,
                &authority,
                PermissionAction::Approve,
                None,
                None,
            )
            .unwrap();
        assert!(broker.grants().is_empty());

        let pending = submit_user(
            &broker,
            &requester,
            PermissionIntent {
                tool_use_id: "call-2".into(),
                ..intent()
            },
        );
        broker
            .resolve(
                &pending.request.request_id,
                &authority,
                PermissionAction::Approve,
                Some(GrantScope::ChildRunSameTool {
                    run_id: requester.run_id.clone(),
                    tool_name: "bash.spawn".into(),
                }),
                None,
            )
            .unwrap();
        assert!(matches!(
            broker.submit(
                Some(&requester.session_id),
                Some(&requester.run_id),
                PermissionIntent {
                    tool_use_id: "call-3".into(),
                    ..intent()
                },
                false,
                &ask_policy(),
            ),
            Ok(SubmissionOutcome::Immediate(value)) if matches!(value.authorization, ImmediateAuthorization::Granted { .. })
        ));
        assert_eq!(broker.grants().len(), 1);
    }

    #[test]
    fn terminal_transition_cancels_pending_without_explicit_sweep() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::shared(Arc::clone(&flows));
        let first = submit_user(&broker, &requester, intent());
        let second = submit_user(
            &broker,
            &requester,
            PermissionIntent {
                tool_use_id: "call-2".into(),
                tool_name: "fs.write".into(),
                ..intent()
            },
        );

        flows.mark_terminal(&requester.run_id);

        for pending in [first, second] {
            assert!(matches!(
                pending.resolution.blocking_recv().unwrap(),
                PermissionResolution::Cancelled { .. }
            ));
        }
        assert!(broker.grants().is_empty());
        assert_eq!(broker.expire_terminal("late sweep"), 0);
    }

    #[test]
    fn terminal_before_resolve_leaves_no_grant_and_rejects_approval() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::shared(Arc::clone(&flows));
        let pending = submit_user(&broker, &requester, intent());
        let authority = user_decision(&broker, "session");

        flows.mark_terminal(&requester.run_id);

        assert!(matches!(
            broker.resolve(
                &pending.request.request_id,
                &authority,
                PermissionAction::Approve,
                Some(GrantScope::ChildRunSameTool {
                    run_id: requester.run_id.clone(),
                    tool_name: "bash.spawn".into(),
                }),
                None,
            ),
            Err(PermissionError::AlreadyResolved)
        ));
        assert!(broker.grants().is_empty());
        assert!(matches!(
            broker.get(&pending.request.request_id).unwrap().state,
            PermissionRequestState::Cancelled { .. }
        ));
        assert!(matches!(
            pending.resolution.blocking_recv().unwrap(),
            PermissionResolution::Cancelled { .. }
        ));
    }

    #[test]
    fn resolve_before_terminal_keeps_decision_but_revokes_grant() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::shared(Arc::clone(&flows));
        let pending = submit_user(&broker, &requester, intent());

        broker
            .resolve(
                &pending.request.request_id,
                &user_decision(&broker, "session"),
                PermissionAction::Approve,
                Some(GrantScope::ChildRunSameTool {
                    run_id: requester.run_id.clone(),
                    tool_name: "bash.spawn".into(),
                }),
                None,
            )
            .unwrap();
        assert_eq!(broker.grants().len(), 1);

        flows.mark_terminal(&requester.run_id);

        assert!(broker.grants().is_empty());
        assert!(matches!(
            broker.get(&pending.request.request_id).unwrap().state,
            PermissionRequestState::Approved { .. }
        ));
        assert!(matches!(
            pending.resolution.blocking_recv().unwrap(),
            PermissionResolution::Decision(_)
        ));
    }

    #[test]
    fn terminal_racing_approve_never_leaves_a_terminal_run_authorized() {
        for _ in 0..64 {
            let flows = Arc::new(FlowRegistry::default());
            let requester = register_root(&flows, "session", false);
            let broker = PermissionBroker::shared(Arc::clone(&flows));
            let pending = submit_user(&broker, &requester, intent());
            let request_id = pending.request.request_id.clone();
            let barrier = Arc::new(std::sync::Barrier::new(2));

            let approver = {
                let broker = Arc::clone(&broker);
                let barrier = Arc::clone(&barrier);
                let authority = user_decision(&broker, "session");
                let run_id = requester.run_id.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    broker.resolve(
                        &request_id,
                        &authority,
                        PermissionAction::Approve,
                        Some(GrantScope::ChildRunSameTool {
                            run_id,
                            tool_name: "bash.spawn".into(),
                        }),
                        None,
                    )
                })
            };
            let terminator = {
                let flows = Arc::clone(&flows);
                let barrier = Arc::clone(&barrier);
                let run_id = requester.run_id.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    flows.mark_terminal(&run_id);
                })
            };

            let approved = approver.join().unwrap();
            terminator.join().unwrap();

            // Both interleavings are legal, but a terminal run must never retain
            // authorization: grants are revoked either way.
            assert!(broker.grants().is_empty());
            match approved {
                Ok(ResolveOutcome::Resolved(_)) => assert!(matches!(
                    pending.resolution.blocking_recv().unwrap(),
                    PermissionResolution::Decision(_)
                )),
                Err(PermissionError::AlreadyResolved) => assert!(matches!(
                    pending.resolution.blocking_recv().unwrap(),
                    PermissionResolution::Cancelled { .. }
                )),
                other => panic!("unexpected outcome: {other:?}"),
            }
        }
    }

    #[test]
    fn terminal_observer_does_not_keep_broker_alive() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::shared(Arc::clone(&flows));
        let weak = Arc::downgrade(&broker);
        drop(broker);

        assert!(weak.upgrade().is_none());
        // A terminal transition after the broker is gone must not panic on the dead weak.
        flows.mark_terminal(&requester.run_id);
    }

    #[test]
    fn unrestricted_still_enforces_authority_and_target() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let requester = child(&flows, &root);
        let broker = PermissionBroker::new(Arc::clone(&flows));
        let policy = TrustConfig {
            mode: TrustMode::Reckless,
            ..TrustConfig::default()
        };

        let mut beyond = intent();
        beyond.tier = Tier::Four;
        beyond.risks.insert(RiskKind::Irreversible);
        assert!(matches!(
            broker.submit(
                Some(&requester.session_id),
                Some(&requester.run_id),
                beyond,
                true,
                &policy,
            ),
            Ok(SubmissionOutcome::Immediate(value)) if matches!(value.authorization, ImmediateAuthorization::Auto)
        ));
        assert!(submit_to_flow(&broker, &requester, Arc::clone(&requester)).is_err());
        assert_eq!(broker.list().len(), 1);
    }

    #[test]
    fn resolve_rejects_actor_that_is_not_the_current_target() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let requester = child(&flows, &root);
        let broker = PermissionBroker::new(Arc::clone(&flows));
        let SubmissionOutcome::Pending(pending) =
            submit_to_flow(&broker, &requester, Arc::clone(&root)).unwrap()
        else {
            panic!("expected pending request");
        };

        assert!(matches!(
            broker.resolve(
                &pending.request.request_id,
                &user_decision(&broker, "session"),
                PermissionAction::Approve,
                None,
                None,
            ),
            Err(PermissionError::ActorNotAuthorized)
        ));
        assert!(matches!(
            broker.get(&pending.request.request_id).unwrap().state,
            PermissionRequestState::Pending {
                target: ApprovalTarget::Flow(_)
            }
        ));
    }

    #[test]
    fn same_path_grant_scope_stays_unsupported() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(flows);
        let pending = submit_user(&broker, &requester, intent());

        assert!(matches!(
            broker.resolve(
                &pending.request.request_id,
                &user_decision(&broker, "session"),
                PermissionAction::Approve,
                Some(GrantScope::SamePathRuleUnsupported),
                None,
            ),
            Err(PermissionError::UnsupportedGrantScope)
        ));
        assert!(broker.grants().is_empty());
    }

    #[test]
    fn grant_scope_cannot_target_another_run_or_tool() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let other = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(flows);
        let authority = user_decision(&broker, "session");

        for scope in [
            GrantScope::ChildRunSameTool {
                run_id: other.run_id.clone(),
                tool_name: "bash.spawn".into(),
            },
            GrantScope::ChildRunSameTool {
                run_id: requester.run_id.clone(),
                tool_name: "fs.write".into(),
            },
        ] {
            let pending = submit_user(&broker, &requester, intent());
            assert!(matches!(
                broker.resolve(
                    &pending.request.request_id,
                    &authority,
                    PermissionAction::Approve,
                    Some(scope),
                    None,
                ),
                Err(PermissionError::GrantExceedsAuthority)
            ));
        }
        assert!(broker.grants().is_empty());
    }

    #[test]
    fn concurrent_resolvers_have_exactly_one_winner() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = Arc::new(PermissionBroker::new(flows));
        let pending = submit_user(&broker, &requester, intent());
        let request_id = pending.request.request_id.clone();
        let threads: Vec<_> = [PermissionAction::Approve, PermissionAction::Deny]
            .into_iter()
            .map(|action| {
                let broker = Arc::clone(&broker);
                let request_id = request_id.clone();
                let authority = user_decision(&broker, "session");
                std::thread::spawn(move || {
                    broker.resolve(&request_id, &authority, action, None, None)
                })
            })
            .collect();
        let outcomes: Vec<_> = threads
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();

        assert_eq!(outcomes.iter().filter(|outcome| outcome.is_ok()).count(), 1);
        assert_eq!(
            outcomes
                .iter()
                .filter(|outcome| matches!(outcome, Err(PermissionError::AlreadyResolved)))
                .count(),
            1
        );
        assert!(matches!(
            pending.resolution.blocking_recv().unwrap(),
            PermissionResolution::Decision(_)
        ));
    }
}
