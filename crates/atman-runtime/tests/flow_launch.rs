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
        content: EntrySnapshot {
            name: "main".into(), path: "main.at".into(),
            bundle: RevisionBundle {
                entry: "entry:main.at".into(), edges: Default::default(),
                sources: [("entry:main.at".into(), r#"flow main(label: string, amount: int) -> string {
                    when amount != 42 { return final.answer(message: "wrong arguments") }
                    when env("effort") != "high" { return final.answer(message: "wrong invocation") }
                    return final.answer(message: label)
                }"#.into())].into(),
            },
        }.into(),
    }
}

fn initial(info: &FlowDescriptor, recipient: &FlowRef) -> LaunchInput {
    let mut previous_answer = Message::assistant_text(TurnId::now(), "inherited answer");
    previous_answer.origin = MessageOrigin::FinalAnswer;
    LaunchInput {
        workspace: None,
        initial_goal: None,
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
                user: None,
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
        ..
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

#[tokio::test]
async fn host_entry_launch_retains_scheduling_and_retries_do_not_resume_stopped_work() {
    use atman_runtime::flow_runtime::launch::EntryExecution;
    use atman_runtime::flow_store::launch::EntryBinding;

    async fn delivered(worker: &FlowWorker, expected: &str) {
        let mut changes = worker.subscribe_commits();
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                changes.borrow_and_update();
                if worker.state().await.unwrap().can_start_turn()
                    && worker.context().messages().iter().any(|message| {
                        message.origin == MessageOrigin::FinalAnswer
                            && message.text_concat() == expected
                    })
                {
                    return;
                }
                changes.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
    }

    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let mut info = session_info();
    let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(dir.path().join("config"));
    info.configuration = atman_runtime::flow_store::SessionConfiguration::Atman(hub.source());
    info.project = Some(
        atman_runtime::storage::ProjectContext::resolve(&hub, dir.path(), dir.path()).unwrap(),
    );
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let mut entry = descriptor(info.id);
    entry.entry_mut().unwrap().bundle.sources.insert(
        "entry:main.at".into(),
        r#"
        flow main(label: string, amount: int) -> string {
            contract { invocation { user_message: label } }
            when amount != 42 { return final.answer(message: "wrong arguments") }
            when env("effort") != "high" { return final.answer(message: "wrong invocation") }
            return final.answer(message: label)
        }
    "#
        .into(),
    );
    entry.entry_mut().unwrap().path = dir.path().join("main.at");
    std::fs::write(
        &entry.entry().unwrap().path,
        &entry.entry().unwrap().bundle.sources["entry:main.at"],
    )
    .unwrap();
    entry.entry_mut().unwrap().bundle = atman_runtime::source_program::load_program(
        &entry.entry().unwrap().path,
        &info.source_roots(),
    )
    .unwrap()
    .revision_bundle();
    assert!(
        entry
            .entry_mut()
            .unwrap()
            .bundle
            .entry
            .starts_with("project:")
    );
    std::fs::remove_file(&entry.entry().unwrap().path).unwrap();
    let authority = EffectiveAuthority::root(&Default::default(), false, None);
    let mut input = initial(&entry, &entry.address);
    input.request.sender = SenderIdentity::User(Uuid::new_v4());
    let presentation = atman_runtime::user_input::UserInputPresentation {
        prompt: "human input, separate from entry arguments".into(),
        quote: Some(atman_runtime::user_input::QuoteSnapshot {
            text: "quoted text".into(),
        }),
    };
    let content = atman_runtime::flow_store::inbox::UserContent {
        text: presentation.model_text(),
        images: vec![
            atman_runtime::attachment_store::AttachmentStore::at(std::path::Path::new(""))
                .import_base64("iVBORw0KGgo=", Some("image.png"))
                .unwrap(),
        ],
        presentation: Some(presentation),
    };
    let InboxPayload::Invocation { user, .. } = &mut input.request.payload else {
        unreachable!()
    };
    *user = Some(content.clone());
    input.result_route = None;
    input.execution = Some(EntryBinding {
        session_start: true,
        first_turn: TurnId::now(),
        first_run: RunId::now(),
        authority: authority.clone(),
    });
    let catalog = session.snapshot().await.unwrap().cursor;
    let runtime = atman_runtime::AtmanRuntime::build(atman_runtime::AtmanRuntimeOptions {
        events: Default::default(),
        mock: false,
        config: hub,
        project: info.project.clone(),
        home_dir: None,
        workspace_generation: "startup-hooks".into(),
    })
    .await
    .unwrap();
    let execution = || EntryExecution {
        executor: {
            let mut executor = runtime.executor.clone();
            executor.tool_ctx = atman_runtime::tool::ToolCtx::for_project(info.project.clone());
            let mut hooks = atman_runtime::lifecycle::LifecycleRunner::new();
            hooks.absorb(&atman_rt::parse_file(r#"
                on session.start { memory.todo.set(where: "startup", why: "container", how: "hook", expected_result: "once") }
                on session.end { memory.todo.set(where: "unexpected shutdown", why: "container", how: "hook", expected_result: "never") }
            "#).unwrap());
            executor.tool_ctx.lifecycle_hooks = Some(Arc::new(hooks));
            executor
        },
        authority: authority.clone(),
        trust: atman_runtime::trust::TrustConfig::default(),
        yield_interval: NonZeroUsize::new(16).unwrap(),
        catalog: catalog.clone(),
    };
    let mut invalid = input.clone();
    invalid.execution = None;
    assert!(
        host.launch_entry(entry.clone(), invalid, execution(), |_, _| Ok(()))
            .await
            .is_err()
    );
    assert!(session.snapshot().await.unwrap().catalog.flows.is_empty());
    let mut forged = input.clone();
    forged.request.sender = SenderIdentity::Flow(entry.address.clone());
    assert!(
        host.launch_entry(entry.clone(), forged, execution(), |_, _| Ok(()))
            .await
            .is_err()
    );
    assert!(session.snapshot().await.unwrap().catalog.flows.is_empty());
    let (launched, repeated) = tokio::join!(
        host.launch_entry(entry.clone(), input.clone(), execution(), |_, _| Ok(())),
        host.launch_entry(entry.clone(), input.clone(), execution(), |_, _| Ok(()))
    );
    let launched = launched.unwrap();
    let repeated = repeated.unwrap();
    assert_eq!(
        launched.record.acceptance.receipt,
        repeated.record.acceptance.receipt
    );
    assert_eq!(
        launched.record.acceptance.message_id,
        repeated.record.acceptance.message_id
    );
    let receipt = launched.record.acceptance.clone();
    let worker = launched.worker.clone();
    drop(launched);
    delivered(&worker, "first answer").await;
    let messages = worker.context().messages();
    let human = messages
        .iter()
        .find(|m| m.text_concat() == content.text)
        .unwrap();
    assert_eq!(human.origin, MessageOrigin::User);
    assert_eq!(human.turn_id, input.execution.as_ref().unwrap().first_turn);
    assert!(
        matches!(&human.parts[1], atman_runtime::message::MessagePart::Image { source } if source == &content.images[0])
    );
    let InboxPayload::Invocation { user, .. } = worker
        .launch_record()
        .await
        .unwrap()
        .unwrap()
        .input
        .request
        .payload
    else {
        unreachable!()
    };
    assert_eq!(user.as_ref(), Some(&content));
    assert!(
        session
            .scheduling_status(entry.address.instance_id.clone())
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        worker
            .completion(None)
            .await
            .unwrap()
            .unwrap()
            .completion
            .lease
            .turn,
        input.execution.as_ref().unwrap().first_turn
    );

    let pause = Uuid::new_v4().into();
    change(&worker, FlowTransition::ExecutionPaused { key: pause }).await;
    let next_input = |text: &str| DeliveryRequest {
        request_id: Uuid::new_v4(),
        sender: SenderIdentity::User(Uuid::new_v4()),
        recipient: entry.address.clone(),
        delivery: DeliveryMode::NextTurn,
        cause: None,
        correlation: None,
        scope: None,
        payload: InboxPayload::Interjection { text: text.into() },
    };
    host.deliver(next_input("after pause"), |_| Ok(()))
        .await
        .unwrap();
    let paused = worker.snapshot().await.unwrap();
    assert_eq!(paused.pending.len(), 1);
    let retry = host
        .launch_entry(entry.clone(), input.clone(), execution(), |_, _| {
            panic!("retry has no new effect")
        })
        .await
        .unwrap();
    assert_eq!(retry.record.acceptance.message_id, receipt.message_id);
    assert_eq!(retry.record.acceptance.receipt, receipt.receipt);
    assert!(matches!(
        retry.record.acceptance.disposition,
        InboxDisposition::InputCommitted { .. }
    ));
    assert_eq!(worker.snapshot().await.unwrap().state, paused.state);
    change(&worker, FlowTransition::ExecutionResumed { key: pause }).await;
    delivered(&worker, "after pause").await;

    session
        .stop_scheduling(entry.address.instance_id.clone())
        .await
        .unwrap();
    host.deliver(next_input("after explicit bind"), |_| Ok(()))
        .await
        .unwrap();
    host.launch_entry(entry.clone(), input, execution(), |_, _| {
        panic!("retry has no new effect")
    })
    .await
    .unwrap();
    assert!(
        session
            .scheduling_status(entry.address.instance_id.clone())
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(worker.snapshot().await.unwrap().pending.len(), 1);
    session
        .bind_entry(
            entry.address.instance_id.clone(),
            execution().executor,
            authority.clone(),
            Default::default(),
            NonZeroUsize::new(16).unwrap(),
        )
        .await
        .unwrap();
    delivered(&worker, "after explicit bind").await;
    assert_eq!(worker.planning().await.unwrap().todos.len(), 1);
    assert_eq!(worker.planning().await.unwrap().todos[0].where_, "startup");
    session
        .stop_scheduling(entry.address.instance_id.clone())
        .await
        .unwrap();
    worker.shutdown().await.unwrap();

    // Receipt recovery does not acquire execution authority or rebind a controller.
    let other = descriptor(info.id);
    let mut input = initial(&other, &other.address);
    input.result_route = None;
    input.execution = Some(EntryBinding {
        session_start: false,
        first_turn: TurnId::now(),
        first_run: RunId::now(),
        authority: EffectiveAuthority::root(&Default::default(), false, Some(dir.path().into())),
    });
    let mut invalid_start = input.clone();
    invalid_start.request.sender = SenderIdentity::User(Uuid::new_v4());
    invalid_start.execution.as_mut().unwrap().session_start = true;
    assert!(
        host.launch_entry(other.clone(), invalid_start, execution(), |_, _| Ok(()))
            .await
            .is_err()
    );
    let accepted = host
        .launch_flow(other.clone(), input.clone(), |_, _| Ok(()))
        .await
        .unwrap();
    let retried = host
        .launch_entry(other, input, execution(), |_, _| {
            panic!("receipt lookup requires no new authority")
        })
        .await
        .unwrap();
    assert_eq!(retried.record.acceptance, accepted.record.acceptance);
    accepted.worker.shutdown().await.unwrap();
    host.shutdown().await.unwrap();
    let reopened = RuntimeHost::default();
    let session = reopened
        .attach_session(root.open_session(info.id).unwrap())
        .unwrap();
    let worker = reopened.flow(&entry.address).await.unwrap();
    session
        .bind_entry(
            entry.address.instance_id.clone(),
            execution().executor,
            authority.clone(),
            Default::default(),
            NonZeroUsize::new(16).unwrap(),
        )
        .await
        .unwrap();
    reopened
        .deliver(next_input("after reopen"), |_| Ok(()))
        .await
        .unwrap();
    delivered(&worker, "after reopen").await;
    assert_eq!(worker.planning().await.unwrap().todos.len(), 1);
    reopened.shutdown().await.unwrap();
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
    let mut input = initial(&source_info, recipient.state().address());
    input.initial_goal = Some("Finish the requested work".into());
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
                expires_at: None,
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
        session_start: false,
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
    let mut peer_start = input.clone();
    peer_start.execution = Some(atman_runtime::flow_store::launch::EntryBinding {
        session_start: true,
        first_turn: TurnId::now(),
        first_run: RunId::now(),
        authority: EffectiveAuthority::root(&Default::default(), false, None),
    });
    assert!(
        source
            .activate_with_input(peer_start, Some(proof.clone()), |_| Ok(()))
            .is_err()
    );
    let mut invalid_goal = input.clone();
    invalid_goal.initial_goal = Some("  ".into());
    assert!(
        source
            .activate_with_input(invalid_goal, Some(proof.clone()), |_| Ok(()))
            .is_err()
    );
    assert_eq!(source.state().state(), &InstanceState::Prepared);
    assert!(source.goal().is_none());
    let activation_start = std::fs::metadata(source.dir().join("events.jsonl"))
        .unwrap()
        .len() as usize;
    let committed = source
        .activate_with_input(input.clone(), Some(proof.clone()), |_| Ok(()))
        .unwrap();
    assert_eq!(source.cursor(), &committed.acceptance.receipt);
    let goal = source.goal().cloned().unwrap();
    assert_eq!(goal.objective, "Finish the requested work");
    assert_eq!(goal.revision, 1);
    assert_eq!(
        source.goal_updates()[0].receipt,
        committed.acceptance.receipt
    );
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
    assert_eq!(source.goal(), Some(&goal));
    assert_eq!(source.goal_updates().len(), 1);
    assert_eq!(
        source
            .activate_with_input(input.clone(), None, |_| panic!("retry reauthorized"))
            .unwrap()
            .acceptance,
        committed.acceptance
    );
    assert_eq!(source.state().pauses(), &[pause]);
    let mut changed_goal = input.clone();
    changed_goal.initial_goal = Some("different objective".into());
    assert!(
        source
            .activate_with_input(changed_goal, None, |_| Ok(()))
            .is_err()
    );
    assert_eq!(source.goal(), Some(&goal));
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
    assert!(recovered.goal().is_none());
    assert!(recovered.goal_updates().is_empty());
    recovered
        .activate_with_input(input, Some(proof), |_| Ok(()))
        .unwrap();
    assert_eq!(recovered.pending_inputs().count(), 1);
    assert_eq!(
        recovered.goal().unwrap().objective,
        "Finish the requested work"
    );
    assert_eq!(recovered.goal_updates().len(), 1);
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
