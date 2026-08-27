use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, watch};
use uuid::Uuid;

use crate::event::FlowRunId;
use crate::flow_authority::{FlowExecutionState, FlowIdentity};
use crate::tool::{PathOrigin, Tier};
use crate::tools::agent_ctrl::FlowRegistry;
use crate::trust::{ExecutionPolicy, PolicyAction, RiskKind, TrustConfig};

pub const ANCESTOR_OFFER_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

macro_rules! permission_id {
    ($name:ident) => {
        #[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
permission_id!(PermissionGroupId);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupOwner {
    Flow(FlowRunId),
    User,
    System,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionGroup {
    pub group_id: PermissionGroupId,
    pub owner: GroupOwner,
    pub label: String,
    pub request_ids: BTreeSet<PermissionRequestId>,
    pub created_at: DateTime<Utc>,
    pub revision: u64,
}

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

    pub fn workspace_relative_path(&self) -> Option<String> {
        if self.is_external() || self.is_unbound() || self.authorized_targets().count() != 1 {
            return None;
        }
        let root = self.workspace_root.as_deref()?;
        let relative = self.authorized_targets().next()?.strip_prefix(root).ok()?;
        if relative.as_os_str().is_empty()
            || relative.components().any(|component| {
                matches!(
                    component,
                    std::path::Component::ParentDir | std::path::Component::RootDir
                )
            })
        {
            return None;
        }
        relative.to_str().map(str::to_owned)
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
    ChildRunSamePathRule {
        run_id: FlowRunId,
        tool_name: String,
        workspace_relative_path: String,
    },
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
    Granted { grant: Box<PermissionGrant> },
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

    pub(crate) fn provenance(&self) -> &ResourceProvenance {
        &self.provenance
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
    pub target_changes: watch::Receiver<ApprovalTarget>,
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
    GroupNotFound,
    GroupNotEmpty,
    GroupNotOwner,
    EmptyGroup,
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
            Self::GroupNotFound => "permission group was not found",
            Self::GroupNotEmpty => "permission group must be empty before deletion",
            Self::GroupNotOwner => "permission group is not owned by this actor",
            Self::EmptyGroup => "permission group cannot be empty",
        };
        f.write_str(message)
    }
}

impl std::error::Error for PermissionError {}

struct RequestEntry {
    request: PermissionRequest,
    responder: Option<oneshot::Sender<PermissionResolution>>,
    target_tx: Option<watch::Sender<ApprovalTarget>>,
}

struct SubmissionContext {
    target_authority: Option<ApprovalAuthority>,
}

#[derive(Default)]
struct BrokerState {
    requests: HashMap<PermissionRequestId, RequestEntry>,
    grants: Vec<PermissionGrant>,
    groups: HashMap<PermissionGroupId, PermissionGroup>,
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
            broker.handle_terminal_locked(session_id, run_id);
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
        self.submit_to_with_context(
            session_id,
            run_id,
            intent,
            shell,
            SubmissionContext {
                target_authority: None,
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
                target_authority: Some(target_authority),
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
        let identity =
            self.authenticate_requester(session_id, run_id, &requirement, &intent.provenance)?;
        let explicit_target = context
            .target_authority
            .map(|target| {
                self.authenticate_target(&identity, &requirement, &intent.provenance, target)
            })
            .transpose()?;
        let (execution_policy, action) = identity.effective_authority.constrain_policy(
            policy,
            intent.tier,
            intent.risks.iter().copied(),
        );
        let mut state = self.state.lock().unwrap();
        self.remove_terminal_grants(&mut state);
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
                    matching_grant(&state.grants, &identity, &intent, &requirement).map(|grant| {
                        ImmediateAuthorization::Granted {
                            grant: Box::new(grant),
                        }
                    })
                }
            }
        };
        let target = explicit_target.unwrap_or_else(|| {
            if immediate.is_none() {
                self.next_eligible_target(&identity, &requirement, &intent.provenance, None)
            } else {
                ApprovalTarget::User
            }
        });
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
                    target_tx: None,
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
        let (target_tx, target_changes) = watch::channel(match &request.state {
            PermissionRequestState::Pending { target } => target.clone(),
            _ => unreachable!("pending submission must have a target"),
        });
        state.requests.insert(
            request_id,
            RequestEntry {
                request: request.clone(),
                responder: Some(responder),
                target_tx: Some(target_tx),
            },
        );
        Ok(SubmissionOutcome::Pending(Box::new(PendingPermission {
            request,
            resolution,
            target_changes,
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
            target: target.clone(),
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
            let requester = self
                .flows
                .lookup_run(&entry.request.requesting_run_id)
                .ok_or(PermissionError::RequestNotFound)?;
            let next_target = self.next_eligible_target(
                &requester,
                &entry.request.requirement,
                &entry.request.intent.provenance,
                match &target {
                    ApprovalTarget::Flow(run_id) => Some(run_id),
                    ApprovalTarget::User => None,
                },
            );
            entry.request.state = PermissionRequestState::Pending {
                target: next_target.clone(),
            };
            if let Some(target_tx) = &entry.target_tx {
                let _ = target_tx.send(next_target.clone());
            }
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

    pub fn defer_timed_out_target(
        &self,
        request_id: &PermissionRequestId,
        expected_target: &FlowRunId,
    ) -> Result<bool, PermissionError> {
        self.flows.with_lifecycle_arbitration(|| {
            self.defer_unavailable_target_locked(
                request_id,
                expected_target,
                "permission.parent_timeout",
                "ancestor offer timed out",
            )
        })
    }

    fn defer_unavailable_target_locked(
        &self,
        request_id: &PermissionRequestId,
        expected_target: &FlowRunId,
        component: &str,
        reason: &str,
    ) -> Result<bool, PermissionError> {
        let mut state = self.state.lock().unwrap();
        let entry = state
            .requests
            .get_mut(request_id)
            .ok_or(PermissionError::RequestNotFound)?;
        if !matches!(
            &entry.request.state,
            PermissionRequestState::Pending { target: ApprovalTarget::Flow(run_id) }
                if run_id == expected_target
        ) {
            return Ok(false);
        }
        let requester = self
            .flows
            .lookup_run(&entry.request.requesting_run_id)
            .ok_or(PermissionError::RequestNotFound)?;
        let now = Utc::now();
        entry.request.escalation_path.push(EscalationHop {
            target: ApprovalTarget::Flow(expected_target.clone()),
            actor: Some(DecisionActor::System {
                component: component.into(),
            }),
            action: Some(PermissionAction::Defer),
            reason: Some(reason.into()),
            at: now,
        });
        let next_target = self.next_eligible_target(
            &requester,
            &entry.request.requirement,
            &entry.request.intent.provenance,
            Some(expected_target),
        );
        entry.request.state = PermissionRequestState::Pending {
            target: next_target.clone(),
        };
        entry.request.escalation_path.push(EscalationHop {
            target: next_target.clone(),
            actor: None,
            action: None,
            reason: None,
            at: now,
        });
        if let Some(target_tx) = &entry.target_tx {
            let _ = target_tx.send(next_target);
        }
        Ok(true)
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

    fn handle_terminal_locked(&self, session_id: &str, run_id: &FlowRunId) {
        self.cancel_for_run_locked(session_id, run_id, "requesting flow is terminal");
        let request_ids: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .requests
            .iter()
            .filter(|(_, entry)| {
                matches!(
                    &entry.request.state,
                    PermissionRequestState::Pending { target: ApprovalTarget::Flow(target) }
                        if target == run_id
                )
            })
            .map(|(request_id, _)| request_id.clone())
            .collect();
        for request_id in request_ids {
            let _ = self.defer_unavailable_target_locked(
                &request_id,
                run_id,
                "permission.target_terminal",
                "target flow became terminal",
            );
        }
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

    pub fn visible_get(
        &self,
        actor: &Arc<FlowIdentity>,
        request_id: &PermissionRequestId,
    ) -> Result<Option<PermissionRequest>, PermissionError> {
        self.authenticate_visibility_actor(actor)?;
        let request = self
            .state
            .lock()
            .unwrap()
            .requests
            .get(request_id)
            .map(|entry| entry.request.clone());
        Ok(request.filter(|request| self.visible_to(actor, request)))
    }

    pub fn visible_list(
        &self,
        actor: &Arc<FlowIdentity>,
    ) -> Result<Vec<PermissionRequest>, PermissionError> {
        self.authenticate_visibility_actor(actor)?;
        let mut requests: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .requests
            .values()
            .map(|entry| entry.request.clone())
            .filter(|request| self.visible_to(actor, request))
            .collect();
        requests.sort_by_key(|request| request.requested_at);
        Ok(requests)
    }

    pub fn authenticate_control_actor(
        &self,
        actor: &Arc<FlowIdentity>,
    ) -> Result<(), PermissionError> {
        self.authenticate_visibility_actor(actor)
    }

    pub fn create_group(
        &self,
        actor: &Arc<FlowIdentity>,
        request_ids: BTreeSet<PermissionRequestId>,
        label: String,
    ) -> Result<PermissionGroup, PermissionError> {
        self.authenticate_visibility_actor(actor)?;
        if request_ids.is_empty() {
            return Err(PermissionError::EmptyGroup);
        }
        let mut state = self.state.lock().unwrap();
        if request_ids.iter().any(|request_id| {
            state
                .requests
                .get(request_id)
                .is_none_or(|entry| !self.visible_to(actor, &entry.request))
        }) {
            return Err(PermissionError::ActorNotAuthorized);
        }
        let group = PermissionGroup {
            group_id: PermissionGroupId::now(),
            owner: GroupOwner::Flow(actor.run_id.clone()),
            label,
            request_ids,
            created_at: Utc::now(),
            revision: 0,
        };
        state.groups.insert(group.group_id.clone(), group.clone());
        Ok(group)
    }

    pub fn visible_group_get(
        &self,
        actor: &Arc<FlowIdentity>,
        group_id: &PermissionGroupId,
    ) -> Result<Option<PermissionGroup>, PermissionError> {
        self.authenticate_visibility_actor(actor)?;
        Ok(self
            .state
            .lock()
            .unwrap()
            .groups
            .get(group_id)
            .filter(|group| group.owner == GroupOwner::Flow(actor.run_id.clone()))
            .cloned())
    }

    pub fn visible_group_list(
        &self,
        actor: &Arc<FlowIdentity>,
    ) -> Result<Vec<PermissionGroup>, PermissionError> {
        self.authenticate_visibility_actor(actor)?;
        let mut groups: Vec<_> = self
            .state
            .lock()
            .unwrap()
            .groups
            .values()
            .filter(|group| group.owner == GroupOwner::Flow(actor.run_id.clone()))
            .cloned()
            .collect();
        groups.sort_by_key(|group| group.created_at);
        Ok(groups)
    }

    pub fn ungroup_requests(
        &self,
        actor: &Arc<FlowIdentity>,
        group_id: &PermissionGroupId,
        request_ids: &BTreeSet<PermissionRequestId>,
    ) -> Result<PermissionGroup, PermissionError> {
        self.authenticate_visibility_actor(actor)?;
        let mut state = self.state.lock().unwrap();
        let group = state
            .groups
            .get_mut(group_id)
            .ok_or(PermissionError::GroupNotFound)?;
        if group.owner != GroupOwner::Flow(actor.run_id.clone()) {
            return Err(PermissionError::GroupNotOwner);
        }
        let previous_len = group.request_ids.len();
        group.request_ids.retain(|id| !request_ids.contains(id));
        if group.request_ids.len() != previous_len {
            group.revision += 1;
        }
        Ok(group.clone())
    }

    pub fn delete_empty_group(
        &self,
        actor: &Arc<FlowIdentity>,
        group_id: &PermissionGroupId,
    ) -> Result<PermissionGroup, PermissionError> {
        self.authenticate_visibility_actor(actor)?;
        let mut state = self.state.lock().unwrap();
        let group = state
            .groups
            .get(group_id)
            .ok_or(PermissionError::GroupNotFound)?;
        if group.owner != GroupOwner::Flow(actor.run_id.clone()) {
            return Err(PermissionError::GroupNotOwner);
        }
        if !group.request_ids.is_empty() {
            return Err(PermissionError::GroupNotEmpty);
        }
        Ok(state.groups.remove(group_id).expect("group exists"))
    }

    fn authenticate_visibility_actor(
        &self,
        actor: &Arc<FlowIdentity>,
    ) -> Result<(), PermissionError> {
        self.authenticate_registered_identity(actor)?;
        if !matches!(actor.execution_state(), FlowExecutionState::Running) {
            return Err(PermissionError::ActorNotRunning);
        }
        if !actor.effective_authority.permission_management {
            return Err(PermissionError::PermissionManagementRequired);
        }
        Ok(())
    }

    fn visible_to(&self, actor: &FlowIdentity, request: &PermissionRequest) -> bool {
        request.session_id == actor.session_id
            && !request.state.is_terminal()
            && matches!(
                &request.state,
                PermissionRequestState::Pending {
                    target: ApprovalTarget::Flow(target),
                } if target == &actor.run_id
            )
            && self
                .flows
                .is_strict_ancestor(&actor.run_id, &request.requesting_run_id)
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
        provenance: &ResourceProvenance,
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
        if !authority_contains(&identity.effective_authority, requirement, provenance) {
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

    fn next_eligible_target(
        &self,
        requester: &FlowIdentity,
        requirement: &AuthorityRequirement,
        provenance: &ResourceProvenance,
        after: Option<&FlowRunId>,
    ) -> ApprovalTarget {
        let mut past_current = after.is_none();
        for ancestor in self.flows.strict_ancestors(&requester.run_id) {
            if !past_current {
                if after == Some(&ancestor.run_id) {
                    past_current = true;
                }
                continue;
            }
            if ancestor.session_id == requester.session_id
                && matches!(ancestor.execution_state(), FlowExecutionState::Running)
                && !matches!(
                    ancestor.invocation,
                    crate::flow_authority::InvocationKind::SpawnSync
                )
                && ancestor.effective_authority.permission_management
                && authority_contains(&ancestor.effective_authority, requirement, provenance)
            {
                return ApprovalTarget::Flow(ancestor.run_id.clone());
            }
        }
        ApprovalTarget::User
    }

    fn authenticate_target(
        &self,
        requester: &FlowIdentity,
        requirement: &AuthorityRequirement,
        provenance: &ResourceProvenance,
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
                if !authority_contains(&target.effective_authority, requirement, provenance) {
                    return Err(PermissionError::GrantExceedsAuthority);
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
        if let Some(GrantScope::ChildRunSamePathRule { .. }) = scope {
            validate_same_path_scope(request, scope)?;
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
                if !authority_contains(
                    &identity.effective_authority,
                    &request.requirement,
                    &request.intent.provenance,
                ) {
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
                && match &grant.scope {
                    GrantScope::ChildRunSameTool { run_id, tool_name } => {
                        run_id == &identity.run_id && tool_name == &intent.tool_name
                    }
                    GrantScope::ChildRunSamePathRule {
                        run_id,
                        tool_name,
                        workspace_relative_path,
                    } => {
                        run_id == &identity.run_id
                            && tool_name == &intent.tool_name
                            && intent.provenance.authorized_targets().count() == 1
                            && intent
                                .provenance
                                .workspace_root
                                .as_deref()
                                .and_then(|root| {
                                    intent
                                        .provenance
                                        .authorized_targets()
                                        .next()
                                        .and_then(|path| path.strip_prefix(root).ok())
                                })
                                .is_some_and(|path| {
                                    path.to_str() == Some(workspace_relative_path.as_str())
                                })
                    }
                    GrantScope::CurrentCall => false,
                }
        })
        .cloned()
}

fn validate_same_path_scope(
    request: &PermissionRequest,
    scope: Option<&GrantScope>,
) -> Result<(), PermissionError> {
    let Some(GrantScope::ChildRunSamePathRule {
        run_id,
        tool_name,
        workspace_relative_path,
    }) = scope
    else {
        return Ok(());
    };
    if run_id != &request.requesting_run_id || tool_name != &request.intent.tool_name {
        return Err(PermissionError::GrantExceedsAuthority);
    }
    let provenance = &request.intent.provenance;
    if provenance.is_external()
        || provenance.is_unbound()
        || provenance.authorized_targets().count() != 1
        || workspace_relative_path.is_empty()
    {
        return Err(PermissionError::UnsupportedGrantScope);
    }
    let Some(root) = provenance.workspace_root.as_deref() else {
        return Err(PermissionError::UnsupportedGrantScope);
    };
    let Some(path) = provenance.authorized_targets().next() else {
        return Err(PermissionError::UnsupportedGrantScope);
    };
    let Ok(relative) = path.strip_prefix(root) else {
        return Err(PermissionError::GrantExceedsAuthority);
    };
    if relative.components().any(|component| {
        matches!(
            component,
            std::path::Component::ParentDir | std::path::Component::RootDir
        )
    }) || relative.as_os_str().is_empty()
        || relative.to_str() != Some(workspace_relative_path.as_str())
    {
        return Err(PermissionError::UnsupportedGrantScope);
    }
    Ok(())
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
    provenance: &ResourceProvenance,
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
        && provenance.authorized_targets().all(|target| {
            authority.workspace_root.as_deref().is_none_or(|root| {
                crate::fs_access::canonicalize_stable(target)
                    .strip_prefix(root)
                    .is_ok()
            })
        })
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
                RiskKind::Network,
                RiskKind::Irreversible,
                RiskKind::FilesystemWrite,
                RiskKind::ProcessSpawn,
                RiskKind::RepositoryMutation,
            ]),
            tier_ceiling: [PolicyAction::Auto; 5],
            risk_ceiling: [PolicyAction::Auto; 6],
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
    fn blocked_sync_parent_is_skipped_during_escalation() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let sync_parent = flows
            .register_child(
                &root.run_id,
                FlowRunId::now(),
                InvocationKind::SpawnSync,
                true,
                ChildWorkspaceAuthority::Inherit,
            )
            .unwrap();
        let requester = child(&flows, &sync_parent);
        let broker = PermissionBroker::new(Arc::clone(&flows));

        let SubmissionOutcome::Pending(pending) =
            submit_to_flow(&broker, &requester, sync_parent.clone()).unwrap()
        else {
            panic!("expected pending request");
        };
        assert!(matches!(
            pending.request.state,
            PermissionRequestState::Pending {
                target: ApprovalTarget::Flow(ref run_id)
            } if run_id == &root.run_id
        ));
    }

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
    fn same_path_grant_scope_stays_unsupported_for_unbound_resources() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(flows);
        let pending = submit_user(&broker, &requester, intent());

        assert!(matches!(
            broker.resolve(
                &pending.request.request_id,
                &user_decision(&broker, "session"),
                PermissionAction::Approve,
                Some(GrantScope::ChildRunSamePathRule {
                    run_id: requester.run_id.clone(),
                    tool_name: "bash.spawn".into(),
                    workspace_relative_path: "src/lib.rs".into(),
                }),
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
    #[test]
    fn root_capability_does_not_amplify_child_authority() {
        let trust = TrustConfig::default();
        let root = EffectiveAuthority::root(&trust, false, None);
        assert!(root.permission_management);
        let restricted = EffectiveAuthority {
            permission_management: false,
            ..root.clone()
        };
        let child = root.for_child(&restricted, false, None).unwrap();
        assert!(!child.permission_management);
    }

    #[test]
    fn nearest_target_skips_terminal_ancestor() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let middle = child(&flows, &root);
        let requester = child(&flows, &middle);
        flows.mark_terminal(&middle.run_id);
        let broker = PermissionBroker::new(Arc::clone(&flows));
        let requirement = AuthorityRequirement::from_intent(&intent(), false);
        assert_eq!(
            broker.next_eligible_target(&requester, &requirement, &intent().provenance, None,),
            ApprovalTarget::Flow(root.run_id.clone())
        );
    }

    #[test]
    fn defer_records_flow_actor_and_moves_to_next_hop() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let requester = child(&flows, &root);
        let broker = PermissionBroker::new(Arc::clone(&flows));
        let SubmissionOutcome::Pending(pending) =
            submit_to_flow(&broker, &requester, root.clone()).unwrap()
        else {
            panic!("expected pending");
        };
        let authority = DecisionAuthority::Flow(broker.flow_authority(root.clone()).unwrap());
        assert!(matches!(
            broker.resolve(
                &pending.request.request_id,
                &authority,
                PermissionAction::Defer,
                None,
                Some("not mine".into())
            ),
            Ok(ResolveOutcome::Deferred(_))
        ));
        let request = broker.get(&pending.request.request_id).unwrap();
        assert!(
            matches!(request.escalation_path[1].actor, Some(DecisionActor::Flow { ref run_id, .. }) if run_id == &root.run_id)
        );
        assert!(matches!(
            request.state,
            PermissionRequestState::Pending {
                target: ApprovalTarget::User
            }
        ));
    }

    #[test]
    fn stale_timeout_cannot_retarget_resolved_request() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let requester = child(&flows, &root);
        let broker = PermissionBroker::new(Arc::clone(&flows));
        let SubmissionOutcome::Pending(pending) =
            submit_to_flow(&broker, &requester, root.clone()).unwrap()
        else {
            panic!("expected pending");
        };
        broker
            .resolve(
                &pending.request.request_id,
                &DecisionAuthority::Flow(broker.flow_authority(root).unwrap()),
                PermissionAction::Approve,
                None,
                None,
            )
            .unwrap();
        assert!(
            !broker
                .defer_timed_out_target(&pending.request.request_id, &requester.run_id)
                .unwrap()
        );
        assert!(matches!(
            broker.get(&pending.request.request_id).unwrap().state,
            PermissionRequestState::Approved { .. }
        ));
    }

    #[test]
    fn terminal_target_retargets_pending_request() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let middle = child(&flows, &root);
        let requester = child(&flows, &middle);
        let broker = PermissionBroker::shared(Arc::clone(&flows));
        let SubmissionOutcome::Pending(pending) =
            submit_to_flow(&broker, &requester, middle.clone()).unwrap()
        else {
            panic!("expected pending");
        };
        flows.mark_terminal(&middle.run_id);
        assert_eq!(
            pending.target_changes.borrow().clone(),
            ApprovalTarget::Flow(root.run_id.clone())
        );
        assert_eq!(
            broker.get(&pending.request.request_id).unwrap().state,
            PermissionRequestState::Pending {
                target: ApprovalTarget::Flow(root.run_id.clone())
            }
        );
    }

    #[test]
    fn visible_apis_reject_unknown_forged_terminal_blocked_and_unprivileged_actors() {
        let cases = ["unknown", "forged", "terminal", "blocked", "unprivileged"];
        for case in cases {
            let flows = Arc::new(FlowRegistry::default());
            let actor = register_root(&flows, "session", case != "unprivileged");
            let requester = child(&flows, &actor);
            let broker = PermissionBroker::new(Arc::clone(&flows));
            let pending = if case == "unprivileged" {
                submit_user(&broker, &requester, intent())
            } else {
                let SubmissionOutcome::Pending(pending) =
                    submit_to_flow(&broker, &requester, Arc::clone(&actor)).unwrap()
                else {
                    panic!("expected pending");
                };
                *pending
            };
            let tested_actor = match case {
                "unknown" => Arc::new(FlowIdentity {
                    session_id: actor.session_id.clone(),
                    run_id: FlowRunId::now(),
                    parent_run_id: None,
                    root_run_id: actor.root_run_id.clone(),
                    invocation: InvocationKind::Root,
                    effective_authority: authority(true),
                    execution_state: Mutex::new(FlowExecutionState::Running),
                }),
                "forged" => Arc::new(FlowIdentity {
                    session_id: actor.session_id.clone(),
                    run_id: actor.run_id.clone(),
                    parent_run_id: actor.parent_run_id.clone(),
                    root_run_id: actor.root_run_id.clone(),
                    invocation: actor.invocation,
                    effective_authority: actor.effective_authority.clone(),
                    execution_state: Mutex::new(FlowExecutionState::Running),
                }),
                "terminal" => {
                    flows.mark_terminal(&actor.run_id);
                    Arc::clone(&actor)
                }
                "blocked" => {
                    *actor.execution_state.lock().unwrap() =
                        FlowExecutionState::BlockedOnDescendants {
                            child_run_counts: std::collections::HashMap::from([(
                                requester.run_id.clone(),
                                1,
                            )]),
                        };
                    Arc::clone(&actor)
                }
                "unprivileged" => Arc::clone(&actor),
                _ => unreachable!(),
            };

            assert!(broker.visible_list(&tested_actor).is_err(), "{case}");
            assert!(
                broker
                    .visible_get(&tested_actor, &pending.request.request_id)
                    .is_err(),
                "{case}"
            );
        }
    }

    #[test]
    fn visible_apis_only_return_pending_requests_targeted_to_the_actor() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let middle = child(&flows, &root);
        let requester = child(&flows, &middle);
        let broker = PermissionBroker::new(Arc::clone(&flows));
        let SubmissionOutcome::Pending(pending) =
            submit_to_flow(&broker, &requester, Arc::clone(&middle)).unwrap()
        else {
            panic!("expected pending");
        };

        assert_eq!(broker.visible_list(&middle).unwrap().len(), 1);
        assert!(
            broker
                .visible_get(&root, &pending.request.request_id)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn flow_groups_are_owner_scoped_and_delete_only_when_empty() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let sibling_owner = child(&flows, &root);
        let requester = child(&flows, &root);
        let broker = PermissionBroker::new(Arc::clone(&flows));
        let SubmissionOutcome::Pending(pending) =
            submit_to_flow(&broker, &requester, Arc::clone(&root)).unwrap()
        else {
            panic!("expected pending");
        };
        let request_id = pending.request.request_id.clone();
        let group = broker
            .create_group(&root, BTreeSet::from([request_id.clone()]), "review".into())
            .unwrap();

        assert_eq!(
            broker.visible_group_list(&root).unwrap(),
            vec![group.clone()]
        );
        assert!(
            broker
                .visible_group_get(&sibling_owner, &group.group_id)
                .unwrap()
                .is_none()
        );
        assert!(matches!(
            broker.delete_empty_group(&root, &group.group_id),
            Err(PermissionError::GroupNotEmpty)
        ));
        let emptied = broker
            .ungroup_requests(&root, &group.group_id, &BTreeSet::from([request_id]))
            .unwrap();
        assert!(emptied.request_ids.is_empty());
        assert_eq!(emptied.revision, 1);
        broker.delete_empty_group(&root, &group.group_id).unwrap();
        assert!(broker.visible_group_list(&root).unwrap().is_empty());
    }

    #[test]
    fn group_creation_rejects_hidden_and_unknown_request_ids() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let middle = child(&flows, &root);
        let requester = child(&flows, &middle);
        let broker = PermissionBroker::new(Arc::clone(&flows));
        let SubmissionOutcome::Pending(pending) =
            submit_to_flow(&broker, &requester, Arc::clone(&middle)).unwrap()
        else {
            panic!("expected pending");
        };

        for request_id in [pending.request.request_id, PermissionRequestId::now()] {
            assert!(matches!(
                broker.create_group(&root, BTreeSet::from([request_id]), "hidden".into()),
                Err(PermissionError::ActorNotAuthorized)
            ));
        }
        assert!(broker.visible_group_list(&root).unwrap().is_empty());
    }

    #[test]
    fn delegated_workspace_ancestor_is_skipped_when_resource_is_outside_its_root() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = temp.path().join("root");
        let delegated_path = temp.path().join("delegated");
        std::fs::create_dir_all(&root_path).unwrap();
        std::fs::create_dir_all(&delegated_path).unwrap();
        let flows = Arc::new(FlowRegistry::default());
        let root = flows
            .register_root(
                "session".into(),
                FlowRunId::now(),
                EffectiveAuthority {
                    workspace_root: Some(crate::fs_access::canonicalize_stable(&root_path)),
                    ..authority(true)
                },
            )
            .unwrap();
        let middle = flows
            .register_child(
                &root.run_id,
                FlowRunId::now(),
                InvocationKind::InlineSubflow,
                true,
                ChildWorkspaceAuthority::TrustedDelegation(delegated_path.clone()),
            )
            .unwrap();
        let requester = child(&flows, &middle);
        let broker = PermissionBroker::new(Arc::clone(&flows));
        let delegated_intent = PermissionIntent {
            provenance: ResourceProvenance {
                path: Some(delegated_path.join("file.txt")),
                path_origin: Some(PathOrigin::ExplicitInside),
                workspace_root: Some(delegated_path),
                ..ResourceProvenance::default()
            },
            ..intent()
        };
        let SubmissionOutcome::Pending(pending) = broker
            .submit(
                Some(&requester.session_id),
                Some(&requester.run_id),
                delegated_intent,
                false,
                &ask_policy(),
            )
            .unwrap()
        else {
            panic!("expected pending");
        };
        assert_eq!(
            pending.request.state,
            PermissionRequestState::Pending {
                target: ApprovalTarget::Flow(middle.run_id.clone())
            }
        );
        broker
            .resolve(
                &pending.request.request_id,
                &DecisionAuthority::Flow(broker.flow_authority(middle).unwrap()),
                PermissionAction::Defer,
                None,
                None,
            )
            .unwrap();
        assert_eq!(
            broker.get(&pending.request.request_id).unwrap().state,
            PermissionRequestState::Pending {
                target: ApprovalTarget::User
            }
        );
    }

    #[test]
    fn delegated_requester_cannot_submit_resource_outside_delegated_workspace() {
        let temp = tempfile::tempdir().unwrap();
        let parent_path = temp.path().join("parent");
        let delegated_path = temp.path().join("delegated");
        std::fs::create_dir_all(&parent_path).unwrap();
        std::fs::create_dir_all(&delegated_path).unwrap();
        let parent_root = crate::fs_access::canonicalize_stable(&parent_path);
        let delegated_root = crate::fs_access::canonicalize_stable(&delegated_path);
        let flows = Arc::new(FlowRegistry::default());
        let parent = flows
            .register_root(
                "session".into(),
                FlowRunId::now(),
                EffectiveAuthority {
                    workspace_root: Some(parent_root.clone()),
                    ..authority(true)
                },
            )
            .unwrap();
        let delegated = flows
            .register_child(
                &parent.run_id,
                FlowRunId::now(),
                InvocationKind::InlineSubflow,
                true,
                ChildWorkspaceAuthority::TrustedDelegation(delegated_root.clone()),
            )
            .unwrap();
        let broker = PermissionBroker::new(Arc::clone(&flows));
        let outside_intent = PermissionIntent {
            provenance: ResourceProvenance {
                path: Some(parent_root.join("parent.txt")),
                path_origin: Some(PathOrigin::ExplicitInside),
                workspace_root: Some(parent_root),
                ..ResourceProvenance::default()
            },
            ..intent()
        };
        let policies = [
            ask_policy(),
            TrustConfig {
                mode: TrustMode::Eager,
                escalation: EscalationPolicy::Allow,
                ..TrustConfig::default()
            },
            TrustConfig {
                mode: TrustMode::Reckless,
                ..TrustConfig::default()
            },
        ];
        for policy in policies {
            assert!(matches!(
                broker.submit(
                    Some(&delegated.session_id),
                    Some(&delegated.run_id),
                    outside_intent.clone(),
                    false,
                    &policy,
                ),
                Err(PermissionError::GrantExceedsAuthority)
            ));
        }
        assert!(broker.list().is_empty());
        assert!(broker.grants().is_empty());
    }

    #[test]
    fn automatic_and_denied_submissions_do_not_require_approval_target_selection() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let requester = child(&flows, &root);
        flows.mark_terminal(&root.run_id);
        let broker = PermissionBroker::new(flows);
        for policy in [
            TrustConfig {
                mode: TrustMode::Eager,
                escalation: EscalationPolicy::Allow,
                ..TrustConfig::default()
            },
            TrustConfig {
                mode: TrustMode::Eager,
                escalation: EscalationPolicy::Deny,
                ..TrustConfig::default()
            },
        ] {
            assert!(matches!(
                broker.submit(
                    Some(&requester.session_id),
                    Some(&requester.run_id),
                    intent(),
                    false,
                    &policy,
                ),
                Ok(SubmissionOutcome::Immediate(_))
            ));
        }
    }

    #[test]
    fn ask_target_selection_is_lifecycle_arbitrated_with_terminal_transition() {
        for _ in 0..64 {
            let flows = Arc::new(FlowRegistry::default());
            let root = register_root(&flows, "session", true);
            let requester = child(&flows, &root);
            let broker = PermissionBroker::shared(Arc::clone(&flows));
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let submitter = {
                let broker = Arc::clone(&broker);
                let barrier = Arc::clone(&barrier);
                let requester = Arc::clone(&requester);
                std::thread::spawn(move || {
                    barrier.wait();
                    broker.submit(
                        Some(&requester.session_id),
                        Some(&requester.run_id),
                        intent(),
                        false,
                        &ask_policy(),
                    )
                })
            };
            let terminator = {
                let flows = Arc::clone(&flows);
                let barrier = Arc::clone(&barrier);
                let root_run_id = root.run_id.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    flows.mark_terminal(&root_run_id);
                })
            };

            let outcome = submitter.join().unwrap().unwrap();
            terminator.join().unwrap();
            let SubmissionOutcome::Pending(pending) = outcome else {
                panic!("Ask must remain pending");
            };
            assert_eq!(
                broker.get(&pending.request.request_id).unwrap().state,
                PermissionRequestState::Pending {
                    target: ApprovalTarget::User
                }
            );
        }
    }

    #[test]
    fn timeout_is_per_hop_stale_safe_and_user_is_terminal_fallback() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let middle = child(&flows, &root);
        let requester = child(&flows, &middle);
        let broker = PermissionBroker::new(Arc::clone(&flows));
        let pending = match broker
            .submit(
                Some(&requester.session_id),
                Some(&requester.run_id),
                intent(),
                false,
                &ask_policy(),
            )
            .unwrap()
        {
            SubmissionOutcome::Pending(pending) => pending,
            _ => panic!("expected pending"),
        };
        assert!(
            broker
                .defer_timed_out_target(&pending.request.request_id, &middle.run_id)
                .unwrap()
        );
        assert_eq!(
            pending.target_changes.borrow().clone(),
            ApprovalTarget::Flow(root.run_id.clone())
        );
        assert!(
            !broker
                .defer_timed_out_target(&pending.request.request_id, &middle.run_id)
                .unwrap()
        );
        assert!(
            broker
                .defer_timed_out_target(&pending.request.request_id, &root.run_id)
                .unwrap()
        );
        assert_eq!(
            pending.target_changes.borrow().clone(),
            ApprovalTarget::User
        );
        assert!(
            !broker
                .defer_timed_out_target(&pending.request.request_id, &root.run_id)
                .unwrap()
        );
    }

    #[test]
    fn persistent_same_path_grant_matches_only_same_run_tool_path_and_narrower_risk() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = crate::fs_access::canonicalize_stable(temp.path());
        let file = root_path.join("file.txt");
        let other = root_path.join("other.txt");
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let other_run = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(flows);
        let approved_intent = PermissionIntent {
            risks: BTreeSet::from([RiskKind::FilesystemWrite]),
            provenance: ResourceProvenance {
                path: Some(file.clone()),
                path_origin: Some(PathOrigin::ExplicitInside),
                workspace_root: Some(root_path.clone()),
                risks: BTreeSet::from([RiskKind::FilesystemWrite]),
                ..ResourceProvenance::default()
            },
            ..intent()
        };
        let pending = submit_user(&broker, &requester, approved_intent.clone());
        broker
            .resolve(
                &pending.request.request_id,
                &user_decision(&broker, "session"),
                PermissionAction::Approve,
                Some(GrantScope::ChildRunSamePathRule {
                    run_id: requester.run_id.clone(),
                    tool_name: "bash.spawn".into(),
                    workspace_relative_path: "file.txt".into(),
                }),
                None,
            )
            .unwrap();

        let mut narrower = approved_intent.clone();
        narrower.risks.clear();
        narrower.provenance.risks.clear();
        assert!(
            broker
                .find_matching_grant(&requester, &narrower, false)
                .is_some()
        );
        let mut wrong_tool = narrower.clone();
        wrong_tool.tool_name = "fs.write".into();
        assert!(
            broker
                .find_matching_grant(&requester, &wrong_tool, false)
                .is_none()
        );
        let mut wrong_path = narrower.clone();
        wrong_path.provenance.path = Some(other);
        assert!(
            broker
                .find_matching_grant(&requester, &wrong_path, false)
                .is_none()
        );
        assert!(
            broker
                .find_matching_grant(&other_run, &narrower, false)
                .is_none()
        );
        let mut escalated = approved_intent;
        escalated.risks.insert(RiskKind::Irreversible);
        assert!(
            broker
                .find_matching_grant(&requester, &escalated, false)
                .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn persistent_same_path_grant_rejects_non_utf8_relative_path() {
        use std::os::unix::ffi::OsStringExt;

        let temp = tempfile::tempdir().unwrap();
        let root_path = crate::fs_access::canonicalize_stable(temp.path());
        let path = root_path.join(std::ffi::OsString::from_vec(vec![0xff]));
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(flows);
        let pending = submit_user(
            &broker,
            &requester,
            PermissionIntent {
                provenance: ResourceProvenance {
                    path: Some(path),
                    path_origin: Some(PathOrigin::ExplicitInside),
                    workspace_root: Some(root_path),
                    ..ResourceProvenance::default()
                },
                ..intent()
            },
        );
        assert!(matches!(
            broker.resolve(
                &pending.request.request_id,
                &user_decision(&broker, "session"),
                PermissionAction::Approve,
                Some(GrantScope::ChildRunSamePathRule {
                    run_id: requester.run_id.clone(),
                    tool_name: "bash.spawn".into(),
                    workspace_relative_path: "�".into(),
                }),
                None,
            ),
            Err(PermissionError::UnsupportedGrantScope)
        ));
    }

    #[test]
    fn root_authority_retains_permission_management_capability() {
        let root = EffectiveAuthority::root(&TrustConfig::default(), true, None);
        assert!(root.permission_management);
        let child = root
            .inherited_child(true, ChildWorkspaceAuthority::Inherit)
            .unwrap();
        assert!(child.permission_management);
    }

    #[test]
    fn exact_path_authorization_rejects_sibling_path() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("file.txt");
        let sibling = root.path().join("file.txt.bak");
        let other = root.path().join("other.txt");
        std::fs::write(&file, "content").unwrap();
        std::fs::write(&sibling, "content").unwrap();
        std::fs::write(&other, "content").unwrap();
        let authorization = InvocationAuthorization::new(
            PermissionRequestId::now(),
            "call-1",
            "fs.write",
            ResourceProvenance {
                path: Some(crate::fs_access::canonicalize_stable(&file)),
                path_origin: Some(PathOrigin::ExplicitInside),
                workspace_root: Some(crate::fs_access::canonicalize_stable(root.path())),
                ..ResourceProvenance::default()
            },
            true,
        );
        assert!(authorization.covers("fs.write", &file));
        assert!(!authorization.covers("fs.write", &sibling));
        assert!(!authorization.covers("fs.write", &other));
    }
}
