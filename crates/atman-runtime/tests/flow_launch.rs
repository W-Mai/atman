use std::{io, num::NonZeroUsize, sync::Arc, time::Duration};

use atman_rt::{InstanceState, RunId, TurnId};
use atman_runtime::flow_runtime::{
    FlowSnapshot, FlowWorker, execution::TurnRequest, host::RuntimeHost,
};
use atman_runtime::flow_store::inbox::{
    DeliveryMode, DeliveryRequest, InboxDisposition, InboxPayload, SenderIdentity,
};
use atman_runtime::flow_store::launch::{LaunchInput, ResultRoute};
use atman_runtime::flow_store::subscription::{
    FlowEventFilter, FlowSubscription, SubscriptionPurpose,
};
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowTransition, RuntimeStore,
    SessionDescriptor,
};
use atman_runtime::message::{Message, MessageOrigin};
use atman_runtime::source_program::RevisionBundle;
use atman_runtime::{Executor, flow_authority::EffectiveAuthority};
use uuid::Uuid;

fn session_info() -> SessionDescriptor {
    SessionDescriptor {
        configuration: Default::default(),
        id: Uuid::new_v4(),
        title: "Launch".into(),
        project: None,
    }
}

fn descriptor(session: Uuid) -> FlowDescriptor {
    FlowDescriptor {
        address: FlowRef { session_id: session.into(), instance_id: FlowInstanceId::now() },
        creation_request: Uuid::new_v4(), created_by: None,
        entry: EntrySnapshot {
            name: "main".into(), path: "main.at".into(),
            bundle: RevisionBundle {
                entry: "entry:main.at".into(), edges: Default::default(),
                sources: [("entry:main.at".into(), r#"flow main(label: string, amount: int) -> string {
                    when amount != 42 { return final.answer(message: "wrong arguments") }
                    when env("effort") != "high" { return final.answer(message: "wrong invocation") }
                    return final.answer(message: label)
                }"#.into())].into(),
            },
        },
    }
}

fn initial(info: &FlowDescriptor, recipient: &FlowRef) -> LaunchInput {
    let mut previous_answer = Message::assistant_text(TurnId::now(), "inherited answer");
    previous_answer.origin = MessageOrigin::FinalAnswer;
    LaunchInput {
        execution: None,
        request: DeliveryRequest {
            request_id: info.creation_request,
            sender: SenderIdentity::Flow(recipient.clone()),
            recipient: info.address.clone(),
            delivery: DeliveryMode::NextTurn,
            cause: None,
            correlation: None,
            scope: None,
            payload: InboxPayload::Invocation {
                arguments: [
                    ("label".into(), "first answer".into()),
                    ("amount".into(), 42.into()),
                ]
                .into(),
                invocation: [("effort".into(), "high".into())].into(),
            },
        },
        inherited_context: vec![
            Message::user_text(TurnId::now(), "previous context"),
            previous_answer,
        ],
        result_route: Some(ResultRoute {
            recipient: recipient.clone(),
            subscription: Uuid::new_v4(),
            synchronous: None,
        }),
    }
}

fn prepare(snapshot: &FlowSnapshot) -> io::Result<Option<TurnRequest>> {
    let first = snapshot.pending.first().unwrap();
    let InboxPayload::Invocation {
        arguments,
        invocation,
    } = &first.request.payload
    else {
        return Ok(None);
    };
    let turn = TurnId::now();
    Ok(Some(TurnRequest {
        turn: turn.clone(),
        run: RunId::now(),
        messages: vec![first.id],
        arguments: arguments.clone(),
        invocation: invocation.clone(),
        context: vec![atman_runtime::flow_runtime::input::context_message(
            first, &turn,
        )?],
        authority: EffectiveAuthority::root(&Default::default(), false, None),
        yield_interval: NonZeroUsize::new(16).unwrap(),
    }))
}

async fn change(worker: &FlowWorker, transition: FlowTransition) {
    worker
        .transition(worker.state().await.unwrap().revision(), transition)
        .await
        .unwrap();
}

async fn wait_answer(recipient: &FlowWorker) -> FlowSnapshot {
    let mut commits = recipient.subscribe_commits();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            commits.borrow_and_update();
            let snapshot = recipient.snapshot().await.unwrap();
            if !snapshot.pending.is_empty() {
                return snapshot;
            }
            commits.changed().await.unwrap();
        }
    })
    .await
    .unwrap()
}

#[test]
fn activation_requires_proof_commits_input_atomically_and_recovers_matching_retries() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_info();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut recipient = session.create_flow(descriptor(info.id)).unwrap();
    recipient.transition(0, FlowTransition::Activated).unwrap();
    let source_info = descriptor(info.id);
    let mut source = session.create_flow(source_info.clone()).unwrap();
    let input = initial(&source_info, recipient.state().address());
    let mut invalid_mid_turn = input.request.clone();
    invalid_mid_turn.recipient = recipient.state().address().clone();
    invalid_mid_turn.delivery = DeliveryMode::AtBoundary;
    assert!(
        recipient
            .accept_input(invalid_mid_turn, |_| Ok(()))
            .is_err()
    );
    assert!(
        source
            .activate_with_input(input.clone(), None, |_| Ok(()))
            .is_err()
    );
    assert_eq!(source.state().state(), &InstanceState::Prepared);
    assert!(source.pending_inputs().next().is_none());
    let route = input.result_route.as_ref().unwrap();
    recipient
        .subscribe_events(
            FlowSubscription {
                id: route.subscription,
                source: source_info.address.clone(),
                filter: FlowEventFilter::FinalAnswers,
                once: false,
                purpose: SubscriptionPurpose::SpawnResult,
                reply: None,
            },
            &source.observation_anchor(),
        )
        .unwrap();
    let proof = recipient
        .result_subscription_proof(route.subscription, None)
        .unwrap();
    let mut invalid_binding = input.clone();
    invalid_binding.execution = Some(atman_runtime::flow_store::launch::EntryBinding {
        first_turn: TurnId(Uuid::nil().into()),
        first_run: RunId::now(),
        authority: EffectiveAuthority::root(&Default::default(), false, None),
    });
    assert!(
        source
            .activate_with_input(invalid_binding, Some(proof.clone()), |_| Ok(()))
            .is_err()
    );
    assert_eq!(source.state().state(), &InstanceState::Prepared);
    assert!(source.pending_inputs().next().is_none());
    let pause = Uuid::new_v4().into();
    let activation_start = std::fs::metadata(source.dir().join("events.jsonl"))
        .unwrap()
        .len() as usize;
    let committed = source
        .activate_with_input(input.clone(), Some(proof.clone()), |_| Ok(()))
        .unwrap();
    assert_eq!(source.cursor(), &committed.acceptance.receipt);
    source
        .transition(
            source.state().revision(),
            FlowTransition::ExecutionPaused { key: pause },
        )
        .unwrap();
    assert_eq!(source.state().pauses(), &[pause]);
    assert_eq!(source.pending_inputs().count(), 1);
    assert_eq!(source.context_messages(), input.inherited_context);
    let file = source.dir().join("events.jsonl");
    drop(source);
    let mut source = session.open_flow(&source_info.address.instance_id).unwrap();
    assert_eq!(source.launch_record().unwrap().input, input);
    assert_eq!(
        source
            .activate_with_input(input.clone(), None, |_| panic!("retry reauthorized"))
            .unwrap()
            .acceptance,
        committed.acceptance
    );
    assert_eq!(source.state().pauses(), &[pause]);
    let mut changed = input.clone();
    changed.inherited_context.clear();
    assert!(
        source
            .activate_with_input(changed, None, |_| Ok(()))
            .is_err()
    );
    source
        .transition(source.state().revision(), FlowTransition::CloseRequested)
        .unwrap();
    assert!(matches!(
        source
            .activate_with_input(input.clone(), None, |_| panic!("closed retry reauthorized"))
            .unwrap()
            .acceptance
            .disposition,
        InboxDisposition::Cancelled { .. }
    ));
    assert!(matches!(
        source.state().state(),
        InstanceState::Closing { .. }
    ));
    drop(source);

    // An incomplete activation leaves neither activation nor input. Keep the complete creation prefix.
    let bytes = std::fs::read(&file).unwrap();
    let activation_length = bytes[activation_start..]
        .iter()
        .position(|b| *b == b'\n')
        .unwrap();
    std::fs::write(&file, &bytes[..activation_start + activation_length / 2]).unwrap();
    let mut recovered = session.open_flow(&source_info.address.instance_id).unwrap();
    assert_eq!(recovered.state().state(), &InstanceState::Prepared);
    assert!(recovered.launch_record().is_none());
    assert!(recovered.pending_inputs().next().is_none());
    assert!(recovered.context_messages().is_empty());
    recovered
        .activate_with_input(input, Some(proof), |_| Ok(()))
        .unwrap();
    assert_eq!(recovered.pending_inputs().count(), 1);
}

#[tokio::test]
async fn host_launch_registers_before_execution_and_restores_parameters_context_and_receipts() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_info();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let recipient = session.create_flow(descriptor(info.id)).await.unwrap();
    change(&recipient, FlowTransition::Activated).await;
    let mut source_info = descriptor(info.id);
    source_info.created_by = Some(recipient.address().clone());
    let input = initial(&source_info, recipient.address());
    let source = session.create_flow(source_info.clone()).await.unwrap();
    // A controller is already waiting: activation may run immediately after its commit.
    session
        .schedule_flow(
            source_info.address.instance_id.clone(),
            Executor::new(),
            prepare,
        )
        .await
        .unwrap();
    let (first, retry) = tokio::join!(
        host.launch_flow(source_info.clone(), input.clone(), |_, _| Ok(())),
        host.launch_flow(source_info.clone(), input.clone(), |_, _| Ok(()))
    );
    let first = first.unwrap();
    let retry = retry.unwrap();
    assert_eq!(
        first.record.acceptance.receipt,
        retry.record.acceptance.receipt
    );
    let received = wait_answer(&recipient).await;
    assert_eq!(received.pending.len(), 1);
    let InboxPayload::FlowEvent { content, .. } = &received.pending[0].request.payload else {
        panic!("missing result")
    };
    let message: Message = serde_json::from_value(content["message"].clone()).unwrap();
    assert_eq!(message.text_concat(), "first answer");
    let mut commits = source.subscribe_commits();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            commits.borrow_and_update();
            if source.state().await.unwrap().can_start_turn() {
                break;
            }
            commits.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let history = source.context().messages_full();
    assert_eq!(
        history
            .iter()
            .filter(|m| m.text_concat() == "previous context")
            .count(),
        1
    );
    assert!(history.iter().all(|m| !m.text_concat().contains("effort")));
    assert_eq!(
        history
            .iter()
            .filter(|m| m.origin == MessageOrigin::FinalAnswer)
            .count(),
        2
    );
    assert!(matches!(
        source
            .launch_record()
            .await
            .unwrap()
            .unwrap()
            .acceptance
            .disposition,
        InboxDisposition::InputCommitted { .. }
    ));
    let receipt = first.record.acceptance.receipt.clone();
    drop(first);
    drop(retry);
    session
        .stop_scheduling(source_info.address.instance_id.clone())
        .await
        .unwrap();
    drop(source.shutdown().await.unwrap());
    let restored = host
        .launch_flow(source_info.clone(), input.clone(), |_, _| {
            panic!("recovery reauthorized")
        })
        .await
        .unwrap();
    assert_eq!(
        restored.worker.context().messages_full().as_ref(),
        history.as_ref()
    );
    assert_eq!(restored.record.acceptance.receipt, receipt);
    assert!(restored.worker.snapshot().await.unwrap().pending.is_empty());
    let mut changed = input;
    let InboxPayload::Invocation { arguments, .. } = &mut changed.request.payload else {
        unreachable!()
    };
    arguments.insert("amount".into(), 43.into());
    assert!(
        host.launch_flow(source_info, changed, |_, _| Ok(()))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn failed_activation_stays_prepared_and_abandoned_retry_finishes_without_new_subscription() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_info();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let recipient = session.create_flow(descriptor(info.id)).await.unwrap();
    change(&recipient, FlowTransition::Activated).await;
    let source_info = descriptor(info.id);
    let input = initial(&source_info, recipient.address());
    // Fail at the last authorization boundary, after result registration committed.
    let checks = AtomicUsize::new(0);
    assert!(
        host.launch_flow(source_info.clone(), input.clone(), move |_, _| {
            if checks.fetch_add(1, Ordering::SeqCst) == 3 {
                Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "activation denied",
                ))
            } else {
                Ok(())
            }
        })
        .await
        .is_err()
    );
    let source = host.flow(&source_info.address).await.unwrap();
    assert_eq!(
        source.state().await.unwrap().state(),
        &InstanceState::Prepared
    );
    assert!(source.snapshot().await.unwrap().pending.is_empty());
    let registered = recipient.flow_subscriptions().await.unwrap();
    assert_eq!(registered.len(), 1);
    let registered_cursor = registered[0].started_after.clone();
    let (entered, notified) = tokio::sync::oneshot::channel();
    let entered = std::sync::Mutex::new(Some(entered));
    let (release, released) = std::sync::mpsc::channel();
    let released = std::sync::Mutex::new(released);
    let checks = Arc::new(AtomicUsize::new(0));
    let requests = checks.clone();
    let task_host = host.clone();
    let request_info = source_info.clone();
    let request_input = input.clone();
    let waiting = tokio::spawn(async move {
        task_host
            .launch_flow(request_info, request_input, move |_, _| {
                if requests.fetch_add(1, Ordering::SeqCst) == 2 {
                    entered.lock().unwrap().take().unwrap().send(()).unwrap();
                    released
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                }
                Ok(())
            })
            .await
    });
    notified.await.unwrap();
    waiting.abort();
    release.send(()).unwrap();
    let activated = wait_answer(&source).await;
    assert_eq!(activated.pending.len(), 1);
    assert_ne!(activated.state.state(), &InstanceState::Prepared);
    assert_eq!(
        recipient.flow_subscriptions().await.unwrap()[0].started_after,
        registered_cursor
    );
    let stored = source.launch_record().await.unwrap().unwrap();
    let retried = host
        .launch_flow(source_info, input, |_, _| {
            panic!("committed retry reauthorized")
        })
        .await
        .unwrap();
    assert_eq!(retried.record.acceptance.receipt, stored.acceptance.receipt);
}
