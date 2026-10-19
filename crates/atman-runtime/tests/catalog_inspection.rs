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
    let source_log = flows[0].dir().join("events.jsonl");
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
    owner.select_default(address.instance_id.clone()).unwrap();
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
    let log = flow.dir().join("events.jsonl");
    let bytes = std::fs::read(&log).unwrap();
    let cache = owner.dir().join("session.json");
    std::fs::write(&cache, "invalid disposable cache").unwrap();
    assert_eq!(RuntimeStore::session_ids(&data).unwrap(), [id]);
    let read = RuntimeStore::inspect_session(&data, id).unwrap();
    let inspected_flow = read.inspect_flow(&address.instance_id).unwrap();
    assert_eq!(inspected_flow.state, *flow.state());
    assert_eq!(inspected_flow.descriptor, *flow.descriptor());
    assert_eq!(inspected_flow.journal.cursor.as_ref(), Some(flow.cursor()));
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
    assert_eq!(inspected.cursor.as_ref(), Some(flow.cursor()));
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
