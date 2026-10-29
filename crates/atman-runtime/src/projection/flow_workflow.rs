//! Per-turn execution graphs derived from an instance's committed transcript.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use atman_rt::{TurnLease, TurnOutcome};
use chrono::{DateTime, Utc};

use super::{
    flow::{FlowRecord, FlowRecordId, FlowTranscriptEntry},
    workflow::WorkflowProjection,
};
use crate::{
    event::{AtmanUuid, Event, FlowRunId},
    flow_store::trace::ExecutionScope,
    permission::{PermissionGroupId, PermissionRequestId},
    permission_audit::{PermissionGroupAudit, PermissionRequestAudit},
};

#[derive(Clone, Debug)]
pub struct FlowTurnEnd {
    pub record: FlowRecordId,
    pub at: DateTime<Utc>,
    pub outcome: TurnOutcome,
}

/// Execution belongs to an admitted turn, independently of its creator or the currently selected view.
pub struct FlowTurnProjection {
    lease: TurnLease<AtmanUuid>,
    workflow: WorkflowProjection,
    completion: Option<FlowTurnEnd>,
}

impl FlowTurnProjection {
    pub fn lease(&self) -> &TurnLease<AtmanUuid> {
        &self.lease
    }

    pub fn workflow(&self) -> &WorkflowProjection {
        &self.workflow
    }

    pub fn completion(&self) -> Option<&FlowTurnEnd> {
        self.completion.as_ref()
    }

    fn close_if_finished(&mut self) {
        if let Some(end) = &self.completion {
            self.workflow.finish_invocation(end.outcome, end.at);
        }
    }
}

#[derive(Default)]
pub(super) struct FlowWorkflows {
    turns: BTreeMap<u64, FlowTurnProjection>,
    runs: HashMap<FlowRunId, Option<u64>>,
    requests: HashMap<PermissionRequestId, u64>,
    groups: HashMap<PermissionGroupId, (PermissionGroupAudit, bool)>,
    finished_updates: BTreeSet<u64>,
}

impl FlowWorkflows {
    pub fn turns(&self) -> impl DoubleEndedIterator<Item = &FlowTurnProjection> {
        self.turns.values()
    }

    fn turn(&mut self, lease: &TurnLease<AtmanUuid>) -> &mut FlowTurnProjection {
        self.index_run(&lease.run, lease.epoch);
        self.turns
            .entry(lease.epoch)
            .or_insert_with(|| FlowTurnProjection {
                lease: lease.clone(),
                workflow: WorkflowProjection::new(lease.turn.clone()),
                completion: None,
            })
    }

    fn index_run(&mut self, run: &FlowRunId, epoch: u64) {
        self.runs
            .entry(run.clone())
            .and_modify(|owner| {
                if *owner != Some(epoch) {
                    *owner = None;
                }
            })
            .or_insert(Some(epoch));
    }

    pub fn observe(&mut self, entry: &FlowTranscriptEntry) {
        match &entry.record {
            FlowRecord::Input(input) => {
                self.turn(&input.lease);
            }
            FlowRecord::Publication(publication) => {
                self.index_run(&publication.run, publication.lease.epoch);
                let turn = self.turn(&publication.lease);
                if turn
                    .workflow
                    .apply_published_message(&publication.run, &publication.message)
                    .changed()
                    && turn.completion.is_some()
                {
                    self.finished_updates.insert(publication.lease.epoch);
                }
            }
            FlowRecord::Completion(completion) => {
                let turn = self.turn(&completion.lease);
                turn.completion = Some(FlowTurnEnd {
                    record: entry.id,
                    at: completion.finished_at,
                    outcome: completion.outcome,
                });
                self.finished_updates.insert(completion.lease.epoch);
            }
            FlowRecord::Execution(record) => {
                let epoch = match &record.scope {
                    ExecutionScope::Invocation(lease) => {
                        self.turn(lease);
                        if let Some(run) = execution_run(&record.event) {
                            self.index_run(run, lease.epoch);
                        }
                        Some(lease.epoch)
                    }
                    ExecutionScope::Instance(_) => self.service_turn(&record.event),
                };
                if let Event::PermissionGroupCreated { payload }
                | Event::PermissionGroupUpdated { payload }
                | Event::PermissionGroupResolved { payload } = &record.event
                {
                    let resolved = matches!(record.event, Event::PermissionGroupResolved { .. });
                    let saved = self
                        .groups
                        .entry(payload.group_id.clone())
                        .or_insert_with(|| (payload.clone(), resolved));
                    if payload.revision >= saved.0.revision {
                        *saved = (payload.clone(), saved.1 || resolved);
                    }
                    // Groups can precede requests and can contain more than one invocation.
                    for (key, turn) in self.turns.iter_mut().filter(|(key, turn)| {
                        epoch == Some(**key)
                            || saved.0.request_ids.iter().any(|id| {
                                turn.workflow.permission_requests.contains_key(
                                    &crate::workflow::WorkflowPermissionIdentity::Canonical {
                                        request_id: id.clone(),
                                    },
                                )
                            })
                    }) {
                        if turn
                            .workflow
                            .apply_permission_group(&saved.0, saved.1)
                            .changed()
                            && turn.completion.is_some()
                        {
                            self.finished_updates.insert(*key);
                        }
                    }
                } else if let Some((epoch, turn)) =
                    epoch.and_then(|epoch| self.turns.get_mut(&epoch).map(|turn| (epoch, turn)))
                {
                    let mut changed = turn
                        .workflow
                        .apply_recorded_event(&record.event, record.timestamp)
                        .changed();
                    if let Some(request) = permission_request(&record.event) {
                        if let Some(id) = &request.request_id {
                            self.requests.entry(id.clone()).or_insert(epoch);
                        }
                        for group in request
                            .group_ids
                            .iter()
                            .filter_map(|id| self.groups.get(id))
                        {
                            changed |= turn
                                .workflow
                                .apply_permission_group(&group.0, group.1)
                                .changed();
                        }
                    }
                    if changed && turn.completion.is_some() {
                        self.finished_updates.insert(epoch);
                    }
                }
            }
            FlowRecord::Checkpoint(_) | FlowRecord::Legacy(_) | FlowRecord::Continuation { .. } => {
            }
        }
    }

    pub fn finish_batch(&mut self) {
        for epoch in std::mem::take(&mut self.finished_updates) {
            if let Some(turn) = self.turns.get_mut(&epoch) {
                turn.close_if_finished();
            }
        }
    }

    fn service_turn(&self, event: &Event) -> Option<u64> {
        if let Some(payload) = permission_request(event) {
            return payload
                .request_id
                .as_ref()
                .and_then(|id| self.requests.get(id).copied())
                .or_else(|| self.runs.get(&payload.requesting_run_id).copied().flatten())
                .or_else(|| self.runs.get(&payload.root_run_id).copied().flatten());
        }
        match event {
            Event::PermissionGroupCreated { payload }
            | Event::PermissionGroupUpdated { payload }
            | Event::PermissionGroupResolved { payload } => match &payload.owner {
                crate::permission_audit::PermissionGroupAuditOwner::Flow { run_id } => {
                    self.runs.get(run_id).copied().flatten()
                }
                _ => None,
            },
            _ => None,
        }
    }
}

fn permission_request(event: &Event) -> Option<&PermissionRequestAudit> {
    match event {
        Event::PermissionRequestCreated { payload }
        | Event::PermissionRequestTargeted { payload }
        | Event::PermissionRequestDeferred { payload }
        | Event::PermissionRequestApproved { payload }
        | Event::PermissionRequestDenied { payload }
        | Event::PermissionRequestCancelled { payload }
        | Event::UnrestrictedExecution { payload } => Some(payload),
        _ => None,
    }
}

fn execution_run(event: &Event) -> Option<&FlowRunId> {
    match event {
        Event::FlowStart { run_id, .. }
        | Event::FlowEnd { run_id, .. }
        | Event::FlowGraph { run_id, .. }
        | Event::FlowNodeStart { run_id, .. }
        | Event::FlowNodeEnd { run_id, .. }
        | Event::ToolNode { run_id, .. }
        | Event::ToolPendingApproval { run_id, .. }
        | Event::ToolApproved { run_id, .. }
        | Event::ToolDenied { run_id, .. } => Some(run_id),
        Event::LlmCall { run_id, .. } => run_id.as_ref(),
        _ => None,
    }
}
