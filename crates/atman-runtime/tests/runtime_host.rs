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

fn program_descriptor(session: Uuid, source: String) -> FlowDescriptor {
    let mut descriptor = flow_descriptor(session);
    descriptor
        .entry
        .bundle
        .sources
        .insert("entry:main.at".into(), source);
    descriptor
}

async fn invoke(
    worker: &FlowWorker,
    executor: Executor,
) -> atman_runtime::flow_runtime::execution::TurnCompletion {
    invoke_with_env(worker, executor, BTreeMap::new()).await
}

async fn invoke_with_env(
    worker: &FlowWorker,
    executor: Executor,
    invocation: BTreeMap<String, serde_json::Value>,
) -> atman_runtime::flow_runtime::execution::TurnCompletion {
    worker
        .context()
        .update_trust(
            atman_runtime::trust::TrustConfig {
                mode: atman_runtime::trust::TrustMode::Reckless,
                ..Default::default()
            },
            |_| Ok(()),
        )
        .unwrap();
    worker
        .accept_input(
            DeliveryRequest {
                request_id: Uuid::new_v4(),
                sender: SenderIdentity::User(Uuid::new_v4()),
                recipient: worker.address().clone(),
                delivery: DeliveryMode::NextTurn,
                cause: None,
                correlation: None,
                scope: None,
                payload: InboxPayload::Interjection {
                    text: "execute entry".into(),
                },
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    let mut request = prepare(&worker.snapshot().await.unwrap()).unwrap().unwrap();
    request.authority.shell = true;
    request.invocation = invocation;
    let ticket = worker.start_turn(executor, request).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), ticket.wait())
        .await
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn goal_tools_persist_for_peers_without_freezing_execution_or_inheriting_control() {
    use atman_runtime::ValueJson;
    use atman_runtime::flow_store::{
        access::{AccessTarget, FlowAction, FlowGrant},
        goal::{GoalRequest, GoalVersion},
    };
    use atman_runtime::memory::goal::{
        GoalActor, GoalEdit, GoalPauseReason, GoalStatus, GoalStore,
    };
    use atman_runtime::tools::memory::{MemoryGoalControl, MemoryGoalGet, MemoryGoalSet};
    let dir = tempfile::tempdir().unwrap();
    let child_path = dir.path().join("objective.at");
    std::fs::write(
        &child_path,
        r#"
        flow child() -> any {
            contract { invocation { goal: true } }
            goal = memory.goal.set(text: "child objective")
            memory.goal.pause(goal_id: goal.goal.id, expected_revision: goal.goal.revision,
                reason: "user_requested", summary: "hold objective")
            final.answer(message: "child delivered while goal paused")
            return memory.goal.get()
        }
    "#,
    )
    .unwrap();
    let reference = serde_json::to_string(&format!("{}@child", child_path.display())).unwrap();
    let source = format!(
        r#"
        flow main() -> any {{
            contract {{ invocation {{ goal: true }} capabilities {{ shell: true }} }}
            goal = memory.goal.set(text: "parent objective")
            memory.goal.pause(goal_id: goal.goal.id, expected_revision: goal.goal.revision,
                reason: "user_requested", summary: "hold objective")
            inventory = flow.instances()
            child = flow.spawn(flow: {reference}, spawn_token: inventory.spawn_token, async: false)
            final.answer(message: "parent delivered while goal paused")
            return [memory.goal.get(), child]
        }}
    "#
    );
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let parent = session
        .create_flow(program_descriptor(info.id, source))
        .await
        .unwrap();
    change(&parent, FlowTransition::Activated).await;
    let fallback = Arc::new(GoalStore::at(dir.path().join("unused-legacy")));
    let executor = Executor::new();
    executor.tools.register(Arc::new(MemoryGoalGet {
        store: fallback.clone(),
    }));
    executor.tools.register(Arc::new(MemoryGoalSet {
        store: fallback.clone(),
    }));
    executor
        .tools
        .register(Arc::new(MemoryGoalControl { resume: false }));
    executor
        .tools
        .register(Arc::new(MemoryGoalControl { resume: true }));
    let completed = invoke(&parent, executor).await;
    let result = completed.result.unwrap().to_json().unwrap();
    for result in result.as_array().unwrap() {
        assert_eq!(result["goal"]["status"], "paused");
        assert_eq!(result["flow"]["paused"], false);
    }
    assert_ne!(result[0]["goal"]["owner"], result[1]["goal"]["owner"]);
    assert!(
        !fallback.path().exists(),
        "instance edits must not write legacy goal files"
    );
    let catalog = session.snapshot().await.unwrap();
    let child_id = catalog
        .catalog
        .flows
        .iter()
        .find(|entry| entry.id != parent.address().instance_id)
        .unwrap()
        .id
        .clone();
    let child = session.open_flow(child_id).await.unwrap();
    let saved = child.goal().await.unwrap().goal.unwrap();
    assert_eq!(saved.status, GoalStatus::Paused);
    assert_eq!(child.context().goal_control.record(), Some(saved.clone()));
    assert!(child.context().goal_control.snapshot().paused_by.is_none());
    assert_eq!(child.context().goal(), Some(saved.objective.clone()));
    assert!(child.state().await.unwrap().can_start_turn());
    assert!(
        child
            .context()
            .goal_control
            .set("uncommitted", None)
            .is_err()
    );
    let address = child.address().clone();
    drop(child.shutdown().await.unwrap());
    let child = host.flow(&address).await.unwrap();
    assert_eq!(child.goal().await.unwrap().goal, Some(saved.clone()));
    assert_eq!(child.context().goal_control.record(), Some(saved.clone()));
    assert!(child.context().goal_control.snapshot().paused_by.is_none());
    assert_eq!(child.context().goal(), Some(saved.objective.clone()));
    let lease = start(&parent).await;
    let client = host.bind_invocation(lease.clone()).await.unwrap();
    assert!(
        client.read_goal(address.clone()).await.is_err(),
        "creation is not a management grant"
    );
    let grant = FlowGrant {
        id: Uuid::new_v4(),
        grantee: parent.address().clone(),
        target: AccessTarget::Flow(address.clone()),
        actions: [FlowAction::ManageGoal].into(),
    };
    session
        .grant_access(session.snapshot().await.unwrap().cursor, grant.clone())
        .await
        .unwrap();
    assert_eq!(
        client.read_goal(address.clone()).await.unwrap().goal,
        Some(saved.clone())
    );
    let edit = GoalEdit::Set {
        text: "edited by authorized peer".into(),
        replace_completed: false,
    };
    let request_id = Uuid::new_v4();
    let updated = client
        .edit_goal(
            address.clone(),
            request_id,
            Some(GoalVersion::from(&saved)),
            edit.clone(),
        )
        .await
        .unwrap();
    assert_eq!(updated.goal.as_ref().unwrap().status, GoalStatus::Paused);
    assert_eq!(
        client
            .edit_goal(
                address.clone(),
                request_id,
                Some(GoalVersion::from(&saved)),
                edit
            )
            .await
            .unwrap(),
        updated
    );
    let key = Uuid::new_v4().into();
    change(&child, FlowTransition::ExecutionPaused { key }).await;
    let execution = child.state().await.unwrap();
    client
        .edit_goal(
            address.clone(),
            Uuid::new_v4(),
            updated.goal.as_ref().map(GoalVersion::from),
            GoalEdit::Resume {
                actor: GoalActor::Agent,
            },
        )
        .await
        .unwrap();
    assert_eq!(child.state().await.unwrap(), execution);
    let user_pause = child
        .edit_goal(
            GoalRequest {
                id: Uuid::new_v4(),
                sender: SenderIdentity::User(Uuid::new_v4()),
                expected: child
                    .goal()
                    .await
                    .unwrap()
                    .goal
                    .as_ref()
                    .map(GoalVersion::from),
                edit: GoalEdit::Pause {
                    actor: GoalActor::User,
                    reason: GoalPauseReason::UserRequested,
                    summary: "user pause".into(),
                },
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    for actor in [GoalActor::Agent, GoalActor::User] {
        assert!(
            client
                .edit_goal(
                    address.clone(),
                    Uuid::new_v4(),
                    user_pause.goal.as_ref().map(GoalVersion::from),
                    GoalEdit::Resume { actor }
                )
                .await
                .is_err()
        );
    }
    session
        .revoke_access(session.snapshot().await.unwrap().cursor, grant.id)
        .await
        .unwrap();
    assert!(client.read_goal(address.clone()).await.is_err());
    change(
        &parent,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    )
    .await;
    assert_eq!(child.state().await.unwrap(), execution);
}

#[tokio::test]
async fn entry_results_survive_worker_reopen_and_are_read_through_the_shared_tool() {
    use atman_runtime::{
        ValueJson,
        flow_store::completion::{InvocationResult, StoredValue},
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let worker = session
        .create_flow(program_descriptor(
            info.id,
            "flow main() -> any { return { answer: 42, values: [true, -2.1] } }".into(),
        ))
        .await
        .unwrap();
    change(&worker, FlowTransition::Activated).await;
    let completed = invoke(&worker, Executor::new()).await;
    let record = worker
        .completion(Some(completed.lease.turn.clone()))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(record.receipt, completed.receipt);
    assert_eq!(
        record.completion.result,
        InvocationResult::Returned(StoredValue::capture(completed.result.as_ref().unwrap()))
    );
    assert!(
        worker
            .context()
            .messages()
            .iter()
            .all(|m| m.origin != MessageOrigin::FinalAnswer)
    );
    let address = worker.address().clone();
    drop(worker.shutdown().await.unwrap());
    let reopened = host.flow(&address).await.unwrap();
    assert_eq!(reopened.completion(None).await.unwrap().unwrap(), record);
    let caller = session
        .create_flow(program_descriptor(
            info.id,
            format!(
                r#"
        flow main() -> any {{
            exact = flow.result(handle: "{}", turn: "{}")
            latest = flow.result(handle: "{}")
            missing = flow.result(handle: "{}", turn: "{}")
            output = flow.output(handle: "{}")
            return [exact, latest, missing, output]
        }}
    "#,
                address.instance_id,
                completed.lease.turn,
                address.instance_id,
                address.instance_id,
                Uuid::new_v4(),
                address.instance_id
            ),
        ))
        .await
        .unwrap();
    change(&caller, FlowTransition::Activated).await;
    let queried = invoke(&caller, Executor::new())
        .await
        .result
        .unwrap()
        .to_json()
        .unwrap();
    assert_eq!(
        queried[0]["completion"],
        serde_json::to_value(&record).unwrap()
    );
    assert_eq!(queried[0], queried[1]);
    assert!(queried[2]["completion"].is_null());
    assert_eq!(queried[3]["chunk"], "");
}

#[tokio::test]
async fn persistent_spawn_runs_real_entries_and_retains_sync_and_async_instances() {
    use atman_runtime::ValueJson;
    let dir = tempfile::tempdir().unwrap();
    let child_path = dir.path().join("child.at");
    std::fs::write(
        &child_path,
        r#"
        flow child(message: string, count: int = 2) -> string {
            contract { invocation { user_message: message } }
            when count != 2 { return final.answer(message: "wrong default") }
            when env("effort") != "high" { return final.answer(message: "wrong effort") }
            return final.answer(message: message)
        }
        flow value(n: int) -> int { return n * 2 }
    "#,
    )
    .unwrap();
    let reference = serde_json::to_string(&format!("{}@child", child_path.display())).unwrap();
    let raw = serde_json::to_string(&format!("{}@value", child_path.display())).unwrap();
    let source = format!(
        r#"
        flow main() -> any {{
            contract {{ capabilities {{ shell: true }} }}
            first = flow.instances()
            raw = flow.spawn(flow: {raw}, spawn_token: first.spawn_token, arguments: {{n: 21}}, async: false)
            second = flow.instances()
            sync = flow.spawn(flow: {reference}, spawn_token: second.spawn_token, arguments: {{message: "sync answer"}}, async: false, inherit_context: true)
            third = flow.instances()
            background = flow.spawn(flow: {reference}, spawn_token: third.spawn_token, arguments: {{message: "async answer"}})
            return [raw, sync, background]
        }}
    "#
    );
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let a = session
        .create_flow(program_descriptor(info.id, source))
        .await
        .unwrap();
    change(&a, FlowTransition::Activated).await;
    let result = invoke_with_env(
        &a,
        Executor::new(),
        [("effort".into(), "high".into())].into(),
    )
    .await
    .result
    .unwrap()
    .to_json()
    .unwrap();
    assert_eq!(result[0], 42);
    assert_eq!(result[1], "sync answer");
    let background: FlowRef = serde_json::from_value(result[2]["address"].clone()).unwrap();
    assert_eq!(background.session_id.0, info.id);
    wait_inputs(&a, 1).await;
    let inventory = session.snapshot().await.unwrap();
    assert_eq!(inventory.catalog.flows.len(), 4);
    let mut synchronous = None;
    for entry in inventory.catalog.flows {
        if entry.id == a.address().instance_id {
            continue;
        }
        let worker = session.open_flow(entry.id).await.unwrap();
        let record = worker.launch_record().await.unwrap().unwrap();
        assert_eq!(
            record.input.request.sender,
            SenderIdentity::Flow(a.address().clone())
        );
        assert!(
            worker
                .context()
                .messages()
                .iter()
                .all(|message| !message.text_concat().contains("\"effort\""))
        );
        if record
            .input
            .inherited_context
            .iter()
            .any(|message| message.text_concat() == "execute entry")
        {
            synchronous = Some(worker);
        }
    }
    let child = synchronous.expect("inherited context identifies the synchronous child");
    assert_eq!(
        child.state().await.unwrap().state(),
        &InstanceState::Open(InstanceActivity::Idle)
    );
    let pause = Uuid::new_v4().into();
    change(&child, FlowTransition::ExecutionPaused { key: pause }).await;
    std::fs::remove_file(child_path).unwrap();
    host.deliver(peer_request(a.address(), child.address()), |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(child.snapshot().await.unwrap().pending.len(), 1);
    change(&child, FlowTransition::ExecutionResumed { key: pause }).await;
    wait_inputs(&a, 2).await;
    let pending = a.snapshot().await.unwrap().pending;
    let contents: Vec<_> = pending
        .iter()
        .map(|message| serde_json::to_string(&message.request.payload).unwrap())
        .collect();
    assert!(contents.iter().any(|text| text.contains("async answer")));
    assert!(contents.iter().any(|text| text.contains("peer work")));
    assert_eq!(contents.len(), 2);
    assert!(
        contents
            .iter()
            .all(|text| !text.contains("\"text\":\"sync answer\""))
    );
    assert!(
        !a.context()
            .messages()
            .iter()
            .any(|message| message.origin == MessageOrigin::FinalAnswer)
    );
}

#[tokio::test]
async fn persistent_spawn_rejects_catalog_changes_after_inspection_before_creating() {
    use atman_runtime::{
        Value,
        tool::{Tier, ToolDefinition},
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("child.at");
    std::fs::write(&path, "flow child() -> int { return 1 }").unwrap();
    let target = serde_json::to_string(&format!("{}@child", path.display())).unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let a = session
        .create_flow(program_descriptor(
            info.id,
            format!(
                r#"
        flow main() -> any {{
            contract {{ capabilities {{ shell: true }} }}
            inventory = flow.instances()
            change_catalog()
            return flow.spawn(flow: {target}, spawn_token: inventory.spawn_token)
        }}
    "#
            ),
        ))
        .await
        .unwrap();
    change(&a, FlowTransition::Activated).await;
    let executor = Executor::new();
    let changed = session.clone();
    executor
        .tools
        .register_fn(
            ToolDefinition::new("change_catalog", Tier::Zero),
            move |_, _| {
                let changed = changed.clone();
                async move {
                    changed.create_flow(flow_descriptor(info.id)).await.unwrap();
                    Ok(Value::Unit)
                }
            },
        )
        .unwrap();
    let completion = invoke(&a, executor).await;
    assert!(
        completion
            .result
            .unwrap_err()
            .to_string()
            .contains("inventory changed")
    );
    assert_eq!(session.snapshot().await.unwrap().catalog.flows.len(), 2);
}

#[tokio::test]
async fn cancelling_a_sync_caller_keeps_child_work_and_delivers_the_reply_after_recovery() {
    use atman_rt::TurnPhase;
    use atman_runtime::{
        Value, ValueJson,
        flow_store::waiter::ReplyStatus,
        tool::{Tier, ToolDefinition},
    };
    let dir = tempfile::tempdir().unwrap();
    let leaf_path = dir.path().join("leaf.at");
    std::fs::write(
        &leaf_path,
        "flow leaf() -> int { await_release() return 42 }",
    )
    .unwrap();
    let leaf_ref = serde_json::to_string(&format!("{}@leaf", leaf_path.display())).unwrap();
    let caller_path = dir.path().join("caller.at");
    std::fs::write(&caller_path, format!(r#"
        flow caller(message: string) -> any {{
            contract {{ invocation {{ user_message: message }} capabilities {{ shell: true }} }}
            when message == "start" {{
                inventory = flow.instances()
                return flow.spawn(flow: {leaf_ref}, spawn_token: inventory.spawn_token, async: false)
            }}
            return final.answer(message: message)
        }}
    "#)).unwrap();
    let caller_ref = serde_json::to_string(&format!("{}@caller", caller_path.display())).unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let launcher = session.create_flow(program_descriptor(info.id, format!(r#"
        flow main() -> any {{
            contract {{ capabilities {{ shell: true }} }}
            inventory = flow.instances()
            return flow.spawn(flow: {caller_ref}, spawn_token: inventory.spawn_token, arguments: {{message: "start"}})
        }}
    "#))).await.unwrap();
    change(&launcher, FlowTransition::Activated).await;
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let executor = Executor::new();
    let (gate_entered, gate_release) = (entered.clone(), release.clone());
    executor
        .tools
        .register_fn(
            ToolDefinition::new("await_release", Tier::Zero),
            move |_, _| {
                let (entered, release) = (gate_entered.clone(), gate_release.clone());
                async move {
                    entered.add_permits(1);
                    release.acquire().await.unwrap().forget();
                    Ok(Value::Unit)
                }
            },
        )
        .unwrap();
    let value = invoke(&launcher, executor)
        .await
        .result
        .unwrap()
        .to_json()
        .unwrap();
    let address: FlowRef = serde_json::from_value(value["address"].clone()).unwrap();
    let caller = host.flow(&address).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let state = caller.state().await.unwrap();
    let InstanceState::Open(InstanceActivity::Active(active)) = state.state() else {
        panic!("caller is waiting")
    };
    change(
        &caller,
        FlowTransition::PhaseChanged {
            lease: active.lease.clone(),
            phase: TurnPhase::Cancelling,
        },
    )
    .await;
    let mut changes = caller.subscribe_commits();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            changes.borrow_and_update();
            if caller.completion(None).await.unwrap().is_some() {
                break;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    release.add_permits(1);
    let pending = wait_inputs(&caller, 1).await;
    assert!(matches!(
        pending.state.state(),
        InstanceState::Open(InstanceActivity::Interrupted { .. })
    ));
    let InboxPayload::FlowEvent {
        source, content, ..
    } = &pending.pending[0].request.payload
    else {
        panic!("completion remains recoverable")
    };
    assert_eq!(content["completion"]["result"]["value"]["value"], 42);
    let leaf = host.flow(&source.flow).await.unwrap();
    assert_eq!(
        leaf.state().await.unwrap().state(),
        &InstanceState::Open(InstanceActivity::Idle)
    );
    change(&caller, FlowTransition::InterruptionRecovered).await;
    settled(&caller).await;
    wait_inputs(&launcher, 1).await;
    let message = caller
        .context()
        .messages()
        .iter()
        .find(|message| message.origin == MessageOrigin::FinalAnswer)
        .unwrap()
        .text_concat();
    assert!(message.contains("turn_completed"));
    let store = caller.shutdown().await.unwrap();
    assert_eq!(store.completions().len(), 2);
    assert_eq!(store.inbox_entries().len(), 2);
    assert!(store.pending_inputs().next().is_none());
    assert!(store.inbox_entries().iter().all(|record| matches!(
        record.acceptance.disposition,
        InboxDisposition::InputCommitted { .. }
    )));
    assert!(matches!(
        store.reply_waiters()[0].status,
        ReplyStatus::Cancelled { .. }
    ));
    assert_eq!(leaf.shutdown().await.unwrap().completions().len(), 1);
}

#[tokio::test]
async fn persistent_spawned_instances_can_spawn_through_the_same_tool_chain() {
    use atman_runtime::ValueJson;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("delegated.at");
    let leaf = serde_json::to_string(&format!("{}@leaf", path.display())).unwrap();
    std::fs::write(&path, format!(r#"
        flow leaf(n: int) -> int {{ return n + 1 }}
        flow middle(n: int) -> int {{
            contract {{ capabilities {{ shell: true }} }}
            inventory = flow.instances()
            return flow.spawn(flow: {leaf}, spawn_token: inventory.spawn_token, arguments: {{n: n}}, async: false)
        }}
    "#)).unwrap();
    let target = serde_json::to_string(&format!("{}@middle", path.display())).unwrap();
    let source = format!(
        r#"
        flow main() -> int {{
            contract {{ capabilities {{ shell: true }} }}
            inventory = flow.instances()
            return flow.spawn(flow: {target}, spawn_token: inventory.spawn_token, arguments: {{n: 41}}, async: false)
        }}
    "#
    );
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let a = session
        .create_flow(program_descriptor(info.id, source))
        .await
        .unwrap();
    change(&a, FlowTransition::Activated).await;
    assert_eq!(
        invoke(&a, Executor::new())
            .await
            .result
            .unwrap()
            .to_json()
            .unwrap(),
        42
    );
    assert_eq!(session.snapshot().await.unwrap().catalog.flows.len(), 3);
    assert!(a.snapshot().await.unwrap().pending.is_empty());
}

#[tokio::test]
async fn instance_discovery_uses_durable_membership_and_shared_status_through_executor() {
    use atman_runtime::ValueJson;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let prepared = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    let paused = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    change(&paused, FlowTransition::Activated).await;
    change(
        &paused,
        FlowTransition::ExecutionPaused {
            key: Uuid::new_v4().into(),
        },
    )
    .await;
    let closed = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    change(&closed, FlowTransition::CloseRequested).await;
    change(&closed, FlowTransition::Closed).await;
    let remote_info = session_descriptor();
    let remote_session = host
        .attach_session(root.create_session(remote_info.clone()).unwrap())
        .unwrap();
    remote_session
        .create_flow(flow_descriptor(remote_info.id))
        .await
        .unwrap();
    let source = r#"
        flow main() -> any {
            first = flow.instances(limit: 2)
            second = flow.instances(before: first.next_cursor, limit: 2)
            status = flow.status(handle: second.instances[0].handle)
            return [first, second, status]
        }
    "#;
    let a = session
        .create_flow(program_descriptor(info.id, source.into()))
        .await
        .unwrap();
    change(&a, FlowTransition::Activated).await;
    let result = invoke(&a, Executor::new())
        .await
        .result
        .unwrap()
        .to_json()
        .unwrap();
    assert_eq!(result[0]["total"], 4);
    assert_eq!(result[1]["total"], 4);
    assert_eq!(
        result[0]["instances"][0]["handle"],
        a.address().instance_id.to_string()
    );
    assert_eq!(result[0]["instances"][0]["status"], "running");
    assert_eq!(
        result[0]["instances"][1]["handle"],
        closed.address().instance_id.to_string()
    );
    assert_eq!(result[0]["instances"][1]["status"], "closed");
    assert_eq!(
        result[1]["instances"][0]["handle"],
        paused.address().instance_id.to_string()
    );
    assert_eq!(result[1]["instances"][0]["status"], "idle");
    assert_eq!(result[1]["instances"][0]["paused"], true);
    assert_eq!(
        result[1]["instances"][1]["handle"],
        prepared.address().instance_id.to_string()
    );
    assert_eq!(result[1]["instances"][1]["status"], "prepared");
    assert!(result[1]["next_cursor"].is_null());
    for field in ["status", "state", "revision", "paused", "address"] {
        assert_eq!(result[1]["instances"][0][field], result[2][field]);
    }
    assert!(
        result[0]["spawn_token"]
            .as_str()
            .unwrap()
            .starts_with("spawn_")
    );
    assert_eq!(
        prepared.state().await.unwrap().state(),
        &InstanceState::Prepared
    );
    assert_eq!(
        closed.state().await.unwrap().state(),
        &InstanceState::Closed
    );
}

#[tokio::test]
async fn instance_discovery_preserves_unavailable_rows_and_stable_pages_without_reviving_flows() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let older = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    let unavailable = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    let a = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    change(&a, FlowTransition::Activated).await;
    let lease = start(&a).await;
    let client = host.bind_invocation(lease.clone()).await.unwrap();
    change(&unavailable, FlowTransition::Activated).await;
    start(&unavailable).await;
    let store = unavailable.clone().shutdown().await.unwrap();
    let journal_path = store.dir().join("events.jsonl");
    let page = client.instances(None, 2).await.unwrap();
    assert_eq!(page.total, 3);
    assert_eq!(page.instances[0].address, *a.address());
    assert_eq!(page.instances[1].address, *unavailable.address());
    assert!(page.instances[1].state.is_none());
    assert!(page.instances[1].unavailable.is_some());
    assert_eq!(page.next.as_ref(), Some(&unavailable.address().instance_id));
    let inserted = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    let next = client.instances(page.next, 2).await.unwrap();
    assert_eq!(next.total, 4);
    assert_eq!(next.instances.len(), 1);
    assert_eq!(next.instances[0].address, *older.address());
    assert!(next.next.is_none());
    assert_eq!(
        inserted.state().await.unwrap().state(),
        &InstanceState::Prepared
    );
    assert!(
        client
            .instances(Some(FlowInstanceId::now()), 1)
            .await
            .is_err()
    );
    assert!(client.instances(None, 0).await.is_err());
    assert!(client.instances(None, 51).await.is_err());
    drop(store);
    let restored = client.instances(None, 4).await.unwrap();
    let row = restored
        .instances
        .iter()
        .find(|item| item.address == *unavailable.address())
        .unwrap();
    assert!(row.unavailable.is_none());
    assert!(matches!(
        row.state.as_ref().unwrap().state(),
        InstanceState::Open(InstanceActivity::Interrupted { .. })
    ));
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(journal_path)
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&file)
        .expect("discovery must release an inactive journal, not retain a context worker");
    fs2::FileExt::unlock(&file).unwrap();
    change(
        &a,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    )
    .await;
    assert_eq!(
        client.instances(None, 4).await.err().unwrap().kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[tokio::test]
async fn executor_routes_shared_flow_tools_through_bound_instance_identity() {
    use atman_runtime::ValueJson;
    use atman_runtime::flow_store::access::{AccessTarget, FlowAction, FlowGrant};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let remote_info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let remote_session = host
        .attach_session(root.create_session(remote_info.clone()).unwrap())
        .unwrap();
    let b = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    let d = remote_session
        .create_flow(flow_descriptor(remote_info.id))
        .await
        .unwrap();
    for worker in [&b, &d] {
        change(worker, FlowTransition::Activated).await;
    }
    invoke(&b, Executor::new()).await.result.unwrap();
    let request = Uuid::new_v4();
    let source = format!(
        r#"
        flow inspect(target: string) -> string {{
            return flow.status(handle: target).status
        }}
        flow main() -> any {{
            before = inspect("{}").await
            concurrent = fanout [inspect("{}"), inspect("{}/{}")]
            sent = flow.interject(handle: "{}", text: "peer input", request_id: "{}", delivery: "next_turn")
            retried = flow.interject(handle: "{}/{}", text: "peer input", request_id: "{}", delivery: "next_turn")
            remote = flow.interject(handle: "{}/{}", text: "remote input")
            output = flow.output(handle: "{}", cursor: 1, limit: 3)
            return [before, concurrent, sent, retried, remote, output]
        }}
    "#,
        b.address().instance_id,
        b.address().instance_id,
        info.id,
        b.address().instance_id,
        b.address().instance_id,
        request,
        info.id,
        b.address().instance_id,
        request,
        remote_info.id,
        d.address().instance_id,
        b.address().instance_id
    );
    let a = session
        .create_flow(program_descriptor(info.id, source))
        .await
        .unwrap();
    change(&a, FlowTransition::Activated).await;
    session
        .grant_access(
            session.snapshot().await.unwrap().cursor,
            FlowGrant {
                id: Uuid::new_v4(),
                grantee: a.address().clone(),
                target: AccessTarget::Flow(d.address().clone()),
                actions: [FlowAction::Send].into(),
            },
        )
        .await
        .unwrap();
    let completion = invoke(&a, Executor::new()).await;
    assert_eq!(completion.outcome, TurnOutcome::Completed);
    let value = completion.result.unwrap().to_json().unwrap();
    assert_eq!(value[0], "idle");
    assert_eq!(value[1], serde_json::json!(["idle", "idle"]));
    assert_eq!(value[2], value[3]);
    assert_eq!(value[2]["delivery"]["state"], "accepted");
    assert_eq!(value[4]["delivery"]["state"], "accepted");
    assert_eq!(value[5]["chunk"], "eli");
    assert_eq!(value[5]["next_cursor"], 4);
    assert_eq!(value[5]["eof"], false);
    for worker in [&b, &d] {
        let queued = worker.snapshot().await.unwrap().pending;
        assert_eq!(queued.len(), 1);
        assert_eq!(
            queued[0].request.sender,
            SenderIdentity::Flow(a.address().clone())
        );
    }
    assert!(a.pending_sends().await.unwrap().is_empty());
}

#[tokio::test]
async fn instance_flow_tools_fail_closed_and_controls_do_not_close_the_recipient() {
    use atman_runtime::flow_store::access::{AccessTarget, FlowAction, FlowGrant};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let b = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    change(&b, FlowTransition::Activated).await;
    let lease = start(&b).await;
    let revision = b.state().await.unwrap().revision();
    for (source, grant, success) in [
        (
            "flow main() -> any { return flow.status(handle: \"root\") }".to_owned(),
            false,
            false,
        ),
        (
            format!(
                "flow main() -> any {{ return flow.interject(handle: \"{}\", text: \"no\", level: \"l2_course_correct\") }}",
                b.address().instance_id
            ),
            false,
            false,
        ),
        (
            format!(
                "flow main() -> any {{ return flow.kill(handle: \"{}\", expected_revision: {revision}) }}",
                b.address().instance_id
            ),
            false,
            false,
        ),
        (
            format!(
                "flow main() -> any {{ return flow.kill(handle: \"{}\") }}",
                b.address().instance_id
            ),
            true,
            false,
        ),
        (
            format!(
                "flow main() -> any {{ return flow.kill(handle: \"{}\", expected_revision: 0) }}",
                b.address().instance_id
            ),
            true,
            false,
        ),
        (
            format!(
                "flow main() -> any {{ return flow.kill(handle: \"{}\", expected_revision: {revision}) }}",
                b.address().instance_id
            ),
            true,
            true,
        ),
    ] {
        let source = source.replacen(
            "-> any {",
            "-> any { contract { capabilities { shell: true } }",
            1,
        );
        let a = session
            .create_flow(program_descriptor(info.id, source))
            .await
            .unwrap();
        change(&a, FlowTransition::Activated).await;
        if grant {
            session
                .grant_access(
                    session.snapshot().await.unwrap().cursor,
                    FlowGrant {
                        id: Uuid::new_v4(),
                        grantee: a.address().clone(),
                        target: AccessTarget::Flow(b.address().clone()),
                        actions: [FlowAction::Control].into(),
                    },
                )
                .await
                .unwrap();
        }
        let result = invoke(&a, Executor::new()).await.result;
        assert_eq!(result.is_ok(), success, "{result:?}");
        assert!(b.snapshot().await.unwrap().pending.is_empty());
        if !success {
            assert_eq!(b.state().await.unwrap().revision(), revision);
        }
    }
    assert!(
        matches!(b.state().await.unwrap().state(), InstanceState::Open(InstanceActivity::Active(active)) if active.phase == atman_rt::TurnPhase::Cancelling)
    );
    change(
        &b,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Interrupted(atman_rt::Interruption::Cancelled),
        },
    )
    .await;
    assert!(matches!(
        b.state().await.unwrap().state(),
        InstanceState::Open(InstanceActivity::Interrupted { .. })
    ));
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

#[test]
fn access_grants_are_scoped_revocable_and_recovered_from_facts_without_default_flow_privileges() {
    use atman_runtime::flow_store::access::{AccessTarget, FlowAction, FlowGrant};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut store = root.create_session(info.clone()).unwrap();
    let first = flow_descriptor(info.id);
    let mut second = flow_descriptor(info.id);
    second.created_by = Some(first.address.clone());
    drop(store.create_flow(first.clone()).unwrap());
    drop(store.create_flow(second.clone()).unwrap());
    let remote = flow_descriptor(Uuid::new_v4());
    for target in [&second.address, &remote.address] {
        assert!(
            !store
                .projection()
                .access
                .allows(&first.address, target, FlowAction::ReadContext)
        );
        assert!(
            !store
                .projection()
                .access
                .allows(&first.address, target, FlowAction::Control)
        );
    }
    assert!(
        store
            .projection()
            .access
            .allows(&first.address, &second.address, FlowAction::Send)
    );
    assert!(
        !store
            .projection()
            .access
            .allows(&first.address, &remote.address, FlowAction::Send)
    );
    assert!(!store.projection().access.allows(
        &first.address,
        &first.address,
        FlowAction::Delegate
    ));
    store
        .select_default(second.address.instance_id.clone())
        .unwrap();
    assert!(!store.projection().access.allows(
        &second.address,
        &first.address,
        FlowAction::Control
    ));

    let initial = store.cursor().clone();
    let grant = FlowGrant {
        id: Uuid::new_v4(),
        grantee: first.address.clone(),
        target: AccessTarget::Flow(second.address.clone()),
        actions: [FlowAction::ReadContext].into(),
    };
    let granted = store.grant_access(&initial, grant.clone()).unwrap();
    assert_eq!(
        store.grant_access(&initial, grant.clone()).unwrap(),
        granted
    );
    assert!(store.projection().access.allows(
        &first.address,
        &second.address,
        FlowAction::ReadContext
    ));
    assert!(!store.projection().access.allows(
        &second.address,
        &first.address,
        FlowAction::ReadContext
    ));
    let mut conflict = grant.clone();
    conflict.actions.insert(FlowAction::Control);
    assert!(store.grant_access(&granted, conflict).is_err());
    let mut unknown = grant.clone();
    unknown.id = Uuid::new_v4();
    unknown.grantee = flow_descriptor(info.id).address;
    assert!(store.grant_access(&granted, unknown).is_err());
    let cross = FlowGrant {
        id: Uuid::new_v4(),
        grantee: first.address.clone(),
        target: AccessTarget::AllSessions,
        actions: [FlowAction::Send].into(),
    };
    assert!(store.grant_access(&initial, cross.clone()).is_err());
    let crossed = store.grant_access(&granted, cross).unwrap();
    assert!(
        store
            .projection()
            .access
            .allows(&first.address, &remote.address, FlowAction::Send)
    );
    assert!(!store.projection().access.allows(
        &first.address,
        &remote.address,
        FlowAction::Control
    ));
    let revoked = store.revoke_access(&crossed, grant.id).unwrap();
    assert_eq!(store.revoke_access(&crossed, grant.id).unwrap(), revoked);
    assert!(store.grant_access(&revoked, grant).is_err());
    assert!(!store.projection().access.allows(
        &first.address,
        &second.address,
        FlowAction::ReadContext
    ));
    let records: Vec<_> = store.projection().access.grants().cloned().collect();
    std::fs::write(
        store.dir().join("session.json"),
        "forged disposable access cache",
    )
    .unwrap();
    drop(store);
    let reopened = root.open_session(info.id).unwrap();
    assert_eq!(
        reopened
            .projection()
            .access
            .grants()
            .cloned()
            .collect::<Vec<_>>(),
        records
    );
    assert!(!reopened.projection().access.allows(
        &first.address,
        &second.address,
        FlowAction::ReadContext
    ));
    assert!(
        reopened
            .projection()
            .access
            .allows(&first.address, &remote.address, FlowAction::Send)
    );
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
    a_session
        .schedule_flow(local.address.instance_id.clone(), Executor::new(), prepare)
        .await
        .unwrap();
    b_session
        .schedule_flow(remote.address.instance_id.clone(), Executor::new(), prepare)
        .await
        .unwrap();
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
    a_session
        .stop_scheduling(b.address().instance_id.clone())
        .await
        .unwrap();
    b_session
        .stop_scheduling(c.address().instance_id.clone())
        .await
        .unwrap();
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

#[tokio::test]
async fn bound_peer_clients_enforce_live_grants_identity_and_independent_pause_ownership() {
    use atman_runtime::flow_runtime::access::PeerControl;
    use atman_runtime::flow_store::access::{AccessTarget, FlowAction, FlowGrant};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let remote_info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let remote_session = host
        .attach_session(root.create_session(remote_info.clone()).unwrap())
        .unwrap();
    let a_info = flow_descriptor(info.id);
    let b_info = flow_descriptor(info.id);
    let c_info = flow_descriptor(info.id);
    let d_info = flow_descriptor(remote_info.id);
    let a = session.create_flow(a_info.clone()).await.unwrap();
    let b = session.create_flow(b_info.clone()).await.unwrap();
    let c = session.create_flow(c_info.clone()).await.unwrap();
    let d = remote_session.create_flow(d_info.clone()).await.unwrap();
    for worker in [&a, &b, &c, &d] {
        change(worker, FlowTransition::Activated).await;
    }
    let lease = start(&a).await;
    let client = host.bind_invocation(lease.clone()).await.unwrap();
    let controller = host.bind_invocation(start(&c).await).await.unwrap();
    host.deliver(
        DeliveryRequest {
            request_id: Uuid::new_v4(),
            sender: SenderIdentity::User(Uuid::new_v4()),
            recipient: b_info.address.clone(),
            delivery: DeliveryMode::NextTurn,
            cause: None,
            correlation: None,
            scope: None,
            payload: InboxPayload::Interjection {
                text: "private context".into(),
            },
        },
        |_| Ok(()),
    )
    .await
    .unwrap();
    b.start_turn(
        Executor::new(),
        prepare(&b.snapshot().await.unwrap()).unwrap().unwrap(),
    )
    .await
    .unwrap()
    .wait()
    .await
    .unwrap()
    .result
    .unwrap();
    assert_eq!(
        client
            .read_context(b_info.address.clone())
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    let output = client.read_output(b_info.address.clone()).await.unwrap();
    assert_eq!(output.len(), 1);
    assert_eq!(output[0].publication.message.text_concat(), "delivered");
    assert!(
        client
            .read_result(b_info.address.clone(), None)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(
        client
            .read_result(d_info.address.clone(), None)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(client.inspect(b_info.address.clone()).await.is_ok());
    assert!(client.inspect(d_info.address.clone()).await.is_err());
    assert!(
        client
            .control(
                b_info.address.clone(),
                b.state().await.unwrap().revision(),
                PeerControl::Pause
            )
            .await
            .is_err()
    );
    assert!(
        client
            .send(
                d_info.address.clone(),
                Uuid::new_v4(),
                "unauthorized".into(),
                DeliveryMode::AtBoundary
            )
            .await
            .is_err()
    );

    let private = FlowGrant {
        id: Uuid::new_v4(),
        grantee: a_info.address.clone(),
        target: AccessTarget::Flow(b_info.address.clone()),
        actions: [FlowAction::ReadContext, FlowAction::Control].into(),
    };
    session
        .grant_access(session.snapshot().await.unwrap().cursor, private.clone())
        .await
        .unwrap();
    assert!(
        client
            .read_context(b_info.address.clone())
            .await
            .unwrap()
            .iter()
            .any(|message| message.text_concat() == "private context")
    );
    let other_control = FlowGrant {
        id: Uuid::new_v4(),
        grantee: c_info.address.clone(),
        target: AccessTarget::Flow(b_info.address.clone()),
        actions: [FlowAction::Control].into(),
    };
    session
        .grant_access(session.snapshot().await.unwrap().cursor, other_control)
        .await
        .unwrap();
    let user_key = Uuid::new_v4().into();
    change(&b, FlowTransition::ExecutionPaused { key: user_key }).await;
    let stale = b.state().await.unwrap().revision();
    client
        .control(b_info.address.clone(), stale, PeerControl::Pause)
        .await
        .unwrap();
    assert!(
        client
            .control(b_info.address.clone(), stale, PeerControl::Resume)
            .await
            .is_err()
    );
    controller
        .control(
            b_info.address.clone(),
            b.state().await.unwrap().revision(),
            PeerControl::Pause,
        )
        .await
        .unwrap();
    assert_eq!(b.state().await.unwrap().pauses().len(), 3);
    client
        .control(
            b_info.address.clone(),
            b.state().await.unwrap().revision(),
            PeerControl::Resume,
        )
        .await
        .unwrap();
    assert_eq!(b.state().await.unwrap().pauses().len(), 2);
    controller
        .control(
            b_info.address.clone(),
            b.state().await.unwrap().revision(),
            PeerControl::Resume,
        )
        .await
        .unwrap();
    assert_eq!(b.state().await.unwrap().pauses().len(), 1);
    assert!(b.state().await.unwrap().pauses().contains(&user_key));

    let cross = FlowGrant {
        id: Uuid::new_v4(),
        grantee: a_info.address.clone(),
        target: AccessTarget::Session(remote_info.id),
        actions: [FlowAction::Send].into(),
    };
    session
        .grant_access(session.snapshot().await.unwrap().cursor, cross.clone())
        .await
        .unwrap();
    let delivery_id = Uuid::new_v4();
    assert!(
        client
            .send(
                d_info.address.clone(),
                delivery_id,
                "allowed".into(),
                DeliveryMode::AtBoundary
            )
            .await
            .unwrap()
            .pending_reason
            .is_none()
    );
    assert_eq!(
        d.snapshot().await.unwrap().pending[0].request.sender,
        SenderIdentity::Flow(a_info.address.clone())
    );
    assert!(client.inspect(d_info.address.clone()).await.is_err());
    session
        .revoke_access(session.snapshot().await.unwrap().cursor, cross.id)
        .await
        .unwrap();
    assert!(
        client
            .send(
                d_info.address.clone(),
                delivery_id,
                "allowed".into(),
                DeliveryMode::AtBoundary
            )
            .await
            .unwrap()
            .pending_reason
            .is_none()
    );
    assert!(
        client
            .send(
                d_info.address,
                Uuid::new_v4(),
                "revoked".into(),
                DeliveryMode::AtBoundary
            )
            .await
            .is_err()
    );
    assert_eq!(d.snapshot().await.unwrap().pending.len(), 1);
    session
        .revoke_access(session.snapshot().await.unwrap().cursor, private.id)
        .await
        .unwrap();
    assert!(client.read_context(b_info.address.clone()).await.is_err());

    change(
        &a,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    )
    .await;
    let next = host.bind_invocation(start(&a).await).await.unwrap();
    assert!(client.inspect(b_info.address.clone()).await.is_err());
    assert!(next.inspect(b_info.address).await.is_ok());
}

#[tokio::test]
async fn peer_reads_and_goal_edits_recheck_revocation_inside_the_target_owner() {
    use atman_runtime::flow_store::access::{AccessTarget, FlowAction, FlowGrant};
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
    let client = host.bind_invocation(start(&a).await).await.unwrap();
    let grant = FlowGrant {
        id: Uuid::new_v4(),
        grantee: a_info.address.clone(),
        target: AccessTarget::Flow(b_info.address.clone()),
        actions: [FlowAction::ReadContext, FlowAction::ManageGoal].into(),
    };
    session
        .grant_access(session.snapshot().await.unwrap().cursor, grant.clone())
        .await
        .unwrap();
    let (entered, observed) = tokio::sync::oneshot::channel();
    let (release, held) = std::sync::mpsc::channel();
    let target = b.clone();
    let request = peer_request(&a_info.address, &b_info.address);
    let blocking = tokio::spawn(async move {
        target
            .accept_input(request, move |_| {
                let _ = entered.send(());
                held.recv_timeout(Duration::from_secs(10)).unwrap();
                Ok(())
            })
            .await
    });
    observed.await.unwrap();
    let mut read = Box::pin(client.read_context(b_info.address.clone()));
    let mut goal_read = Box::pin(client.read_goal(b_info.address.clone()));
    let mut goal_edit = Box::pin(client.edit_goal(
        b_info.address,
        Uuid::new_v4(),
        None,
        atman_runtime::memory::goal::GoalEdit::Create {
            text: "revoked change".into(),
        },
    ));
    // The initial authorization passed and the read is queued behind the held target command.
    assert!(futures::poll!(&mut read).is_pending());
    assert!(futures::poll!(&mut goal_read).is_pending());
    assert!(futures::poll!(&mut goal_edit).is_pending());
    session
        .revoke_access(session.snapshot().await.unwrap().cursor, grant.id)
        .await
        .unwrap();
    release.send(()).unwrap();
    blocking.await.unwrap().unwrap();
    assert_eq!(
        read.await.unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        goal_read.await.unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        goal_edit.await.unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(b.goal().await.unwrap().goal.is_none());
}

async fn wait_inputs(worker: &FlowWorker, count: usize) -> FlowSnapshot {
    let mut changes = worker.subscribe_commits();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            changes.borrow_and_update();
            let snapshot = worker.snapshot().await.unwrap();
            if snapshot.pending.len() == count {
                return snapshot;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .expect("observation did not deliver expected inputs")
}

fn answer_watch(source: &FlowWorker) -> atman_runtime::flow_store::subscription::FlowSubscription {
    use atman_runtime::flow_store::subscription::{
        FlowEventFilter, FlowSubscription, SubscriptionPurpose,
    };
    FlowSubscription {
        id: Uuid::new_v4(),
        source: source.address().clone(),
        filter: FlowEventFilter::FinalAnswers,
        once: false,
        purpose: SubscriptionPurpose::Watcher,
        reply: None,
    }
}

#[tokio::test]
async fn correlated_completions_share_inbox_ownership_and_resume_independent_continuations() {
    use atman_runtime::{
        Value,
        flow_store::{inbox::InputContent, subscription::FlowEventFilter, waiter::ReplyStatus},
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let recipient = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    change(&recipient, FlowTransition::Activated).await;
    let lease = start(&recipient).await;
    let client = host.bind_invocation(lease.clone()).await.unwrap();
    let mut waits = Vec::new();
    for expression in ["42", "1 / 0"] {
        let source = session
            .create_flow(program_descriptor(
                info.id,
                format!("flow main() -> int {{ return {expression} }}"),
            ))
            .await
            .unwrap();
        change(&source, FlowTransition::Activated).await;
        source
            .accept_input(peer_request(recipient.address(), source.address()), |_| {
                Ok(())
            })
            .await
            .unwrap();
        let request = prepare(&source.snapshot().await.unwrap()).unwrap().unwrap();
        let wait = client
            .prepare_reply(source.address().clone(), Uuid::new_v4())
            .await
            .unwrap();
        let mut subscription = answer_watch(&source);
        subscription.filter = FlowEventFilter::TurnCompleted {
            turn: Some(request.turn.clone()),
        };
        subscription.once = true;
        subscription.reply = Some(wait.waiter().clone());
        client.watch_events(subscription.clone()).await.unwrap();
        subscription.id = Uuid::new_v4();
        subscription.reply = None;
        client.watch_events(subscription).await.unwrap();
        waits.push((source, request, wait));
    }
    let (second, second_request, second_wait) = waits.pop().unwrap();
    let (first, first_request, first_wait) = waits.pop().unwrap();
    let retry = first_wait.waiter().clone();
    assert!(
        client
            .prepare_reply(second.address().clone(), retry.correlation)
            .await
            .is_err()
    );
    let pause = Uuid::new_v4().into();
    change(&recipient, FlowTransition::ExecutionPaused { key: pause }).await;
    let first_turn = first_request.turn.clone();
    first
        .start_turn(Executor::new(), first_request)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    let error = second
        .start_turn(Executor::new(), second_request)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap()
        .result
        .unwrap_err();
    let pending = wait_inputs(&recipient, 2).await;
    for input in &pending.pending {
        let InboxPayload::FlowEvent { content, .. } = &input.request.payload else {
            panic!("completion")
        };
        assert_eq!(content["subscriptions"].as_array().unwrap().len(), 2);
        assert!(input.request.correlation.is_some());
    }
    let second_turn = second
        .completion(None)
        .await
        .unwrap()
        .unwrap()
        .completion
        .lease
        .turn;
    let mut first_result = Box::pin(first_wait.completion(first_turn.clone()));
    assert!(futures::poll!(&mut first_result).is_pending());
    change(&recipient, FlowTransition::ExecutionResumed { key: pause }).await;
    let (first_result, second_result) = tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(first_result, second_wait.completion(second_turn))
    })
    .await
    .unwrap();
    assert!(matches!(first_result.unwrap(), Value::Int(42)));
    assert_eq!(second_result.unwrap_err(), error);
    assert!(recipient.snapshot().await.unwrap().pending.is_empty());
    let repeated = client
        .prepare_reply(first.address().clone(), retry.correlation)
        .await
        .unwrap();
    assert!(matches!(
        repeated.completion(first_turn).await.unwrap(),
        Value::Int(42)
    ));
    let abandoned = client
        .prepare_reply(first.address().clone(), Uuid::new_v4())
        .await
        .unwrap();
    let mut abandoned_subscription = answer_watch(&first);
    abandoned_subscription.filter = FlowEventFilter::TurnCompleted {
        turn: Some(TurnId::now()),
    };
    abandoned_subscription.once = true;
    abandoned_subscription.reply = Some(abandoned.waiter().clone());
    let mut changes = recipient.subscribe_commits();
    changes.borrow_and_update();
    drop(abandoned);
    tokio::time::timeout(Duration::from_secs(10), changes.changed())
        .await
        .unwrap()
        .unwrap();
    assert!(client.watch_events(abandoned_subscription).await.is_err());
    change(
        &recipient,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    )
    .await;
    let store = recipient.shutdown().await.unwrap();
    assert_eq!(store.committed_inputs().len(), 2);
    assert!(store.committed_inputs().iter().all(|record| matches!(
        record.input.content,
        InputContent::WaitingContinuation { .. }
    )));
    assert_eq!(
        store
            .reply_waiters()
            .iter()
            .filter(|record| matches!(record.status, ReplyStatus::Delivered { .. }))
            .count(),
        2
    );
    assert!(matches!(
        store.reply_waiters()[2].status,
        ReplyStatus::Cancelled { .. }
    ));
    drop(store);
    let reopened = host.flow(client.source()).await.unwrap();
    assert!(reopened.snapshot().await.unwrap().pending.is_empty());
}

#[tokio::test]
async fn completion_observation_shares_answer_delivery_and_retains_paused_inputs_after_reopen() {
    use atman_runtime::flow_store::subscription::FlowEventFilter;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let recipient = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    let source = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    change(&recipient, FlowTransition::Activated).await;
    let lease = start(&recipient).await;
    let client = host.bind_invocation(lease.clone()).await.unwrap();
    let answer = answer_watch(&source);
    let mut completion = answer_watch(&source);
    completion.filter = FlowEventFilter::TurnCompleted { turn: None };
    let mut once = completion.clone();
    once.id = Uuid::new_v4();
    once.once = true;
    client.watch_events(answer).await.unwrap();
    client.watch_events(completion).await.unwrap();
    client.watch_events(once).await.unwrap();
    change(
        &recipient,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    )
    .await;
    let pause = Uuid::new_v4().into();
    change(&recipient, FlowTransition::ExecutionPaused { key: pause }).await;
    change(&source, FlowTransition::Activated).await;
    let completed = invoke(&source, Executor::new()).await;
    let accepted = wait_inputs(&recipient, 2).await;
    assert_eq!(accepted.state.pauses(), &[pause]);
    let InboxPayload::FlowEvent {
        source: answer_ref,
        event,
        ..
    } = &accepted.pending[0].request.payload
    else {
        panic!("missing answer")
    };
    assert_eq!(event, "final_answer");
    let InboxPayload::FlowEvent {
        source: completion_ref,
        event,
        content,
    } = &accepted.pending[1].request.payload
    else {
        panic!("missing completion")
    };
    assert_eq!(event, "turn_completed");
    assert!(answer_ref.sequence < completion_ref.sequence);
    assert_eq!(content["subscriptions"].as_array().unwrap().len(), 2);
    assert_eq!(
        content["completion"]["receipt"],
        serde_json::to_value(&completed.receipt).unwrap()
    );
    assert_eq!(content["completion"]["result"]["kind"], "returned");
    let originals: Vec<_> = accepted.pending.iter().map(|e| e.id).collect();
    let address = recipient.address().clone();
    drop(client);
    drop(recipient.shutdown().await.unwrap());
    invoke(&source, Executor::new()).await.result.unwrap();
    let reopened = host.flow(&address).await.unwrap();
    let recovered = wait_inputs(&reopened, 4).await;
    assert_eq!(
        recovered.pending[..2]
            .iter()
            .map(|e| e.id)
            .collect::<Vec<_>>(),
        originals
    );
    let InboxPayload::FlowEvent { content, .. } = &recovered.pending[3].request.payload else {
        panic!("missing completion")
    };
    assert_eq!(content["subscriptions"].as_array().unwrap().len(), 1);
    assert_ne!(
        recovered.pending[1].request.request_id,
        recovered.pending[3].request.request_id
    );
    change(&reopened, FlowTransition::ExecutionResumed { key: pause }).await;
    assert_eq!(reopened.snapshot().await.unwrap().pending.len(), 4);
}

#[tokio::test]
async fn owned_observation_survives_turn_end_pause_and_reopen_without_duplicate_input() {
    use atman_runtime::flow_store::subscription::{SubscriptionPurpose, SubscriptionStatus};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let a = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    let b = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    change(&a, FlowTransition::Activated).await;
    let lease = start(&a).await;
    let client = host.bind_invocation(lease.clone()).await.unwrap();
    // Register against Prepared sources; a creator can safely activate only after acknowledgement.
    let result = atman_runtime::flow_store::subscription::FlowSubscription {
        purpose: SubscriptionPurpose::SpawnResult,
        reply: None,
        ..answer_watch(&b)
    };
    let watcher = atman_runtime::flow_store::subscription::FlowSubscription {
        once: true,
        ..answer_watch(&b)
    };
    let (registered, duplicate) = tokio::join!(
        client.watch_events(result.clone()),
        client.watch_events(result.clone())
    );
    assert_eq!(registered.unwrap(), duplicate.unwrap());
    client.watch_events(watcher.clone()).await.unwrap();
    let self_watch = answer_watch(&a);
    client.watch_events(self_watch.clone()).await.unwrap();
    change(
        &a,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    )
    .await;
    let pause = Uuid::new_v4().into();
    change(&a, FlowTransition::ExecutionPaused { key: pause }).await;
    change(&b, FlowTransition::Activated).await;
    invoke(&b, Executor::new()).await.result.unwrap();
    let accepted = wait_inputs(&a, 1).await;
    assert_eq!(accepted.state.pauses(), &[pause]);
    let InboxPayload::FlowEvent { content, .. } = &accepted.pending[0].request.payload else {
        panic!("missing source event")
    };
    assert_eq!(content["subscriptions"].as_array().unwrap().len(), 2);
    let original = accepted.pending[0].id;
    assert!(
        a.flow_subscriptions()
            .await
            .unwrap()
            .iter()
            .any(|r| r.subscription.id == watcher.id && r.status == SubscriptionStatus::Matched)
    );

    // A stopped view/executor does not erase subscription positions; reopening the owner catches up.
    let address = a.address().clone();
    drop(client);
    drop(a.shutdown().await.unwrap());
    invoke(&b, Executor::new()).await.result.unwrap();
    let a = host.flow(&address).await.unwrap();
    let restored = wait_inputs(&a, 2).await;
    assert_eq!(restored.pending[0].id, original);
    let InboxPayload::FlowEvent { content, .. } = &restored.pending[1].request.payload else {
        panic!("missing source event")
    };
    assert_eq!(content["subscriptions"].as_array().unwrap().len(), 1);
    change(&a, FlowTransition::ExecutionResumed { key: pause }).await;
    let client = host.bind_invocation(start(&a).await).await.unwrap();
    client.unwatch_events(result.id).await.unwrap();
    // Retrying registration must retain cancellation, even when a newer source anchor is available.
    assert_eq!(
        client.watch_events(result).await.unwrap().status,
        SubscriptionStatus::Cancelled
    );
    let lease = match a.state().await.unwrap().state() {
        InstanceState::Open(InstanceActivity::Active(active)) => active.lease.clone(),
        _ => unreachable!(),
    };
    change(
        &a,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    )
    .await;
    invoke(&a, Executor::new()).await.result.unwrap();
    // Existing inputs are consumed by the invocation; its self-observed final answer is the only new input.
    wait_inputs(&a, 1).await;
    let before = a.snapshot().await.unwrap().cursor;
    a.retry_observations();
    tokio::time::sleep(Duration::from_millis(40)).await;
    assert_eq!(a.snapshot().await.unwrap().cursor, before);
    invoke(&b, Executor::new()).await.result.unwrap();
    assert_eq!(a.snapshot().await.unwrap().pending.len(), 1);

    // The host directory and catalog own scanners without retaining each other indefinitely.
    let mut commits = a.subscribe_commits();
    drop(client);
    drop(a);
    drop(b);
    drop(session);
    drop(host);
    tokio::time::timeout(Duration::from_secs(5), async {
        while commits.changed().await.is_ok() {}
    })
    .await
    .expect("observer leaked the directory or worker");
}

#[tokio::test]
async fn cross_session_observation_retains_cursor_on_revocation_and_recovers_after_late_attach() {
    use atman_runtime::flow_store::access::{AccessTarget, FlowAction, FlowGrant};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let a_info = session_descriptor();
    let b_info = session_descriptor();
    let session = host
        .attach_session(root.create_session(a_info.clone()).unwrap())
        .unwrap();
    let remote = host
        .attach_session(root.create_session(b_info.clone()).unwrap())
        .unwrap();
    let a = session
        .create_flow(flow_descriptor(a_info.id))
        .await
        .unwrap();
    let b = remote
        .create_flow(flow_descriptor(b_info.id))
        .await
        .unwrap();
    change(&a, FlowTransition::Activated).await;
    change(&b, FlowTransition::Activated).await;
    let lease = start(&a).await;
    let client = host.bind_invocation(lease.clone()).await.unwrap();
    let subscription = answer_watch(&b);
    assert_eq!(
        client
            .watch_events(subscription.clone())
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    let grant = FlowGrant {
        id: Uuid::new_v4(),
        grantee: a.address().clone(),
        target: AccessTarget::Flow(b.address().clone()),
        actions: [FlowAction::Watch].into(),
    };
    session
        .grant_access(session.snapshot().await.unwrap().cursor, grant.clone())
        .await
        .unwrap();
    client.watch_events(subscription.clone()).await.unwrap();
    change(
        &a,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    )
    .await;
    session
        .revoke_access(session.snapshot().await.unwrap().cursor, grant.id)
        .await
        .unwrap();
    let mut issues = a.subscribe_observation_issues();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !issues.borrow_and_update().is_empty() {
                break;
            }
            issues.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    invoke(&b, Executor::new()).await.result.unwrap();
    assert!(a.snapshot().await.unwrap().pending.is_empty());
    session
        .grant_access(
            session.snapshot().await.unwrap().cursor,
            FlowGrant {
                id: Uuid::new_v4(),
                ..grant
            },
        )
        .await
        .unwrap();
    wait_inputs(&a, 1).await;
    let address = a.address().clone();
    let mut changes = a.subscribe_commits();
    drop(issues);
    drop(client);
    drop(a);
    drop(b);
    drop(session);
    drop(remote);
    drop(host);
    tokio::time::timeout(Duration::from_secs(5), async {
        while changes.changed().await.is_ok() {}
    })
    .await
    .unwrap();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.open_session(a_info.id).unwrap())
        .unwrap();
    let a = session.open_flow(address.instance_id).await.unwrap();
    let mut issues = a.subscribe_observation_issues();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if !issues.borrow_and_update().is_empty() {
                break;
            }
            issues.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    // A late source attachment must wake the receiver without a new turn or manual resubscription.
    let remote_store = root.open_session(b_info.id).unwrap();
    let mut source_store = remote_store
        .open_flow(&subscription.source.instance_id)
        .unwrap();
    // More than one source page, with no further source commit after attachment.
    for _ in 0..65 {
        let lease = source_store
            .state()
            .clone()
            .begin_turn(TurnId::now(), RunId::now())
            .unwrap();
        source_store
            .transition(
                source_store.state().revision(),
                FlowTransition::TurnStarted {
                    turn: lease.turn.clone(),
                    run: lease.run.clone(),
                },
            )
            .unwrap();
        let mut message =
            atman_runtime::message::Message::assistant_text(lease.turn.clone(), "offline answer");
        message.origin = MessageOrigin::FinalAnswer;
        source_store
            .publish_message(atman_runtime::flow_store::publication::MessagePublication {
                id: Uuid::new_v4(),
                lease: lease.clone(),
                run: lease.run.clone(),
                context_run: None,
                message,
            })
            .unwrap();
        source_store
            .transition(
                source_store.state().revision(),
                FlowTransition::TurnFinished {
                    lease,
                    outcome: TurnOutcome::Completed,
                },
            )
            .unwrap();
    }
    drop(source_store);
    host.attach_session(remote_store).unwrap();
    wait_inputs(&a, 66).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if issues.borrow_and_update().is_empty() {
                break;
            }
            issues.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
}

async fn wait_answers(worker: &FlowWorker, count: usize) {
    let mut commits = worker.subscribe_commits();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            commits.borrow_and_update();
            let snapshot = worker.snapshot().await.unwrap();
            if snapshot.state.can_start_turn()
                && worker
                    .context()
                    .messages()
                    .iter()
                    .filter(|message| message.origin == MessageOrigin::FinalAnswer)
                    .count()
                    == count
            {
                break;
            }
            commits.changed().await.unwrap();
        }
    })
    .await
    .expect("scheduled answer was not delivered");
}

#[tokio::test]
async fn catalog_owned_schedulers_run_subscribed_inputs_after_callers_and_views_detach() {
    use atman_runtime::flow_runtime::scheduler::SchedulingStatus;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let a = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    let b = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    change(&a, FlowTransition::Activated).await;
    let lease = start(&a).await;
    let client = host.bind_invocation(lease.clone()).await.unwrap();
    client.watch_events(answer_watch(&b)).await.unwrap();
    drop(client);
    change(
        &a,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    )
    .await;
    let a_ref = a.address().clone();
    let b_ref = b.address().clone();
    // Dropping the result receivers and views must not withdraw execution ownership.
    session
        .schedule_flow(a_ref.instance_id.clone(), Executor::new(), prepare)
        .await
        .unwrap();
    session
        .schedule_flow(b_ref.instance_id.clone(), Executor::new(), prepare)
        .await
        .unwrap();
    assert!(
        session
            .schedule_flow(b_ref.instance_id.clone(), Executor::new(), prepare)
            .await
            .is_err()
    );
    let pause = Uuid::new_v4().into();
    change(&a, FlowTransition::ExecutionPaused { key: pause }).await;
    drop(a);
    drop(b);
    drop(session);
    let a = host.flow(&a_ref).await.unwrap();
    let b = host.flow(&b_ref).await.unwrap();
    // Installing a controller does not activate the instance or admit arbitrary input.
    let initial = peer_request(&a_ref, &b_ref);
    assert!(host.deliver(initial.clone(), |_| Ok(())).await.is_err());
    assert!(matches!(
        b.state().await.unwrap().state(),
        InstanceState::Prepared
    ));
    assert!(b.snapshot().await.unwrap().pending.is_empty());
    change(&b, FlowTransition::Activated).await;
    host.deliver(initial, |_| Ok(())).await.unwrap();
    wait_answers(&b, 1).await;
    wait_inputs(&a, 1).await;
    assert!(
        a.context()
            .messages()
            .iter()
            .all(|m| m.origin != MessageOrigin::FinalAnswer)
    );
    change(&a, FlowTransition::ExecutionResumed { key: pause }).await;
    wait_answers(&a, 1).await;
    assert!(a.snapshot().await.unwrap().pending.is_empty());
    let session = host.session(info.id).unwrap();
    assert_eq!(
        *session
            .scheduling_status(a_ref.instance_id.clone())
            .await
            .unwrap()
            .unwrap()
            .borrow(),
        SchedulingStatus::Running
    );
    // Only explicit stopping releases admission; input stays durable until a new binding is installed.
    session
        .stop_scheduling(a_ref.instance_id.clone())
        .await
        .unwrap();
    host.deliver(peer_request(&b_ref, &a_ref), |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(a.snapshot().await.unwrap().pending.len(), 1);
    session
        .schedule_flow(a_ref.instance_id.clone(), Executor::new(), prepare)
        .await
        .unwrap();
    wait_answers(&a, 2).await;
    let mut status = session
        .scheduling_status(a_ref.instance_id)
        .await
        .unwrap()
        .unwrap();
    drop(a);
    drop(b);
    drop(session);
    drop(host);
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if *status.borrow_and_update() != SchedulingStatus::Running {
                break;
            }
            status.changed().await.unwrap();
        }
    })
    .await
    .expect("host scheduler kept the directory alive");
    assert_eq!(*status.borrow(), SchedulingStatus::Stopped);
}

#[tokio::test]
async fn host_scheduler_failures_remain_observable_and_replacement_keeps_unconsumed_input() {
    use atman_runtime::flow_runtime::scheduler::SchedulingStatus;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let worker = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    change(&worker, FlowTransition::Activated).await;
    let mut status = session
        .schedule_flow(
            worker.address().instance_id.clone(),
            Executor::new(),
            |_| panic!("preparation panic"),
        )
        .await
        .unwrap();
    host.deliver(peer_request(worker.address(), worker.address()), |_| Ok(()))
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
    assert!(matches!(
        *session
            .scheduling_status(worker.address().instance_id.clone())
            .await
            .unwrap()
            .unwrap()
            .borrow(),
        SchedulingStatus::Failed(_)
    ));
    assert_eq!(worker.snapshot().await.unwrap().pending.len(), 1);
    assert!(worker.state().await.unwrap().can_start_turn());
    session
        .schedule_flow(
            worker.address().instance_id.clone(),
            Executor::new(),
            prepare,
        )
        .await
        .unwrap();
    wait_answers(&worker, 1).await;
    assert!(worker.snapshot().await.unwrap().pending.is_empty());
}
