use std::{collections::BTreeMap, io, num::NonZeroUsize, sync::Arc, time::Duration};

use atman_rt::{InstanceActivity, InstanceState, RunId, TurnId, TurnOutcome};
use atman_runtime::flow_runtime::access::PeerControl;
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

fn control_request(
    worker: &FlowWorker,
    expected_revision: u64,
    pause_key: atman_runtime::event::AtmanUuid,
    action: atman_runtime::flow_runtime::access::PeerControl,
) -> atman_runtime::flow_store::control::ControlRequest {
    atman_runtime::flow_store::control::ControlRequest {
        id: Uuid::new_v4(),
        sender: SenderIdentity::User(Uuid::from_bytes(*b"atman-local-user")),
        target: worker.address().clone(),
        expected_revision,
        pause_key,
        action,
    }
}

fn session_descriptor() -> SessionDescriptor {
    SessionDescriptor {
        configuration: Default::default(),
        id: Uuid::new_v4(),
        title: "Workspace".into(),
        project: None,
    }
}

#[tokio::test]
async fn process_source_directory_does_not_open_or_recover_detached_instances() {
    use std::io::Write;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut store = root.create_session(info.clone()).unwrap();
    let mut flow = store.create_flow(flow_descriptor(info.id)).unwrap();
    let id = flow.descriptor().address.instance_id.clone();
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
    let log = flow.dir().join("events.jsonl");
    drop(flow);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(b"unfinished tail")
        .unwrap();
    let before = std::fs::read(&log).unwrap();
    let host = RuntimeHost::default();
    let session = host.attach_session(store).unwrap();
    assert!(
        session
            .process_watch_sources(id.clone())
            .await
            .unwrap()
            .is_empty()
    );
    assert!(session.observe().await.unwrap().loaded.is_empty());
    assert!(session.scheduling_status(id).await.unwrap().is_none());
    assert_eq!(
        session
            .process_watch_sources(FlowInstanceId::now())
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::NotFound
    );
    host.shutdown().await.unwrap();
    assert_eq!(std::fs::read(log).unwrap(), before);
}

#[tokio::test]
async fn session_titles_publish_only_committed_changes_without_loading_flows() {
    use atman_runtime::flow_runtime::catalog::SessionCatalog;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut store = root.create_session(info.clone()).unwrap();
    let flow = store.create_flow(flow_descriptor(info.id)).unwrap();
    let flow_log = flow.dir().join("events.jsonl");
    let flow_before = std::fs::read(&flow_log).unwrap();
    drop(flow);
    let host = RuntimeHost::default();
    let session = host.attach_session(store).unwrap();
    let catalog = SessionCatalog::new(Some(host.clone()), Some(dir.path().to_owned()));
    let mut first = session.subscribe_title();
    let mut second = session.subscribe_title();
    assert_eq!(first.borrow_and_update().as_deref(), Some("Workspace"));
    second.borrow_and_update();
    let initial = session.snapshot().await.unwrap().cursor;
    let renamed = session
        .set_title(initial.clone(), Some("Renamed".into()))
        .await
        .unwrap();
    first.changed().await.unwrap();
    second.changed().await.unwrap();
    assert_eq!(first.borrow_and_update().as_deref(), Some("Renamed"));
    assert_eq!(second.borrow_and_update().as_deref(), Some("Renamed"));
    assert_eq!(session.snapshot().await.unwrap().catalog.descriptor, info);
    assert_eq!(
        catalog
            .overview(info.id)
            .await
            .unwrap()
            .summary
            .title
            .as_deref(),
        Some("Renamed")
    );
    assert_eq!(
        session
            .set_title(initial.clone(), Some("Renamed".into()))
            .await
            .unwrap(),
        renamed
    );
    assert!(!first.has_changed().unwrap());
    assert!(
        session
            .set_title(initial, Some("Stale".into()))
            .await
            .is_err()
    );
    assert!(!second.has_changed().unwrap());
    drop(first);
    drop(second);
    session.set_title(renamed, None).await.unwrap();
    let mut reattached = session.subscribe_title();
    assert!(reattached.borrow_and_update().is_none());
    let view = catalog.overview(info.id).await.unwrap();
    assert!(view.summary.title.is_none());
    assert_eq!(view.summary.loaded_flows, Some(0));
    assert_eq!(std::fs::read(&flow_log).unwrap(), flow_before);
    host.shutdown().await.unwrap();
    assert!(reattached.changed().await.is_err());
    let detached = SessionCatalog::new(None, Some(dir.path().to_owned()))
        .overview(info.id)
        .await
        .unwrap();
    assert!(detached.summary.title.is_none());
    assert_eq!(std::fs::read(&flow_log).unwrap(), flow_before);
}

#[tokio::test]
async fn session_discovery_requires_container_authority_and_preserves_detached_execution() {
    use atman_runtime::flow_store::{
        access::{AccessTarget, FlowAction, FlowGrant},
        profile::ProfileStore,
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let profile = host
        .attach_profile(ProfileStore::open(&root).unwrap())
        .unwrap();
    assert!(profile.snapshot().await.unwrap().unavailable.is_none());
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let caller = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    change(&caller, FlowTransition::Activated).await;
    let lease = start(&caller).await;
    let client = host.bind_invocation(lease.clone()).await.unwrap();
    let remote_info = session_descriptor();
    let mut detached = root.create_session(remote_info.clone()).unwrap();
    let mut target = detached
        .create_flow(flow_descriptor(remote_info.id))
        .unwrap();
    target
        .transition(target.state().revision(), FlowTransition::Activated)
        .unwrap();
    target
        .transition(
            target.state().revision(),
            FlowTransition::TurnStarted {
                turn: TurnId::now(),
                run: RunId::now(),
            },
        )
        .unwrap();
    let flow_log = target.dir().join("events.jsonl");
    let flow_bytes = std::fs::read(&flow_log).unwrap();
    let hidden = root.create_session(session_descriptor()).unwrap();
    std::fs::write(hidden.dir().join("events.jsonl"), b"corrupt\n").unwrap();
    let single = FlowGrant {
        id: Uuid::new_v4(),
        grantee: caller.address().clone(),
        target: AccessTarget::Flow(target.state().address().clone()),
        actions: [FlowAction::Discover].into(),
    };
    session
        .grant_access(session.snapshot().await.unwrap().cursor, single)
        .await
        .unwrap();
    let own = client.sessions(None, 50).await.unwrap();
    assert_eq!(own.total, 1);
    assert_eq!(own.sessions[0].session_id, info.id);
    assert!(client.instances_in(remote_info.id, None, 50).await.is_err());
    let grant = FlowGrant {
        id: Uuid::new_v4(),
        grantee: caller.address().clone(),
        target: AccessTarget::Session(remote_info.id),
        actions: [FlowAction::Discover].into(),
    };
    session
        .grant_access(session.snapshot().await.unwrap().cursor, grant.clone())
        .await
        .unwrap();
    let first = client.sessions(None, 1).await.unwrap();
    let second = client.sessions(first.next, 1).await.unwrap();
    assert_eq!(first.total, 2);
    assert_eq!(second.total, 2);
    assert!(first.sessions[0].session_id > second.sessions[0].session_id);
    assert!(second.next.is_none());
    let page = client.instances_in(remote_info.id, None, 1).await.unwrap();
    assert_eq!(page.total, 1);
    assert!(!page.instances[0].loaded);
    assert_eq!(page.instances[0].state.as_ref(), Some(target.state()));
    assert_eq!(std::fs::read(&flow_log).unwrap(), flow_bytes);
    assert!(host.session(remote_info.id).is_err());
    assert!(client.sessions(None, 0).await.is_err());
    assert!(client.sessions(None, 51).await.is_err());
    let key = Uuid::new_v4().into();
    change(&caller, FlowTransition::ExecutionPaused { key }).await;
    assert!(client.sessions(None, 50).await.is_err());
    assert!(client.instances_in(remote_info.id, None, 1).await.is_err());
    change(&caller, FlowTransition::ExecutionResumed { key }).await;
    assert_eq!(client.sessions(None, 50).await.unwrap().total, 2);
    session
        .revoke_access(session.snapshot().await.unwrap().cursor, grant.id)
        .await
        .unwrap();
    assert_eq!(client.sessions(None, 50).await.unwrap().total, 1);
    assert!(client.instances_in(remote_info.id, None, 1).await.is_err());
    change(
        &caller,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    )
    .await;
    assert!(client.sessions(None, 50).await.is_err());
}

#[tokio::test]
async fn session_discovery_tools_share_management_grants_and_return_qualified_handles() {
    use atman_runtime::ValueJson;
    use atman_runtime::flow_store::{
        access::{AccessTarget, FlowAction, FlowGrant},
        profile::{ManagementBinding, ProfileStore, ReplaceManagement},
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let mut store = ProfileStore::open(&root).unwrap();
    let info = store
        .bootstrap(Default::default())
        .unwrap()
        .management
        .unwrap()
        .session;
    let host = RuntimeHost::default();
    let profile = host.attach_profile(store).unwrap();
    let session = host
        .attach_session(root.open_session(info.id).unwrap())
        .unwrap();
    let remote_info = session_descriptor();
    let mut remote = root.create_session(remote_info.clone()).unwrap();
    let target = remote.create_flow(flow_descriptor(remote_info.id)).unwrap();
    let address = target.state().address().clone();
    let bytes = std::fs::read(target.dir().join("events.jsonl")).unwrap();
    let damaged = root.create_session(session_descriptor()).unwrap();
    std::fs::write(damaged.dir().join("events.jsonl"), b"corrupt\n").unwrap();
    let source = format!(
        r#"
        flow main() -> any {{
            sessions = flow.sessions()
            remote = flow.instances(session_id: "{}")
            local = flow.instances()
            return [sessions, remote, local]
        }}
    "#,
        remote_info.id
    );
    let caller = session
        .create_flow(program_descriptor(info.id, source))
        .await
        .unwrap();
    let request = ReplaceManagement {
        request_id: Uuid::new_v4(),
        expected: profile.snapshot().await.unwrap().profile.cursor,
        binding: ManagementBinding {
            session: info,
            grants: vec![FlowGrant {
                id: Uuid::new_v4(),
                grantee: caller.address().clone(),
                target: AccessTarget::AllSessions,
                actions: [FlowAction::Discover].into(),
            }],
        },
    };
    profile.replace(request).await.unwrap();
    change(&caller, FlowTransition::Activated).await;
    let result = invoke(&caller, Executor::new())
        .await
        .result
        .unwrap()
        .to_json()
        .unwrap();
    assert_eq!(result[0]["total"], 3);
    let rows = result[0]["sessions"].as_array().unwrap();
    assert!(rows.iter().any(|row| row["status"] == "unavailable"));
    assert!(rows.iter().any(|row| row["status"] == "running"));
    assert!(rows.iter().any(|row| row["status"] == "detached"));
    assert!(
        rows.iter()
            .all(|row| row.get("goal").is_none() && row.get("configuration").is_none())
    );
    assert_eq!(result[1]["session_id"], remote_info.id.to_string());
    let handle = result[1]["instances"][0]["handle"].as_str().unwrap();
    let lease = start(&caller).await;
    let client = host.bind_invocation(lease.clone()).await.unwrap();
    assert_eq!(client.resolve_address(handle).unwrap(), address);
    assert_eq!(result[1]["instances"][0]["status"], "detached");
    assert_eq!(result[1]["instances"][0]["recorded_status"], "prepared");
    assert!(result[1]["spawn_token"].is_null());
    assert!(
        result[2]["spawn_token"]
            .as_str()
            .unwrap()
            .starts_with("spawn_")
    );
    assert_eq!(
        std::fs::read(target.dir().join("events.jsonl")).unwrap(),
        bytes
    );
    assert!(host.session(remote_info.id).is_err());
    change(
        &caller,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    )
    .await;
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn management_authority_is_explicit_revocable_and_shared_with_observers() {
    use atman_runtime::flow_store::{
        access::{AccessTarget, FlowAction, FlowGrant},
        profile::{ManagementBinding, ProfileStore, ReplaceManagement},
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let mut store = ProfileStore::open(&root).unwrap();
    let descriptor = store
        .bootstrap(Default::default())
        .unwrap()
        .management
        .unwrap()
        .session;
    let host = RuntimeHost::default();
    let profile = host.attach_profile(store).unwrap();
    assert!(profile.snapshot().await.unwrap().unavailable.is_none());
    let session = host
        .attach_session(root.open_session(descriptor.id).unwrap())
        .unwrap();
    let remote_info = SessionDescriptor {
        title: "Atman".into(),
        ..session_descriptor()
    };
    let remote = host
        .attach_session(root.create_session(remote_info.clone()).unwrap())
        .unwrap();
    let manager = session
        .create_flow(flow_descriptor(descriptor.id))
        .await
        .unwrap();
    let sibling = session
        .create_flow(flow_descriptor(descriptor.id))
        .await
        .unwrap();
    let source = remote
        .create_flow(flow_descriptor(remote_info.id))
        .await
        .unwrap();
    for worker in [&manager, &sibling, &source] {
        change(worker, FlowTransition::Activated).await;
    }
    let manager_lease = start(&manager).await;
    let client = host.bind_invocation(manager_lease.clone()).await.unwrap();
    let sibling_lease = start(&sibling).await;
    let sibling_client = host.bind_invocation(sibling_lease.clone()).await.unwrap();
    assert!(client.inspect(source.address().clone()).await.is_err());
    let grants = vec![FlowGrant {
        id: Uuid::new_v4(),
        grantee: manager.address().clone(),
        target: AccessTarget::AllSessions,
        actions: [
            FlowAction::Discover,
            FlowAction::Watch,
            FlowAction::Send,
            FlowAction::ReadOutput,
        ]
        .into(),
    }];
    let request = ReplaceManagement {
        request_id: Uuid::new_v4(),
        expected: profile.snapshot().await.unwrap().profile.cursor,
        binding: ManagementBinding {
            session: descriptor.clone(),
            grants,
        },
    };
    profile.replace(request.clone()).await.unwrap();
    assert!(client.inspect(source.address().clone()).await.is_ok());
    assert!(
        sibling_client
            .inspect(source.address().clone())
            .await
            .is_err()
    );
    assert!(client.read_context(source.address().clone()).await.is_err());
    client.watch_events(answer_watch(&source)).await.unwrap();
    assert!(invoke(&source, Executor::new()).await.result.is_ok());
    wait_inputs(&manager, 1).await;

    let remove = ReplaceManagement {
        request_id: Uuid::new_v4(),
        expected: profile.snapshot().await.unwrap().profile.cursor,
        binding: ManagementBinding {
            session: remote_info,
            grants: Vec::new(),
        },
    };
    let removed = profile.replace(remove).await.unwrap();
    assert!(client.inspect(source.address().clone()).await.is_err());
    assert!(client.watch_events(answer_watch(&source)).await.is_err());
    assert!(
        client
            .send(
                source.address().clone(),
                Uuid::new_v4(),
                "revoked".into(),
                DeliveryMode::AtBoundary
            )
            .await
            .is_err()
    );
    let mut issues = manager.subscribe_observation_issues();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if !issues.borrow_and_update().is_empty() {
                break;
            }
            issues.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(invoke(&source, Executor::new()).await.result.is_ok());
    assert_eq!(manager.snapshot().await.unwrap().pending.len(), 1);
    profile.replace(request).await.unwrap();
    assert_eq!(profile.snapshot().await.unwrap().profile.cursor, removed);
    assert!(client.inspect(source.address().clone()).await.is_err());

    // Restore the original manager explicitly, then pause it. Remote execution stays independent.
    let current = profile.snapshot().await.unwrap();
    profile
        .replace(ReplaceManagement {
            request_id: Uuid::new_v4(),
            expected: current.profile.cursor,
            binding: ManagementBinding {
                session: descriptor,
                grants: vec![FlowGrant {
                    id: Uuid::new_v4(),
                    grantee: manager.address().clone(),
                    target: AccessTarget::AllSessions,
                    actions: [FlowAction::Discover, FlowAction::Watch].into(),
                }],
            },
        })
        .await
        .unwrap();
    change(
        &manager,
        FlowTransition::ExecutionPaused {
            key: Uuid::new_v4().into(),
        },
    )
    .await;
    assert!(client.inspect(source.address().clone()).await.is_err());
    assert!(invoke(&source, Executor::new()).await.result.is_ok());
    wait_inputs(&manager, 3).await;
    for (worker, lease) in [(&manager, manager_lease), (&sibling, sibling_lease)] {
        change(
            worker,
            FlowTransition::TurnFinished {
                lease,
                outcome: TurnOutcome::Completed,
            },
        )
        .await;
    }
    host.shutdown().await.unwrap();
    assert!(profile.snapshot().await.is_err());
    let store = ProfileStore::open(&root).unwrap();
    assert!(host.attach_profile(store).is_err());
    let reopened = RuntimeHost::default();
    let profile = reopened
        .attach_profile(ProfileStore::open(&root).unwrap())
        .unwrap();
    assert_eq!(
        profile
            .snapshot()
            .await
            .unwrap()
            .profile
            .management
            .unwrap()
            .grants
            .len(),
        1
    );
    assert!(reopened.sessions().is_empty());
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn unavailable_management_binding_can_be_explicitly_repaired_without_starting_work() {
    use atman_runtime::flow_store::profile::{ManagementBinding, ProfileStore, ReplaceManagement};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let mut store = ProfileStore::open(&root).unwrap();
    let original = store
        .bootstrap(Default::default())
        .unwrap()
        .management
        .unwrap()
        .session;
    std::fs::remove_dir_all(root.root().join("sessions").join(original.id.to_string())).unwrap();
    let host = RuntimeHost::default();
    let profile = host.attach_profile(store).unwrap();
    let invalid = profile.snapshot().await.unwrap();
    assert!(invalid.unavailable.is_some());
    assert!(profile.bootstrap(Default::default()).await.is_err());
    let replacement = session_descriptor();
    root.create_session(replacement.clone()).unwrap();
    profile
        .replace(ReplaceManagement {
            request_id: Uuid::new_v4(),
            expected: invalid.profile.cursor,
            binding: ManagementBinding {
                session: replacement.clone(),
                grants: Vec::new(),
            },
        })
        .await
        .unwrap();
    let repaired = profile.snapshot().await.unwrap();
    assert!(repaired.unavailable.is_none());
    assert_eq!(repaired.profile.management.unwrap().session, replacement);
    assert!(host.sessions().is_empty());
    host.shutdown().await.unwrap();
}

async fn wait_closed(worker: &FlowWorker) {
    let mut state = worker.subscribe_state();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if state.borrow_and_update().state() == &InstanceState::Closed {
                return;
            }
            state.changed().await.unwrap();
        }
    })
    .await
    .expect("accepted close must finish cleanup");
}

#[tokio::test]
async fn accepted_close_unwinds_paused_execution_and_outlives_its_waiter() {
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
            "flow main() { loop { yield } }".into(),
        ))
        .await
        .unwrap();
    let peer = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    for worker in [&worker, &peer] {
        change(worker, FlowTransition::Activated).await;
    }
    worker
        .accept_input(peer_request(peer.address(), worker.address()), |_| Ok(()))
        .await
        .unwrap();
    let request = prepare(&worker.snapshot().await.unwrap()).unwrap().unwrap();
    let ticket = worker.start_turn(Executor::new(), request).await.unwrap();
    let key = Uuid::now_v7().into();
    change(&worker, FlowTransition::ExecutionPaused { key }).await;
    let queued = peer_request(peer.address(), worker.address());
    let accepted = worker
        .accept_input(queued.clone(), |_| Ok(()))
        .await
        .unwrap();
    let before = worker.state().await.unwrap();
    assert!(
        worker
            .control(
                control_request(
                    &worker,
                    before.revision(),
                    Uuid::from_bytes(*b"atman-user-pause").into(),
                    PeerControl::Close
                ),
                |_| Err(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            )
            .await
            .is_err()
    );
    assert!(
        worker
            .control(
                control_request(
                    &worker,
                    before.revision() - 1,
                    Uuid::from_bytes(*b"atman-user-pause").into(),
                    PeerControl::Close
                ),
                |_| Ok(())
            )
            .await
            .is_err()
    );
    assert_eq!(worker.state().await.unwrap(), before);

    let (sent, received) = tokio::sync::oneshot::channel();
    let owned = worker.clone();
    let close_request = control_request(&worker, before.revision(), key, PeerControl::Close);
    let submitted = close_request.clone();
    let closing = tokio::spawn(async move {
        owned
            .control(submitted, move |_| {
                let _ = sent.send(());
                Ok(())
            })
            .await
    });
    received.await.unwrap();
    closing.abort();
    let _ = closing.await;
    wait_closed(&worker).await;
    assert_eq!(
        ticket.wait().await.unwrap().outcome,
        TurnOutcome::Interrupted(atman_rt::Interruption::Cancelled)
    );
    assert!(worker.snapshot().await.unwrap().pending.is_empty());
    assert!(
        worker
            .accept_input(peer_request(peer.address(), worker.address()), |_| Ok(()))
            .await
            .is_err()
    );
    let retry = worker
        .accept_input(queued, |_| {
            panic!("recorded delivery retry is not new work")
        })
        .await
        .unwrap();
    assert_eq!(retry.message_id, accepted.message_id);
    assert_eq!(retry.receipt, accepted.receipt);
    assert!(matches!(
        retry.disposition,
        InboxDisposition::Cancelled { .. }
    ));
    let closed = worker.state().await.unwrap();
    let accepted_close = worker
        .control(close_request.clone(), |_| Ok(()))
        .await
        .unwrap();
    assert!(matches!(
        accepted_close.execution.state(),
        InstanceState::Closing { .. }
    ));
    assert_eq!(worker.state().await.unwrap(), closed);
    assert!(
        worker
            .control(close_request.clone(), |_| {
                Err(io::Error::new(io::ErrorKind::PermissionDenied, "revoked"))
            })
            .await
            .is_err()
    );
    assert_eq!(worker.state().await.unwrap(), closed);
    let receipt = worker
        .control(
            control_request(
                &worker,
                closed.revision(),
                Uuid::from_bytes(*b"atman-user-pause").into(),
                PeerControl::Close,
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    assert_eq!(
        worker
            .control(receipt.update.request.clone(), |_| Ok(()))
            .await
            .unwrap(),
        receipt
    );
    assert!(peer.state().await.unwrap().can_start_turn());
    host.shutdown().await.unwrap();
    let mut saved = root
        .open_session(info.id)
        .unwrap()
        .open_flow(&worker.address().instance_id)
        .unwrap();
    assert_eq!(saved.state().state(), &InstanceState::Closed);
    assert_eq!(saved.completions().len(), 1);
    assert_eq!(saved.control(close_request).unwrap(), accepted_close);
}

#[tokio::test]
async fn close_tool_can_close_its_own_instance_without_waiting_on_its_turn() {
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
            r#"
        flow main() -> any {
            contract { capabilities { shell: true } }
            current = flow.current()
            handle = current.instance.instance_id
            state = flow.status(handle: handle)
            return flow.close(handle: handle, expected_revision: state.revision)
        }
    "#
            .into(),
        ))
        .await
        .unwrap();
    change(&worker, FlowTransition::Activated).await;
    let result = invoke(&worker, Executor::new()).await;
    assert_eq!(
        result.outcome,
        TurnOutcome::Interrupted(atman_rt::Interruption::Cancelled),
        "{:?}",
        result.result
    );
    wait_closed(&worker).await;
    assert!(worker.read_output(|_| Ok(())).await.unwrap().is_empty());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn reopening_closing_instances_completes_cleanup_without_replaying_entries() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let descriptor = program_descriptor(info.id, "flow main() { loop { yield } }".into());
    let mut catalog = root.create_session(info.clone()).unwrap();
    let mut saved = catalog.create_flow(descriptor.clone()).unwrap();
    saved
        .transition(saved.state().revision(), FlowTransition::Activated)
        .unwrap();
    saved
        .transition(saved.state().revision(), FlowTransition::CloseRequested)
        .unwrap();
    drop(saved);
    drop(catalog);
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.open_session(info.id).unwrap())
        .unwrap();
    let worker = session
        .open_flow(descriptor.address.instance_id)
        .await
        .unwrap();
    let address = worker.address().clone();
    tokio::time::timeout(Duration::from_secs(10), host.shutdown())
        .await
        .unwrap()
        .unwrap();
    let saved = root
        .open_session(info.id)
        .unwrap()
        .open_flow(&address.instance_id)
        .unwrap();
    assert_eq!(saved.state().state(), &InstanceState::Closed);
    assert!(saved.completions().is_empty());
    assert!(saved.execution_events().is_empty());
}

#[tokio::test]
async fn turn_hooks_share_the_admitted_scope_for_peers_and_survive_reopen() {
    use atman_runtime::{AtmanRuntime, AtmanRuntimeOptions, Value};
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    std::fs::create_dir_all(config.join("lib")).unwrap();
    let todo = |label: &str| {
        format!(
            r#"memory.todo.set(where: "{label}", why: "scope", how: "hook", expected_result: "persisted")"#
        )
    };
    std::fs::write(
        config.join("lib/hooks.at"),
        format!(
            r#"pub flow mark() {{ {} }}
pub flow shell_gate() {{ contract {{ capabilities {{ shell: true }} }} bash(command: "exit 0") }}"#,
            todo("start")
        ),
    )
    .unwrap();
    std::fs::write(config.join("a.at"), format!(
        "use \"user:hooks.at\"::mark\nuse \"user:hooks.at\"::shell_gate\non turn.start {{ mark().await\n shell_gate().await\n {} }}\non session.start {{ {} }}\non session.end {{ {} }}",
        todo("unexpected authority"), todo("unexpected start"), todo("unexpected end")
    )).unwrap();
    std::fs::write(
        config.join("b.at"),
        format!(
            "on turn.start {{ {} }}\non turn.end {{ {}\n missing_tool() }}",
            todo("after-error"),
            todo("end")
        ),
    )
    .unwrap();
    let compact = r#"replace_messages_range(messages: [], start: 0, end: 0, summary: "summary")"#;
    std::fs::write(
        config.join("c.at"),
        format!(
            "on session.context_compact {{ {}\n {compact} }}",
            todo("compacted")
        ),
    )
    .unwrap();
    let child_path = dir.path().join("child.at");
    std::fs::write(
        &child_path,
        format!(
            "flow child() -> string {{ {}\n {compact}\n return final.answer(message: \"child\") }}",
            todo("body")
        ),
    )
    .unwrap();
    let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(&config);
    let runtime = AtmanRuntime::build(AtmanRuntimeOptions {
        events: Default::default(),
        mock: false,
        config: hub.clone(),
        project: None,
        home_dir: None,
        workspace_generation: "turn-hooks".into(),
    })
    .await
    .unwrap();
    // Binding retains the hook graph; an invocation must not reload a changed helper.
    std::fs::remove_file(config.join("lib/hooks.at")).unwrap();
    let mut info = session_descriptor();
    info.configuration = atman_runtime::flow_store::SessionConfiguration::Atman(hub.source());
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let source = format!(
        r#"flow main() -> string {{
        when env("fail") == "yes" {{ missing_entry_tool() }}
        inventory = flow.instances()
        flow.spawn(flow: {}, spawn_token: inventory.spawn_token, async: false)
        {}
        {compact}
        return final.answer(message: "parent")
    }}"#,
        serde_json::to_string(&format!("{}@child", child_path.display())).unwrap(),
        todo("body")
    );
    let worker = session
        .create_flow(program_descriptor(info.id, source))
        .await
        .unwrap();
    change(&worker, FlowTransition::Activated).await;
    let mut leases = Vec::new();
    for _ in 0..2 {
        let complete = invoke(&worker, runtime.executor.clone()).await;
        assert!(
            matches!(complete.result, Ok(Value::Str(ref text)) if text == "parent"),
            "{:?}",
            complete.result
        );
        assert_eq!(complete.outcome, TurnOutcome::Completed);
        let entry = worker
            .context()
            .flow_registry
            .lookup(&worker.address().instance_id.to_string())
            .unwrap();
        assert_eq!(entry.child_run_id, complete.lease.run);
        assert_eq!(entry.goal, "main");
        leases.push(complete.lease);
    }
    let expected = ["start", "after-error", "body", "compacted", "end"];
    let planning = worker.planning().await.unwrap();
    assert_eq!(
        planning
            .todos
            .iter()
            .map(|todo| todo.where_.as_str())
            .collect::<Vec<_>>(),
        expected.repeat(2)
    );
    let trace = worker.execution_events().await.unwrap();
    for lease in &leases {
        let runs: Vec<_> = trace
            .iter()
            .flat_map(|batch| &batch.events)
            .filter(|event| {
                event.scope
                    == atman_runtime::flow_store::trace::ExecutionScope::Invocation(lease.clone())
            })
            .filter_map(|event| match &event.event {
                atman_runtime::Event::FlowStart {
                    run_id, flow_name, ..
                } => Some((run_id, flow_name)),
                _ => None,
            })
            .collect();
        assert!(
            runs.iter()
                .any(|(id, name)| *id == &lease.run && name.as_str() == "main")
        );
        assert_eq!(
            runs.iter()
                .map(|(id, _)| *id)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            runs.len()
        );
        assert_eq!(
            runs.iter()
                .filter(|(_, name)| name.starts_with("__lifecycle_turn"))
                .count(),
            3
        );
        assert_eq!(
            runs.iter()
                .filter(|(_, name)| name.starts_with("__lifecycle_session.context_compact"))
                .count(),
            1
        );
    }
    let mut children = 0;
    for row in session.snapshot().await.unwrap().catalog.flows {
        if row.id == worker.address().instance_id {
            continue;
        }
        let child = session.open_flow(row.id).await.unwrap();
        assert_eq!(
            child
                .planning()
                .await
                .unwrap()
                .todos
                .iter()
                .map(|todo| todo.where_.as_str())
                .collect::<Vec<_>>(),
            expected
        );
        assert!(
            child
                .execution_events()
                .await
                .unwrap()
                .iter()
                .flat_map(|batch| &batch.events)
                .all(|event| event.scope.flow() == child.address())
        );
        children += 1;
    }
    assert_eq!(children, 2);
    let failed = invoke_with_env(
        &worker,
        runtime.executor.clone(),
        [("fail".into(), serde_json::json!("yes"))].into(),
    )
    .await;
    assert!(
        failed
            .result
            .unwrap_err()
            .to_string()
            .contains("missing_entry_tool")
    );
    let planning = worker.planning().await.unwrap();
    assert_eq!(planning.todos.len(), 13);
    assert_eq!(planning.todos.last().unwrap().where_, "end");
    let address = worker.address().clone();
    host.shutdown().await.unwrap();
    let stored = root.open_session(info.id).unwrap();
    let reopened = FlowWorker::start(stored.open_flow(&address.instance_id).unwrap())
        .await
        .unwrap();
    assert_eq!(reopened.planning().await.unwrap(), planning);
    assert_eq!(
        reopened
            .completion(None)
            .await
            .unwrap()
            .unwrap()
            .completion
            .lease,
        failed.lease
    );
    drop(reopened.shutdown().await.unwrap());
}

#[tokio::test]
async fn turn_hook_pause_and_cancel_share_the_entry_lease() {
    cancel_paused_hook("turn.start").await;
    cancel_paused_hook("turn.end").await;
}

async fn cancel_paused_hook(event: &str) {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let worker = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    change(&worker, FlowTransition::Activated).await;
    let mut hooks = atman_runtime::lifecycle::LifecycleRunner::new();
    hooks.absorb(
        &atman_rt::parse_file(&format!(
            "on {event} {{ loop {{ yield }} }}\non turn.end {{ missing_tool() }}"
        ))
        .unwrap(),
    );
    let mut executor = Executor::new();
    executor.tool_ctx.lifecycle_hooks = Some(Arc::new(hooks));
    worker
        .accept_input(peer_request(worker.address(), worker.address()), |_| Ok(()))
        .await
        .unwrap();
    let ticket = worker
        .start_turn(
            executor,
            prepare(&worker.snapshot().await.unwrap()).unwrap().unwrap(),
        )
        .await
        .unwrap();
    let mut commits = worker.subscribe_commits();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            commits.borrow_and_update();
            if worker
                .execution_events()
                .await
                .unwrap()
                .iter()
                .flat_map(|batch| &batch.events)
                .any(|record| {
                    matches!(&record.event,
                atman_runtime::Event::FlowStart { flow_name, .. } if flow_name.contains(event))
                })
            {
                break;
            }
            commits.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let key = Uuid::now_v7().into();
    change(&worker, FlowTransition::ExecutionPaused { key }).await;
    assert!(worker.completion(None).await.unwrap().is_none());
    worker
        .cancel_run(ticket.lease.run.clone(), |_| Ok(()))
        .await
        .unwrap()
        .unwrap();
    let complete = tokio::time::timeout(Duration::from_secs(5), ticket.wait())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        complete.outcome,
        TurnOutcome::Interrupted(atman_rt::Interruption::Cancelled)
    );
    assert_eq!(
        worker.read_output(|_| Ok(())).await.unwrap().len(),
        usize::from(event == "turn.end")
    );
    let trace = worker.execution_events().await.unwrap();
    let end_hooks = trace
        .iter()
        .flat_map(|batch| &batch.events)
        .filter(|record| {
            matches!(&record.event,
        atman_runtime::Event::FlowStart { flow_name, .. } if flow_name.contains("turn.end"))
        })
        .count();
    assert_eq!(end_hooks, usize::from(event == "turn.end"));
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_shutdown_unwinds_paused_execution_without_closing_instances_or_losing_input() {
    use atman_runtime::flow_store::goal::GoalRequest;
    use atman_runtime::memory::goal::GoalEdit;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let mut saved = Vec::new();
    for _ in 0..2 {
        let info = session_descriptor();
        let session = host
            .attach_session(root.create_session(info.clone()).unwrap())
            .unwrap();
        let worker = session
            .create_flow(program_descriptor(
                info.id,
                "flow main() -> any { loop { yield } }".into(),
            ))
            .await
            .unwrap();
        change(&worker, FlowTransition::Activated).await;
        let request = peer_request(worker.address(), worker.address());
        worker.accept_input(request, |_| Ok(())).await.unwrap();
        let turn = prepare(&worker.snapshot().await.unwrap()).unwrap().unwrap();
        let ticket = worker.start_turn(Executor::new(), turn).await.unwrap();
        let key = Uuid::now_v7().into();
        change(&worker, FlowTransition::ExecutionPaused { key }).await;
        let goal = worker
            .edit_goal(
                GoalRequest {
                    start_turn: false,
                    id: Uuid::now_v7(),
                    sender: SenderIdentity::User(Uuid::now_v7()),
                    expected: None,
                    edit: GoalEdit::Set {
                        text: "retained objective".into(),
                        replace_completed: false,
                    },
                },
                |_| Ok(()),
            )
            .await
            .unwrap()
            .goal;
        let queued = worker
            .accept_input(peer_request(worker.address(), worker.address()), |_| Ok(()))
            .await
            .unwrap();
        let idle = session.create_flow(flow_descriptor(info.id)).await.unwrap();
        change(&idle, FlowTransition::Activated).await;
        change(&idle, FlowTransition::ExecutionPaused { key }).await;
        idle.accept_input(peer_request(worker.address(), idle.address()), |_| Ok(()))
            .await
            .unwrap();
        session
            .schedule_flow(idle.address().instance_id.clone(), Executor::new(), prepare)
            .await
            .unwrap();
        saved.push((info, worker, idle, ticket, key, goal, queued));
    }
    let owned = host.clone();
    let waiter = tokio::spawn(async move { owned.shutdown().await });
    tokio::time::timeout(Duration::from_secs(5), async {
        while host.session(saved[0].0.id).is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    waiter.abort();
    let _ = waiter.await;
    tokio::time::timeout(Duration::from_secs(10), host.shutdown())
        .await
        .unwrap()
        .unwrap();
    host.shutdown().await.unwrap();
    assert!(host.sessions().is_empty());
    let extra = root.create_session(session_descriptor()).unwrap();
    assert!(host.attach_session(extra).is_err());
    for (info, worker, idle, ticket, key, goal, queued) in saved {
        let complete = ticket.wait().await.unwrap();
        assert_eq!(
            complete.outcome,
            TurnOutcome::Interrupted(atman_rt::Interruption::Cancelled)
        );
        assert!(worker.snapshot().await.is_err());
        let session = root.open_session(info.id).unwrap();
        let flow = session.open_flow(&worker.address().instance_id).unwrap();
        assert_eq!(
            flow.completion(None).unwrap().completion.lease,
            complete.lease
        );
        assert!(matches!(
            flow.state().state(),
            InstanceState::Open(InstanceActivity::Interrupted { .. })
        ));
        assert_eq!(flow.state().pauses(), &[key]);
        assert_eq!(flow.goal(), goal.as_ref());
        assert_eq!(
            flow.pending_inputs()
                .map(|item| item.id)
                .collect::<Vec<_>>(),
            vec![queued.message_id]
        );
        let idle = session.open_flow(&idle.address().instance_id).unwrap();
        assert_eq!(
            idle.state().state(),
            &InstanceState::Open(InstanceActivity::Idle)
        );
        assert!(idle.completion(None).is_none());
        assert_eq!(idle.pending_inputs().count(), 1);
    }
}

#[tokio::test]
async fn failed_storage_shutdown_stops_execution_and_releases_retained_owners() {
    use atman_runtime::tool::{Tier, ToolDefinition};
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Stopped(Arc<AtomicBool>);
    impl Drop for Stopped {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let store = root.create_session(info.clone()).unwrap();
    let session_path = store.dir().to_path_buf();
    let session = host.attach_session(store).unwrap();
    let worker = session
        .create_flow(program_descriptor(
            info.id,
            "flow main() -> any { return await_stop() }".into(),
        ))
        .await
        .unwrap();
    change(&worker, FlowTransition::Activated).await;
    worker
        .accept_input(peer_request(worker.address(), worker.address()), |_| Ok(()))
        .await
        .unwrap();
    let (started, mut entered) = tokio::sync::mpsc::unbounded_channel();
    let stopped = Arc::new(AtomicBool::new(false));
    let observed_stop = stopped.clone();
    let executor = Executor::new();
    executor
        .tools
        .register_fn(
            ToolDefinition::new("await_stop", Tier::Zero),
            move |_, ctx| {
                let started = started.clone();
                let stopped = stopped.clone();
                async move {
                    let _stopped = Stopped(stopped);
                    started.send(()).unwrap();
                    ctx.cancel.cancelled().await;
                    Err(atman_runtime::RuntimeError::Cancelled("stopped".into()))
                }
            },
        )
        .unwrap();
    let ticket = worker
        .start_turn(
            executor,
            prepare(&worker.snapshot().await.unwrap()).unwrap().unwrap(),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), entered.recv())
        .await
        .unwrap()
        .unwrap();
    let key = Uuid::now_v7().into();
    change(&worker, FlowTransition::ExecutionPaused { key }).await;
    let queued = worker
        .accept_input(peer_request(worker.address(), worker.address()), |_| Ok(()))
        .await
        .unwrap();
    let journal = session_path
        .join("flows")
        .join(worker.address().instance_id.to_string())
        .join("events.jsonl");
    // Force a real short read after all trace writes have reached the owner.
    let saved = std::fs::read(&journal).unwrap();
    std::fs::write(&journal, []).unwrap();
    assert!(
        worker
            .read_updates(None, NonZeroUsize::new(1).unwrap())
            .await
            .is_err()
    );
    // Restore the bytes for recovery; the current writer remains poisoned.
    std::fs::write(&journal, saved).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(5), host.shutdown())
            .await
            .unwrap()
            .is_err()
    );
    assert!(
        worker.snapshot().await.is_err(),
        "failed storage must relinquish its writer"
    );
    assert!(
        observed_stop.load(Ordering::Acquire),
        "paused tool execution must exit before shutdown returns"
    );
    assert!(
        tokio::time::timeout(Duration::from_secs(5), ticket.wait())
            .await
            .unwrap()
            .is_err()
    );
    let restored_session = root.open_session(info.id).unwrap();
    let restored = restored_session
        .open_flow(&worker.address().instance_id)
        .unwrap();
    assert!(matches!(
        restored.state().state(),
        InstanceState::Open(InstanceActivity::Interrupted { .. })
    ));
    assert_eq!(restored.state().pauses(), &[key]);
    assert_eq!(
        restored
            .pending_inputs()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        vec![queued.message_id]
    );
    assert!(
        entered.try_recv().is_err(),
        "recovery must not rerun the entry"
    );
}

#[tokio::test]
async fn late_form_replies_use_the_instance_inbox_and_commit_before_transport_acknowledgement() {
    use atman_runtime::form::{
        CompositeForm, DeferredFormAnswer, FormAnswer, FormKind, FormQuestion, FormSubmission,
    };
    use atman_runtime::rendezvous::PromptId;

    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut catalog = root.create_session(info.clone()).unwrap();
    let worker = FlowWorker::start(
        catalog
            .create_flow(program_descriptor(
                info.id,
                "flow main() -> string { return \"done\" }".into(),
            ))
            .unwrap(),
    )
    .await
    .unwrap();
    change(&worker, FlowTransition::Activated).await;
    let pause = Uuid::now_v7().into();
    change(&worker, FlowTransition::ExecutionPaused { key: pause }).await;
    let forms = Arc::new(atman_runtime::session::FormRegistry::new());
    let _view = forms.subscribe();
    let ctx = atman_runtime::tool::ToolCtx::new()
        .with_session_runtime(worker.context())
        .with_forms(forms.clone());
    let resolver = ctx.form_resolver().unwrap();
    let id = PromptId::now();
    let form = CompositeForm {
        questions: vec![FormQuestion {
            id: "scope".into(),
            kind: FormKind::Text {
                prompt: "Which scope?".into(),
                placeholder: None,
                multiline: false,
            },
        }],
    };
    assert!(
        atman_runtime::rendezvous::await_expirable_prompt_with_payload_cancel(
            &resolver,
            id,
            "form_ask",
            serde_json::to_value(&form).unwrap(),
            Duration::ZERO,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap()
        .is_none()
    );
    let submission = FormSubmission::Submitted {
        answers: vec![FormAnswer::TextEntered {
            text: "selected scope".into(),
        }],
    };
    let form_id = id.to_string();
    let (first, retry) = tokio::join!(
        forms.submit(&form_id, submission.clone()),
        forms.submit(&form_id, submission.clone()),
    );
    assert_eq!(usize::from(first.unwrap()) + usize::from(retry.unwrap()), 1);
    assert!(forms.list_pending().is_empty());
    let snapshot = worker.snapshot().await.unwrap();
    assert!(!snapshot.state.can_start_turn());
    assert_eq!(snapshot.pending.len(), 1);
    assert!(matches!(
        snapshot.pending[0].request.payload,
        InboxPayload::FormReply { .. }
    ));
    assert_eq!(
        worker.context().deferred_form_inbox().pending_count(),
        0,
        "persistent answers have only one inbox"
    );
    let answer = DeferredFormAnswer {
        prompt_id: id.to_string(),
        form,
        submission,
    };
    let address = worker.address().clone();
    drop(worker.shutdown().await.unwrap());
    let reopened = FlowWorker::start(catalog.open_flow(&address.instance_id).unwrap())
        .await
        .unwrap();
    assert_eq!(reopened.snapshot().await.unwrap().pending.len(), 1);
    assert!(
        reopened
            .context()
            .deferred_form_inbox()
            .record(answer.clone())
            .await
            .unwrap()
    );
    assert_eq!(reopened.snapshot().await.unwrap().cursor, snapshot.cursor);
    change(&reopened, FlowTransition::ExecutionResumed { key: pause }).await;
    let request = prepare(&reopened.snapshot().await.unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(request.context.len(), 1);
    assert_eq!(request.context[0].origin, MessageOrigin::Interjection);
    assert!(
        request.context[0]
            .text_concat()
            .contains("Which scope?: selected scope")
    );
    assert!(
        reopened
            .start_turn(Executor::new(), request)
            .await
            .unwrap()
            .wait()
            .await
            .unwrap()
            .result
            .is_ok()
    );
    assert!(reopened.snapshot().await.unwrap().pending.is_empty());
    let consumed = reopened.snapshot().await.unwrap().cursor;
    assert!(
        reopened
            .context()
            .deferred_form_inbox()
            .record(answer.clone())
            .await
            .unwrap()
    );
    assert_eq!(reopened.snapshot().await.unwrap().cursor, consumed);
    let mut conflicting = answer.clone();
    conflicting.submission = FormSubmission::Submitted {
        answers: vec![FormAnswer::TextEntered {
            text: "different scope".into(),
        }],
    };
    assert!(
        reopened
            .context()
            .deferred_form_inbox()
            .record(conflicting)
            .await
            .is_err()
    );

    let ctx = atman_runtime::tool::ToolCtx::new()
        .with_session_runtime(reopened.context())
        .with_forms(forms.clone());
    let resolver = ctx.form_resolver().unwrap();
    let failed_id = PromptId::now();
    let _waiting = resolver.register_with_payload(
        failed_id,
        "form_ask",
        serde_json::to_value(&answer.form).unwrap(),
    );
    assert!(resolver.expire_pending(&failed_id));
    drop(reopened.shutdown().await.unwrap());
    assert!(
        forms
            .submit(&failed_id.to_string(), answer.submission.clone())
            .await
            .is_err()
    );
    assert_eq!(
        forms.list_pending().len(),
        1,
        "failed commits must retain the answer form"
    );
    let restored = FlowWorker::start(catalog.open_flow(&address.instance_id).unwrap())
        .await
        .unwrap();
    assert!(restored.snapshot().await.unwrap().pending.is_empty());
    assert_eq!(
        restored
            .context()
            .messages()
            .iter()
            .filter(|message| message
                .text_concat()
                .contains("Which scope?: selected scope"))
            .count(),
        1
    );
    restored.shutdown().await.unwrap();
}

#[tokio::test]
async fn consistent_views_bound_catch_up_and_preserve_cancelled_observation() {
    use atman_runtime::{
        flow_store::{
            goal::GoalRequest,
            planning::{PlanningEdit, PlanningRequest},
        },
        memory::goal::GoalEdit,
        projection::flow::FlowRecord,
    };
    use futures::FutureExt;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let worker = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    let mut reader = worker
        .subscribe_view(NonZeroUsize::new(1).unwrap())
        .await
        .unwrap();
    assert!(reader.view().is_none());
    let created = worker.status().await.unwrap().cursor;
    change(&worker, FlowTransition::Activated).await;
    worker
        .edit_goal(
            GoalRequest {
                id: Uuid::new_v4(),
                sender: SenderIdentity::User(Uuid::new_v4()),
                expected: None,
                edit: GoalEdit::Set {
                    text: "Inspect persistent views".into(),
                    replace_completed: false,
                },
                start_turn: false,
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    worker
        .edit_planning(
            PlanningRequest {
                id: Uuid::new_v4(),
                sender: SenderIdentity::User(Uuid::new_v4()),
                expected_revision: None,
                edit: PlanningEdit::WritePlan {
                    id: "review".into(),
                    title: "Review".into(),
                    steps: vec!["Inspect".into()],
                },
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    invoke(&worker, Executor::new()).await.result.unwrap();
    let request = DeliveryRequest {
        request_id: Uuid::new_v4(),
        sender: SenderIdentity::User(Uuid::new_v4()),
        recipient: worker.address().clone(),
        delivery: DeliveryMode::NextTurn,
        cause: None,
        correlation: None,
        scope: None,
        payload: InboxPayload::Interjection {
            text: "Keep unread".into(),
        },
    };
    worker.accept_input(request, |_| Ok(())).await.unwrap();
    let tip = worker.status().await.unwrap().cursor;
    // Registration captures state before first poll; later publications cannot leak into it.
    let initial = reader.next().await.unwrap();
    assert_eq!(initial.transcript().cursor(), Some(&created));
    assert_eq!(initial.snapshot().status.cursor, created);
    assert!(initial.snapshot().status.goal.is_none());
    assert!(initial.snapshot().planning.plans.is_empty());
    assert!(initial.transcript().entries().is_empty());

    // Observe a ready notification, then cancel while the owner is serving its snapshot.
    assert!(reader.next().now_or_never().is_none());
    // No new writes are needed to finish the interrupted read.
    let view = tokio::time::timeout(Duration::from_secs(5), reader.next())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(view.transcript().cursor(), Some(&tip));
    assert_eq!(view.snapshot().status.cursor, tip);
    assert_eq!(
        view.snapshot().status.goal.as_ref().unwrap().objective,
        "Inspect persistent views"
    );
    assert_eq!(view.snapshot().planning.plans.len(), 1);
    assert_eq!(view.snapshot().pending.len(), 1);
    let entries = view.transcript().entries().len();
    assert_eq!(
        view.transcript()
            .entries()
            .iter()
            .filter(|entry| matches!(&entry.record,
        FlowRecord::Publication(value) if value.message.origin == MessageOrigin::FinalAnswer))
            .count(),
        1
    );
    assert!(
        !view
            .transcript()
            .entries()
            .iter()
            .flat_map(|entry| entry.record.shared_messages())
            .any(|message| message.text_concat() == "Keep unread")
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(40), reader.next())
            .await
            .is_err()
    );
    let refreshed = reader.refresh().await.unwrap();
    assert_eq!(refreshed.transcript().entries().len(), entries);
    assert_eq!(refreshed.snapshot().status.cursor, tip);
    assert_eq!(
        worker.snapshot().await.unwrap().pending.len(),
        1,
        "reading must not claim inbox content"
    );

    let key = Uuid::new_v4().into();
    change(&worker, FlowTransition::ExecutionPaused { key }).await;
    let view = reader.next().await.unwrap();
    assert_eq!(view.snapshot().status.execution.pauses().len(), 1);
    assert_eq!(
        view.snapshot().status.goal.as_ref().unwrap().status,
        atman_runtime::memory::goal::GoalStatus::Active
    );
    let entry = view.snapshot().pending[0].clone();
    let edit = atman_runtime::flow_store::inbox::InboxEditRequest {
        id: Uuid::new_v4(),
        sender: entry.envelope.request.sender.clone(),
        message: entry.envelope.id,
        expected: entry.revision,
        change: atman_runtime::flow_store::inbox::InboxChange::Replace {
            content: atman_runtime::flow_store::inbox::UserContent {
                text: "Edited unread".into(),
                images: vec![],
                presentation: None,
            },
            delivery: DeliveryMode::NextTurn,
        },
    };
    let paused_state = view.snapshot().status.execution.clone();
    let committed = worker.edit_input(edit, |_| Ok(())).await.unwrap();
    let view = reader.next().await.unwrap();
    assert_eq!(view.transcript().cursor(), Some(&committed));
    assert_eq!(view.transcript().entries().len(), entries);
    assert_eq!(view.snapshot().status.cursor, committed);
    assert_eq!(view.snapshot().status.execution, paused_state);
    assert_eq!(view.snapshot().pending[0].revision, committed);
    assert_eq!(view.snapshot().pending[0].acceptance, entry.acceptance);
    assert!(
        matches!(&view.snapshot().pending[0].envelope.request.payload, InboxPayload::Interjection { text } if text == "Edited unread")
    );
    let paused = view.snapshot().status.cursor.clone();
    drop(reader);
    assert_eq!(worker.status().await.unwrap().cursor, paused);
    let address = worker.address().clone();
    drop(worker.shutdown().await.unwrap());
    let restored = session.open_flow(address.instance_id).await.unwrap();
    let mut replay = restored
        .subscribe_view(NonZeroUsize::new(2).unwrap())
        .await
        .unwrap();
    let view = replay.next().await.unwrap();
    assert_eq!(view.snapshot().status.cursor, paused);
    assert_eq!(view.transcript().entries().len(), entries);
    assert_eq!(view.snapshot().pending.len(), 1);
    assert_eq!(view.snapshot().pending[0].revision, committed);
    assert_eq!(view.snapshot().planning.plans.len(), 1);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn view_pages_and_live_updates_share_commit_order_across_turns_pause_and_reopen() {
    use atman_runtime::flow_store::view::FlowUpdate;
    use futures::StreamExt;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let worker = session
        .create_flow(program_descriptor(
            info.id,
            "flow main() -> string { return final.answer(message: \"view answer\") }".into(),
        ))
        .await
        .unwrap();
    let limit = NonZeroUsize::new(2).unwrap();
    let mut live = worker.subscribe_updates(None, limit).await.unwrap();
    // Work commits after registration but before the consumer first polls the stream.
    change(&worker, FlowTransition::Activated).await;
    invoke(&worker, Executor::new()).await.result.unwrap();
    invoke(&worker, Executor::new()).await.result.unwrap();
    change(
        &worker,
        FlowTransition::ExecutionPaused {
            key: Uuid::new_v4().into(),
        },
    )
    .await;
    let tip = worker.status().await.unwrap().cursor;
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let page = live.next().await.unwrap().unwrap();
            assert!(page.batches.len() <= limit.get());
            received.extend(page.batches);
            if page.next == Some(tip.clone()) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        received
            .iter()
            .map(|batch| batch.receipt.sequence)
            .collect::<Vec<_>>(),
        (1..=tip.sequence).collect::<Vec<_>>()
    );
    assert!(
        received.iter().any(|batch| batch.facts.len() > 1),
        "turn admission must remain atomic"
    );
    assert_eq!(received.iter().flat_map(|batch| &batch.facts).filter(|fact| matches!(fact, FlowUpdate::MessagePublished(message) if message.message.origin == MessageOrigin::FinalAnswer)).count(), 2);
    assert_eq!(
        received
            .iter()
            .flat_map(|batch| &batch.facts)
            .filter(|fact| matches!(fact, FlowUpdate::TurnCompleted(_)))
            .count(),
        2
    );
    assert!(
        received
            .iter()
            .flat_map(|batch| &batch.facts)
            .any(|fact| matches!(fact, FlowUpdate::ExecutionEvents(_)))
    );
    let encoded = serde_json::to_value(&received).unwrap();
    assert!(
        !serde_json::to_string(&encoded[0])
            .unwrap()
            .contains("flow main()")
    );
    let first = received[0].receipt.clone();
    let mut foreign = first.clone();
    foreign.journal_id = Uuid::new_v4();
    assert!(
        worker
            .subscribe_updates(Some(foreign), limit)
            .await
            .is_err()
    );
    assert!(
        tokio::time::timeout(Duration::from_millis(40), live.next())
            .await
            .is_err(),
        "idle streams must wait, without empty updates"
    );
    let before = worker.state().await.unwrap();
    drop(live);
    assert_eq!(worker.state().await.unwrap(), before);
    let address = worker.address().clone();
    drop(worker.shutdown().await.unwrap());
    let reopened = session.open_flow(address.instance_id).await.unwrap();
    let mut after = None;
    let mut history = Vec::new();
    loop {
        let page = reopened.read_updates(after, limit).await.unwrap();
        after = page.next;
        history.extend(page.batches);
        if !page.has_more {
            break;
        }
    }
    assert_eq!(serde_json::to_value(&history).unwrap(), encoded);
    assert_eq!(reopened.state().await.unwrap(), before);
    let mut resumed = reopened
        .subscribe_updates(Some(first), limit)
        .await
        .unwrap();
    let mut tail = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let page = resumed.next().await.unwrap().unwrap();
            tail.extend(page.batches);
            if page.next == Some(tip.clone()) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(
        serde_json::to_value(tail).unwrap(),
        serde_json::to_value(&history[1..]).unwrap()
    );
}

#[tokio::test]
async fn permission_endpoint_lifetime_is_independent_of_retained_instance_handles() {
    use atman_runtime::{
        flow_runtime::host::HostServices,
        permission::{
            ApprovalAuthority, PermissionClients, PermissionIntent, PermissionResolution,
            SubmissionOutcome,
        },
        tool::Tier,
    };

    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let clients = Arc::new(PermissionClients::default());
    let host = RuntimeHost::with_services(HostServices {
        permission_clients: Arc::downgrade(&clients),
        ..Default::default()
    });
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let worker = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    let context = worker.context();
    let broker = context.permission_broker();
    assert!(
        broker.has_clients(),
        "registration must precede the creation acknowledgement"
    );
    let owner = context
        .flow_registry
        .register_root(
            context.id().to_string(),
            RunId::now(),
            EffectiveAuthority::root(&context.trust_config(), true, None),
        )
        .unwrap();
    let SubmissionOutcome::Pending(pending) = broker
        .submit_to(
            Some(&owner.session_id),
            Some(&owner.run_id),
            PermissionIntent::minimal("pending", "fs.write", Tier::Two),
            false,
            ApprovalAuthority::User(broker.user_authority(&owner.session_id, None)),
            &context.trust_config(),
        )
        .unwrap()
    else {
        panic!("expected pending permission")
    };
    drop(clients);
    assert!(!broker.has_clients());
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(1), pending.resolution).await.unwrap().unwrap(),
        PermissionResolution::Cancelled { reason } if reason == "permission client disconnected"
    ));
    assert!(
        worker.snapshot().await.is_ok(),
        "execution handles do not retain the endpoint"
    );
    let other = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    assert!(
        !other.context().permission_broker().has_clients(),
        "new instances must not revive a disconnected endpoint"
    );
}

#[tokio::test]
async fn permission_audit_uses_instance_scope_and_retains_source_runs_across_turns() {
    use atman_runtime::{
        Value,
        event::Event,
        flow_store::trace::ExecutionScope,
        permission::{PermissionIntent, SubmissionOutcome},
        redact::{RedactMode, Redactor},
        tool::{Tier, ToolDefinition},
        trust::{TrustConfig, TrustMode},
    };

    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let host = RuntimeHost::with_redactor(Arc::new(Redactor::from_pairs(
        &[("secret", "private-label")],
        RedactMode::Full,
    )));
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let worker = session
        .create_flow(program_descriptor(
            info.id,
            "flow main() -> string { return permission_probe() }".into(),
        ))
        .await
        .unwrap();
    let other = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    change(&worker, FlowTransition::Activated).await;
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = requests.clone();
    let executor = Executor::new();
    executor
        .tools
        .register_fn(
            ToolDefinition::new("permission_probe", Tier::Zero),
            move |_, ctx| {
                let owner = ctx.flow_identity.as_ref().unwrap();
                let broker = ctx.permission_broker.as_ref().unwrap();
                let SubmissionOutcome::Pending(pending) = broker
                    .submit(
                        Some(&owner.session_id),
                        Some(&owner.run_id),
                        PermissionIntent::minimal("pending", "permission_probe", Tier::Two),
                        false,
                        &TrustConfig {
                            mode: TrustMode::Steady,
                            ..Default::default()
                        },
                    )
                    .unwrap()
                else {
                    panic!("expected pending permission")
                };
                let request = pending.request;
                let group = broker
                    .user_create_group(
                        &owner.session_id,
                        [request.request_id.clone()].into(),
                        "private-label".into(),
                        &[(request.request_id.clone(), request.revision)].into(),
                    )
                    .unwrap();
                captured.lock().unwrap().push((request, group));
                async { Ok(Value::Str("done".into())) }
            },
        )
        .unwrap();

    let first = invoke(&worker, executor.clone()).await;
    assert!(first.result.is_ok(), "{:?}", first.result);
    let second = invoke(&worker, executor).await;
    assert!(second.result.is_ok(), "{:?}", second.result);
    let records = requests.lock().unwrap().clone();
    assert_eq!(records.len(), 2);
    // A late service emission retains its own group after a later turn has finished.
    let group = &records[0].1;
    let late_service = worker.context().sink().clone();
    let late_event = Event::PermissionGroupUpdated {
        payload: atman_runtime::permission_audit::PermissionGroupAudit::from_group(
            group,
            &info.id.to_string(),
            chrono::Utc::now(),
        ),
    };
    late_service.emit(late_event.clone());
    let trace = worker.execution_events().await.unwrap();
    let events: Vec<_> = trace.iter().flat_map(|batch| &batch.events).collect();
    for ((request, group), completion) in records.iter().zip([&first, &second]) {
        let created = events.iter().filter(|event| matches!(&event.event,
            Event::PermissionRequestCreated { payload } if payload.request_id.as_ref() == Some(&request.request_id)
                && payload.requesting_run_id == completion.lease.run));
        assert_eq!(created.count(), 1);
        let cancelled: Vec<_> = events.iter().filter(|event| matches!(&event.event,
            Event::PermissionRequestCancelled { payload } if payload.request_id.as_ref() == Some(&request.request_id)
                && payload.requesting_run_id == completion.lease.run)).collect();
        assert_eq!(cancelled.len(), 1);
        assert_eq!(
            cancelled[0].scope,
            ExecutionScope::Instance(worker.address().clone())
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(&event.event,
                    Event::PermissionGroupResolved { payload } if payload.group_id == group.group_id
                ))
                .count(),
            1
        );
    }
    let grouped = events
        .iter()
        .find(|event| {
            matches!(&event.event,
        Event::PermissionGroupUpdated { payload } if payload.group_id == group.group_id)
        })
        .unwrap();
    assert_eq!(
        grouped.scope,
        ExecutionScope::Instance(worker.address().clone())
    );
    assert!(
        !serde_json::to_string(&trace)
            .unwrap()
            .contains("private-label")
    );
    assert!(other.execution_events().await.unwrap().is_empty());
    let address = worker.address().clone();
    let mut store = worker.shutdown().await.unwrap();
    let cursor = store.cursor().clone();
    let mut invalid = (*grouped).clone();
    invalid.sequence = events.last().unwrap().sequence + 1;
    invalid.scope = ExecutionScope::Instance(other.address().clone());
    assert!(
        store
            .append_execution_events(vec![invalid.clone()])
            .is_err()
    );
    invalid.scope = ExecutionScope::Instance(address.clone());
    invalid.event = Event::MermaidDiagram {
        source: "requires an invocation".into(),
    };
    assert!(store.append_execution_events(vec![invalid]).is_err());
    assert_eq!(store.cursor(), &cursor);
    drop(store);
    let reopened = session.open_flow(address.instance_id).await.unwrap();
    late_service.emit(late_event);
    assert_eq!(
        serde_json::to_value(reopened.execution_events().await.unwrap()).unwrap(),
        serde_json::to_value(trace).unwrap()
    );
    reopened.shutdown().await.unwrap();
    other.shutdown().await.unwrap();
}

#[tokio::test]
async fn execution_traces_share_instance_order_and_survive_turns_and_reopen() {
    use atman_runtime::{
        Value,
        event::{Event, EventSink},
        redact::{RedactMode, Redactor},
        tool::{Tier, ToolDefinition},
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let info = session_descriptor();
    let mut session = root.create_session(info.clone()).unwrap();
    let source = r#"
        flow main() -> string {
            index = 0
            loop {
                index = index + 1
                when index == 128 { break }
            }
            value = helper().await
            results = fanout [helper(), helper()]
            return final.answer(message: value)
        }
        flow helper() -> string { return trace_probe() }
    "#;
    let first = FlowWorker::start(
        session
            .create_flow(program_descriptor(info.id, source.into()))
            .unwrap(),
    )
    .await
    .unwrap();
    let second = FlowWorker::start(
        session
            .create_flow(program_descriptor(info.id, source.into()))
            .unwrap(),
    )
    .await
    .unwrap();
    change(&first, FlowTransition::Activated).await;
    change(&second, FlowTransition::Activated).await;
    let executor = Executor::with_events(EventSink::new().with_redactor(Arc::new(
        Redactor::from_pairs(&[("secret", "trace-secret")], RedactMode::Full),
    )));
    let captured = Arc::new(std::sync::Mutex::new(BTreeMap::new()));
    let probes = captured.clone();
    executor
        .tools
        .register_fn(
            ToolDefinition::new("trace_probe", Tier::Zero),
            move |_, ctx| {
                probes.lock().unwrap().insert(
                    ctx.flow_identity
                        .as_ref()
                        .unwrap()
                        .instance
                        .as_ref()
                        .unwrap()
                        .instance_id
                        .to_string(),
                    ctx.events.clone().unwrap(),
                );
                async { Ok(Value::Str("trace-secret".into())) }
            },
        )
        .unwrap();
    let (a, b) = tokio::join!(
        invoke(&first, executor.clone()),
        invoke(&second, executor.clone())
    );
    assert!(a.result.is_ok(), "{:?}", a.result);
    assert!(b.result.is_ok(), "{:?}", b.result);
    assert!(executor.events.snapshot().is_empty());
    for (worker, completion) in [(&first, &a), (&second, &b)] {
        let trace = worker.execution_events().await.unwrap();
        assert!(!trace.is_empty());
        assert!(trace.last().unwrap().receipt.sequence < completion.receipt.sequence);
        let events: Vec<_> = trace.iter().flat_map(|batch| &batch.events).collect();
        let live = worker.context().sink().snapshot_envelopes();
        let node_positions = |events: Vec<(u64, &Event)>| {
            events
                .into_iter()
                .filter_map(|(seq, event)| {
                    matches!(
                        event,
                        Event::FlowNodeStart { .. } | Event::FlowNodeEnd { .. }
                    )
                    .then_some(seq)
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            node_positions(
                events
                    .iter()
                    .map(|event| (event.sequence, &event.event))
                    .collect()
            ),
            node_positions(live.iter().map(|event| (event.seq, &event.event)).collect()),
            "batching must retain every live execution node exactly once"
        );
        assert!(events.iter().all(|event| {
            event
                .scope
                .lease()
                .is_none_or(|lease| lease == &completion.lease)
        }));
        assert!(
            events
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence)
        );
        assert!(events.iter().any(|event| matches!(&event.event, Event::FlowStart { flow_name, .. } if flow_name == "helper")));
        assert!(events.iter().any(|event| matches!(&event.event, Event::FlowEnd { run_id, .. } if run_id == &completion.lease.run)));
        assert!(!events.iter().any(|event| matches!(
            event.event,
            Event::AssistantMsg { .. } | Event::UserMsg { .. }
        )));
        assert!(
            !serde_json::to_string(&trace)
                .unwrap()
                .contains("trace-secret")
        );
    }
    let late_sink = captured
        .lock()
        .unwrap()
        .get(&first.address().instance_id.to_string())
        .unwrap()
        .clone();
    late_sink.emit(Event::MermaidDiagram {
        source: "late resource diagnostic".into(),
    });
    let saved = first.execution_events().await.unwrap();
    assert!(matches!(
        saved.last().unwrap().events.last().unwrap().event,
        Event::MermaidDiagram { .. }
    ));
    let previous_sequence = saved.last().unwrap().events.last().unwrap().sequence;
    let address = first.address().clone();
    let mut store = first.shutdown().await.unwrap();
    assert_eq!(
        serde_json::to_value(store.execution_events()).unwrap(),
        serde_json::to_value(&saved).unwrap()
    );
    let cursor = store.cursor().clone();
    let old = saved[0].events[0].clone();
    assert!(store.append_execution_events(vec![old.clone()]).is_err());
    let mut wrong_owner = old.clone();
    wrong_owner.sequence = previous_sequence + 1;
    let atman_runtime::flow_store::trace::ExecutionScope::Invocation(lease) =
        &mut wrong_owner.scope
    else {
        panic!("expected invocation event")
    };
    lease.flow = second.address().clone();
    assert!(store.append_execution_events(vec![wrong_owner]).is_err());
    let mut message = old;
    message.sequence = previous_sequence + 1;
    message.event = Event::TurnEnd {
        turn_id: a.lease.turn.clone(),
    };
    assert!(store.append_execution_events(vec![message]).is_err());
    assert_eq!(store.cursor(), &cursor);
    drop(store);
    let restored = FlowWorker::start(session.open_flow(&address.instance_id).unwrap())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(restored.execution_events().await.unwrap()).unwrap(),
        serde_json::to_value(saved).unwrap()
    );
    assert!(invoke(&restored, executor).await.result.is_ok());
    let updated = restored.execution_events().await.unwrap();
    let new_events: Vec<_> = updated
        .iter()
        .flat_map(|batch| &batch.events)
        .filter(|event| event.scope.lease().is_some_and(|lease| lease != &a.lease))
        .collect();
    assert!(!new_events.is_empty());
    assert!(
        new_events
            .iter()
            .all(|event| event.sequence > previous_sequence)
    );
    restored.shutdown().await.unwrap();
    second.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_watcher_replacement_shares_process_producers_without_backfill() {
    use atman_runtime::flow_store::{process_watch::ProcessWatch, watch_edit::WatchDefinition};
    use atman_runtime::{AtmanRuntime, AtmanRuntimeOptions};
    let dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let config = atman_runtime::config_hub::ConfigHub::from_config_dir(dir.path());
    let mut info = session_descriptor();
    info.configuration = atman_runtime::flow_store::SessionConfiguration::Atman(config.source());
    info.project = Some(
        atman_runtime::storage::ProjectContext::resolve(&config, project.path(), dir.path())
            .unwrap(),
    );
    let runtime = AtmanRuntime::build(AtmanRuntimeOptions {
        events: atman_runtime::event::EventSink::new(),
        mock: false,
        config,
        project: info.project.clone(),
        home_dir: None,
        workspace_generation: "watch-replacement".into(),
    })
    .await
    .unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let worker = session
        .create_flow(program_descriptor(
            info.id,
            r#"
        flow main() -> any {
            contract { capabilities { shell: true } }
            bash.spawn(cmd: env("bash"), timeout_ms: 0)
            term.spawn(cmd: env("terminal"))
            return "started"
        }
    "#
            .into(),
        ))
        .await
        .unwrap();
    let source = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    for flow in [&worker, &source] {
        change(flow, FlowTransition::Activated).await;
    }
    let command = |name| {
        format!(
            "printf ready; while [ ! -f '{}' ]; do sleep 0.05; done; printf ready; exec sleep 60",
            project.path().join(name).display()
        )
    };
    invoke_with_env(
        &worker,
        runtime.executor.clone(),
        [
            ("bash".into(), command("bash").into()),
            ("terminal".into(), command("terminal").into()),
        ]
        .into(),
    )
    .await
    .result
    .unwrap();
    change(
        &worker,
        FlowTransition::ExecutionPaused {
            key: Uuid::now_v7().into(),
        },
    )
    .await;
    let state = worker.state().await.unwrap();
    let owner = worker.address().clone();
    let sources = worker.process_watch_sources();
    assert_eq!(sources.len(), 2);
    let mut pending = 0;
    let mut retries = Vec::new();
    for entry in sources {
        let original = ProcessWatch {
            id: Uuid::now_v7(),
            source: entry.source.clone(),
            pattern: "ready".into(),
            once: false,
            expires_at: None,
        };
        worker
            .watch_process_source(original.clone(), |_| Ok(()))
            .await
            .unwrap();
        pending += 1;
        wait_inputs(&worker, pending).await;
        let expected = WatchDefinition::Process(original);
        let watch = ProcessWatch {
            id: Uuid::now_v7(),
            source: entry.source.clone(),
            pattern: "ready".into(),
            once: false,
            expires_at: None,
        };
        let replacement = WatchDefinition::Process(watch.clone());
        let initial = worker.snapshot().await.unwrap();
        let denied = host
            .replace_watcher(owner.clone(), expected.clone(), replacement.clone(), {
                let calls = std::sync::atomic::AtomicUsize::new(0);
                move |_| {
                    if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 2 {
                        Err(io::ErrorKind::PermissionDenied.into())
                    } else {
                        Ok(())
                    }
                }
            })
            .await
            .unwrap_err();
        assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(worker.snapshot().await.unwrap().cursor, initial.cursor);
        let (first, retry) = tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(
                host.replace_watcher(
                    owner.clone(),
                    expected.clone(),
                    replacement.clone(),
                    |_| Ok(())
                ),
                host.replace_watcher(
                    owner.clone(),
                    expected.clone(),
                    replacement.clone(),
                    |_| Ok(())
                )
            )
        })
        .await
        .unwrap();
        let receipt = first.unwrap();
        assert_eq!(receipt, retry.unwrap());
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            worker.snapshot().await.unwrap().pending,
            initial.pending,
            "retained process output must not match a replacement"
        );
        let marker = match entry.source {
            atman_runtime::flow_store::process_watch::ProcessSource::Bash { .. } => "bash",
            _ => "terminal",
        };
        std::fs::write(project.path().join(marker), "go").unwrap();
        pending += 1;
        wait_inputs(&worker, pending).await;
        let inputs = worker.snapshot().await.unwrap().pending;
        assert!(inputs.iter().any(|input| matches!(&input.request.payload, InboxPayload::ProcessEvent { content, .. } if content["subscriptions"].as_array().is_some_and(|ids| ids.len() == 1 && ids[0][0] == watch.id.to_string()))));
        let flow_watch = WatchDefinition::Flow(answer_watch(&source));
        host.replace_watcher(
            owner.clone(),
            replacement.clone(),
            flow_watch.clone(),
            |_| Ok(()),
        )
        .await
        .unwrap();
        invoke(&source, runtime.executor.clone())
            .await
            .result
            .unwrap();
        pending += 1;
        wait_inputs(&worker, pending).await;
        let back = WatchDefinition::Process(ProcessWatch {
            id: Uuid::now_v7(),
            ..watch
        });
        host.replace_watcher(owner.clone(), flow_watch, back.clone(), |_| Ok(()))
            .await
            .unwrap();
        worker.unwatch(back.id(), |_| Ok(())).await.unwrap();
        assert_eq!(
            host.replace_watcher(
                owner.clone(),
                expected.clone(),
                replacement.clone(),
                |_| Ok(())
            )
            .await
            .unwrap(),
            receipt
        );
        retries.push((expected, replacement, receipt));
    }
    assert_eq!(worker.state().await.unwrap(), state);
    host.wait_quiescent().await.unwrap();
    let quiet = worker.snapshot().await.unwrap().cursor;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(worker.snapshot().await.unwrap().cursor, quiet);
    host.shutdown().await.unwrap();
    let reopened = RuntimeHost::default();
    reopened
        .attach_session(root.open_session(info.id).unwrap())
        .unwrap();
    for (expected, replacement, receipt) in retries {
        assert_eq!(
            reopened
                .replace_watcher(owner.clone(), expected, replacement, |_| Ok(()))
                .await
                .unwrap(),
            receipt
        );
    }
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn process_watch_tools_share_instance_delivery_across_pause_turns_and_explicit_wait() {
    use atman_runtime::{AtmanRuntime, AtmanRuntimeOptions, ValueJson};
    let dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let config = atman_runtime::config_hub::ConfigHub::from_config_dir(dir.path());
    let mut info = session_descriptor();
    info.configuration = atman_runtime::flow_store::SessionConfiguration::Atman(config.source());
    info.project = Some(
        atman_runtime::storage::ProjectContext::resolve(&config, project.path(), dir.path())
            .unwrap(),
    );
    let runtime = AtmanRuntime::build(AtmanRuntimeOptions {
        events: atman_runtime::event::EventSink::new(),
        mock: false,
        config,
        project: info.project.clone(),
        home_dir: None,
        workspace_generation: "process-observation".into(),
    })
    .await
    .unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let source = r#"
        flow main() -> any {
            contract { capabilities { shell: true } }
            when env("action") == "receive" {
                later = watch(handle: env("background"), pattern: "later")
                bash.spawn(cmd: env("release"), block: true)
                received = wait_for_watcher(timeout_ms: 4000)
                watcher.unwatch(watcher_id: env("persist"))
                absent = watch(handle: env("terminal"), pattern: "absent", timeout_ms: 100)
                expired = wait_for_watcher(timeout_ms: 4000)
                return [received, expired, watcher.list()]
            }
            background = bash.spawn(cmd: env("command"), timeout_ms: 0)
            first = watch(handle: background.handle, pattern: "ready", mode: "persist")
            second = watch(handle: background.handle, pattern: "ready")
            terminal = term.spawn(cmd: "printf 'terminal-ready\\n'; exec sleep 60")
            term_watch = watch(handle: terminal.handle, pattern: "terminal-ready")
            return [background, terminal, first, second, watcher.list()]
        }
    "#;
    let worker = session
        .create_flow(program_descriptor(info.id, source.into()))
        .await
        .unwrap();
    change(&worker, FlowTransition::Activated).await;
    let ready = project.path().join("ready");
    let later = project.path().join("later");
    let command = format!(
        "while [ ! -f '{}' ]; do sleep 0.05; done; printf rea; sleep 0.01; printf dy; while [ ! -f '{}' ]; do sleep 0.05; done; printf later; exec sleep 60",
        ready.display(),
        later.display()
    );
    let setup = invoke_with_env(
        &worker,
        runtime.executor.clone(),
        [("command".into(), command.into())].into(),
    )
    .await
    .result
    .unwrap()
    .to_json()
    .unwrap();
    let pause = Uuid::now_v7().into();
    change(&worker, FlowTransition::ExecutionPaused { key: pause }).await;
    std::fs::write(ready, "go").unwrap();
    let mut commits = worker.subscribe_commits();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            commits.borrow_and_update();
            if worker.snapshot().await.unwrap().pending.len() == 2 {
                break;
            }
            commits.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let pending = worker.snapshot().await.unwrap().pending;
    assert_eq!(pending.len(), 2);
    assert!(pending.iter().any(|input| matches!(&input.request.payload, InboxPayload::ProcessEvent { content, .. } if content["subscriptions"].as_array().is_some_and(|reasons| reasons.len() == 2))));
    let cursor = worker.snapshot().await.unwrap().cursor;
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        worker.snapshot().await.unwrap().cursor,
        cursor,
        "quiet sources must not spin or write duplicate matches"
    );
    let persistent = Uuid::parse_str(
        setup[2]["watcher_id"]
            .as_str()
            .unwrap()
            .strip_prefix("w_")
            .unwrap(),
    )
    .unwrap();
    assert!(
        worker
            .unwatch(persistent, |_| Err(
                std::io::ErrorKind::PermissionDenied.into()
            ))
            .await
            .is_err()
    );
    assert_eq!(worker.snapshot().await.unwrap().cursor, cursor);
    let owner = worker.address().clone();
    let before = worker.state().await.unwrap();
    let operator_id = Uuid::now_v7();
    let process_handle = setup[0]["handle"].as_str().unwrap().to_owned();
    let sources = session
        .process_watch_sources(worker.address().instance_id.clone())
        .await
        .unwrap();
    assert_eq!(sources.len(), 2);
    assert!(sources.iter().any(|entry| matches!(
        entry.source,
        atman_runtime::flow_store::process_watch::ProcessSource::Terminal { .. }
    )));
    let selected = sources
        .iter()
        .find(|entry| entry.source.handle() == process_handle)
        .unwrap()
        .source
        .clone();
    let exact = atman_runtime::flow_store::process_watch::ProcessWatch {
        id: operator_id,
        source: selected,
        pattern: "operator-only".into(),
        once: false,
        expires_at: None,
    };
    for entry in &sources {
        use atman_runtime::flow_store::process_watch::ProcessSource;
        let mut stale = exact.clone();
        stale.source = entry.source.clone();
        match &mut stale.source {
            ProcessSource::Bash { incarnation, .. }
            | ProcessSource::Terminal { incarnation, .. } => *incarnation = Uuid::now_v7(),
        }
        assert!(
            worker
                .watch_process_source(stale, |_| Ok(()))
                .await
                .unwrap_err()
                .to_string()
                .contains("incarnation")
        );
    }
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = attempts.clone();
    assert_eq!(
        worker
            .watch_process_source(exact.clone(), move |_| {
                if calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 2 {
                    Err(io::ErrorKind::PermissionDenied.into())
                } else {
                    Ok(())
                }
            })
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(attempts.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(worker.snapshot().await.unwrap().cursor, cursor);
    assert_eq!(
        worker
            .watch_process(
                operator_id,
                "unknown".into(),
                "operator-only".into(),
                false,
                None,
                |_| Err(io::ErrorKind::PermissionDenied.into())
            )
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    let expected = owner.clone();
    let registration = worker
        .watch_process_source(exact.clone(), move |actual| {
            assert_eq!(actual, &expected);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        worker
            .watch_process(
                operator_id,
                process_handle.clone(),
                "operator-only".into(),
                false,
                None,
                |_| Ok(())
            )
            .await
            .unwrap(),
        registration
    );
    worker.unwatch(operator_id, |_| Ok(())).await.unwrap();
    let mut conflict = exact.clone();
    conflict.pattern = "different".into();
    assert!(
        worker
            .watch_process_source(conflict, |_| Ok(()))
            .await
            .is_err()
    );
    assert_eq!(
        worker
            .watch_process_source(exact.clone(), |_| Ok(()))
            .await
            .unwrap()
            .status,
        atman_runtime::flow_store::process_watch::ProcessWatchStatus::Cancelled
    );
    assert_eq!(
        worker
            .watch_process(
                operator_id,
                process_handle,
                "operator-only".into(),
                false,
                None,
                |_| Ok(())
            )
            .await
            .unwrap()
            .status,
        atman_runtime::flow_store::process_watch::ProcessWatchStatus::Cancelled
    );
    let cancelled = worker
        .unwatch(persistent, move |address| {
            assert_eq!(address, &owner);
            Ok(())
        })
        .await
        .unwrap();
    assert_eq!(
        worker.unwatch(persistent, |_| Ok(())).await.unwrap(),
        cancelled
    );
    assert_eq!(worker.state().await.unwrap(), before);
    assert_eq!(worker.snapshot().await.unwrap().pending.len(), 2);
    change(&worker, FlowTransition::ExecutionResumed { key: pause }).await;
    let result = invoke_with_env(
        &worker,
        runtime.executor.clone(),
        [
            ("action".into(), "receive".into()),
            ("background".into(), setup[0]["handle"].clone()),
            ("terminal".into(), setup[1]["handle"].clone()),
            ("persist".into(), setup[2]["watcher_id"].clone()),
            (
                "release".into(),
                format!("touch '{}'", later.display()).into(),
            ),
        ]
        .into(),
    )
    .await
    .result
    .unwrap()
    .to_json()
    .unwrap();
    assert!(
        result[0].as_str().unwrap().starts_with("1 watcher"),
        "{result}"
    );
    assert!(
        result[1].as_str().unwrap().starts_with("1 watcher"),
        "{result}"
    );
    assert_eq!(result[2], serde_json::json!([]));
    assert!(worker.snapshot().await.unwrap().pending.is_empty());
    let messages = worker.context().messages_full();
    assert_eq!(
        messages
            .iter()
            .filter(|message| message.origin == MessageOrigin::Watcher)
            .count(),
        4
    );
    let address = worker.address().clone();
    tokio::time::timeout(Duration::from_secs(10), host.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert!(worker.process_watch_sources().is_empty());
    let stored = root
        .open_session(info.id)
        .unwrap()
        .open_flow(&address.instance_id)
        .unwrap();
    assert!(stored.pending_inputs().next().is_none());
    assert_eq!(
        stored
            .context_messages()
            .iter()
            .filter(|message| message.origin == MessageOrigin::Watcher)
            .count(),
        4
    );
    drop(stored);
    let reopened_host = RuntimeHost::default();
    let reopened = reopened_host
        .attach_session(root.open_session(info.id).unwrap())
        .unwrap()
        .open_flow(address.instance_id)
        .await
        .unwrap();
    assert!(reopened.process_watch_sources().is_empty());
    let restored_cursor = reopened.snapshot().await.unwrap().cursor;
    assert_eq!(
        reopened
            .watch_process_source(exact, |_| Ok(()))
            .await
            .unwrap()
            .status,
        atman_runtime::flow_store::process_watch::ProcessWatchStatus::Cancelled
    );
    assert_eq!(reopened.snapshot().await.unwrap().cursor, restored_cursor);
    reopened_host.shutdown().await.unwrap();
}

#[tokio::test]
async fn structured_watch_tools_observe_answers_completions_and_closure_through_one_inbox() {
    use atman_runtime::ValueJson;
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let observer = session
        .create_flow(program_descriptor(
            info.id,
            r#"
        flow main() -> any {
            contract { capabilities { shell: true } }
            when env("action") == "invalid" {
                return watch(handle: env("source"), event: "instance_closed", pattern: "bad")
            }
            when env("action") == "consume" {
                return [wait_for_watcher(timeout_ms: 4000), watcher.list()]
            }
            first = watch(handle: env("source"))
            second = watch(handle: env("source"), event: "turn_completed", turn_id: env("turn"))
            third = watch(handle: env("source"), event: "instance_closed", mode: "persist")
            return watcher.list()
        }
    "#
            .into(),
        ))
        .await
        .unwrap();
    let source = session
        .create_flow(program_descriptor(
            info.id,
            r#"
        flow main() -> string { return final.answer(message: "structured-ready") }
    "#
            .into(),
        ))
        .await
        .unwrap();
    for worker in [&observer, &source] {
        change(worker, FlowTransition::Activated).await;
    }
    let executor = Executor::new();
    atman_runtime::tools::register_watch(&executor.tools);
    let address = source.address().instance_id.to_string();
    let invalid = invoke_with_env(
        &observer,
        executor.clone(),
        [
            ("action".into(), "invalid".into()),
            ("source".into(), address.clone().into()),
        ]
        .into(),
    )
    .await;
    let error = invalid.result.unwrap_err().to_string();
    assert!(
        error.contains("invalid event, pattern, or turn_id combination"),
        "{error}"
    );
    assert!(observer.flow_subscriptions().await.unwrap().is_empty());
    change(&observer, FlowTransition::InterruptionRecovered).await;
    source
        .accept_input(peer_request(observer.address(), source.address()), |_| {
            Ok(())
        })
        .await
        .unwrap();
    let request = prepare(&source.snapshot().await.unwrap()).unwrap().unwrap();
    let list = invoke_with_env(
        &observer,
        executor.clone(),
        [
            ("source".into(), address.into()),
            ("turn".into(), request.turn.to_string().into()),
        ]
        .into(),
    )
    .await
    .result
    .unwrap()
    .to_json()
    .unwrap();
    assert_eq!(list.as_array().unwrap().len(), 3);
    assert_eq!(list[0]["filter"]["kind"], "final_answers");
    assert_eq!(list[1]["filter"]["kind"], "turn_completed");
    assert_eq!(list[2]["filter"]["kind"], "instance_closed");
    let pause = Uuid::new_v4().into();
    change(&observer, FlowTransition::ExecutionPaused { key: pause }).await;
    source
        .start_turn(executor.clone(), request)
        .await
        .unwrap()
        .wait()
        .await
        .unwrap()
        .result
        .unwrap();
    wait_inputs(&observer, 2).await;
    source
        .control(
            control_request(
                &source,
                source.state().await.unwrap().revision(),
                Uuid::from_bytes(*b"atman-user-pause").into(),
                PeerControl::Close,
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    wait_closed(&source).await;
    let pending = wait_inputs(&observer, 3).await;
    let kinds: Vec<_> = pending
        .pending
        .iter()
        .map(|input| {
            let InboxPayload::FlowEvent { event, .. } = &input.request.payload else {
                panic!("flow event")
            };
            event.as_str()
        })
        .collect();
    assert_eq!(kinds, ["final_answer", "turn_completed", "instance_closed"]);
    change(&observer, FlowTransition::ExecutionResumed { key: pause }).await;
    let consumed = invoke_with_env(
        &observer,
        executor,
        [("action".into(), "consume".into())].into(),
    )
    .await
    .result
    .unwrap()
    .to_json()
    .unwrap();
    assert_eq!(consumed, serde_json::json!([null, []]));
    assert!(observer.snapshot().await.unwrap().pending.is_empty());
    assert_eq!(
        observer
            .context()
            .messages_full()
            .iter()
            .filter(|message| message.origin == MessageOrigin::Watcher)
            .count(),
        3
    );
    let cursor = observer.snapshot().await.unwrap().cursor;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(observer.snapshot().await.unwrap().cursor, cursor);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn flow_watch_tools_merge_spawn_results_and_support_bidirectional_delivery() {
    use atman_runtime::{
        Value, ValueJson,
        tool::{Tier, ToolDefinition},
    };
    let dir = tempfile::tempdir().unwrap();
    let child_file = dir.path().join("watched.at");
    std::fs::write(
        &child_file,
        r#"
        flow child(input: string) -> string {
            contract { invocation { user_message: input } capabilities { shell: true } }
            when input == "start" {
                reverse = watch(handle: env("parent"), pattern: "parent-finished")
                await_release()
            }
            return final.answer(message: "child-ready")
        }
    "#,
    )
    .unwrap();
    let target = serde_json::to_string(&format!("{}@child", child_file.display())).unwrap();
    let source = format!(
        r#"
        flow main() -> any {{
            contract {{ capabilities {{ shell: true }} }}
            when env("action") == "finish" {{ return final.answer(message: "parent-finished") }}
            when env("action") == "consume" {{
                next = watch(handle: env("child"), pattern: "child-ready")
                flow.interject(handle: env("child"), text: "again")
                received = wait_for_watcher(timeout_ms: 4000)
                watcher.unwatch(watcher_id: env("persist"))
                absent = watch(handle: env("child"), pattern: "absent", timeout_ms: 100)
                expired = wait_for_watcher(timeout_ms: 4000)
                return [received, expired, watcher.list()]
            }}
            inventory = flow.instances()
            child = flow.spawn(flow: {target}, spawn_token: inventory.spawn_token, arguments: {{input: "start"}})
            first = watch(handle: child.handle, pattern: "child-ready", mode: "persist")
            second = watch(handle: child.handle, pattern: "child-ready")
            return [child, first, second, watcher.list()]
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
    let executor = Executor::new();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    atman_runtime::tools::register_watch(&executor.tools);
    let gate = release.clone();
    executor
        .tools
        .register_fn(
            ToolDefinition::new("await_release", Tier::Zero),
            move |_, _| {
                let gate = gate.clone();
                async move {
                    gate.acquire().await.unwrap().forget();
                    Ok(Value::Unit)
                }
            },
        )
        .unwrap();
    let setup = invoke_with_env(
        &parent,
        executor.clone(),
        [(
            "parent".into(),
            parent.address().instance_id.to_string().into(),
        )]
        .into(),
    )
    .await
    .result
    .unwrap()
    .to_json()
    .unwrap();
    assert_eq!(
        setup[3].as_array().unwrap().len(),
        2,
        "internal spawn routes must not appear in watcher.list"
    );
    assert_eq!(setup[3][0]["source"]["kind"], "flow");
    let child_address: FlowRef = serde_json::from_value(setup[0]["address"].clone()).unwrap();
    let child = host.flow(&child_address).await.unwrap();
    let pause = Uuid::new_v4().into();
    change(&parent, FlowTransition::ExecutionPaused { key: pause }).await;
    release.add_permits(1);
    let accepted = wait_inputs(&parent, 1).await;
    assert!(
        matches!(&accepted.pending[0].request.payload, InboxPayload::FlowEvent { content, .. } if content["subscriptions"].as_array().unwrap().len() == 3)
    );
    change(&parent, FlowTransition::ExecutionResumed { key: pause }).await;
    let result = invoke_with_env(
        &parent,
        executor.clone(),
        [
            ("action".into(), "consume".into()),
            ("child".into(), setup[0]["handle"].clone()),
            ("persist".into(), setup[1]["watcher_id"].clone()),
        ]
        .into(),
    )
    .await
    .result
    .unwrap()
    .to_json()
    .unwrap();
    assert!(
        result[0].as_str().unwrap().starts_with("1 watcher"),
        "{result}"
    );
    assert!(
        result[1].as_str().unwrap().starts_with("1 watcher"),
        "{result}"
    );
    assert_eq!(result[2], serde_json::json!([]));
    assert!(parent.snapshot().await.unwrap().pending.is_empty());
    assert_eq!(
        parent
            .context()
            .messages_full()
            .iter()
            .filter(|message| message.origin == MessageOrigin::Watcher)
            .count(),
        3
    );
    invoke_with_env(
        &parent,
        executor,
        [("action".into(), "finish".into())].into(),
    )
    .await
    .result
    .unwrap();
    wait_inputs(&parent, 1).await;
    wait_answers(&child, 3).await;
    let completion = child.completion(None).await.unwrap().unwrap();
    let mut observed = parent.subscribe_commits();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            observed.borrow_and_update();
            if parent
                .flow_subscriptions()
                .await
                .unwrap()
                .iter()
                .any(|record| {
                    record.subscription.purpose
                        == atman_runtime::flow_store::subscription::SubscriptionPurpose::SpawnResult
                        && record.cursor >= completion.receipt.sequence
                })
            {
                break;
            }
            observed.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(
        child
            .context()
            .messages_full()
            .iter()
            .filter(|message| message.origin == MessageOrigin::Watcher)
            .count(),
        1
    );
    let cursor = parent.snapshot().await.unwrap().cursor;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        parent.snapshot().await.unwrap().cursor,
        cursor,
        "idle subscriptions must not spin"
    );
    host.shutdown().await.unwrap();
    let stored = root
        .open_session(info.id)
        .unwrap()
        .open_flow(&parent.address().instance_id)
        .unwrap();
    assert_eq!(stored.pending_inputs().count(), 1);
    assert!(
        stored
            .flow_subscriptions()
            .filter(|record| record.subscription.purpose
                == atman_runtime::flow_store::subscription::SubscriptionPurpose::Watcher)
            .all(|record| record.status
                != atman_runtime::flow_store::subscription::SubscriptionStatus::Active)
    );
}

#[tokio::test]
async fn process_resources_belong_to_flow_instances_across_turns_and_storage_handoff() {
    use atman_runtime::task_registry::{TaskFilter, TaskKind};
    use atman_runtime::{AtmanRuntime, AtmanRuntimeOptions, ValueJson};

    let dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let config = atman_runtime::config_hub::ConfigHub::from_config_dir(dir.path());
    let mut info = session_descriptor();
    info.configuration = atman_runtime::flow_store::SessionConfiguration::Atman(config.source());
    info.project = Some(
        atman_runtime::storage::ProjectContext::resolve(&config, project.path(), dir.path())
            .unwrap(),
    );
    let runtime = AtmanRuntime::build(AtmanRuntimeOptions {
        events: atman_runtime::event::EventSink::new(),
        mock: false,
        config,
        project: info.project.clone(),
        home_dir: None,
        workspace_generation: "instance-processes".into(),
    })
    .await
    .unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let mut session = root.create_session(info.clone()).unwrap();
    let source = r#"
        flow main() -> any {
            contract { capabilities { shell: true } }
            when env("action") == "inspect" {
                return [bash.status(handle: env("background")), term.capture(handle: env("terminal")), task.list()]
            }
            background = bash.spawn(cmd: "printf process-ready; exec sleep 60", timeout_ms: 0)
            terminal = term.spawn(cmd: "printf terminal-ready; exec sleep 60")
            return [background, terminal, task.list()]
        }
    "#;
    let first = FlowWorker::start(
        session
            .create_flow(program_descriptor(info.id, source.into()))
            .unwrap(),
    )
    .await
    .unwrap();
    let second = FlowWorker::start(
        session
            .create_flow(program_descriptor(info.id, source.into()))
            .unwrap(),
    )
    .await
    .unwrap();
    change(&first, FlowTransition::Activated).await;
    change(&second, FlowTransition::Activated).await;
    let first_result = invoke(&first, runtime.executor.clone())
        .await
        .result
        .unwrap()
        .to_json()
        .unwrap();
    let second_result = invoke(&second, runtime.executor.clone())
        .await
        .result
        .unwrap()
        .to_json()
        .unwrap();
    let background = first_result[0]["handle"].as_str().unwrap();
    let terminal = first_result[1]["handle"].as_str().unwrap();
    assert!(
        std::path::Path::new(first_result[0]["log_path"].as_str().unwrap())
            .starts_with(first.context().dir())
    );
    assert_ne!(first.context().dir(), second.context().dir());
    assert!(
        first
            .context()
            .dir()
            .ends_with(first.address().instance_id.to_string())
    );
    assert_eq!(first_result[2].as_array().unwrap().len(), 3);
    assert_eq!(second_result[2].as_array().unwrap().len(), 3);
    assert!(
        runtime
            .executor
            .tool_ctx
            .task_registry
            .as_ref()
            .unwrap()
            .list(&TaskFilter::all())
            .is_empty()
    );
    assert!(
        second
            .resources()
            .background()
            .lookup(background, &info.id.to_string())
            .is_err()
    );
    assert!(
        second
            .resources()
            .terminals()
            .lookup(terminal, &info.id.to_string())
            .is_err()
    );

    let pause = Uuid::now_v7().into();
    change(&first, FlowTransition::ExecutionPaused { key: pause }).await;
    assert!(matches!(
        *first
            .resources()
            .background()
            .lookup(background, &info.id.to_string())
            .unwrap()
            .status
            .lock()
            .unwrap(),
        atman_runtime::tools::bash_bg::BgStatus::Running { .. }
    ));
    change(&first, FlowTransition::ExecutionResumed { key: pause }).await;
    let inspect = [
        ("action".into(), "inspect".into()),
        ("background".into(), background.into()),
        ("terminal".into(), terminal.into()),
    ]
    .into();
    let resumed = invoke_with_env(&first, runtime.executor.clone(), inspect)
        .await
        .result
        .unwrap()
        .to_json()
        .unwrap();
    assert_eq!(resumed[0]["status"], "running");
    assert!(resumed[1].to_string().contains("terminal-ready"));
    assert_eq!(resumed[2].as_array().unwrap().len(), 3);

    let context = first.context();
    let resources = first.resources().clone();
    first
        .control(
            control_request(
                &first,
                first.state().await.unwrap().revision(),
                Uuid::from_bytes(*b"atman-user-pause").into(),
                PeerControl::Close,
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    wait_closed(&first).await;
    let store = tokio::time::timeout(Duration::from_secs(10), first.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert!(
        resources
            .tasks()
            .list(&TaskFilter::all())
            .iter()
            .all(|task| task.status.is_terminal())
    );
    assert!(
        std::fs::read_to_string(first_result[0]["log_path"].as_str().unwrap())
            .unwrap()
            .contains("process-ready")
    );
    assert_eq!(context.dir(), store.dir());
    assert!(store.execution_events().iter().flat_map(|batch| &batch.events).any(|event| {
        matches!(&event.event, atman_runtime::event::Event::TerminalFinalState { handle, .. } if handle == terminal)
    }));
    let restored = FlowWorker::start(store).await.unwrap();
    assert_eq!(
        restored.state().await.unwrap().state(),
        &InstanceState::Closed
    );
    assert!(
        restored
            .resources()
            .tasks()
            .list(&TaskFilter::all())
            .is_empty()
    );
    assert!(
        second
            .resources()
            .tasks()
            .list(&TaskFilter::all())
            .iter()
            .filter(|task| task.kind != TaskKind::Flow)
            .all(|task| !task.status.is_terminal())
    );
    let second_resources = second.resources().clone();
    // Storage ownership ends execution resources even when readers retain their projections.
    let _second_context = second.context();
    tokio::time::timeout(Duration::from_secs(10), second.shutdown())
        .await
        .unwrap()
        .unwrap();
    assert!(
        second_resources
            .tasks()
            .list(&TaskFilter::all())
            .iter()
            .all(|task| task.status.is_terminal())
    );
    restored.shutdown().await.unwrap();
}

#[tokio::test]
async fn peer_flows_share_runtime_mcp_without_owning_connections() {
    use atman_runtime::{AtmanRuntime, AtmanRuntimeOptions, ValueJson};
    let dir = tempfile::tempdir().unwrap();
    let config = atman_runtime::config_hub::ConfigHub::from_config_dir(dir.path());
    let project_path = dir.path().join("alpha");
    std::fs::create_dir(&project_path).unwrap();
    let project =
        atman_runtime::storage::ProjectContext::resolve(&config, &project_path, dir.path())
            .unwrap();
    let script = dir.path().join("server.py");
    std::fs::write(
        &script,
        r#"
import json, os, sys
for line in sys.stdin:
    req = json.loads(line)
    if "id" not in req:
        continue
    method = req["method"]
    if method == "initialize":
        result = {"protocolVersion": "2024-11-05", "capabilities": {}}
    elif method == "tools/list":
        result = {"tools": [{"name": "identify", "inputSchema": {"type": "object"}}]}
    else:
        result = {"content": [{"type": "text", "text": json.dumps({"pid": os.getpid(), "cwd": os.getcwd()})}]}
    print(json.dumps({"jsonrpc": "2.0", "id": req["id"], "result": result}), flush=True)
"#,
    )
    .unwrap();
    config
        .save_mcp(&[atman_runtime::mcp::McpServerConfig::stdio(
            "shared",
            "python3",
            vec![script.to_string_lossy().into()],
            atman_runtime::tool::Tier::Zero,
            5000,
        )])
        .unwrap();
    let runtime = AtmanRuntime::build(AtmanRuntimeOptions {
        events: atman_runtime::event::EventSink::new(),
        mock: false,
        config: config.clone(),
        project: Some(project.clone()),
        home_dir: None,
        workspace_generation: "peer-mcp".into(),
    })
    .await
    .unwrap();
    let child_path = dir.path().join("mcp-child.at");
    let helper = r#"
        flow identify() -> any {
            mcp.await(server: "shared")
            return mcp.call(server: "shared", tool: "identify", input: {})
        }
    "#;
    std::fs::write(
        &child_path,
        format!(
            r#"
        flow child() -> any {{ return identify().await }}
        {helper}
    "#
        ),
    )
    .unwrap();
    let reference = serde_json::to_string(&format!("{}@child", child_path.display())).unwrap();
    let source = format!(
        r#"
        flow main() -> any {{
            contract {{ capabilities {{ shell: true }} }}
            before = identify().await
            inventory = flow.instances()
            child = flow.spawn(flow: {reference}, spawn_token: inventory.spawn_token, async: false)
            return [before, child, identify().await]
        }}
        {helper}
    "#
    );
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let mut info = session_descriptor();
    info.project = Some(project.clone());
    info.configuration = atman_runtime::flow_store::SessionConfiguration::Atman(config.source());
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let parent = session
        .create_flow(program_descriptor(info.id, source.clone()))
        .await
        .unwrap();
    change(&parent, FlowTransition::Activated).await;
    let result = tokio::time::timeout(
        Duration::from_secs(10),
        invoke(&parent, runtime.executor.clone()),
    )
    .await
    .unwrap()
    .result
    .unwrap()
    .to_json()
    .unwrap();
    assert_eq!(result[0], result[1]);
    assert_eq!(result[0], result[2]);
    let identity = |result: &serde_json::Value| {
        serde_json::from_str::<serde_json::Value>(result[0]["text"].as_str().unwrap()).unwrap()
    };
    assert_eq!(
        identity(&result)["cwd"],
        project.root().to_string_lossy().as_ref()
    );
    assert!(parent.context().subscribe_context().borrow().mcp_servers[0].is_ok());
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
    assert!(child.context().subscribe_context().borrow().mcp_servers[0].is_ok());
    let other_config =
        atman_runtime::config_hub::ConfigHub::from_config_dir(dir.path().join("other-config"));
    other_config.save_mcp(&config.load_mcp()).unwrap();
    let other_path = dir.path().join("beta");
    std::fs::create_dir(&other_path).unwrap();
    let other_project =
        atman_runtime::storage::ProjectContext::resolve(&other_config, &other_path, dir.path())
            .unwrap();
    let other_runtime = AtmanRuntime::build(AtmanRuntimeOptions {
        events: atman_runtime::event::EventSink::new(),
        mock: false,
        config: other_config.clone(),
        project: Some(other_project.clone()),
        home_dir: None,
        workspace_generation: "isolated-peer-mcp".into(),
    })
    .await
    .unwrap();
    let mut other_info = session_descriptor();
    other_info.project = Some(other_project.clone());
    other_info.configuration =
        atman_runtime::flow_store::SessionConfiguration::Atman(other_config.source());
    let other_session = host
        .attach_session(root.create_session(other_info.clone()).unwrap())
        .unwrap();
    let other = other_session
        .create_flow(program_descriptor(other_info.id, source))
        .await
        .unwrap();
    change(&other, FlowTransition::Activated).await;
    let isolated = tokio::time::timeout(
        Duration::from_secs(10),
        invoke(&other, other_runtime.executor.clone()),
    )
    .await
    .unwrap()
    .result
    .unwrap()
    .to_json()
    .unwrap();
    assert_eq!(isolated[0], isolated[1]);
    assert_eq!(isolated[0], isolated[2]);
    assert_ne!(identity(&isolated)["pid"], identity(&result)["pid"]);
    assert_eq!(
        identity(&isolated)["cwd"],
        other_project.root().to_string_lossy().as_ref()
    );
    runtime
        .executor
        .tool_ctx
        .mcp_service()
        .unwrap()
        .shutdown()
        .await;
    for context in [parent.context(), child.context()] {
        assert!(matches!(
            context.subscribe_context().borrow().mcp_servers[0].state,
            atman_runtime::mcp::McpServerState::Disconnected { .. }
        ));
    }
    assert!(other.context().subscribe_context().borrow().mcp_servers[0].is_ok());
    let after = tokio::time::timeout(
        Duration::from_secs(10),
        invoke(&other, other_runtime.executor.clone()),
    )
    .await
    .unwrap()
    .result
    .unwrap()
    .to_json()
    .unwrap();
    assert_eq!(after, isolated);
    other_runtime
        .executor
        .tool_ctx
        .mcp_service()
        .unwrap()
        .shutdown()
        .await;
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn runtime_projects_bind_rules_and_spawned_flows_without_borrowing_another_project() {
    use atman_runtime::config_hub::ConfigHub;
    use atman_runtime::flow_store::SessionConfiguration;
    use atman_runtime::storage::ProjectContext;
    use atman_runtime::{AtmanRuntime, AtmanRuntimeOptions, ValueJson};
    let data = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    std::fs::write(
        home.path().join(".claude/CLAUDE.md"),
        "# global-rule\nshared rule",
    )
    .unwrap();
    for (project, content) in [(first.path(), "alpha"), (second.path(), "beta")] {
        std::fs::write(
            project.join("AGENTS.md"),
            format!("# project-rule\n{content}"),
        )
        .unwrap();
    }
    let hub = ConfigHub::from_config_dir(config.path());
    let bindings = [
        Some(ProjectContext::resolve(&hub, first.path(), data.path()).unwrap()),
        Some(ProjectContext::resolve(&hub, second.path(), data.path()).unwrap()),
        None,
    ];
    let options = |project| AtmanRuntimeOptions {
        events: atman_runtime::event::EventSink::new(),
        mock: false,
        config: hub.clone(),
        project,
        home_dir: Some(home.path().to_path_buf()),
        workspace_generation: "bound-projects".into(),
    };
    let alpha = AtmanRuntime::build(options(bindings[0].clone()))
        .await
        .unwrap();
    let beta = AtmanRuntime::build(options(bindings[1].clone()))
        .await
        .unwrap();
    let global = AtmanRuntime::build(options(None)).await.unwrap();
    assert!(global.executor.tool_ctx.flow_workspace_service.is_none());
    assert!(global.executor.tool_ctx.resolve_cwd(None).is_err());
    assert_eq!(
        alpha.executor.tool_ctx.resolve_cwd(None).unwrap(),
        bindings[0].as_ref().unwrap().root()
    );
    let runtimes = [&alpha, &beta, &global];
    let root = RuntimeStore::open(data.path()).unwrap();
    let host = RuntimeHost::default();
    let child = data.path().join("child.at");
    std::fs::write(
        &child,
        r#"flow child() -> string { return rule.fetch(name: "project-rule") }"#,
    )
    .unwrap();
    let reference = serde_json::to_string(&format!("{}@child", child.display())).unwrap();
    let source = format!(
        r#"
        flow main() -> any {{
            contract {{ capabilities {{ shell: true }} }}
            inventory = flow.instances()
            child = flow.spawn(flow: {reference}, spawn_token: inventory.spawn_token, async: false)
            return [rule.fetch(name: "project-rule"), child, rule.fetch(name: "global-rule")]
        }}
    "#
    );
    for (index, binding) in bindings.iter().enumerate() {
        let mut info = session_descriptor();
        info.project = binding.clone();
        info.configuration = SessionConfiguration::Atman(hub.source());
        let session = host
            .attach_session(root.create_session(info.clone()).unwrap())
            .unwrap();
        let worker = session
            .create_flow(program_descriptor(info.id, source.clone()))
            .await
            .unwrap();
        change(&worker, FlowTransition::Activated).await;
        let mut input = peer_request(worker.address(), worker.address());
        input.sender = SenderIdentity::User(Uuid::new_v4());
        worker.accept_input(input, |_| Ok(())).await.unwrap();
        let before = worker.snapshot().await.unwrap();
        let wrong = &runtimes[(index + 1) % runtimes.len()].executor;
        assert!(worker.schedule(wrong.clone(), prepare).is_err());
        let error = worker
            .start_turn(wrong.clone(), prepare(&before).unwrap().unwrap())
            .await
            .err()
            .unwrap();
        assert!(error.to_string().contains("project"), "{error}");
        assert_eq!(worker.snapshot().await.unwrap().cursor, before.cursor);
        let completed = tokio::time::timeout(
            Duration::from_secs(10),
            invoke(&worker, runtimes[index].executor.clone()),
        )
        .await
        .unwrap();
        let result = completed.result.unwrap().to_json().unwrap();
        let expected = match index {
            0 => "# project-rule\nalpha",
            1 => "# project-rule\nbeta",
            _ => "",
        };
        assert_eq!(
            result,
            serde_json::json!([expected, expected, "# global-rule\nshared rule"])
        );
        assert_eq!(session.snapshot().await.unwrap().catalog.flows.len(), 2);
        if let Some(binding) = binding {
            assert!(
                !binding.storage_root().exists(),
                "reading rules must not initialize knowledge stores"
            );
        }
    }
}

#[tokio::test]
async fn runtime_project_knowledge_is_shared_by_peers_and_isolated_across_projects() {
    use atman_runtime::{AtmanRuntime, AtmanRuntimeOptions, ValueJson};
    use atman_runtime::{config_hub::ConfigHub, flow_store::SessionConfiguration};
    let dir = tempfile::tempdir().unwrap();
    let hub = ConfigHub::from_config_dir(dir.path());
    std::fs::write(
        dir.path().join("config.toml"),
        "[redact]\nenabled = true\ncustom_patterns = [{ kind = 'sample', regex = 'TOKEN-[0-9]+' }]\n",
    )
    .unwrap();
    let options = |project| AtmanRuntimeOptions {
        events: atman_runtime::event::EventSink::new(),
        mock: false,
        config: hub.clone(),
        project,
        home_dir: None,
        workspace_generation: "project-knowledge".into(),
    };
    let projectless = AtmanRuntime::build(options(None)).await.unwrap();
    for name in ["memory.confess", "memory.spec.read"] {
        assert!(projectless.executor.tools.get(name).is_none());
    }
    atman_runtime::templates::ensure_managed_agent_at(dir.path()).unwrap();
    let program = atman_runtime::source_program::load_program(
        dir.path().join("commands/agent.at"),
        &atman_runtime::source_program::SourceRoots {
            config_dir: Some(dir.path().to_owned()),
            project_root: None,
        },
    )
    .unwrap();
    let entry = program.flow(&program.entry_flow("agent").unwrap()).unwrap();
    atman_runtime::validate::validate(entry, &projectless.executor.tools).unwrap();
    let empty = atman_rt::parse_file(
        "flow read() -> any { return [memory.fetch_confessions(), memory.fetch_confessions(trigger: \"test\")] }",
    ).unwrap();
    let result = projectless
        .executor
        .run(&empty, "read", vec![])
        .await
        .unwrap();
    assert_eq!(result.to_json().unwrap(), serde_json::json!([[], []]));
    let reader = r#"
        flow read() -> any {
            return [memory.fetch_confessions(), memory.spec.read(feature: "shared")]
        }
    "#;
    let helper = format!(
        r#"
        flow write(label: string) -> any {{
            memory.confess(trigger: label, rule_violated: "rule", what_i_did: "TOKEN-42",
                why: "test", mitigation: "review")
            memory.spec.update(feature: "shared", phase: "research", content: label)
            return read().await
        }}
        {reader}
    "#
    );
    let child_path = dir.path().join("knowledge.at");
    std::fs::write(
        &child_path,
        format!("flow child() -> any {{ return write(\"child\").await }}\n{helper}"),
    )
    .unwrap();
    let reference = serde_json::to_string(&format!("{}@child", child_path.display())).unwrap();
    let root = RuntimeStore::open(&dir.path().join("runtime")).unwrap();
    let host = RuntimeHost::default();
    for label in ["alpha", "beta"] {
        let project = tempfile::tempdir().unwrap();
        let binding = atman_runtime::storage::ProjectContext::resolve(
            &hub,
            project.path(),
            &dir.path().join("data"),
        )
        .unwrap();
        let runtime = AtmanRuntime::build(options(Some(binding.clone())))
            .await
            .unwrap();
        assert!(
            !binding.storage_root().exists(),
            "registration must not create knowledge directories"
        );
        let mut info = session_descriptor();
        info.project = Some(binding.clone());
        info.configuration = SessionConfiguration::Atman(hub.source());
        let session = host
            .attach_session(root.create_session(info.clone()).unwrap())
            .unwrap();
        let source = format!(
            r#"
            flow main() -> any {{
                contract {{ capabilities {{ shell: true }} }}
                write("{label}").await
                inventory = flow.instances()
                child = flow.spawn(flow: {reference}, spawn_token: inventory.spawn_token, async: false)
                return [child, read().await]
            }}
            {helper}
        "#
        );
        let worker = session
            .create_flow(program_descriptor(info.id, source))
            .await
            .unwrap();
        change(&worker, FlowTransition::Activated).await;
        let result = invoke(&worker, runtime.executor.clone())
            .await
            .result
            .unwrap()
            .to_json()
            .unwrap();
        assert_eq!(result[0], result[1], "peers share the project's knowledge");
        let confessions = result[0][0].as_array().unwrap();
        assert_eq!(confessions.len(), 2);
        assert_eq!(confessions[0]["trigger"], label);
        assert_eq!(confessions[1]["trigger"], "child");
        assert!(
            confessions
                .iter()
                .all(|item| item["what_i_did"] == "<REDACTED:sample>")
        );
        let entries = result[0][1]["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0]["content"], label);
        assert_eq!(entries[1]["content"], "child");
        drop(runtime);
        let restored = AtmanRuntime::build(options(Some(binding))).await.unwrap();
        let mut other_info = info.clone();
        other_info.id = Uuid::new_v4();
        let other_session = host
            .attach_session(root.create_session(other_info.clone()).unwrap())
            .unwrap();
        let other = other_session
            .create_flow(program_descriptor(
                other_info.id,
                format!("flow main() -> any {{ return read().await }}\n{reader}"),
            ))
            .await
            .unwrap();
        change(&other, FlowTransition::Activated).await;
        let recovered = invoke(&other, restored.executor)
            .await
            .result
            .unwrap()
            .to_json()
            .unwrap();
        assert_eq!(
            recovered, result[0],
            "another session uses the same saved project knowledge"
        );
    }
}

#[tokio::test]
async fn persistent_configuration_is_checked_before_scheduling_or_consuming_input() {
    use atman_runtime::config_hub::ConfigHub;
    use atman_runtime::flow_store::SessionConfiguration;
    use atman_runtime::{AtmanRuntime, AtmanRuntimeOptions};
    let dir = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let hub = ConfigHub::from_auth_path(config.path().join("account.json"));
    let mut info = session_descriptor();
    info.project = Some(
        atman_runtime::storage::ProjectContext::resolve(&hub, project.path(), dir.path()).unwrap(),
    );
    info.configuration = SessionConfiguration::Atman(hub.source());
    let root = RuntimeStore::open(dir.path()).unwrap();
    let mut store = root.create_session(info.clone()).unwrap();
    let descriptor = flow_descriptor(info.id);
    let worker = FlowWorker::start(store.create_flow(descriptor.clone()).unwrap())
        .await
        .unwrap();
    change(&worker, FlowTransition::Activated).await;
    let options = |config| AtmanRuntimeOptions {
        events: atman_runtime::event::EventSink::new(),
        mock: false,
        config,
        project: info.project.clone(),
        home_dir: None,
        workspace_generation: "persistent-config-test".into(),
    };
    let wrong = AtmanRuntime::build(options(ConfigHub::from_config_dir(config.path())))
        .await
        .unwrap();
    let runtime = AtmanRuntime::build(options(hub.clone())).await.unwrap();
    let mut request = peer_request(worker.address(), worker.address());
    request.sender = SenderIdentity::User(Uuid::new_v4());
    worker.accept_input(request, |_| Ok(())).await.unwrap();
    let before = worker.snapshot().await.unwrap();
    assert!(worker.schedule(wrong.executor.clone(), prepare).is_err());
    let mut unconfigured = Executor::new();
    unconfigured.tool_ctx = atman_runtime::tool::ToolCtx::for_project(info.project.clone());
    for executor in [wrong.executor.clone(), unconfigured] {
        let request = prepare(&before).unwrap().unwrap();
        let error = worker.start_turn(executor, request).await.err().unwrap();
        assert!(error.to_string().contains("session source"), "{error}");
    }
    let after = worker.snapshot().await.unwrap();
    assert_eq!(after.cursor, before.cursor);
    assert_eq!(after.state, before.state);
    assert_eq!(after.pending.len(), 1);
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
        .start_turn(runtime.executor.clone(), prepare(&after).unwrap().unwrap())
        .await
        .unwrap()
        .wait()
        .await
        .unwrap();
    assert_eq!(
        worker
            .context()
            .messages()
            .iter()
            .filter(|message| message.origin == MessageOrigin::FinalAnswer)
            .count(),
        1
    );
    drop(worker.shutdown().await.unwrap());
    drop(store);
    drop(runtime);
    drop(wrong);
    drop(hub);

    let store = root.open_session(info.id).unwrap();
    let SessionConfiguration::Atman(source) = &store.projection().descriptor.configuration else {
        panic!("missing source")
    };
    let restored_hub = ConfigHub::from_source(source).unwrap();
    assert_eq!(
        restored_hub.source().auth_path().file_name().unwrap(),
        "account.json"
    );
    let runtime = AtmanRuntime::build(options(restored_hub)).await.unwrap();
    let worker = FlowWorker::start(store.open_flow(&descriptor.address.instance_id).unwrap())
        .await
        .unwrap();
    assert!(worker.snapshot().await.unwrap().pending.is_empty());
    assert_eq!(
        worker
            .context()
            .messages()
            .iter()
            .filter(|message| message.origin == MessageOrigin::FinalAnswer)
            .count(),
        1
    );
    // Attaching to an idle restored flow retains history and does not invent a turn.
    let cursor = worker.snapshot().await.unwrap().cursor;
    let scheduler = worker.schedule(runtime.executor, |_| Ok(None)).unwrap();
    scheduler.stop().await.unwrap();
    assert_eq!(worker.snapshot().await.unwrap().cursor, cursor);
    drop(worker.shutdown().await.unwrap());
}

fn flow_descriptor(session: Uuid) -> FlowDescriptor {
    FlowDescriptor {
        address: FlowRef {
            session_id: session.into(),
            instance_id: FlowInstanceId::now(),
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
                    "flow main() -> string { return final.answer(message: \"delivered\") }".into(),
                )]
                .into(),
                edges: Default::default(),
            },
        }
        .into(),
    }
}

fn program_descriptor(session: Uuid, source: String) -> FlowDescriptor {
    let mut descriptor = flow_descriptor(session);
    descriptor
        .entry_mut()
        .unwrap()
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
    request.authority.workspace_root = executor
        .tool_ctx
        .project()
        .map(|project| project.root().to_path_buf());
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
        store: Some(fallback.clone()),
    }));
    executor.tools.register(Arc::new(MemoryGoalSet {
        store: Some(fallback.clone()),
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
                start_turn: false,
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
async fn planning_tools_follow_the_invocation_across_inline_spawn_and_reopen() {
    use atman_runtime::{AtmanRuntime, AtmanRuntimeOptions, ValueJson};
    let dir = tempfile::tempdir().unwrap();
    let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(dir.path());
    let runtime = AtmanRuntime::build(AtmanRuntimeOptions {
        events: atman_runtime::event::EventSink::new(),
        mock: false,
        config: hub.clone(),
        project: None,
        home_dir: None,
        workspace_generation: "flow-memory".into(),
    })
    .await
    .unwrap();
    let helper = r#"
        flow prepare(label: string) -> any {
            memory.goal.set(text: label)
            plan.write(id: "route", title: label, steps: ["read", "write"])
            plan.tick(step_index: 0)
            id = memory.todo.set(where: label, why: "isolation", how: "execute", expected_result: "saved")
            memory.todo.done(id: id)
            discarded = memory.todo.set(where: "temporary", why: "cancel", how: "delete", expected_result: "absent")
            memory.todo.cancel(id: discarded)
            memory.todo.delete(id: discarded)
            return [plan.read(), memory.todo.list()]
        }
    "#;
    let child_path = dir.path().join("planning.at");
    std::fs::write(
        &child_path,
        format!(
            r#"
        flow child() -> any {{
            initial = plan.read()
            result = prepare("child").await
            final.answer(message: "child planning saved")
            return [initial, result]
        }}
        {helper}
    "#
        ),
    )
    .unwrap();
    let reference = serde_json::to_string(&format!("{}@child", child_path.display())).unwrap();
    let source = format!(
        r#"
        flow main() -> any {{
            contract {{ capabilities {{ shell: true }} }}
            before = prepare("parent").await
            inventory = flow.instances()
            child = flow.spawn(flow: {reference}, spawn_token: inventory.spawn_token, async: false)
            final.answer(message: "parent planning saved")
            return [before, child, [plan.read(), memory.todo.list()]]
        }}
        {helper}
    "#
    );
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let mut info = session_descriptor();
    info.configuration = atman_runtime::flow_store::SessionConfiguration::Atman(hub.source());
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let parent = session
        .create_flow(program_descriptor(info.id, source))
        .await
        .unwrap();
    change(&parent, FlowTransition::Activated).await;
    let result = invoke(&parent, runtime.executor.clone())
        .await
        .result
        .unwrap()
        .to_json()
        .unwrap();
    assert_eq!(
        result[0], result[2],
        "child updates cannot replace the parent's plan/todos"
    );
    assert_eq!(
        result[1][0], "",
        "child must start with its own empty planning state"
    );
    assert_eq!(result[0][1][0]["where"], "parent");
    assert_eq!(result[1][1][1][0]["where"], "child");
    assert_ne!(result[0][1][0]["id"], result[1][1][1][0]["id"]);
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
    let parent_trace = parent.execution_events().await.unwrap();
    let child_trace = child.execution_events().await.unwrap();
    assert!(
        parent_trace
            .iter()
            .flat_map(|batch| &batch.events)
            .all(|event| event.scope.flow() == parent.address())
    );
    assert!(
        child_trace
            .iter()
            .flat_map(|batch| &batch.events)
            .all(|event| event.scope.flow() == child.address())
    );
    assert!(
        child_trace
            .iter()
            .flat_map(|batch| &batch.events)
            .any(|event| matches!(&event.event,
        atman_runtime::event::Event::FlowStart { flow_name, .. } if flow_name == "child"))
    );
    assert!(
        !parent_trace
            .iter()
            .flat_map(|batch| &batch.events)
            .any(|event| matches!(&event.event,
        atman_runtime::event::Event::FlowStart { flow_name, .. } if flow_name == "child"))
    );
    let snapshot = child.planning().await.unwrap();
    let child_goal = child.goal().await.unwrap().goal.unwrap();
    assert_eq!(child_goal.objective, "child");
    assert_eq!(
        parent.goal().await.unwrap().goal.unwrap().objective,
        "parent"
    );
    assert_eq!(snapshot.todos.len(), 1);
    assert_eq!(snapshot.plans[0].title, "child");
    assert_eq!(snapshot.plans[0].progress(), (1, 2));
    let expected_prompt = atman_runtime::tools::plan::render_plan(&snapshot.plans[0]);
    assert_eq!(
        child.context().plan_system_prompt().await.as_deref(),
        Some(expected_prompt.as_str())
    );
    child.context().refresh_plans_from_store_async().await;
    child.context().refresh_todos_from_store_async().await;
    child.context().refresh_todos_from_store();
    assert_eq!(*child.context().subscribe_todos().borrow(), snapshot.todos);
    assert_eq!(*child.context().subscribe_plans().borrow(), snapshot.plans);
    let address = child.address().clone();
    drop(child.shutdown().await.unwrap());
    let reopened = host.flow(&address).await.unwrap();
    assert_eq!(
        serde_json::to_value(reopened.execution_events().await.unwrap()).unwrap(),
        serde_json::to_value(child_trace).unwrap()
    );
    assert_eq!(reopened.planning().await.unwrap(), snapshot);
    assert_eq!(reopened.goal().await.unwrap().goal, Some(child_goal));
    assert_eq!(
        reopened.context().plan_system_prompt().await.as_deref(),
        Some(expected_prompt.as_str())
    );
    assert_eq!(
        *reopened.context().subscribe_todos().borrow(),
        snapshot.todos
    );
    assert_eq!(
        *reopened.context().subscribe_plans().borrow(),
        snapshot.plans
    );
    assert!(reopened.state().await.unwrap().can_start_turn());
    let unbound = atman_runtime::tool::ToolCtx::new().with_session_runtime(reopened.context());
    for name in ["memory.goal.get", "memory.todo.list", "plan.read"] {
        let tool = runtime.executor.tools.get(name).unwrap();
        let error = tool.call(Default::default(), &unbound).await.unwrap_err();
        assert!(
            error.to_string().contains("host binding"),
            "{name}: {error}"
        );
        assert!(
            tool.call(Default::default(), &Default::default())
                .await
                .is_err(),
            "{name} must not synthesize a legacy memory store"
        );
    }
    let lease = start(&parent).await;
    let client = host.bind_invocation(lease.clone()).await.unwrap();
    assert!(
        client.read_planning(address).await.is_err(),
        "creating a peer grants no context access"
    );
    assert_eq!(
        client
            .read_planning(parent.address().clone())
            .await
            .unwrap()
            .plans[0]
            .title,
        "parent"
    );
    let key = Uuid::new_v4().into();
    change(&parent, FlowTransition::ExecutionPaused { key }).await;
    assert!(
        client
            .edit_planning(
                Uuid::new_v4(),
                None,
                atman_runtime::flow_store::planning::PlanningEdit::DeleteTodo {
                    id: snapshot.todos[0].id.clone()
                }
            )
            .await
            .is_err(),
        "execution pause blocks new tool effects"
    );
    change(&parent, FlowTransition::ExecutionResumed { key }).await;
    change(
        &parent,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    )
    .await;
    assert!(
        client
            .edit_planning(
                Uuid::new_v4(),
                None,
                atman_runtime::flow_store::planning::PlanningEdit::DeleteTodo {
                    id: snapshot.todos[0].id.clone()
                }
            )
            .await
            .is_err(),
        "expired invocations cannot mutate planning"
    );
}

fn goal_executor(dir: &std::path::Path) -> Executor {
    use atman_runtime::{memory::goal::GoalStore, tools::memory::*};
    let executor = Executor::new();
    let store = Arc::new(GoalStore::at(dir));
    executor.tools.register(Arc::new(MemoryGoalGet {
        store: Some(store.clone()),
    }));
    executor
        .tools
        .register(Arc::new(MemoryGoalSet { store: Some(store) }));
    executor
        .tools
        .register(Arc::new(MemoryGoalControl { resume: false }));
    executor.tools.register(Arc::new(MemoryGoalSchedule));
    executor
}

#[tokio::test]
async fn recorded_entry_binding_recovers_identity_and_intersects_host_authority_without_resuming_work()
 {
    use atman_runtime::{
        Value, ValueJson,
        flow_store::launch::{EntryBinding, LaunchInput},
        tool::{Tier, ToolDefinition},
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let descriptor = program_descriptor(
        info.id,
        r#"
        flow main(input: string) -> string {
            contract { capabilities { shell: true } invocation { user_message: input } }
            return deliver(input).await
        }
        flow deliver(input: string) -> string {
            return final.answer(message: binding.inspect(input: input, effort: env("effort")))
        }
    "#
        .into(),
    );
    let address = descriptor.address.clone();
    let initial_turn = TurnId::now();
    let initial_run = RunId::now();
    let workspace = dir.path().canonicalize().unwrap();
    let mut ceiling = EffectiveAuthority::root(&Default::default(), false, Some(workspace.clone()));
    ceiling.allowed_tiers[1] = false;
    ceiling.permission_management = false;
    let worker = host
        .launch_flow(
            descriptor.clone(),
            LaunchInput {
                workspace: None,
                initial_goal: None,
                request: atman_runtime::flow_store::launch::EntryRequest {
                    request_id: descriptor.creation_request,
                    sender: SenderIdentity::User(Uuid::new_v4()),
                    recipient: address.clone(),
                    arguments: [("input".into(), "first".into())].into(),
                    invocation: [("effort".into(), "high".into())].into(),
                    start: atman_runtime::flow_store::launch::EntryStart::Invoke { user: None },
                },
                inherited_context: Vec::new(),
                result_route: None,
                execution: Some(EntryBinding {
                    user_routes: false,
                    session_start: false,
                    first_turn: initial_turn.clone(),
                    first_run: initial_run.clone(),
                    authority: ceiling.clone(),
                }),
            },
            |_, _| Ok(()),
        )
        .await
        .unwrap()
        .worker;
    let key = Uuid::new_v4().into();
    change(&worker, FlowTransition::ExecutionPaused { key }).await;
    let mut next = peer_request(&address, &address);
    next.request_id = descriptor.creation_request; // Equal request IDs from distinct senders are not the initial invocation.
    next.sender = SenderIdentity::User(Uuid::new_v4());
    next.delivery = DeliveryMode::NextTurn;
    next.payload = InboxPayload::UserSubmission {
        content: atman_runtime::flow_store::inbox::UserContent {
            text: "second".into(),
            images: vec![],
            presentation: Default::default(),
        },
        invocation: [("effort".into(), "low".into())].into(),
    };
    worker.accept_input(next, |_| Ok(())).await.unwrap();
    // A flow's commit channel does not acknowledge release of its session owner.
    // This entry has not run; shut down the host before testing persisted binding recovery.
    assert!(matches!(
        worker.state().await.unwrap().state(),
        InstanceState::Open(InstanceActivity::Idle)
    ));
    host.shutdown().await.unwrap();
    drop(worker);
    drop(session);
    drop(host);
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.open_session(info.id).unwrap())
        .unwrap();
    let worker = host.flow(&address).await.unwrap();
    assert!(worker.completion(None).await.unwrap().is_none());
    assert!(
        session
            .scheduling_status(address.instance_id.clone())
            .await
            .unwrap()
            .is_none()
    );
    let mut executor = Executor::new();
    executor.tool_ctx =
        executor
            .tool_ctx
            .with_workspace(atman_runtime::git_workspace::WorkspaceBinding {
                workspace_id: "unrelated-checkout".into(),
                path: workspace.join("another-project"),
                repository_root: workspace.join("another-project"),
                branch: None,
            });
    executor.tool_ctx.fs_access.mode = atman_runtime::fs_access::FsAccessMode::ReadOnly;
    executor
        .tools
        .register_fn(
            ToolDefinition::new("binding.inspect", Tier::Zero),
            |args, ctx| {
                let value = serde_json::json!({
                    "input": args.named("input").unwrap().to_json().unwrap(),
                    "effort": args.named("effort").unwrap().to_json().unwrap(),
                    "authority": ctx.flow_identity.as_ref().unwrap().effective_authority,
                    "cwd": ctx.resolve_cwd(None).unwrap(),
                    "fs_root": ctx.fs_access.workspace,
                    "workspace": ctx.workspace,
                    "fs_mode": ctx.fs_access.mode,
                });
                async move { Ok(Value::Str(value.to_string())) }
            },
        )
        .unwrap();
    let mut current = EffectiveAuthority::root(&Default::default(), true, Some(workspace.clone()));
    current.allowed_tiers[2] = false;
    let interval = NonZeroUsize::new(16).unwrap();
    let binding = worker
        .entry_recipe(current.clone(), interval)
        .await
        .unwrap();
    assert_eq!(binding.first_turn, initial_turn);
    assert_eq!(binding.first_run, initial_run);
    assert!(!binding.authority.shell);
    assert!(!binding.authority.allowed_tiers[1]);
    assert!(!binding.authority.allowed_tiers[2]);
    assert!(!binding.authority.permission_management);
    let bind = || {
        session.bind_entry(
            address.instance_id.clone(),
            executor.clone(),
            current.clone(),
            Default::default(),
            interval,
        )
    };
    let (first, competing) = tokio::join!(bind(), bind());
    assert_ne!(first.is_ok(), competing.is_ok());
    assert!(
        session
            .bind_entry(
                address.instance_id.clone(),
                executor.clone(),
                current.clone(),
                atman_runtime::trust::TrustConfig {
                    mode: atman_runtime::trust::TrustMode::Reckless,
                    ..Default::default()
                },
                interval
            )
            .await
            .is_err()
    );
    assert_eq!(
        worker.context().trust_config().mode,
        atman_runtime::trust::TrustConfig::default().mode
    );
    assert_eq!(worker.state().await.unwrap().pauses(), &[key]);
    assert_eq!(worker.snapshot().await.unwrap().pending.len(), 2);
    assert!(worker.completion(None).await.unwrap().is_none());
    change(&worker, FlowTransition::ExecutionResumed { key }).await;
    wait_answers(&worker, 2).await;
    let context = worker.context();
    let answers: Vec<_> = context
        .messages()
        .iter()
        .filter(|message| message.origin == MessageOrigin::FinalAnswer)
        .cloned()
        .collect();
    assert_eq!(answers[0].turn_id, initial_turn);
    assert_ne!(answers[1].turn_id, initial_turn);
    let first = worker
        .completion(Some(initial_turn))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.completion.lease.run, initial_run);
    for (message, expected_input, expected_effort) in [
        (&answers[0], "first", "high"),
        (&answers[1], "second", "low"),
    ] {
        let result: serde_json::Value = serde_json::from_str(&message.text_concat()).unwrap();
        assert_eq!(result["input"], expected_input);
        assert_eq!(result["effort"], expected_effort);
        assert_eq!(result["cwd"], workspace.to_string_lossy().as_ref());
        assert_eq!(result["fs_root"], workspace.to_string_lossy().as_ref());
        assert!(result["workspace"].is_null());
        assert_eq!(result["fs_mode"], "read-only");
        let actual: EffectiveAuthority =
            serde_json::from_value(result["authority"].clone()).unwrap();
        assert_eq!(actual, binding.authority);
    }
    session
        .stop_scheduling(address.instance_id.clone())
        .await
        .unwrap();
    let _lost = start(&worker).await;
    let mut pending = peer_request(&address, &address);
    pending.sender = SenderIdentity::User(Uuid::new_v4());
    pending.payload = InboxPayload::Interjection {
        text: "after explicit recovery".into(),
    };
    worker.accept_input(pending, |_| Ok(())).await.unwrap();
    drop(worker.shutdown().await.unwrap());
    let worker = host.flow(&address).await.unwrap();
    session
        .bind_entry(
            address.instance_id.clone(),
            executor.clone(),
            current,
            Default::default(),
            interval,
        )
        .await
        .unwrap();
    assert!(matches!(
        worker.state().await.unwrap().state(),
        InstanceState::Open(InstanceActivity::Interrupted { .. })
    ));
    assert_eq!(worker.snapshot().await.unwrap().pending.len(), 1);
    change(&worker, FlowTransition::InterruptionRecovered).await;
    wait_answers(&worker, 3).await;
    session
        .stop_scheduling(address.instance_id.clone())
        .await
        .unwrap();
    let outside =
        EffectiveAuthority::root(&Default::default(), true, Some(workspace.join("other")));
    assert!(
        session
            .bind_entry(
                address.instance_id.clone(),
                executor,
                outside,
                Default::default(),
                interval
            )
            .await
            .is_err()
    );
    assert!(worker.state().await.unwrap().can_start_turn());
    assert!(worker.snapshot().await.unwrap().pending.is_empty());
}

#[tokio::test]
async fn entry_authored_continuation_uses_the_same_scheduler_for_direct_and_spawned_flows() {
    use atman_runtime::{ValueJson, flow_runtime::entry::EntryRecipe, memory::goal::GoalStatus};
    let source = r#"
        flow current_identity() -> any { return flow.current() }
        flow main(input: string, label: string) -> string {
            contract { invocation { user_message: input, goal: true } }
            caller = flow.current()
            inline = current_identity().await
            when is_empty(caller.instance.instance_id) { return "missing instance" }
            when caller.instance.instance_id != inline.instance.instance_id { return "lost inline owner" }
            when caller.instance.session_id != inline.instance.session_id { return "lost inline session" }
            when caller.turn_id != inline.turn_id { return "lost inline turn" }
            when caller.run_id == inline.run_id { return "missing inline run" }
            when input == "ready" { return final.answer(message: "ready") }
            goal = memory.goal.get()
            when goal.goal {
                paused = memory.goal.pause(goal_id: goal.goal.id, expected_revision: goal.goal.revision, reason: "completed", summary: "two turns delivered")
                declined = memory.goal.schedule(goal_id: paused.goal.id, expected_revision: paused.goal.revision, message: "must not run")
                when declined.accepted { return "unexpected proposal" }
                when env("effort") != "high" { return "lost invocation" }
                return final.answer(message: label + ": " + input)
            }
            goal = memory.goal.set(text: "finish in two turns")
            answer = final.answer(message: "first answer")
            first = memory.goal.schedule(goal_id: goal.goal.id, expected_revision: goal.goal.revision, message: "continue objective")
            again = memory.goal.schedule(goal_id: goal.goal.id, expected_revision: goal.goal.revision, message: "continue objective")
            when first.acceptance.message_id != again.acceptance.message_id { return "duplicate proposal" }
            return answer
        }
    "#;
    for (spawned, mapped, managed_policy) in [
        (false, true, false),
        (true, true, false),
        (true, false, false),
        (false, true, true),
        (true, true, true),
        (true, false, true),
    ] {
        let mut source = source.to_string();
        let continuation_text = if managed_policy {
            source = source.replace(
                "return final.answer(message: label + \": \" + input)",
                "return deliver(label + \": \" + input).await",
            );
            let start = source
                .find("            answer = final.answer(message: \"first answer\")")
                .unwrap();
            let end = start
                + source[start..].find("            return answer").unwrap()
                + "            return answer".len();
            source.replace_range(
                start..end,
                "            return deliver(\"first answer\").await",
            );
            source.push_str(atman_runtime::templates::AGENT_DELIVERY_AT);
            "Goal continuation control, not a new user request. Continue the existing objective from the recorded context. Do not repeat completed effects. Pause the goal with a concrete result or blocker when no autonomous work remains, then deliver the final answer."
        } else {
            "continue objective"
        };
        let source = if mapped {
            source
        } else {
            source.replace("user_message: input, ", "")
        };
        let dir = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(dir.path()).unwrap();
        let host = RuntimeHost::default();
        let info = session_descriptor();
        let session = host
            .attach_session(root.create_session(info.clone()).unwrap())
            .unwrap();
        let executor = goal_executor(&dir.path().join("legacy"));
        let (worker, scheduler) = if spawned {
            let path = dir.path().join("worker.at");
            std::fs::write(&path, &source).unwrap();
            let target = serde_json::to_string(&format!("{}@main", path.display())).unwrap();
            let launcher = session.create_flow(program_descriptor(info.id, format!(r#"
                flow main() -> any {{
                    inventory = flow.instances()
                    return flow.spawn(flow: {target}, spawn_token: inventory.spawn_token, arguments: {{input: "ready", label: "initial"}})
                }}
            "#))).await.unwrap();
            change(&launcher, FlowTransition::Activated).await;
            let result = invoke_with_env(
                &launcher,
                executor,
                [("effort".into(), "low".into())].into(),
            )
            .await
            .result
            .unwrap()
            .to_json()
            .unwrap();
            let address = serde_json::from_value(result["address"].clone()).unwrap();
            (host.flow(&address).await.unwrap(), None)
        } else {
            let descriptor = program_descriptor(info.id, source);
            let worker = session.create_flow(descriptor.clone()).await.unwrap();
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
            change(&worker, FlowTransition::Activated).await;
            let recipe = EntryRecipe {
                first_turn: TurnId::now(),
                first_run: RunId::now(),
                arguments: [
                    ("input".into(), "ready".into()),
                    ("label".into(), "initial".into()),
                ]
                .into(),
                invocation: [("effort".into(), "low".into())].into(),
                parameter: mapped.then(|| "input".into()),
                authority: EffectiveAuthority::root(&Default::default(), false, None),
                yield_interval: NonZeroUsize::new(16).unwrap(),
            };
            worker
                .accept_input(
                    DeliveryRequest {
                        request_id: descriptor.creation_request,
                        sender: SenderIdentity::User(Uuid::new_v4()),
                        recipient: worker.address().clone(),
                        delivery: DeliveryMode::NextTurn,
                        cause: None,
                        correlation: None,
                        scope: None,
                        payload: InboxPayload::Invocation {
                            user: None,
                            arguments: recipe.arguments.clone(),
                            invocation: recipe.invocation.clone(),
                        },
                    },
                    |_| Ok(()),
                )
                .await
                .unwrap();
            let scheduler = worker
                .schedule(executor, move |snapshot| recipe.prepare(snapshot))
                .unwrap();
            (worker, Some(scheduler))
        };
        wait_answers(&worker, 1).await;
        let mut submission = peer_request(worker.address(), worker.address());
        submission.sender = SenderIdentity::User(Uuid::new_v4());
        submission.delivery = DeliveryMode::NextTurn;
        submission.payload = InboxPayload::Invocation {
            user: None,
            arguments: [
                ("input".into(), "start".into()),
                ("label".into(), "carried".into()),
            ]
            .into(),
            invocation: [("effort".into(), "high".into())].into(),
        };
        worker.accept_input(submission, |_| Ok(())).await.unwrap();
        wait_answers(&worker, 3).await;
        // Publication precedes entry completion and observer delivery. Settle both
        // before testing a clean storage handoff rather than host-loss recovery.
        tokio::time::timeout(Duration::from_secs(10), host.wait_quiescent())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            worker.state().await.unwrap().state(),
            InstanceState::Open(InstanceActivity::Idle)
        ));
        assert_eq!(
            worker.goal().await.unwrap().goal.unwrap().status,
            GoalStatus::Paused
        );
        assert!(worker.snapshot().await.unwrap().pending.is_empty());
        let context = worker.context();
        let messages = context.messages().to_vec();
        let continuation: Vec<_> = messages
            .iter()
            .filter(|message| message.text_concat() == continuation_text)
            .collect();
        assert_eq!(continuation.len(), 1);
        assert_eq!(continuation[0].origin, MessageOrigin::Internal);
        let expected = if mapped {
            format!("carried: {continuation_text}")
        } else {
            "carried: start".into()
        };
        assert!(
            messages
                .iter()
                .any(|message| message.origin == MessageOrigin::FinalAnswer
                    && message.text_concat() == expected)
        );
        if let Some(scheduler) = scheduler {
            scheduler.stop().await.unwrap();
        } else {
            session
                .stop_scheduling(worker.address().instance_id.clone())
                .await
                .unwrap();
        }
        let address = worker.address().clone();
        drop(worker.shutdown().await.unwrap());
        let reopened = host.flow(&address).await.unwrap();
        let admitted = reopened.snapshot().await.unwrap().last_invocation.unwrap();
        assert_eq!(admitted.arguments["label"], "carried");
        assert_eq!(admitted.invocation["effort"], "high");
    }
}

#[tokio::test]
async fn continuation_admission_prioritizes_new_input_and_declines_changed_goals() {
    use atman_runtime::{
        Value, ValueJson,
        flow_store::goal::{GoalRequest, GoalVersion},
        memory::goal::{GoalActor, GoalEdit, GoalPauseReason},
        tool::{Tier, ToolDefinition},
    };
    for before_proposal in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("worker.at");
        let barrier = "await_release()";
        let (before, after) = if before_proposal {
            (barrier, "")
        } else {
            ("", barrier)
        };
        std::fs::write(&path, format!(r#"
            flow main(input: string) -> string {{
                contract {{ invocation {{ user_message: input, goal: true }} }}
                goal = memory.goal.get()
                when goal.goal {{
                    memory.goal.pause(goal_id: goal.goal.id, expected_revision: goal.goal.revision, reason: "completed", summary: "input handled")
                    return final.answer(message: "done: " + input)
                }}
                goal = memory.goal.set(text: "objective")
                {before}
                memory.goal.schedule(goal_id: goal.goal.id, expected_revision: goal.goal.revision, message: "old continuation")
                {after}
                return final.answer(message: "first answer")
            }}
        "#)).unwrap();
        let target = serde_json::to_string(&format!("{}@main", path.display())).unwrap();
        let root = RuntimeStore::open(dir.path()).unwrap();
        let host = RuntimeHost::default();
        let info = session_descriptor();
        let session = host
            .attach_session(root.create_session(info.clone()).unwrap())
            .unwrap();
        let launcher = session.create_flow(program_descriptor(info.id, format!(r#"
            flow main() -> any {{
                inventory = flow.instances()
                return flow.spawn(flow: {target}, spawn_token: inventory.spawn_token, arguments: {{input: "start"}})
            }}
        "#))).await.unwrap();
        change(&launcher, FlowTransition::Activated).await;
        let entered = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let executor = goal_executor(&dir.path().join("legacy"));
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
        let result = invoke(&launcher, executor)
            .await
            .result
            .unwrap()
            .to_json()
            .unwrap();
        let address = serde_json::from_value(result["address"].clone()).unwrap();
        let worker = host.flow(&address).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), entered.acquire())
            .await
            .unwrap_or_else(|error| panic!("barrier: {error}"))
            .unwrap()
            .forget();
        let key = Uuid::new_v4().into();
        if before_proposal {
            let goal = worker.goal().await.unwrap().goal.unwrap();
            worker
                .edit_goal(
                    GoalRequest {
                        start_turn: false,
                        id: Uuid::new_v4(),
                        sender: SenderIdentity::User(Uuid::new_v4()),
                        expected: Some(GoalVersion::from(&goal)),
                        edit: GoalEdit::Pause {
                            actor: GoalActor::User,
                            reason: GoalPauseReason::UserRequested,
                            summary: "hold goal".into(),
                        },
                    },
                    |_| Ok(()),
                )
                .await
                .unwrap();
        } else {
            change(&worker, FlowTransition::ExecutionPaused { key }).await;
            let mut request = peer_request(launcher.address(), worker.address());
            request.sender = SenderIdentity::User(Uuid::new_v4());
            request.delivery = DeliveryMode::NextTurn;
            request.payload = InboxPayload::Interjection {
                text: "urgent request".into(),
            };
            worker.accept_input(request, |_| Ok(())).await.unwrap();
            assert_eq!(worker.snapshot().await.unwrap().pending.len(), 2);
        }
        release.add_permits(1);
        if !before_proposal {
            change(&worker, FlowTransition::ExecutionResumed { key }).await;
        }
        wait_answers(&worker, if before_proposal { 1 } else { 2 }).await;
        assert!(worker.snapshot().await.unwrap().pending.is_empty());
        let context = worker.context();
        assert!(
            !context
                .messages()
                .iter()
                .any(|message| message.text_concat() == "old continuation")
        );
        if !before_proposal {
            assert!(
                context
                    .messages()
                    .iter()
                    .any(|message| message.text_concat() == "done: urgent request")
            );
        }
    }
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
async fn persistent_workspaces_share_across_peers_turns_and_host_restarts() {
    use atman_runtime::git_workspace::{WorkspaceManager, WorkspaceState};
    use atman_runtime::{AtmanRuntime, AtmanRuntimeOptions, ValueJson};
    let data = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.invalid"]);
    git(&["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.path().join("base.txt"), "base").unwrap();
    git(&["add", "base.txt"]);
    git(&["commit", "-q", "-m", "base"]);
    let child_path = data.path().join("child.at");
    let leaf = serde_json::to_string(&format!("{}@leaf", child_path.display())).unwrap();
    std::fs::write(
        &child_path,
        format!(
            r#"
        flow leaf(input: string = "start") -> string {{
            contract {{ invocation {{ user_message: input }} }}
            contents = fs.read(path: "shared.txt")
            return contents
        }}
        flow child() -> string {{
            fs.write(path: "shared.txt", content: "preserved")
            inventory = flow.instances()
            return flow.spawn(flow: {leaf}, spawn_token: inventory.spawn_token, async: false)
        }}
    "#
        ),
    )
    .unwrap();
    let reference = serde_json::to_string(&format!("{}@child", child_path.display())).unwrap();
    let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(data.path().join("config"));
    let mut info = session_descriptor();
    info.project = Some(
        atman_runtime::storage::ProjectContext::resolve(&hub, repo.path(), data.path()).unwrap(),
    );
    info.configuration = atman_runtime::flow_store::SessionConfiguration::Atman(hub.source());
    let runtime = AtmanRuntime::build(AtmanRuntimeOptions {
        events: Default::default(),
        mock: false,
        config: hub,
        project: info.project.clone(),
        home_dir: None,
        workspace_generation: "workspaces".into(),
    })
    .await
    .unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let parent = session.create_flow(program_descriptor(info.id, format!(r#"
        flow main() -> string {{
            inventory = flow.instances()
            return flow.spawn(flow: {reference}, spawn_token: inventory.spawn_token, async: false, workspace: "auto")
        }}
    "#))).await.unwrap();
    change(&parent, FlowTransition::Activated).await;
    let completion = invoke(&parent, runtime.executor.clone()).await;
    assert_eq!(completion.result.unwrap().to_json().unwrap(), "preserved");
    assert!(!repo.path().join("shared.txt").exists());
    let mut child = None;
    let mut leaf = None;
    for item in session.snapshot().await.unwrap().catalog.flows {
        let worker = session.open_flow(item.id).await.unwrap();
        let descriptor = worker.descriptor().await.unwrap();
        if descriptor.created_by.as_ref() == Some(parent.address()) {
            child = Some(worker);
        } else if descriptor.created_by.is_some() {
            leaf = Some(worker);
        }
    }
    let child = child.unwrap();
    let leaf = leaf.unwrap();
    let request = child
        .launch_record()
        .await
        .unwrap()
        .unwrap()
        .input
        .workspace
        .unwrap();
    let binding = request.binding;
    assert_eq!(
        leaf.launch_record()
            .await
            .unwrap()
            .unwrap()
            .input
            .workspace
            .unwrap()
            .binding,
        binding
    );
    let manager = WorkspaceManager::open_existing(repo.path(), None)
        .unwrap()
        .unwrap();
    assert_eq!(
        manager.get(&binding.workspace_id).unwrap().instances.len(),
        2
    );
    assert_eq!(
        manager
            .get(&binding.workspace_id)
            .unwrap()
            .lifecycle_state(),
        WorkspaceState::Bound
    );
    child
        .control(
            control_request(
                &child,
                child.state().await.unwrap().revision(),
                Uuid::from_bytes(*b"atman-user-pause").into(),
                PeerControl::Close,
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    wait_closed(&child).await;
    assert!(binding.path.join("shared.txt").exists());
    manager.validate_instance(&binding, leaf.address()).unwrap();
    let leaf_address = leaf.address().clone();
    let child_address = child.address().clone();
    host.shutdown().await.unwrap();
    std::fs::remove_file(child_path).unwrap();
    manager.validate_instance(&binding, &leaf_address).unwrap();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.open_session(info.id).unwrap())
        .unwrap();
    let leaf = session.open_flow(leaf_address.instance_id).await.unwrap();
    let previous = leaf.completion(None).await.unwrap().unwrap();
    let trust = atman_runtime::trust::TrustConfig {
        mode: atman_runtime::trust::TrustMode::Reckless,
        ..Default::default()
    };
    let authority = EffectiveAuthority::root(
        &trust,
        true,
        info.project.as_ref().map(|project| project.root().into()),
    );
    session
        .bind_entry(
            leaf.address().instance_id.clone(),
            runtime.executor.clone(),
            authority,
            trust,
            NonZeroUsize::new(64).unwrap(),
        )
        .await
        .unwrap();
    let mut input = peer_request(&child_address, leaf.address());
    input.sender = SenderIdentity::User(Uuid::new_v4());
    let mut commits = leaf.subscribe_commits();
    leaf.accept_input(input, |_| Ok(())).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if leaf
                .completion(None)
                .await
                .unwrap()
                .is_some_and(|current| current.receipt != previous.receipt)
            {
                break;
            }
            commits.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(binding.path.join("shared.txt")).unwrap(),
        "preserved"
    );
    assert_eq!(
        leaf.completion(None)
            .await
            .unwrap()
            .unwrap()
            .completion
            .result,
        atman_runtime::flow_store::completion::InvocationResult::Returned(
            atman_runtime::flow_store::completion::StoredValue::String("preserved".into())
        )
    );
    leaf.control(
        control_request(
            &leaf,
            leaf.state().await.unwrap().revision(),
            Uuid::from_bytes(*b"atman-user-pause").into(),
            PeerControl::Close,
        ),
        |_| Ok(()),
    )
    .await
    .unwrap();
    wait_closed(&leaf).await;
    let record = manager.get(&binding.workspace_id).unwrap();
    assert!(!record.is_instance_bound());
    assert_eq!(record.lifecycle_state(), WorkspaceState::Dirty);
    assert!(binding.path.join("shared.txt").exists());
    host.shutdown().await.unwrap();
    let session = root.open_session(info.id).unwrap();
    let restored = session.open_flow(&leaf.address().instance_id).unwrap();
    assert_eq!(restored.state().state(), &InstanceState::Closed);
    assert_eq!(restored.workspace_request().unwrap().binding, binding);
}

#[tokio::test]
async fn persistent_workspace_failed_launch_keeps_intent_and_close_retries_do_not_reallocate() {
    use atman_runtime::flow_store::launch::LaunchInput;
    use atman_runtime::flow_workspace::FlowWorkspaceService;
    use atman_runtime::git_workspace::WorkspacePolicy;
    let data = tempfile::tempdir().unwrap();
    let repo = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "-q"]);
    git(&["config", "user.name", "Test"]);
    git(&["config", "user.email", "test@example.invalid"]);
    git(&["config", "commit.gpgsign", "false"]);
    git(&["commit", "--allow-empty", "-q", "-m", "base"]);
    let info = session_descriptor();
    let descriptor = flow_descriptor(info.id);
    let service = FlowWorkspaceService::new(repo.path(), None, "intent-test").unwrap();
    let workspace = service
        .prepare_instance(WorkspacePolicy::Auto, &descriptor.address, None)
        .unwrap();
    assert!(
        !repo.path().join(".atman").exists(),
        "planning must not allocate storage"
    );
    let path = workspace.binding.path.clone();
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(path.join("sentinel"), "preserve").unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let mut request = peer_request(&descriptor.address, &descriptor.address);
    request.request_id = descriptor.creation_request;
    request.delivery = DeliveryMode::NextTurn;
    request.sender = SenderIdentity::User(Uuid::new_v4());
    request.payload = InboxPayload::Invocation {
        user: None,
        arguments: Default::default(),
        invocation: Default::default(),
    };
    let input = LaunchInput {
        workspace: Some(workspace.clone()),
        request: atman_runtime::flow_store::launch::EntryRequest {
            request_id: request.request_id,
            sender: request.sender,
            recipient: request.recipient,
            arguments: Default::default(),
            invocation: Default::default(),
            start: atman_runtime::flow_store::launch::EntryStart::Invoke { user: None },
        },
        initial_goal: None,
        inherited_context: vec![],
        result_route: None,
        execution: None,
    };
    assert!(
        host.launch_flow(descriptor.clone(), input.clone(), |_, _| Ok(()))
            .await
            .is_err()
    );
    let worker = session
        .open_flow(descriptor.address.instance_id.clone())
        .await
        .unwrap();
    assert_eq!(
        worker.state().await.unwrap().state(),
        &InstanceState::Prepared
    );
    assert!(worker.launch_record().await.unwrap().is_none());
    assert_eq!(
        std::fs::read_to_string(path.join("sentinel")).unwrap(),
        "preserve"
    );
    let updates = worker
        .read_updates(None, NonZeroUsize::new(64).unwrap())
        .await
        .unwrap();
    assert!(
        updates
            .batches
            .iter()
            .flat_map(|batch| &batch.facts)
            .any(|update| matches!(
                update,
                atman_runtime::flow_store::view::FlowUpdate::WorkspaceRequested(_)
            ))
    );
    change(&worker, FlowTransition::CloseRequested).await;
    let revision = worker.state().await.unwrap().revision();
    assert!(
        worker
            .transition(revision, FlowTransition::Closed)
            .await
            .is_err()
    );
    worker
        .control(
            control_request(
                &worker,
                revision,
                Uuid::from_bytes(*b"atman-user-pause").into(),
                PeerControl::Close,
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let error = tokio::time::timeout(Duration::from_secs(10), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("workspace Git registration mismatch"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(path.join("sentinel")).unwrap(),
        "preserve"
    );
    let error = host.shutdown().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("workspace Git registration mismatch"),
        "{error}"
    );
    std::fs::remove_file(path.join("sentinel")).unwrap();
    std::fs::remove_dir(&path).unwrap();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.open_session(info.id).unwrap())
        .unwrap();
    let worker = session
        .open_flow(descriptor.address.instance_id.clone())
        .await
        .unwrap();
    assert!(!path.exists());
    wait_closed(&worker).await;
    assert!(
        host.launch_flow(descriptor.clone(), input.clone(), |_, _| Ok(()))
            .await
            .is_err()
    );
    assert!(
        !path.exists(),
        "retry of a closed prepared instance must not acquire resources"
    );
    let clean = flow_descriptor(info.id);
    let clean_workspace = service
        .prepare_instance(WorkspacePolicy::Auto, &clean.address, None)
        .unwrap();
    let clean_path = clean_workspace.binding.path.clone();
    let mut clean_input = input;
    clean_input.workspace = Some(clean_workspace);
    clean_input.request.request_id = clean.creation_request;
    clean_input.request.recipient = clean.address.clone();
    let clean_worker = host
        .launch_flow(clean, clean_input, |_, _| Ok(()))
        .await
        .unwrap()
        .worker;
    let revision = clean_worker.state().await.unwrap().revision();
    assert!(
        clean_worker
            .transition(revision, FlowTransition::Closed)
            .await
            .is_err()
    );
    assert!(
        clean_path.exists(),
        "invalid close must not release the resource"
    );
    change(&clean_worker, FlowTransition::CloseRequested).await;
    assert!(
        clean_worker
            .transition(revision, FlowTransition::Closed)
            .await
            .is_err()
    );
    assert!(
        clean_path.exists(),
        "stale close must not release the resource"
    );
    change(&clean_worker, FlowTransition::Closed).await;
    assert!(!clean_path.exists());
    host.shutdown().await.unwrap();
    let session = root.open_session(info.id).unwrap();
    let recorded = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(recorded.workspace_request(), Some(&workspace));
    assert_eq!(recorded.state().state(), &InstanceState::Closed);
    let before = std::fs::read(recorded.dir().join("events.jsonl")).unwrap();
    let inspected = RuntimeStore::inspect_session(data.path(), info.id)
        .unwrap()
        .inspect_flow(&descriptor.address.instance_id)
        .unwrap();
    assert_eq!(inspected.workspace.as_ref(), Some(&workspace));
    assert_eq!(
        std::fs::read(recorded.dir().join("events.jsonl")).unwrap(),
        before
    );
    assert!(
        !path.exists(),
        "read-only workspace intent must not recreate a checkout"
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
    use atman_runtime::{flow_runtime::host::HostServices, permission::PermissionClients};
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
    let clients = Arc::new(PermissionClients::default());
    let host = RuntimeHost::with_services(HostServices {
        permission_clients: Arc::downgrade(&clients),
        ..Default::default()
    });
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
    let mut brokers = Vec::new();
    for instance in session.snapshot().await.unwrap().catalog.flows {
        let broker = session
            .open_flow(instance.id)
            .await
            .unwrap()
            .context()
            .permission_broker();
        assert!(
            broker.has_clients(),
            "spawned peers must share endpoint registration"
        );
        brokers.push(broker);
    }
    drop(clients);
    assert!(brokers.iter().all(|broker| !broker.has_clients()));
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
    assert_eq!(page.instances[1].state.as_ref(), Some(store.state()));
    assert!(page.instances[1].unavailable.is_none());
    assert!(!page.instances[1].loaded);
    assert!(page.instances[0].loaded);
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
        InstanceState::Open(InstanceActivity::Active(_))
    ));
    assert!(!row.loaded);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&journal_path)
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&file)
        .expect("discovery must release an inactive journal, not retain a context worker");
    fs2::FileExt::unlock(&file).unwrap();
    std::fs::write(&journal_path, b"corrupt\n").unwrap();
    let page = client.instances(None, 4).await.unwrap();
    let damaged = page
        .instances
        .iter()
        .find(|item| item.address == *unavailable.address())
        .unwrap();
    assert!(damaged.state.is_none());
    assert!(damaged.unavailable.is_some());
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
            format!(
                "flow main() -> any {{ return flow.close(handle: \"{}\", expected_revision: {revision}) }}",
                b.address().instance_id
            ),
            false,
            false,
        ),
        (
            format!(
                "flow main() -> any {{ return flow.close(handle: \"{}\") }}",
                b.address().instance_id
            ),
            true,
            false,
        ),
        (
            format!(
                "flow main() -> any {{ return flow.close(handle: \"{}\", expected_revision: 0) }}",
                b.address().instance_id
            ),
            true,
            false,
        ),
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
    conflict.entry_mut().unwrap().name = "other".into();
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
    let journal = store.dir().join("events.jsonl");
    let before = std::fs::read(&journal).unwrap();
    let recorded = store.state().clone();
    assert_eq!(
        session
            .describe_flow(description.address.instance_id.clone())
            .await
            .unwrap()
            .state,
        recorded
    );
    assert!(
        session
            .open_flow(description.address.instance_id.clone())
            .await
            .is_err()
    );
    drop(store);
    assert_eq!(
        session
            .describe_flow(description.address.instance_id.clone())
            .await
            .unwrap()
            .state,
        recorded
    );
    assert_eq!(std::fs::read(&journal).unwrap(), before);
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
                Uuid::new_v4(),
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
        .control(
            Uuid::new_v4(),
            b_info.address.clone(),
            stale,
            PeerControl::Pause,
        )
        .await
        .unwrap();
    assert!(
        client
            .control(
                Uuid::new_v4(),
                b_info.address.clone(),
                stale,
                PeerControl::Resume
            )
            .await
            .is_err()
    );
    controller
        .control(
            Uuid::new_v4(),
            b_info.address.clone(),
            b.state().await.unwrap().revision(),
            PeerControl::Pause,
        )
        .await
        .unwrap();
    assert_eq!(b.state().await.unwrap().pauses().len(), 3);
    client
        .control(
            Uuid::new_v4(),
            b_info.address.clone(),
            b.state().await.unwrap().revision(),
            PeerControl::Resume,
        )
        .await
        .unwrap();
    assert_eq!(b.state().await.unwrap().pauses().len(), 2);
    controller
        .control(
            Uuid::new_v4(),
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
        FlowTransition::PhaseChanged {
            lease: lease.clone(),
            phase: atman_rt::TurnPhase::Finishing,
        },
    )
    .await;
    let cleanup_pause = Uuid::new_v4().into();
    change(&a, FlowTransition::ExecutionPaused { key: cleanup_pause }).await;
    client
        .edit_planning(
            Uuid::new_v4(),
            None,
            atman_runtime::flow_store::planning::PlanningEdit::ClearTodos,
        )
        .await
        .unwrap();
    client
        .edit_goal(
            a_info.address.clone(),
            Uuid::new_v4(),
            None,
            atman_runtime::memory::goal::GoalEdit::Create {
                text: "saved during cleanup".into(),
            },
        )
        .await
        .unwrap();
    assert!(
        client
            .read_goal(a_info.address.clone())
            .await
            .unwrap()
            .goal
            .is_some()
    );
    assert!(client.read_context(a_info.address.clone()).await.is_ok());
    assert!(client.read_output(b_info.address.clone()).await.is_err());
    assert!(
        client
            .send(
                b_info.address.clone(),
                Uuid::new_v4(),
                "late work".into(),
                DeliveryMode::AtBoundary,
            )
            .await
            .is_err()
    );
    assert!(
        client
            .control(
                Uuid::new_v4(),
                a_info.address.clone(),
                a.state().await.unwrap().revision(),
                PeerControl::CancelTurn
            )
            .await
            .is_err()
    );
    change(
        &a,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    )
    .await;
    assert!(client.read_context(a_info.address.clone()).await.is_err());
    assert!(
        client
            .edit_planning(
                Uuid::new_v4(),
                None,
                atman_runtime::flow_store::planning::PlanningEdit::ClearTodos
            )
            .await
            .is_err()
    );
    change(&a, FlowTransition::ExecutionResumed { key: cleanup_pause }).await;
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
        expires_at: None,
        reply: None,
    }
}

#[tokio::test]
async fn host_watcher_replacement_rechecks_authority_and_keeps_pending_across_retries() {
    use atman_runtime::flow_store::{
        subscription::SubscriptionStatus, watch_edit::WatchDefinition,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let recipient = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    let source = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    for worker in [&recipient, &source] {
        change(worker, FlowTransition::Activated).await;
    }
    let owner = recipient.address().clone();
    let original = answer_watch(&source);
    host.watch_events(owner.clone(), original.clone(), |_| Ok(()))
        .await
        .unwrap();
    invoke(&source, Executor::new()).await.result.unwrap();
    wait_inputs(&recipient, 1).await;
    change(
        &recipient,
        FlowTransition::ExecutionPaused {
            key: Uuid::now_v7().into(),
        },
    )
    .await;
    let initial = recipient.snapshot().await.unwrap();
    let expected = WatchDefinition::Flow(original.clone());
    let replacement = WatchDefinition::Flow(answer_watch(&source));
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    assert_eq!(
        host.replace_watcher(
            owner.clone(),
            expected.clone(),
            replacement.clone(),
            move |_| {
                if counter.fetch_add(1, Ordering::SeqCst) == 3 {
                    Err(io::ErrorKind::PermissionDenied.into())
                } else {
                    Ok(())
                }
            }
        )
        .await
        .unwrap_err()
        .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(recipient.snapshot().await.unwrap().cursor, initial.cursor);
    let (a, b) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            host.replace_watcher(
                owner.clone(),
                expected.clone(),
                replacement.clone(),
                |_| Ok(())
            ),
            host.replace_watcher(
                owner.clone(),
                expected.clone(),
                replacement.clone(),
                |_| Ok(())
            )
        )
    })
    .await
    .unwrap();
    let receipt = a.unwrap();
    assert_eq!(receipt, b.unwrap());
    assert_eq!(recipient.state().await.unwrap(), initial.state);
    assert_eq!(recipient.snapshot().await.unwrap().pending, initial.pending);
    let records = recipient.flow_subscriptions().await.unwrap();
    assert_eq!(records.len(), 2);
    assert_eq!(
        records
            .iter()
            .find(|r| r.subscription.id == original.id)
            .unwrap()
            .status,
        SubscriptionStatus::Cancelled
    );
    assert!(
        host.replace_watcher(
            owner.clone(),
            expected.clone(),
            WatchDefinition::Flow(answer_watch(&source)),
            |_| Ok(())
        )
        .await
        .is_err()
    );
    invoke(&source, Executor::new()).await.result.unwrap();
    wait_inputs(&recipient, 2).await;
    recipient
        .unwatch(replacement.id(), |_| Ok(()))
        .await
        .unwrap();
    change(&source, FlowTransition::CloseRequested).await;
    change(&source, FlowTransition::Closed).await;
    assert_eq!(
        host.replace_watcher(
            owner.clone(),
            expected.clone(),
            replacement.clone(),
            |_| Ok(())
        )
        .await
        .unwrap(),
        receipt
    );
    assert!(
        host.replace_watcher(owner.clone(), expected.clone(), replacement.clone(), |_| {
            Err(io::ErrorKind::PermissionDenied.into())
        })
        .await
        .is_err()
    );
    host.shutdown().await.unwrap();
    let reopened = RuntimeHost::default();
    reopened
        .attach_session(root.open_session(info.id).unwrap())
        .unwrap();
    assert_eq!(
        reopened
            .replace_watcher(owner, expected, replacement, |_| Ok(()))
            .await
            .unwrap(),
        receipt
    );
    reopened.shutdown().await.unwrap();
}

#[tokio::test]
async fn host_watch_registration_preserves_authority_cursor_and_idle_execution() {
    use atman_runtime::flow_store::{
        access::{AccessTarget, FlowAction, FlowGrant},
        subscription::SubscriptionStatus,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let recipient = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    let source = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    for worker in [&recipient, &source] {
        change(worker, FlowTransition::Activated).await;
    }
    change(
        &recipient,
        FlowTransition::ExecutionPaused {
            key: Uuid::now_v7().into(),
        },
    )
    .await;
    let initial = recipient.snapshot().await.unwrap();
    let watch = answer_watch(&source);
    let owner = recipient.address().clone();
    // Owner authority is checked again after capturing the source's anchor.
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let expected = owner.clone();
    let denied = host
        .watch_events(owner.clone(), watch.clone(), move |actual| {
            assert_eq!(actual, &expected);
            if counter.fetch_add(1, Ordering::SeqCst) == 3 {
                Err(io::ErrorKind::PermissionDenied.into())
            } else {
                Ok(())
            }
        })
        .await
        .unwrap_err();
    assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    assert_eq!(recipient.snapshot().await.unwrap().cursor, initial.cursor);
    invoke(&source, Executor::new()).await.result.unwrap();
    let (first, retry) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            host.watch_events(owner.clone(), watch.clone(), |_| Ok(())),
            host.watch_events(owner.clone(), watch.clone(), |_| Ok(()))
        )
    })
    .await
    .expect("registration must not hold source and recipient locks together");
    let first = first.unwrap();
    assert_eq!(first, retry.unwrap());
    assert_eq!(recipient.flow_subscriptions().await.unwrap().len(), 1);
    assert_eq!(recipient.state().await.unwrap(), initial.state);
    assert!(
        recipient.snapshot().await.unwrap().pending.is_empty(),
        "registration must not replay earlier answers"
    );
    let mut conflict = watch.clone();
    conflict.once = true;
    assert!(
        host.watch_events(owner.clone(), conflict, |_| Ok(()))
            .await
            .is_err()
    );
    invoke(&source, Executor::new()).await.result.unwrap();
    wait_inputs(&recipient, 1).await;
    assert_eq!(
        host.watch_events(owner.clone(), watch.clone(), |_| Ok(()))
            .await
            .unwrap()
            .started_after,
        first.started_after
    );
    let (self_watch, reverse) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(
            host.watch_events(owner.clone(), answer_watch(&recipient), |_| Ok(())),
            host.watch_events(source.address().clone(), answer_watch(&recipient), |_| Ok(
                ()
            ))
        )
    })
    .await
    .unwrap();
    self_watch.unwrap();
    reverse.unwrap();
    tokio::time::timeout(Duration::from_secs(5), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    let quiet = recipient.snapshot().await.unwrap().cursor;
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        recipient.snapshot().await.unwrap().cursor,
        quiet,
        "self-observation must not spin on private registration facts"
    );
    recipient.unwatch(watch.id, |_| Ok(())).await.unwrap();
    assert_eq!(
        host.watch_events(owner.clone(), watch, |_| Ok(()))
            .await
            .unwrap()
            .status,
        SubscriptionStatus::Cancelled
    );

    let other_info = session_descriptor();
    let other = host
        .attach_session(root.create_session(other_info.clone()).unwrap())
        .unwrap();
    let remote = other
        .create_flow(flow_descriptor(other_info.id))
        .await
        .unwrap();
    let remote_watch = answer_watch(&remote);
    assert_eq!(
        host.watch_events(owner.clone(), remote_watch.clone(), |_| Ok(()))
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    let grant = FlowGrant {
        id: Uuid::now_v7(),
        grantee: owner.clone(),
        target: AccessTarget::Flow(remote.address().clone()),
        actions: [FlowAction::Watch].into(),
    };
    session
        .grant_access(session.snapshot().await.unwrap().cursor, grant.clone())
        .await
        .unwrap();
    let registered = host
        .watch_events(owner.clone(), remote_watch.clone(), |_| Ok(()))
        .await
        .unwrap();
    session
        .revoke_access(session.snapshot().await.unwrap().cursor, grant.id)
        .await
        .unwrap();
    assert_eq!(
        host.watch_events(owner.clone(), remote_watch, |_| Ok(()))
            .await
            .unwrap(),
        registered
    );
    assert!(
        host.watch_events(owner, answer_watch(&remote), |_| Ok(()))
            .await
            .is_err()
    );
    assert_eq!(recipient.state().await.unwrap(), initial.state);
    assert_eq!(recipient.snapshot().await.unwrap().pending.len(), 1);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn flow_watch_expiry_survives_revocation_pause_and_reopen_without_source() {
    use atman_runtime::flow_store::{
        access::{AccessTarget, FlowAction, FlowGrant},
        subscription::SubscriptionStatus,
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let remote_info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let remote = host
        .attach_session(root.create_session(remote_info.clone()).unwrap())
        .unwrap();
    let recipient = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    let source = remote
        .create_flow(flow_descriptor(remote_info.id))
        .await
        .unwrap();
    change(&recipient, FlowTransition::Activated).await;
    let lease = start(&recipient).await;
    let client = host.bind_invocation(lease.clone()).await.unwrap();
    let grant = FlowGrant {
        id: Uuid::new_v4(),
        grantee: recipient.address().clone(),
        target: AccessTarget::Flow(source.address().clone()),
        actions: [FlowAction::Watch].into(),
    };
    session
        .grant_access(session.snapshot().await.unwrap().cursor, grant.clone())
        .await
        .unwrap();
    let watch = atman_runtime::flow_store::subscription::FlowSubscription {
        expires_at: Some(chrono::Utc::now() + chrono::Duration::seconds(1)),
        ..answer_watch(&source)
    };
    client.watch_events(watch.clone()).await.unwrap();
    change(
        &recipient,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    )
    .await;
    change(
        &recipient,
        FlowTransition::ExecutionPaused {
            key: Uuid::new_v4().into(),
        },
    )
    .await;
    session
        .revoke_access(session.snapshot().await.unwrap().cursor, grant.id)
        .await
        .unwrap();
    // The deadline belongs to the receiver; neither source access nor execution is required.
    let pending = wait_inputs(&recipient, 1).await;
    assert!(matches!(
        &pending.pending[0].request.payload,
        InboxPayload::Timer { timer, content, .. }
            if *timer == watch.id && content["kind"] == "flow_watch_expired"
    ));
    assert_eq!(
        recipient.flow_subscriptions().await.unwrap()[0].status,
        SubscriptionStatus::Expired
    );
    let address = recipient.address().clone();
    host.shutdown().await.unwrap();
    let host = RuntimeHost::default();
    let session = host
        .attach_session(root.open_session(info.id).unwrap())
        .unwrap();
    let restored = session.open_flow(address.instance_id).await.unwrap();
    let snapshot = restored.snapshot().await.unwrap();
    assert_eq!(snapshot.pending, pending.pending);
    assert_eq!(
        restored.flow_subscriptions().await.unwrap()[0].status,
        SubscriptionStatus::Expired
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(restored.snapshot().await.unwrap().cursor, snapshot.cursor);
    assert!(restored.subscribe_observation_issues().borrow().is_empty());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn cyclic_peer_waits_park_and_unwind_through_pause_cancel_and_close() {
    use atman_runtime::{
        RuntimeError,
        flow_runtime::access::PeerControl,
        flow_store::subscription::{FlowEventFilter, FlowSubscription, SubscriptionPurpose},
        tool::{Tier, ToolDefinition},
    };
    for close in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(dir.path()).unwrap();
        let host = RuntimeHost::default();
        let info = session_descriptor();
        let session = host
            .attach_session(root.create_session(info.clone()).unwrap())
            .unwrap();
        let a = session
            .create_flow(program_descriptor(
                info.id,
                "flow main() -> any { return peer_wait() }".into(),
            ))
            .await
            .unwrap();
        let b = session
            .create_flow(program_descriptor(
                info.id,
                "flow main() -> any { return peer_wait() }".into(),
            ))
            .await
            .unwrap();
        for worker in [&a, &b] {
            change(worker, FlowTransition::Activated).await;
            worker
                .accept_input(peer_request(worker.address(), worker.address()), |_| Ok(()))
                .await
                .unwrap();
        }
        let a_request = prepare(&a.snapshot().await.unwrap()).unwrap().unwrap();
        let b_request = prepare(&b.snapshot().await.unwrap()).unwrap().unwrap();
        let (entered, mut ready) = tokio::sync::mpsc::unbounded_channel();
        let executor = |peer: FlowRef, turn: TurnId<_>| {
            let executor = Executor::new();
            let entered = entered.clone();
            executor
                .tools
                .register_fn(
                    ToolDefinition::new("peer_wait", Tier::Zero),
                    move |_, ctx| {
                        let peer = peer.clone();
                        let turn = turn.clone();
                        let entered = entered.clone();
                        async move {
                            let client = ctx.flow_client().unwrap();
                            let failed =
                                |error: io::Error| RuntimeError::ToolFailed(error.to_string());
                            let sent = client
                                .send(
                                    peer.clone(),
                                    Uuid::new_v4(),
                                    "peer input".into(),
                                    DeliveryMode::AtBoundary,
                                )
                                .await
                                .map_err(failed)?;
                            assert!(sent.pending_reason.is_none());
                            let wait = client
                                .prepare_reply(peer.clone(), Uuid::new_v4())
                                .await
                                .map_err(failed)?;
                            client
                                .watch_events(FlowSubscription {
                                    id: Uuid::new_v4(),
                                    source: peer,
                                    filter: FlowEventFilter::TurnCompleted {
                                        turn: Some(turn.clone()),
                                    },
                                    once: true,
                                    purpose: SubscriptionPurpose::Watcher,
                                    expires_at: None,
                                    reply: Some(wait.waiter().clone()),
                                })
                                .await
                                .map_err(failed)?;
                            entered.send(()).unwrap();
                            wait.completion(turn).await
                        }
                    },
                )
                .unwrap();
            executor
        };
        let a_executor = executor(b.address().clone(), b_request.turn.clone());
        let b_executor = executor(a.address().clone(), a_request.turn.clone());
        let a_ticket = a.start_turn(a_executor, a_request).await.unwrap();
        let b_ticket = b.start_turn(b_executor, b_request).await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), async {
            ready.recv().await.unwrap();
            ready.recv().await.unwrap();
        })
        .await
        .unwrap();
        for (worker, peer) in [(&a, &b), (&b, &a)] {
            let subscriptions = worker.flow_subscriptions().await.unwrap();
            assert_eq!(subscriptions.len(), 1);
            assert_eq!(&subscriptions[0].subscription.source, peer.address());
            assert_eq!(
                &subscriptions[0].subscription.reply.as_ref().unwrap().sender,
                &SenderIdentity::Flow(peer.address().clone())
            );
        }
        let a_cursor = a.snapshot().await.unwrap().cursor;
        let b_cursor = b.snapshot().await.unwrap().cursor;
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(a.snapshot().await.unwrap().cursor, a_cursor);
        assert_eq!(b.snapshot().await.unwrap().cursor, b_cursor);
        let key = Uuid::new_v4().into();
        for worker in [&a, &b] {
            worker
                .control(
                    control_request(
                        worker,
                        worker.state().await.unwrap().revision(),
                        key,
                        PeerControl::Pause,
                    ),
                    |_| Ok(()),
                )
                .await
                .unwrap();
        }
        a.control(
            control_request(
                &a,
                a.state().await.unwrap().revision(),
                key,
                if close {
                    PeerControl::Close
                } else {
                    PeerControl::CancelTurn
                },
            ),
            |_| Ok(()),
        )
        .await
        .unwrap();
        let a_completion = tokio::time::timeout(Duration::from_secs(5), a_ticket.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(a_completion.result.is_err());
        if close {
            wait_closed(&a).await;
        }
        // B keeps both ordinary input and the completion while its own gate is paused.
        let pending = wait_inputs(&b, 2).await;
        assert!(b.completion(None).await.unwrap().is_none());
        assert!(matches!(
            pending.state.state(),
            InstanceState::Open(InstanceActivity::Active(_))
        ));
        assert_eq!(pending.state.pauses(), &[key]);
        let reply = pending
            .pending
            .iter()
            .find(|input| input.request.correlation.is_some())
            .unwrap()
            .id;
        b.control(
            control_request(&b, pending.state.revision(), key, PeerControl::Resume),
            |_| Ok(()),
        )
        .await
        .unwrap();
        let b_completion = tokio::time::timeout(Duration::from_secs(5), b_ticket.wait())
            .await
            .unwrap()
            .unwrap();
        assert!(b_completion.result.is_err());
        host.wait_quiescent().await.unwrap();
        let remaining = b.snapshot().await.unwrap();
        assert_eq!(remaining.pending.len(), 1);
        assert_ne!(remaining.pending[0].id, reply);
        assert!(matches!(
            remaining.pending[0].request.payload,
            InboxPayload::Interjection { .. }
        ));
        host.shutdown().await.unwrap();
        let restored = root.open_session(info.id).unwrap();
        let b = restored.open_flow(&b.address().instance_id).unwrap();
        assert_eq!(
            b.pending_inputs().cloned().collect::<Vec<_>>(),
            remaining.pending
        );
        assert_eq!(b.committed_inputs().len(), 2);
        let a = restored.open_flow(&a.address().instance_id).unwrap();
        assert_eq!(matches!(a.state().state(), InstanceState::Closed), close);
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
    drop(first_wait);
    // On this current-thread runtime the retry queues before the spawned cleanup.
    // The retired claim must not cancel the replacement when cleanup catches up.
    let first_wait = client
        .prepare_reply(first.address().clone(), retry.correlation)
        .await
        .unwrap();
    let duplicate = client
        .prepare_reply(first.address().clone(), retry.correlation)
        .await
        .unwrap();
    drop(duplicate);
    // Process a cancelled duplicate's queued cleanup before delivering the reply.
    tokio::task::yield_now().await;
    recipient.snapshot().await.unwrap();
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
    host.wait_quiescent().await.unwrap();
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
    // Reopening session storage requires its owner's shutdown receipt, not a flow notification close.
    tokio::time::timeout(Duration::from_secs(5), host.shutdown())
        .await
        .unwrap()
        .unwrap();
    drop(issues);
    drop(client);
    drop(a);
    drop(b);
    drop(session);
    drop(remote);
    drop(host);
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
    let result = tokio::time::timeout(Duration::from_secs(10), async {
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
    .await;
    assert!(
        result.is_ok(),
        "scheduled answer missing: {:?}; {:?}",
        worker.snapshot().await,
        worker.completion(None).await
    );
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
async fn quiescence_drains_handoffs_without_consuming_paused_or_unbound_input() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let host = RuntimeHost::default();
    let info = session_descriptor();
    let session = host
        .attach_session(root.create_session(info.clone()).unwrap())
        .unwrap();
    let a = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    let b = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    let c = session.create_flow(flow_descriptor(info.id)).await.unwrap();
    for worker in [&a, &b, &c] {
        change(worker, FlowTransition::Activated).await;
        session
            .schedule_flow(
                worker.address().instance_id.clone(),
                Executor::new(),
                prepare,
            )
            .await
            .unwrap();
    }
    for (recipient, source) in [(&a, &b), (&b, &c)] {
        let lease = start(recipient).await;
        let mut subscription = answer_watch(source);
        subscription.once = true;
        host.bind_invocation(lease.clone())
            .await
            .unwrap()
            .watch_events(subscription)
            .await
            .unwrap();
        change(
            recipient,
            FlowTransition::TurnFinished {
                lease,
                outcome: TurnOutcome::Completed,
            },
        )
        .await;
    }
    // Cancelling a batch waiter does not cancel the watchers or their owners.
    assert!(
        tokio::time::timeout(Duration::from_millis(30), host.wait_quiescent())
            .await
            .is_err()
    );
    let key = Uuid::now_v7().into();
    for worker in [&a, &b] {
        change(worker, FlowTransition::ExecutionPaused { key }).await;
    }
    tokio::time::timeout(Duration::from_secs(5), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    change(&b, FlowTransition::ExecutionResumed { key }).await;
    c.accept_input(peer_request(c.address(), c.address()), |_| Ok(()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    assert!(b.completion(None).await.unwrap().is_some());
    assert_eq!(a.snapshot().await.unwrap().pending.len(), 1);
    assert!(a.read_output(|_| Ok(())).await.unwrap().is_empty());
    change(&a, FlowTransition::ExecutionResumed { key }).await;
    tokio::time::timeout(Duration::from_secs(5), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    for worker in [&a, &b, &c] {
        assert!(worker.snapshot().await.unwrap().pending.is_empty());
        assert_eq!(worker.read_output(|_| Ok(())).await.unwrap().len(), 1);
    }
    session
        .stop_scheduling(a.address().instance_id.clone())
        .await
        .unwrap();
    a.accept_input(peer_request(c.address(), a.address()), |_| Ok(()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.snapshot().await.unwrap().pending.len(), 1);
    // Only the installed policy decides whether input can form a turn.
    session
        .schedule_flow(a.address().instance_id.clone(), Executor::new(), |_| {
            Ok(None)
        })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(a.snapshot().await.unwrap().pending.len(), 1);
    session
        .stop_scheduling(a.address().instance_id.clone())
        .await
        .unwrap();
    session
        .schedule_flow(a.address().instance_id.clone(), Executor::new(), prepare)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(5), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    assert!(a.snapshot().await.unwrap().pending.is_empty());
    assert_eq!(a.read_output(|_| Ok(())).await.unwrap().len(), 2);
    host.shutdown().await.unwrap();
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
    assert!(
        host.wait_quiescent()
            .await
            .unwrap_err()
            .to_string()
            .contains("scheduler")
    );
    session
        .schedule_flow(
            worker.address().instance_id.clone(),
            Executor::new(),
            prepare,
        )
        .await
        .unwrap();
    wait_answers(&worker, 1).await;
    host.wait_quiescent().await.unwrap();
    assert!(worker.snapshot().await.unwrap().pending.is_empty());
}
