use std::collections::BTreeMap;

use atman_rt::{
    InstanceActivity, InstanceState, Interruption, RunId, TurnId, TurnLease, TurnOutcome, TurnPhase,
};
use atman_runtime::event::AtmanUuid;
use atman_runtime::flow_store::inbox::{
    DeliveryMode, DeliveryRequest, InboxDisposition, InboxPayload, InputContent, SenderIdentity,
};
use atman_runtime::flow_store::outbox::{DeliveryReceipt, SendStatus};
use atman_runtime::flow_store::waiter::{ReplyStatus, ReplyWaiter};
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowStore, FlowTransition,
    RuntimeStore, SessionDescriptor,
};
use atman_runtime::message::Message;
use atman_runtime::source_program::RevisionBundle;
use uuid::Uuid;

mod common;

fn turn_request(
    message: Uuid,
    argument: i64,
) -> atman_runtime::flow_runtime::execution::TurnRequest {
    let turn = TurnId::now();
    atman_runtime::flow_runtime::execution::TurnRequest {
        turn: turn.clone(),
        run: RunId::now(),
        messages: vec![message],
        arguments: BTreeMap::from([("input".into(), serde_json::json!(argument))]),
        invocation: BTreeMap::from([("marker".into(), serde_json::json!("recorded"))]),
        context: vec![Message::user_text(turn, "run the entry")],
        authority: atman_runtime::flow_authority::EffectiveAuthority::root(
            &Default::default(),
            false,
            None,
        ),
        yield_interval: std::num::NonZeroUsize::new(16).unwrap(),
    }
}

struct ExecutionProbe {
    entered: std::sync::Arc<tokio::sync::Semaphore>,
    release: std::sync::Arc<tokio::sync::Semaphore>,
}

struct InboxRecordingProvider {
    requests: std::sync::Mutex<Vec<atman_runtime::provider::LlmRequest>>,
    inner: atman_runtime::providers::mock::MockProvider,
}

impl atman_runtime::provider::Provider for InboxRecordingProvider {
    fn name(&self) -> &str {
        "mock"
    }

    fn call<'a>(
        &'a self,
        request: atman_runtime::provider::LlmRequest,
    ) -> atman_runtime::tool::BoxFut<
        'a,
        Result<atman_runtime::provider::AssistantMessage, atman_runtime::RuntimeError>,
    > {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.call(request)
    }

    fn call_streaming(
        &self,
        request: atman_runtime::provider::LlmRequest,
    ) -> atman_runtime::event::Observable<atman_runtime::provider::AssistantMessage> {
        self.requests.lock().unwrap().push(request.clone());
        self.inner.call_streaming(request)
    }
}

#[tokio::test]
async fn instance_inbox_enters_only_shared_general_calls_and_survives_recovery_without_reconsumption()
 {
    use atman_runtime::flow_runtime::FlowWorker;
    use atman_runtime::message::MessageOrigin;
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut description = descriptor(info.id);
    description.entry.bundle.sources.insert("entry:main.at".into(), r#"
        flow shared() {
            llm.call(model: "mock", prompt: "shared", context: "session", record: false)
        }
        flow branch() {
            llm.call(model: "mock", prompt: "branch", context: "session", record: false)
        }
        flow main(input: int) -> string {
            execution_probe()
            llm.extract(model: "mock", prompt: "extract", context: "session", fields: { value: string -- "value" })
            fanout [branch()]
            llm.call(model: "mock", prompt: "isolated", context: "none", record: false)
            shared().await
            shared().await
            return final.answer(message: "finished")
        }
    "#.into());
    let mut store = session.create_flow(description.clone()).unwrap();
    change(&mut store, FlowTransition::Activated);
    let worker = FlowWorker::start(store).await.unwrap();
    let first = worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::new_v4()),
                "entry",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let executor = atman_runtime::Executor::new();
    let provider = std::sync::Arc::new(InboxRecordingProvider {
        requests: Default::default(),
        inner: atman_runtime::providers::mock::MockProvider::new("mock")
            .with_fallback(atman_runtime::Value::Str(r#"{"value":"ok"}"#.into())),
    });
    executor.providers.register(provider.clone());
    let probe = std::sync::Arc::new(ExecutionProbe {
        entered: std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
        release: std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
    });
    executor.tools.register(probe.clone());
    let ticket = worker
        .start_turn(executor, turn_request(first.message_id, 0))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), probe.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let key = Uuid::new_v4().into();
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::ExecutionPaused { key },
        )
        .await
        .unwrap();
    let mut accepted = Vec::new();
    let mut pending = Vec::new();
    for (text, mode, marker) in [
        ("boundary-one", DeliveryMode::AtBoundary, None),
        ("boundary-two", DeliveryMode::AtBoundary, Some("recorded")),
        ("next-turn-only", DeliveryMode::NextTurn, None),
        (
            "different-config",
            DeliveryMode::AtBoundary,
            Some("different"),
        ),
    ] {
        let mut request = delivery(
            &description.address,
            SenderIdentity::User(Uuid::new_v4()),
            text,
        );
        request.delivery = mode;
        if let Some(marker) = marker {
            request.payload = InboxPayload::UserSubmission {
                text: text.into(),
                images: vec![],
                invocation: [("marker".into(), serde_json::json!(marker))].into(),
                presentation: None,
            };
        }
        let receipt = worker.accept_input(request, |_| Ok(())).await.unwrap();
        if text.starts_with("boundary") {
            accepted.push(receipt.message_id);
        } else {
            pending.push(receipt.message_id);
        }
    }
    let peer = descriptor(info.id).address;
    let peer_receipt = worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::Flow(peer),
                "peer-not-user",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    accepted.push(peer_receipt.message_id);
    probe.release.add_permits(1);
    assert!(provider.requests.lock().unwrap().is_empty());
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::ExecutionResumed { key },
        )
        .await
        .unwrap();
    let done = tokio::time::timeout(std::time::Duration::from_secs(15), ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(done.result.is_ok(), "{:?}", done.result);
    let requests = provider.requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 5);
    for (index, request) in requests.iter().enumerate() {
        for text in ["boundary-one", "boundary-two", "peer-not-user"] {
            let messages: Vec<_> = request
                .messages
                .iter()
                .filter(|m| m.text_concat().contains(text))
                .collect();
            assert_eq!(
                messages.len(),
                usize::from(index >= 3),
                "call {index}: {text}"
            );
            if text == "peer-not-user" && index >= 3 {
                assert_ne!(messages[0].origin, MessageOrigin::User);
                assert!(messages[0].text_concat().contains("not user authorization"));
            }
        }
        assert!(
            !request
                .messages
                .iter()
                .any(|m| m.text_concat().contains("next-turn-only")
                    || m.text_concat().contains("different-config"))
        );
    }
    let messages = worker.context().messages().to_vec();
    let store = worker.shutdown().await.unwrap();
    assert_eq!(
        store.pending_inputs().map(|e| e.id).collect::<Vec<_>>(),
        pending
    );
    let inputs = store.committed_inputs();
    assert_eq!(inputs.len(), 2);
    assert_eq!(inputs[1].input.message_ids, accepted);
    assert_eq!(store.context_messages(), messages);
    drop(store);
    let restored = session.open_flow(&description.address.instance_id).unwrap();
    assert_eq!(restored.context_messages(), messages);
    assert_eq!(
        restored.pending_inputs().map(|e| e.id).collect::<Vec<_>>(),
        pending
    );
}

impl atman_runtime::tool::Tool for ExecutionProbe {
    fn name(&self) -> &str {
        "execution_probe"
    }
    fn tier(&self) -> atman_runtime::tool::Tier {
        atman_runtime::tool::Tier::Zero
    }
    fn call<'a>(
        &'a self,
        _args: atman_runtime::tool::ToolArgs,
        _ctx: &'a atman_runtime::tool::ToolCtx,
    ) -> atman_runtime::tool::BoxFut<'a, atman_runtime::tool::ToolResult> {
        Box::pin(async move {
            self.entered.add_permits(1);
            self.release.acquire().await.unwrap().forget();
            Ok(atman_runtime::Value::Unit)
        })
    }
}

#[tokio::test]
async fn owned_execution_commits_admission_and_completion_even_when_the_observer_leaves() {
    use atman_runtime::flow_runtime::FlowWorker;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut description = descriptor(info.id);
    description.entry.bundle.sources.insert(
        "entry:main.at".into(),
        r#"flow main(input: int) -> string {
        when env("marker") != "recorded" { return "invalid environment" }
        execution_probe()
        return final.answer(message: to_json_string(input * 2))
    }"#
        .into(),
    );
    let mut store = session.create_flow(description.clone()).unwrap();
    change(&mut store, FlowTransition::Activated);
    let worker = FlowWorker::start(store).await.unwrap();
    let first = worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::new_v4()),
                "first",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let executor = atman_runtime::Executor::new();
    let probe = std::sync::Arc::new(ExecutionProbe {
        entered: std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
        release: std::sync::Arc::new(tokio::sync::Semaphore::new(0)),
    });
    executor.tools.register(probe.clone());
    let ticket = worker
        .start_turn(executor.clone(), turn_request(first.message_id, 21))
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), probe.entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let duplicate = atman_runtime::InstanceInvocation::new(
        ticket.lease.clone(),
        atman_runtime::flow_authority::EffectiveAuthority::root(&Default::default(), false, None),
        Default::default(),
    )
    .with_storage(worker.clone())
    .unwrap();
    let duplicate_entry = atman_rt::parse_file("flow main() { execution_probe() }").unwrap();
    assert!(
        executor
            .run_with_invocation(
                &duplicate_entry,
                "main",
                vec![],
                atman_runtime::RootInvocation {
                    instance: Some(duplicate),
                    ..Default::default()
                }
            )
            .await
            .is_err()
    );
    assert!(
        worker
            .start_turn(executor.clone(), turn_request(first.message_id, 9))
            .await
            .is_err()
    );
    let pause_a = Uuid::new_v4().into();
    let pause_b = Uuid::new_v4().into();
    let mut state = worker.subscribe_state();
    for key in [pause_a, pause_b] {
        worker
            .transition(
                worker.state().await.unwrap().revision(),
                FlowTransition::ExecutionPaused { key },
            )
            .await
            .unwrap();
    }
    assert_eq!(state.borrow().pauses().len(), 2);
    probe.release.add_permits(1);
    let second = worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::new_v4()),
                "second",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::ExecutionResumed { key: pause_a },
        )
        .await
        .unwrap();
    for _ in 0..30 {
        tokio::task::yield_now().await;
    }
    assert!(
        !worker
            .context()
            .messages()
            .iter()
            .any(|message| message.origin == atman_runtime::message::MessageOrigin::FinalAnswer)
    );
    drop(ticket);
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::ExecutionResumed { key: pause_b },
        )
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if state.borrow_and_update().can_start_turn() {
                break;
            }
            state.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(
        worker.context().messages().last().unwrap().text_concat(),
        "42"
    );
    let ticket = worker
        .start_turn(executor, turn_request(second.message_id, 8))
        .await
        .unwrap();
    probe.release.add_permits(1);
    let end = tokio::time::timeout(std::time::Duration::from_secs(5), ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(end.outcome, TurnOutcome::Completed);
    assert!(matches!(end.result, Ok(atman_runtime::Value::Str(text)) if text == "16"));
    assert!(worker.state().await.unwrap().can_start_turn());
    let store = worker.shutdown().await.unwrap();
    assert_eq!(store.committed_inputs().len(), 2);
    assert!(store.pending_inputs().next().is_none());
    assert_eq!(store.cursor(), &end.receipt);
    drop(store);
    let store = session.open_flow(&description.address.instance_id).unwrap();
    assert!(store.state().can_start_turn());
    assert_eq!(store.context_messages().last().unwrap().text_concat(), "16");
}

#[tokio::test]
async fn cooperative_instance_execution_can_be_cancelled_while_paused_without_replaying_input() {
    use atman_runtime::flow_runtime::FlowWorker;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut description = descriptor(info.id);
    description.entry.bundle.sources.insert(
        "entry:main.at".into(),
        "flow main(input: int) { loop { value = input + 1 } }".into(),
    );
    let mut store = session.create_flow(description.clone()).unwrap();
    change(&mut store, FlowTransition::Activated);
    let worker = FlowWorker::start(store).await.unwrap();
    let accepted = worker
        .accept_input(
            delivery(
                &description.address,
                SenderIdentity::User(Uuid::new_v4()),
                "loop",
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let ticket = worker
        .start_turn(
            atman_runtime::Executor::new(),
            turn_request(accepted.message_id, 1),
        )
        .await
        .unwrap();
    let lease = ticket.lease.clone();
    let key = Uuid::new_v4().into();
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::ExecutionPaused { key },
        )
        .await
        .unwrap();
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::PhaseChanged {
                lease: lease.clone(),
                phase: TurnPhase::Cancelling,
            },
        )
        .await
        .unwrap();
    let end = tokio::time::timeout(std::time::Duration::from_secs(5), ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        end.outcome,
        TurnOutcome::Interrupted(Interruption::Cancelled)
    );
    assert!(end.result.is_err());
    let store = worker.shutdown().await.unwrap();
    assert_eq!(store.state().pauses(), &[key]);
    assert_eq!(store.committed_inputs().len(), 1);
    assert!(store.pending_inputs().next().is_none());
    drop(store);
    let store = session.open_flow(&description.address.instance_id).unwrap();
    assert!(matches!(
        store.state().state(),
        InstanceState::Open(InstanceActivity::Interrupted {
            reason: Interruption::Cancelled,
            ..
        })
    ));
    assert_eq!(store.state().pauses(), &[key]);
}

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
fn publications_preserve_input_order_and_fence_stale_delivery_across_recovery() {
    use atman_runtime::flow_store::publication::MessagePublication;
    use atman_runtime::message::MessageOrigin;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let lease = start(&mut flow);
    let first = MessagePublication {
        id: Uuid::new_v4(),
        lease: lease.clone(),
        run: RunId::now(),
        context_run: None,
        message: Message::assistant_text(lease.turn.clone(), "working"),
    };
    let first_record = flow.publish_message(first.clone()).unwrap();
    flow.accept_input(
        delivery(
            &description.address,
            SenderIdentity::User(Uuid::new_v4()),
            "detail",
        ),
        |_| Ok(()),
    )
    .unwrap();
    let reserved = flow
        .reserve_context_input(&lease, |_| true)
        .unwrap()
        .unwrap();
    let input = Message::user_text(lease.turn.clone(), "detail");
    flow.commit_input(
        &reserved,
        InputContent::Context {
            messages: vec![input.clone()],
        },
    )
    .unwrap();
    change(
        &mut flow,
        FlowTransition::ExecutionPaused {
            key: Uuid::now_v7().into(),
        },
    );
    change(
        &mut flow,
        FlowTransition::PhaseChanged {
            lease: lease.clone(),
            phase: TurnPhase::Finishing,
        },
    );
    let mut answer = first.clone();
    answer.id = Uuid::new_v4();
    answer.message = Message::assistant_text(lease.turn.clone(), "done");
    answer.message.origin = MessageOrigin::FinalAnswer;
    flow.publish_message(answer.clone()).unwrap();
    let expected = vec![first.message.clone(), input, answer.message.clone()];
    assert_eq!(flow.context_messages(), expected);
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease: lease.clone(),
            outcome: TurnOutcome::Completed,
        },
    );
    let cursor = flow.cursor().clone();
    assert_eq!(flow.publish_message(first.clone()).unwrap(), first_record);
    assert_eq!(flow.cursor(), &cursor);
    let mut stale = first.clone();
    stale.id = Uuid::new_v4();
    assert!(flow.publish_message(stale.clone()).is_err());
    let mut conflict = first.clone();
    conflict.message.parts.clear();
    assert!(flow.publish_message(conflict).is_err());
    drop(flow);
    let mut flow = session.open_flow(&description.address.instance_id).unwrap();
    assert_eq!(flow.context_messages(), expected);
    assert_eq!(flow.publish_message(first).unwrap(), first_record);
    let key = *flow.state().pauses().first().unwrap();
    change(&mut flow, FlowTransition::ExecutionResumed { key });
    let current = start(&mut flow);
    assert!(flow.publish_message(stale.clone()).is_err());
    stale.lease = current.clone();
    assert!(flow.publish_message(stale.clone()).is_err());
    stale.message.turn_id = current.turn.clone();
    change(
        &mut flow,
        FlowTransition::PhaseChanged {
            lease: current,
            phase: TurnPhase::Cancelling,
        },
    );
    assert!(flow.publish_message(stale).is_err());
    assert_eq!(flow.context_messages(), expected);
}

#[tokio::test]
async fn owned_execution_separates_pre_admission_errors_from_interrupted_turns() {
    use atman_runtime::flow_runtime::FlowWorker;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    for (source, admitted) in [
        ("flow main(input: int) { missing_tool() }", true),
        ("flow main(", false),
    ] {
        let mut description = descriptor(info.id);
        description
            .entry
            .bundle
            .sources
            .insert("entry:main.at".into(), source.into());
        let mut store = session.create_flow(description.clone()).unwrap();
        change(&mut store, FlowTransition::Activated);
        let worker = FlowWorker::start(store).await.unwrap();
        let input = worker
            .accept_input(
                delivery(
                    &description.address,
                    SenderIdentity::User(Uuid::new_v4()),
                    "run",
                ),
                |_| Ok(()),
            )
            .await
            .unwrap();
        let launched = worker
            .start_turn(
                atman_runtime::Executor::new(),
                turn_request(input.message_id, 1),
            )
            .await;
        if admitted {
            let end = launched.unwrap().wait().await.unwrap();
            assert!(end.result.is_err());
            assert_eq!(end.outcome, TurnOutcome::Interrupted(Interruption::Failed));
        } else {
            assert!(launched.is_err());
        }
        let store = worker.shutdown().await.unwrap();
        assert_eq!(store.committed_inputs().len(), usize::from(admitted));
        assert_eq!(store.pending_inputs().count(), usize::from(!admitted));
        assert_eq!(store.state().can_start_turn(), !admitted);
    }
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

#[test]
fn cross_session_delivery_reconciles_lost_ack_after_restart_and_closure() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let a_info = session_descriptor();
    let b_info = session_descriptor();
    let mut a_session = root.create_session(a_info.clone()).unwrap();
    let mut b_session = root.create_session(b_info.clone()).unwrap();
    let mut a = a_session.create_flow(descriptor(a_info.id)).unwrap();
    let mut b = b_session.create_flow(descriptor(b_info.id)).unwrap();
    change(&mut a, FlowTransition::Activated);
    change(&mut b, FlowTransition::Activated);
    let lease = start(&mut a);
    let request = delivery(
        &b.descriptor().address,
        SenderIdentity::Flow(a.descriptor().address.clone()),
        "cross-session task",
    );
    let queued = a
        .queue_send(&lease, request.clone(), None, |_| Ok(()))
        .unwrap();
    assert_eq!(queued.status, SendStatus::PendingDelivery);
    let accepted = b.accept_input(request.clone(), |_| Ok(())).unwrap();
    let a_id = a.descriptor().address.instance_id.clone();
    let b_id = b.descriptor().address.instance_id.clone();
    drop((a, b));
    let mut a = a_session.open_flow(&a_id).unwrap();
    let mut b = b_session.open_flow(&b_id).unwrap();
    assert_eq!(a.pending_sends().next().unwrap(), &queued);
    assert_eq!(
        a.queue_send(&lease, request.clone(), None, |_| panic!("send retry"))
            .unwrap(),
        queued
    );
    change(&mut a, FlowTransition::CloseRequested);
    change(&mut b, FlowTransition::CloseRequested);
    let replayed = b
        .accept_input(request.clone(), |_| {
            panic!("already accepted before revocation")
        })
        .unwrap();
    assert_eq!(replayed.message_id, accepted.message_id);
    assert_eq!(replayed.receipt, accepted.receipt);
    assert!(matches!(
        replayed.disposition,
        InboxDisposition::Cancelled { .. }
    ));
    let ack = DeliveryReceipt::from(&replayed);
    let mut foreign = ack.clone();
    foreign.receipt.journal_id = Uuid::new_v4();
    let before = a.cursor().clone();
    assert!(a.confirm_delivery(request.request_id, foreign).is_err());
    assert_eq!(a.cursor(), &before);
    let confirmed = a.confirm_delivery(request.request_id, ack.clone()).unwrap();
    assert!(a.pending_sends().next().is_none());
    assert_eq!(
        a.confirm_delivery(request.request_id, ack.clone()).unwrap(),
        confirmed
    );
    let mut conflicting = ack;
    conflicting.message_id = Uuid::new_v4();
    assert!(a.confirm_delivery(request.request_id, conflicting).is_err());
    drop(a);
    let mut a = a_session.open_flow(&a_id).unwrap();
    assert_eq!(a.sends(), std::slice::from_ref(&confirmed));
    assert_eq!(
        a.queue_send(&lease, request.clone(), None, |_| panic!("closed retry"))
            .unwrap(),
        confirmed
    );
    let mut changed = request.clone();
    changed.payload = InboxPayload::Interjection {
        text: "different task".into(),
    };
    assert!(a.queue_send(&lease, changed, None, |_| Ok(())).is_err());
    let mut new = request;
    new.request_id = Uuid::new_v4();
    assert!(a.queue_send(&lease, new, None, |_| Ok(())).is_err());
    assert_eq!(b.inbox_entries().len(), 1);
}

#[test]
fn request_registration_and_send_share_one_commit_and_peers_can_reply_in_the_same_turn() {
    let (_dir, mut session, mut a, initial) = waiting_flow();
    let mut b = session
        .create_flow(descriptor(a.descriptor().address.session_id.0))
        .unwrap();
    change(&mut b, FlowTransition::Activated);
    let waiter = ReplyWaiter {
        correlation: Uuid::new_v4(),
        lease: initial.lease.clone(),
        sender: SenderIdentity::Flow(b.descriptor().address.clone()),
    };
    let mut request = delivery(
        &b.descriptor().address,
        SenderIdentity::Flow(a.descriptor().address.clone()),
        "request",
    );
    request.correlation = Some(waiter.correlation);
    let before = a.cursor().clone();
    assert!(
        a.queue_send(&waiter.lease, request.clone(), Some(waiter.clone()), |_| {
            Err(std::io::Error::other("denied"))
        })
        .is_err()
    );
    assert_eq!(a.cursor(), &before);
    assert_eq!(a.reply_waiters().len(), 1);
    assert!(a.sends().is_empty());
    let queued = a
        .queue_send(&waiter.lease, request.clone(), Some(waiter.clone()), |_| {
            Ok(())
        })
        .unwrap();
    assert_eq!(queued.queued.sequence, before.sequence + 1);
    let registered = a
        .reply_waiters()
        .iter()
        .find(|record| record.waiter == waiter)
        .unwrap();
    assert_eq!(registered.registered, queued.queued);
    let accepted = b.accept_input(request.clone(), |_| Ok(())).unwrap();
    a.confirm_delivery(request.request_id, DeliveryReceipt::from(&accepted))
        .unwrap();
    wait_phase(&mut a, &waiter);
    let b_lease = start(&mut b);
    let mut reply = delivery(
        &a.descriptor().address,
        SenderIdentity::Flow(b.descriptor().address.clone()),
        "result",
    );
    reply.correlation = Some(waiter.correlation);
    b.queue_send(&b_lease, reply.clone(), None, |_| Ok(()))
        .unwrap();
    let accepted_reply = a.accept_input(reply.clone(), |_| Ok(())).unwrap();
    b.confirm_delivery(reply.request_id, DeliveryReceipt::from(&accepted_reply))
        .unwrap();
    let batch = a
        .reserve_reply_input(&waiter.lease, waiter.correlation)
        .unwrap()
        .unwrap();
    assert_eq!(batch.message_ids(), &[accepted_reply.message_id]);
    a.commit_input(&batch, reply_content(&waiter)).unwrap();
    assert_eq!(a.committed_inputs()[0].input.lease, waiter.lease);
    assert!(a.pending_sends().next().is_none());
    assert!(b.pending_sends().next().is_none());
}

#[test]
fn torn_send_batch_cannot_leave_a_registered_wait_without_its_send_intent() {
    let (_dir, session, mut flow, first) = waiting_flow();
    let mut waiter = first.clone();
    waiter.correlation = Uuid::new_v4();
    let SenderIdentity::Flow(target) = &waiter.sender else {
        unreachable!()
    };
    let mut request = delivery(
        target,
        SenderIdentity::Flow(flow.descriptor().address.clone()),
        "request",
    );
    request.correlation = Some(waiter.correlation);
    let path = flow.dir().join("events.jsonl");
    flow.queue_send(&waiter.lease, request, Some(waiter.clone()), |_| Ok(()))
        .unwrap();
    let id = flow.descriptor().address.instance_id.clone();
    drop(flow);
    let length = std::fs::metadata(&path).unwrap().len();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(length - 1)
        .unwrap();
    let flow = session.open_flow(&id).unwrap();
    assert!(flow.recovered_tail.is_some());
    assert!(flow.sends().is_empty());
    assert!(
        !flow
            .reply_waiters()
            .iter()
            .any(|record| record.waiter.correlation == waiter.correlation)
    );
    assert!(matches!(
        flow.state().state(),
        InstanceState::Open(InstanceActivity::Interrupted { .. })
    ));
}

#[tokio::test]
async fn recorded_source_and_committed_input_execute_with_the_persistent_instance_binding() {
    use atman_runtime::ValueJson;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut description = descriptor(info.id);
    description.entry.bundle.sources.insert("entry:main.at".into(),
        "flow helper(input: int) -> int { return input * 2 } flow main(input: int) -> int { when env(\"marker\") != \"persisted\" { return 0 } return helper(input).await }".into());
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    flow.accept_input(
        delivery(
            &description.address,
            SenderIdentity::User(Uuid::new_v4()),
            "execute",
        ),
        |_| Ok(()),
    )
    .unwrap();
    let program = atman_runtime::source_program::load_program_from_bundle(
        &description.entry.bundle,
        &description.entry.path,
        &Default::default(),
    )
    .unwrap();
    let reservation = flow
        .reserve_invocation_input(TurnId::now(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let lease = reservation.lease().clone();
    flow.commit_input(
        &reservation,
        InputContent::InvocationInput {
            arguments: BTreeMap::from([("input".into(), serde_json::json!(21))]),
            invocation: BTreeMap::from([("marker".into(), serde_json::json!("persisted"))]),
            messages: Vec::new(),
        },
    )
    .unwrap();
    let InputContent::InvocationInput {
        arguments,
        invocation,
        ..
    } = &flow.committed_inputs()[0].input.content
    else {
        unreachable!()
    };
    let args = arguments
        .iter()
        .map(|(key, value)| (key.clone(), atman_runtime::Value::from_json(value.clone())))
        .collect();
    let env = atman_runtime::invocation_env::InvocationEnv::from_values(
        invocation
            .iter()
            .map(|(key, value)| (key.clone(), atman_runtime::Value::from_json(value.clone()))),
    );
    let mut authority =
        atman_runtime::flow_authority::EffectiveAuthority::root(&Default::default(), false, None);
    authority.permission_management = false;
    let context = std::sync::Arc::new(atman_runtime::Session::open_instance_context(
        description.address.clone(),
    ));
    let executor = atman_runtime::Executor::new();
    let result = executor
        .run_linked_with_invocation(
            &program,
            &description.entry.name,
            args,
            atman_runtime::RootInvocation {
                session: Some(context.clone()),
                env,
                instance: Some(atman_runtime::InstanceInvocation::new(
                    lease.clone(),
                    authority,
                    Default::default(),
                )),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(result, atman_runtime::Value::Int(42)));
    assert_eq!(
        context
            .flow_registry
            .lookup_run(&lease.run)
            .unwrap()
            .instance
            .as_ref(),
        Some(&description.address)
    );
    change(
        &mut flow,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    );
    drop(flow);
    let flow = session.open_flow(&description.address.instance_id).unwrap();
    assert!(flow.state().can_start_turn());
    assert_eq!(flow.committed_inputs().len(), 1);
    assert!(flow.pending_inputs().next().is_none());
}

#[tokio::test]
async fn instance_worker_persists_real_tool_delivery_and_restores_only_shared_context() {
    let _registry = common::ModelRegistryGuard::mock("mock").await;
    use atman_runtime::flow_runtime::FlowWorker;
    use atman_runtime::message::MessageOrigin;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let description = descriptor(info.id);
    let mut flow = session.create_flow(description.clone()).unwrap();
    change(&mut flow, FlowTransition::Activated);
    let lease = start(&mut flow);
    let worker = FlowWorker::start(flow).await.unwrap();
    let context = worker.context();
    let mut stream = context.stream_subscribe();
    let executor = atman_runtime::Executor::new();
    let file = atman_rt::parse_file(
        r#"
        flow inline() -> string {
            context.record(key: "agent.rule.example", content: "Keep records")
            return final.answer(message: "inline delivery")
        }
        flow branch() -> string { return final.answer(message: "isolated delivery") }
        flow main() -> string {
            inline().await
            fanout [branch()]
            return final.answer(message: "shared delivery")
        }
    "#,
    )
    .unwrap();
    let authority =
        atman_runtime::flow_authority::EffectiveAuthority::root(&Default::default(), false, None);
    let binding =
        atman_runtime::InstanceInvocation::new(lease.clone(), authority, Default::default())
            .with_storage(worker.clone())
            .unwrap();
    let result = executor
        .run_with_invocation(
            &file,
            "main",
            vec![],
            atman_runtime::RootInvocation {
                instance: Some(binding),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(matches!(result, atman_runtime::Value::Str(text) if text == "shared delivery"));
    let messages = context.messages().to_vec();
    assert!(
        messages
            .iter()
            .any(|message| message.contains_context_record())
    );
    let answers: Vec<_> = messages
        .iter()
        .filter(|message| message.origin == MessageOrigin::FinalAnswer)
        .map(Message::text_concat)
        .collect();
    assert_eq!(answers, ["inline delivery", "shared delivery"]);
    let mut streamed = Vec::new();
    while let Ok(frame) = stream.try_recv() {
        if let atman_runtime::stream::StreamFrame::AssistantMsg { message, .. } = frame {
            streamed.push(message.text_concat());
        }
    }
    assert_eq!(
        streamed,
        ["inline delivery", "isolated delivery", "shared delivery"]
    );
    let revision = worker.state().await.unwrap().revision();
    worker
        .transition(
            revision,
            FlowTransition::TurnFinished {
                lease,
                outcome: TurnOutcome::Completed,
            },
        )
        .await
        .unwrap();
    let flow = worker.shutdown().await.unwrap();
    assert_eq!(flow.published_messages().len(), 4);
    assert_eq!(flow.context_messages(), messages);
    assert_eq!(
        flow.published_messages()
            .iter()
            .filter(|record| record.publication.context_run.is_some())
            .count(),
        1
    );
    drop(flow);
    let flow = session.open_flow(&description.address.instance_id).unwrap();
    let worker = FlowWorker::start(flow).await.unwrap();
    assert_eq!(worker.context().messages().to_vec(), messages);
    assert!(worker.context().stream_subscribe().try_recv().is_err());
    assert!(worker.state().await.unwrap().can_start_turn());
    let state = worker.state().await.unwrap();
    let next = state
        .clone()
        .begin_turn(TurnId::now(), RunId::now())
        .unwrap();
    worker
        .transition(
            state.revision(),
            FlowTransition::TurnStarted {
                turn: next.turn.clone(),
                run: next.run.clone(),
            },
        )
        .await
        .unwrap();
    executor.providers.register(std::sync::Arc::new(
        atman_runtime::providers::mock::MockProvider::new("mock")
            .with_model("mock", atman_runtime::Value::Str("model record".into())),
    ));
    let file = atman_rt::parse_file(
        r#"flow main() -> string {
        llm.call(model: "mock", prompt: "continue", context: "session")
        session.push(message.user("explicit input"))
        return final.answer(message: "after restart")
    }"#,
    )
    .unwrap();
    let authority =
        atman_runtime::flow_authority::EffectiveAuthority::root(&Default::default(), false, None);
    executor
        .run_with_invocation(
            &file,
            "main",
            vec![],
            atman_runtime::RootInvocation {
                instance: Some(
                    atman_runtime::InstanceInvocation::new(next, authority, Default::default())
                        .with_storage(worker.clone())
                        .unwrap(),
                ),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    let restored = worker.context();
    assert!(restored.messages().len() > messages.len() + 1);
    assert_eq!(
        &restored.messages().to_vec()[..messages.len()],
        messages.as_slice()
    );
    assert_eq!(
        restored.messages().last().unwrap().text_concat(),
        "after restart"
    );
    assert!(
        restored
            .messages()
            .iter()
            .any(|message| message.text_concat() == "model record")
    );
    assert!(
        restored
            .messages()
            .iter()
            .any(|message| message.text_concat() == "explicit input")
    );
    let store = worker.shutdown().await.unwrap();
    assert_eq!(store.context_messages(), restored.messages().to_vec());
}
