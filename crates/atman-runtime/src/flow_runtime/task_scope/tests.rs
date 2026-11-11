use super::*;
use crate::flow_store::inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity};
use crate::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowInstanceId, RuntimeStore, SessionDescriptor,
};
use crate::source_program::RevisionBundle;
use std::{sync::Arc, time::Duration};

fn scoped_input(target: &FlowWorker, scope: Option<TaskScopeRef>) -> DeliveryRequest {
    DeliveryRequest {
        expected_head: None,
        request_id: Uuid::new_v4(),
        sender: SenderIdentity::User(Uuid::new_v4()),
        recipient: target.address().clone(),
        delivery: DeliveryMode::AtBoundary,
        cause: None,
        correlation: None,
        scope,
        payload: InboxPayload::Interjection {
            text: "work".into(),
        },
    }
}

async fn rehost(root: &RuntimeStore, host: &RuntimeHost, worker: FlowWorker) -> FlowWorker {
    let address = worker.address().clone();
    drop(worker.shutdown().await.unwrap());
    host.attach_session(root.open_session(address.session_id.0).unwrap())
        .unwrap()
        .open_flow(address.instance_id)
        .await
        .unwrap()
}

async fn pending_count(worker: &FlowWorker, count: usize) {
    let mut changes = worker.subscribe_commits();
    let result = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            changes.borrow_and_update();
            let snapshot = worker.snapshot().await.unwrap();
            if snapshot.pending.len() == count && snapshot.state.can_start_turn() {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await;
    assert!(
        result.is_ok(),
        "expected {count} pending: {:?}; scheduler: {:?}; completion: {:?}",
        worker.snapshot().await.unwrap(),
        *worker.scheduling_progress.borrow(),
        worker.completion(None).await.unwrap()
    );
}

async fn begin_turn(worker: &FlowWorker) -> atman_rt::TurnLease<crate::event::AtmanUuid> {
    worker
        .request(|store| {
            let lease = store
                .state()
                .clone()
                .begin_turn(atman_rt::TurnId::now(), atman_rt::RunId::now())
                .map_err(invalid)?;
            store.transition(
                store.state().revision(),
                FlowTransition::TurnStarted {
                    turn: lease.turn.clone(),
                    run: lease.run.clone(),
                },
            )?;
            Ok(lease)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn scoped_send_cancellation_ends_only_bound_waiters_and_preserves_delivery_reconciliation() {
    use crate::flow_runtime::reply::ReplyWait;
    use crate::flow_store::{
        outbox::SendStatus,
        waiter::{ReplyStatus, ReplyWaiter},
    };
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let host = RuntimeHost::default();
    let (source, scope) = owner(&root, FlowInstanceId::now()).await;
    let (sender, _) = owner(&root, FlowInstanceId::now()).await;
    let sender = rehost(&root, &host, sender).await;
    let (recipient, _) = owner(&root, FlowInstanceId::now()).await;
    let recipient = rehost(&root, &host, recipient).await;
    let lease = begin_turn(&sender).await;
    let mut sends = Vec::new();
    for scoped in [true, true, false] {
        let waiter = ReplyWaiter {
            correlation: Uuid::new_v4(),
            lease: lease.clone(),
            sender: SenderIdentity::Flow(recipient.address().clone()),
        };
        let mut request = scoped_input(&recipient, scoped.then(|| scope.clone()));
        request.sender = SenderIdentity::Flow(sender.address().clone());
        request.correlation = Some(waiter.correlation);
        sends.push((request, waiter));
    }
    // Source lookup failure must leave neither a send nor a registered wait.
    assert!(
        sender
            .queue_send(
                lease.clone(),
                sends[0].0.clone(),
                Some(sends[0].1.clone()),
                |_| Ok(())
            )
            .await
            .is_err()
    );
    sender
        .request(|store| {
            assert!(store.sends().is_empty());
            assert!(store.reply_waiters().is_empty());
            Ok(())
        })
        .await
        .unwrap();
    let source = rehost(&root, &host, source).await;
    for (request, waiter) in &sends {
        sender
            .queue_send(lease.clone(), request.clone(), Some(waiter.clone()), |_| {
                Ok(())
            })
            .await
            .unwrap();
    }
    // The first receiver ACK is lost; the second request has not reached the receiver.
    assert!(sender.pending_input_scopes().await.unwrap().is_empty());
    assert_eq!(
        sender.pending_message_scopes().await.unwrap(),
        vec![scope.clone()]
    );
    let accepted = host.deliver(sends[0].0.clone(), |_| Ok(())).await.unwrap();
    let pause = Uuid::new_v4().into();
    sender
        .transition(
            sender.state().await.unwrap().revision(),
            FlowTransition::ExecutionPaused { key: pause },
        )
        .await
        .unwrap();
    let waiting = tokio::spawn(
        ReplyWait::new(sender.clone(), sends[0].1.clone()).completion(atman_rt::TurnId::now()),
    );
    source
        .cancel_task_scope(scope.id, |_| Ok(()))
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    assert!(result.unwrap_err().to_string().contains("cancelled"));
    sender
        .request(|store| {
            let waits = store.reply_waiters();
            assert!(matches!(waits[0].status, ReplyStatus::Cancelled { .. }));
            assert_eq!(waits[0].status, waits[1].status);
            assert_eq!(waits[2].status, ReplyStatus::Pending);
            assert_eq!(store.pending_sends().count(), 3);
            assert!(!store.state().can_start_turn());
            assert!(store.committed_inputs().is_empty());
            Ok(())
        })
        .await
        .unwrap();
    let confirmed = host
        .retry_send(sender.address().clone(), sends[0].0.request_id, |_| {
            panic!("accepted retry")
        })
        .await
        .unwrap();
    assert!(confirmed.pending_reason.is_none());
    assert!(
        matches!(confirmed.record.status, SendStatus::Accepted { delivery, .. } if delivery.message_id == accepted.message_id)
    );
    let never_delivered = host
        .retry_send(sender.address().clone(), sends[1].0.request_id, |_| Ok(()))
        .await
        .unwrap();
    assert!(never_delivered.pending_reason.is_some());
    assert_eq!(never_delivered.record.status, SendStatus::PendingDelivery);
    sender
        .queue_send(
            lease.clone(),
            sends[0].0.clone(),
            Some(sends[0].1.clone()),
            |_| panic!("recorded retry"),
        )
        .await
        .unwrap();
    let mut new = sends[1].0.clone();
    new.request_id = Uuid::new_v4();
    new.correlation = None;
    assert!(
        sender
            .queue_send(lease, new, None, |_| Ok(()))
            .await
            .is_err()
    );
    let address = sender.address().clone();
    let lease = sends[0].1.lease.clone();
    sender
        .request(move |store| {
            store.finish_turn(
                lease,
                crate::flow_store::completion::InvocationResult::Returned(
                    crate::flow_store::completion::StoredValue::Unit,
                ),
            )
        })
        .await
        .unwrap();
    host.shutdown().await.unwrap();
    let session = root.open_session(address.session_id.0).unwrap();
    let restored = session.open_flow(&address.instance_id).unwrap();
    assert!(matches!(
        restored.reply_waiters()[0].status,
        ReplyStatus::Cancelled { .. }
    ));
    assert_eq!(
        restored.reply_waiters()[0].status,
        restored.reply_waiters()[1].status
    );
    assert!(matches!(
        restored.sends()[0].status,
        SendStatus::Accepted { .. }
    ));
    assert_eq!(restored.sends()[1].status, SendStatus::PendingDelivery);
}

#[tokio::test]
async fn reply_pickup_requires_the_original_send_scope_and_preserves_delivered_results() {
    use crate::flow_runtime::reply::ReplyWait;
    use crate::flow_store::{
        completion::{InvocationResult, StoredValue},
        inbox::FlowEventRef,
        waiter::{ReplyStatus, ReplyWaiter},
    };
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let host = RuntimeHost::default();
    let (source, scope) = owner(&root, FlowInstanceId::now()).await;
    let (sender, _) = owner(&root, FlowInstanceId::now()).await;
    let sender = rehost(&root, &host, sender).await;
    let (recipient, _) = owner(&root, FlowInstanceId::now()).await;
    let recipient = rehost(&root, &host, recipient).await;
    let lease = begin_turn(&sender).await;
    let waiter = ReplyWaiter {
        correlation: Uuid::new_v4(),
        lease: lease.clone(),
        sender: SenderIdentity::Flow(recipient.address().clone()),
    };
    let mut request = scoped_input(&recipient, Some(scope.clone()));
    request.sender = SenderIdentity::Flow(sender.address().clone());
    request.correlation = Some(waiter.correlation);
    let registered = waiter.clone();
    let queued_lease = lease.clone();
    // Model recorded intent whose scope owner is absent from the current host.
    sender
        .request(move |store| {
            store.queue_send(&queued_lease, request, Some(registered), |_| Ok(()))
        })
        .await
        .unwrap();
    let source_lease = begin_turn(&recipient).await;
    let source_turn = source_lease.turn.clone();
    let completion = recipient
        .request(move |store| {
            store.finish_turn(
                source_lease,
                InvocationResult::Returned(StoredValue::String("done".into())),
            )
        })
        .await
        .unwrap();
    let event = FlowEventRef {
        flow: recipient.address().clone(),
        sequence: completion.receipt.sequence(),
        index: 0,
    };
    let mut reply = scoped_input(&sender, None);
    reply.sender = waiter.sender.clone();
    reply.correlation = Some(waiter.correlation);
    reply.cause = Some(event.clone());
    reply.payload = InboxPayload::FlowEvent {
        source: event,
        event: "turn_completed".into(),
        content: serde_json::json!({
            "subscriptions": [], "kind": "turn_completed", "completion": completion
        }),
    };
    sender.accept_input(reply, |_| Ok(())).await.unwrap();
    let mut waiting =
        Box::pin(ReplyWait::new(sender.clone(), waiter.clone()).completion(source_turn.clone()));
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut waiting)
            .await
            .is_err()
    );
    assert_eq!(sender.snapshot().await.unwrap().pending.len(), 1);
    let source = rehost(&root, &host, source).await;
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), waiting)
            .await
            .unwrap()
            .unwrap(),
        crate::Value::Str(value) if value == "done"
    ));
    source
        .cancel_task_scope(scope.id, |_| Ok(()))
        .await
        .unwrap();
    assert!(matches!(
        ReplyWait::new(sender.clone(), waiter)
            .completion(source_turn)
            .await
            .unwrap(),
        crate::Value::Str(value) if value == "done"
    ));
    sender
        .request(|store| {
            assert!(matches!(
                store.reply_waiters()[0].status,
                ReplyStatus::Delivered { .. }
            ));
            assert_eq!(store.committed_inputs().len(), 1);
            Ok(())
        })
        .await
        .unwrap();
    sender
        .request(move |store| {
            store.finish_turn(lease, InvocationResult::Returned(StoredValue::Unit))
        })
        .await
        .unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn scoped_scheduler_wakes_on_source_recovery_and_preserves_ordinary_work() {
    use crate::flow_runtime::execution::TurnRequest;
    use atman_rt::{RunId, TurnId};
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let (source, scope) = owner(&root, FlowInstanceId::now()).await;
    let (target, _) = owner(&root, FlowInstanceId::now()).await;
    for selected in [Some(scope.clone()), None, Some(scope.clone())] {
        let input = scoped_input(&target, selected);
        target
            .request(move |store| store.accept_input(input, |_| Ok(())))
            .await
            .unwrap();
    }
    let host = RuntimeHost::default();
    let target = rehost(&root, &host, target).await;
    assert_eq!(
        target.pending_input_scopes().await.unwrap(),
        vec![scope.clone()]
    );
    let batches = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = batches.clone();
    let recipe = crate::flow_runtime::entry::EntryRecipe {
        first_turn: TurnId::now(),
        first_run: RunId::now(),
        arguments: Default::default(),
        invocation: Default::default(),
        parameter: Some("input".into()),
        authority: crate::flow_authority::EffectiveAuthority::root(
            &Default::default(),
            false,
            None,
        ),
        yield_interval: std::num::NonZeroUsize::new(16).unwrap(),
    };
    let scheduler = target
        .schedule(crate::Executor::new(), move |snapshot| {
            captured.lock().unwrap().push(snapshot.pending.len());
            recipe.prepare(snapshot)
        })
        .unwrap();
    pending_count(&target, 2).await;
    assert_eq!(*batches.lock().unwrap(), vec![1]);
    let mut progress = target.scheduling_progress.subscribe();
    let idle = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let checkpoint = progress.borrow_and_update().clone();
            if let crate::flow_runtime::scheduler::SchedulingProgress::Idle(_, sources) = checkpoint
            {
                break sources;
            }
            progress.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(!idle.has_changed());
    // No new recipient input or control is sent when the source becomes available.
    let source = rehost(&root, &host, source).await;
    // A recipient cursor alone cannot acknowledge a changed scope dependency.
    assert!(idle.has_changed());
    pending_count(&target, 0).await;
    assert_eq!(*batches.lock().unwrap(), vec![1, 2]);
    tokio::time::timeout(Duration::from_secs(5), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        *scheduler.subscribe_status().borrow(),
        crate::flow_runtime::scheduler::SchedulingStatus::Running
    ));
    scheduler.stop().await.unwrap();

    // A request prepared while active must not execute after cancellation.
    target
        .accept_input(scoped_input(&target, Some(scope.clone())), |_| Ok(()))
        .await
        .unwrap();
    let view = target.snapshot().await.unwrap();
    let request = TurnRequest {
        turn: TurnId::now(),
        run: RunId::now(),
        messages: view.pending.iter().map(|input| input.id).collect(),
        arguments: Default::default(),
        invocation: Default::default(),
        context: vec![],
        authority: crate::flow_authority::EffectiveAuthority::root(
            &Default::default(),
            false,
            None,
        ),
        yield_interval: std::num::NonZeroUsize::new(16).unwrap(),
    };
    source
        .cancel_task_scope(scope.id, |_| Ok(()))
        .await
        .unwrap();
    assert!(
        target
            .admit_turn(crate::Executor::new(), request, Some(view.cursor))
            .await
            .unwrap()
            .is_none()
    );
    pending_count(&target, 0).await;
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn scheduling_checks_execution_and_scope_gates_before_reading_pending_bodies() {
    use crate::flow_runtime::scheduler::{SchedulingProgress, SchedulingStatus};
    for paused in [false, true] {
        let data = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(data.path()).unwrap();
        let host = RuntimeHost::default();
        let (source, scope) = owner(&root, FlowInstanceId::now()).await;
        let (target, _) = owner(&root, FlowInstanceId::now()).await;
        let pause = Uuid::now_v7().into();
        if paused {
            target
                .transition(
                    target.state().await.unwrap().revision(),
                    FlowTransition::ExecutionPaused { key: pause },
                )
                .await
                .unwrap();
        }
        let mut input = scoped_input(&target, (!paused).then_some(scope));
        input.payload = InboxPayload::Interjection {
            text: "deferred-body".into(),
        };
        // This accepted input predates attachment of its source to this host.
        target
            .request(move |store| store.accept_input(input, |_| Ok(())))
            .await
            .unwrap();
        let target = rehost(&root, &host, target).await;
        let path = target
            .request(|store| Ok(store.dir()?.join("events.jsonl")))
            .await
            .unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let mut unavailable = bytes.clone();
        let position = unavailable
            .windows(b"deferred-body".len())
            .position(|part| part == b"deferred-body")
            .unwrap();
        unavailable[position] = b'X';
        std::fs::write(&path, unavailable).unwrap();
        let (prepared, mut received) = tokio::sync::mpsc::unbounded_channel();
        let scheduler = target
            .schedule(crate::Executor::new(), move |snapshot| {
                prepared.send(snapshot.pending.clone()).unwrap();
                Ok(None)
            })
            .unwrap();
        let mut progress = target.scheduling_progress.subscribe();
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match progress.borrow_and_update().clone() {
                    SchedulingProgress::Idle(..) => break,
                    SchedulingProgress::Failed(error) => {
                        panic!("ineligible body was read: {error}")
                    }
                    _ => {}
                }
                progress.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        assert!(received.try_recv().is_err());
        assert_eq!(
            *scheduler.subscribe_status().borrow(),
            SchedulingStatus::Running
        );
        assert_eq!(std::fs::read(&path).unwrap()[position], b'X');

        // Restoring the disk body alone is not a notification; the original gate change wakes preparation.
        std::fs::write(&path, bytes).unwrap();
        if paused {
            target
                .transition(
                    target.state().await.unwrap().revision(),
                    FlowTransition::ExecutionResumed { key: pause },
                )
                .await
                .unwrap();
        } else {
            rehost(&root, &host, source).await;
        }
        let pending = tokio::time::timeout(Duration::from_secs(5), received.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(pending.len(), 1);
        assert!(
            matches!(&pending[0].request.payload, InboxPayload::Interjection { text } if text == "deferred-body")
        );
        assert_eq!(
            target.snapshot().await.unwrap().pending,
            pending,
            "preparation must not consume input"
        );
        scheduler.stop().await.unwrap();
        host.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn scoped_context_pickup_batches_input_and_respects_pause_finish_and_cancellation() {
    use atman_rt::{RunId, TurnId, TurnPhase};
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let host = RuntimeHost::default();
    let (source, scope) = owner(&root, FlowInstanceId::now()).await;
    let source = rehost(&root, &host, source).await;
    let (target, _) = owner(&root, FlowInstanceId::now()).await;
    let target = rehost(&root, &host, target).await;
    let lease = target
        .request(|store| {
            let lease = store
                .state()
                .clone()
                .begin_turn(TurnId::now(), RunId::now())
                .map_err(invalid)?;
            store.transition(
                store.state().revision(),
                FlowTransition::TurnStarted {
                    turn: lease.turn.clone(),
                    run: lease.run.clone(),
                },
            )?;
            Ok(lease)
        })
        .await
        .unwrap();
    let mut ctx = crate::tool::ToolCtx::new().with_session_runtime(target.context().clone());
    ctx.turn_id = Some(lease.turn.clone());
    for _ in 0..2 {
        target
            .accept_input(scoped_input(&target, Some(scope.clone())), |_| Ok(()))
            .await
            .unwrap();
    }
    let pause = Uuid::new_v4().into();
    target
        .transition(
            target.state().await.unwrap().revision(),
            FlowTransition::ExecutionPaused { key: pause },
        )
        .await
        .unwrap();
    let guard = target.context().acquire_compact_lock_owned().await;
    drop(
        target
            .receive_context(lease.clone(), ctx.clone(), false, guard)
            .await
            .unwrap(),
    );
    assert_eq!(target.snapshot().await.unwrap().pending.len(), 2);
    target
        .transition(
            target.state().await.unwrap().revision(),
            FlowTransition::ExecutionResumed { key: pause },
        )
        .await
        .unwrap();
    let guard = target.context().acquire_compact_lock_owned().await;
    drop(
        target
            .receive_context(lease.clone(), ctx.clone(), false, guard)
            .await
            .unwrap(),
    );
    assert_eq!(
        target
            .request(|store| Ok(store.committed_inputs()[0].message_ids.len()))
            .await
            .unwrap(),
        2
    );
    target
        .accept_input(scoped_input(&target, Some(scope.clone())), |_| Ok(()))
        .await
        .unwrap();
    source
        .cancel_task_scope(scope.id, |_| Ok(()))
        .await
        .unwrap();
    let guard = target.context().acquire_compact_lock_owned().await;
    drop(
        target
            .receive_context(lease.clone(), ctx.clone(), false, guard)
            .await
            .unwrap(),
    );
    assert!(target.snapshot().await.unwrap().pending.is_empty());
    assert_eq!(
        target
            .request(|store| Ok(store.committed_inputs().len()))
            .await
            .unwrap(),
        1
    );
    target
        .accept_input(scoped_input(&target, None), |_| Ok(()))
        .await
        .unwrap();
    target
        .transition(
            target.state().await.unwrap().revision(),
            FlowTransition::PhaseChanged {
                lease: lease.clone(),
                phase: TurnPhase::Finishing,
            },
        )
        .await
        .unwrap();
    let guard = target.context().acquire_compact_lock_owned().await;
    drop(
        target
            .receive_context(lease.clone(), ctx, false, guard)
            .await
            .unwrap(),
    );
    assert_eq!(target.snapshot().await.unwrap().pending.len(), 1);
    target
        .transition(
            target.state().await.unwrap().revision(),
            FlowTransition::TurnFinished {
                lease,
                outcome: atman_rt::TurnOutcome::Completed,
            },
        )
        .await
        .unwrap();
    host.shutdown().await.unwrap();
}

async fn owner(root: &RuntimeStore, instance: FlowInstanceId) -> (FlowWorker, TaskScopeRef) {
    let id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            configuration: Default::default(),
            id,
            project: None,
            title: "Scope transactions".into(),
        })
        .unwrap();
    let mut store = session
        .create_flow(FlowDescriptor {
            address: FlowRef {
                session_id: id.into(),
                instance_id: instance,
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
                        "flow main(input: string = \"\") { }".into(),
                    )]
                    .into(),
                    edges: Default::default(),
                },
            }
            .into(),
        })
        .unwrap();
    store
        .transition(store.state().revision(), FlowTransition::Activated)
        .unwrap();
    let scope = TaskScopeRef {
        owner: store.state().address().clone(),
        id: Uuid::new_v4(),
    };
    store
        .create_task_scope(TaskScopeDefinition {
            id: scope.id,
            name: "Group".into(),
            goal: None,
        })
        .unwrap();
    (FlowWorker::start(store).await.unwrap(), scope)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reciprocal_scope_transactions_share_one_order_and_deduplicate_self_sources() {
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    // Equal instance UUIDs in different sessions must still acquire different gates.
    let instance = FlowInstanceId::now();
    let (left, left_scope) = owner(&root, instance.clone()).await;
    let (right, right_scope) = owner(&root, instance).await;
    let dependencies = ScopeAdmission(vec![
        (right_scope.clone(), right.clone()),
        (left_scope.clone(), left.clone()),
        (right_scope.clone(), right.clone()),
    ]);
    let inspected = dependencies.clone();
    let inspected_left = left.clone();
    tokio::task::spawn_blocking(move || {
        let transaction = ScopeTransaction::acquire(
            &inspected,
            inspected_left.address(),
            &inspected_left.scope_state,
        )
        .unwrap();
        assert_eq!(transaction.0.len(), 2);
        let addresses: Vec<_> = transaction
            .0
            .iter()
            .map(|(address, _)| (address.session_id.0, address.instance_id.0.0))
            .collect();
        assert!(addresses[0] < addresses[1]);
        assert_eq!(
            transaction
                .0
                .iter()
                .filter(|(_, gate)| matches!(gate, ScopeGuard::Write(_)))
                .count(),
            1
        );
        inspected.check(&transaction.snapshot(&inspected)).unwrap();
    })
    .await
    .unwrap();

    let start = Arc::new(tokio::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for worker in [left.clone(), right.clone()] {
        let start = start.clone();
        let dependencies = dependencies.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..12 {
                // Synchronize before enqueueing, never while a writer owns its transaction.
                start.wait().await;
                let check = dependencies.clone();
                worker
                    .request_scoped(dependencies.clone(), move |store, snapshot| {
                        check.check(snapshot)?;
                        store.create_task_scope(TaskScopeDefinition {
                            id: Uuid::new_v4(),
                            name: "Concurrent group".into(),
                            goal: None,
                        })
                    })
                    .await
                    .unwrap();
            }
        }));
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        for task in tasks {
            task.await.unwrap();
        }
    })
    .await
    .unwrap();
    for worker in [left.clone(), right.clone()] {
        assert_eq!(worker.task_scopes(|_| Ok(())).await.unwrap().len(), 13);
    }

    // Dependencies prepared before cancellation must use the committed source state.
    left.cancel_task_scope(left_scope.id, |_| Ok(()))
        .await
        .unwrap();
    let before = right.snapshot().await.unwrap().cursor;
    let check = dependencies.clone();
    let error = right
        .request_scoped(dependencies, move |store, snapshot| {
            check.check(snapshot)?;
            store.create_task_scope(TaskScopeDefinition {
                id: Uuid::new_v4(),
                name: "Must not commit".into(),
                goal: None,
            })
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
    assert_eq!(right.snapshot().await.unwrap().cursor, before);
    left.shutdown().await.unwrap();
    right.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_scope_source_rejects_only_the_dependent_transaction() {
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let (source, scope) = owner(&root, FlowInstanceId::now()).await;
    let (target, _) = owner(&root, FlowInstanceId::now()).await;
    let state = source.scope_state.clone();
    tokio::task::spawn_blocking(move || {
        let _ = std::panic::catch_unwind(move || {
            let _guard = state.write().unwrap();
            panic!("failed scope source");
        });
    })
    .await
    .unwrap();
    let before = target.snapshot().await.unwrap().cursor;
    let dependencies = ScopeAdmission(vec![(scope, source.clone())]);
    let checked = dependencies.clone();
    assert!(
        target
            .request_scoped(dependencies, move |_, snapshot| -> io::Result<()> {
                checked.check(snapshot)
            })
            .await
            .is_err()
    );
    assert_eq!(target.snapshot().await.unwrap().cursor, before);
    target
        .create_task_scope(
            TaskScopeDefinition {
                id: Uuid::new_v4(),
                name: "Independent work".into(),
                goal: None,
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}
