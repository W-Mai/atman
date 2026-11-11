use std::{collections::BTreeMap, io::Write};

use atman_runtime::{
    event_log::journal::Journal,
    flow_store::{
        EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowTransition, RuntimeStore,
        SessionDescriptor,
    },
    source_program::RevisionBundle,
};
use uuid::Uuid;

#[tokio::test]
async fn flow_catalog_pages_preserve_owner_identity_authority_and_read_only_boundaries() {
    use atman_runtime::{
        flow_runtime::catalog::{
            FlowCatalogCursor, FlowCatalogEntry, OverviewExecution, SessionCatalog,
            WatchCatalogEntry, WorkspaceCatalogEntry,
        },
        flow_store::subscription::{FlowEventFilter, FlowSubscription, SubscriptionPurpose},
        flow_store::workspace::{WorkspaceRequest, WorkspaceSource},
        git_workspace::WorkspaceBinding,
    };
    let directory = tempfile::tempdir().unwrap();
    let runtime = RuntimeStore::open(directory.path()).unwrap();
    let project_root = directory.path().join("missing-repository");
    let external_root = directory.path().join("external-storage");
    let config =
        atman_runtime::config_hub::ConfigHub::from_config_dir(directory.path().join("config"));
    let project =
        atman_runtime::storage::ProjectContext::resolve(&config, &project_root, directory.path())
            .unwrap();
    let mut sessions = Vec::new();
    let mut flows: Vec<atman_runtime::flow_store::FlowStore> = Vec::new();
    for (index, count) in [(1, 1), (2, 3), (3, 0), (4, 0)] {
        let id = Uuid::from_u128(index);
        let mut session = runtime
            .create_session(SessionDescriptor {
                id,
                project: (index == 2).then(|| project.clone()),
                configuration: Default::default(),
                title: format!("Session {index}"),
            })
            .unwrap();
        for _ in 0..count {
            let mut flow = session
                .create_flow(FlowDescriptor {
                    address: FlowRef {
                        session_id: id.into(),
                        instance_id: FlowInstanceId::now(),
                    },
                    creation_request: Uuid::now_v7(),
                    created_by: None,
                    content: EntrySnapshot {
                        name: "main".into(),
                        path: "/missing/main.at".into(),
                        bundle: RevisionBundle {
                            entry: "entry:main.at".into(),
                            sources: BTreeMap::from([(
                                "entry:main.at".into(),
                                "flow main() -> int { return 7 }".into(),
                            )]),
                            edges: BTreeMap::new(),
                        },
                    }
                    .into(),
                })
                .unwrap();
            if index == 2 && flows.len() < 3 {
                let source = if flows.len() == 1 {
                    WorkspaceSource::Allocate {
                        base: "a".repeat(40),
                        retained: true,
                    }
                } else {
                    WorkspaceSource::Share {
                        sponsor: flows[1].descriptor().address.clone(),
                    }
                };
                flow.request_workspace(WorkspaceRequest {
                    binding: WorkspaceBinding {
                        workspace_id: "shared".into(),
                        repository_root: project.root().to_owned(),
                        path: external_root.join(".atman/worktrees/shared"),
                        branch: None,
                    },
                    source,
                })
                .unwrap();
            }
            flow.transition(flow.state().revision(), FlowTransition::Activated)
                .unwrap();
            flows.push(flow);
        }
        sessions.push(session);
    }
    let source = flows[0].descriptor().address.clone();
    let anchor = flows[0].observation_anchor();
    let watcher_id = Uuid::now_v7();
    // IDs belong to an owning Flow, so the same ID in two recipients must survive discovery.
    for flow in &mut flows[1..3] {
        flow.subscribe_events(
            FlowSubscription {
                id: watcher_id,
                source: source.clone(),
                filter: FlowEventFilter::FinalAnswers,
                once: false,
                purpose: SubscriptionPurpose::Watcher,
                expires_at: None,
                reply: None,
            },
            &anchor,
        )
        .unwrap();
    }
    let lease = flows[0]
        .state()
        .clone()
        .begin_turn(atman_rt::TurnId::now(), atman_rt::RunId::now())
        .unwrap();
    let revision = flows[0].state().revision();
    flows[0]
        .transition(
            revision,
            FlowTransition::TurnStarted {
                turn: lease.turn.clone(),
                run: lease.run.clone(),
            },
        )
        .unwrap();
    let mut message = atman_runtime::message::Message::assistant_text(lease.turn.clone(), "ready");
    message.origin = atman_runtime::message::MessageOrigin::FinalAnswer;
    flows[0]
        .publish_message(atman_runtime::flow_store::publication::MessagePublication {
            id: Uuid::now_v7(),
            run: lease.run.clone(),
            context_run: None,
            message,
            lease: lease.clone(),
        })
        .unwrap();
    let revision = flows[0].state().revision();
    flows[0]
        .transition(
            revision,
            FlowTransition::TurnFinished {
                lease,
                outcome: atman_rt::TurnOutcome::Completed,
            },
        )
        .unwrap();
    for index in 1..3 {
        let observed = flows[0]
            .events_after(
                anchor.cursor().sequence(),
                std::num::NonZeroUsize::new(10).unwrap(),
            )
            .unwrap();
        flows[index].observe_events(&observed).unwrap();
    }
    let revision = flows[1].state().revision();
    flows[1]
        .transition(
            revision,
            FlowTransition::TurnStarted {
                turn: atman_rt::TurnId::now(),
                run: atman_rt::RunId::now(),
            },
        )
        .unwrap();
    flows[1]
        .edit_goal(
            atman_runtime::flow_store::goal::GoalRequest {
                id: Uuid::now_v7(),
                sender: atman_runtime::flow_store::inbox::SenderIdentity::User(Uuid::now_v7()),
                expected: None,
                edit: atman_runtime::memory::goal::GoalEdit::Set {
                    text: "Inspect all sessions".into(),
                    replace_completed: false,
                },
                start_turn: false,
            },
            |_| Ok(()),
        )
        .unwrap();
    let active_state = flows[1].state().clone();
    let active_cursor = flows[1].cursor().clone();
    let active_address = flows[1].descriptor().address.clone();
    let other_address = flows[2].descriptor().address.clone();
    let session_log = sessions[3].dir().join("events.jsonl");
    let broken_flow_log = flows[3].dir().unwrap().join("events.jsonl");
    let active_log = flows[1].dir().unwrap().join("events.jsonl");
    drop(flows.pop());
    drop(sessions.pop());
    std::fs::write(&session_log, b"corrupt\n").unwrap();
    std::fs::write(&broken_flow_log, b"corrupt\n").unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&active_log)
        .unwrap()
        .write_all(b"{unfinished")
        .unwrap();
    let active_bytes = std::fs::read(&active_log).unwrap();
    let catalog = SessionCatalog::new(None, Some(directory.path().to_owned()));
    let mut cursor = None;
    let mut entries = Vec::new();
    let mut workspaces = Vec::new();
    let mut overview = Vec::new();
    let mut steps = 0;
    loop {
        let page = catalog
            .watch_page(cursor.clone(), 1, |_| Ok(true), |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(page.total_sessions, 4);
        assert_eq!(page.inspected, 1);
        assert!(page.next.is_none() || page.next != cursor);
        let bindings = catalog
            .workspace_page(cursor.clone(), 1, |_| Ok(true), |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(bindings.next, page.next);
        assert_eq!(bindings.total_sessions, page.total_sessions);
        assert_eq!(bindings.inspected, page.inspected);
        workspaces.extend(bindings.entries);
        let nodes = catalog
            .overview_page(cursor.clone(), 1, |_| Ok(true), |_| Ok(()))
            .await
            .unwrap();
        assert_eq!(nodes.next, page.next);
        assert_eq!(nodes.total_sessions, page.total_sessions);
        assert_eq!(nodes.inspected, page.inspected);
        overview.extend(nodes.entries);
        entries.extend(page.entries);
        steps += 1;
        assert!(steps <= 6, "pagination did not advance");
        let Some(next) = page.next else { break };
        cursor = Some(next);
    }
    assert_eq!(steps, 6); // Four Flows, one empty Session, one failed Session.
    assert_eq!(entries.len(), 5);
    assert_eq!(workspaces.len(), entries.len());
    assert_eq!(overview.len(), entries.len());
    assert!(matches!(
        &overview[0],
        FlowCatalogEntry::SessionUnavailable { .. }
    ));
    assert!(matches!(
        &overview[1],
        FlowCatalogEntry::Flow {
            snapshot: Err(_),
            ..
        }
    ));
    assert!(matches!(
        &workspaces[0],
        WorkspaceCatalogEntry::SessionUnavailable { .. }
    ));
    assert!(matches!(
        &workspaces[1],
        WorkspaceCatalogEntry::Flow {
            snapshot: Err(_),
            ..
        }
    ));
    for (offset, address) in [&other_address, &active_address, &source]
        .into_iter()
        .enumerate()
    {
        let FlowCatalogEntry::Flow {
            owner: node_owner,
            snapshot: Ok(node),
            ..
        } = &overview[offset + 2]
        else {
            panic!("missing overview snapshot")
        };
        assert_eq!(node_owner, address);
        assert_eq!(node.title, "main");
        assert!(matches!(node.execution, OverviewExecution::Detached));
        assert_eq!(node.pending_inputs, usize::from(offset != 2));
        assert_eq!(node.activity.is_some(), offset != 2);
        if offset == 1 {
            assert_eq!(
                node.goal.as_ref().unwrap().objective,
                "Inspect all sessions"
            );
            assert_eq!(node.recorded_state, active_state);
            assert_eq!(&node.cursor, active_cursor.require_durable().unwrap());
            assert_eq!(node.trailing_bytes, 11);
        }
        if offset != 2 {
            assert!(
                node.relations
                    .iter()
                    .any(|relation| relation.id == watcher_id)
            );
            assert_eq!(node.project_root.as_deref(), Some(project.root()));
        }
        let WorkspaceCatalogEntry::Flow {
            owner,
            snapshot: Ok(snapshot),
            ..
        } = &workspaces[offset + 2]
        else {
            panic!("missing workspace snapshot")
        };
        assert_eq!(owner, address);
        if offset == 2 {
            assert!(snapshot.workspace.is_none() && snapshot.project_root.is_none());
        } else {
            assert_eq!(snapshot.title, "main");
            assert_eq!(snapshot.project_root.as_deref(), Some(project.root()));
            let request = snapshot.workspace.as_ref().unwrap();
            assert_eq!(
                request.binding.path,
                external_root.join(".atman/worktrees/shared")
            );
            if offset == 1 {
                assert_eq!(&snapshot.cursor, active_cursor.require_durable().unwrap());
                assert_eq!(snapshot.trailing_bytes, 11);
                assert!(matches!(
                    request.source,
                    WorkspaceSource::Allocate { retained: true, .. }
                ));
            } else {
                assert!(
                    matches!(&request.source, WorkspaceSource::Share { sponsor } if sponsor == &active_address)
                );
            }
        }
    }
    assert!(!project_root.exists() && !external_root.exists());
    assert!(
        catalog
            .workspace_page(None, 0, |_| Ok(true), |_| Ok(()))
            .await
            .is_err()
    );
    let checks = std::cell::Cell::new(0);
    let denied = catalog
        .workspace_page(
            None,
            50,
            |_| Ok(true),
            |owner| {
                if owner == &active_address {
                    checks.set(checks.get() + 1);
                    if checks.get() == 3 {
                        return Err(std::io::ErrorKind::PermissionDenied.into());
                    }
                }
                Ok(())
            },
        )
        .await;
    assert!(matches!(denied, Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied));
    assert_eq!(checks.get(), 3);
    checks.set(0);
    let denied = catalog
        .overview_page(
            None,
            50,
            |_| Ok(true),
            |owner| {
                if owner == &active_address {
                    checks.set(checks.get() + 1);
                    if checks.get() == 3 {
                        return Err(std::io::ErrorKind::PermissionDenied.into());
                    }
                }
                Ok(())
            },
        )
        .await;
    assert!(matches!(denied, Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied));
    assert_eq!(checks.get(), 3);
    assert!(
        matches!(&entries[0], WatchCatalogEntry::SessionUnavailable { session_id, .. } if *session_id == Uuid::from_u128(4))
    );
    assert!(matches!(
        &entries[1],
        WatchCatalogEntry::Flow {
            snapshot: Err(_),
            ..
        }
    ));
    let records: Vec<_> = entries
        .iter()
        .filter_map(|entry| match entry {
            WatchCatalogEntry::Flow {
                owner,
                session_title,
                snapshot: Ok(snapshot),
            } if !snapshot.records.is_empty() => {
                assert_eq!(session_title.as_deref(), Some("Session 2"));
                assert_eq!(snapshot.title, "main");
                assert_eq!(snapshot.records[0].registration.id(), watcher_id);
                Some((owner, snapshot))
            }
            _ => None,
        })
        .collect();
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].0, &other_address);
    assert_eq!(records[1].0, &active_address);
    assert_eq!(
        &records[1].1.cursor,
        active_cursor.require_durable().unwrap()
    );
    assert_eq!(records[1].1.trailing_bytes, 11);
    for (_, snapshot) in records {
        assert_eq!(snapshot.records[0].progress.matched, 1);
        assert_eq!(snapshot.records[0].progress.pending, 1);
        assert_eq!(snapshot.records[0].progress.input_committed, 0);
    }
    let hidden = catalog
        .watch_page(None, 50, |id| Ok(id == Uuid::from_u128(1)), |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(hidden.total_sessions, 1);
    assert_eq!(hidden.entries.len(), 1);
    assert!(hidden.next.is_none());
    let authorizations = std::cell::Cell::new(0);
    let denied = catalog
        .watch_page(
            None,
            50,
            |_| Ok(true),
            |owner| {
                if owner == &active_address {
                    authorizations.set(authorizations.get() + 1);
                    if authorizations.get() == 3 {
                        return Err(std::io::ErrorKind::PermissionDenied.into());
                    }
                }
                Ok(())
            },
        )
        .await;
    assert!(matches!(denied, Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied));
    assert_eq!(authorizations.get(), 3);
    let checks = std::cell::Cell::new(0);
    let revoked = catalog
        .watch_page(
            None,
            50,
            |id| {
                if id == Uuid::from_u128(4) {
                    checks.set(checks.get() + 1);
                    return Ok(checks.get() < 3);
                }
                Ok(true)
            },
            |_| Ok(()),
        )
        .await;
    assert!(matches!(revoked, Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied));
    for invalid_limit in [0, 51] {
        assert!(
            catalog
                .watch_page(None, invalid_limit, |_| Ok(true), |_| Ok(()))
                .await
                .is_err()
        );
    }
    let beyond = catalog
        .watch_page(
            Some(FlowCatalogCursor {
                session_id: Uuid::nil(),
                before: None,
            }),
            1,
            |_| Ok(true),
            |_| Ok(()),
        )
        .await
        .unwrap();
    assert_eq!(beyond.inspected, 0);
    assert!(beyond.entries.is_empty() && beyond.next.is_none());
    let exhausted = catalog
        .watch_page(
            Some(FlowCatalogCursor {
                session_id: Uuid::from_u128(2),
                before: Some(active_address.instance_id.clone()),
            }),
            1,
            |_| Ok(true),
            |_| Ok(()),
        )
        .await
        .unwrap();
    assert_eq!(exhausted.inspected, 1);
    assert!(exhausted.entries.is_empty());
    assert_eq!(exhausted.next.unwrap().session_id, Uuid::from_u128(1));
    assert_eq!(*flows[1].state(), active_state);
    assert_eq!(std::fs::read(&active_log).unwrap(), active_bytes);
    assert_eq!(std::fs::read(&session_log).unwrap(), b"corrupt\n");
    assert_eq!(std::fs::read(&broken_flow_log).unwrap(), b"corrupt\n");
    drop(flows);
    drop(sessions);
    let inspection =
        RuntimeStore::inspect_session(directory.path(), active_address.session_id.0).unwrap();
    assert_eq!(
        inspection
            .inspect_flow(&active_address.instance_id)
            .unwrap()
            .state,
        active_state
    );
    assert_eq!(std::fs::read(&active_log).unwrap(), active_bytes);
    // Merely attaching a Session must not recover or load its recorded active Flow.
    let host = atman_runtime::flow_runtime::host::RuntimeHost::default();
    let session = host
        .attach_session(runtime.open_session(active_address.session_id.0).unwrap())
        .unwrap();
    let attached_catalog =
        SessionCatalog::new(Some(host.clone()), Some(directory.path().to_owned()));
    let attached = attached_catalog
        .overview_page(
            None,
            50,
            |id| Ok(id == active_address.session_id.0),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let snapshot = attached
        .entries
        .iter()
        .find_map(|entry| match entry {
            FlowCatalogEntry::Flow {
                owner,
                snapshot: Ok(snapshot),
                ..
            } if owner == &active_address => Some(snapshot),
            _ => None,
        })
        .unwrap();
    assert!(matches!(snapshot.execution, OverviewExecution::Detached));
    assert_eq!(snapshot.recorded_state, active_state);
    assert_eq!(&snapshot.cursor, active_cursor.require_durable().unwrap());
    assert_eq!(snapshot.trailing_bytes, 11);
    assert!(session.observe().await.unwrap().loaded.is_empty());
    assert_eq!(std::fs::read(&active_log).unwrap(), active_bytes);

    // Only explicit opening owns recovery; the next directory sample acknowledges that writer.
    let worker = session
        .open_flow(active_address.instance_id.clone())
        .await
        .unwrap();
    session
        .select_default(
            session.snapshot().await.unwrap().cursor,
            active_address.instance_id.clone(),
        )
        .await
        .unwrap();
    let loaded = attached_catalog
        .overview_page(
            None,
            50,
            |id| Ok(id == active_address.session_id.0),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let loaded = loaded
        .entries
        .iter()
        .find_map(|entry| match entry {
            FlowCatalogEntry::Flow {
                owner,
                snapshot: Ok(snapshot),
                ..
            } if owner == &active_address => Some(snapshot),
            _ => None,
        })
        .unwrap();
    let OverviewExecution::Loaded { state } = &loaded.execution else {
        panic!("missing live observation")
    };
    assert_eq!(*state, worker.status().await.unwrap().execution);
    assert_eq!(*state, loaded.recorded_state);
    assert!(loaded.default_flow);
    assert_ne!(&loaded.cursor, active_cursor.require_durable().unwrap());
    assert_eq!(loaded.trailing_bytes, 0);
    assert_eq!(loaded.pending_inputs, 1);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn relation_catalog_pages_recheck_visibility_and_advance_past_empty_or_broken_sessions() {
    use atman_runtime::{
        flow_runtime::catalog::SessionCatalog,
        flow_store::access::{AccessTarget, FlowAction, FlowGrant},
    };
    let directory = tempfile::tempdir().unwrap();
    let runtime = RuntimeStore::open(directory.path()).unwrap();
    let mut owners = Vec::new();
    let mut flows = Vec::new();
    for index in 1..=4 {
        let session = Uuid::from_u128(index);
        let mut owner = runtime
            .create_session(SessionDescriptor {
                id: session,
                project: None,
                configuration: Default::default(),
                title: format!("Session {index}"),
            })
            .unwrap();
        if index <= 2 {
            flows.push(
                owner
                    .create_flow(FlowDescriptor {
                        address: FlowRef {
                            session_id: session.into(),
                            instance_id: FlowInstanceId::now(),
                        },
                        creation_request: Uuid::now_v7(),
                        created_by: None,
                        content: EntrySnapshot {
                            name: "main".into(),
                            path: "/missing/main.at".into(),
                            bundle: RevisionBundle {
                                entry: "entry:main.at".into(),
                                sources: BTreeMap::from([(
                                    "entry:main.at".into(),
                                    "flow main() -> int { return 7 }".into(),
                                )]),
                                edges: BTreeMap::new(),
                            },
                        }
                        .into(),
                    })
                    .unwrap(),
            );
        }
        owners.push(owner);
    }
    let source = flows[0].descriptor().address.clone();
    let target = flows[1].descriptor().address.clone();
    let receipt = owners[0].cursor().clone();
    owners[0]
        .grant_access(
            &receipt,
            FlowGrant {
                id: Uuid::now_v7(),
                grantee: source.clone(),
                target: AccessTarget::Flow(target.clone()),
                actions: [FlowAction::Watch].into_iter().collect(),
            },
        )
        .unwrap();
    let broken = owners.pop().unwrap();
    let broken_log = broken.dir().join("events.jsonl");
    drop(broken);
    std::fs::write(&broken_log, b"corrupt\n").unwrap();
    let source_log = flows[0].dir().unwrap().join("events.jsonl");
    let source_bytes = std::fs::read(&source_log).unwrap();
    let catalog = SessionCatalog::new(None, Some(directory.path().to_owned()));
    let first = catalog.relation_page(None, 2, |_| Ok(true)).await.unwrap();
    assert_eq!(first.total, 4);
    assert_eq!(first.sessions.len(), 2);
    assert!(first.sessions[0].relations.is_err());
    assert!(first.sessions[1].relations.as_ref().unwrap().is_empty());
    assert_eq!(first.next, Some(Uuid::from_u128(3)));
    let last = catalog
        .relation_page(first.next, 2, |_| Ok(true))
        .await
        .unwrap();
    assert!(last.next.is_none());
    let records: Vec<_> = last
        .sessions
        .iter()
        .flat_map(|entry| entry.relations.as_ref().unwrap())
        .collect();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].source, source);
    assert_eq!(records[0].target, AccessTarget::Flow(target));
    let hidden = catalog
        .relation_page(None, 4, |id| Ok(id != source.session_id.0))
        .await
        .unwrap();
    assert_eq!(hidden.total, 3);
    assert!(
        hidden
            .sessions
            .iter()
            .all(|entry| entry.session_id != source.session_id.0)
    );
    let reads = std::cell::Cell::new(0);
    let revoked = catalog
        .relation_page(None, 2, |id| {
            if id == source.session_id.0 {
                reads.set(reads.get() + 1);
                return Ok(reads.get() == 1);
            }
            Ok(true)
        })
        .await;
    assert!(matches!(revoked, Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied));
    assert!(catalog.relation_page(None, 0, |_| Ok(true)).await.is_err());
    assert_eq!(std::fs::read(&source_log).unwrap(), source_bytes);
    assert_eq!(std::fs::read(&broken_log).unwrap(), b"corrupt\n");
}

#[test]
fn inspection_is_bounded_while_the_writer_appends_and_leaves_incomplete_tails_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let id = Uuid::new_v4();
    let mut writer = Journal::create(&path, id).unwrap();
    let first = writer.commit(&["first"]).unwrap();
    let mut seen = vec![];
    let inspected = Journal::inspect(&path, id, |_, facts: Vec<String>| {
        seen.extend(facts);
        writer.commit(&["appended during read"])?;
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, ["first"]);
    assert_eq!(inspected.cursor, Some(first));
    assert_eq!(inspected.trailing_bytes, 0);
    assert_eq!(writer.sequence(), 2);
    drop(writer);
    let complete = std::fs::read(&path).unwrap();
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"{unfinished")
        .unwrap();
    let before = std::fs::read(&path).unwrap();
    let inspected = Journal::inspect::<String>(&path, id, |_, _| Ok(())).unwrap();
    assert_eq!(inspected.cursor.unwrap().sequence, 2);
    assert_eq!(inspected.valid_bytes, complete.len() as u64);
    assert_eq!(inspected.trailing_bytes, 11);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    let (mut reopened, tail) = Journal::open::<String>(&path, id, |_, _| Ok(())).unwrap();
    assert_eq!(tail.unwrap().bytes, 11);
    assert_eq!(std::fs::read(&path).unwrap(), complete);
    assert_eq!(reopened.commit(&["after recovery"]).unwrap().sequence, 3);
}

#[test]
fn inspection_and_recovery_reject_the_same_complete_corruption_without_rewriting_it() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("events.jsonl");
    let id = Uuid::new_v4();
    let mut writer = Journal::create(&path, id).unwrap();
    writer.commit(&["first"]).unwrap();
    writer.commit(&["second"]).unwrap();
    drop(writer);
    let original = std::fs::read_to_string(&path).unwrap();
    let corrupted = original.replace("second", "broken");
    std::fs::write(&path, &corrupted).unwrap();
    let inspect = Journal::inspect::<String>(&path, id, |_, _| Ok(())).unwrap_err();
    let recover = Journal::open::<String>(&path, id, |_, _| Ok(()))
        .err()
        .unwrap();
    assert_eq!(inspect.to_string(), recover.to_string());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupted);
    assert!(Journal::inspect::<String>(&path, Uuid::new_v4(), |_, _| Ok(())).is_err());
}

#[test]
fn catalog_inspection_ignores_cache_and_never_recovers_an_active_flow_or_creates_storage() {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    assert!(RuntimeStore::session_ids(&data).unwrap().is_empty());
    assert!(RuntimeStore::inspect_session(&data, Uuid::new_v4()).is_err());
    assert!(!data.exists());
    let runtime = RuntimeStore::open(&data).unwrap();
    let id = Uuid::new_v4();
    let mut owner = runtime
        .create_session(SessionDescriptor {
            id,
            project: None,
            configuration: Default::default(),
            title: "Recorded title".into(),
        })
        .unwrap();
    let address = FlowRef {
        session_id: id.into(),
        instance_id: FlowInstanceId::now(),
    };
    let mut flow = owner
        .create_flow(FlowDescriptor {
            address: address.clone(),
            creation_request: Uuid::new_v4(),
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: "/missing/main.at".into(),
                bundle: RevisionBundle {
                    entry: "entry:main.at".into(),
                    sources: BTreeMap::from([(
                        "entry:main.at".into(),
                        "flow main() -> int { return 7 }".into(),
                    )]),
                    edges: BTreeMap::new(),
                },
            }
            .into(),
        })
        .unwrap();
    owner
        .select_default(&owner.cursor().clone(), address.instance_id.clone())
        .unwrap();
    flow.transition(flow.state().revision(), FlowTransition::Activated)
        .unwrap();
    flow.transition(
        flow.state().revision(),
        FlowTransition::TurnStarted {
            turn: atman_rt::TurnId::now(),
            run: atman_rt::RunId::now(),
        },
    )
    .unwrap();
    let log = flow.dir().unwrap().join("events.jsonl");
    let bytes = std::fs::read(&log).unwrap();
    let cache = owner.dir().join("session.json");
    std::fs::write(&cache, "invalid disposable cache").unwrap();
    assert_eq!(RuntimeStore::session_ids(&data).unwrap(), [id]);
    let read = RuntimeStore::inspect_session(&data, id).unwrap();
    let inspected_flow = read.inspect_flow(&address.instance_id).unwrap();
    assert_eq!(inspected_flow.state, *flow.state());
    assert_eq!(inspected_flow.descriptor, *flow.descriptor());
    assert_eq!(
        inspected_flow.journal.cursor.as_ref(),
        Some(flow.cursor().require_durable().unwrap())
    );
    assert_eq!(
        owner.inspect_flow(&address.instance_id).unwrap().state,
        *flow.state()
    );
    assert_eq!(read.projection.descriptor.title, "Recorded title");
    assert_eq!(
        read.projection.default_flow,
        Some(address.instance_id.clone())
    );
    let mut updates = Vec::new();
    let inspected = read
        .inspect_flow_updates(&address.instance_id, |_, batch| {
            updates.extend(batch);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        inspected.cursor.as_ref(),
        Some(flow.cursor().require_durable().unwrap())
    );
    assert!(updates.iter().any(|update| matches!(
        update,
        atman_runtime::flow_store::view::FlowUpdate::Transition(FlowTransition::TurnStarted { .. })
    )));
    assert!(
        read.inspect_flow_updates(&FlowInstanceId::now(), |_, _| Ok(()))
            .is_err()
    );
    assert_eq!(read.journal.cursor.as_ref(), Some(owner.cursor()));
    assert_eq!(read.projection.flows, owner.projection().flows);
    assert_eq!(std::fs::read(&log).unwrap(), bytes);
    assert_eq!(
        std::fs::read_to_string(&cache).unwrap(),
        "invalid disposable cache"
    );
    drop(flow);
    drop(owner);
    let read = RuntimeStore::inspect_session(&data, id).unwrap();
    assert_eq!(
        read.inspect_flow(&address.instance_id).unwrap().state,
        inspected_flow.state
    );
    assert_eq!(std::fs::read(&log).unwrap(), bytes);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log)
        .unwrap()
        .write_all(b"{partial")
        .unwrap();
    let partial = std::fs::read(&log).unwrap();
    let inspection = read.inspect_flow(&address.instance_id).unwrap();
    assert_eq!(inspection.state, inspected_flow.state);
    assert_eq!(inspection.journal.trailing_bytes, 8);
    assert_eq!(std::fs::read(&log).unwrap(), partial);
    std::fs::write(&log, b"corrupt\n").unwrap();
    assert!(read.inspect_flow(&address.instance_id).is_err());
    assert_eq!(std::fs::read(&log).unwrap(), b"corrupt\n");
}
