use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
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
    pub policy_reference: crate::permission_audit::PermissionPolicyReference,
    pub state: PermissionRequestState,
    /// Optimistic concurrency token for client decisions. Incremented for every
    /// state or target transition while the stable request ID is unchanged.
    pub revision: u64,
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

/// Stable projections used by batch permission controls. Selectors never carry
/// authority; they are expanded and checked against the authenticated actor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionSelector {
    RequestIds(Vec<PermissionRequestId>),
    Group(PermissionGroupId),
    DescendantRun(FlowRunId),
    ChildRun,
    Tool(String),
    Tier(Tier),
    Risk(RiskKind),
    PathPrefix(PathBuf),
    Target(ApprovalTarget),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchMode {
    BestEffort,
    Atomic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BatchRequestOutcome {
    Approved(PermissionDecision),
    Denied(PermissionDecision),
    Deferred(PermissionDecision),
    SkippedAlreadyResolved,
    RejectedNotAncestor,
    RejectedOverAuthority,
    RejectedStale,
    RejectedNotFound,
    RejectedNotRunning,
    RejectedPermissionManagementRequired,
    RejectedUnsupportedGrantScope,
    RejectedNoEscalationTarget,
    Rejected(PermissionError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BatchResolution {
    pub request_id: PermissionRequestId,
    pub outcome: BatchRequestOutcome,
}

struct PreparedResolution {
    request: PermissionRequest,
    actor: DecisionActor,
    target: ApprovalTarget,
    next_target: Option<ApprovalTarget>,
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
    GroupRevisionConflict,
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
            Self::GroupRevisionConflict => "permission group revision is stale",
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
    resolved_group_audits: BTreeSet<PermissionGroupId>,
    next_audit_bundle: u64,
    pending_audit_bundles: BTreeMap<u64, Vec<crate::permission_audit::PermissionAuditRecord>>,
}

#[derive(Default)]
struct AuditDispatcher {
    next_bundle: u64,
}

#[cfg(test)]
type AuditDispatchHook = Arc<dyn Fn(u64) + Send + Sync>;

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
    audit: Mutex<Option<crate::permission_audit::PermissionAuditProjector>>,
    audit_dispatcher: Mutex<AuditDispatcher>,
    #[cfg(test)]
    audit_dispatch_hook: Mutex<Option<AuditDispatchHook>>,
    /// Keeps the registry-observed cleanup hook alive for exactly this broker's lifetime.
    observer: Mutex<Option<Arc<dyn crate::tools::agent_ctrl::FlowTerminalObserver>>>,
}

/// Bridges registry terminal transitions into the broker without keeping the broker
/// alive: the registry stores only a `Weak`, so a dropped broker deregisters itself.
struct TerminalCleanup {
    broker: std::sync::Weak<PermissionBroker>,
}

impl crate::tools::agent_ctrl::FlowTerminalObserver for TerminalCleanup {
    fn flow_became_terminal(
        &self,
        session_id: &str,
        run_id: &FlowRunId,
    ) -> Option<Box<dyn FnOnce() + Send>> {
        let broker = self.broker.upgrade()?;
        broker.handle_terminal_locked(session_id, run_id);
        Some(Box::new(move || broker.dispatch_audit_bundles()))
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
            audit: Mutex::new(None),
            audit_dispatcher: Mutex::new(AuditDispatcher::default()),
            #[cfg(test)]
            audit_dispatch_hook: Mutex::new(None),
            observer: Mutex::new(None),
        }
    }

    pub fn set_audit_projector(
        &self,
        projector: crate::permission_audit::PermissionAuditProjector,
    ) {
        self.audit.lock().unwrap().replace(projector);
    }

    fn group_is_resolved(state: &BrokerState, group: &PermissionGroup) -> bool {
        group.request_ids.iter().all(|request_id| {
            state.requests.get(request_id).is_some_and(|entry| {
                !matches!(entry.request.state, PermissionRequestState::Pending { .. })
            })
        })
    }

    fn unresolved_groups_for_requests(
        state: &BrokerState,
        request_ids: &BTreeSet<PermissionRequestId>,
    ) -> BTreeSet<PermissionGroupId> {
        state
            .groups
            .values()
            .filter(|group| {
                !group.request_ids.is_disjoint(request_ids)
                    && !Self::group_is_resolved(state, group)
            })
            .map(|group| group.group_id.clone())
            .collect()
    }

    fn append_resolved_group_audits(
        state: &mut BrokerState,
        candidate_group_ids: &BTreeSet<PermissionGroupId>,
        records: &mut Vec<crate::permission_audit::PermissionAuditRecord>,
        at: DateTime<Utc>,
    ) {
        for group_id in candidate_group_ids {
            if state.resolved_group_audits.contains(group_id) {
                continue;
            }
            let Some(group) = state.groups.get(group_id) else {
                continue;
            };
            if !Self::group_is_resolved(state, group) {
                continue;
            }
            let session_id = group
                .request_ids
                .iter()
                .find_map(|request_id| {
                    state
                        .requests
                        .get(request_id)
                        .map(|entry| entry.request.session_id.as_str())
                })
                .unwrap_or("unknown");
            let audit =
                crate::permission_audit::PermissionGroupAudit::from_group(group, session_id, at);
            records.push(crate::permission_audit::PermissionAuditRecord::GroupResolved(audit));
            state.resolved_group_audits.insert(group_id.clone());
        }
    }

    fn enqueue_audit_bundle(
        state: &mut BrokerState,
        records: Vec<crate::permission_audit::PermissionAuditRecord>,
    ) -> u64 {
        let sequence = state.next_audit_bundle;
        state.next_audit_bundle += 1;
        assert!(
            state
                .pending_audit_bundles
                .insert(sequence, records)
                .is_none(),
            "audit bundle sequence is unique"
        );
        sequence
    }

    fn dispatch_audit_bundles(&self) {
        let mut dispatcher = self.audit_dispatcher.lock().unwrap();
        loop {
            let records = self
                .state
                .lock()
                .unwrap()
                .pending_audit_bundles
                .remove(&dispatcher.next_bundle);
            let Some(records) = records else {
                break;
            };
            dispatcher.next_bundle += 1;
            let projector = self.audit.lock().unwrap().clone();
            if let Some(projector) = projector {
                for record in records {
                    projector.emit(record);
                }
            }
        }
    }

    fn dispatch_audit_bundle(&self, _sequence: u64) {
        #[cfg(test)]
        let hook = self.audit_dispatch_hook.lock().unwrap().clone();
        #[cfg(test)]
        if let Some(hook) = hook {
            hook(_sequence);
        }
        self.dispatch_audit_bundles();
    }

    #[cfg(test)]
    fn set_audit_dispatch_hook(&self, hook: AuditDispatchHook) {
        self.audit_dispatch_hook.lock().unwrap().replace(hook);
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

    pub fn user_authority(
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
        let (outcome, sequence) = self.flows.with_lifecycle_arbitration(|| {
            self.submit_to_locked(session_id, run_id, intent, shell, context, policy)
        })?;
        self.dispatch_audit_bundle(sequence);
        Ok(outcome)
    }

    fn submit_to_locked(
        &self,
        session_id: Option<&str>,
        run_id: Option<&FlowRunId>,
        intent: PermissionIntent,
        shell: bool,
        context: SubmissionContext,
        policy: &TrustConfig,
    ) -> Result<(SubmissionOutcome, u64), PermissionError> {
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
            policy_reference: crate::permission_audit::PermissionPolicyReference::capture(
                policy,
                intent.tier,
                &intent.risks,
            ),
            intent,
            requirement,
            state: request_state,
            revision: 0,
            escalation_path: vec![EscalationHop {
                target,
                actor: None,
                action: None,
                reason: None,
                at: now,
            }],
            requested_at: now,
        };
        let audits = submission_audits(&request, immediate.as_ref(), now);
        if let Some(authorization) = immediate {
            state.requests.insert(
                request_id,
                RequestEntry {
                    request: request.clone(),
                    responder: None,
                    target_tx: None,
                },
            );
            let sequence = Self::enqueue_audit_bundle(&mut state, audits);
            return Ok((
                SubmissionOutcome::Immediate(Box::new(ImmediateSubmission {
                    request,
                    authorization,
                })),
                sequence,
            ));
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
        let sequence = Self::enqueue_audit_bundle(&mut state, audits);
        Ok((
            SubmissionOutcome::Pending(Box::new(PendingPermission {
                request,
                resolution,
                target_changes,
            })),
            sequence,
        ))
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
        let (outcome, sequence) = self.flows.with_lifecycle_arbitration(|| {
            self.resolve_locked(request_id, authority, action, grant_scope, reason)
        })?;
        self.dispatch_audit_bundle(sequence);
        Ok(outcome)
    }

    fn resolve_locked(
        &self,
        request_id: &PermissionRequestId,
        authority: &DecisionAuthority,
        action: PermissionAction,
        grant_scope: Option<GrantScope>,
        reason: Option<String>,
    ) -> Result<(ResolveOutcome, u64), PermissionError> {
        let mut state = self.state.lock().unwrap();
        let request_ids = BTreeSet::from([request_id.clone()]);
        let candidate_groups = Self::unresolved_groups_for_requests(&state, &request_ids);
        let outcome = self.resolve_locked_state(
            &mut state,
            request_id,
            authority,
            action,
            grant_scope,
            reason,
        )?;
        let decision = match &outcome {
            ResolveOutcome::Resolved(decision) | ResolveOutcome::Deferred(decision) => decision,
        };
        let mut audits = resolution_audits_from_state(&state, request_id, decision);
        Self::append_resolved_group_audits(
            &mut state,
            &candidate_groups,
            &mut audits,
            decision.decided_at,
        );
        let sequence = Self::enqueue_audit_bundle(&mut state, audits);
        Ok((outcome, sequence))
    }

    fn prepare_resolution(
        &self,
        state: &BrokerState,
        request_id: &PermissionRequestId,
        authority: &DecisionAuthority,
        action: PermissionAction,
        grant_scope: Option<&GrantScope>,
    ) -> Result<PreparedResolution, PermissionError> {
        let request = state
            .requests
            .get(request_id)
            .ok_or(PermissionError::RequestNotFound)?
            .request
            .clone();
        if request.state.is_terminal() {
            return Err(PermissionError::AlreadyResolved);
        }
        let requester = self
            .flows
            .lookup_run(&request.requesting_run_id)
            .filter(|identity| !matches!(identity.execution_state(), FlowExecutionState::Terminal))
            .ok_or(PermissionError::ActorNotRunning)?;
        let actor = self.validate_authority(&request, authority, action, grant_scope)?;
        let target = match &request.state {
            PermissionRequestState::Pending { target } => target.clone(),
            PermissionRequestState::Evaluating => {
                return Err(PermissionError::ActorNotAuthorized);
            }
            _ => return Err(PermissionError::AlreadyResolved),
        };
        if !actor_matches_target(&actor, &target) {
            return Err(PermissionError::ActorNotAuthorized);
        }
        let next_target = (action == PermissionAction::Defer
            && !matches!(target, ApprovalTarget::User))
        .then(|| {
            self.next_eligible_target(
                &requester,
                &request.requirement,
                &request.intent.provenance,
                match &target {
                    ApprovalTarget::Flow(run_id) => Some(run_id),
                    ApprovalTarget::User => None,
                },
            )
        });
        Ok(PreparedResolution {
            request,
            actor,
            target,
            next_target,
        })
    }

    fn commit_prepared_resolution(
        &self,
        state: &mut BrokerState,
        prepared: PreparedResolution,
        action: PermissionAction,
        grant_scope: Option<GrantScope>,
        reason: Option<String>,
    ) -> ResolveOutcome {
        let request_id = prepared.request.request_id.clone();
        let decision = PermissionDecision {
            decision_id: PermissionDecisionId::now(),
            request_id: request_id.clone(),
            actor: prepared.actor.clone(),
            action,
            grant_scope: grant_scope.clone(),
            reason: reason.clone(),
            decided_at: Utc::now(),
        };
        let entry = state
            .requests
            .get_mut(&request_id)
            .expect("prepared request exists");
        entry.request.escalation_path.push(EscalationHop {
            target: prepared.target.clone(),
            actor: Some(prepared.actor.clone()),
            action: Some(action),
            reason: reason.clone(),
            at: decision.decided_at,
        });
        if action == PermissionAction::Defer {
            if matches!(prepared.target, ApprovalTarget::User) {
                cancel_entry(
                    entry,
                    "permission.user_defer",
                    reason.unwrap_or_else(|| "user deferred without a fallback".into()),
                    decision.decided_at,
                );
            } else {
                let next_target = prepared
                    .next_target
                    .expect("flow-target defer has a prepared next target");
                entry.request.state = PermissionRequestState::Pending {
                    target: next_target.clone(),
                };
                entry.request.revision += 1;
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
            }
            return ResolveOutcome::Deferred(decision);
        }
        let persistent_grant = if action == PermissionAction::Approve {
            grant_scope
                .clone()
                .filter(|scope| !matches!(scope, GrantScope::CurrentCall))
                .map(|scope| PermissionGrant {
                    grant_id: PermissionGrantId::now(),
                    request_id: request_id.clone(),
                    session_id: prepared.request.session_id,
                    requesting_run_id: prepared.request.requesting_run_id,
                    requirement: prepared.request.requirement,
                    scope,
                    actor: prepared.actor,
                    granted_at: decision.decided_at,
                })
        } else {
            None
        };
        let entry = state
            .requests
            .get_mut(&request_id)
            .expect("prepared request exists");
        entry.request.state = if action == PermissionAction::Approve {
            PermissionRequestState::Approved {
                decision_id: decision.decision_id.clone(),
            }
        } else {
            PermissionRequestState::Denied {
                decision_id: decision.decision_id.clone(),
            }
        };
        entry.request.revision += 1;
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
        ResolveOutcome::Resolved(decision)
    }

    fn resolve_locked_state(
        &self,
        state: &mut BrokerState,
        request_id: &PermissionRequestId,
        authority: &DecisionAuthority,
        action: PermissionAction,
        grant_scope: Option<GrantScope>,
        reason: Option<String>,
    ) -> Result<ResolveOutcome, PermissionError> {
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
            cancel_entry(
                state.requests.get_mut(request_id).unwrap(),
                "permission.requester_terminal",
                reason,
                Utc::now(),
            );
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
                cancel_entry(entry, "permission.user_defer", reason, decision.decided_at);
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
        entry.request.revision += 1;
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
        let (deferred, sequence) = self.flows.with_lifecycle_arbitration(|| {
            let mut state = self.state.lock().unwrap();
            let (deferred, audits) = self.defer_unavailable_target_state(
                &mut state,
                request_id,
                expected_target,
                "permission.parent_timeout",
                "ancestor offer timed out",
            )?;
            Ok((deferred, Self::enqueue_audit_bundle(&mut state, audits)))
        })?;
        self.dispatch_audit_bundle(sequence);
        Ok(deferred)
    }

    fn defer_unavailable_target_state(
        &self,
        state: &mut BrokerState,
        request_id: &PermissionRequestId,
        expected_target: &FlowRunId,
        component: &str,
        reason: &str,
    ) -> Result<(bool, Vec<crate::permission_audit::PermissionAuditRecord>), PermissionError> {
        let entry = state
            .requests
            .get_mut(request_id)
            .ok_or(PermissionError::RequestNotFound)?;
        if !matches!(
            &entry.request.state,
            PermissionRequestState::Pending { target: ApprovalTarget::Flow(run_id) }
                if run_id == expected_target
        ) {
            return Ok((false, Vec::new()));
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
        entry.request.revision += 1;
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
        let audit = request_audit_from_state(state, request_id, None, now)
            .expect("deferred request exists");
        Ok((
            true,
            vec![
                crate::permission_audit::PermissionAuditRecord::RequestDeferred(audit.clone()),
                crate::permission_audit::PermissionAuditRecord::RequestTargeted(audit),
            ],
        ))
    }

    pub fn cancel(
        &self,
        request_id: &PermissionRequestId,
        reason: impl Into<String>,
    ) -> Result<(), PermissionError> {
        let sequence = {
            let mut state = self.state.lock().unwrap();
            let at = Utc::now();
            let request_ids = BTreeSet::from([request_id.clone()]);
            let candidate_groups = Self::unresolved_groups_for_requests(&state, &request_ids);
            let entry = state
                .requests
                .get_mut(request_id)
                .ok_or(PermissionError::RequestNotFound)?;
            if entry.request.state.is_terminal() || entry.responder.is_none() {
                return Err(PermissionError::AlreadyResolved);
            }
            cancel_entry(entry, "permission.user_cancel", reason.into(), at);
            let audit = request_audit_from_state(&state, request_id, None, at)
                .expect("cancelled request exists");
            let mut audits =
                vec![crate::permission_audit::PermissionAuditRecord::RequestCancelled(audit)];
            Self::append_resolved_group_audits(&mut state, &candidate_groups, &mut audits, at);
            Self::enqueue_audit_bundle(&mut state, audits)
        };
        self.dispatch_audit_bundle(sequence);
        Ok(())
    }

    pub fn cancel_for_run(&self, session_id: &str, run_id: &FlowRunId, reason: &str) -> usize {
        let (cancelled, sequence) = self.flows.with_lifecycle_arbitration(|| {
            let mut state = self.state.lock().unwrap();
            let (cancelled, audits) =
                self.cancel_for_run_state(&mut state, session_id, run_id, reason);
            (cancelled, Self::enqueue_audit_bundle(&mut state, audits))
        });
        self.dispatch_audit_bundle(sequence);
        cancelled
    }

    fn handle_terminal_locked(&self, session_id: &str, run_id: &FlowRunId) -> u64 {
        let mut state = self.state.lock().unwrap();
        let (_, mut audits) = self.cancel_for_run_state(
            &mut state,
            session_id,
            run_id,
            "requesting flow is terminal",
        );
        let request_ids: Vec<_> = state
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
            if let Ok((_, deferred_audits)) = self.defer_unavailable_target_state(
                &mut state,
                &request_id,
                run_id,
                "permission.target_terminal",
                "target flow became terminal",
            ) {
                audits.extend(deferred_audits);
            }
        }
        Self::enqueue_audit_bundle(&mut state, audits)
    }

    fn cancel_for_run_state(
        &self,
        state: &mut BrokerState,
        session_id: &str,
        run_id: &FlowRunId,
        reason: &str,
    ) -> (usize, Vec<crate::permission_audit::PermissionAuditRecord>) {
        let now = Utc::now();
        let request_ids: Vec<_> = state
            .requests
            .iter()
            .filter(|(_, entry)| {
                entry.request.session_id == session_id
                    && entry.request.requesting_run_id == *run_id
                    && !entry.request.state.is_terminal()
                    && entry.responder.is_some()
            })
            .map(|(request_id, _)| request_id.clone())
            .collect();
        let request_id_set = request_ids.iter().cloned().collect();
        let candidate_groups = Self::unresolved_groups_for_requests(state, &request_id_set);
        for request_id in &request_ids {
            cancel_entry(
                state.requests.get_mut(request_id).expect("request exists"),
                "permission.run_cleanup",
                reason.to_owned(),
                now,
            );
        }
        let audits = request_ids
            .iter()
            .map(|request_id| {
                request_audit_from_state(state, request_id, None, now)
                    .expect("cancelled request exists")
            })
            .collect::<Vec<_>>();
        let mut expired_grants = Vec::new();
        state.grants.retain(|grant| {
            let remove = grant.session_id == session_id && grant.requesting_run_id == *run_id;
            if remove {
                expired_grants.push(grant.clone());
            }
            !remove
        });
        let cancelled = audits.len();
        let mut records = audits
            .into_iter()
            .map(crate::permission_audit::PermissionAuditRecord::RequestCancelled)
            .collect::<Vec<_>>();
        records.extend(expired_grants.into_iter().map(|grant| {
            crate::permission_audit::PermissionAuditRecord::GrantExpired(
                crate::permission_audit::PermissionGrantAudit::from_grant_with_actor(
                    &grant,
                    crate::permission_audit::PermissionProjectionActor::System {
                        component: "permission.run_cleanup".into(),
                    },
                    Some(reason.to_owned()),
                    now,
                ),
            )
        }));
        Self::append_resolved_group_audits(state, &candidate_groups, &mut records, now);
        (cancelled, records)
    }

    pub fn expire_terminal(&self, reason: &str) -> usize {
        let (expired, sequence) = self.flows.with_lifecycle_arbitration(|| {
            let mut state = self.state.lock().unwrap();
            let terminal_runs: HashSet<_> = state
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
                .collect();
            let mut expired = 0;
            let mut audits = Vec::new();
            for (session_id, run_id) in &terminal_runs {
                let (run_expired, run_audits) =
                    self.cancel_for_run_state(&mut state, session_id, run_id, reason);
                expired += run_expired;
                audits.extend(run_audits);
            }
            (expired, Self::enqueue_audit_bundle(&mut state, audits))
        });
        self.dispatch_audit_bundle(sequence);
        expired
    }

    pub fn expire_before(&self, deadline: DateTime<Utc>, reason: &str) -> usize {
        let (expired, sequence) = {
            let mut state = self.state.lock().unwrap();
            let now = Utc::now();
            let request_ids: Vec<_> = state
                .requests
                .iter()
                .filter(|(_, entry)| {
                    entry.request.requested_at <= deadline
                        && !entry.request.state.is_terminal()
                        && entry.responder.is_some()
                })
                .map(|(request_id, _)| request_id.clone())
                .collect();
            let request_id_set = request_ids.iter().cloned().collect();
            let candidate_groups = Self::unresolved_groups_for_requests(&state, &request_id_set);
            for request_id in &request_ids {
                cancel_entry(
                    state.requests.get_mut(request_id).expect("request exists"),
                    "permission.expiry",
                    reason.to_owned(),
                    now,
                );
            }
            let mut audits = request_ids
                .iter()
                .filter_map(|request_id| request_audit_from_state(&state, request_id, None, now))
                .map(crate::permission_audit::PermissionAuditRecord::RequestCancelled)
                .collect::<Vec<_>>();
            let expired = audits.len();
            Self::append_resolved_group_audits(&mut state, &candidate_groups, &mut audits, now);
            (expired, Self::enqueue_audit_bundle(&mut state, audits))
        };
        self.dispatch_audit_bundle(sequence);
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

    pub fn user_list(&self, session_id: &str) -> (Vec<PermissionRequest>, Vec<PermissionGroup>) {
        let state = self.state.lock().unwrap();
        let requests: Vec<_> = state
            .requests
            .values()
            .filter(|entry| Self::user_visible_request(&entry.request, session_id))
            .map(|entry| entry.request.clone())
            .collect();
        let groups: Vec<_> = state
            .groups
            .values()
            .filter(|group| group.owner == GroupOwner::User)
            .filter(|group| {
                !group.request_ids.is_empty()
                    && group.request_ids.iter().all(|request_id| {
                        state.requests.get(request_id).is_some_and(|entry| {
                            Self::user_visible_request(&entry.request, session_id)
                        })
                    })
            })
            .cloned()
            .collect();
        (requests, groups)
    }

    pub fn user_create_group(
        &self,
        session_id: &str,
        request_ids: BTreeSet<PermissionRequestId>,
        label: String,
        expected_revisions: &HashMap<PermissionRequestId, u64>,
    ) -> Result<PermissionGroup, PermissionError> {
        if request_ids.is_empty() {
            return Err(PermissionError::EmptyGroup);
        }
        let mut state = self.state.lock().unwrap();
        if request_ids.iter().any(|id| {
            state.requests.get(id).is_none_or(|entry| {
                !Self::user_visible_request(&entry.request, session_id)
                    || expected_revisions.get(id) != Some(&entry.request.revision)
            })
        }) {
            return Err(PermissionError::GroupRevisionConflict);
        }
        let group = PermissionGroup {
            group_id: PermissionGroupId::now(),
            owner: GroupOwner::User,
            label,
            request_ids,
            created_at: Utc::now(),
            revision: 0,
        };
        state.groups.insert(group.group_id.clone(), group.clone());
        let records = vec![
            crate::permission_audit::PermissionAuditRecord::GroupCreated(
                crate::permission_audit::PermissionGroupAudit::from_group(
                    &group,
                    session_id,
                    group.created_at,
                ),
            ),
        ];
        let sequence = Self::enqueue_audit_bundle(&mut state, records);
        drop(state);
        self.dispatch_audit_bundle(sequence);
        Ok(group)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn user_resolve(
        &self,
        session_id: &str,
        principal_id: Option<String>,
        request_ids: Vec<PermissionRequestId>,
        expected_revisions: &HashMap<PermissionRequestId, u64>,
        group_id: Option<(PermissionGroupId, u64)>,
        action: PermissionAction,
        grant_scope: Option<GrantScope>,
        reason: Option<String>,
    ) -> Result<Vec<BatchResolution>, PermissionError> {
        let (results, sequence) = self.flows.with_lifecycle_arbitration(|| {
            let mut state = self.state.lock().unwrap();
            let is_group_selector = group_id.is_some();
            let ids = if let Some((group_id, expected_revision)) = group_id {
                let group = state
                    .groups
                    .get(&group_id)
                    .ok_or(PermissionError::GroupNotFound)?;
                if group.owner != GroupOwner::User || group.revision != expected_revision {
                    return Err(PermissionError::GroupRevisionConflict);
                }
                group.request_ids.iter().cloned().collect()
            } else {
                request_ids
            };
            if ids.iter().any(|id| {
                state.requests.get(id).is_none_or(|entry| {
                    !Self::user_visible_request(&entry.request, session_id)
                        || (!is_group_selector
                            && expected_revisions.get(id) != Some(&entry.request.revision))
                })
            }) {
                return Err(PermissionError::GroupRevisionConflict);
            }
            let authority =
                DecisionAuthority::User(self.user_authority(session_id.to_string(), principal_id));
            let request_set = ids.iter().cloned().collect();
            let candidate_groups = Self::unresolved_groups_for_requests(&state, &request_set);
            let mut results = Vec::with_capacity(ids.len());
            let mut audits = Vec::new();
            for id in ids {
                let outcome = self.resolve_locked_state(
                    &mut state,
                    &id,
                    &authority,
                    action,
                    grant_scope.clone(),
                    reason.clone(),
                )?;
                let decision = match &outcome {
                    ResolveOutcome::Resolved(decision) | ResolveOutcome::Deferred(decision) => {
                        decision
                    }
                };
                audits.extend(resolution_audits_from_state(&state, &id, decision));
                results.push(BatchResolution {
                    request_id: id,
                    outcome: batch_outcome(action, outcome),
                });
            }
            Self::append_resolved_group_audits(
                &mut state,
                &candidate_groups,
                &mut audits,
                Utc::now(),
            );
            Ok((results, Self::enqueue_audit_bundle(&mut state, audits)))
        })?;
        self.dispatch_audit_bundle(sequence);
        Ok(results)
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
            state.requests.get(request_id).is_none_or(|entry| {
                !(self.visible_to(actor, &entry.request)
                    || entry.request.state.is_terminal()
                        && entry.request.session_id == actor.session_id
                        && self
                            .flows
                            .is_strict_ancestor(&actor.run_id, &entry.request.requesting_run_id))
            })
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
        let mut records = vec![
            crate::permission_audit::PermissionAuditRecord::GroupCreated(
                crate::permission_audit::PermissionGroupAudit::from_group(
                    &group,
                    &actor.session_id,
                    group.created_at,
                ),
            ),
        ];
        Self::append_resolved_group_audits(
            &mut state,
            &BTreeSet::from([group.group_id.clone()]),
            &mut records,
            group.created_at,
        );
        let sequence = Self::enqueue_audit_bundle(&mut state, records);
        drop(state);
        self.dispatch_audit_bundle(sequence);
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
        let group = group.clone();
        let at = Utc::now();
        let mut records = vec![
            crate::permission_audit::PermissionAuditRecord::GroupUpdated(
                crate::permission_audit::PermissionGroupAudit::from_group(
                    &group,
                    &actor.session_id,
                    at,
                ),
            ),
        ];
        Self::append_resolved_group_audits(
            &mut state,
            &BTreeSet::from([group.group_id.clone()]),
            &mut records,
            at,
        );
        let sequence = Self::enqueue_audit_bundle(&mut state, records);
        drop(state);
        self.dispatch_audit_bundle(sequence);
        Ok(group)
    }

    fn expand_selector(
        &self,
        state: &BrokerState,
        actor: &FlowIdentity,
        selector: &PermissionSelector,
        expected_group_revision: Option<u64>,
    ) -> Result<Vec<PermissionRequestId>, PermissionError> {
        let mut ids = BTreeSet::new();
        match selector {
            PermissionSelector::RequestIds(request_ids) => ids.extend(request_ids.iter().cloned()),
            PermissionSelector::Group(group_id) => {
                let group = state
                    .groups
                    .get(group_id)
                    .ok_or(PermissionError::GroupNotFound)?;
                if group.owner != GroupOwner::Flow(actor.run_id.clone()) {
                    return Err(PermissionError::GroupNotOwner);
                }
                if expected_group_revision.is_some_and(|revision| revision != group.revision) {
                    return Err(PermissionError::GroupRevisionConflict);
                }
                ids.extend(group.request_ids.iter().cloned());
            }
            PermissionSelector::DescendantRun(run_id) => {
                ids.extend(
                    state
                        .requests
                        .values()
                        .filter(|entry| {
                            let request = &entry.request;
                            request.session_id == actor.session_id
                                && request.requesting_run_id == *run_id
                                && self.flows.is_strict_ancestor(&actor.run_id, run_id)
                        })
                        .map(|entry| entry.request.request_id.clone()),
                );
            }
            selector => {
                ids.extend(state.requests.values().filter(|entry| {
                    let request = &entry.request;
                    if request.session_id != actor.session_id || !self.visible_to(actor, request) {
                        return false;
                    }
                    match selector {
                        PermissionSelector::ChildRun => request.parent_run_id.as_ref() == Some(&actor.run_id),
                        PermissionSelector::Tool(tool) => &request.intent.tool_name == tool,
                        PermissionSelector::Tier(tier) => request.intent.tier == *tier,
                        PermissionSelector::Risk(risk) => request.intent.risks.contains(risk),
                        PermissionSelector::PathPrefix(prefix) => request
                            .intent
                            .provenance
                            .authorized_targets()
                            .any(|path| path.starts_with(prefix)),
                        PermissionSelector::Target(target) => matches!(&request.state, PermissionRequestState::Pending { target: current } if current == target),
                        _ => false,
                    }
                }).map(|entry| entry.request.request_id.clone()));
            }
        }
        Ok(ids.into_iter().collect())
    }

    /// Resolve a stable selector, deduplicating request IDs deterministically.
    #[allow(clippy::too_many_arguments)]
    pub fn resolve_batch(
        &self,
        actor: &Arc<FlowIdentity>,
        selector: PermissionSelector,
        action: PermissionAction,
        grant_scope: Option<GrantScope>,
        reason: Option<String>,
        mode: BatchMode,
        expected_group_revision: Option<u64>,
    ) -> Result<Vec<BatchResolution>, PermissionError> {
        let (results, sequence) = self.flows.with_lifecycle_arbitration(|| {
            self.authenticate_control_actor(actor)?;
            let authority = DecisionAuthority::Flow(self.flow_authority(Arc::clone(actor))?);
            let mut state = self.state.lock().unwrap();
            let ids = self.expand_selector(&state, actor, &selector, expected_group_revision)?;
            let request_ids = ids.iter().cloned().collect();
            let candidate_groups = Self::unresolved_groups_for_requests(&state, &request_ids);
            let mut prepared = if mode == BatchMode::Atomic {
                Some(
                    ids.iter()
                        .map(|id| {
                            self.prepare_resolution(
                                &state,
                                id,
                                &authority,
                                action,
                                grant_scope.as_ref(),
                            )
                        })
                        .collect::<Result<Vec<_>, _>>()?
                        .into_iter(),
                )
            } else {
                None
            };
            let mut results = Vec::with_capacity(ids.len());
            let mut audits = Vec::new();
            for request_id in ids {
                let outcome = if let Some(prepared) = &mut prepared {
                    Ok(self.commit_prepared_resolution(
                        &mut state,
                        prepared
                            .next()
                            .expect("one prepared resolution per request"),
                        action,
                        grant_scope.clone(),
                        reason.clone(),
                    ))
                } else {
                    self.resolve_locked_state(
                        &mut state,
                        &request_id,
                        &authority,
                        action,
                        grant_scope.clone(),
                        reason.clone(),
                    )
                };
                let batch = match outcome {
                    Ok(outcome) => {
                        let decision = match &outcome {
                            ResolveOutcome::Resolved(decision)
                            | ResolveOutcome::Deferred(decision) => decision,
                        };
                        audits.extend(resolution_audits_from_state(&state, &request_id, decision));
                        batch_outcome(action, outcome)
                    }
                    Err(error) if mode == BatchMode::BestEffort => batch_error_outcome(error),
                    Err(error) => return Err(error),
                };
                results.push(BatchResolution {
                    request_id,
                    outcome: batch,
                });
            }
            Self::append_resolved_group_audits(
                &mut state,
                &candidate_groups,
                &mut audits,
                Utc::now(),
            );
            let sequence = Self::enqueue_audit_bundle(&mut state, audits);
            Ok((results, sequence))
        })?;
        self.dispatch_audit_bundle(sequence);
        Ok(results)
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
        let group = state.groups.get(group_id).expect("group exists").clone();
        let mut records = Vec::new();
        Self::append_resolved_group_audits(
            &mut state,
            &BTreeSet::from([group_id.clone()]),
            &mut records,
            Utc::now(),
        );
        state.groups.remove(group_id).expect("group exists");
        let sequence = Self::enqueue_audit_bundle(&mut state, records);
        drop(state);
        self.dispatch_audit_bundle(sequence);
        Ok(group)
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

    fn user_visible_request(request: &PermissionRequest, session_id: &str) -> bool {
        request.session_id == session_id
            && matches!(
                &request.state,
                PermissionRequestState::Pending {
                    target: ApprovalTarget::User
                }
            )
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
            let state = self.state.lock().unwrap();
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
}

fn request_audit_from_state(
    state: &BrokerState,
    request_id: &PermissionRequestId,
    decision: Option<&PermissionDecision>,
    at: DateTime<Utc>,
) -> Option<crate::permission_audit::PermissionRequestAudit> {
    let request = &state.requests.get(request_id)?.request;
    let group_ids = state
        .groups
        .values()
        .filter(|group| group.request_ids.contains(request_id))
        .map(|group| group.group_id.clone())
        .collect();
    Some(
        crate::permission_audit::PermissionRequestAudit::from_request(
            request, group_ids, decision, at,
        ),
    )
}

fn submission_audits(
    request: &PermissionRequest,
    authorization: Option<&ImmediateAuthorization>,
    at: DateTime<Utc>,
) -> Vec<crate::permission_audit::PermissionAuditRecord> {
    let decision = authorization.map(|authorization| PermissionDecision {
        decision_id: match &request.state {
            PermissionRequestState::Approved { decision_id }
            | PermissionRequestState::Denied { decision_id } => decision_id.clone(),
            _ => unreachable!("immediate request is terminal"),
        },
        request_id: request.request_id.clone(),
        actor: match authorization {
            ImmediateAuthorization::Granted { grant } => grant.actor.clone(),
            _ => DecisionActor::Policy {
                policy_version: request.policy_reference.snapshot_id.clone(),
                rule_id: request.policy_reference.rule_id.clone(),
            },
        },
        action: if matches!(authorization, ImmediateAuthorization::Denied { .. }) {
            PermissionAction::Deny
        } else {
            PermissionAction::Approve
        },
        grant_scope: match authorization {
            ImmediateAuthorization::Granted { grant } => Some(grant.scope.clone()),
            _ => None,
        },
        reason: match authorization {
            ImmediateAuthorization::Denied { reason } => Some(reason.clone()),
            _ => None,
        },
        decided_at: at,
    });
    let audit = crate::permission_audit::PermissionRequestAudit::from_request(
        request,
        Vec::new(),
        decision.as_ref(),
        at,
    );
    let mut records = vec![
        crate::permission_audit::PermissionAuditRecord::RequestCreated(audit.clone()),
        crate::permission_audit::PermissionAuditRecord::RequestTargeted(audit.clone()),
    ];
    if let Some(authorization) = authorization {
        records.push(match authorization {
            ImmediateAuthorization::Denied { .. } => {
                crate::permission_audit::PermissionAuditRecord::RequestDenied(audit)
            }
            ImmediateAuthorization::Unrestricted => {
                crate::permission_audit::PermissionAuditRecord::UnrestrictedExecution(audit)
            }
            _ => crate::permission_audit::PermissionAuditRecord::RequestApproved(audit),
        });
    }
    records
}

fn resolution_audits_from_state(
    state: &BrokerState,
    request_id: &PermissionRequestId,
    decision: &PermissionDecision,
) -> Vec<crate::permission_audit::PermissionAuditRecord> {
    let Some(entry) = state.requests.get(request_id) else {
        return Vec::new();
    };
    let Some(audit) =
        request_audit_from_state(state, request_id, Some(decision), decision.decided_at)
    else {
        return Vec::new();
    };
    let cancelled = matches!(
        entry.request.state,
        PermissionRequestState::Cancelled { .. }
    );
    let grant = state
        .grants
        .iter()
        .find(|grant| grant.request_id == *request_id && grant.granted_at == decision.decided_at);
    let mut records = vec![match decision.action {
        PermissionAction::Approve => {
            crate::permission_audit::PermissionAuditRecord::RequestApproved(audit)
        }
        PermissionAction::Deny => {
            crate::permission_audit::PermissionAuditRecord::RequestDenied(audit)
        }
        PermissionAction::Defer if cancelled => {
            crate::permission_audit::PermissionAuditRecord::RequestCancelled(audit)
        }
        PermissionAction::Defer => {
            crate::permission_audit::PermissionAuditRecord::RequestDeferred(audit)
        }
    }];
    if decision.action == PermissionAction::Defer
        && !cancelled
        && let Some(mut targeted) =
            request_audit_from_state(state, request_id, None, decision.decided_at)
    {
        targeted.actor = None;
        records.push(crate::permission_audit::PermissionAuditRecord::RequestTargeted(targeted));
    }
    if let Some(grant) = grant {
        records.push(
            crate::permission_audit::PermissionAuditRecord::GrantCreated(
                crate::permission_audit::PermissionGrantAudit::from_grant(
                    grant,
                    decision.reason.clone(),
                    decision.decided_at,
                ),
            ),
        );
    }
    records
}

fn batch_outcome(action: PermissionAction, outcome: ResolveOutcome) -> BatchRequestOutcome {
    let decision = match outcome {
        ResolveOutcome::Resolved(decision) | ResolveOutcome::Deferred(decision) => decision,
    };
    match action {
        PermissionAction::Approve => BatchRequestOutcome::Approved(decision),
        PermissionAction::Deny => BatchRequestOutcome::Denied(decision),
        PermissionAction::Defer => BatchRequestOutcome::Deferred(decision),
    }
}

fn batch_error_outcome(error: PermissionError) -> BatchRequestOutcome {
    match error {
        PermissionError::AlreadyResolved => BatchRequestOutcome::SkippedAlreadyResolved,
        PermissionError::ActorNotAuthorized => BatchRequestOutcome::RejectedNotAncestor,
        PermissionError::GrantExceedsAuthority => BatchRequestOutcome::RejectedOverAuthority,
        PermissionError::RequestNotFound => BatchRequestOutcome::RejectedNotFound,
        PermissionError::ActorNotRunning => BatchRequestOutcome::RejectedNotRunning,
        PermissionError::IdentityMismatch => BatchRequestOutcome::RejectedStale,
        PermissionError::PermissionManagementRequired => {
            BatchRequestOutcome::RejectedPermissionManagementRequired
        }
        PermissionError::UnsupportedGrantScope => {
            BatchRequestOutcome::RejectedUnsupportedGrantScope
        }
        PermissionError::NoEscalationTarget => BatchRequestOutcome::RejectedNoEscalationTarget,
        error => BatchRequestOutcome::Rejected(error),
    }
}

fn cancel_entry(entry: &mut RequestEntry, component: &str, reason: String, at: DateTime<Utc>) {
    let target = match &entry.request.state {
        PermissionRequestState::Pending { target } => target.clone(),
        _ => ApprovalTarget::User,
    };
    entry.request.escalation_path.push(EscalationHop {
        target,
        actor: Some(DecisionActor::System {
            component: component.into(),
        }),
        action: None,
        reason: Some(reason.clone()),
        at,
    });
    entry.request.state = PermissionRequestState::Cancelled {
        reason: reason.clone(),
    };
    entry.request.revision += 1;
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
    use crate::stream::StreamFrame;
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
        submit_to_flow_with_intent(broker, requester, target, intent())
    }

    fn submit_to_flow_with_intent(
        broker: &PermissionBroker,
        requester: &FlowIdentity,
        target: Arc<FlowIdentity>,
        intent: PermissionIntent,
    ) -> Result<SubmissionOutcome, PermissionError> {
        let target = ApprovalAuthority::Flow(broker.flow_authority(target)?);
        broker.submit_to(
            Some(&requester.session_id),
            Some(&requester.run_id),
            intent,
            false,
            target,
            &ask_policy(),
        )
    }

    fn set_blocked_on_child(parent: &FlowIdentity, child: &FlowIdentity) {
        *parent.execution_state.lock().unwrap() = FlowExecutionState::BlockedOnDescendants {
            child_run_counts: HashMap::from([(child.run_id.clone(), 1)]),
        };
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

    fn submit_to_user(broker: &PermissionBroker, requester: &FlowIdentity) -> PendingPermission {
        let target = ApprovalAuthority::User(broker.user_authority(&requester.session_id, None));
        let SubmissionOutcome::Pending(pending) = broker
            .submit_to(
                Some(&requester.session_id),
                Some(&requester.run_id),
                intent(),
                false,
                target,
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

    fn attach_audit(broker: &PermissionBroker) -> tokio::sync::broadcast::Receiver<StreamFrame> {
        let (stream, receiver) = tokio::sync::broadcast::channel(64);
        broker.set_audit_projector(crate::permission_audit::PermissionAuditProjector::new(
            crate::event::EventSink::new(),
            stream,
        ));
        receiver
    }

    fn drain_audit(
        receiver: &mut tokio::sync::broadcast::Receiver<StreamFrame>,
    ) -> Vec<StreamFrame> {
        std::iter::from_fn(|| receiver.try_recv().ok()).collect()
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
    fn running_sync_parent_remains_an_escalation_target() {
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

        let pending = submit_user(&broker, &requester, intent());
        assert!(matches!(
            pending.request.state,
            PermissionRequestState::Pending {
                target: ApprovalTarget::Flow(ref run_id)
            } if run_id == &sync_parent.run_id
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
        set_blocked_on_child(&sync_parent, &requester);
        let broker = PermissionBroker::new(Arc::clone(&flows));

        let pending = submit_user(&broker, &requester, intent());
        assert!(matches!(
            pending.request.state,
            PermissionRequestState::Pending {
                target: ApprovalTarget::Flow(ref run_id)
            } if run_id == &root.run_id
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
    fn s8_deterministic_approve_terminal_interleavings_preserve_state_and_audit_order() {
        enum Schedule {
            ApproveThenTerminal,
            TerminalThenApprove,
        }
        for schedule in [Schedule::ApproveThenTerminal, Schedule::TerminalThenApprove] {
            let flows = Arc::new(FlowRegistry::default());
            let requester = register_root(&flows, "session", false);
            let broker = PermissionBroker::shared(Arc::clone(&flows));
            let mut audit = attach_audit(&broker);
            let pending = submit_to_user(&broker, &requester);
            drain_audit(&mut audit);
            let request_id = pending.request.request_id.clone();
            let scope = GrantScope::ChildRunSameTool {
                run_id: requester.run_id.clone(),
                tool_name: pending.request.intent.tool_name.clone(),
            };
            let (first_done_tx, first_done_rx) = std::sync::mpsc::channel();
            let (continue_tx, continue_rx) = std::sync::mpsc::channel();

            let approver = {
                let broker = Arc::clone(&broker);
                let authority = user_decision(&broker, "session");
                let first_done_tx = first_done_tx.clone();
                let approve_first = matches!(schedule, Schedule::ApproveThenTerminal);
                std::thread::spawn(move || {
                    if !approve_first {
                        continue_rx.recv().unwrap();
                    }
                    let outcome = broker.resolve(
                        &request_id,
                        &authority,
                        PermissionAction::Approve,
                        Some(scope),
                        Some("accepted".into()),
                    );
                    if approve_first {
                        first_done_tx.send(()).unwrap();
                    }
                    outcome
                })
            };
            let terminator = {
                let flows = Arc::clone(&flows);
                let run_id = requester.run_id.clone();
                let first_done_tx = first_done_tx;
                let approve_first = matches!(schedule, Schedule::ApproveThenTerminal);
                std::thread::spawn(move || {
                    if approve_first {
                        first_done_rx.recv().unwrap();
                    }
                    flows.mark_terminal(&run_id);
                    if !approve_first {
                        first_done_tx.send(()).unwrap();
                        continue_tx.send(()).unwrap();
                    }
                })
            };

            let approval = approver.join().unwrap();
            terminator.join().unwrap();
            assert!(broker.grants().is_empty());
            let frames = drain_audit(&mut audit);
            match schedule {
                Schedule::ApproveThenTerminal => {
                    assert!(matches!(approval, Ok(ResolveOutcome::Resolved(_))));
                    assert!(matches!(
                        pending.resolution.blocking_recv().unwrap(),
                        PermissionResolution::Decision(_)
                    ));
                    assert!(matches!(
                        broker.get(&pending.request.request_id).unwrap().state,
                        PermissionRequestState::Approved { .. }
                    ));
                    assert!(matches!(
                        frames.as_slice(),
                        [
                            StreamFrame::PermissionRequestApproved { .. },
                            StreamFrame::PermissionGrantCreated { .. },
                            StreamFrame::PermissionGrantExpired { .. }
                        ]
                    ));
                }
                Schedule::TerminalThenApprove => {
                    assert!(matches!(approval, Err(PermissionError::AlreadyResolved)));
                    assert!(matches!(
                        pending.resolution.blocking_recv().unwrap(),
                        PermissionResolution::Cancelled { .. }
                    ));
                    assert!(matches!(
                        broker.get(&pending.request.request_id).unwrap().state,
                        PermissionRequestState::Cancelled { .. }
                    ));
                    assert!(matches!(
                        frames.as_slice(),
                        [StreamFrame::PermissionRequestCancelled { .. }]
                    ));
                }
            }
        }
    }

    #[test]
    fn s8_audit_dispatch_preserves_linearized_bundle_order_when_callers_arrive_out_of_order() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = Arc::new(PermissionBroker::new(flows));
        let mut audit = attach_audit(&broker);
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        broker.set_audit_dispatch_hook({
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            Arc::new(move |sequence| {
                if sequence == 0 {
                    entered.wait();
                    release.wait();
                }
            })
        });

        let first = {
            let broker = Arc::clone(&broker);
            let requester = Arc::clone(&requester);
            std::thread::spawn(move || submit_to_user(&broker, &requester))
        };
        entered.wait();
        let second = submit_to_user(&broker, &requester);
        release.wait();
        let first = first.join().unwrap();

        let frames = drain_audit(&mut audit);
        let expected = [
            first.request.request_id.clone(),
            first.request.request_id,
            second.request.request_id.clone(),
            second.request.request_id,
        ];
        let actual = frames
            .iter()
            .map(|frame| match frame {
                StreamFrame::PermissionRequestCreated { payload, .. }
                | StreamFrame::PermissionRequestTargeted { payload, .. } => {
                    payload.request_id.clone().unwrap()
                }
                _ => panic!("unexpected frame: {frame:?}"),
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
        assert!(matches!(
            frames.as_slice(),
            [
                StreamFrame::PermissionRequestCreated { .. },
                StreamFrame::PermissionRequestTargeted { .. },
                StreamFrame::PermissionRequestCreated { .. },
                StreamFrame::PermissionRequestTargeted { .. }
            ]
        ));
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
        let mut audit = attach_audit(&broker);
        let SubmissionOutcome::Pending(pending) =
            submit_to_flow(&broker, &requester, root.clone()).unwrap()
        else {
            panic!("expected pending");
        };
        drain_audit(&mut audit);
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
        let frames = drain_audit(&mut audit);
        assert!(matches!(
            frames.as_slice(),
            [
                StreamFrame::PermissionRequestDeferred { payload: deferred, .. },
                StreamFrame::PermissionRequestTargeted { payload: targeted, .. }
            ] if deferred.actor == Some(crate::permission_audit::PermissionProjectionActor::Flow {
                session_id: root.session_id.clone(),
                run_id: root.run_id.clone(),
            }) && targeted.actor.is_none()
                && targeted.target == crate::permission_audit::PermissionAuditTarget::User
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
        let mut audit = attach_audit(&broker);
        let SubmissionOutcome::Pending(pending) =
            submit_to_flow(&broker, &requester, middle.clone()).unwrap()
        else {
            panic!("expected pending");
        };
        drain_audit(&mut audit);
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
        assert!(matches!(
            drain_audit(&mut audit).as_slice(),
            [
                StreamFrame::PermissionRequestDeferred { payload: deferred, .. },
                StreamFrame::PermissionRequestTargeted { payload: targeted, .. }
            ] if deferred.request_id == targeted.request_id
                && targeted.target == crate::permission_audit::PermissionAuditTarget::Flow {
                    run_id: root.run_id.clone(),
                }
        ));
    }

    #[test]
    fn s8_timeout_defer_emits_deferred_then_targeted_from_real_broker() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let middle = child(&flows, &root);
        let requester = child(&flows, &middle);
        let broker = PermissionBroker::new(flows);
        let mut audit = attach_audit(&broker);
        let SubmissionOutcome::Pending(pending) =
            submit_to_flow(&broker, &requester, middle.clone()).unwrap()
        else {
            panic!("expected pending");
        };
        drain_audit(&mut audit);
        assert!(
            broker
                .defer_timed_out_target(&pending.request.request_id, &middle.run_id)
                .unwrap()
        );
        assert!(matches!(
            drain_audit(&mut audit).as_slice(),
            [
                StreamFrame::PermissionRequestDeferred { payload: deferred, .. },
                StreamFrame::PermissionRequestTargeted { payload: targeted, .. }
            ] if deferred.request_id == targeted.request_id
                && targeted.target == crate::permission_audit::PermissionAuditTarget::Flow {
                    run_id: root.run_id.clone(),
                }
        ));
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
    fn built_in_batch_selectors_match_stable_request_identity() {
        for case in 0..6 {
            let temp = tempfile::tempdir().unwrap();
            let path = temp.path().join("nested/file.txt");
            let flows = Arc::new(FlowRegistry::default());
            let root = register_root(&flows, "session", true);
            let requester = child(&flows, &root);
            let broker = PermissionBroker::new(flows);
            let selected_intent = PermissionIntent {
                risks: BTreeSet::from([RiskKind::Network]),
                provenance: ResourceProvenance {
                    path: Some(path),
                    ..ResourceProvenance::default()
                },
                ..intent()
            };
            let SubmissionOutcome::Pending(pending) =
                submit_to_flow_with_intent(&broker, &requester, Arc::clone(&root), selected_intent)
                    .unwrap()
            else {
                panic!("expected pending request");
            };
            let selector = match case {
                0 => PermissionSelector::ChildRun,
                1 => PermissionSelector::Tool("bash.spawn".into()),
                2 => PermissionSelector::Tier(Tier::Two),
                3 => PermissionSelector::Risk(RiskKind::Network),
                4 => PermissionSelector::PathPrefix(temp.path().join("nested")),
                5 => PermissionSelector::Target(ApprovalTarget::Flow(root.run_id.clone())),
                _ => unreachable!(),
            };
            let results = broker
                .resolve_batch(
                    &root,
                    selector,
                    PermissionAction::Approve,
                    None,
                    None,
                    BatchMode::BestEffort,
                    None,
                )
                .unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].request_id, pending.request.request_id);
            assert!(matches!(
                results[0].outcome,
                BatchRequestOutcome::Approved(_)
            ));
        }
    }

    #[test]
    fn path_prefix_selector_matches_every_authorized_provenance_target() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = crate::fs_access::canonicalize_stable(temp.path());
        for (case, prefix) in [
            (0, root_path.join("primary")),
            (1, root_path.join("cwd")),
            (2, root_path.join("extra")),
            (3, root_path.join("missing")),
        ] {
            let flows = Arc::new(FlowRegistry::default());
            let root = register_root(&flows, "session", true);
            let requester = child(&flows, &root);
            let broker = PermissionBroker::new(flows);
            let SubmissionOutcome::Pending(pending) = submit_to_flow_with_intent(
                &broker,
                &requester,
                Arc::clone(&root),
                PermissionIntent {
                    provenance: ResourceProvenance {
                        path: Some(root_path.join("primary/file.txt")),
                        cwd: Some(root_path.join("cwd/work")),
                        extra_targets: vec![root_path.join("extra/other.txt")],
                        ..ResourceProvenance::default()
                    },
                    ..intent()
                },
            )
            .unwrap() else {
                panic!("expected pending request");
            };

            let results = broker
                .resolve_batch(
                    &root,
                    PermissionSelector::PathPrefix(prefix),
                    PermissionAction::Approve,
                    None,
                    None,
                    BatchMode::BestEffort,
                    None,
                )
                .unwrap();
            if case == 3 {
                assert!(results.is_empty());
                assert!(matches!(
                    broker.get(&pending.request.request_id).unwrap().state,
                    PermissionRequestState::Pending { .. }
                ));
            } else {
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].request_id, pending.request.request_id);
                assert!(matches!(
                    results[0].outcome,
                    BatchRequestOutcome::Approved(_)
                ));
            }
        }
    }

    #[test]
    fn best_effort_batch_preserves_prior_success_before_later_member_rejection() {
        let temp = tempfile::tempdir().unwrap();
        let root_path = crate::fs_access::canonicalize_stable(temp.path());
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let requester = child(&flows, &root);
        let broker = PermissionBroker::new(flows);
        let SubmissionOutcome::Pending(first) = submit_to_flow_with_intent(
            &broker,
            &requester,
            Arc::clone(&root),
            PermissionIntent {
                provenance: ResourceProvenance {
                    path: Some(root_path.join("first.txt")),
                    path_origin: Some(PathOrigin::ExplicitInside),
                    workspace_root: Some(root_path),
                    ..ResourceProvenance::default()
                },
                ..intent()
            },
        )
        .unwrap() else {
            panic!("expected pending request");
        };
        let SubmissionOutcome::Pending(second) =
            submit_to_flow(&broker, &requester, Arc::clone(&root)).unwrap()
        else {
            panic!("expected pending request");
        };
        let scope = GrantScope::ChildRunSamePathRule {
            run_id: requester.run_id.clone(),
            tool_name: "bash.spawn".into(),
            workspace_relative_path: "first.txt".into(),
        };

        let results = broker
            .resolve_batch(
                &root,
                PermissionSelector::RequestIds(vec![
                    first.request.request_id.clone(),
                    second.request.request_id.clone(),
                ]),
                PermissionAction::Approve,
                Some(scope),
                None,
                BatchMode::BestEffort,
                None,
            )
            .unwrap();

        assert_eq!(results.len(), 2);
        assert_eq!(results[0].request_id, first.request.request_id);
        assert!(matches!(
            results[0].outcome,
            BatchRequestOutcome::Approved(_)
        ));
        assert_eq!(results[1].request_id, second.request.request_id);
        assert!(matches!(
            results[1].outcome,
            BatchRequestOutcome::RejectedUnsupportedGrantScope
        ));
        assert!(matches!(
            broker.get(&first.request.request_id).unwrap().state,
            PermissionRequestState::Approved { .. }
        ));
        assert!(matches!(
            broker.get(&second.request.request_id).unwrap().state,
            PermissionRequestState::Pending { .. }
        ));
    }

    #[test]
    fn best_effort_batch_reports_mixed_outcomes_and_independent_decisions() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let requesters = [child(&flows, &root), child(&flows, &root)];
        let broker = PermissionBroker::new(flows);
        let pending = requesters.map(|requester| {
            let SubmissionOutcome::Pending(pending) =
                submit_to_flow(&broker, &requester, Arc::clone(&root)).unwrap()
            else {
                panic!("expected pending request");
            };
            *pending
        });
        broker
            .resolve(
                &pending[0].request.request_id,
                &DecisionAuthority::Flow(broker.flow_authority(Arc::clone(&root)).unwrap()),
                PermissionAction::Approve,
                None,
                None,
            )
            .unwrap();
        let missing = PermissionRequestId::now();
        let results = broker
            .resolve_batch(
                &root,
                PermissionSelector::RequestIds(vec![
                    pending[0].request.request_id.clone(),
                    pending[1].request.request_id.clone(),
                    missing.clone(),
                ]),
                PermissionAction::Approve,
                None,
                None,
                BatchMode::BestEffort,
                None,
            )
            .unwrap();
        assert!(
            results
                .iter()
                .any(|result| result.request_id == pending[0].request.request_id
                    && matches!(result.outcome, BatchRequestOutcome::SkippedAlreadyResolved))
        );
        assert!(
            results
                .iter()
                .any(|result| result.request_id == pending[1].request.request_id
                    && matches!(result.outcome, BatchRequestOutcome::Approved(_)))
        );
        assert!(results.iter().any(|result| result.request_id == missing
            && matches!(result.outcome, BatchRequestOutcome::RejectedNotFound)));
        let decision_id =
            |request_id: &PermissionRequestId| match broker.get(request_id).unwrap().state {
                PermissionRequestState::Approved { decision_id, .. } => decision_id,
                state => panic!("unexpected state: {state:?}"),
            };
        assert_ne!(
            decision_id(&pending[0].request.request_id),
            decision_id(&pending[1].request.request_id)
        );
    }

    #[test]
    fn atomic_batch_rolls_back_and_group_revision_is_checked() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let requester = child(&flows, &root);
        let broker = PermissionBroker::new(flows);
        let SubmissionOutcome::Pending(pending) =
            submit_to_flow(&broker, &requester, Arc::clone(&root)).unwrap()
        else {
            panic!("expected pending request");
        };
        let request_id = pending.request.request_id.clone();
        assert!(matches!(
            broker.resolve_batch(
                &root,
                PermissionSelector::RequestIds(vec![
                    request_id.clone(),
                    PermissionRequestId::now()
                ]),
                PermissionAction::Approve,
                None,
                None,
                BatchMode::Atomic,
                None,
            ),
            Err(PermissionError::RequestNotFound)
        ));
        assert!(matches!(
            broker.get(&request_id).unwrap().state,
            PermissionRequestState::Pending { .. }
        ));
        let group = broker
            .create_group(&root, BTreeSet::from([request_id]), "batch".into())
            .unwrap();
        assert!(matches!(
            broker.resolve_batch(
                &root,
                PermissionSelector::Group(group.group_id),
                PermissionAction::Approve,
                None,
                None,
                BatchMode::Atomic,
                Some(group.revision + 1),
            ),
            Err(PermissionError::GroupRevisionConflict)
        ));
    }

    #[test]
    fn group_selector_expands_membership_and_revision_from_the_same_state() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let requester = child(&flows, &root);
        let broker = PermissionBroker::new(flows);
        let SubmissionOutcome::Pending(first) =
            submit_to_flow(&broker, &requester, Arc::clone(&root)).unwrap()
        else {
            panic!("expected pending request");
        };
        let SubmissionOutcome::Pending(second) =
            submit_to_flow(&broker, &requester, Arc::clone(&root)).unwrap()
        else {
            panic!("expected pending request");
        };
        let group = broker
            .create_group(
                &root,
                BTreeSet::from([first.request.request_id.clone()]),
                "atomic".into(),
            )
            .unwrap();
        let mut state = broker.state.lock().unwrap();
        let current = state.groups.get_mut(&group.group_id).unwrap();
        current
            .request_ids
            .insert(second.request.request_id.clone());
        current.revision += 1;
        assert!(matches!(
            broker.expand_selector(
                &state,
                &root,
                &PermissionSelector::Group(group.group_id.clone()),
                Some(group.revision)
            ),
            Err(PermissionError::GroupRevisionConflict)
        ));
        let expected = BTreeSet::from([first.request.request_id, second.request.request_id])
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(
            broker
                .expand_selector(
                    &state,
                    &root,
                    &PermissionSelector::Group(group.group_id),
                    Some(group.revision + 1)
                )
                .unwrap(),
            expected
        );
    }

    #[test]
    fn s8_broker_group_emissions_keep_request_finals_before_single_resolved_transition() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let requesters = [
            child(&flows, &root),
            child(&flows, &root),
            child(&flows, &root),
        ];
        let broker = PermissionBroker::new(flows);
        let mut audit = attach_audit(&broker);
        let pending = requesters.each_ref().map(|requester| {
            let SubmissionOutcome::Pending(pending) =
                submit_to_flow(&broker, requester, Arc::clone(&root)).unwrap()
            else {
                panic!("expected pending request");
            };
            *pending
        });
        drain_audit(&mut audit);
        let ids = pending
            .each_ref()
            .map(|pending| pending.request.request_id.clone());
        let group = broker
            .create_group(&root, BTreeSet::from(ids.clone()), "review".into())
            .unwrap();
        broker
            .ungroup_requests(&root, &group.group_id, &BTreeSet::from([ids[2].clone()]))
            .unwrap();
        let current = broker
            .visible_group_get(&root, &group.group_id)
            .unwrap()
            .unwrap();
        broker
            .resolve_batch(
                &root,
                PermissionSelector::Group(group.group_id.clone()),
                PermissionAction::Approve,
                None,
                Some("accepted".into()),
                BatchMode::Atomic,
                Some(current.revision),
            )
            .unwrap();

        let frames = drain_audit(&mut audit);
        assert!(matches!(
            frames.as_slice(),
            [
                StreamFrame::PermissionGroupCreated { .. },
                StreamFrame::PermissionGroupUpdated { .. },
                StreamFrame::PermissionRequestApproved { .. },
                StreamFrame::PermissionRequestApproved { .. },
                StreamFrame::PermissionGroupResolved { .. }
            ]
        ));
        let final_ids = frames[2..4]
            .iter()
            .map(|frame| match frame {
                StreamFrame::PermissionRequestApproved { payload, .. } => {
                    payload.request_id.clone().unwrap()
                }
                _ => unreachable!(),
            })
            .collect::<BTreeSet<_>>();
        assert_eq!(final_ids, BTreeSet::from([ids[0].clone(), ids[1].clone()]));
        assert!(matches!(
            broker.get(&ids[2]).unwrap().state,
            PermissionRequestState::Pending { .. }
        ));

        broker
            .resolve_batch(
                &root,
                PermissionSelector::Group(group.group_id),
                PermissionAction::Approve,
                None,
                None,
                BatchMode::BestEffort,
                Some(current.revision),
            )
            .unwrap();
        assert!(drain_audit(&mut audit).is_empty());
    }

    #[test]
    fn s8_empty_group_delete_emits_terminal_snapshot_and_live_replay_remove_it() {
        use crate::projection::message_window::{TranscriptEntry, replay_transcript_from};
        use crate::workflow::WorkflowGraph;

        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let requester = child(&flows, &root);
        let broker = PermissionBroker::new(flows);
        let mut audit = attach_audit(&broker);
        let SubmissionOutcome::Pending(pending) =
            submit_to_flow(&broker, &requester, Arc::clone(&root)).unwrap()
        else {
            panic!("expected pending request");
        };
        drain_audit(&mut audit);
        let request_id = pending.request.request_id;
        let group = broker
            .create_group(
                &root,
                BTreeSet::from([request_id.clone()]),
                "delete-empty".into(),
            )
            .unwrap();
        broker
            .ungroup_requests(&root, &group.group_id, &BTreeSet::from([request_id]))
            .unwrap();
        let deleted = broker.delete_empty_group(&root, &group.group_id).unwrap();
        assert!(deleted.request_ids.is_empty());
        assert!(
            broker
                .visible_group_get(&root, &group.group_id)
                .unwrap()
                .is_none()
        );

        let frames = drain_audit(&mut audit);
        assert!(matches!(
            frames.as_slice(),
            [
                StreamFrame::PermissionGroupCreated { .. },
                StreamFrame::PermissionGroupUpdated { payload: updated, .. },
                StreamFrame::PermissionGroupResolved { payload: resolved, .. }
            ] if updated.request_ids.is_empty()
                && resolved.request_ids.is_empty()
                && resolved.revision == updated.revision
        ));
        let mut live = WorkflowGraph::new(crate::event::TurnId::now());
        for frame in &frames {
            live.apply_stream_frame(frame);
        }
        assert!(!live.permission_groups.contains_key(&group.group_id));

        let lines = frames
            .iter()
            .enumerate()
            .map(|(index, frame)| {
                let (kind, payload) = match frame {
                    StreamFrame::PermissionGroupCreated { payload, .. } => {
                        ("permission_group_created", payload)
                    }
                    StreamFrame::PermissionGroupUpdated { payload, .. } => {
                        ("permission_group_updated", payload)
                    }
                    StreamFrame::PermissionGroupResolved { payload, .. } => {
                        ("permission_group_resolved", payload)
                    }
                    _ => unreachable!(),
                };
                serde_json::json!({"type": kind, "seq": index + 1, "payload": payload}).to_string()
            })
            .collect::<Vec<_>>();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("events.jsonl");
        std::fs::write(&path, lines.join("\n")).unwrap();
        let mut replay = WorkflowGraph::new(crate::event::TurnId::now());
        for entry in replay_transcript_from(&path).unwrap() {
            if let TranscriptEntry::PermissionGroup { payload, resolved } = entry {
                replay.apply_permission_group(&payload, resolved);
            }
        }
        assert_eq!(replay.permission_groups, live.permission_groups);
        assert!(!replay.permission_groups.contains_key(&group.group_id));
    }

    #[test]
    fn s8_batch_resolve_versus_ungroup_uses_one_locked_membership_snapshot() {
        enum Schedule {
            BatchThenUngroup,
            UngroupThenBatch,
        }
        for schedule in [Schedule::BatchThenUngroup, Schedule::UngroupThenBatch] {
            let flows = Arc::new(FlowRegistry::default());
            let root = register_root(&flows, "session", true);
            let requester = child(&flows, &root);
            let broker = Arc::new(PermissionBroker::new(flows));
            let mut audit = attach_audit(&broker);
            let SubmissionOutcome::Pending(pending) =
                submit_to_flow(&broker, &requester, Arc::clone(&root)).unwrap()
            else {
                panic!("expected pending request");
            };
            drain_audit(&mut audit);
            let request_id = pending.request.request_id.clone();
            let group = broker
                .create_group(
                    &root,
                    BTreeSet::from([request_id.clone()]),
                    "batch-linearized".into(),
                )
                .unwrap();
            drain_audit(&mut audit);
            let (first_done_tx, first_done_rx) = std::sync::mpsc::channel();
            let (continue_tx, continue_rx) = std::sync::mpsc::channel();
            let batch_first = matches!(schedule, Schedule::BatchThenUngroup);
            let resolver = {
                let broker = Arc::clone(&broker);
                let root = Arc::clone(&root);
                let group_id = group.group_id.clone();
                let first_done_tx = first_done_tx.clone();
                std::thread::spawn(move || {
                    if !batch_first {
                        continue_rx.recv().unwrap();
                    }
                    let result = broker.resolve_batch(
                        &root,
                        PermissionSelector::Group(group_id),
                        PermissionAction::Approve,
                        None,
                        None,
                        BatchMode::Atomic,
                        None,
                    );
                    if batch_first {
                        first_done_tx.send(()).unwrap();
                    }
                    result
                })
            };
            let ungrouper = {
                let broker = Arc::clone(&broker);
                let root = Arc::clone(&root);
                let group_id = group.group_id.clone();
                let request_id = request_id.clone();
                std::thread::spawn(move || {
                    if batch_first {
                        first_done_rx.recv().unwrap();
                    }
                    broker
                        .ungroup_requests(&root, &group_id, &BTreeSet::from([request_id]))
                        .unwrap();
                    if !batch_first {
                        first_done_tx.send(()).unwrap();
                        continue_tx.send(()).unwrap();
                    }
                })
            };
            let resolutions = resolver.join().unwrap().unwrap();
            ungrouper.join().unwrap();
            let frames = drain_audit(&mut audit);

            match schedule {
                Schedule::BatchThenUngroup => {
                    assert_eq!(resolutions.len(), 1);
                    assert!(matches!(
                        broker.get(&request_id).unwrap().state,
                        PermissionRequestState::Approved { .. }
                    ));
                    let StreamFrame::PermissionRequestApproved { payload, .. } = &frames[0] else {
                        panic!("expected approved frame");
                    };
                    assert_eq!(payload.group_ids, vec![group.group_id.clone()]);
                    assert!(matches!(
                        frames.as_slice(),
                        [
                            StreamFrame::PermissionRequestApproved { .. },
                            StreamFrame::PermissionGroupResolved { .. },
                            StreamFrame::PermissionGroupUpdated { .. }
                        ]
                    ));
                }
                Schedule::UngroupThenBatch => {
                    assert!(resolutions.is_empty());
                    assert!(matches!(
                        broker.get(&request_id).unwrap().state,
                        PermissionRequestState::Pending { .. }
                    ));
                    assert!(matches!(
                        frames.as_slice(),
                        [
                            StreamFrame::PermissionGroupUpdated { payload, .. },
                            StreamFrame::PermissionGroupResolved { .. }
                        ] if payload.request_ids.is_empty()
                    ));
                }
            }
        }
    }

    #[test]
    fn s8_non_group_terminal_paths_emit_one_resolved_after_request_finals() {
        use crate::workflow::WorkflowGraph;

        enum Path {
            RequestIdsBatch,
            UserCancel,
            RunCleanup,
        }
        for path in [Path::RequestIdsBatch, Path::UserCancel, Path::RunCleanup] {
            let flows = Arc::new(FlowRegistry::default());
            let root = register_root(&flows, "session", true);
            let requester = child(&flows, &root);
            let broker = PermissionBroker::new(flows);
            let mut audit = attach_audit(&broker);
            let SubmissionOutcome::Pending(pending) =
                submit_to_flow(&broker, &requester, Arc::clone(&root)).unwrap()
            else {
                panic!("expected pending request");
            };
            drain_audit(&mut audit);
            let request_id = pending.request.request_id.clone();
            let group = broker
                .create_group(
                    &root,
                    BTreeSet::from([request_id.clone()]),
                    "non-group-terminal".into(),
                )
                .unwrap();
            drain_audit(&mut audit);

            match path {
                Path::RequestIdsBatch => {
                    broker
                        .resolve_batch(
                            &root,
                            PermissionSelector::RequestIds(vec![request_id]),
                            PermissionAction::Approve,
                            None,
                            None,
                            BatchMode::Atomic,
                            None,
                        )
                        .unwrap();
                }
                Path::UserCancel => broker.cancel(&request_id, "cancelled").unwrap(),
                Path::RunCleanup => {
                    assert_eq!(
                        broker.cancel_for_run("session", &requester.run_id, "terminal"),
                        1
                    );
                }
            }

            let frames = drain_audit(&mut audit);
            assert_eq!(
                frames
                    .iter()
                    .filter(|frame| matches!(frame, StreamFrame::PermissionGroupResolved { .. }))
                    .count(),
                1
            );
            assert!(matches!(
                frames.last(),
                Some(StreamFrame::PermissionGroupResolved { payload, .. })
                    if payload.group_id == group.group_id
            ));
            assert!(matches!(
                frames.first(),
                Some(
                    StreamFrame::PermissionRequestApproved { .. }
                        | StreamFrame::PermissionRequestCancelled { .. }
                )
            ));
            let mut live = WorkflowGraph::new(crate::event::TurnId::now());
            live.apply_permission_group(
                &crate::permission_audit::PermissionGroupAudit::from_group(
                    &group,
                    "session",
                    group.created_at,
                ),
                false,
            );
            for frame in &frames {
                live.apply_stream_frame(frame);
            }
            assert!(!live.permission_groups.contains_key(&group.group_id));
        }

        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let requester = child(&flows, &root);
        let broker = PermissionBroker::new(flows);
        let mut audit = attach_audit(&broker);
        let SubmissionOutcome::Pending(pending) =
            submit_to_flow(&broker, &requester, Arc::clone(&root)).unwrap()
        else {
            panic!("expected pending request");
        };
        drain_audit(&mut audit);
        let request_id = pending.request.request_id.clone();
        broker.cancel(&request_id, "already terminal").unwrap();
        drain_audit(&mut audit);
        let group = broker
            .create_group(&root, BTreeSet::from([request_id]), "terminal-only".into())
            .unwrap();
        assert!(matches!(
            drain_audit(&mut audit).as_slice(),
            [
                StreamFrame::PermissionGroupCreated { payload: created, .. },
                StreamFrame::PermissionGroupResolved { payload: resolved, .. }
            ] if created.group_id == group.group_id && resolved.group_id == group.group_id
        ));
    }

    #[test]
    fn s8_resolve_versus_ungroup_linearization_controls_final_membership_snapshot() {
        enum Schedule {
            ResolveThenUngroup,
            UngroupThenResolve,
        }
        for schedule in [Schedule::ResolveThenUngroup, Schedule::UngroupThenResolve] {
            let flows = Arc::new(FlowRegistry::default());
            let root = register_root(&flows, "session", true);
            let requester = child(&flows, &root);
            let broker = Arc::new(PermissionBroker::new(flows));
            let mut audit = attach_audit(&broker);
            let SubmissionOutcome::Pending(pending) =
                submit_to_flow(&broker, &requester, Arc::clone(&root)).unwrap()
            else {
                panic!("expected pending request");
            };
            drain_audit(&mut audit);
            let request_id = pending.request.request_id.clone();
            let group = broker
                .create_group(
                    &root,
                    BTreeSet::from([request_id.clone()]),
                    "linearized".into(),
                )
                .unwrap();
            drain_audit(&mut audit);
            let (first_done_tx, first_done_rx) = std::sync::mpsc::channel();
            let (continue_tx, continue_rx) = std::sync::mpsc::channel();
            let resolve_first = matches!(schedule, Schedule::ResolveThenUngroup);
            let resolver = {
                let broker = Arc::clone(&broker);
                let root = Arc::clone(&root);
                let request_id = request_id.clone();
                let first_done_tx = first_done_tx.clone();
                std::thread::spawn(move || {
                    if !resolve_first {
                        continue_rx.recv().unwrap();
                    }
                    let outcome = broker.resolve(
                        &request_id,
                        &DecisionAuthority::Flow(broker.flow_authority(root).unwrap()),
                        PermissionAction::Approve,
                        None,
                        None,
                    );
                    if resolve_first {
                        first_done_tx.send(()).unwrap();
                    }
                    outcome
                })
            };
            let ungrouper = {
                let broker = Arc::clone(&broker);
                let root = Arc::clone(&root);
                let group_id = group.group_id.clone();
                let request_id = request_id.clone();
                std::thread::spawn(move || {
                    if resolve_first {
                        first_done_rx.recv().unwrap();
                    }
                    broker
                        .ungroup_requests(&root, &group_id, &BTreeSet::from([request_id]))
                        .unwrap();
                    if !resolve_first {
                        first_done_tx.send(()).unwrap();
                        continue_tx.send(()).unwrap();
                    }
                })
            };
            assert!(resolver.join().unwrap().is_ok());
            ungrouper.join().unwrap();
            let frames = drain_audit(&mut audit);
            let final_payload = frames
                .iter()
                .find_map(|frame| match frame {
                    StreamFrame::PermissionRequestApproved { payload, .. } => Some(payload),
                    _ => None,
                })
                .unwrap();
            match schedule {
                Schedule::ResolveThenUngroup => {
                    assert_eq!(final_payload.group_ids, vec![group.group_id.clone()]);
                    assert!(matches!(
                        frames.as_slice(),
                        [
                            StreamFrame::PermissionRequestApproved { .. },
                            StreamFrame::PermissionGroupResolved { .. },
                            StreamFrame::PermissionGroupUpdated { .. }
                        ]
                    ));
                }
                Schedule::UngroupThenResolve => {
                    assert!(final_payload.group_ids.is_empty());
                    assert!(matches!(
                        frames.as_slice(),
                        [
                            StreamFrame::PermissionGroupUpdated { .. },
                            StreamFrame::PermissionGroupResolved { .. },
                            StreamFrame::PermissionRequestApproved { .. }
                        ]
                    ));
                }
            }
        }
    }

    #[test]
    fn atomic_batch_invalid_target_member_leaves_every_request_unchanged() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let requesters = [child(&flows, &root), child(&flows, &root)];
        let broker = PermissionBroker::new(flows);
        let SubmissionOutcome::Pending(flow_pending) =
            submit_to_flow(&broker, &requesters[0], Arc::clone(&root)).unwrap()
        else {
            panic!("expected pending request");
        };
        let user_pending = submit_to_user(&broker, &requesters[1]);
        let ids = [
            flow_pending.request.request_id.clone(),
            user_pending.request.request_id.clone(),
        ];
        let result = broker.resolve_batch(
            &root,
            PermissionSelector::RequestIds(ids.to_vec()),
            PermissionAction::Approve,
            None,
            None,
            BatchMode::Atomic,
            None,
        );
        assert!(
            matches!(result, Err(PermissionError::ActorNotAuthorized)),
            "unexpected result: {result:?}"
        );
        for id in ids {
            let request = broker.get(&id).unwrap();
            assert!(matches!(
                request.state,
                PermissionRequestState::Pending { .. }
            ));
            assert!(
                request
                    .escalation_path
                    .iter()
                    .all(|hop| hop.actor.is_none())
            );
        }
    }

    #[test]
    fn atomic_batch_terminal_requester_member_leaves_every_request_unchanged() {
        let flows = Arc::new(FlowRegistry::default());
        let root = register_root(&flows, "session", true);
        let requesters = [child(&flows, &root), child(&flows, &root)];
        let broker = PermissionBroker::new(flows);
        let pending = requesters.each_ref().map(|requester| {
            let SubmissionOutcome::Pending(pending) =
                submit_to_flow(&broker, requester, Arc::clone(&root)).unwrap()
            else {
                panic!("expected pending request");
            };
            *pending
        });
        let mut audit = attach_audit(&broker);
        *requesters[1].execution_state.lock().unwrap() = FlowExecutionState::Terminal;
        let ids = pending
            .each_ref()
            .map(|pending| pending.request.request_id.clone());
        assert!(matches!(
            broker.resolve_batch(
                &root,
                PermissionSelector::RequestIds(ids.to_vec()),
                PermissionAction::Approve,
                None,
                None,
                BatchMode::Atomic,
                None
            ),
            Err(PermissionError::ActorNotRunning)
        ));
        assert!(drain_audit(&mut audit).is_empty());
        for id in ids {
            let request = broker.get(&id).unwrap();
            assert!(matches!(
                request.state,
                PermissionRequestState::Pending { .. }
            ));
            assert!(
                request
                    .escalation_path
                    .iter()
                    .all(|hop| hop.actor.is_none())
            );
        }
    }

    #[test]
    fn cancellation_audit_attributes_the_system_component() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(flows);
        let mut audit = attach_audit(&broker);
        let pending = submit_to_user(&broker, &requester);
        drain_audit(&mut audit);

        broker
            .cancel(&pending.request.request_id, "operator cancelled")
            .unwrap();

        let frames = drain_audit(&mut audit);
        let [StreamFrame::PermissionRequestCancelled { payload, .. }] = frames.as_slice() else {
            panic!("expected one cancellation audit");
        };
        assert_eq!(
            payload.actor,
            Some(crate::permission_audit::PermissionProjectionActor::System {
                component: "permission.user_cancel".into(),
            })
        );
        assert_eq!(payload.reason.as_deref(), Some("operator cancelled"));
    }

    #[test]
    fn approval_audit_precedes_its_persistent_grant() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(flows);
        let mut audit = attach_audit(&broker);
        let pending = submit_to_user(&broker, &requester);
        drain_audit(&mut audit);

        broker
            .resolve(
                &pending.request.request_id,
                &user_decision(&broker, &requester.session_id),
                PermissionAction::Approve,
                Some(GrantScope::ChildRunSameTool {
                    run_id: requester.run_id.clone(),
                    tool_name: pending.request.intent.tool_name.clone(),
                }),
                Some("approved".into()),
            )
            .unwrap();

        let frames = drain_audit(&mut audit);
        assert!(matches!(
            frames.as_slice(),
            [
                StreamFrame::PermissionRequestApproved { .. },
                StreamFrame::PermissionGrantCreated { .. }
            ]
        ));
    }

    #[test]
    fn terminal_cleanup_emits_after_the_lifecycle_transition() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::shared(Arc::clone(&flows));
        let mut audit = attach_audit(&broker);
        submit_to_user(&broker, &requester);
        drain_audit(&mut audit);

        flows.mark_terminal(&requester.run_id);

        let frames = drain_audit(&mut audit);
        let [StreamFrame::PermissionRequestCancelled { payload, .. }] = frames.as_slice() else {
            panic!("expected synchronous terminal cancellation audit");
        };
        assert_eq!(
            payload.actor,
            Some(crate::permission_audit::PermissionProjectionActor::System {
                component: "permission.run_cleanup".into(),
            })
        );
    }

    #[test]
    fn user_group_facade_creates_and_resolves_with_current_revision() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(flows);
        let pending = submit_to_user(&broker, &requester);
        let request_id = pending.request.request_id.clone();
        let expected_revisions = HashMap::from([(request_id.clone(), pending.request.revision)]);

        let group = broker
            .user_create_group(
                &requester.session_id,
                BTreeSet::from([request_id.clone()]),
                "user review".into(),
                &expected_revisions,
            )
            .unwrap();

        assert_eq!(group.owner, GroupOwner::User);
        assert_eq!(group.request_ids, BTreeSet::from([request_id.clone()]));
        assert_eq!(
            broker.user_list(&requester.session_id).1,
            vec![group.clone()]
        );

        let resolutions = broker
            .user_resolve(
                &requester.session_id,
                Some("principal".into()),
                Vec::new(),
                &HashMap::new(),
                Some((group.group_id, group.revision)),
                PermissionAction::Deny,
                None,
                Some("denied by user".into()),
            )
            .unwrap();

        assert_eq!(resolutions.len(), 1);
        assert_eq!(resolutions[0].request_id, request_id);
        assert!(matches!(
            resolutions[0].outcome,
            BatchRequestOutcome::Denied(_)
        ));
        let resolved = broker.get(&resolutions[0].request_id).unwrap();
        assert!(matches!(
            resolved.state,
            PermissionRequestState::Denied { .. }
        ));
        assert_eq!(resolved.revision, pending.request.revision + 1);
        assert!(matches!(
            pending.resolution.blocking_recv().unwrap(),
            PermissionResolution::Decision(_)
        ));
    }

    #[test]
    fn user_group_facade_rejects_stale_group_revision_without_resolving() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(flows);
        let pending = submit_to_user(&broker, &requester);
        let request_id = pending.request.request_id.clone();
        let group = broker
            .user_create_group(
                &requester.session_id,
                BTreeSet::from([request_id.clone()]),
                "user review".into(),
                &HashMap::from([(request_id.clone(), pending.request.revision)]),
            )
            .unwrap();
        assert!(matches!(
            broker.user_resolve(
                &requester.session_id,
                None,
                Vec::new(),
                &HashMap::new(),
                Some((group.group_id, group.revision + 1)),
                PermissionAction::Deny,
                None,
                None,
            ),
            Err(PermissionError::GroupRevisionConflict)
        ));
        assert_eq!(broker.get(&request_id).unwrap(), pending.request);
    }

    #[test]
    fn user_request_facade_rejects_stale_and_duplicate_resolutions() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = PermissionBroker::new(flows);
        let pending = submit_to_user(&broker, &requester);
        let request_id = pending.request.request_id.clone();

        assert!(matches!(
            broker.user_resolve(
                &requester.session_id,
                None,
                vec![request_id.clone()],
                &HashMap::from([(request_id.clone(), pending.request.revision + 1)]),
                None,
                PermissionAction::Deny,
                None,
                None,
            ),
            Err(PermissionError::GroupRevisionConflict)
        ));

        let expected_revisions = HashMap::from([(request_id.clone(), pending.request.revision)]);
        let first = broker
            .user_resolve(
                &requester.session_id,
                None,
                vec![request_id.clone()],
                &expected_revisions,
                None,
                PermissionAction::Deny,
                None,
                None,
            )
            .unwrap();
        assert_eq!(first.len(), 1);
        assert!(matches!(first[0].outcome, BatchRequestOutcome::Denied(_)));

        let after_first = broker.get(&request_id).unwrap();
        assert_eq!(after_first.revision, pending.request.revision + 1);
        assert!(matches!(
            broker.user_resolve(
                &requester.session_id,
                None,
                vec![request_id.clone()],
                &expected_revisions,
                None,
                PermissionAction::Deny,
                None,
                None,
            ),
            Err(PermissionError::GroupRevisionConflict)
        ));
        assert_eq!(broker.get(&request_id).unwrap(), after_first);
    }

    #[test]
    fn concurrent_user_resolutions_have_exactly_one_winner() {
        let flows = Arc::new(FlowRegistry::default());
        let requester = register_root(&flows, "session", false);
        let broker = Arc::new(PermissionBroker::new(flows));
        let pending = submit_to_user(&broker, &requester);
        let request_id = pending.request.request_id.clone();
        let revision = pending.request.revision;
        let barrier = Arc::new(std::sync::Barrier::new(3));

        let resolve = |broker: Arc<PermissionBroker>, barrier: Arc<std::sync::Barrier>| {
            let session_id = requester.session_id.clone();
            let request_id = request_id.clone();
            std::thread::spawn(move || {
                barrier.wait();
                broker.user_resolve(
                    &session_id,
                    None,
                    vec![request_id.clone()],
                    &HashMap::from([(request_id, revision)]),
                    None,
                    PermissionAction::Deny,
                    None,
                    None,
                )
            })
        };

        let first = resolve(Arc::clone(&broker), Arc::clone(&barrier));
        let second = resolve(Arc::clone(&broker), Arc::clone(&barrier));
        barrier.wait();
        let results = [first.join().unwrap(), second.join().unwrap()];

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(PermissionError::GroupRevisionConflict)))
                .count(),
            1
        );
        let resolved = broker.get(&request_id).unwrap();
        assert!(matches!(
            resolved.state,
            PermissionRequestState::Denied { .. }
        ));
        assert_eq!(resolved.revision, revision + 1);
        assert!(matches!(
            pending.resolution.blocking_recv().unwrap(),
            PermissionResolution::Decision(_)
        ));
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
