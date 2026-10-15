use std::{collections::BTreeMap, io, num::NonZeroUsize, sync::Arc, time::Duration};

use atman_rt::{InstanceActivity, InstanceState, RunId, TurnId, TurnOutcome};
use atman_runtime::flow_runtime::{
    FlowSnapshot, FlowWorker, execution::TurnRequest, host::RuntimeHost,
};
use atman_runtime::flow_store::inbox::{
    DeliveryMode, DeliveryRequest, InboxDisposition, InboxPayload, SenderIdentity,
};
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowTransition, RuntimeStore,
    SessionDescriptor,
};
use atman_runtime::message::MessageOrigin;
use atman_runtime::source_program::RevisionBundle;
use atman_runtime::{Executor, flow_authority::EffectiveAuthority};
use uuid::Uuid;

fn session_descriptor() -> SessionDescriptor {
    SessionDescriptor {
        id: Uuid::new_v4(),
        title: "Workspace".into(),
        project_root: None,
    }
}

fn flow_descriptor(session: Uuid) -> FlowDescriptor {
    FlowDescriptor {
        address: FlowRef {
            session_id: session.into(),
            instance_id: FlowInstanceId::now(),
        },
        creation_request: Uuid::new_v4(),
        created_by: None,
        entry: EntrySnapshot {
            name: "main".into(),
            path: "main.at".into(),
            bundle: RevisionBundle {
                entry: "entry:main.at".into(),
                sources: [(
                    "entry:main.at".into(),
                    "flow main() -> string { return final.answer(message: \"delivered\") }".into(),
                )]
                .into(),
                edges: Default::default(),
            },
        },
    }
}

async fn change(worker: &FlowWorker, change: FlowTransition) {
    worker
        .transition(worker.state().await.unwrap().revision(), change)
        .await
        .unwrap();
}

async fn start(worker: &FlowWorker) -> atman_rt::TurnLease<atman_runtime::event::AtmanUuid> {
    let lease = worker
        .state()
        .await
        .unwrap()
        .begin_turn(TurnId::now(), RunId::now())
        .unwrap();
    change(
        worker,
        FlowTransition::TurnStarted {
            turn: lease.turn.clone(),
            run: lease.run.clone(),
        },
    )
    .await;
    lease
}

fn peer_request(sender: &FlowRef, recipient: &FlowRef) -> DeliveryRequest {
    DeliveryRequest {
        request_id: Uuid::new_v4(),
        sender: SenderIdentity::Flow(sender.clone()),
        recipient: recipient.clone(),
        delivery: DeliveryMode::AtBoundary,
        cause: None,
        correlation: None,
        scope: None,
        payload: InboxPayload::Interjection {
            text: "peer work".into(),
        },
    }
}

fn prepare(snapshot: &FlowSnapshot) -> io::Result<Option<TurnRequest>> {
    let turn = TurnId::now();
    Ok(Some(TurnRequest {
        turn: turn.clone(),
        run: RunId::now(),
        messages: snapshot.pending.iter().map(|message| message.id).collect(),
        arguments: BTreeMap::new(),
        invocation: BTreeMap::new(),
        context: snapshot
            .pending
            .iter()
            .map(|envelope| atman_runtime::flow_runtime::input::context_message(envelope, &turn))
            .collect::<io::Result<_>>()?,
        authority: EffectiveAuthority::root(&Default::default(), false, None),
        yield_interval: NonZeroUsize::new(16).unwrap(),
    }))
}

async fn settled(worker: &FlowWorker) {
    let mut commits = worker.subscribe_commits();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            commits.borrow_and_update();
            let snapshot = worker.snapshot().await.unwrap();
            if snapshot.pending.is_empty() && snapshot.state.can_start_turn() {
                break;
            }
            commits.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn directory_reuses_active_instances_and_default_binding_has_no_execution_privilege() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let first = flow_descriptor(info.id);
    let mut second = flow_descriptor(info.id);
    second.created_by = Some(first.address.clone());
    let (a, b) = tokio::join!(
        session.create_flow(first.clone()),
        session.create_flow(second.clone())
    );
    let a = a.unwrap();
    let b = b.unwrap();
    change(&a, FlowTransition::Activated).await;
    change(&b, FlowTransition::Activated).await;
    let lease = a
        .state()
        .await
        .unwrap()
        .begin_turn(TurnId::now(), RunId::now())
        .unwrap();
    change(
        &a,
        FlowTransition::TurnStarted {
            turn: lease.turn.clone(),
            run: lease.run.clone(),
        },
    )
    .await;
    let before = a.state().await.unwrap();
    let (opened, created) = tokio::join!(
        host.flow(&first.address),
        session.create_flow(first.clone())
    );
    assert!(Arc::ptr_eq(&opened.unwrap().context(), &a.context()));
    assert!(Arc::ptr_eq(&created.unwrap().context(), &a.context()));
    assert_eq!(a.state().await.unwrap(), before);
    let mut conflict = first.clone();
    conflict.entry.name = "other".into();
    assert!(session.create_flow(conflict).await.is_err());
    let mut request_reuse = second.clone();
    request_reuse.address.instance_id = FlowInstanceId::now();
    assert!(session.create_flow(request_reuse).await.is_err());
    session
        .select_default(second.address.instance_id.clone())
        .await
        .unwrap();
    assert_eq!(
        session.snapshot().await.unwrap().catalog.default_flow,
        Some(second.address.instance_id)
    );
    assert_eq!(a.state().await.unwrap(), before);
    assert!(b.state().await.unwrap().can_start_turn());
    assert_eq!(host.sessions(), [info.id]);
    assert!(host.session(Uuid::new_v4()).is_err());
    assert!(session.open_flow(FlowInstanceId::now()).await.is_err());
    // Once enqueued, creation belongs to the catalog owner even if the caller abandons its receipt.
    let abandoned = flow_descriptor(info.id);
    let mut creation = Box::pin(session.create_flow(abandoned.clone()));
    assert!(futures::poll!(&mut creation).is_pending());
    drop(creation);
    let catalog = session.snapshot().await.unwrap();
    assert!(
        catalog
            .catalog
            .flows
            .iter()
            .any(|flow| flow.id == abandoned.address.instance_id)
    );
    let recovered = session.create_flow(abandoned.clone()).await.unwrap();
    assert!(Arc::ptr_eq(
        &recovered.context(),
        &session
            .open_flow(abandoned.address.instance_id)
            .await
            .unwrap()
            .context()
    ));
    change(
        &a,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    )
    .await;
}

#[tokio::test]
async fn directory_reopens_only_after_writer_handoff_and_recovers_unknown_execution_as_interrupted()
{
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let description = flow_descriptor(info.id);
    let worker = session.create_flow(description.clone()).await.unwrap();
    change(&worker, FlowTransition::Activated).await;
    let lease = worker
        .state()
        .await
        .unwrap()
        .begin_turn(TurnId::now(), RunId::now())
        .unwrap();
    change(
        &worker,
        FlowTransition::TurnStarted {
            turn: lease.turn.clone(),
            run: lease.run.clone(),
        },
    )
    .await;
    let context = worker.context();
    let store = worker.shutdown().await.unwrap();
    assert!(
        session
            .open_flow(description.address.instance_id.clone())
            .await
            .is_err()
    );
    drop(store);
    let reopened = host.flow(&description.address).await.unwrap();
    assert!(!Arc::ptr_eq(&context, &reopened.context()));
    assert!(matches!(
        reopened.state().await.unwrap().state(),
        InstanceState::Open(InstanceActivity::Interrupted {
            reason: atman_rt::Interruption::HostLost,
            ..
        })
    ));
    let cursor = reopened.snapshot().await.unwrap().cursor;
    let again = session.create_flow(description).await.unwrap();
    assert!(Arc::ptr_eq(&again.context(), &reopened.context()));
    assert_eq!(again.snapshot().await.unwrap().cursor, cursor);
}

#[tokio::test]
async fn same_and_cross_session_delivery_share_admission_without_parent_lifecycle_coupling() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let first = session_descriptor();
    let second = session_descriptor();
    let a_session = host
        .attach_session(root.create_session(first.clone()).unwrap())
        .unwrap();
    let b_session = host
        .attach_session(root.create_session(second.clone()).unwrap())
        .unwrap();
    let sender = flow_descriptor(first.id);
    let mut local = flow_descriptor(first.id);
    local.created_by = Some(sender.address.clone());
    let mut remote = flow_descriptor(second.id);
    remote.created_by = Some(sender.address.clone());
    let a = a_session.create_flow(sender.clone()).await.unwrap();
    let b = a_session.create_flow(local.clone()).await.unwrap();
    let c = b_session.create_flow(remote.clone()).await.unwrap();
    for worker in [&a, &b, &c] {
        change(worker, FlowTransition::Activated).await;
    }
    let pause = Uuid::new_v4().into();
    change(&b, FlowTransition::ExecutionPaused { key: pause }).await;
    let b_scheduler = b.schedule(Executor::new(), prepare).unwrap();
    let c_scheduler = c.schedule(Executor::new(), prepare).unwrap();
    let mut sent = Vec::new();
    for recipient in [local.address.clone(), remote.address.clone()] {
        let request = DeliveryRequest {
            request_id: Uuid::new_v4(),
            sender: SenderIdentity::Flow(sender.address.clone()),
            recipient,
            delivery: DeliveryMode::NextTurn,
            cause: None,
            correlation: None,
            scope: None,
            payload: InboxPayload::Interjection {
                text: "peer work".into(),
            },
        };
        let accepted = host.deliver(request.clone(), |_| Ok(())).await.unwrap();
        sent.push((request, accepted));
    }
    change(&a, FlowTransition::CloseRequested).await;
    change(&a, FlowTransition::Closed).await;
    settled(&c).await;
    assert!(matches!(
        a.state().await.unwrap().state(),
        InstanceState::Closed
    ));
    assert_eq!(b.snapshot().await.unwrap().pending.len(), 1);
    assert!(matches!(
        b.state().await.unwrap().state(),
        InstanceState::Open(InstanceActivity::Idle)
    ));
    change(&b, FlowTransition::ExecutionResumed { key: pause }).await;
    settled(&b).await;
    for (request, original) in sent {
        let receipt = host
            .deliver(request, |_| {
                panic!("accepted retry must not be authorized as new work")
            })
            .await
            .unwrap();
        assert_eq!(receipt.receipt, original.receipt);
        assert!(matches!(
            receipt.disposition,
            InboxDisposition::InputCommitted { .. }
        ));
    }
    for worker in [&b, &c] {
        assert!(worker.context().messages().iter().any(|message| {
            message.origin == MessageOrigin::Interjection
                && message.text_concat().contains("peer work")
                && message
                    .text_concat()
                    .contains(&sender.address.instance_id.to_string())
        }));
        assert_eq!(
            worker
                .context()
                .messages()
                .iter()
                .filter(|m| m.origin == MessageOrigin::FinalAnswer)
                .count(),
            1
        );
    }
    let denied = DeliveryRequest {
        request_id: Uuid::new_v4(),
        sender: SenderIdentity::Flow(sender.address),
        recipient: remote.address,
        delivery: DeliveryMode::AtBoundary,
        cause: None,
        correlation: None,
        scope: None,
        payload: InboxPayload::Interjection {
            text: "denied".into(),
        },
    };
    assert_eq!(
        host.deliver(denied, |_| Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "no grant"
        )))
        .await
        .unwrap_err()
        .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(c.snapshot().await.unwrap().pending.is_empty());
    b_scheduler.stop().await.unwrap();
    c_scheduler.stop().await.unwrap();
}

#[tokio::test]
async fn host_sends_reconcile_concurrent_retries_and_receiver_acknowledgement_after_closure() {
    use atman_runtime::flow_store::outbox::SendStatus;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let a_info = flow_descriptor(info.id);
    let b_info = flow_descriptor(info.id);
    let a = session.create_flow(a_info.clone()).await.unwrap();
    let b = session.create_flow(b_info.clone()).await.unwrap();
    for worker in [&a, &b] {
        change(worker, FlowTransition::Activated).await;
    }
    let a_turn = start(&a).await;
    let b_turn = start(&b).await;
    let request = peer_request(&a_info.address, &b_info.address);
    let reverse = peer_request(&b_info.address, &a_info.address);
    let (one, retry, reverse) = tokio::join!(
        host.send(
            a_turn.clone(),
            request.clone(),
            None,
            |_| Ok(()),
            |_| Ok(())
        ),
        host.send(
            a_turn.clone(),
            request.clone(),
            None,
            |_| Ok(()),
            |_| Ok(())
        ),
        host.send(b_turn.clone(), reverse, None, |_| Ok(()), |_| Ok(())),
    );
    let one = one.unwrap();
    let retry = retry.unwrap();
    let reverse = reverse.unwrap();
    assert_eq!(one.record, retry.record);
    for attempt in [&one, &retry, &reverse] {
        assert!(attempt.pending_reason.is_none());
        assert!(matches!(attempt.record.status, SendStatus::Accepted { .. }));
    }
    assert_eq!(a.snapshot().await.unwrap().pending.len(), 1);
    assert_eq!(b.snapshot().await.unwrap().pending.len(), 1);
    let self_send = host
        .send(
            a_turn.clone(),
            peer_request(&a_info.address, &a_info.address),
            None,
            |_| Ok(()),
            |_| Ok(()),
        )
        .await
        .unwrap();
    assert!(self_send.pending_reason.is_none());

    // Simulate a lost receiver ACK: the original intent and receiver acceptance exist, but sender confirmation does not.
    let uncertain = peer_request(&a_info.address, &b_info.address);
    a.queue_send(a_turn.clone(), uncertain.clone(), None, |_| Ok(()))
        .await
        .unwrap();
    let accepted = host.deliver(uncertain.clone(), |_| Ok(())).await.unwrap();
    for (worker, lease) in [(&a, a_turn.clone()), (&b, b_turn)] {
        change(
            worker,
            FlowTransition::TurnFinished {
                lease,
                outcome: TurnOutcome::Completed,
            },
        )
        .await;
        change(worker, FlowTransition::CloseRequested).await;
        change(worker, FlowTransition::Closed).await;
    }
    let reconciled = host
        .retry_send(a_info.address.clone(), uncertain.request_id, |_| {
            panic!("accepted retry is not new delivery")
        })
        .await
        .unwrap();
    assert!(reconciled.pending_reason.is_none());
    let SendStatus::Accepted { delivery, .. } = &reconciled.record.status else {
        panic!("missing confirmation");
    };
    assert_eq!(delivery.message_id, accepted.message_id);
    assert_eq!(delivery.receipt, accepted.receipt);
    let repeated = host
        .send(
            a_turn,
            uncertain,
            None,
            |_| panic!("existing intent is not a new send"),
            |_| panic!("confirmed send needs no new delivery"),
        )
        .await
        .unwrap();
    assert_eq!(repeated.record, reconciled.record);
    assert!(a.pending_sends().await.unwrap().is_empty());
}

#[tokio::test]
async fn abandoned_host_send_preserves_intent_and_cross_session_retry_survives_sender_reopen() {
    use atman_runtime::flow_store::outbox::SendStatus;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let first = session_descriptor();
    let second = session_descriptor();
    let a_session = host
        .attach_session(root.create_session(first.clone()).unwrap())
        .unwrap();
    let b_session = host
        .attach_session(root.create_session(second.clone()).unwrap())
        .unwrap();
    let a_info = flow_descriptor(first.id);
    let b_info = flow_descriptor(second.id);
    let a = a_session.create_flow(a_info.clone()).await.unwrap();
    let b = b_session.create_flow(b_info.clone()).await.unwrap();
    for worker in [&a, &b] {
        change(worker, FlowTransition::Activated).await;
    }
    let lease = start(&a).await;
    let request = peer_request(&a_info.address, &b_info.address);
    let (denied, observed) = tokio::sync::oneshot::channel();
    let mut abandoned = Box::pin(host.send(
        lease.clone(),
        request.clone(),
        None,
        |_| Ok(()),
        move |_| {
            let _ = denied.send(());
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "delivery grant unavailable",
            ))
        },
    ));
    assert!(futures::poll!(&mut abandoned).is_pending());
    drop(abandoned);
    tokio::time::timeout(Duration::from_secs(10), observed)
        .await
        .unwrap()
        .unwrap();
    let queued = a.send_record(request.request_id).await.unwrap().unwrap();
    assert_eq!(queued.status, SendStatus::PendingDelivery);
    assert_eq!(a.pending_sends().await.unwrap().len(), 1);
    assert!(b.snapshot().await.unwrap().pending.is_empty());
    drop(a.shutdown().await.unwrap());
    let retried = host
        .retry_send(a_info.address.clone(), request.request_id, |_| Ok(()))
        .await
        .unwrap();
    assert!(retried.pending_reason.is_none());
    assert_eq!(retried.record.queued, queued.queued);
    assert!(matches!(retried.record.status, SendStatus::Accepted { .. }));
    let a = host.flow(&a_info.address).await.unwrap();
    assert!(matches!(
        a.state().await.unwrap().state(),
        InstanceState::Open(InstanceActivity::Interrupted {
            reason: atman_rt::Interruption::HostLost,
            ..
        })
    ));
    assert!(a.pending_sends().await.unwrap().is_empty());
    assert_eq!(b.snapshot().await.unwrap().pending.len(), 1);
    let retried_again = host
        .retry_send(a_info.address, request.request_id, |_| {
            panic!("already confirmed")
        })
        .await
        .unwrap();
    assert_eq!(retried_again.record, retried.record);
    let mut forged = request;
    forged.request_id = Uuid::new_v4();
    forged.sender = SenderIdentity::Flow(b_info.address);
    assert!(
        host.send(
            lease,
            forged,
            None,
            |_| panic!("forged identity must fail first"),
            |_| panic!("must not deliver")
        )
        .await
        .is_err()
    );
}
