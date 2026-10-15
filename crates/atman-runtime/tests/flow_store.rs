use std::collections::BTreeMap;

use atman_rt::{
    InstanceActivity, InstanceState, Interruption, RunId, TurnId, TurnLease, TurnOutcome, TurnPhase,
};
use atman_runtime::event::AtmanUuid;
use atman_runtime::flow_store::inbox::{
    DeliveryMode, DeliveryRequest, InboxDisposition, InboxPayload, InputContent, SenderIdentity,
};
use atman_runtime::flow_store::waiter::{ReplyStatus, ReplyWaiter};
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowStore, FlowTransition,
    RuntimeStore, SessionDescriptor,
};
use atman_runtime::message::Message;
use atman_runtime::source_program::RevisionBundle;
use uuid::Uuid;

fn descriptor(session: Uuid) -> FlowDescriptor {
    FlowDescriptor {
        address: FlowRef {
            session_id: session.into(),
            instance_id: FlowInstanceId::now(),
        },
        creation_request: Uuid::new_v4(),
        entry: EntrySnapshot {
            name: "main".into(),
            path: "/missing/project/main.at".into(),
            bundle: RevisionBundle {
                entry: "entry:main.at".into(),
                sources: BTreeMap::from([(
                    "entry:main.at".into(),
                    "flow main() -> int { return 7 }".into(),
                )]),
                edges: BTreeMap::new(),
            },
        },
        created_by: None,
    }
}

fn session_descriptor() -> SessionDescriptor {
    SessionDescriptor {
        id: Uuid::new_v4(),
        project_root: Some("/missing/project".into()),
        title: "Example".into(),
    }
}

fn change(flow: &mut FlowStore, transition: FlowTransition) {
    flow.transition(flow.state().revision(), transition)
        .unwrap();
}

fn start(flow: &mut FlowStore) -> TurnLease<AtmanUuid> {
    change(
        flow,
        FlowTransition::TurnStarted {
            turn: TurnId::now(),
            run: RunId::now(),
        },
    );
    let InstanceState::Open(InstanceActivity::Active(active)) = flow.state().state() else {
        panic!("turn was not started");
    };
    active.lease.clone()
}

#[test]
fn separated_journals_restore_peers_and_ignore_corrupt_metadata_without_touching_legacy() {
    let dir = tempfile::tempdir().unwrap();
    let session_info = session_descriptor();
    let legacy_dir = dir
        .path()
        .join("sessions")
        .join(session_info.id.to_string());
    std::fs::create_dir_all(&legacy_dir).unwrap();
    std::fs::write(legacy_dir.join("events.jsonl"), "legacy history\n").unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let mut session = root.create_session(session_info.clone()).unwrap();
    let a_info = descriptor(session_info.id);
    let mut b_info = descriptor(session_info.id);
    b_info.created_by = Some(a_info.address.clone());
    let mut a = session.create_flow(a_info.clone()).unwrap();
    let mut b = session.create_flow(b_info.clone()).unwrap();
    let a_path = a.dir().to_path_buf();
    let session_path = session.dir().to_path_buf();
    assert_eq!(
        a_path,
        session_path
            .join("flows")
            .join(a_info.address.instance_id.to_string())
    );
    change(&mut a, FlowTransition::Activated);
    change(&mut b, FlowTransition::Activated);
    let lease = start(&mut a);
    change(
        &mut a,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    );
    let pause = AtmanUuid(Uuid::new_v4());
    change(&mut a, FlowTransition::ExecutionPaused { key: pause });
    session
        .select_default(b_info.address.instance_id.clone())
        .unwrap();
    let a_state = a.state().clone();
    let b_state = b.state().clone();
    drop((session, a, b));
    std::fs::write(a_path.join("flow.json"), "corrupt cache").unwrap();
    std::fs::write(session_path.join("session.json"), "corrupt cache").unwrap();
    let session = root.open_session(session_info.id).unwrap();
    let a = session.open_flow(&a_info.address.instance_id).unwrap();
    let b = session.open_flow(&b_info.address.instance_id).unwrap();
    assert_eq!(session.projection().flows.len(), 2);
    assert_eq!(
        session.projection().default_flow,
        Some(b_info.address.instance_id)
    );
    assert_eq!(a.descriptor(), &a_info);
    assert_eq!(a.state(), &a_state);
    assert_eq!(b.state(), &b_state);
    assert!(!a.state().can_start_turn());
    assert!(b.state().can_start_turn());
    assert!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(a_path.join("flow.json")).unwrap()
        )
        .is_ok()
    );
    assert_eq!(
        std::fs::read_to_string(legacy_dir.join("events.jsonl")).unwrap(),
        "legacy history\n"
    );
}

#[test]
fn restored_active_turn_is_interrupted_once_and_keeps_independent_pause_controls() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let old = start(&mut flow);
    let pause = AtmanUuid(Uuid::new_v4());
    change(&mut flow, FlowTransition::ExecutionPaused { key: pause });
    let cursor = flow.cursor().sequence;
    drop((flow, session));
    let session = root.open_session(info.id).unwrap();
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(flow.cursor().sequence, cursor + 1);
    assert!(matches!(
        flow.state().state(),
        InstanceState::Open(InstanceActivity::Interrupted {
            reason: Interruption::HostLost,
            ..
        })
    ));
    assert_eq!(flow.state().pauses(), &[pause]);
    assert!(flow.interrupt_lost_turn().unwrap().is_none());
    change(&mut flow, FlowTransition::InterruptionRecovered);
    assert!(!flow.state().can_start_turn());
    change(&mut flow, FlowTransition::ExecutionResumed { key: pause });
    let new = start(&mut flow);
    assert!(new.epoch > old.epoch);
    let before = flow.state().clone();
    assert!(
        flow.transition(
            flow.state().revision(),
            FlowTransition::TurnFinished {
                lease: old,
                outcome: TurnOutcome::Completed
            }
        )
        .is_err()
    );
    assert_eq!(flow.state(), &before);
}

#[test]
fn lost_closing_turn_still_requires_explicit_resource_cleanup() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    start(&mut flow);
    change(&mut flow, FlowTransition::CloseRequested);
    drop(flow);
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(
        flow.state().state(),
        &InstanceState::Closing { active: None }
    );
    assert!(!flow.state().accepts_input());
    change(&mut flow, FlowTransition::Closed);
    drop(flow);
    let flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(flow.state().state(), &InstanceState::Closed);
}

#[test]
fn owner_lease_survives_session_handle_and_prevents_duplicate_writers() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let flow = session.create_flow(descriptor.clone()).unwrap();
    assert!(root.open_session(info.id).is_err());
    assert!(session.open_flow(&descriptor.address.instance_id).is_err());
    drop(session);
    assert!(root.open_session(info.id).is_err());
    drop(flow);
    assert!(root.open_session(info.id).is_ok());
}

#[test]
fn prepared_orphan_is_unaddressable_until_registration_and_retry_is_idempotent() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let path = session.dir().join("events.jsonl");
    let before_registration = std::fs::read(&path).unwrap();
    let descriptor = descriptor(info.id);
    let flow = session.create_flow(descriptor.clone()).unwrap();
    drop((flow, session));
    // Simulate a crash after the flow's creation commit, before the session registration commit.
    std::fs::write(&path, before_registration).unwrap();
    let mut session = root.open_session(info.id).unwrap();
    assert!(session.projection().flows.is_empty());
    assert!(session.open_flow(&descriptor.address.instance_id).is_err());
    let flow = session.create_flow(descriptor.clone()).unwrap();
    assert_eq!(flow.state().state(), &InstanceState::Prepared);
    let receipt = session.cursor().clone();
    drop(flow);
    drop(session.create_flow(descriptor.clone()).unwrap());
    assert_eq!(session.cursor(), &receipt);
    let mut conflict = descriptor.clone();
    conflict.entry.name = "different".into();
    assert!(session.create_flow(conflict).is_err());
    let mut conflict = descriptor;
    conflict.address.instance_id = FlowInstanceId::now();
    assert!(session.create_flow(conflict).is_err());
    assert_eq!(session.projection().flows.len(), 1);
}

#[test]
fn stale_or_invalid_controls_do_not_commit_and_projection_failures_do_not_erase_facts() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    let before = flow.cursor().clone();
    assert!(flow.transition(999, FlowTransition::Activated).is_err());
    assert!(flow.transition(0, FlowTransition::Closed).is_err());
    assert_eq!(flow.cursor(), &before);
    let cache = flow.dir().join("flow.json");
    std::fs::remove_file(&cache).unwrap();
    std::fs::create_dir(&cache).unwrap();
    change(&mut flow, FlowTransition::Activated);
    assert!(flow.take_projection_error().is_some());
    let committed = flow.cursor().clone();
    drop(flow);
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(flow.cursor(), &committed);
    assert!(flow.state().can_start_turn());
    assert!(flow.take_projection_error().is_some());
}

fn delivery(recipient: &FlowRef, sender: SenderIdentity, text: &str) -> DeliveryRequest {
    DeliveryRequest {
        request_id: Uuid::new_v4(),
        sender,
        recipient: recipient.clone(),
        delivery: DeliveryMode::AtBoundary,
        cause: None,
        correlation: None,
        scope: None,
        payload: InboxPayload::Interjection { text: text.into() },
    }
}

#[test]
fn accepted_retries_survive_restart_revocation_and_close_without_duplicate_input() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let request = delivery(
        &descriptor.address,
        SenderIdentity::User(Uuid::new_v4()),
        "hello",
    );
    let accepted = flow.accept_input(request.clone(), |_| Ok(())).unwrap();
    assert_eq!(flow.pending_inputs().count(), 1);
    assert_eq!(
        flow.accept_input(request.clone(), |_| panic!(
            "accepted retry must not reauthorize new delivery"
        ))
        .unwrap(),
        accepted
    );
    let mut conflict = request.clone();
    conflict.payload = InboxPayload::Interjection {
        text: "changed".into(),
    };
    assert!(flow.accept_input(conflict, |_| Ok(())).is_err());
    drop(flow);
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(
        flow.accept_input(request.clone(), |_| panic!("retry after restart"))
            .unwrap(),
        accepted
    );
    change(&mut flow, FlowTransition::CloseRequested);
    let close_receipt = flow.cursor().clone();
    assert_eq!(flow.pending_inputs().count(), 0);
    let retried = flow
        .accept_input(request.clone(), |_| panic!("retry after close"))
        .unwrap();
    assert_eq!(retried.message_id, accepted.message_id);
    assert_eq!(retried.receipt, accepted.receipt);
    assert_eq!(
        retried.disposition,
        InboxDisposition::Cancelled { by: close_receipt }
    );
    let new = delivery(&descriptor.address, request.sender.clone(), "later");
    assert!(flow.accept_input(new, |_| Ok(())).is_err());
    change(&mut flow, FlowTransition::Closed);
    drop((flow, session));
    let session = root.open_session(info.id).unwrap();
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(
        flow.accept_input(request, |_| panic!("closed retry"))
            .unwrap(),
        retried
    );
    assert_eq!(flow.inbox_entries().len(), 1);
}

#[test]
fn paused_and_interrupted_flows_keep_ordered_inputs_without_resuming_execution() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    let sender = SenderIdentity::User(Uuid::new_v4());
    assert!(
        flow.accept_input(
            delivery(&descriptor.address, sender.clone(), "unprepared"),
            |_| Ok(())
        )
        .is_err()
    );
    change(&mut flow, FlowTransition::Activated);
    let lease = start(&mut flow);
    let key = AtmanUuid(Uuid::new_v4());
    change(&mut flow, FlowTransition::ExecutionPaused { key });
    let first = flow
        .accept_input(
            delivery(&descriptor.address, sender.clone(), "first"),
            |_| Ok(()),
        )
        .unwrap();
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Interrupted(Interruption::Cancelled),
        },
    );
    let second = flow
        .accept_input(delivery(&descriptor.address, sender, "second"), |_| Ok(()))
        .unwrap();
    assert!(!flow.state().can_start_turn());
    drop(flow);
    let flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(
        flow.pending_inputs()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        [first.message_id, second.message_id]
    );
    assert!(!flow.state().can_start_turn());
    assert_eq!(flow.state().pauses(), &[key]);
}

#[test]
fn delivery_identity_scope_and_authorization_reject_invalid_new_acceptance() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let sender = SenderIdentity::Flow(FlowRef {
        session_id: info.id.into(),
        instance_id: FlowInstanceId::now(),
    });
    let request = delivery(&descriptor.address, sender, "peer data");
    let before = flow.cursor().clone();
    assert!(
        flow.accept_input(request.clone(), |_| Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "revoked"
        )))
        .is_err()
    );
    assert_eq!(flow.cursor(), &before);
    let mut impersonation = request.clone();
    impersonation.payload = InboxPayload::UserSubmission {
        text: "user authority".into(),
        images: Vec::new(),
        invocation: BTreeMap::new(),
        presentation: None,
    };
    assert!(
        flow.accept_input(impersonation, |_| panic!(
            "invalid source passed validation"
        ))
        .is_err()
    );
    let mut foreign = request.clone();
    foreign.recipient.instance_id = FlowInstanceId::now();
    assert!(flow.accept_input(foreign, |_| Ok(())).is_err());
    let first = flow.accept_input(request.clone(), |_| Ok(())).unwrap();
    let mut other_sender = request;
    other_sender.sender = SenderIdentity::User(Uuid::new_v4());
    let second = flow.accept_input(other_sender, |_| Ok(())).unwrap();
    assert_ne!(first.message_id, second.message_id);
    assert_eq!(flow.pending_inputs().count(), 2);
}

fn invocation_content(lease: &TurnLease<AtmanUuid>) -> InputContent {
    InputContent::InvocationInput {
        arguments: BTreeMap::from([("input".into(), serde_json::json!("accepted input"))]),
        invocation: BTreeMap::from([("effort".into(), serde_json::json!("high"))]),
        messages: vec![Message::user_text(
            lease.turn.clone(),
            "exact projected input",
        )],
    }
}

fn context_content(lease: &TurnLease<AtmanUuid>) -> InputContent {
    InputContent::Context {
        messages: vec![Message::user_text(lease.turn.clone(), "boundary input")],
    }
}

#[test]
fn input_consumption_and_exact_content_survive_restart_without_requeue_or_duplicate_acceptance() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let request = delivery(
        &descriptor.address,
        SenderIdentity::User(Uuid::new_v4()),
        "first",
    );
    let first = flow.accept_input(request.clone(), |_| Ok(())).unwrap();
    let mut later = request.clone();
    later.request_id = Uuid::new_v4();
    later.delivery = DeliveryMode::NextTurn;
    let second = flow.accept_input(later, |_| Ok(())).unwrap();
    let before = flow.cursor().sequence;
    let batch = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |envelope| {
            envelope.id == first.message_id
        })
        .unwrap()
        .unwrap();
    assert!(flow.state().can_start_turn());
    assert_eq!(flow.cursor().sequence, before);
    let lease = batch.lease().clone();
    let actual = invocation_content(&lease);
    let receipt = flow.commit_input(&batch, actual.clone()).unwrap();
    assert_eq!(receipt.sequence, before + 1);
    assert!(!flow.state().can_start_turn());
    assert!(flow.commit_input(&batch, actual.clone()).is_err());
    assert!(
        flow.reserve_context_input(&lease, |_| true)
            .unwrap()
            .is_none()
    );
    let third = flow
        .accept_input(
            delivery(&descriptor.address, request.sender.clone(), "third"),
            |_| Ok(()),
        )
        .unwrap();
    let fourth = flow
        .accept_input(
            delivery(&descriptor.address, request.sender.clone(), "fourth"),
            |_| Ok(()),
        )
        .unwrap();
    let batch = flow
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    assert_eq!(batch.message_ids(), &[third.message_id, fourth.message_id]);
    flow.commit_input(&batch, context_content(&lease)).unwrap();
    assert_eq!(
        flow.pending_inputs()
            .map(|envelope| envelope.id)
            .collect::<Vec<_>>(),
        [second.message_id]
    );
    drop(flow);
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(flow.committed_inputs().len(), 2);
    assert_eq!(
        flow.committed_inputs()[1].input.message_ids,
        [third.message_id, fourth.message_id]
    );
    assert_eq!(flow.committed_inputs()[0].input.content, actual);
    assert_eq!(flow.committed_inputs()[0].receipt, receipt);
    assert_eq!(
        flow.pending_inputs()
            .map(|envelope| envelope.id)
            .collect::<Vec<_>>(),
        [second.message_id]
    );
    let retried = flow
        .accept_input(request, |_| {
            panic!("accepted retry must not be reauthorized")
        })
        .unwrap();
    assert_eq!(retried.receipt, first.receipt);
    assert_eq!(
        retried.disposition,
        InboxDisposition::InputCommitted { by: receipt }
    );
    assert!(matches!(
        flow.state().state(),
        InstanceState::Open(InstanceActivity::Interrupted { .. })
    ));
    change(&mut flow, FlowTransition::CloseRequested);
    assert!(matches!(
        flow.inbox_entries()[0].acceptance.disposition,
        InboxDisposition::InputCommitted { .. }
    ));
    assert!(matches!(
        flow.inbox_entries()[1].acceptance.disposition,
        InboxDisposition::Cancelled { .. }
    ));
    assert_eq!(flow.committed_inputs().len(), 2);
}

#[test]
fn execution_changes_revoke_reservations_without_late_release_or_consumption() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    flow.accept_input(
        delivery(
            &descriptor.address,
            SenderIdentity::User(Uuid::new_v4()),
            "input",
        ),
        |_| Ok(()),
    )
    .unwrap();
    let lease = start(&mut flow);
    let old = flow
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    let key = AtmanUuid(Uuid::new_v4());
    change(&mut flow, FlowTransition::ExecutionPaused { key });
    assert!(flow.commit_input(&old, context_content(&lease)).is_err());
    assert!(flow.reserve_context_input(&lease, |_| true).is_err());
    change(&mut flow, FlowTransition::ExecutionResumed { key });
    let current = flow
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    assert!(!flow.release_input(&old));
    assert!(flow.commit_input(&old, context_content(&lease)).is_err());
    assert!(
        flow.reserve_context_input(&lease, |_| true)
            .unwrap()
            .is_none()
    );
    change(
        &mut flow,
        FlowTransition::PhaseChanged {
            lease: lease.clone(),
            phase: TurnPhase::Finishing,
        },
    );
    assert!(
        flow.commit_input(&current, context_content(&lease))
            .is_err()
    );
    assert!(flow.reserve_context_input(&lease, |_| true).is_err());
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease: lease.clone(),
            outcome: TurnOutcome::Completed,
        },
    );
    let next = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    assert_ne!(next.lease().epoch, lease.epoch);
    assert!(!flow.release_input(&current));
    flow.commit_input(&next, invocation_content(next.lease()))
        .unwrap();
    assert!(flow.pending_inputs().next().is_none());
}

#[test]
fn invalid_or_abandoned_input_does_not_start_a_turn_or_consume_messages() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    flow.accept_input(
        delivery(
            &descriptor.address,
            SenderIdentity::User(Uuid::new_v4()),
            "input",
        ),
        |_| Ok(()),
    )
    .unwrap();
    let cursor = flow.cursor().clone();
    let batch = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    assert!(
        flow.commit_input(&batch, context_content(batch.lease()))
            .is_err()
    );
    let mut wrong_turn = batch.lease().clone();
    wrong_turn.turn = TurnId::now();
    assert!(
        flow.commit_input(&batch, invocation_content(&wrong_turn))
            .is_err()
    );
    assert_eq!(flow.cursor(), &cursor);
    assert!(flow.state().can_start_turn());
    assert_eq!(flow.pending_inputs().count(), 1);
    drop(batch);
    let next = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let key = AtmanUuid(Uuid::new_v4());
    change(&mut flow, FlowTransition::ExecutionPaused { key });
    assert!(
        flow.commit_input(&next, invocation_content(next.lease()))
            .is_err()
    );
    assert!(
        flow.reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
            .is_err()
    );
    change(&mut flow, FlowTransition::ExecutionResumed { key });
    let next = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    assert!(flow.release_input(&next));
    assert!(
        flow.commit_input(&next, invocation_content(next.lease()))
            .is_err()
    );
    assert!(flow.state().can_start_turn());
}

#[test]
fn torn_input_batch_restores_pending_messages_without_a_ghost_turn() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let descriptor = descriptor(info.id);
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let accepted = flow
        .accept_input(
            delivery(
                &descriptor.address,
                SenderIdentity::User(Uuid::new_v4()),
                "input",
            ),
            |_| Ok(()),
        )
        .unwrap();
    let journal = flow.dir().join("events.jsonl");
    let prefix = std::fs::metadata(&journal).unwrap().len();
    let batch = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    flow.commit_input(&batch, invocation_content(batch.lease()))
        .unwrap();
    drop(flow);
    let length = std::fs::metadata(&journal).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&journal)
        .unwrap()
        .set_len(length - 1)
        .unwrap();
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert!(flow.recovered_tail.is_some());
    assert_eq!(std::fs::metadata(&journal).unwrap().len(), prefix);
    assert!(flow.state().can_start_turn());
    assert!(flow.committed_inputs().is_empty());
    assert_eq!(
        flow.pending_inputs().next().unwrap().id,
        accepted.message_id
    );
    assert!(
        flow.commit_input(&batch, invocation_content(batch.lease()))
            .is_err()
    );
    let next = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    flow.commit_input(&next, invocation_content(next.lease()))
        .unwrap();
    assert_eq!(flow.committed_inputs().len(), 1);
}

fn waiting_flow() -> (
    tempfile::TempDir,
    atman_runtime::flow_store::SessionStore,
    FlowStore,
    ReplyWaiter,
) {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut flow = session.create_flow(descriptor(info.id)).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let lease = start(&mut flow);
    let waiter = ReplyWaiter {
        correlation: Uuid::new_v4(),
        lease,
        sender: SenderIdentity::Flow(FlowRef {
            session_id: info.id.into(),
            instance_id: FlowInstanceId::now(),
        }),
    };
    flow.register_reply_waiter(waiter.clone()).unwrap();
    (dir, session, flow, waiter)
}

fn reply_request(flow: &FlowStore, waiter: &ReplyWaiter) -> DeliveryRequest {
    let mut request = delivery(&flow.descriptor().address, waiter.sender.clone(), "reply");
    request.correlation = Some(waiter.correlation);
    request
}

fn wait_phase(flow: &mut FlowStore, waiter: &ReplyWaiter) {
    change(
        flow,
        FlowTransition::PhaseChanged {
            lease: waiter.lease.clone(),
            phase: TurnPhase::Waiting(atman_rt::WaitReason::Reply),
        },
    );
}

fn reply_content(waiter: &ReplyWaiter) -> InputContent {
    InputContent::WaitingContinuation {
        correlation: waiter.correlation,
        value: serde_json::json!({ "result": 42 }),
    }
}

#[test]
fn replies_share_inbox_ownership_and_cannot_be_stolen_by_context_or_foreign_senders() {
    let (_dir, _session, mut flow, waiter) = waiting_flow();
    let request = reply_request(&flow, &waiter);
    let mut foreign = request.clone();
    foreign.sender = SenderIdentity::User(Uuid::new_v4());
    let unrelated = flow.accept_input(foreign, |_| Ok(())).unwrap();
    let mut deferred = request.clone();
    deferred.request_id = Uuid::new_v4();
    deferred.delivery = DeliveryMode::NextTurn;
    let deferred = flow.accept_input(deferred, |_| Ok(())).unwrap();
    let first = flow.accept_input(request.clone(), |_| Ok(())).unwrap();
    let extra = flow
        .accept_input(reply_request(&flow, &waiter), |_| Ok(()))
        .unwrap();
    let ordinary = flow
        .reserve_context_input(&waiter.lease, |_| true)
        .unwrap()
        .unwrap();
    assert_eq!(ordinary.message_ids(), &[unrelated.message_id]);
    assert!(
        flow.reserve_reply_input(&waiter.lease, waiter.correlation)
            .is_err()
    );
    wait_phase(&mut flow, &waiter);
    assert!(!flow.release_input(&ordinary));
    let batch = flow
        .reserve_reply_input(&waiter.lease, waiter.correlation)
        .unwrap()
        .unwrap();
    assert_eq!(batch.message_ids(), &[first.message_id]);
    assert!(
        flow.reserve_reply_input(&waiter.lease, waiter.correlation)
            .unwrap()
            .is_none()
    );
    let before = flow.cursor().clone();
    let wrong = InputContent::WaitingContinuation {
        correlation: Uuid::new_v4(),
        value: serde_json::json!(42),
    };
    assert!(flow.commit_input(&batch, wrong).is_err());
    assert_eq!(flow.cursor(), &before);
    let receipt = flow.commit_input(&batch, reply_content(&waiter)).unwrap();
    assert_eq!(
        flow.reply_waiters()[0].status,
        ReplyStatus::Delivered {
            message_id: first.message_id,
            by: receipt.clone()
        }
    );
    assert_eq!(flow.committed_inputs()[0].input.lease, waiter.lease);
    assert_eq!(
        flow.committed_inputs()[0].input.content,
        reply_content(&waiter)
    );
    assert!(
        flow.reserve_reply_input(&waiter.lease, waiter.correlation)
            .is_err()
    );
    assert_eq!(
        flow.accept_input(request, |_| panic!("retry"))
            .unwrap()
            .disposition,
        InboxDisposition::InputCommitted { by: receipt }
    );
    assert_eq!(
        flow.pending_inputs()
            .map(|input| input.id)
            .collect::<Vec<_>>(),
        [unrelated.message_id, deferred.message_id, extra.message_id]
    );
    change(
        &mut flow,
        FlowTransition::PhaseChanged {
            lease: waiter.lease.clone(),
            phase: TurnPhase::Running,
        },
    );
    let ordinary = flow
        .reserve_context_input(&waiter.lease, |_| true)
        .unwrap()
        .unwrap();
    assert_eq!(
        ordinary.message_ids(),
        &[unrelated.message_id, extra.message_id]
    );
}

#[test]
fn paused_and_cancelled_waiters_retain_replies_and_reject_late_commits() {
    let (_dir, _session, mut flow, waiter) = waiting_flow();
    let accepted = flow
        .accept_input(reply_request(&flow, &waiter), |_| Ok(()))
        .unwrap();
    wait_phase(&mut flow, &waiter);
    let old = flow
        .reserve_reply_input(&waiter.lease, waiter.correlation)
        .unwrap()
        .unwrap();
    let key = AtmanUuid(Uuid::new_v4());
    change(&mut flow, FlowTransition::ExecutionPaused { key });
    assert!(flow.commit_input(&old, reply_content(&waiter)).is_err());
    assert!(
        flow.reserve_reply_input(&waiter.lease, waiter.correlation)
            .is_err()
    );
    assert_eq!(flow.reply_waiters()[0].status, ReplyStatus::Pending);
    change(&mut flow, FlowTransition::ExecutionResumed { key });
    let current = flow
        .reserve_reply_input(&waiter.lease, waiter.correlation)
        .unwrap()
        .unwrap();
    let cancelled = flow
        .cancel_reply_waiter(&waiter.lease, waiter.correlation)
        .unwrap();
    assert!(matches!(cancelled.status, ReplyStatus::Cancelled { .. }));
    assert!(flow.commit_input(&current, reply_content(&waiter)).is_err());
    assert_eq!(
        flow.register_reply_waiter(waiter.clone()).unwrap(),
        cancelled
    );
    change(
        &mut flow,
        FlowTransition::PhaseChanged {
            lease: waiter.lease.clone(),
            phase: TurnPhase::Running,
        },
    );
    let ordinary = flow
        .reserve_context_input(&waiter.lease, |_| true)
        .unwrap()
        .unwrap();
    assert_eq!(ordinary.message_ids(), &[accepted.message_id]);
    assert!(!flow.release_input(&old));
    assert!(!flow.release_input(&current));
    flow.commit_input(&ordinary, context_content(&waiter.lease))
        .unwrap();
    let mut reused = waiter.clone();
    reused.sender = SenderIdentity::User(Uuid::new_v4());
    assert!(flow.register_reply_waiter(reused).is_err());
}

#[test]
fn lost_waiting_turn_releases_uncommitted_replies_but_retains_delivered_input() {
    for committed in [false, true] {
        let (_dir, session, mut flow, waiter) = waiting_flow();
        wait_phase(&mut flow, &waiter);
        let accepted = flow
            .accept_input(reply_request(&flow, &waiter), |_| Ok(()))
            .unwrap();
        let batch = flow
            .reserve_reply_input(&waiter.lease, waiter.correlation)
            .unwrap()
            .unwrap();
        if committed {
            flow.commit_input(&batch, reply_content(&waiter)).unwrap();
        }
        let id = flow.descriptor().address.instance_id.clone();
        drop(flow);
        let mut flow = session.open_flow(&id).unwrap();
        assert!(matches!(
            flow.state().state(),
            InstanceState::Open(InstanceActivity::Interrupted {
                reason: Interruption::HostLost,
                ..
            })
        ));
        assert!(flow.commit_input(&batch, reply_content(&waiter)).is_err());
        if committed {
            assert!(matches!(
                flow.reply_waiters()[0].status,
                ReplyStatus::Delivered { .. }
            ));
            assert!(flow.pending_inputs().next().is_none());
            assert_eq!(
                flow.committed_inputs()[0].input.content,
                reply_content(&waiter)
            );
        } else {
            assert!(matches!(
                flow.reply_waiters()[0].status,
                ReplyStatus::Cancelled { .. }
            ));
            assert_eq!(
                flow.pending_inputs().next().unwrap().id,
                accepted.message_id
            );
            change(&mut flow, FlowTransition::InterruptionRecovered);
            let next = flow
                .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
                .unwrap()
                .unwrap();
            assert_eq!(next.message_ids(), &[accepted.message_id]);
            flow.commit_input(&next, invocation_content(next.lease()))
                .unwrap();
        }
    }
}

#[test]
fn terminal_turn_transitions_cancel_waits_and_late_registration_cannot_capture_old_replies() {
    for close in [false, true] {
        let (_dir, _session, mut flow, waiter) = waiting_flow();
        wait_phase(&mut flow, &waiter);
        flow.accept_input(reply_request(&flow, &waiter), |_| Ok(()))
            .unwrap();
        let batch = flow
            .reserve_reply_input(&waiter.lease, waiter.correlation)
            .unwrap()
            .unwrap();
        if close {
            change(&mut flow, FlowTransition::CloseRequested);
            assert_eq!(flow.pending_inputs().count(), 0);
        } else {
            change(
                &mut flow,
                FlowTransition::PhaseChanged {
                    lease: waiter.lease.clone(),
                    phase: TurnPhase::Finishing,
                },
            );
            assert_eq!(flow.pending_inputs().count(), 1);
        }
        assert!(matches!(
            flow.reply_waiters()[0].status,
            ReplyStatus::Cancelled { .. }
        ));
        assert!(flow.commit_input(&batch, reply_content(&waiter)).is_err());
        assert!(
            flow.reserve_reply_input(&waiter.lease, waiter.correlation)
                .is_err()
        );
    }
    let (_dir, _session, mut flow, waiter) = waiting_flow();
    let mut late = waiter.clone();
    late.correlation = Uuid::new_v4();
    flow.accept_input(reply_request(&flow, &late), |_| Ok(()))
        .unwrap();
    let before = flow.cursor().clone();
    assert!(flow.register_reply_waiter(late).is_err());
    assert_eq!(flow.cursor(), &before);
}

#[test]
fn independent_waiters_use_one_fifo_without_releasing_each_others_reply() {
    let (_dir, _session, mut flow, first) = waiting_flow();
    let mut second = first.clone();
    second.correlation = Uuid::new_v4();
    let registered = flow.register_reply_waiter(second.clone()).unwrap();
    assert_eq!(
        flow.register_reply_waiter(second.clone()).unwrap(),
        registered
    );
    wait_phase(&mut flow, &first);
    let second_message = flow
        .accept_input(reply_request(&flow, &second), |_| Ok(()))
        .unwrap();
    let first_message = flow
        .accept_input(reply_request(&flow, &first), |_| Ok(()))
        .unwrap();
    let batch = flow
        .reserve_reply_input(&first.lease, first.correlation)
        .unwrap()
        .unwrap();
    assert!(
        flow.reserve_reply_input(&second.lease, second.correlation)
            .unwrap()
            .is_none()
    );
    drop(batch);
    let batch = flow
        .reserve_reply_input(&second.lease, second.correlation)
        .unwrap()
        .unwrap();
    assert_eq!(batch.message_ids(), &[second_message.message_id]);
    flow.commit_input(&batch, reply_content(&second)).unwrap();
    assert_eq!(flow.reply_waiters()[0].status, ReplyStatus::Pending);
    let batch = flow
        .reserve_reply_input(&first.lease, first.correlation)
        .unwrap()
        .unwrap();
    assert_eq!(batch.message_ids(), &[first_message.message_id]);
    flow.commit_input(&batch, reply_content(&first)).unwrap();
    assert!(flow.pending_inputs().next().is_none());
    assert_eq!(flow.committed_inputs().len(), 2);
}
