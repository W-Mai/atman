use super::*;
use crate::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowStore, FlowTransition,
    RuntimeStore, SessionDescriptor, SessionStore,
    completion::{InvocationResult, StoredValue},
};
use atman_rt::{InstanceActivity, InstanceState, RunId, TurnId, TurnPhase};

fn fixture() -> (tempfile::TempDir, SessionStore, FlowStore, TransientBudget) {
    fixture_with_source("flow main() -> int { return 7 }")
}

fn fixture_with_source(
    source: &str,
) -> (tempfile::TempDir, SessionStore, FlowStore, TransientBudget) {
    let directory = tempfile::tempdir().unwrap();
    let runtime = RuntimeStore::open(directory.path()).unwrap();
    let id = Uuid::now_v7();
    let mut session = runtime
        .create_session(SessionDescriptor {
            id,
            project: None,
            configuration: Default::default(),
            title: "Retained".into(),
        })
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
                sources: [("entry:main.at".into(), source.into())].into(),
            },
        }
        .into(),
    };
    let budget = TransientBudget::new(128 * 1024).unwrap();
    let retention = TransientRetention::new(
        budget.clone(),
        Uuid::now_v7(),
        descriptor.address.instance_id.0.0,
        TransientLimits {
            maximum_bytes: 64 * 1024,
            terminal_reserve_bytes: 16 * 1024,
        },
    )
    .unwrap();
    let cursor = session.cursor().clone();
    let flow = session
        .create_transient_flow(descriptor, Some(&cursor), retention)
        .unwrap();
    (directory, session, flow, budget)
}

#[test]
fn transient_storage_retains_identity_without_registration_and_releases_snapshots() {
    let (_directory, session, mut flow, budget) = fixture();
    assert!(session.projection().flows.is_empty());
    assert!(flow.directory().is_none());
    assert!(flow.dir().is_err());
    let first = flow.cursor().clone();
    let mut foreign = first.clone();
    if let FlowPosition::Transient { transient } = &mut foreign {
        transient.host_generation = Uuid::now_v7();
    }
    assert!(
        flow.read_updates(Some(&foreign), NonZeroUsize::new(1).unwrap())
            .is_err()
    );
    if let FlowPosition::Transient { transient } = &mut foreign {
        transient.host_generation = flow.transient_retention().unwrap().host_generation();
        transient.flow_id = Uuid::now_v7();
    }
    assert!(
        flow.read_updates(Some(&foreign), NonZeroUsize::new(1).unwrap())
            .is_err()
    );
    assert_eq!(flow.cursor(), &first);
    let captured = flow.freeze_transient().unwrap();
    assert!(
        flow.transition(flow.state().revision(), FlowTransition::Activated)
            .is_err()
    );
    assert_eq!(captured.read_updates(0).unwrap().receipt, first);
    assert!(flow.thaw_transient(Uuid::now_v7()).is_err());
    flow.thaw_transient(captured.id).unwrap();
    flow.transition(flow.state().revision(), FlowTransition::Activated)
        .unwrap();
    assert_eq!(captured.len(), 1);
    let accepted = flow
        .accept_input(
            crate::flow_store::inbox::DeliveryRequest {
                expected_head: None,
                request_id: Uuid::now_v7(),
                sender: crate::flow_store::inbox::SenderIdentity::User(Uuid::now_v7()),
                recipient: flow.state().address().clone(),
                delivery: crate::flow_store::inbox::DeliveryMode::NextTurn,
                cause: None,
                correlation: None,
                scope: None,
                payload: crate::flow_store::inbox::InboxPayload::Invocation {
                    arguments: Default::default(),
                    invocation: Default::default(),
                    user: None,
                },
            },
            |_| Ok(()),
        )
        .unwrap();
    let pending = flow.freeze_transient().unwrap();
    assert_eq!(pending.cursor, accepted.receipt);
    assert_eq!(flow.pending_inputs().count(), 1);
    assert!(pending.read_updates(pending.len() - 1).unwrap().facts.iter().any(|fact|
        matches!(fact, crate::flow_store::view::FlowUpdate::InputAccepted(input) if matches!(input.as_ref(), crate::flow_store::view::AcceptedInput::Envelope(envelope) if envelope.id == accepted.message_id))
    ));
    let usage = budget.retained_bytes();
    drop(flow);
    assert_eq!(budget.retained_bytes(), usage);
    drop(captured);
    assert!(budget.retained_bytes() > 0);
    drop(pending);
    assert_eq!(budget.retained_bytes(), 0);
}

#[test]
fn transient_capacity_preserves_terminal_reserve_and_allocation_lifetimes() {
    let (_directory, _session, mut flow, budget) = fixture();
    flow.transition(flow.state().revision(), FlowTransition::Activated)
        .unwrap();
    flow.transition(
        flow.state().revision(),
        FlowTransition::TurnStarted {
            turn: TurnId::now(),
            run: RunId::now(),
        },
    )
    .unwrap();
    let InstanceState::Open(InstanceActivity::Active(active)) = flow.state().state() else {
        panic!("active turn");
    };
    let lease = active.lease.clone();
    let retention = flow.transient_retention().unwrap().clone();
    let usage = retention.usage();
    let allocation = retention
        .reserve(usage.maximum_bytes - usage.terminal_reserve_bytes - usage.retained_bytes)
        .unwrap();
    let cloned = allocation.clone();
    let before = flow.cursor().clone();
    assert!(is_capacity_error(&retention.reserve(1).err().unwrap()));
    assert_eq!(flow.cursor(), &before);
    flow.observe_retention_limit().unwrap();
    assert!(
        matches!(flow.state().state(), InstanceState::Open(InstanceActivity::Active(active)) if active.phase == TurnPhase::Cancelling)
    );
    let used = retention.usage().retained_bytes;
    let cancelled = flow.cursor().clone();
    assert_eq!(
        flow.transition(
            flow.state().revision(),
            FlowTransition::PhaseChanged {
                lease: lease.clone(),
                phase: TurnPhase::Cancelling,
            }
        )
        .unwrap(),
        cancelled
    );
    assert!(is_capacity_error(
        &flow
            .control(crate::flow_store::control::ControlRequest {
                id: Uuid::now_v7(),
                sender: crate::flow_store::inbox::SenderIdentity::User(Uuid::now_v7()),
                target: flow.state().address().clone(),
                expected_revision: flow.state().revision(),
                pause_key: Uuid::now_v7().into(),
                action: crate::flow_store::control::ControlAction::CancelTurn,
            })
            .unwrap_err()
    ));
    assert_eq!(retention.usage().retained_bytes, used);
    assert!(retention.usage().admission_closed);
    assert!(is_capacity_error(
        &flow
            .finish_turn(
                lease.clone(),
                InvocationResult::Returned(StoredValue::String("x".repeat(4096)))
            )
            .unwrap_err()
    ));
    flow.finish_turn(
        lease,
        InvocationResult::Failed(crate::RuntimeError::Cancelled("retention limit".into())),
    )
    .unwrap();
    flow.transition(flow.state().revision(), FlowTransition::CloseRequested)
        .unwrap();
    flow.transition(flow.state().revision(), FlowTransition::Closed)
        .unwrap();
    assert!(matches!(flow.state().state(), InstanceState::Closed));
    let retained = retention.usage().retained_bytes;
    drop(allocation);
    assert_eq!(retention.usage().retained_bytes, retained);
    assert!(cloned.bytes() > 0);
    let retained = flow.freeze_transient().unwrap();
    drop(cloned);
    assert!(is_capacity_error(&retention.reserve(1).err().unwrap()));
    drop(flow);
    drop(retention);
    assert!(budget.retained_bytes() > 0);
    drop(retained);
    assert_eq!(budget.retained_bytes(), 0);
}

#[tokio::test]
async fn transient_capacity_and_freeze_leave_worker_observation_available() {
    let (_directory, _session, mut flow, budget) = fixture();
    flow.transition(flow.state().revision(), FlowTransition::Activated)
        .unwrap();
    flow.transition(
        flow.state().revision(),
        FlowTransition::TurnStarted {
            turn: TurnId::now(),
            run: RunId::now(),
        },
    )
    .unwrap();
    let InstanceState::Open(InstanceActivity::Active(active)) = flow.state().state() else {
        panic!("active turn");
    };
    let lease = active.lease.clone();
    let retention = flow.transient_retention().unwrap().clone();
    let worker = crate::flow_runtime::FlowWorker::start(flow).await.unwrap();
    let sink = worker.execution_sink(lease.clone(), &crate::event::EventSink::new());
    let usage = retention.usage();
    let allocation = retention
        .reserve(usage.maximum_bytes - usage.terminal_reserve_bytes - usage.retained_bytes)
        .unwrap();
    sink.emit(crate::event::Event::MermaidDiagram {
        source: "retention limit".into(),
    });
    let state = worker.state().await.unwrap();
    assert!(
        matches!(state.state(), InstanceState::Open(InstanceActivity::Active(active)) if active.phase == TurnPhase::Cancelling)
    );
    worker
        .request(move |store| {
            store.finish_turn(
                lease,
                InvocationResult::Failed(crate::RuntimeError::Cancelled(
                    "retention capacity".into(),
                )),
            )
        })
        .await
        .unwrap();
    drop(allocation);
    let capture = worker
        .request(|store| store.freeze_transient())
        .await
        .unwrap();
    sink.emit(crate::event::Event::MermaidDiagram {
        source: "late diagnostic".into(),
    });
    assert!(worker.completion(None).await.unwrap().is_some());
    assert_eq!(*worker.subscribe_commits().borrow(), capture.cursor);
    assert!(!worker.is_stopped());
    worker
        .request(move |store| store.thaw_transient(capture.id))
        .await
        .unwrap();
    assert!(worker.execution_events().await.unwrap().is_empty());
    drop(worker.shutdown().await.unwrap());
    drop(sink);
    drop(retention);
    drop(capture);
    assert_eq!(budget.retained_bytes(), 0);
}

#[tokio::test]
async fn transient_oversized_native_return_is_an_explicit_completion_failure() {
    use crate::flow_store::inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity};
    let mut source = String::from("flow main() -> string {\nx0 = \"x\"\n");
    for index in 1..=18 {
        source.push_str(&format!("x{index} = x{} + x{}\n", index - 1, index - 1));
    }
    source.push_str("return x18\n}");
    let (_directory, _session, mut flow, budget) = fixture_with_source(&source);
    flow.transition(flow.state().revision(), FlowTransition::Activated)
        .unwrap();
    let worker = crate::flow_runtime::FlowWorker::start(flow).await.unwrap();
    let accepted = worker
        .accept_input(
            DeliveryRequest {
                expected_head: None,
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
                    user: None,
                },
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    let ticket = worker
        .start_turn(
            crate::Executor::new(),
            crate::flow_runtime::execution::TurnRequest {
                turn: TurnId::now(),
                run: RunId::now(),
                messages: vec![accepted.message_id],
                arguments: Default::default(),
                invocation: Default::default(),
                context: vec![],
                authority: crate::flow_authority::EffectiveAuthority::root(
                    &Default::default(),
                    false,
                    None,
                )
                .read_only(),
                yield_interval: NonZeroUsize::new(16).unwrap(),
            },
        )
        .await
        .unwrap();
    let completion = tokio::time::timeout(std::time::Duration::from_secs(5), ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(&completion.result, Err(crate::RuntimeError::ToolFailed(reason)) if reason.contains("retention limit"))
    );
    let stored = worker.completion(None).await.unwrap().unwrap();
    assert_eq!(
        stored.completion.result,
        InvocationResult::Failed(completion.result.unwrap_err())
    );
    assert!(!worker.is_stopped());
    drop(worker.shutdown().await.unwrap());
    assert_eq!(budget.retained_bytes(), 0);
}
