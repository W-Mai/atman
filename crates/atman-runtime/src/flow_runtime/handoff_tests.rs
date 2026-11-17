use super::*;
use crate::flow_runtime::FlowWorker;
use crate::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowRef, FlowTransition, RuntimeStore, SessionDescriptor,
    inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity, UserContent},
};
use std::time::Duration;
use tokio::sync::oneshot;

async fn fixture(host: &RuntimeHost, data: &std::path::Path) -> (SessionHandle, FlowWorker) {
    let id = Uuid::now_v7();
    let session = host
        .create_session(
            data,
            SessionDescriptor {
                id,
                title: "Handoff".into(),
                project: None,
                configuration: Default::default(),
            },
        )
        .await
        .unwrap();
    let worker = session
        .create_flow(FlowDescriptor {
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
                        "flow main(input: string) -> string { return input }".into(),
                    )]
                    .into(),
                },
            }
            .into(),
        })
        .await
        .unwrap();
    worker
        .transition(0, FlowTransition::Activated)
        .await
        .unwrap();
    (session, worker)
}

fn submission(worker: &FlowWorker) -> DeliveryRequest {
    DeliveryRequest {
        expected_head: None,
        request_id: Uuid::now_v7(),
        sender: SenderIdentity::User(Uuid::now_v7()),
        recipient: worker.address().clone(),
        delivery: DeliveryMode::NextTurn,
        cause: None,
        correlation: None,
        scope: None,
        payload: InboxPayload::UserSubmission {
            content: UserContent {
                text: "retained input".into(),
                images: vec![],
                presentation: None,
            },
            invocation: Default::default(),
        },
    }
}

async fn released(host: &RuntimeHost, session: Uuid, request: Uuid) -> SessionHandoffReceipt {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match host.handoff_session(session, request).await.unwrap() {
                SessionHandoffOutcome::Released { receipt } => return receipt,
                SessionHandoffOutcome::Pending { .. } => tokio::task::yield_now().await,
            }
        }
    })
    .await
    .unwrap()
}

async fn blocked_by(host: &RuntimeHost, session: Uuid, expected: HandoffBlocker) {
    let request = Uuid::now_v7();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match host.handoff_session(session, request).await.unwrap() {
                SessionHandoffOutcome::Pending { blockers, .. } => {
                    if blockers.contains(&expected) {
                        return;
                    }
                    // Accepted observation may still be publishing; its owner must drain before the precise blocker can be inspected.
                    assert_eq!(blockers, vec![HandoffBlocker::Admission]);
                }
                SessionHandoffOutcome::Released { .. } => {
                    panic!("handoff released an owner while its original blocker was retained")
                }
            }
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn admission_freeze_waits_without_rejecting_and_release_retires_original_handles() {
    let admission = Arc::new(SessionAdmission::new(Uuid::now_v7(), Uuid::now_v7()));
    let accepted = admission.enter().await.unwrap();
    assert!(admission.try_freeze().is_none());
    drop(accepted);
    let freeze = admission.try_freeze().unwrap();
    let mut waiting = Box::pin(admission.enter());
    assert!(futures::poll!(waiting.as_mut()).is_pending());
    drop(freeze);
    drop(waiting.await.unwrap());
    admission.try_freeze().unwrap().release();
    let error = admission.enter().await.err().unwrap();
    assert!(
        error
            .get_ref()
            .unwrap()
            .downcast_ref::<SessionOwnerReleased>()
            .is_some()
    );
}

#[tokio::test]
async fn deleting_a_closed_owner_waits_for_authority_before_opening_and_reuses_its_guard() {
    use crate::peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan, PeerAuthority};
    let data = tempfile::tempdir().unwrap();
    let host = RuntimeHost::default();
    let (session, worker) = fixture(&host, data.path()).await;
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::CloseRequested,
        )
        .await
        .unwrap();
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::Closed,
        )
        .await
        .unwrap();
    let domain = PeerAuthority::new(data.path().canonicalize().unwrap(), Uuid::now_v7()).unwrap();
    host.bind_peer_authority(domain.clone()).unwrap();
    let held = domain
        .acquire(
            AuthorityPlan::new([(
                AuthorityKey::Session(session.id()),
                AuthorityMode::Exclusive,
            )])
            .unwrap(),
            tokio_util::sync::CancellationToken::new(),
            std::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    let request = crate::flow_store::SessionRemoval {
        session_id: session.id(),
        expected: session.snapshot().await.unwrap().cursor,
    };
    let mut deleting = Box::pin(host.delete_session(data.path(), request.clone()));
    assert!(futures::poll!(deleting.as_mut()).is_pending());
    tokio::time::timeout(Duration::from_secs(2), async {
        while session.source_reads.clone().try_read_owned().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // On this current-thread runtime, the accepted task has yielded at its gate wait.
    assert!(host.directory.opening.try_lock().is_ok());
    let reopening = {
        let host = host.clone();
        let data = data.path().to_owned();
        let session_id = session.id();
        tokio::spawn(async move { host.open_session(&data, session_id).await })
    };
    tokio::task::yield_now().await;
    assert!(!reopening.is_finished());
    tokio::time::timeout(
        Duration::from_secs(2),
        host.create_session(
            data.path(),
            SessionDescriptor {
                id: Uuid::now_v7(),
                title: "Unrelated owner".into(),
                project: None,
                configuration: Default::default(),
            },
        ),
    )
    .await
    .unwrap()
    .unwrap();
    held.release().await.unwrap();
    let (deleted, reopened) = tokio::time::timeout(Duration::from_secs(2), async {
        tokio::join!(deleting, reopening)
    })
    .await
    .unwrap();
    deleted.unwrap();
    if let Ok(reopened) = reopened.unwrap() {
        assert!(reopened.same_owner(&session));
    }
    assert!(session.is_stopped());
    assert_eq!(
        RuntimeStore::inspect_session_removal(data.path(), session.id()).unwrap(),
        Some(request)
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn prepared_store_rollback_preserves_original_readers_and_context_writers() {
    let data = tempfile::tempdir().unwrap();
    let host = RuntimeHost::default();
    let (session, worker) = fixture(&host, data.path()).await;
    let original = worker.context();
    let freeze = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(freeze) = session.admission.try_freeze() {
                return freeze;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(
        worker
            .schedule(crate::Executor::new(), |_| Ok(None))
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::WouldBlock
    );
    let prepared = worker.suspend_for_handoff().await.unwrap();
    assert!(!worker.is_stopped());
    assert!(Arc::ptr_eq(&original, &worker.context()));
    assert!(
        RuntimeStore::open(data.path())
            .unwrap()
            .open_session(session.id())
            .is_err()
    );
    drop(prepared);
    drop(freeze);
    assert!(worker.status().await.is_ok());
    assert!(
        worker.commands.same_channel(
            &session
                .loaded_flow(&worker.address().instance_id)
                .unwrap()
                .commands
        )
    );
    worker
        .accept_input(submission(&worker), |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(worker.snapshot().await.unwrap().pending.len(), 1);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn external_scheduler_retains_its_original_controller_until_it_stops() {
    let data = tempfile::tempdir().unwrap();
    let host = RuntimeHost::default();
    let (session, worker) = fixture(&host, data.path()).await;
    let controller = worker
        .schedule(crate::Executor::new(), |_| Ok(None))
        .unwrap();
    worker
        .scheduling_progress
        .subscribe()
        .wait_for(|progress| {
            matches!(
                progress,
                crate::flow_runtime::scheduler::SchedulingProgress::Idle(..)
            )
        })
        .await
        .unwrap();
    blocked_by(
        &host,
        session.id(),
        HandoffBlocker::Observation {
            instance_id: worker.address().instance_id.clone(),
        },
    )
    .await;
    assert_eq!(
        *controller.subscribe_status().borrow(),
        SchedulingStatus::Running
    );
    assert!(worker.status().await.is_ok());
    controller.stop().await.unwrap();
    released(&host, session.id(), Uuid::now_v7()).await;
    let error = worker
        .schedule(crate::Executor::new(), |_| Ok(None))
        .err()
        .unwrap();
    assert!(
        error
            .get_ref()
            .unwrap()
            .downcast_ref::<SessionOwnerReleased>()
            .is_some()
    );
    host.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn peer_effect_retains_original_gate_until_accepted_writer_acknowledgement() {
    use crate::peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan, PeerAuthority};
    let data = tempfile::tempdir().unwrap();
    let host = RuntimeHost::default();
    let (source, source_worker) = fixture(&host, data.path()).await;
    let (target, target_worker) = fixture(&host, data.path()).await;
    let domain = PeerAuthority::new(data.path().canonicalize().unwrap(), Uuid::now_v7()).unwrap();
    host.bind_peer_authority(domain.clone()).unwrap();
    let guard = Arc::new(
        domain
            .acquire(
                AuthorityPlan::new([
                    (AuthorityKey::Session(source.id()), AuthorityMode::Shared),
                    (AuthorityKey::Session(target.id()), AuthorityMode::Exclusive),
                ])
                .unwrap(),
                tokio_util::sync::CancellationToken::new(),
                std::time::Instant::now() + Duration::from_secs(5),
            )
            .await
            .unwrap(),
    );
    let delivery = submission(&target_worker);
    let (entered, observed) = oneshot::channel();
    let (finish, resumed) = std::sync::mpsc::channel();
    let writer = target_worker.clone();
    let effect = tokio::spawn(async move {
        writer
            .request_with_authority(guard, move |store| {
                let _ = entered.send(());
                resumed.recv().unwrap();
                store.accept_input(delivery, |_| Ok(()))
            })
            .await
    });
    observed.await.unwrap();
    effect.abort();
    let revision = source_worker.subscribe_state().borrow().revision();
    let mut invalidate = Box::pin(source_worker.transition(
        revision,
        FlowTransition::ExecutionPaused {
            key: Uuid::now_v7().into(),
        },
    ));
    assert!(futures::poll!(invalidate.as_mut()).is_pending());
    assert!(!host.try_shutdown_idle().await.unwrap());
    finish.send(()).unwrap();
    invalidate.await.unwrap();
    assert_eq!(target_worker.snapshot().await.unwrap().pending.len(), 1);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn idle_shutdown_fences_profile_and_sessions_without_cancelling_original_work() {
    let data = tempfile::tempdir().unwrap();
    let host = RuntimeHost::default();
    let (_, worker) = fixture(&host, data.path()).await;
    host.open_profile(data.path()).await.unwrap();
    let compact = worker.context().compact_lock_handle().lock_owned().await;
    assert!(!host.try_shutdown_idle().await.unwrap());
    assert!(worker.status().await.is_ok());
    drop(compact);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if host.try_shutdown_idle().await.unwrap() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(host.sessions().is_empty());
    assert!(worker.status().await.is_err());
    host.shutdown().await.unwrap();
    assert!(
        crate::flow_store::profile::ProfileStore::open(&RuntimeStore::open(data.path()).unwrap())
            .is_ok()
    );
}

#[tokio::test]
async fn crash_recovery_validates_release_lineage_and_keeps_advanced_turn_interrupted() {
    use crate::peer_authority::PeerAuthority;
    for has_release in [false, true] {
        let data = tempfile::tempdir().unwrap();
        let domain =
            PeerAuthority::new(data.path().canonicalize().unwrap(), Uuid::now_v7()).unwrap();
        let source = RuntimeHost::default();
        let (session, initial) = fixture(&source, data.path()).await;
        source.bind_peer_authority(domain.clone()).unwrap();
        let original = if has_release {
            Some(released(&source, session.id(), Uuid::now_v7()).await)
        } else {
            None
        };
        let destination = RuntimeHost::default();
        destination.bind_peer_authority(domain.clone()).unwrap();
        let worker = if let Some(receipt) = &original {
            destination
                .acquire_handoff(data.path(), receipt)
                .await
                .unwrap()
                .open_flow(initial.address().instance_id.clone())
                .await
                .unwrap()
        } else {
            initial
        };
        worker
            .accept_input(submission(&worker), |_| Ok(()))
            .await
            .unwrap();
        let lease = worker
            .request(|store| {
                let mut state = store.state().clone();
                let lease = state
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
            .unwrap();
        // Drop the actual writer without a completion, matching owner loss rather than a User control.
        drop(worker.shutdown().await.unwrap());
        if has_release {
            destination.shutdown().await.unwrap();
        } else {
            source.shutdown().await.unwrap();
        }
        let replacement = RuntimeHost::default();
        replacement.bind_peer_authority(domain).unwrap();
        let request = Uuid::now_v7();
        if let Some(receipt) = &original {
            let mut corrupt = receipt.clone();
            corrupt.catalog.digest = "foreign-prefix".into();
            assert!(
                replacement
                    .recover_handoff(
                        data.path(),
                        session.id(),
                        source.generation(),
                        request,
                        Some(&corrupt)
                    )
                    .await
                    .is_err()
            );
        }
        let installed = replacement
            .recover_handoff(
                data.path(),
                session.id(),
                source.generation(),
                request,
                original.as_ref(),
            )
            .await
            .unwrap();
        assert_eq!(
            replacement
                .installed_handoff(session.id(), request, source.generation())
                .unwrap(),
            Some(installed.clone())
        );
        assert_eq!(
            replacement
                .recover_handoff(
                    data.path(),
                    session.id(),
                    source.generation(),
                    request,
                    original.as_ref()
                )
                .await
                .unwrap(),
            installed
        );
        assert!(
            installed
                .flows
                .iter()
                .all(|flow| flow.scheduling != Some(HandoffScheduling::Running))
        );
        let restored = replacement.flow(&lease.flow).await.unwrap();
        assert!(
            matches!(restored.state().await.unwrap().state(), atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Interrupted { lease: retained, reason: atman_rt::Interruption::HostLost }) if retained == &lease)
        );
        replacement.shutdown().await.unwrap();
        source.shutdown().await.unwrap();
        destination.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn dropped_lifecycle_closes_new_principals_until_guarded_terminal_publication() {
    use crate::peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan, PeerAuthority};
    let data = tempfile::tempdir().unwrap();
    let host = RuntimeHost::default();
    let (session, worker) = fixture(&host, data.path()).await;
    let domain = PeerAuthority::new(data.path().canonicalize().unwrap(), Uuid::now_v7()).unwrap();
    host.bind_peer_authority(domain.clone()).unwrap();
    let registry = worker.context().flow_registry.clone();
    let run = crate::event::FlowRunId::now();
    let (identity, lifecycle) = registry
        .register_owned_instance(
            &worker,
            run.clone(),
            crate::flow_authority::EffectiveAuthority::root(
                &crate::trust::TrustConfig::default(),
                false,
                None,
            ),
        )
        .await
        .unwrap();
    let admitted = domain
        .acquire(
            AuthorityPlan::new([(AuthorityKey::Session(session.id()), AuthorityMode::Shared)])
                .unwrap(),
            tokio_util::sync::CancellationToken::new(),
            std::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    drop(lifecycle);
    assert!(!identity.principal_available());
    assert!(matches!(
        identity.execution_state(),
        crate::flow_authority::FlowExecutionState::Running
    ));
    assert!(!worker.resources().executions.is_empty());
    assert!(matches!(
        host.handoff_session(session.id(), Uuid::now_v7())
            .await
            .unwrap(),
        SessionHandoffOutcome::Pending { .. }
    ));
    drop(admitted);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !matches!(
            identity.execution_state(),
            crate::flow_authority::FlowExecutionState::Terminal
        ) || !worker.resources().executions.is_empty()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn cold_scope_validation_uses_original_metadata_without_recovering_a_worker() {
    use crate::peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan, PeerAuthority};
    let data = tempfile::tempdir().unwrap();
    let domain = PeerAuthority::new(data.path().canonicalize().unwrap(), Uuid::now_v7()).unwrap();
    let source = RuntimeHost::default();
    let (session, worker) = fixture(&source, data.path()).await;
    source.bind_peer_authority(domain.clone()).unwrap();
    let scope = crate::flow_store::task_scope::TaskScopeRef {
        owner: worker.address().clone(),
        id: Uuid::now_v7(),
    };
    worker
        .create_task_scope(
            crate::flow_store::task_scope::TaskScopeDefinition {
                id: scope.id,
                name: "Retained group".into(),
                goal: None,
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    let receipt = released(&source, session.id(), Uuid::now_v7()).await;
    let target = RuntimeHost::default();
    target.bind_peer_authority(domain.clone()).unwrap();
    let acquired = target.acquire_handoff(data.path(), &receipt).await.unwrap();
    assert!(acquired.loaded_flow(&scope.owner.instance_id).is_none());
    let prepared = target
        .prepare_scope_evidence(std::slice::from_ref(&scope))
        .await
        .unwrap();
    assert!(prepared[0].open && prepared[0].cancellation.is_none());
    assert!(acquired.loaded_flow(&scope.owner.instance_id).is_none());
    let guard = domain
        .acquire(
            AuthorityPlan::new([(AuthorityKey::Session(session.id()), AuthorityMode::Shared)])
                .unwrap(),
            tokio_util::sync::CancellationToken::new(),
            std::time::Instant::now() + Duration::from_secs(5),
        )
        .await
        .unwrap();
    assert!(
        target
            .scope_evidence_with_authority(&guard, std::slice::from_ref(&scope))
            .unwrap()[0]
            .open
    );
    drop(guard);
    let restored = target.flow(&scope.owner).await.unwrap();
    restored
        .cancel_task_scope(scope.id, |_| Ok(()))
        .await
        .unwrap();
    drop(restored.shutdown().await.unwrap());
    let current = target
        .prepare_scope_evidence(std::slice::from_ref(&scope))
        .await
        .unwrap();
    assert!(current[0].cancellation.is_some());
    assert!(acquired.loaded_flow(&scope.owner.instance_id).is_none());
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handoff_drains_accepted_input_and_preserves_pause_without_touching_other_sessions() {
    let data = tempfile::tempdir().unwrap();
    let source = RuntimeHost::default();
    let (session, worker) = fixture(&source, data.path()).await;
    let (other, _) = fixture(&source, data.path()).await;
    let delivery = submission(&worker);
    let expected = delivery.clone();
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (finish, resume) = std::sync::mpsc::channel();
    let writer = worker.clone();
    let pending = tokio::spawn(async move {
        writer
            .request(move |store| {
                let _ = entered.send(());
                resume.recv().unwrap();
                store.accept_input(delivery, |_| Ok(()))
            })
            .await
    });
    observed.await.unwrap();
    pending.abort();
    let request = Uuid::now_v7();
    assert!(
        matches!(source.handoff_session(session.id(), request).await.unwrap(),
        SessionHandoffOutcome::Pending { blockers, .. } if blockers.contains(&HandoffBlocker::Admission))
    );
    finish.send(()).unwrap();
    let acceptance = worker
        .accept_input(expected.clone(), |_| Ok(()))
        .await
        .unwrap();
    let revision = worker.state().await.unwrap().revision();
    worker
        .transition(
            revision,
            FlowTransition::ExecutionPaused {
                key: Uuid::now_v7().into(),
            },
        )
        .await
        .unwrap();
    let before = worker.status().await.unwrap();
    let receipt = released(&source, session.id(), request).await;
    assert!(other.snapshot().await.is_ok());
    assert_eq!(
        source.handoff_session(session.id(), request).await.unwrap(),
        SessionHandoffOutcome::Released {
            receipt: receipt.clone()
        }
    );
    assert!(
        worker
            .status()
            .await
            .unwrap_err()
            .get_ref()
            .unwrap()
            .downcast_ref::<SessionOwnerReleased>()
            .is_some()
    );
    assert!(
        source
            .open_session(data.path(), session.id())
            .await
            .is_err()
    );
    let target = RuntimeHost::default();
    let acquired = target.acquire_handoff(data.path(), &receipt).await.unwrap();
    let restored = acquired
        .open_flow(worker.address().instance_id.clone())
        .await
        .unwrap();
    assert_eq!(restored.status().await.unwrap().execution, before.execution);
    assert_eq!(
        restored.accept_input(expected, |_| Ok(())).await.unwrap(),
        acceptance
    );
    assert_eq!(restored.snapshot().await.unwrap().pending.len(), 1);
    restored
        .accept_input(submission(&restored), |_| Ok(()))
        .await
        .unwrap();
    target.acquire_handoff(data.path(), &receipt).await.unwrap();
    let mut changed = receipt.clone();
    changed.request_id = Uuid::now_v7();
    assert!(target.acquire_handoff(data.path(), &changed).await.is_err());
    assert!(
        RuntimeStore::open(data.path())
            .unwrap()
            .open_session(session.id())
            .is_err()
    );
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}

#[tokio::test]
async fn active_and_source_capture_keep_the_original_owner_live() {
    let data = tempfile::tempdir().unwrap();
    let host = RuntimeHost::default();
    let (session, worker) = fixture(&host, data.path()).await;
    let pin = session.retain_source().await.unwrap();
    assert!(
        matches!(host.handoff_session(session.id(), Uuid::now_v7()).await.unwrap(),
        SessionHandoffOutcome::Pending { blockers, .. } if blockers.contains(&HandoffBlocker::SourceCapture))
    );
    drop(pin);
    let lease = worker
        .request(|store| {
            let mut state = store.state().clone();
            let lease = state
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
        .unwrap();
    let before = worker.status().await.unwrap();
    assert!(
        matches!(host.handoff_session(session.id(), Uuid::now_v7()).await.unwrap(),
        SessionHandoffOutcome::Pending { blockers, .. } if blockers.iter().any(|blocker| matches!(blocker, HandoffBlocker::Active { .. })))
    );
    assert_eq!(worker.status().await.unwrap().execution, before.execution);
    worker
        .request(move |store| {
            store.transition(
                store.state().revision(),
                FlowTransition::TurnFinished {
                    lease,
                    outcome: atman_rt::TurnOutcome::Completed,
                },
            )
        })
        .await
        .unwrap();
    let receipt = released(&host, session.id(), Uuid::now_v7()).await;
    let target = RuntimeHost::default();
    let mut altered = receipt.clone();
    altered.flows[0].cursor = FlowPosition::Durable(CommitReceipt {
        journal_id: Uuid::now_v7(),
        sequence: 0,
        digest: "changed".into(),
    });
    assert!(target.acquire_handoff(data.path(), &altered).await.is_err());
    target.acquire_handoff(data.path(), &receipt).await.unwrap();
    target.shutdown().await.unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn profile_initialization_is_shared_cancel_safe_and_ownership_failure_can_retry() {
    let data = tempfile::tempdir().unwrap();
    let host = RuntimeHost::default();
    let held =
        crate::flow_store::profile::ProfileStore::open(&RuntimeStore::open(data.path()).unwrap())
            .unwrap();
    assert!(host.open_profile(data.path()).await.is_err());
    drop(held);
    let opening = host.directory.opening.lock().await;
    let mut attempted = Box::pin(host.open_profile(data.path()));
    assert!(futures::poll!(attempted.as_mut()).is_pending());
    drop(attempted);
    drop(opening);
    let (first, second) = tokio::join!(
        host.open_profile(data.path()),
        host.open_profile(data.path())
    );
    assert_eq!(
        first.unwrap().snapshot().await.unwrap().profile.cursor,
        second.unwrap().snapshot().await.unwrap().profile.cursor
    );
    assert!(host.profile_handle().is_some());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn first_profile_creation_keeps_owned_initialization_ahead_of_shutdown_fencing() {
    let data = tempfile::tempdir().unwrap();
    let host = RuntimeHost::default();
    let opening = host.directory.opening.lock().await;
    let mut initializing = Box::pin(host.open_profile(data.path()));
    assert!(futures::poll!(initializing.as_mut()).is_pending());
    tokio::task::yield_now().await;
    assert!(host.directory.finalizing.try_lock().is_err());
    assert!(host.authority_domain().is_none());
    drop(initializing);
    let closing = {
        let host = host.clone();
        tokio::spawn(async move { host.shutdown().await })
    };
    tokio::task::yield_now().await;
    assert!(
        !host
            .directory
            .stopping
            .load(std::sync::atomic::Ordering::Acquire)
    );
    assert!(!closing.is_finished());
    drop(opening);
    tokio::time::timeout(Duration::from_secs(2), closing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let profile_id =
        crate::flow_store::profile::ProfileStore::inspect_identity(data.path()).unwrap();
    assert_eq!(host.authority_domain().unwrap().profile_id(), profile_id);
    assert!(host.profile_handle().is_none());
    // A new host can acquire the completed identity; the cancelled initializer left no writer behind.
    let replacement = RuntimeHost::default();
    assert_eq!(
        replacement
            .open_profile(data.path())
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap()
            .profile
            .profile_id,
        profile_id
    );
    replacement.shutdown().await.unwrap();
}

#[tokio::test]
async fn retained_interactions_and_compaction_are_not_lost_at_an_idle_boundary() {
    let data = tempfile::tempdir().unwrap();
    let host = RuntimeHost::default();
    let (session, worker) = fixture(&host, data.path()).await;
    let registry = worker.context().forms();
    let _observer = registry.subscribe();
    let kind = crate::form::FormKind::Confirm {
        prompt: "Keep this form".into(),
    };
    let pending = crate::form::PendingForm {
        form_id: "pending-form".into(),
        run_id: crate::event::FlowRunId::now(),
        tool_use_id: "form-call".into(),
        form: crate::form::CompositeForm {
            questions: vec![crate::form::FormQuestion {
                id: "confirm".into(),
                kind: kind.clone(),
            }],
        },
        kind,
        emitted_at: chrono::Utc::now(),
    };
    let _answer = registry.request(pending);
    assert!(registry.expire("pending-form"));
    assert!(
        matches!(host.handoff_session(session.id(), Uuid::now_v7()).await.unwrap(),
        SessionHandoffOutcome::Pending { blockers, .. } if blockers.iter().any(|blocker| matches!(blocker, HandoffBlocker::Interactions { .. })))
    );
    assert_eq!(registry.list_pending().len(), 1);
    assert!(registry.cancel("pending-form"));
    let compact = worker.context().compact_lock_handle().lock_owned().await;
    blocked_by(
        &host,
        session.id(),
        HandoffBlocker::Interactions {
            instance_id: worker.address().instance_id.clone(),
        },
    )
    .await;
    drop(compact);
    released(&host, session.id(), Uuid::now_v7()).await;
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn failed_scheduling_is_preserved_and_repeated_installation_keeps_new_user_binding() {
    let data = tempfile::tempdir().unwrap();
    let source = RuntimeHost::default();
    let (session, worker) = fixture(&source, data.path()).await;
    worker
        .accept_input(submission(&worker), |_| Ok(()))
        .await
        .unwrap();
    let mut status = session
        .schedule_flow(
            worker.address().instance_id.clone(),
            crate::Executor::new(),
            |_| Err(io::Error::other("entry preparation failed")),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(*status.borrow_and_update(), SchedulingStatus::Failed(_)) {
                break;
            }
            status.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let receipt = released(&source, session.id(), Uuid::now_v7()).await;
    assert!(matches!(
        receipt.flows[0].scheduling,
        Some(HandoffScheduling::Failed { .. })
    ));
    let target = RuntimeHost::default();
    let acquired = target.acquire_handoff(data.path(), &receipt).await.unwrap();
    let failed = acquired
        .scheduling_status(worker.address().instance_id.clone())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        *failed.borrow(),
        SchedulingStatus::Failed("entry preparation failed".into())
    );
    let restored = acquired
        .open_flow(worker.address().instance_id.clone())
        .await
        .unwrap();
    assert_eq!(restored.snapshot().await.unwrap().pending.len(), 1);
    restored
        .accept_input(submission(&restored), |_| Ok(()))
        .await
        .unwrap();
    acquired
        .schedule_flow(
            worker.address().instance_id.clone(),
            crate::Executor::new(),
            |_| Ok(None),
        )
        .await
        .unwrap();
    target.acquire_handoff(data.path(), &receipt).await.unwrap();
    assert_eq!(
        *acquired
            .scheduling_status(worker.address().instance_id.clone())
            .await
            .unwrap()
            .unwrap()
            .borrow(),
        SchedulingStatus::Running
    );
    assert_eq!(restored.snapshot().await.unwrap().pending.len(), 2);
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}
