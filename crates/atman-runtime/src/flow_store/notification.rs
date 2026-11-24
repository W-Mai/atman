//! Compact notification candidates projected from writer-acknowledged Flow batches.

use std::{io, num::NonZeroUsize};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    FlowFact, FlowJournalBackend, FlowRef, FlowStore,
    completion::InvocationResult,
    invalid,
    position::{FlowBatch, FlowPage, FlowPosition},
};
use crate::{
    event::Event, permission::PermissionRequestId, permission_audit::PermissionAuditTarget,
};

/// A candidate's location in its original commit. The separate batch receipt binds its identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FlowNotification {
    pub fact_index: usize,
    pub event_index: Option<usize>,
    pub at: DateTime<Utc>,
    #[serde(flatten)]
    pub kind: FlowNotificationKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FlowNotificationKind {
    TurnCompleted,
    /// The current User queue still needs to confirm the exact request and revision at delivery.
    ApprovalRequested {
        request_id: PermissionRequestId,
        revision: u64,
    },
}

impl FlowStore {
    pub(crate) fn read_notification_events(
        &mut self,
        after: Option<&FlowPosition>,
        limit: NonZeroUsize,
    ) -> io::Result<FlowPage<FlowNotification>> {
        let address = &self.descriptor.address;
        let FlowJournalBackend::Durable { journal, .. } = &mut self.journal.backend else {
            return Err(invalid("notification events require a durable flow"));
        };
        let after = after.map(FlowPosition::require_durable).transpose()?;
        let page = journal.read_after::<FlowFact>(after, limit)?;
        Ok(FlowPage {
            batches: page
                .batches
                .into_iter()
                .map(|batch| FlowBatch {
                    receipt: batch.receipt.into(),
                    facts: project(address, &batch.facts),
                })
                .collect(),
            next: page.next.map(Into::into),
            has_more: page.has_more,
        })
    }
}

fn project(address: &FlowRef, facts: &[FlowFact]) -> Vec<FlowNotification> {
    let mut notifications = Vec::new();
    let session_id = address.session_id.to_string();
    for (fact_index, fact) in facts.iter().enumerate() {
        match fact {
            FlowFact::TurnCompleted(completion)
                if &completion.lease.flow == address
                    && !matches!(completion.result, InvocationResult::Unobserved(_)) =>
            {
                notifications.push(FlowNotification {
                    fact_index,
                    event_index: None,
                    at: completion.finished_at,
                    kind: FlowNotificationKind::TurnCompleted,
                });
            }
            FlowFact::ExecutionEvents(events) => {
                for (event_index, event) in events.iter().enumerate() {
                    let audit = match &event.event {
                        Event::PermissionRequestCreated { payload }
                        | Event::PermissionRequestTargeted { payload } => payload,
                        _ => continue,
                    };
                    if let Some(request_id) = &audit.request_id
                        && audit.target == PermissionAuditTarget::User
                        && audit.decision_id.is_none()
                        && event.scope.flow() == address
                        && audit.session_id == session_id
                    {
                        notifications.push(FlowNotification {
                            fact_index,
                            event_index: Some(event_index),
                            at: audit.at,
                            kind: FlowNotificationKind::ApprovalRequested {
                                request_id: request_id.clone(),
                                revision: audit.revision,
                            },
                        });
                    }
                }
            }
            _ => {}
        }
    }
    notifications
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        flow_runtime::FlowWorker,
        flow_store::{
            EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowTransition, RuntimeStore,
            SessionDescriptor,
            completion::StoredValue,
            trace::{ExecutionEvent, ExecutionScope},
        },
        permission_audit::{PermissionPolicyReference, PermissionRequestAudit},
        source_program::RevisionBundle,
        tool::Tier,
    };
    use atman_rt::{RunId, TurnId};
    use uuid::Uuid;

    #[tokio::test]
    async fn notification_pages_retain_committed_positions_and_project_only_owned_candidates() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = RuntimeStore::open(directory.path()).unwrap();
        let id = Uuid::new_v4();
        let mut session = runtime
            .create_session(SessionDescriptor {
                id,
                project: None,
                configuration: Default::default(),
                title: "Notification source".into(),
            })
            .unwrap();
        let address = FlowRef {
            session_id: id.into(),
            instance_id: FlowInstanceId::now(),
        };
        let mut store = session
            .create_flow(FlowDescriptor {
                address: address.clone(),
                creation_request: Uuid::new_v4(),
                created_by: None,
                content: EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: RevisionBundle {
                        entry: "main.at".into(),
                        sources: [("main.at".into(), "flow main() { }".into())].into(),
                        edges: Default::default(),
                    },
                }
                .into(),
            })
            .unwrap();
        store
            .transition(store.state().revision(), FlowTransition::Activated)
            .unwrap();
        let lease = store
            .state()
            .clone()
            .begin_turn(TurnId::now(), RunId::now())
            .unwrap();
        store
            .transition(
                store.state().revision(),
                FlowTransition::TurnStarted {
                    turn: lease.turn.clone(),
                    run: lease.run.clone(),
                },
            )
            .unwrap();
        let start = store.cursor().clone();
        let worker = FlowWorker::start(store).await.unwrap();
        let request_id = PermissionRequestId::now();
        let audit = PermissionRequestAudit {
            request_id: Some(request_id.clone()),
            revision: 2,
            session_id: id.to_string(),
            requesting_run_id: lease.run.clone(),
            parent_run_id: None,
            root_run_id: lease.run.clone(),
            tool_use_id: "private-tool-call".into(),
            tool: "private-tool-name".into(),
            call_intent: None,
            tier: Tier::Two,
            execution_boundary: None,
            provenance: Default::default(),
            target: PermissionAuditTarget::User,
            group_ids: vec![],
            policy: PermissionPolicyReference {
                snapshot_id: "private-policy".into(),
                rule_id: "private-rule".into(),
            },
            escalation_path: vec![],
            decision_id: None,
            user_approval: None,
            actor: None,
            scope: None,
            reason: Some("private-reason".into()),
            at: Utc::now(),
        };
        let events = [
            Event::PermissionRequestCreated {
                payload: PermissionRequestAudit {
                    decision_id: Some("already-decided".into()),
                    ..audit.clone()
                },
            },
            Event::PermissionRequestCreated {
                payload: PermissionRequestAudit {
                    target: PermissionAuditTarget::Flow {
                        run_id: lease.run.clone(),
                    },
                    ..audit.clone()
                },
            },
            Event::PermissionRequestTargeted { payload: audit },
        ];
        let scope = ExecutionScope::Invocation(lease.clone());
        let approval_receipt = worker
            .request(move |store| {
                store.append_execution_events(
                    events
                        .into_iter()
                        .zip(1..)
                        .map(|(event, sequence)| ExecutionEvent {
                            scope: scope.clone(),
                            sequence,
                            timestamp: Utc::now(),
                            event,
                        })
                        .collect(),
                )
            })
            .await
            .unwrap();
        let completion = worker
            .request(move |store| {
                store.finish_turn(
                    lease,
                    InvocationResult::Returned(StoredValue::String("private-answer".into())),
                )
            })
            .await
            .unwrap();

        let limit = NonZeroUsize::MIN;
        let observed = worker
            .read_notification_events(Some(start), limit)
            .await
            .unwrap();
        assert!(observed.has_more);
        assert_eq!(observed.batches[0].receipt, approval_receipt);
        assert_eq!(observed.batches[0].facts.len(), 1);
        let candidate = &observed.batches[0].facts[0];
        assert_eq!((candidate.fact_index, candidate.event_index), (0, Some(2)));
        assert_eq!(
            candidate.kind,
            FlowNotificationKind::ApprovalRequested {
                request_id,
                revision: 2,
            }
        );
        let finished = worker
            .read_notification_events(observed.next, limit)
            .await
            .unwrap();
        assert!(!finished.has_more);
        assert_eq!(finished.batches[0].receipt, completion.receipt);
        assert_eq!(finished.batches[0].facts.len(), 1);
        assert_eq!(
            finished.batches[0].facts[0].kind,
            FlowNotificationKind::TurnCompleted
        );
        for encoded in [
            serde_json::to_string(&observed.batches).unwrap(),
            serde_json::to_string(&finished).unwrap(),
        ] {
            assert!(!encoded.contains("private-"));
        }
        let mut forged = completion.receipt.require_durable().unwrap().clone();
        forged.digest = "0".repeat(64);
        assert!(
            worker
                .read_notification_events(Some(forged.into()), limit)
                .await
                .is_err()
        );
        worker.shutdown().await.unwrap();
    }
}
