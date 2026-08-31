use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::permission::{PermissionGroupId, PermissionRequestId};
use crate::permission_audit::{PermissionGroupAudit, PermissionRequestAudit};
use crate::workflow::{
    ApprovalState, WorkflowGraph, WorkflowPermissionIdentity, WorkflowPermissionRequest,
    WorkflowPermissionState,
};

pub(super) type ToolKey = (String, String);

#[cfg(test)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct PermissionPerfCounters {
    pub winner_selections: u64,
    pub group_member_visits: u64,
}

#[cfg(test)]
thread_local! {
    static PERF_COUNTERS: std::cell::Cell<PermissionPerfCounters> = const {
        std::cell::Cell::new(PermissionPerfCounters {
            winner_selections: 0,
            group_member_visits: 0,
        })
    };
}

#[cfg(test)]
pub(super) fn reset_perf_counters() {
    PERF_COUNTERS.with(|counters| counters.set(PermissionPerfCounters::default()));
}

#[cfg(test)]
pub(super) fn perf_counters() -> PermissionPerfCounters {
    PERF_COUNTERS.with(std::cell::Cell::get)
}

#[cfg(test)]
fn count_winner_selection() {
    PERF_COUNTERS.with(|counters| {
        let mut value = counters.get();
        value.winner_selections = value.winner_selections.saturating_add(1);
        counters.set(value);
    });
}

#[cfg(test)]
fn count_group_member_visit() {
    PERF_COUNTERS.with(|counters| {
        let mut value = counters.get();
        value.group_member_visits = value.group_member_visits.saturating_add(1);
        counters.set(value);
    });
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PermissionCandidate {
    pending: bool,
    at: DateTime<Utc>,
    identity: WorkflowPermissionIdentity,
}

impl PermissionCandidate {
    fn new(identity: WorkflowPermissionIdentity, request: &WorkflowPermissionRequest) -> Self {
        Self {
            pending: request.state.is_pending(),
            at: request.payload.at,
            identity,
        }
    }
}

impl Ord for PermissionCandidate {
    fn cmp(&self, other: &Self) -> Ordering {
        self.pending
            .cmp(&other.pending)
            .then_with(|| self.at.cmp(&other.at))
            .then_with(|| other.identity.cmp(&self.identity))
    }
}

impl PartialOrd for PermissionCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct PermissionProjection {
    candidates: HashMap<ToolKey, BTreeSet<PermissionCandidate>>,
    winners: HashMap<ToolKey, WorkflowPermissionIdentity>,
    pending_tools: HashSet<ToolKey>,
    pending_identities: BTreeSet<WorkflowPermissionIdentity>,
    pending_by_run: HashMap<String, usize>,
    group_members: BTreeMap<PermissionGroupId, BTreeMap<PermissionRequestId, usize>>,
    request_groups: BTreeMap<PermissionRequestId, BTreeSet<PermissionGroupId>>,
    group_resolved_counts: BTreeMap<PermissionGroupId, usize>,
}

#[derive(Debug, Default)]
pub(super) struct PermissionUpdate {
    pub changed: bool,
    pub winner_tools: Vec<ToolKey>,
    pub group_progress_changed: bool,
}

impl PermissionProjection {
    pub(super) fn rebuild(graph: &WorkflowGraph) -> Self {
        let mut projection = Self::default();
        for (identity, request) in &graph.permission_requests {
            projection.insert_request_indices(identity.clone(), request);
        }
        for (group_id, group) in &graph.permission_groups {
            projection.index_group(group_id.clone(), group);
        }
        let group_ids = projection.group_members.keys().cloned().collect::<Vec<_>>();
        for group_id in group_ids {
            projection.recompute_group_count(graph, &group_id);
        }
        let tool_keys = projection.candidates.keys().cloned().collect::<Vec<_>>();
        for tool_key in tool_keys {
            projection.recompute_winner(graph, &tool_key);
        }
        projection
    }

    pub(super) fn apply_request(
        &mut self,
        graph: &mut WorkflowGraph,
        identity: WorkflowPermissionIdentity,
        payload: PermissionRequestAudit,
        state: WorkflowPermissionState,
    ) -> PermissionUpdate {
        let next = WorkflowPermissionRequest { payload, state };
        let previous = graph.permission_requests.get(&identity).cloned();
        if previous.as_ref() == Some(&next) {
            return PermissionUpdate::default();
        }

        let mut affected_tools = BTreeSet::new();
        if let Some(previous) = previous.as_ref() {
            affected_tools.insert(tool_key(previous));
        }
        affected_tools.insert(tool_key(&next));
        let before_winners = affected_tools
            .iter()
            .map(|tool_key| (tool_key.clone(), self.winner_snapshot(graph, tool_key)))
            .collect::<BTreeMap<_, _>>();

        let affected_request_ids = previous
            .iter()
            .filter_map(|request| request.payload.request_id.clone())
            .chain(next.payload.request_id.clone())
            .chain(match &identity {
                WorkflowPermissionIdentity::Canonical { request_id } => Some(request_id.clone()),
                WorkflowPermissionIdentity::Legacy { .. } => None,
            })
            .collect::<BTreeSet<_>>();
        let before_request_resolution = affected_request_ids
            .iter()
            .map(|request_id| {
                (
                    request_id.clone(),
                    canonical_request_is_resolved(graph, request_id),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let affected_groups = affected_request_ids
            .iter()
            .flat_map(|request_id| {
                self.request_groups
                    .get(request_id)
                    .into_iter()
                    .flat_map(|groups| groups.iter().cloned())
            })
            .collect::<BTreeSet<_>>();
        let before_group_progress = affected_groups
            .iter()
            .map(|group_id| (group_id.clone(), self.group_progress(group_id)))
            .collect::<BTreeMap<_, _>>();

        if let Some(previous) = previous.as_ref() {
            self.remove_request_indices(&identity, previous);
        }
        graph
            .permission_requests
            .insert(identity.clone(), next.clone());
        self.insert_request_indices(identity, &next);

        for tool_key in &affected_tools {
            self.recompute_winner(graph, tool_key);
        }
        for group_id in &affected_groups {
            let resolved = self
                .group_resolved_counts
                .get(group_id)
                .copied()
                .unwrap_or(0);
            let next_resolved = affected_request_ids
                .iter()
                .fold(resolved, |count, request_id| {
                    let multiplicity = self
                        .group_members
                        .get(group_id)
                        .and_then(|members| members.get(request_id))
                        .copied()
                        .unwrap_or(0);
                    match (
                        before_request_resolution
                            .get(request_id)
                            .copied()
                            .unwrap_or(false),
                        canonical_request_is_resolved(graph, request_id),
                    ) {
                        (false, true) => count.saturating_add(multiplicity),
                        (true, false) => count.saturating_sub(multiplicity),
                        _ => count,
                    }
                });
            self.group_resolved_counts
                .insert(group_id.clone(), next_resolved);
        }

        let winner_tools = affected_tools
            .into_iter()
            .filter(|tool_key| {
                before_winners.get(tool_key).cloned().flatten()
                    != self.winner_snapshot(graph, tool_key)
            })
            .collect();
        let group_progress_changed = affected_groups.into_iter().any(|group_id| {
            before_group_progress.get(&group_id).copied().flatten()
                != self.group_progress(&group_id)
        });
        PermissionUpdate {
            changed: true,
            winner_tools,
            group_progress_changed,
        }
    }

    pub(super) fn apply_group(
        &mut self,
        graph: &mut WorkflowGraph,
        payload: &PermissionGroupAudit,
        resolved: bool,
    ) -> bool {
        let before_group = graph.permission_groups.get(&payload.group_id).cloned();
        let before_resolved = graph.resolved_permission_groups.contains(&payload.group_id);
        graph.apply_permission_group(payload, resolved);
        let changed = before_group != graph.permission_groups.get(&payload.group_id).cloned()
            || before_resolved != graph.resolved_permission_groups.contains(&payload.group_id);
        if !changed {
            return false;
        }

        self.remove_group(&payload.group_id);
        if let Some(group) = graph.permission_groups.get(&payload.group_id) {
            self.index_group(payload.group_id.clone(), group);
            self.recompute_group_count(graph, &payload.group_id);
        }
        true
    }

    pub(super) fn winner_request_for_tool<'a>(
        &self,
        graph: &'a WorkflowGraph,
        tool_key: &ToolKey,
    ) -> Option<&'a WorkflowPermissionRequest> {
        graph.permission_requests.get(self.winners.get(tool_key)?)
    }

    pub(super) fn approval_for_tool(
        &self,
        graph: &WorkflowGraph,
        tool_key: &ToolKey,
    ) -> Option<ApprovalState> {
        self.winner_request_for_tool(graph, tool_key)
            .map(request_approval)
    }

    pub(super) fn group_progress(&self, group_id: &PermissionGroupId) -> Option<(usize, usize)> {
        let members = self.group_members.get(group_id)?;
        Some((
            self.group_resolved_counts
                .get(group_id)
                .copied()
                .unwrap_or(0),
            members.values().sum(),
        ))
    }

    pub(super) fn pending_identities(&self) -> Vec<WorkflowPermissionIdentity> {
        self.pending_identities.iter().cloned().collect()
    }

    pub(super) fn pending_count_for_run(&self, run_id: &str) -> usize {
        self.pending_by_run.get(run_id).copied().unwrap_or(0)
    }

    #[cfg(test)]
    pub(super) fn pending_tool_count(&self) -> usize {
        self.pending_tools.len()
    }

    fn winner_snapshot(
        &self,
        graph: &WorkflowGraph,
        tool_key: &ToolKey,
    ) -> Option<(WorkflowPermissionIdentity, WorkflowPermissionRequest)> {
        let identity = self.winners.get(tool_key)?;
        Some((
            identity.clone(),
            graph.permission_requests.get(identity)?.clone(),
        ))
    }

    fn insert_request_indices(
        &mut self,
        identity: WorkflowPermissionIdentity,
        request: &WorkflowPermissionRequest,
    ) {
        let key = tool_key(request);
        self.candidates
            .entry(key)
            .or_default()
            .insert(PermissionCandidate::new(identity.clone(), request));
        if request.state.is_pending() {
            self.pending_identities.insert(identity);
            *self
                .pending_by_run
                .entry(request.payload.requesting_run_id.0.to_string())
                .or_default() += 1;
        }
    }

    fn remove_request_indices(
        &mut self,
        identity: &WorkflowPermissionIdentity,
        request: &WorkflowPermissionRequest,
    ) {
        let key = tool_key(request);
        if let Some(candidates) = self.candidates.get_mut(&key) {
            candidates.remove(&PermissionCandidate::new(identity.clone(), request));
            if candidates.is_empty() {
                self.candidates.remove(&key);
            }
        }
        if request.state.is_pending() {
            self.pending_identities.remove(identity);
            let run_id = request.payload.requesting_run_id.0.to_string();
            if let Some(count) = self.pending_by_run.get_mut(&run_id) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    self.pending_by_run.remove(&run_id);
                }
            }
        }
    }

    fn recompute_winner(&mut self, graph: &WorkflowGraph, tool_key: &ToolKey) {
        #[cfg(test)]
        count_winner_selection();
        let winner = self
            .candidates
            .get(tool_key)
            .and_then(|candidates| candidates.last())
            .map(|candidate| candidate.identity.clone());
        match winner {
            Some(identity) => {
                let pending = graph
                    .permission_requests
                    .get(&identity)
                    .is_some_and(|request| request.state.is_pending());
                self.winners.insert(tool_key.clone(), identity);
                if pending {
                    self.pending_tools.insert(tool_key.clone());
                } else {
                    self.pending_tools.remove(tool_key);
                }
            }
            None => {
                self.winners.remove(tool_key);
                self.pending_tools.remove(tool_key);
            }
        }
    }

    fn index_group(&mut self, group_id: PermissionGroupId, group: &PermissionGroupAudit) {
        let mut members = BTreeMap::new();
        for request_id in &group.request_ids {
            *members.entry(request_id.clone()).or_default() += 1;
            self.request_groups
                .entry(request_id.clone())
                .or_default()
                .insert(group_id.clone());
        }
        self.group_members.insert(group_id, members);
    }

    fn remove_group(&mut self, group_id: &PermissionGroupId) {
        let Some(members) = self.group_members.remove(group_id) else {
            self.group_resolved_counts.remove(group_id);
            return;
        };
        for (request_id, _) in members {
            if let Some(groups) = self.request_groups.get_mut(&request_id) {
                groups.remove(group_id);
                if groups.is_empty() {
                    self.request_groups.remove(&request_id);
                }
            }
        }
        self.group_resolved_counts.remove(group_id);
    }

    fn recompute_group_count(&mut self, graph: &WorkflowGraph, group_id: &PermissionGroupId) {
        let Some(members) = self.group_members.get(group_id) else {
            self.group_resolved_counts.remove(group_id);
            return;
        };
        let resolved = members
            .iter()
            .map(|(request_id, multiplicity)| {
                #[cfg(test)]
                count_group_member_visit();
                if canonical_request_is_resolved(graph, request_id) {
                    *multiplicity
                } else {
                    0
                }
            })
            .sum();
        self.group_resolved_counts
            .insert(group_id.clone(), resolved);
    }
}

fn canonical_request_is_resolved(graph: &WorkflowGraph, request_id: &PermissionRequestId) -> bool {
    graph
        .permission_requests
        .get(&WorkflowPermissionIdentity::Canonical {
            request_id: request_id.clone(),
        })
        .is_some_and(|request| !request.state.is_pending())
}

fn tool_key(request: &WorkflowPermissionRequest) -> ToolKey {
    (
        request.payload.requesting_run_id.0.to_string(),
        request.payload.tool_use_id.clone(),
    )
}

fn request_approval(request: &WorkflowPermissionRequest) -> ApprovalState {
    match request.state {
        WorkflowPermissionState::Pending => ApprovalState::Pending {
            level: format!("{:?}", request.payload.tier).to_lowercase(),
            preview: None,
        },
        WorkflowPermissionState::Approved | WorkflowPermissionState::Unrestricted => {
            ApprovalState::Approved
        }
        WorkflowPermissionState::Denied
        | WorkflowPermissionState::Cancelled
        | WorkflowPermissionState::Interrupted => ApprovalState::Denied {
            reason: request
                .payload
                .reason
                .clone()
                .unwrap_or_else(|| "permission denied".into()),
        },
    }
}
