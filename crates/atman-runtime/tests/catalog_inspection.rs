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
    assert_eq!(read.projection.descriptor.title, "Recorded title");
    assert_eq!(read.projection.default_flow, Some(address.instance_id));
    assert_eq!(read.journal.cursor.as_ref(), Some(owner.cursor()));
    assert_eq!(read.projection.flows, owner.projection().flows);
    assert_eq!(std::fs::read(&log).unwrap(), bytes);
    assert_eq!(
        std::fs::read_to_string(&cache).unwrap(),
        "invalid disposable cache"
    );
    drop(flow);
    drop(owner);
    RuntimeStore::inspect_session(&data, id).unwrap();
    assert_eq!(std::fs::read(&log).unwrap(), bytes);
}
