use std::collections::{BTreeMap, BTreeSet};

use crate::event::{Event, FlowRunId};
use crate::permission_audit::PermissionGroupAuditOwner;

use super::{LegacyOwner, UnassignedReason};

pub(super) enum Subject {
    Default,
    Run(String),
    Owner(String),
    Unscoped,
    Invalid,
}

impl Subject {
    pub(super) fn of(event: &Event) -> Self {
        match event {
            Event::GoalTurnStarted { run_id, .. }
            | Event::FlowStart { run_id, .. }
            | Event::FlowEnd { run_id, .. }
            | Event::WorkspaceLifecycle { run_id, .. }
            | Event::FlowGraph { run_id, .. }
            | Event::FlowNodeStart { run_id, .. }
            | Event::FlowNodeEnd { run_id, .. }
            | Event::ToolNode { run_id, .. }
            | Event::ToolPendingApproval { run_id, .. }
            | Event::ToolApproved { run_id, .. }
            | Event::ToolDenied { run_id, .. } => Self::Run(run_id.to_string()),
            Event::GoalChanged { owner, .. } => Self::Owner(owner.clone()),
            // Published legacy session messages use None for the default agent.
            Event::UserMsg { flow_run_id, .. }
            | Event::AssistantMsg { flow_run_id, .. }
            | Event::ToolResultMsg { flow_run_id, .. }
            | Event::SystemMsg { flow_run_id, .. }
            | Event::CompactionSummary { flow_run_id, .. }
            | Event::ContextCompact { flow_run_id, .. }
            | Event::Checkpoint { flow_run_id, .. }
            | Event::DeferredFormApplied { flow_run_id, .. } => {
                Self::run_or(flow_run_id, Self::Default)
            }
            Event::TurnStart { .. } | Event::TurnEnd { .. } | Event::UserInject { .. } => {
                Self::Default
            }
            Event::LlmCall { run_id, .. } => Self::run_or(run_id, Self::Unscoped),
            Event::ToolResultMetrics { flow_run_id, .. }
            | Event::DiffPreview { flow_run_id, .. }
            | Event::FileEditApplied { flow_run_id, .. }
            | Event::ContentFilterHit { flow_run_id, .. }
            | Event::ContextTruncated { flow_run_id, .. }
            | Event::WatchWarn { flow_run_id, .. }
            | Event::LlmPartialCall { flow_run_id, .. }
            | Event::AttachmentDegraded { flow_run_id, .. } => {
                Self::run_or(flow_run_id, Self::Unscoped)
            }
            Event::PermissionRequestCreated { payload }
            | Event::PermissionRequestTargeted { payload }
            | Event::PermissionRequestDeferred { payload }
            | Event::PermissionRequestApproved { payload }
            | Event::PermissionRequestDenied { payload }
            | Event::PermissionRequestCancelled { payload }
            | Event::UnrestrictedExecution { payload } => {
                Self::Run(payload.requesting_run_id.to_string())
            }
            Event::PermissionGrantCreated { payload }
            | Event::PermissionGrantExpired { payload } => {
                Self::Run(payload.requesting_run_id.to_string())
            }
            Event::PermissionGroupCreated { payload }
            | Event::PermissionGroupUpdated { payload }
            | Event::PermissionGroupResolved { payload } => match &payload.owner {
                PermissionGroupAuditOwner::Flow { run_id } => Self::Run(run_id.to_string()),
                PermissionGroupAuditOwner::User { .. } | PermissionGroupAuditOwner::System => {
                    Self::Unscoped
                }
            },
            Event::PendingPrompt { .. }
            | Event::PromptExpired { .. }
            | Event::PromptResolved { .. }
            | Event::DeferredFormRecorded { .. }
            | Event::TerminalFinalState { .. }
            | Event::MermaidDiagram { .. } => Self::Unscoped,
        }
    }

    fn run_or(run: &Option<FlowRunId>, otherwise: Self) -> Self {
        run.as_ref()
            .map(|r| Self::Run(r.to_string()))
            .unwrap_or(otherwise)
    }

    pub(super) fn owner(&self, session: &str, runs: &BTreeMap<String, LegacyOwner>) -> LegacyOwner {
        match self {
            Self::Default => LegacyOwner::Default,
            Self::Run(run) => runs
                .get(run)
                .cloned()
                .unwrap_or_else(|| unassigned(UnassignedReason::UnknownRun)),
            Self::Owner(owner) => named_owner(owner, session),
            Self::Unscoped => unassigned(UnassignedReason::Unscoped),
            Self::Invalid => unassigned(UnassignedReason::InvalidRecord),
        }
    }
}

#[derive(Default)]
pub(super) struct Lineage {
    runs: BTreeMap<String, Run>,
}

#[derive(Default)]
struct Run {
    start: Option<Start>,
    owner: Option<String>,
    conflict: Option<UnassignedReason>,
}

#[derive(PartialEq, Eq)]
struct Start {
    parent: Option<String>,
    spawned: bool,
    name: String,
}

impl Lineage {
    pub(super) fn observe(&mut self, event: &Event) {
        match event {
            Event::FlowStart {
                run_id,
                parent_run_id,
                spawned,
                flow_name,
                ..
            } => {
                if let Some(parent) = parent_run_id {
                    self.runs.entry(parent.to_string()).or_default();
                }
                let run = self.runs.entry(run_id.to_string()).or_default();
                let start = Start {
                    parent: parent_run_id.as_ref().map(ToString::to_string),
                    spawned: *spawned,
                    name: flow_name.clone(),
                };
                if run
                    .start
                    .as_ref()
                    .is_some_and(|previous| previous != &start)
                {
                    run.conflict = Some(UnassignedReason::ConflictingRun);
                } else {
                    run.start = Some(start);
                }
            }
            Event::GoalTurnStarted { owner, run_id, .. } => {
                let run = self.runs.entry(run_id.to_string()).or_default();
                if owner.is_empty() || run.owner.as_ref().is_some_and(|previous| previous != owner)
                {
                    run.conflict = Some(UnassignedReason::ConflictingOwner);
                } else {
                    run.owner = Some(owner.clone());
                }
            }
            _ => {}
        }
    }

    pub(super) fn reference(&mut self, subject: &Subject) {
        if let Subject::Run(run) = subject {
            self.runs.entry(run.clone()).or_default();
        }
    }

    pub(super) fn resolve(&self, session: &str) -> BTreeMap<String, LegacyOwner> {
        let mut resolved: BTreeMap<String, LegacyOwner> = BTreeMap::new();
        for run_id in self.runs.keys() {
            let mut current = run_id;
            let mut path = Vec::new();
            let mut visiting = BTreeSet::new();
            let mut owner = loop {
                if let Some(owner) = resolved.get(current) {
                    break owner.clone();
                }
                if !visiting.insert(current) {
                    break unassigned(UnassignedReason::CyclicLineage);
                }
                let run = &self.runs[current];
                if let Some(reason) = run.conflict {
                    resolved.insert(current.clone(), unassigned(reason));
                    break unassigned(reason);
                }
                if run.start.is_none() && run.owner.is_none() {
                    resolved.insert(current.clone(), unassigned(UnassignedReason::UnknownRun));
                    break unassigned(UnassignedReason::UnknownRun);
                }
                if let Some(parent) = run.start.as_ref().and_then(|s| s.parent.as_ref()) {
                    path.push(current);
                    current = parent;
                } else {
                    let owner = run.resolve(current, session, None);
                    resolved.insert(current.clone(), owner.clone());
                    break owner;
                }
            };
            // Iterative resolution avoids stack overflow on deep or corrupt lineages.
            for id in path.into_iter().rev() {
                owner = self.runs[id].resolve(id, session, Some(owner));
                resolved.insert(id.clone(), owner.clone());
            }
        }
        resolved
    }
}

impl Run {
    fn resolve(&self, id: &str, session: &str, parent: Option<LegacyOwner>) -> LegacyOwner {
        if let Some(LegacyOwner::Unassigned { reason }) = parent {
            return unassigned(reason);
        }
        let owner = self.owner.as_ref().map(|owner| named_owner(owner, session));
        if self.start.as_ref().is_some_and(|s| s.spawned) {
            // Continuations can have an earlier run of the same agent as parent.
            return owner.unwrap_or_else(|| LegacyOwner::SpawnedRun { run_id: id.into() });
        }
        match (owner, parent) {
            (Some(owner), Some(parent)) if owner != parent => {
                unassigned(UnassignedReason::ConflictingOwner)
            }
            (Some(owner), _) => owner,
            (None, Some(parent)) => parent,
            (None, None) => LegacyOwner::Default,
        }
    }
}

fn named_owner(owner: &str, session: &str) -> LegacyOwner {
    if owner.is_empty() {
        unassigned(UnassignedReason::ConflictingOwner)
    } else if owner == session {
        LegacyOwner::Default
    } else {
        LegacyOwner::Agent {
            owner: owner.into(),
        }
    }
}

fn unassigned(reason: UnassignedReason) -> LegacyOwner {
    LegacyOwner::Unassigned { reason }
}
