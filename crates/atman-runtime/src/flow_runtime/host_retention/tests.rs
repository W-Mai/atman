use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::*;
use crate::flow_store::{
    EntrySnapshot, FlowTransition,
    inbox::{DeliveryMode, InboxPayload, SenderIdentity},
};

async fn fixture() -> (tempfile::TempDir, RuntimeHost, SessionHandle, FlowWorker) {
    let directory = tempfile::tempdir().unwrap();
    let host = RuntimeHost::default();
    let id = Uuid::now_v7();
    let session = host
        .create_session(
            directory.path(),
            SessionDescriptor {
                id,
                configuration: Default::default(),
                project: None,
                title: "Retention".into(),
            },
        )
        .await
        .unwrap();
    let descriptor = FlowDescriptor {
        address: FlowRef {
            session_id: id.into(),
            instance_id: FlowInstanceId::now(),
        },
        creation_request: Uuid::now_v7(),
        created_by: None,
        content: EntrySnapshot {
            name: "main".into(),
            path: "main.at".into(),
            bundle: crate::source_program::RevisionBundle {
                entry: "entry:main.at".into(),
                edges: Default::default(),
                sources: [(
                    "entry:main.at".into(),
                    "flow main() -> int { return 42 }".into(),
                )]
                .into(),
            },
        }
        .into(),
    };
    let worker = session
        .create_transient_flow(descriptor, None, None)
        .await
        .unwrap();
    worker
        .request(|store| store.transition(store.state().revision(), FlowTransition::Activated))
        .await
        .unwrap();
    (directory, host, session, worker)
}

async fn queue(worker: &FlowWorker) -> Acceptance {
    worker
        .accept_input(
            DeliveryRequest {
                request_id: Uuid::now_v7(),
                sender: SenderIdentity::User(Uuid::now_v7()),
                recipient: worker.address().clone(),
                delivery: DeliveryMode::NextTurn,
                cause: None,
                correlation: None,
                scope: None,
                payload: InboxPayload::Invocation {
                    arguments: Default::default(),
                    invocation: Default::default(),
                    user: Some(crate::flow_store::inbox::UserContent {
                        text: "continue".into(),
                        images: vec![],
                        presentation: None,
                    }),
                },
            },
            |_| Ok(()),
        )
        .await
        .unwrap()
}

async fn schedule(
    session: &SessionHandle,
    worker: &FlowWorker,
    ready: Arc<AtomicBool>,
) -> watch::Receiver<SchedulingStatus> {
    session
        .schedule_flow(
            worker.address().instance_id.clone(),
            crate::Executor::new(),
            move |snapshot| {
                Ok(ready
                    .load(Ordering::Acquire)
                    .then(|| snapshot.pending.first())
                    .flatten()
                    .map(|input| TurnRequest {
                        turn: atman_rt::TurnId::now(),
                        run: atman_rt::RunId::now(),
                        messages: vec![input.id],
                        arguments: Default::default(),
                        invocation: Default::default(),
                        context: vec![],
                        authority: crate::flow_authority::EffectiveAuthority::root(
                            &Default::default(),
                            false,
                            None,
                        ),
                        yield_interval: std::num::NonZeroUsize::new(16).unwrap(),
                    }))
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn keep_rebinds_canonical_owner_retains_old_view_until_ack_and_resumes_only_on_explicit_send()
{
    let (_directory, host, session, memory) = fixture().await;
    let ready = Arc::new(AtomicBool::new(false));
    let original_status = schedule(&session, &memory, ready.clone()).await;
    let before_keep = queue(&memory).await;
    let expected = memory.snapshot().await.unwrap().cursor;
    let request = Uuid::now_v7();
    let kept = host
        .keep_transient(memory.address().clone(), request, expected.clone(), |_| {
            Ok(())
        })
        .await
        .unwrap();
    assert!(!kept.worker.is_transient());
    assert_eq!(*original_status.borrow(), SchedulingStatus::Stopped);
    assert!(
        session
            .scheduling_status(memory.address().instance_id.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(kept.worker.snapshot().await.unwrap().pending.len(), 1);
    let old = kept
        .worker
        .request(move |store| {
            Ok(store
                .inbox_entry(before_keep.message_id)?
                .unwrap()
                .acceptance)
        })
        .await
        .unwrap();
    assert!(
        host.resume_retained_scheduling(kept.worker.address().clone(), request, old, |_| Ok(()))
            .await
            .is_err()
    );
    assert!(
        session
            .schedule_flow(
                memory.address().instance_id.clone(),
                crate::Executor::new(),
                |_| Ok(None)
            )
            .await
            .is_err()
    );
    assert!(
        session
            .loaded_flow(&memory.address().instance_id)
            .unwrap()
            .commands
            .same_channel(&kept.worker.commands)
    );
    assert_eq!(memory.snapshot().await.unwrap().cursor, expected);
    let retry = host
        .keep_transient(memory.address().clone(), request, expected, |_| Ok(()))
        .await
        .unwrap();
    assert!(retry.worker.commands.same_channel(&kept.worker.commands));
    host.acknowledge_retention(kept.receipt.clone(), |_| Ok(()))
        .await
        .unwrap();
    assert!(memory.is_stopped());
    host.acknowledge_retention(kept.receipt.clone(), |_| Ok(()))
        .await
        .unwrap();
    ready.store(true, Ordering::Release);
    let accepted = queue(&kept.worker).await;
    host.resume_retained_scheduling(
        kept.worker.address().clone(),
        request,
        accepted.clone(),
        |_| Ok(()),
    )
    .await
    .unwrap();
    host.resume_retained_scheduling(kept.worker.address().clone(), request, accepted, |_| Ok(()))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    assert!(kept.worker.snapshot().await.unwrap().pending.is_empty());
    let completion = kept.worker.completion(None).await.unwrap().unwrap();
    assert_eq!(
        completion.completion.result,
        crate::flow_store::completion::InvocationResult::Returned(
            crate::flow_store::completion::StoredValue::Int(42)
        )
    );
    assert!(completion.receipt.require_durable().is_ok());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn rejected_freeze_restores_the_same_scheduler_policy() {
    let (_directory, host, session, memory) = fixture().await;
    let ready = Arc::new(AtomicBool::new(false));
    schedule(&session, &memory, ready.clone()).await;
    queue(&memory).await;
    let expected = memory.snapshot().await.unwrap().cursor;
    let checks = AtomicUsize::new(0);
    let error = host
        .keep_transient(
            memory.address().clone(),
            Uuid::now_v7(),
            expected,
            move |_| {
                if checks.fetch_add(1, Ordering::AcqRel) == 3 {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "retention denied",
                    ))
                } else {
                    Ok(())
                }
            },
        )
        .await
        .err()
        .unwrap();
    assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
    assert!(
        memory
            .request(|store| Ok(!store.transient_frozen()))
            .await
            .unwrap()
    );
    assert_eq!(
        *session
            .scheduling_status(memory.address().instance_id.clone())
            .await
            .unwrap()
            .unwrap()
            .borrow(),
        SchedulingStatus::Running
    );
    ready.store(true, Ordering::Release);
    queue(&memory).await;
    tokio::time::timeout(std::time::Duration::from_secs(5), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    assert!(memory.snapshot().await.unwrap().pending.is_empty());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn uncertain_keep_stays_readable_frozen_and_retries_the_same_source() {
    let (directory, host, session, memory) = fixture().await;
    let expected = memory.snapshot().await.unwrap().cursor;
    let request = Uuid::now_v7();
    let listing = RuntimeStore::inspect_session(directory.path(), session.id()).unwrap();
    let plans = listing.dir.join("retentions");
    std::fs::write(&plans, "blocked").unwrap();
    let error = host
        .keep_transient(memory.address().clone(), request, expected.clone(), |_| {
            Ok(())
        })
        .await
        .err()
        .unwrap();
    let pending = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<RetentionPending>())
        .unwrap();
    assert_eq!(pending.request, request);
    assert_eq!(memory.snapshot().await.unwrap().cursor, expected);
    assert!(
        memory
            .request(|store| Ok(store.transient_frozen()))
            .await
            .unwrap()
    );
    assert!(
        host.discard_transient(memory.address().clone(), expected.clone(), |_| Ok(()))
            .await
            .is_err()
    );
    assert!(
        host.keep_transient(
            memory.address().clone(),
            Uuid::now_v7(),
            expected.clone(),
            |_| Ok(())
        )
        .await
        .is_err()
    );
    std::fs::remove_file(plans).unwrap();
    let kept = host
        .keep_transient(memory.address().clone(), request, expected, |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(session.snapshot().await.unwrap().catalog.flows.len(), 1);
    host.acknowledge_retention(kept.receipt, |_| Ok(()))
        .await
        .unwrap();
    assert!(memory.is_stopped());
    host.shutdown().await.unwrap();
}

async fn wait_frozen(worker: &FlowWorker) {
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !worker
            .request(|store| Ok(store.transient_frozen()))
            .await
            .unwrap()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn drained_late_trace_advances_source_before_publication_and_preserves_late_outputs() {
    let (directory, host, _session, memory) = fixture().await;
    let lease = memory
        .request(|store| {
            store.transition(
                store.state().revision(),
                FlowTransition::TurnStarted {
                    turn: atman_rt::TurnId::now(),
                    run: atman_rt::RunId::now(),
                },
            )?;
            let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) =
                store.state().state()
            else {
                unreachable!()
            };
            let lease = active.lease.clone();
            store.finish_turn(
                lease.clone(),
                crate::flow_store::completion::InvocationResult::Returned(
                    crate::flow_store::completion::StoredValue::Unit,
                ),
            )?;
            Ok(lease)
        })
        .await
        .unwrap();
    let sink = memory.execution_sink(lease, &crate::event::EventSink::new());
    let producer = memory.resources.executions.token();
    let expected = memory.snapshot().await.unwrap().cursor;
    let request = Uuid::now_v7();
    let target = memory.address().clone();
    let owner = host.clone();
    let saving = tokio::spawn(async move {
        owner
            .keep_transient(target, request, expected, |_| Ok(()))
            .await
    });
    wait_frozen(&memory).await;
    sink.emit(crate::event::Event::MermaidDiagram {
        source: "late diagnostic".into(),
    });
    let output = memory
        .context()
        .output_store()
        .register("late producer", "late asset")
        .unwrap();
    drop(producer);
    let error = saving.await.unwrap().err().unwrap();
    assert!(
        error
            .get_ref()
            .and_then(|error| error.downcast_ref::<RetentionPending>())
            .is_none()
    );
    assert!(
        !memory
            .request(|store| Ok(store.transient_frozen()))
            .await
            .unwrap()
    );
    let listing =
        RuntimeStore::inspect_session(directory.path(), memory.address().session_id.0).unwrap();
    assert!(!listing.dir.join("retentions").exists());
    let events = memory.execution_events().await.unwrap();
    assert!(events.iter().flat_map(|batch| &batch.events).any(|event| matches!(&event.event, crate::event::Event::MermaidDiagram { source } if source == "late diagnostic")));
    let expected = memory.snapshot().await.unwrap().cursor;
    let kept = host
        .keep_transient(memory.address().clone(), request, expected, |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(
        kept.worker
            .context()
            .output_store()
            .read_lines(&output, 0, 1, Default::default())
            .unwrap()
            .content,
        "late asset"
    );
    host.acknowledge_retention(kept.receipt, |_| Ok(()))
        .await
        .unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn accepted_keep_survives_waiter_cancellation_and_reconciles_to_the_canonical_worker() {
    let (_directory, host, session, memory) = fixture().await;
    let producer = memory.resources.executions.token();
    let expected = memory.snapshot().await.unwrap().cursor;
    let request = Uuid::now_v7();
    let owner = host.clone();
    let target = memory.address().clone();
    let boundary = expected.clone();
    let saving = tokio::spawn(async move {
        owner
            .keep_transient(target, request, boundary, |_| Ok(()))
            .await
    });
    wait_frozen(&memory).await;
    saving.abort();
    let _ = saving.await;
    drop(producer);
    let snapshot = tokio::time::timeout(std::time::Duration::from_secs(5), session.snapshot())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(snapshot.catalog.flows.len(), 1);
    let kept = host
        .keep_transient(memory.address().clone(), request, expected, |_| Ok(()))
        .await
        .unwrap();
    assert!(!kept.worker.commands.same_channel(&memory.commands));
    assert!(
        session
            .loaded_flow(&memory.address().instance_id)
            .unwrap()
            .commands
            .same_channel(&kept.worker.commands)
    );
    host.acknowledge_retention(kept.receipt, |_| Ok(()))
        .await
        .unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn retired_memory_releases_after_the_last_reader_leaves_without_a_ui_acknowledgement() {
    let (_directory, host, session, memory) = fixture().await;
    let mut observed = memory.subscribe_state();
    let expected = memory.snapshot().await.unwrap().cursor;
    let kept = host
        .keep_transient(memory.address().clone(), Uuid::now_v7(), expected, |_| {
            Ok(())
        })
        .await
        .unwrap();
    assert!(host.directory.transient_budget.retained_bytes() > 0);
    drop(memory);
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            tokio::select! {
                changed = observed.changed() => { if changed.is_err() { break; } }
                // Continuous catalog traffic cannot postpone the retirement clock.
                _ = session.snapshot() => { tokio::task::yield_now().await; }
            }
        }
    })
    .await
    .unwrap();
    session.snapshot().await.unwrap();
    assert_eq!(host.directory.transient_budget.retained_bytes(), 0);
    assert!(!kept.worker.is_stopped());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn retention_scheduling_gate_survives_restart_and_requires_a_verified_later_user_acceptance()
{
    let (directory, host, session, memory) = fixture().await;
    let queued = queue(&memory).await;
    let expected = memory.snapshot().await.unwrap().cursor;
    let request = Uuid::now_v7();
    let target = memory.address().clone();
    let kept = host
        .keep_transient(target.clone(), request, expected, |_| Ok(()))
        .await
        .unwrap();
    host.acknowledge_retention(kept.receipt, |_| Ok(()))
        .await
        .unwrap();
    host.shutdown().await.unwrap();
    drop(session);

    let restarted = RuntimeHost::default();
    let session = restarted
        .open_session(directory.path(), target.session_id.0)
        .await
        .unwrap();
    let worker = session.open_flow(target.instance_id.clone()).await.unwrap();
    assert_eq!(worker.snapshot().await.unwrap().pending.len(), 1);
    assert!(
        session.snapshot().await.unwrap().catalog.flows[0]
            .retention_resume
            .is_none()
    );
    assert!(
        session
            .schedule_flow(target.instance_id.clone(), crate::Executor::new(), |_| Ok(
                None
            ))
            .await
            .is_err()
    );
    let old = worker
        .request(move |store| Ok(store.inbox_entry(queued.message_id)?.unwrap().acceptance))
        .await
        .unwrap();
    assert!(
        restarted
            .resume_retained_scheduling(target.clone(), request, old.clone(), |_| Ok(()))
            .await
            .is_err()
    );

    let accepted = queue(&worker).await;
    let mut forged = accepted.clone();
    forged.message_id = old.message_id;
    assert!(
        restarted
            .resume_retained_scheduling(target.clone(), request, forged, |_| Ok(()))
            .await
            .is_err()
    );
    assert!(
        session.snapshot().await.unwrap().catalog.flows[0]
            .retention_resume
            .is_none()
    );
    restarted
        .resume_retained_scheduling(target.clone(), request, accepted.clone(), |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(
        session.snapshot().await.unwrap().catalog.flows[0]
            .retention_resume
            .as_ref()
            .unwrap()
            .receipt,
        accepted.receipt
    );
    session
        .schedule_flow(target.instance_id.clone(), crate::Executor::new(), |_| {
            Ok(None)
        })
        .await
        .unwrap();
    restarted.shutdown().await.unwrap();

    let restarted = RuntimeHost::default();
    let session = restarted
        .open_session(directory.path(), target.session_id.0)
        .await
        .unwrap();
    assert!(
        session.snapshot().await.unwrap().catalog.flows[0]
            .retention_resume
            .is_some()
    );
    session
        .schedule_flow(target.instance_id, crate::Executor::new(), |_| Ok(None))
        .await
        .unwrap();
    restarted.shutdown().await.unwrap();
}
