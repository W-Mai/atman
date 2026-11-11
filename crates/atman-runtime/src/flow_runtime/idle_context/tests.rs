use super::*;
use crate::{
    context_plan::{ContextRecordAuthority, ContextRecordBody, ContextRecordRetention},
    flow_store::{
        EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowTransition, RuntimeStore,
        SessionDescriptor, SessionStore,
        completion::{InvocationResult, StoredValue},
        inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity, UserContent},
        publication::MessagePublication,
        transient::{TransientBudget, TransientLimits, TransientRetention},
    },
};

fn fixture(
    memory: bool,
) -> (
    tempfile::TempDir,
    SessionStore,
    crate::flow_store::FlowStore,
    TurnId,
) {
    let directory = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(directory.path()).unwrap();
    let id = Uuid::now_v7();
    let mut session = root
        .create_session(SessionDescriptor {
            id,
            title: "Context".into(),
            configuration: Default::default(),
            project: None,
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
                sources: [(
                    "entry:main.at".into(),
                    "flow main() -> int { return 7 }".into(),
                )]
                .into(),
            },
        }
        .into(),
    };
    let mut store = if memory {
        let retention = TransientRetention::new(
            TransientBudget::new(64 * 1024 * 1024).unwrap(),
            Uuid::now_v7(),
            descriptor.address.instance_id.0.0,
            TransientLimits::default(),
        )
        .unwrap();
        session
            .create_transient_flow(descriptor, None, retention)
            .unwrap()
    } else {
        session.create_flow(descriptor).unwrap()
    };
    store
        .transition(store.state().revision(), FlowTransition::Activated)
        .unwrap();
    let turn = TurnId::now();
    let run = crate::event::FlowRunId::now();
    store
        .transition(
            store.state().revision(),
            FlowTransition::TurnStarted {
                turn: turn.clone(),
                run: run.clone(),
            },
        )
        .unwrap();
    let InstanceState::Open(InstanceActivity::Active(active)) = store.state().state() else {
        panic!("active")
    };
    let lease = active.lease.clone();
    store
        .publish_message(MessagePublication {
            id: Uuid::now_v7(),
            lease: lease.clone(),
            run,
            context_run: None,
            message: Message::user_text(turn.clone(), "question"),
        })
        .unwrap();
    store
        .finish_turn(lease, InvocationResult::Returned(StoredValue::Int(7)))
        .unwrap();
    (directory, session, store, turn)
}

fn update(worker: &FlowWorker, turn: &TurnId) -> IdleContextUpdate {
    IdleContextUpdate {
        id: Uuid::now_v7(),
        expected: worker.subscribe_commits().borrow().clone(),
        turn: turn.clone(),
        records: vec![ContextRecordSpec::new(
            "quickask.source",
            ContextRecordAuthority::Retrieved,
            ContextRecordRetention::Latest,
            ContextRecordBody::Text {
                text: "updated source".into(),
            },
        )],
    }
}

#[tokio::test]
async fn idle_context_append_reconciles_without_turn_or_compaction_and_reopens_durably() {
    for memory in [false, true] {
        let (_directory, session, mut store, turn) = fixture(memory);
        let address = store.state().address().clone();
        let history = store.history_messages().unwrap();
        let state = store.state().clone();
        let worker = FlowWorker::start(store).await.unwrap();
        let context = worker.context();
        let mut stream = context.stream_subscribe();
        let request = update(&worker, &turn);
        let saved = worker
            .append_idle_context(request.clone(), Arc::default(), |_| Ok(()))
            .await
            .unwrap();
        assert!(!saved.checkpoint.purpose.is_compaction());
        assert_eq!(context.messages().len(), history.len() + 1);
        assert_eq!(*worker.subscribe_state().borrow(), state);
        assert!(
            stream.try_recv().is_err(),
            "context append must not emit compaction or execution frames"
        );
        let retry = worker
            .append_idle_context(request.clone(), Arc::default(), |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(retry, saved);
        let mut changed = request;
        changed.turn = TurnId::now();
        assert!(
            worker
                .append_idle_context(changed, Arc::default(), |_| Ok(()))
                .await
                .is_err()
        );
        let mut store = worker.shutdown().await.unwrap();
        assert_eq!(store.history_messages().unwrap(), history);
        if !memory {
            drop(store);
            let mut restored = session.open_flow(&address.instance_id).unwrap();
            assert_eq!(
                restored.context_messages().unwrap(),
                saved.checkpoint.messages
            );
            assert_eq!(restored.history_messages().unwrap(), history);
            assert_eq!(
                restored.context_checkpoint(saved.checkpoint.id).unwrap(),
                Some(saved)
            );
        }
    }
}

#[tokio::test]
async fn idle_context_append_rejects_pending_input_and_stale_boundaries() {
    let (_directory, _session, store, turn) = fixture(true);
    let worker = FlowWorker::start(store).await.unwrap();
    let mut stale = update(&worker, &turn);
    if let FlowPosition::Transient { transient } = &mut stale.expected {
        transient.host_generation = Uuid::now_v7();
    }
    assert!(
        worker
            .append_idle_context(stale, Arc::default(), |_| Ok(()))
            .await
            .is_err()
    );
    worker
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
                payload: InboxPayload::UserSubmission {
                    content: UserContent {
                        text: "queued".into(),
                        images: vec![],
                        presentation: None,
                    },
                    invocation: Default::default(),
                },
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    let request = update(&worker, &turn);
    let position = request.expected.clone();
    assert!(
        worker
            .append_idle_context(request, Arc::default(), |_| Ok(()))
            .await
            .is_err()
    );
    assert_eq!(*worker.subscribe_commits().borrow(), position);
    worker.shutdown().await.unwrap();
}
