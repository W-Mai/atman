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
    flow_store::{FlowRef, trace::ExecutionScope},
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
    sources: Vec<SourceWorkflows>,
    source_indices: HashMap<FlowRef, usize>,
    source_order: Vec<usize>,
}

impl FlowWorkflows {
    pub fn turns(&self) -> impl DoubleEndedIterator<Item = &FlowTurnProjection> {
        self.source_order
            .iter()
            .flat_map(|index| self.sources[*index].turns())
    }

    pub fn observe(&mut self, entry: &FlowTranscriptEntry) {
        let source = match &entry.record {
            FlowRecord::Input(input) => &input.lease.flow,
            FlowRecord::Publication(publication) => &publication.lease.flow,
            FlowRecord::Completion(completion) => &completion.lease.flow,
            FlowRecord::Execution(record) => record.scope.flow(),
            FlowRecord::Checkpoint(_) | FlowRecord::Legacy(_) | FlowRecord::Continuation { .. } => {
                return;
            }
        };
        let index = *self
            .source_indices
            .entry(source.clone())
            .or_insert_with(|| {
                self.sources.push(SourceWorkflows::default());
                self.sources.len() - 1
            });
        let empty = self.sources[index].turns.is_empty();
        self.sources[index].observe(entry);
        if empty && !self.sources[index].turns.is_empty() {
            self.source_order.push(index);
        }
    }

    pub fn finish_batch(&mut self) {
        for source in &mut self.sources {
            source.finish_batch();
        }
    }
}

#[derive(Default)]
struct SourceWorkflows {
    turns: BTreeMap<u64, FlowTurnProjection>,
    runs: HashMap<FlowRunId, Option<u64>>,
    requests: HashMap<PermissionRequestId, u64>,
    groups: HashMap<PermissionGroupId, (PermissionGroupAudit, bool)>,
    finished_updates: BTreeSet<u64>,
}

impl SourceWorkflows {
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
            Event::FileEditApplied {
                flow_run_id: Some(run),
                ..
            } => self.runs.get(run).copied().flatten(),
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
        Event::FileEditApplied { flow_run_id, .. } => flow_run_id.as_ref(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_sources_isolate_equal_epochs_runs_and_late_service_events() {
        let first = TurnLease {
            flow: FlowRef {
                session_id: uuid::Uuid::now_v7().into(),
                instance_id: atman_rt::FlowInstanceId::now(),
            },
            turn: crate::event::TurnId::now(),
            run: FlowRunId::now(),
            epoch: 1,
        };
        let mut second = first.clone();
        second.flow.instance_id = atman_rt::FlowInstanceId::now();
        let mut workflows = FlowWorkflows::default();
        let mut sequence = 0;
        let mut observe = |scope, event| {
            sequence += 1;
            workflows.observe(&FlowTranscriptEntry {
                id: FlowRecordId {
                    commit: sequence,
                    fact: 0,
                    item: 0,
                },
                record: FlowRecord::Execution(Box::new(crate::flow_store::trace::ExecutionEvent {
                    scope,
                    sequence,
                    timestamp: Utc::now(),
                    event,
                })),
            });
            workflows.finish_batch();
        };
        let edit = || Event::FileEditApplied {
            turn_id: None,
            flow_run_id: Some(first.run.clone()),
            tool_use_id: None,
            tool_name: "fs.edit".into(),
            path: "a.rs".into(),
            metrics: crate::activity::EditMetrics {
                hunks: 1,
                insertions: 8,
                deletions: 2,
            },
        };
        // An unbound service event cannot reserve the first visible source position.
        observe(ExecutionScope::Instance(second.flow.clone()), edit());
        for lease in [&first, &second] {
            observe(
                ExecutionScope::Invocation(lease.clone()),
                Event::FlowStart {
                    run_id: lease.run.clone(),
                    flow_name: "agent".into(),
                    parent_run_id: None,
                    parent_node_id: None,
                    spawned: false,
                },
            );
        }
        observe(ExecutionScope::Instance(second.flow.clone()), edit());
        let turns = workflows.turns().collect::<Vec<_>>();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].lease(), &first);
        assert_eq!(turns[1].lease(), &second);
        assert_eq!(turns[0].workflow().applied_edits().applied_edits, 0);
        assert_eq!(turns[1].workflow().applied_edits().insertions, 8);
    }

    #[test]
    fn applied_edits_follow_the_recorded_lease_and_late_instance_source() {
        use crate::flow_store::{FlowInstanceId, FlowRef, trace::ExecutionEvent};
        let flow = FlowRef {
            session_id: uuid::Uuid::new_v4().into(),
            instance_id: FlowInstanceId::now(),
        };
        let lease = |epoch| TurnLease {
            flow: flow.clone(),
            turn: crate::event::TurnId::now(),
            run: FlowRunId::now(),
            epoch,
        };
        let first = lease(1);
        let second = lease(2);
        let mut projection = FlowWorkflows::default();
        let at = Utc::now();
        let mut sequence = 0;
        let mut observe = |projection: &mut FlowWorkflows, scope, event| {
            sequence += 1;
            projection.observe(&FlowTranscriptEntry {
                id: FlowRecordId {
                    commit: sequence,
                    fact: 0,
                    item: 0,
                },
                record: FlowRecord::Execution(Box::new(ExecutionEvent {
                    scope,
                    sequence,
                    timestamp: at,
                    event,
                })),
            });
            projection.finish_batch();
        };
        for lease in [&first, &second] {
            observe(
                &mut projection,
                ExecutionScope::Invocation(lease.clone()),
                Event::FlowStart {
                    run_id: lease.run.clone(),
                    flow_name: "agent".into(),
                    parent_run_id: None,
                    parent_node_id: None,
                    spawned: false,
                },
            );
        }
        let edit = |run| Event::FileEditApplied {
            turn_id: None,
            flow_run_id: run,
            tool_use_id: None,
            tool_name: "fs.edit".into(),
            path: "a.rs".into(),
            metrics: crate::activity::EditMetrics {
                hunks: 1,
                insertions: 8,
                deletions: 2,
            },
        };
        observe(
            &mut projection,
            ExecutionScope::Instance(flow.clone()),
            edit(Some(first.run.clone())),
        );
        observe(
            &mut projection,
            ExecutionScope::Instance(flow.clone()),
            edit(None),
        );
        observe(
            &mut projection,
            ExecutionScope::Instance(flow),
            edit(Some(FlowRunId::now())),
        );
        let turns = projection.turns().collect::<Vec<_>>();
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[0].workflow().applied_edits().insertions, 8);
        assert_eq!(turns[1].workflow().applied_edits().applied_edits, 0);
    }
}
