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

#[test]
fn activation_cannot_omit_or_invent_recorded_scope_membership() {
    use atman_runtime::flow_store::task_scope::TaskScopeRef;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_info();
    let mut session = root.create_session(info.clone()).unwrap();
    let entry = descriptor(info.id);
    let mut store = session.create_flow(entry.clone()).unwrap();
    let scope = TaskScopeRef {
        owner: descriptor(info.id).address,
        id: Uuid::new_v4(),
    };
    let mut input = initial(&entry, &scope.owner);
    input.result_route = None;
    input.scope = Some(scope.clone());
    let unbound = store.cursor().clone();
    assert!(
        store
            .activate_entry(input.clone(), None, |_| Ok(()))
            .is_err()
    );
    assert_eq!(store.cursor(), &unbound);
    store.bind_task_scope(scope.clone()).unwrap();
    let bound = store.cursor().clone();
    let mut missing = input.clone();
    missing.scope = None;
    assert!(store.activate_entry(missing, None, |_| Ok(())).is_err());
    assert_eq!(store.cursor(), &bound);
    let committed = store.activate_entry(input, None, |_| Ok(())).unwrap();
    drop(store);
    let restored = session.open_flow(&entry.address.instance_id).unwrap();
    assert_eq!(restored.task_scope_binding(), Some(&scope));
    assert_eq!(restored.launch_record().unwrap().receipt, committed.receipt);
    assert_eq!(restored.launch_record().unwrap().input.scope, Some(scope));
}

#[tokio::test]
async fn launch_scope_matches_prepared_membership_and_retains_cancelled_activation_receipts() {
    use atman_runtime::flow_store::task_scope::{TaskScopeDefinition, TaskScopeRef};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_info();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let owner = session.create_flow(descriptor(info.id)).await.unwrap();
    owner
        .transition(0, FlowTransition::Activated)
        .await
        .unwrap();
    let id = Uuid::new_v4();
    owner
        .create_task_scope(
            TaskScopeDefinition {
                id,
                name: "task".into(),
                goal: None,
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    let scope = TaskScopeRef {
        owner: owner.address().clone(),
        id,
    };
    let entry = descriptor(info.id);
    let worker = session.create_flow(entry.clone()).await.unwrap();
    host.bind_task_scope(worker.address().clone(), scope.clone(), |_, _| Ok(()))
        .await
        .unwrap();
    let mut input = initial(&entry, owner.address());
    input.result_route = None;
    let before = worker.snapshot().await.unwrap().cursor;
    assert!(
        host.launch_flow(entry.clone(), input.clone(), |_, _| Ok(()))
            .await
            .err()
            .expect("unbound launch cannot replace recorded membership")
            .to_string()
            .contains("scope")
    );
    assert_eq!(worker.snapshot().await.unwrap().cursor, before);
    assert!(matches!(
        worker.state().await.unwrap().state(),
        InstanceState::Prepared
    ));
    input.scope = Some(scope.clone());
    let launched = host
        .launch_flow(entry.clone(), input.clone(), |_, _| Ok(()))
        .await
        .unwrap();
    owner.cancel_task_scope(id, |_| Ok(())).await.unwrap();
    let retried = host
        .launch_flow(entry.clone(), input.clone(), |_, _| {
            panic!("retry reauthorized")
        })
        .await
        .unwrap();
    assert_eq!(retried.record.receipt, launched.record.receipt);
    let fresh = descriptor(info.id);
    let mut cancelled_input = initial(&fresh, owner.address());
    cancelled_input.result_route = None;
    cancelled_input.scope = Some(scope.clone());
    assert!(
        host.launch_flow(fresh, cancelled_input, |_, _| Ok(()))
            .await
            .is_err()
    );
    // A different request cannot remove the lifetime binding, even after cancellation.
    input.scope = None;
    assert!(
        host.launch_flow(entry.clone(), input, |_, _| Ok(()))
            .await
            .is_err()
    );
    host.shutdown().await.unwrap();
    let restored = root.open_session(info.id).unwrap();
    let member = restored.open_flow(&entry.address.instance_id).unwrap();
    assert_eq!(member.task_scope_binding(), Some(&scope));
    assert_eq!(
        member.launch_record().unwrap().receipt,
        launched.record.receipt
    );
    assert_eq!(member.launch_record().unwrap().input.scope, Some(scope));
}

#[tokio::test]
async fn deferred_launch_checks_catalog_before_registration_and_retains_accepted_retries() {
    let dir = tempfile::tempdir().unwrap();
    let store = RuntimeStore::open(dir.path()).unwrap();
    let info = session_info();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(store.create_session(info.clone()).unwrap())
        .unwrap();
    let stale = session.snapshot().await.unwrap().cursor;
    let project = atman_runtime::storage::ProjectContext::resolve(
        &atman_runtime::config_hub::ConfigHub::from_config_dir(dir.path().join("config")),
        &dir.path().join("destination"),
        dir.path(),
    )
    .unwrap();
    session
        .set_project(stale.clone(), Some(project.clone()))
        .await
        .unwrap();
    let current = session.snapshot().await.unwrap();
    let entry = descriptor(info.id);
    let mut input = initial(&entry, &entry.address);
    input.result_route = None;
    input.request.sender = SenderIdentity::Host(Uuid::now_v7());
    assert!(
        host.launch_flow_at(entry.clone(), input.clone(), stale.clone(), |_, _| Ok(()))
            .await
            .is_err()
    );
    assert_eq!(session.snapshot().await.unwrap().cursor, current.cursor);
    assert!(session.snapshot().await.unwrap().catalog.flows.is_empty());
    let accepted = host
        .launch_flow_at(entry.clone(), input.clone(), current.cursor, |_, _| Ok(()))
        .await
        .unwrap();
    assert_eq!(accepted.worker.project().await.unwrap(), Some(project));
    let before = accepted.worker.snapshot().await.unwrap();
    let receipt = session.snapshot().await.unwrap().cursor;
    session.set_project(receipt, None).await.unwrap();
    let repeated = host
        .launch_flow_at(entry, input, stale, |_, _| {
            panic!("accepted activation must not repeat authorization or effects")
        })
        .await
        .unwrap();
    assert_eq!(repeated.record.receipt, accepted.record.receipt);
    assert_eq!(
        accepted.worker.snapshot().await.unwrap().cursor,
        before.cursor
    );
    assert!(
        session
            .scheduling_status(accepted.worker.address().instance_id.clone())
            .await
            .unwrap()
            .is_none()
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn launch_validates_resource_scope_before_registering_a_flow() {
    use atman_runtime::config_hub::ConfigHub;
    use atman_runtime::flow_runtime::launch::EntryExecution;
    use atman_runtime::flow_store::SessionConfiguration;
    use atman_runtime::flow_store::launch::EntryBinding;
    use atman_runtime::provider::ProviderRegistry;
    use atman_runtime::storage::ProjectContext;
    use atman_runtime::tool::ToolCtx;

    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let config = ConfigHub::from_config_dir(dir.path().join("config"));
    let other = ConfigHub::from_config_dir(dir.path().join("other-config"));
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let a = ProjectContext::resolve(&config, &a, dir.path()).unwrap();
    let b = ProjectContext::resolve(&config, &b, dir.path()).unwrap();
    let mut info = session_info();
    info.configuration = SessionConfiguration::Atman(config.source());
    info.project = Some(a.clone());
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let snapshot = session.snapshot().await.unwrap();
    session
        .set_project(snapshot.cursor, Some(b.clone()))
        .await
        .unwrap();
    let before = session.snapshot().await.unwrap();
    let entry = descriptor(info.id);
    let authority = EffectiveAuthority::root(&Default::default(), false, None);
    let mut input = initial(&entry, &entry.address);
    input.result_route = None;
    input.request.sender = SenderIdentity::Host(Uuid::now_v7());
    input.execution = Some(EntryBinding {
        user_routes: false,
        session_start: false,
        first_turn: TurnId::now(),
        first_run: RunId::now(),
        authority: authority.clone(),
    });
    let executor = |hub: Option<&ConfigHub>, project| {
        let mut executor = Executor::new();
        executor.tool_ctx = ToolCtx::for_project(project);
        if let Some(hub) = hub {
            executor.providers = ProviderRegistry::with_models(hub.models().clone());
            executor.attach_provider_lifecycle(hub.clone()).unwrap();
        }
        executor
    };
    let execution = |executor| EntryExecution {
        executor,
        authority: authority.clone(),
        trust: Default::default(),
        yield_interval: NonZeroUsize::new(16).unwrap(),
        catalog: before.cursor.clone(),
    };
    for (candidate, reason) in [
        (executor(Some(&config), Some(a)), "executor project"),
        (executor(Some(&config), None), "executor project"),
        (
            executor(Some(&other), Some(b.clone())),
            "provider configuration",
        ),
        (executor(None, Some(b.clone())), "provider configuration"),
    ] {
        let error = host
            .launch_entry(
                entry.clone(),
                input.clone(),
                execution(candidate),
                |_, _| Ok(()),
            )
            .await
            .err()
            .expect("scope mismatch must reject creation");
        assert!(error.to_string().contains(reason), "{error}");
        let after = session.snapshot().await.unwrap();
        assert_eq!(after.cursor, before.cursor);
        assert!(after.catalog.flows.is_empty());
        assert!(
            session
                .instances(None, 50, |_| Ok(()))
                .await
                .unwrap()
                .instances
                .is_empty()
        );
    }
    let valid = executor(Some(&config), Some(b.clone()));
    let launched = host
        .launch_entry(
            entry.clone(),
            input.clone(),
            execution(valid),
            |_, _| Ok(()),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(launched.worker.project().await.unwrap(), Some(b.clone()));
    // An accepted retry acknowledges its original activation without adopting replacement services.
    let repeated = host
        .launch_entry(entry, input, execution(Executor::new()), |_, _| Ok(()))
        .await
        .unwrap();
    assert_eq!(repeated.record.receipt, launched.record.receipt);
    assert_eq!(session.snapshot().await.unwrap().catalog.flows.len(), 1);
    let prepared = descriptor(info.id);
    session.create_flow(prepared.clone()).await.unwrap();
    let snapshot = session.snapshot().await.unwrap();
    session.set_project(snapshot.cursor, None).await.unwrap();
    let current = session.snapshot().await.unwrap();
    let mut input = initial(&prepared, &prepared.address);
    input.result_route = None;
    input.request.sender = SenderIdentity::Host(Uuid::now_v7());
    input.execution = Some(EntryBinding {
        user_routes: false,
        session_start: false,
        first_turn: TurnId::now(),
        first_run: RunId::now(),
        authority: authority.clone(),
    });
    let mut invalid = execution(executor(Some(&config), None));
    invalid.catalog = current.cursor.clone();
    assert!(
        host.launch_entry(prepared.clone(), input.clone(), invalid, |_, _| Ok(()))
            .await
            .is_err()
    );
    assert_eq!(session.snapshot().await.unwrap().cursor, current.cursor);
    let mut valid = execution(executor(Some(&config), Some(b.clone())));
    valid.catalog = current.cursor;
    let resumed = host
        .launch_entry(prepared, input, valid, |_, _| Ok(()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resumed.worker.project().await.unwrap(), Some(b));
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn captured_startup_commands_run_once_without_replacing_the_default_entry() {
    use atman_runtime::flow_runtime::launch::EntryExecution;
    use atman_runtime::flow_store::launch::{EntryBinding, EntryStart};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_info();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let mut entry = descriptor(info.id);
    entry.entry_mut().unwrap().bundle.sources.insert(
        "entry:main.at".into(),
        r#"flow main(label: string, amount: int) -> string {
        contract { invocation { user_message: label } }
        when amount != 42 { return final.answer(message: "wrong defaults") }
        when env("effort") != "high" { return final.answer(message: "wrong invocation") }
        return final.answer(message: label)
    }"#
        .into(),
    );
    let authority = EffectiveAuthority::root(&Default::default(), false, None);
    let mut input = initial(&entry, &entry.address);
    input.request.sender = SenderIdentity::Host(Uuid::now_v7());
    input.request.start = EntryStart::Command {
        entry: Box::new(EntrySnapshot {
            name: "startup".into(),
            path: dir.path().join("startup.at"),
            bundle: RevisionBundle {
                entry: "entry:startup.at".into(),
                edges: Default::default(),
                sources: [(
                    "entry:startup.at".into(),
                    r#"// STARTUP_SOURCE_ONLY_SENTINEL
                    flow startup(label: string) -> string { return final.answer(message: label) }"#
                        .into(),
                )]
                .into(),
            },
        }),
        arguments: [("label".into(), "ready".into())].into(),
        invocation: Default::default(),
    };
    input.inherited_context.clear();
    input.result_route = None;
    input.execution = Some(EntryBinding {
        user_routes: false,
        session_start: true,
        first_turn: TurnId::now(),
        first_run: RunId::now(),
        authority: authority.clone(),
    });
    let launched = host
        .launch_entry(
            entry.clone(),
            input.clone(),
            EntryExecution {
                executor: Executor::new(),
                authority: authority.clone(),
                trust: Default::default(),
                yield_interval: NonZeroUsize::new(16).unwrap(),
                catalog: session.snapshot().await.unwrap().cursor,
            },
            |_, _| Ok(()),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    let receipt = launched.record.receipt.clone();
    assert!(launched.record.acceptance.is_some());
    let messages = launched.worker.context().messages_full();
    assert_eq!(messages[0].origin, MessageOrigin::Internal);
    assert_eq!(messages.last().unwrap().text_concat(), "ready");
    assert!(!messages.iter().any(|message| {
        message
            .text_concat()
            .contains("STARTUP_SOURCE_ONLY_SENTINEL")
    }));
    let mut reader = launched
        .worker
        .subscribe_view(NonZeroUsize::new(1).unwrap())
        .await
        .unwrap();
    let view = reader.refresh().await.unwrap();
    assert_eq!(view.transcript().turns().count(), 1);
    assert!(!view.transcript().has_work_input());
    let page = launched
        .worker
        .read_updates(None, NonZeroUsize::new(64).unwrap())
        .await
        .unwrap();
    assert!(
        !serde_json::to_string(&page.batches)
            .unwrap()
            .contains("STARTUP_SOURCE_ONLY_SENTINEL")
    );
    host.shutdown().await.unwrap();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.open_session(info.id).unwrap())
        .unwrap();
    session
        .bind_entry(
            entry.address.instance_id.clone(),
            Executor::new(),
            authority,
            Default::default(),
            NonZeroUsize::new(16).unwrap(),
        )
        .await
        .unwrap();
    let worker = host.flow(&entry.address).await.unwrap();
    host.wait_quiescent().await.unwrap();
    assert_eq!(worker.context().messages_full().len(), messages.len());
    let mut restored = worker
        .subscribe_view(NonZeroUsize::new(1).unwrap())
        .await
        .unwrap();
    assert!(
        !restored
            .refresh()
            .await
            .unwrap()
            .transcript()
            .has_work_input()
    );
    worker
        .accept_input(
            DeliveryRequest {
                request_id: Uuid::now_v7(),
                sender: SenderIdentity::User(Uuid::now_v7()),
                recipient: entry.address.clone(),
                delivery: DeliveryMode::NextTurn,
                cause: None,
                correlation: None,
                scope: None,
                payload: InboxPayload::UserSubmission {
                    content: atman_runtime::flow_store::inbox::UserContent {
                        text: "ordinary input".into(),
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
    tokio::time::timeout(Duration::from_secs(10), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        worker
            .context()
            .messages_full()
            .last()
            .unwrap()
            .text_concat(),
        "ordinary input"
    );
    let retry = host
        .launch_flow(entry, input, |_, _| panic!("retry reauthorized"))
        .await
        .unwrap();
    assert_eq!(retry.record.receipt, receipt);
    assert!(
        restored
            .refresh()
            .await
            .unwrap()
            .transcript()
            .has_work_input()
    );
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn waiting_entries_restore_without_input_and_start_hooks_with_the_first_real_turn() {
    use atman_runtime::flow_runtime::launch::EntryExecution;
    use atman_runtime::flow_store::launch::{EntryBinding, EntryStart};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_info();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let mut entry = descriptor(info.id);
    entry.entry_mut().unwrap().bundle.sources.insert(
        "entry:main.at".into(),
        r#"flow main(label: string, amount: int) -> string {
        contract { invocation { user_message: label } }
        when amount != 42 { return final.answer(message: "wrong defaults") }
        return final.answer(message: label)
    }"#
        .into(),
    );
    let authority = EffectiveAuthority::root(&Default::default(), false, None);
    let first_turn = TurnId::now();
    let first_run = RunId::now();
    let mut input = initial(&entry, &entry.address);
    input.request.start = EntryStart::Waiting;
    input.request.sender = SenderIdentity::User(Uuid::now_v7());
    input.inherited_context.clear();
    input.result_route = None;
    input.execution = Some(EntryBinding {
        user_routes: false,
        session_start: true,
        first_turn: first_turn.clone(),
        first_run: first_run.clone(),
        authority: authority.clone(),
    });
    let mut invalid = input.clone();
    invalid.execution = None;
    assert!(
        host.launch_flow(entry.clone(), invalid, |_, _| Ok(()))
            .await
            .is_err()
    );
    assert!(session.snapshot().await.unwrap().catalog.flows.is_empty());
    let executor = || {
        let mut executor = Executor::new();
        atman_runtime::tools::register_flow_memory(&executor.tools);
        let mut hooks = atman_runtime::lifecycle::LifecycleRunner::new();
        hooks.absorb(&atman_rt::parse_file(r#"on session.start { memory.todo.set(where: "startup", why: "container", how: "hook", expected_result: "once") }"#).unwrap());
        executor.tool_ctx.lifecycle_hooks = Some(Arc::new(hooks));
        executor
    };
    let launched = host
        .launch_entry(
            entry.clone(),
            input.clone(),
            EntryExecution {
                executor: executor(),
                authority: authority.clone(),
                trust: Default::default(),
                yield_interval: NonZeroUsize::new(16).unwrap(),
                catalog: session.snapshot().await.unwrap().cursor,
            },
            |_, _| Ok(()),
        )
        .await
        .unwrap();
    assert!(launched.record.acceptance.is_none());
    host.wait_quiescent().await.unwrap();
    let snapshot = launched.worker.snapshot().await.unwrap();
    assert!(snapshot.pending.is_empty() && snapshot.last_invocation.is_none());
    assert!(launched.worker.context().messages_full().is_empty());
    assert!(launched.worker.planning().await.unwrap().todos.is_empty());
    let mut reader = launched
        .worker
        .subscribe_view(NonZeroUsize::new(1).unwrap())
        .await
        .unwrap();
    let view = reader.refresh().await.unwrap();
    assert_eq!(view.transcript().turns().count(), 0);
    assert!(view.snapshot().pending.is_empty());
    let receipt = launched.record.receipt;
    host.shutdown().await.unwrap();

    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.open_session(info.id).unwrap())
        .unwrap();
    let worker = session
        .open_flow(entry.address.instance_id.clone())
        .await
        .unwrap();
    let restored = worker.launch_record().await.unwrap().unwrap();
    assert_eq!(restored.receipt, receipt);
    assert!(restored.acceptance.is_none());
    assert_eq!(restored.input, input);
    session
        .bind_entry(
            entry.address.instance_id.clone(),
            executor(),
            authority,
            Default::default(),
            NonZeroUsize::new(16).unwrap(),
        )
        .await
        .unwrap();
    host.wait_quiescent().await.unwrap();
    assert!(worker.snapshot().await.unwrap().last_invocation.is_none());
    for text in ["first real input", "second real input"] {
        worker
            .accept_input(
                DeliveryRequest {
                    request_id: Uuid::now_v7(),
                    sender: input.request.sender.clone(),
                    recipient: entry.address.clone(),
                    delivery: DeliveryMode::NextTurn,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload: InboxPayload::UserSubmission {
                        content: atman_runtime::flow_store::inbox::UserContent {
                            text: text.into(),
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
        tokio::time::timeout(Duration::from_secs(10), host.wait_quiescent())
            .await
            .unwrap()
            .unwrap();
        let snapshot = worker.snapshot().await.unwrap();
        let invocation = snapshot.last_invocation.unwrap();
        if text == "first real input" {
            assert_eq!(invocation.lease.turn, first_turn);
            assert_eq!(invocation.lease.run, first_run);
        } else {
            assert_ne!(invocation.lease.turn, first_turn);
        }
        assert_eq!(worker.planning().await.unwrap().todos.len(), 1);
        assert_eq!(
            worker
                .context()
                .messages_full()
                .last()
                .unwrap()
                .text_concat(),
            text
        );
    }
    let retry = host
        .launch_flow(entry, input, |_, _| {
            panic!("accepted activation reauthorized")
        })
        .await
        .unwrap();
    assert_eq!(retry.record.receipt, receipt);
    assert!(retry.record.acceptance.is_none());
    host.shutdown().await.unwrap();
}

#[test]
fn waiting_activation_and_initial_goal_share_atomic_recovery_without_an_inbox_receipt() {
    use atman_runtime::flow_store::launch::{EntryBinding, EntryStart};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_info();
    let mut session = root.create_session(info.clone()).unwrap();
    let entry = descriptor(info.id);
    let mut store = session.create_flow(entry.clone()).unwrap();
    let mut input = initial(&entry, &entry.address);
    input.request.start = EntryStart::Waiting;
    input.result_route = None;
    input.initial_goal = Some("Wait for the user's work".into());
    input.execution = Some(EntryBinding {
        user_routes: false,
        session_start: false,
        first_turn: TurnId::now(),
        first_run: RunId::now(),
        authority: EffectiveAuthority::root(&Default::default(), false, None),
    });
    let path = store.dir().join("events.jsonl");
    let prefix = std::fs::metadata(&path).unwrap().len();
    let record = store
        .activate_entry(input.clone(), None, |_| Ok(()))
        .unwrap();
    assert!(record.acceptance.is_none());
    assert!(store.pending_inputs().next().is_none());
    assert!(store.goal().is_some());
    drop(store);
    let complete = std::fs::read(&path).unwrap();
    std::fs::write(&path, &complete[..complete.len() - 1]).unwrap();
    let mut store = session.open_flow(&entry.address.instance_id).unwrap();
    assert_eq!(store.state().state(), &InstanceState::Prepared);
    assert!(store.goal().is_none() && store.launch_record().is_none());
    assert!(store.pending_inputs().next().is_none());
    assert_eq!(std::fs::metadata(&path).unwrap().len(), prefix);
    let record = store.activate_entry(input, None, |_| Ok(())).unwrap();
    assert!(record.acceptance.is_none());
    drop(store);
    let store = session.open_flow(&entry.address.instance_id).unwrap();
    assert_eq!(store.launch_record().unwrap().receipt, record.receipt);
    assert!(store.goal().is_some());
    assert!(store.inbox_entries().is_empty());
}

#[tokio::test]
async fn live_output_tracks_execution_and_clears_before_committed_final_delivery() {
    use atman_runtime::flow_runtime::live::DraftPart;
    let directory = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(directory.path()).unwrap();
    let info = session_info();
    let mut session = root.create_session(info.clone()).unwrap();
    let mut entry = descriptor(info.id);
    entry.entry_mut().unwrap().bundle.sources.insert(
        "entry:main.at".into(),
        r#"
        flow main(label: string, amount: int) -> string {
            reply = llm.call(model: "mock", context: "session", record: false)
            return final.answer(message: reply)
        }
    "#
        .into(),
    );
    let mut store = session.create_flow(entry.clone()).unwrap();
    store
        .transition(store.state().revision(), FlowTransition::Activated)
        .unwrap();
    let mut request = initial(&entry, &entry.address)
        .request
        .initial_input()
        .unwrap();
    request.sender = SenderIdentity::User(Uuid::now_v7());
    store.accept_input(request.clone(), |_| Ok(())).unwrap();
    let worker = FlowWorker::start(store).await.unwrap();
    let mut reader = worker.subscribe_live_output();
    assert!(reader.snapshot().calls.is_empty());
    let answer = "active preview ".repeat(30);
    let models = Arc::new(atman_runtime::model_registry::ModelRegistry::new());
    models
        .replace_config(atman_runtime::model_registry::ProviderConfig {
            providers: [(
                "mock".into(),
                atman_runtime::model_registry::ProviderEntry {
                    name: "mock".into(),
                    kind: "openai-compat".into(),
                    ..Default::default()
                },
            )]
            .into(),
            models: [(
                "mock".into(),
                atman_runtime::model_registry::ModelEntry {
                    model: "mock".into(),
                    provider: Some("mock".into()),
                    context_budget: Some(100_000),
                    ..Default::default()
                },
            )]
            .into(),
            ..Default::default()
        })
        .unwrap();
    let mut executor = Executor::new();
    executor.providers = atman_runtime::provider::ProviderRegistry::with_models(models);
    executor.providers.register(Arc::new(
        atman_runtime::providers::mock::MockProvider::new("mock")
            .with_fallback(atman_runtime::Value::Str(answer.clone()))
            .with_chunk_delay(Duration::from_millis(2)),
    ));
    let ticket = worker
        .start_turn(
            executor.clone(),
            prepare(&worker.snapshot().await.unwrap()).unwrap().unwrap(),
        )
        .await
        .unwrap();
    let snapshot = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            reader.changed().await;
            let snapshot = reader.snapshot();
            if snapshot.calls.iter().any(|call| {
                call.parts
                    .iter()
                    .any(|part| matches!(part, DraftPart::Text(text) if !text.is_empty()))
            }) {
                break snapshot;
            }
        }
    })
    .await;
    if snapshot.is_err() {
        panic!(
            "preview absent: {:?}; {:?}",
            worker.status().await,
            worker.completion(None).await
        );
    }
    let snapshot = snapshot.unwrap();
    assert_eq!(snapshot.flow, *worker.address());
    assert_eq!(snapshot.calls.len(), 1);
    assert_eq!(snapshot.calls[0].lease, ticket.lease);
    assert!(
        worker
            .context()
            .messages()
            .iter()
            .all(|message| message.origin != MessageOrigin::FinalAnswer)
    );
    let cursor = worker.status().await.unwrap().cursor;
    let mut attached = worker.subscribe_live_output();
    assert_eq!(attached.snapshot().calls[0].id, snapshot.calls[0].id);
    assert!(!reader.snapshot().calls.is_empty());
    assert_eq!(
        worker.status().await.unwrap().cursor,
        cursor,
        "reading a preview must not write storage"
    );
    let done = tokio::time::timeout(Duration::from_secs(5), ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(done.result.is_ok(), "{:?}", done.result);
    assert!(reader.snapshot().calls.is_empty());
    assert!(attached.snapshot().calls.is_empty());
    let messages = worker.context().messages();
    let finals = messages
        .iter()
        .filter(|message| message.origin == MessageOrigin::FinalAnswer)
        .collect::<Vec<_>>();
    assert_eq!(finals.len(), 1);
    assert_eq!(finals[0].text_concat(), answer);

    request.request_id = Uuid::now_v7();
    worker.accept_input(request, |_| Ok(())).await.unwrap();
    let cancelled = worker
        .start_turn(
            executor,
            prepare(&worker.snapshot().await.unwrap()).unwrap().unwrap(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            reader.changed().await;
            if reader.snapshot().calls.iter().any(|call| {
                call.parts
                    .iter()
                    .any(|part| matches!(part, DraftPart::Text(_)))
            }) {
                break;
            }
        }
    })
    .await
    .unwrap();
    change(
        &worker,
        FlowTransition::PhaseChanged {
            lease: cancelled.lease.clone(),
            phase: atman_rt::TurnPhase::Cancelling,
        },
    )
    .await;
    let done = tokio::time::timeout(Duration::from_secs(5), cancelled.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(
        matches!(
            done.result,
            Err(atman_runtime::error::RuntimeError::Cancelled(_))
        ),
        "{:?}",
        done.result
    );
    assert!(reader.snapshot().calls.is_empty());
    assert!(attached.snapshot().calls.is_empty());
    assert_eq!(
        worker
            .context()
            .messages()
            .iter()
            .filter(|message| message.origin == MessageOrigin::FinalAnswer)
            .count(),
        1
    );
    worker.shutdown().await.unwrap();
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
        scope: None,
        workspace: None,
        initial_goal: None,
        execution: None,
        request: atman_runtime::flow_store::launch::EntryRequest {
            request_id: info.creation_request,
            sender: SenderIdentity::Flow(recipient.clone()),
            recipient: info.address.clone(),
            arguments: [
                ("label".into(), "first answer".into()),
                ("amount".into(), 42.into()),
            ]
            .into(),
            invocation: [("effort".into(), "high".into())].into(),
            start: atman_runtime::flow_store::launch::EntryStart::Invoke { user: None },
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
    let atman_runtime::flow_store::launch::EntryStart::Invoke { user } = &mut input.request.start
    else {
        unreachable!()
    };
    *user = Some(content.clone());
    input.result_route = None;
    input.execution = Some(EntryBinding {
        user_routes: false,
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
        launched.record.acceptance.as_ref().unwrap().receipt,
        repeated.record.acceptance.as_ref().unwrap().receipt
    );
    assert_eq!(
        launched.record.acceptance.as_ref().unwrap().message_id,
        repeated.record.acceptance.as_ref().unwrap().message_id
    );
    let receipt = launched.record.acceptance.clone().unwrap();
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
    assert_eq!(
        human
            .parts
            .iter()
            .filter_map(|part| match part {
                atman_runtime::message::MessagePart::Image { source } => Some(source),
                _ => None,
            })
            .collect::<Vec<_>>(),
        content.images.iter().collect::<Vec<_>>()
    );
    assert!(human.parts.iter().any(|part| matches!(part, atman_runtime::message::MessagePart::InputPresentation { presentation } if Some(presentation) == content.presentation.as_ref())));
    let atman_runtime::flow_store::launch::EntryStart::Invoke { user } = worker
        .launch_record()
        .await
        .unwrap()
        .unwrap()
        .input
        .request
        .start
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
    assert_eq!(
        retry.record.acceptance.as_ref().unwrap().message_id,
        receipt.message_id
    );
    assert_eq!(
        retry.record.acceptance.as_ref().unwrap().receipt,
        receipt.receipt
    );
    assert!(matches!(
        retry.record.acceptance.as_ref().unwrap().disposition,
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
        user_routes: false,
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
    let mut invalid_mid_turn = input.request.initial_input().unwrap();
    invalid_mid_turn.recipient = recipient.state().address().clone();
    invalid_mid_turn.delivery = DeliveryMode::AtBoundary;
    assert!(
        recipient
            .accept_input(invalid_mid_turn, |_| Ok(()))
            .is_err()
    );
    assert!(
        source
            .activate_entry(input.clone(), None, |_| Ok(()))
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
        user_routes: false,
        session_start: false,
        first_turn: TurnId(Uuid::nil().into()),
        first_run: RunId::now(),
        authority: EffectiveAuthority::root(&Default::default(), false, None),
    });
    assert!(
        source
            .activate_entry(invalid_binding, Some(proof.clone()), |_| Ok(()))
            .is_err()
    );
    assert_eq!(source.state().state(), &InstanceState::Prepared);
    assert!(source.pending_inputs().next().is_none());
    let pause = Uuid::new_v4().into();
    let mut peer_start = input.clone();
    peer_start.execution = Some(atman_runtime::flow_store::launch::EntryBinding {
        user_routes: false,
        session_start: true,
        first_turn: TurnId::now(),
        first_run: RunId::now(),
        authority: EffectiveAuthority::root(&Default::default(), false, None),
    });
    assert!(
        source
            .activate_entry(peer_start, Some(proof.clone()), |_| Ok(()))
            .is_err()
    );
    let mut invalid_goal = input.clone();
    invalid_goal.initial_goal = Some("  ".into());
    assert!(
        source
            .activate_entry(invalid_goal, Some(proof.clone()), |_| Ok(()))
            .is_err()
    );
    assert_eq!(source.state().state(), &InstanceState::Prepared);
    assert!(source.goal().is_none());
    let activation_start = std::fs::metadata(source.dir().join("events.jsonl"))
        .unwrap()
        .len() as usize;
    let committed = source
        .activate_entry(input.clone(), Some(proof.clone()), |_| Ok(()))
        .unwrap();
    assert_eq!(
        source.cursor(),
        &committed.acceptance.as_ref().unwrap().receipt
    );
    let goal = source.goal().cloned().unwrap();
    assert_eq!(goal.objective, "Finish the requested work");
    assert_eq!(goal.revision, 1);
    assert_eq!(
        source.goal_updates()[0].receipt,
        committed.acceptance.as_ref().unwrap().receipt
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
            .activate_entry(input.clone(), None, |_| panic!("retry reauthorized"))
            .unwrap()
            .acceptance,
        committed.acceptance
    );
    assert_eq!(source.state().pauses(), &[pause]);
    let mut changed_goal = input.clone();
    changed_goal.initial_goal = Some("different objective".into());
    assert!(
        source
            .activate_entry(changed_goal, None, |_| Ok(()))
            .is_err()
    );
    assert_eq!(source.goal(), Some(&goal));
    let mut changed = input.clone();
    changed.inherited_context.clear();
    assert!(source.activate_entry(changed, None, |_| Ok(())).is_err());
    source
        .transition(source.state().revision(), FlowTransition::CloseRequested)
        .unwrap();
    assert!(matches!(
        source
            .activate_entry(input.clone(), None, |_| panic!("closed retry reauthorized"))
            .unwrap()
            .acceptance
            .unwrap()
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
        .activate_entry(input, Some(proof), |_| Ok(()))
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
        first.record.acceptance.as_ref().unwrap().receipt,
        retry.record.acceptance.as_ref().unwrap().receipt
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
    let mut transcript =
        atman_runtime::projection::flow::FlowTranscript::new(source.address().clone());
    loop {
        let page = source
            .read_updates(transcript.cursor().cloned(), NonZeroUsize::new(2).unwrap())
            .await
            .unwrap();
        for batch in page.batches {
            transcript.apply(batch).unwrap();
        }
        if !page.has_more {
            break;
        }
    }
    let conversation = transcript
        .entries()
        .iter()
        .flat_map(|entry| entry.record.shared_messages())
        .collect::<Vec<_>>();
    assert_eq!(conversation.len(), 2);
    assert_eq!(conversation[0].origin, MessageOrigin::Internal);
    assert_eq!(
        conversation
            .iter()
            .filter(|message| message.is_visible_assistant_reply())
            .map(|message| message.text_concat())
            .collect::<Vec<_>>(),
        ["first answer"]
    );
    assert!(transcript.entries().iter().any(|entry| matches!(&entry.record, atman_runtime::projection::flow::FlowRecord::Execution(event) if event.scope.flow() == source.address())));
    assert_eq!(transcript.turns().count(), 1);
    let mut observed_runs = std::collections::HashSet::new();
    for entry in transcript.entries() {
        if let atman_runtime::projection::flow::FlowRecord::Execution(record) = &entry.record
            && let atman_runtime::event::Event::FlowStart { run_id, .. } = &record.event
            && observed_runs.insert(run_id.clone())
        {
            let owner = record.scope.lease().unwrap();
            let turn = transcript
                .turns()
                .find(|turn| turn.lease() == owner)
                .unwrap();
            assert_eq!(
                turn.workflow()
                    .find_node(&run_id.0.to_string())
                    .unwrap()
                    .started_at,
                Some(record.timestamp)
            );
        }
    }
    assert!(!observed_runs.is_empty());
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
            .unwrap()
            .disposition,
        InboxDisposition::InputCommitted { .. }
    ));
    let receipt = first.record.acceptance.as_ref().unwrap().receipt.clone();
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
    assert_eq!(
        restored.record.acceptance.as_ref().unwrap().receipt,
        receipt
    );
    assert!(restored.worker.snapshot().await.unwrap().pending.is_empty());
    let mut changed = input;
    changed.request.arguments.insert("amount".into(), 43.into());
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
    assert_eq!(
        retried.record.acceptance.as_ref().unwrap().receipt,
        stored.acceptance.as_ref().unwrap().receipt
    );
}
