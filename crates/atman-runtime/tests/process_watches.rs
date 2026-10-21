mod common;

use atman_rt::{RunId, TurnId};
use atman_runtime::flow_runtime::FlowWorker;
use atman_runtime::flow_store::inbox::{InboxDisposition, InboxPayload, InputContent};
use atman_runtime::flow_store::process_watch::{
    ProcessMatch, ProcessReport, ProcessSignal, ProcessSource, ProcessWatch, ProcessWatchStatus,
};
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowStore, FlowTransition,
    RuntimeStore, SessionDescriptor, SessionStore,
};
use atman_runtime::source_program::RevisionBundle;
use chrono::Utc;
use uuid::Uuid;

fn fixture() -> (tempfile::TempDir, SessionStore, FlowStore) {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            configuration: Default::default(),
            id,
            title: "Process observation".into(),
            project: None,
        })
        .unwrap();
    let mut flow = session
        .create_flow(FlowDescriptor {
            address: FlowRef {
                session_id: id.into(),
                instance_id: FlowInstanceId::now(),
            },
            creation_request: Uuid::new_v4(),
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: "main.at".into(),
                bundle: RevisionBundle {
                    entry: "entry:main.at".into(),
                    sources: [(
                        "entry:main.at".into(),
                        "flow main() -> int { return 1 }".into(),
                    )]
                    .into(),
                    edges: Default::default(),
                },
            }
            .into(),
        })
        .unwrap();
    transition(&mut flow, FlowTransition::Activated);
    (dir, session, flow)
}

fn transition(flow: &mut FlowStore, transition: FlowTransition) {
    flow.transition(flow.state().revision(), transition)
        .unwrap();
}

fn source() -> ProcessSource {
    ProcessSource::Bash {
        incarnation: Uuid::new_v4(),
        handle: "bg-test".into(),
    }
}

fn watch(source: &ProcessSource, once: bool) -> ProcessWatch {
    ProcessWatch {
        id: Uuid::new_v4(),
        source: source.clone(),
        pattern: "ready".into(),
        once,
        expires_at: None,
    }
}

fn report(source: &ProcessSource, position: u64, watchers: &[Uuid]) -> ProcessReport {
    ProcessReport {
        source: source.clone(),
        position,
        observed_at: Utc::now(),
        signal: ProcessSignal::Matches(
            watchers
                .iter()
                .map(|id| ProcessMatch {
                    watcher: *id,
                    text: "ready".into(),
                    row: None,
                    col: None,
                })
                .collect(),
        ),
    }
}

#[tokio::test]
async fn watcher_catalog_uses_observation_provenance_and_does_not_recover_or_consume() {
    use atman_runtime::flow_runtime::catalog::SessionCatalog;
    use atman_runtime::flow_store::inbox::{DeliveryMode, DeliveryRequest, SenderIdentity};
    use std::io::Write;

    let (dir, session, mut flow) = fixture();
    let source = source();
    let watch = watch(&source, false);
    let address = flow.descriptor().address.clone();
    flow.register_process_watch(watch.clone(), 0).unwrap();
    flow.observe_process(report(&source, 1, &[watch.id]))
        .unwrap();
    let payload = common::pending_bodies(&mut flow)
        .first()
        .unwrap()
        .request
        .payload
        .clone();
    for payload in [
        payload,
        InboxPayload::ProcessEvent {
            source,
            content: serde_json::json!({"unrelated": true}),
        },
    ] {
        flow.accept_input(
            DeliveryRequest {
                request_id: Uuid::new_v4(),
                sender: SenderIdentity::Host(Uuid::new_v4()),
                recipient: address.clone(),
                delivery: DeliveryMode::AtBoundary,
                cause: None,
                correlation: None,
                scope: None,
                payload,
            },
            |_| Ok(()),
        )
        .unwrap();
    }
    let catalog = SessionCatalog::new(None, Some(dir.path().to_owned()));
    let log = flow.dir().join("events.jsonl");
    let bytes = std::fs::read(&log).unwrap();
    let inspected = catalog.inspect_flow(&address, |_| Ok(())).await.unwrap();
    assert_eq!(inspected.journal.cursor.as_ref(), Some(flow.cursor()));
    assert_eq!(inspected.watchers.len(), 1);
    assert_eq!(inspected.watchers[0].progress.matched, 1);
    assert_eq!(inspected.watchers[0].progress.pending, 1);
    assert_eq!(flow.pending_inputs().count(), 3);
    let calls = std::cell::Cell::new(0);
    let denied = catalog
        .inspect_flow(&address, |_| {
            calls.set(calls.get() + 1);
            if calls.get() == 3 {
                Err(std::io::ErrorKind::PermissionDenied.into())
            } else {
                Ok(())
            }
        })
        .await;
    assert!(matches!(denied, Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied));
    assert_eq!(std::fs::read(&log).unwrap(), bytes);
    transition(&mut flow, FlowTransition::CloseRequested);
    let closed = catalog.inspect_flow(&address, |_| Ok(())).await.unwrap();
    assert_eq!(closed.watchers[0].progress.pending, 0);
    assert_eq!(closed.watchers[0].progress.cancelled, 1);
    let mut expected = std::fs::read(&log).unwrap();
    drop(flow);
    drop(session);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(b"{partial")
        .unwrap();
    expected.extend_from_slice(b"{partial");
    let inspected = catalog.inspect_flow(&address, |_| Ok(())).await.unwrap();
    assert_eq!(inspected.journal.trailing_bytes, 8);
    assert_eq!(inspected.watchers[0].progress.cancelled, 1);
    assert_eq!(std::fs::read(&log).unwrap(), expected);
}

#[test]
fn output_gap_retires_old_watches_without_replacing_the_live_process() {
    let (_dir, session, mut flow) = fixture();
    let source = source();
    let old = watch(&source, false);
    flow.register_process_watch(old.clone(), 0).unwrap();
    let gap = ProcessReport {
        source: source.clone(),
        position: 1,
        observed_at: Utc::now(),
        signal: ProcessSignal::HistoryGap,
    };
    let receipt = flow.observe_process(gap.clone()).unwrap();
    assert_eq!(flow.observe_process(gap).unwrap(), receipt);
    assert_eq!(
        flow.process_watches().next().unwrap().status,
        ProcessWatchStatus::HistoryGap
    );
    let fresh = watch(&source, true);
    flow.register_process_watch(fresh.clone(), 1).unwrap();
    flow.observe_process(report(&source, 2, &[old.id, fresh.id]))
        .unwrap();
    let inputs: Vec<_> = common::pending_bodies(&mut flow);
    assert_eq!(inputs.len(), 2);
    let InboxPayload::ProcessEvent { content, .. } = &inputs[1].request.payload else {
        panic!()
    };
    assert_eq!(content["subscriptions"].as_array().unwrap().len(), 1);
    let address = flow.descriptor().address.clone();
    let inspection = session.inspect_flow(&address.instance_id).unwrap();
    assert_eq!(inspection.watchers.len(), 2);
    for record in &inspection.watchers {
        assert_eq!(
            record.progress.matched,
            usize::from(record.registration.id() == fresh.id)
        );
        assert_eq!(record.progress.pending, 1);
    }
    drop(flow);
    let mut flow = session.open_flow(&address.instance_id).unwrap();
    assert_eq!(common::pending_bodies(&mut flow), inputs);
}

#[test]
fn source_matches_share_one_inbox_input_and_survive_pause_unwatch_and_reopen() {
    let (_dir, session, mut flow) = fixture();
    let source = source();
    let once = watch(&source, true);
    let persist = watch(&source, false);
    flow.register_process_watch(once.clone(), 7).unwrap();
    flow.register_process_watch(persist.clone(), 7).unwrap();
    transition(
        &mut flow,
        FlowTransition::ExecutionPaused {
            key: Uuid::nil().into(),
        },
    );
    let report = report(&source, 8, &[once.id, persist.id]);
    let receipt = flow.observe_process(report.clone()).unwrap();
    let input = common::pending_bodies(&mut flow)
        .into_iter()
        .next()
        .unwrap();
    assert_eq!(flow.pending_inputs().count(), 1);
    let InboxPayload::ProcessEvent { content, .. } = &input.request.payload else {
        panic!("process provenance missing");
    };
    assert_eq!(content["subscriptions"].as_array().unwrap().len(), 2);
    assert_eq!(content["matches"].as_array().unwrap().len(), 2);
    assert_eq!(flow.observe_process(report.clone()).unwrap(), receipt);
    let mut conflicting = report.clone();
    conflicting.signal = ProcessSignal::Exited;
    assert!(flow.observe_process(conflicting).is_err());
    flow.unregister_process_watch(persist.id).unwrap();
    let mut late = report.clone();
    late.position += 1;
    flow.observe_process(late).unwrap();
    assert_eq!(flow.pending_inputs().count(), 1);
    let address = flow.descriptor().address.clone();
    let inspection = session.inspect_flow(&address.instance_id).unwrap();
    assert_eq!(inspection.watchers.len(), 2);
    for record in &inspection.watchers {
        assert_eq!(record.progress.matched, 1);
        assert_eq!(record.progress.pending, 1);
        assert_eq!(record.progress.input_committed, 0);
        assert_eq!(
            record.progress.last_delivery.as_ref().unwrap().message_id,
            input.id
        );
    }
    drop(flow);
    let mut flow = session.open_flow(&address.instance_id).unwrap();
    assert_eq!(flow.observe_process(report).unwrap(), receipt);
    assert_eq!(common::pending_bodies(&mut flow).first().unwrap(), &input);
    let record = flow.register_process_watch(once, 99).unwrap();
    assert_eq!(record.started_after, 7);
    assert_eq!(record.status, ProcessWatchStatus::Matched);
    transition(
        &mut flow,
        FlowTransition::ExecutionResumed {
            key: Uuid::nil().into(),
        },
    );
    let turn = TurnId::now();
    let run = RunId::now();
    let lease = flow
        .state()
        .clone()
        .begin_turn(turn.clone(), run.clone())
        .unwrap();
    transition(
        &mut flow,
        FlowTransition::TurnStarted {
            turn: turn.clone(),
            run: run.clone(),
        },
    );
    let reservation = flow
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    let message = atman_runtime::flow_runtime::input::context_message(&input, &turn).unwrap();
    assert_eq!(
        message.origin,
        atman_runtime::message::MessageOrigin::Watcher
    );
    flow.commit_input(
        &reservation,
        InputContent::Context {
            messages: vec![message],
        },
    )
    .unwrap();
    assert_eq!(flow.pending_inputs().count(), 0);
    assert!(matches!(
        flow.inbox_entries()[0].acceptance.disposition,
        InboxDisposition::InputCommitted { .. }
    ));
    for record in session.inspect_flow(&address.instance_id).unwrap().watchers {
        assert_eq!(record.progress.matched, 1);
        assert_eq!(record.progress.pending, 0);
        assert_eq!(record.progress.input_committed, 1);
    }
}

#[test]
fn absolute_expiry_preserves_matches_and_never_extends_or_duplicates_deadlines() {
    let (_dir, session, mut flow) = fixture();
    let source = source();
    let mut bounded = watch(&source, false);
    let expires = Utc::now() + chrono::Duration::days(2);
    bounded.expires_at = Some(expires);
    flow.register_process_watch(bounded.clone(), 0).unwrap();
    let unbounded = watch(&source, false);
    flow.register_process_watch(unbounded.clone(), 0).unwrap();
    flow.observe_process(report(&source, 1, &[bounded.id, unbounded.id]))
        .unwrap();
    let mut boundary = report(&source, 2, &[bounded.id, unbounded.id]);
    boundary.observed_at = expires;
    flow.observe_process(boundary).unwrap();
    let inputs: Vec<_> = common::pending_bodies(&mut flow);
    let InboxPayload::ProcessEvent { content, .. } = &inputs[1].request.payload else {
        panic!();
    };
    assert_eq!(content["subscriptions"].as_array().unwrap().len(), 1);
    let receipt = flow.expire_process_watches(expires).unwrap();
    assert_eq!(
        flow.expire_process_watches(expires + chrono::Duration::days(1))
            .unwrap(),
        receipt
    );
    assert_eq!(flow.pending_inputs().count(), 3);
    let address = flow.descriptor().address.clone();
    let inspection = session.inspect_flow(&address.instance_id).unwrap();
    let expired = inspection
        .watchers
        .iter()
        .find(|record| record.registration.id() == bounded.id)
        .unwrap();
    assert_eq!(expired.progress.matched, 1);
    assert_eq!(expired.progress.pending, 2);
    assert_eq!(
        expired.progress.last_delivery.as_ref().unwrap().event,
        "expired"
    );
    drop(flow);
    let mut flow = session.open_flow(&address.instance_id).unwrap();
    assert_eq!(flow.expire_process_watches(expires).unwrap(), receipt);
    for envelope in common::pending_bodies(&mut flow) {
        assert_eq!(
            flow.inbox_entry(envelope.id).unwrap().unwrap().envelope,
            envelope
        );
    }
    let record = flow.register_process_watch(bounded, 99).unwrap();
    assert_eq!(record.status, ProcessWatchStatus::Expired);
    assert_eq!(record.watch.expires_at, Some(expires));
    assert_eq!(
        flow.process_watches()
            .find(|record| record.watch.id == unbounded.id)
            .unwrap()
            .status,
        ProcessWatchStatus::Active
    );
    let bodies = common::pending_bodies(&mut flow);
    let InboxPayload::Timer { due_at, .. } = &bodies.last().unwrap().request.payload else {
        panic!();
    };
    assert_eq!(*due_at, expires);
}

#[tokio::test]
async fn new_resource_owner_reports_lost_incarnations_once_without_losing_accepted_matches() {
    let (_dir, session, mut flow) = fixture();
    let source = source();
    let watching = watch(&source, false);
    flow.register_process_watch(watching.clone(), 100).unwrap();
    flow.observe_process(report(&source, 101, &[watching.id]))
        .unwrap();
    transition(
        &mut flow,
        FlowTransition::ExecutionPaused {
            key: Uuid::nil().into(),
        },
    );
    let worker = FlowWorker::start(flow).await.unwrap();
    let flow = worker.shutdown().await.unwrap();
    assert_eq!(flow.pending_inputs().count(), 2);
    assert_eq!(
        flow.process_watches().next().unwrap().status,
        ProcessWatchStatus::SourceLost
    );
    let cursor = flow.cursor().clone();
    let address = flow.descriptor().address.clone();
    drop(flow);
    let flow = session.open_flow(&address.instance_id).unwrap();
    let worker = FlowWorker::start(flow).await.unwrap();
    let mut flow = worker.shutdown().await.unwrap();
    assert_eq!(flow.cursor(), &cursor);
    let bodies = common::pending_bodies(&mut flow);
    let InboxPayload::ProcessEvent { content, .. } = &bodies.last().unwrap().request.payload else {
        panic!();
    };
    assert_eq!(content["signal"], "source_lost");
    assert!(
        flow.observe_process(report(&source, 103, &[watching.id]))
            .is_err()
    );
    assert!(
        flow.register_process_watch(watch(&source, false), 102)
            .is_err()
    );
    let replacement = ProcessSource::Bash {
        incarnation: Uuid::new_v4(),
        handle: "bg-test".into(),
    };
    let new_watch = watch(&replacement, true);
    flow.register_process_watch(new_watch.clone(), 0).unwrap();
    flow.observe_process(report(&replacement, 1, &[new_watch.id]))
        .unwrap();
    assert_eq!(flow.pending_inputs().count(), 3);
}

#[test]
fn close_rejects_new_deliveries_but_preserves_original_receipts() {
    let (_dir, session, mut flow) = fixture();
    let source = source();
    let watching = watch(&source, false);
    flow.register_process_watch(watching.clone(), 0).unwrap();
    let report = report(&source, 1, &[watching.id]);
    let receipt = flow.observe_process(report.clone()).unwrap();
    transition(&mut flow, FlowTransition::CloseRequested);
    assert!(
        flow.process_watches()
            .all(|record| record.status == ProcessWatchStatus::Cancelled)
    );
    assert_eq!(flow.pending_inputs().count(), 0);
    assert!(matches!(
        flow.inbox_entries()[0].acceptance.disposition,
        InboxDisposition::Cancelled { .. }
    ));
    assert_eq!(flow.observe_process(report.clone()).unwrap(), receipt);
    let mut late = report.clone();
    late.position = 2;
    assert!(flow.observe_process(late).is_err());
    assert!(
        flow.register_process_watch(watch(&source, false), 1)
            .is_err()
    );
    let address = flow.descriptor().address.clone();
    drop(flow);
    let mut flow = session.open_flow(&address.instance_id).unwrap();
    assert_eq!(flow.observe_process(report).unwrap(), receipt);
    assert!(
        flow.process_watches()
            .all(|record| record.status == ProcessWatchStatus::Cancelled)
    );
}

#[test]
fn malformed_matching_reports_leave_storage_unchanged() {
    let (_dir, _session, mut flow) = fixture();
    let source = source();
    let watching = watch(&source, true);
    flow.register_process_watch(watching.clone(), 9).unwrap();
    let cursor = flow.cursor().clone();
    assert!(
        flow.observe_process(report(&source, 10, &[watching.id, watching.id]))
            .is_err()
    );
    assert!(
        flow.observe_process(report(&source, 10, &[Uuid::new_v4()]))
            .is_err()
    );
    let other = ProcessSource::Terminal {
        incarnation: Uuid::new_v4(),
        handle: "term-test".into(),
    };
    assert!(
        flow.observe_process(report(&other, 10, &[watching.id]))
            .is_err()
    );
    let mut mismatch = report(&source, 10, &[watching.id]);
    mismatch.signal = ProcessSignal::Matches(vec![ProcessMatch {
        watcher: watching.id,
        text: "wrong text".into(),
        row: None,
        col: None,
    }]);
    assert!(flow.observe_process(mismatch).is_err());
    assert_eq!(flow.cursor(), &cursor);
    assert_eq!(flow.pending_inputs().count(), 0);
}

#[test]
fn terminal_reports_retire_all_remaining_watches_and_replay_in_the_view_feed() {
    let (_dir, session, mut flow) = fixture();
    let source = ProcessSource::Terminal {
        incarnation: Uuid::new_v4(),
        handle: "term-test".into(),
    };
    let first = watch(&source, false);
    let mut second = watch(&source, true);
    second.expires_at = Some(Utc::now() + chrono::Duration::hours(1));
    flow.register_process_watch(first, 40).unwrap();
    flow.register_process_watch(second.clone(), 42).unwrap();
    let mut report = ProcessReport {
        source,
        position: 41,
        observed_at: second.expires_at.unwrap(),
        signal: ProcessSignal::Exited,
    };
    let cursor = flow.cursor().clone();
    assert!(flow.observe_process(report.clone()).is_err());
    assert_eq!(flow.cursor(), &cursor);
    report.position = 43;
    flow.observe_process(report).unwrap();
    assert!(
        flow.process_watches()
            .all(|record| record.status == ProcessWatchStatus::Exited)
    );
    assert_eq!(flow.pending_inputs().count(), 1);
    flow.expire_process_watches(second.expires_at.unwrap())
        .unwrap();
    assert_eq!(flow.pending_inputs().count(), 1);
    let address = flow.descriptor().address.clone();
    drop(flow);
    let mut flow = session.open_flow(&address.instance_id).unwrap();
    let page = flow
        .read_updates(Some(&cursor), std::num::NonZeroUsize::new(4).unwrap())
        .unwrap();
    let atman_runtime::flow_store::view::FlowUpdate::ProcessObserved(observation) =
        &page.batches[0].facts[0]
    else {
        panic!("missing typed process update");
    };
    assert_eq!(observation.updates.len(), 2);
    assert_eq!(
        observation.envelope.as_ref(),
        common::pending_bodies(&mut flow).first()
    );
}
