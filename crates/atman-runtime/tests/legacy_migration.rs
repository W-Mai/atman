use std::collections::BTreeMap;
use std::num::NonZeroUsize;

use atman_rt::InstanceState;
use atman_runtime::event_log::legacy::{self, LegacyFlowKey};
use atman_runtime::flow_store::{RuntimeStore, SessionDescriptor, migration, view::FlowUpdate};
use serde_json::json;
use uuid::Uuid;

#[test]
fn migration_publishes_exact_isolated_history_and_retries_without_execution() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("events.jsonl");
    let source_session = Uuid::now_v7();
    let main = Uuid::now_v7();
    let child = Uuid::now_v7();
    let mut raw = b" \n".to_vec();
    for event in [
        json!({"type":"flow_start", "run_id":main, "flow_name":"agent", "spawned":false}),
        json!({"type":"goal_turn_started", "run_id":main, "owner":source_session}),
        json!({"type":"flow_start", "run_id":child, "flow_name":"child", "parent_run_id":main, "spawned":true}),
        json!({"type":"goal_turn_started", "run_id":child, "owner":"child-agent"}),
        json!({"type":"mermaid_diagram", "source":"retained diagnostic"}),
    ] {
        serde_json::to_writer(&mut raw, &event).unwrap();
        raw.push(b'\n');
    }
    raw.extend_from_slice(b"{invalid\n\xfftorn");
    std::fs::write(&path, &raw).unwrap();
    let report = legacy::inspect(&path, source_session).unwrap();
    let root = RuntimeStore::open(temp.path()).unwrap();
    let descriptor = SessionDescriptor {
        id: migration::destination_id(&path, source_session).unwrap(),
        project: None,
        configuration: Default::default(),
        title: "Retained conversation".into(),
    };
    assert!(
        root.archive_legacy(&path, source_session, descriptor.clone(), false)
            .is_err()
    );
    assert!(!root.root().join(".migrations").exists());
    let receipt = root
        .archive_legacy(&path, source_session, descriptor.clone(), true)
        .unwrap();
    assert_eq!(receipt.records, 7);
    assert_eq!(receipt.blank_lines, 1);
    assert_eq!(receipt.flows.len(), 3);
    assert_eq!(std::fs::read(&path).unwrap(), raw);
    assert_eq!(
        root.archive_legacy(&path, source_session, descriptor.clone(), true)
            .unwrap(),
        receipt
    );
    let session = root.open_session(descriptor.id).unwrap();
    assert_eq!(session.projection().migrated_from.as_ref(), Some(&receipt));
    assert_eq!(
        std::fs::read(session.dir().join("artifacts/legacy-events.jsonl")).unwrap(),
        raw
    );
    let mut restored = BTreeMap::new();
    for mapping in &receipt.flows {
        let mut flow = session.open_flow(&mapping.instance).unwrap();
        assert_eq!(flow.state().state(), &InstanceState::Closed);
        assert!(flow.descriptor().entry().is_err());
        assert_eq!(flow.cursor().require_durable().unwrap(), &mapping.cursor);
        let page = flow
            .read_updates(None, NonZeroUsize::new(10).unwrap())
            .unwrap();
        for update in page.batches.into_iter().flat_map(|batch| batch.facts) {
            match update {
                FlowUpdate::Created { .. } => {}
                FlowUpdate::LegacyRecords(records) => {
                    for record in records {
                        assert_eq!(record.source.owner.key(), mapping.owner);
                        assert_eq!(
                            record.bytes().unwrap(),
                            raw[record.source.offset as usize
                                ..(record.source.offset + record.source.bytes) as usize]
                        );
                        assert!(restored.insert(record.source.line, record.source).is_none());
                    }
                }
                _ => panic!("migration replayed execution"),
            }
        }
    }
    assert_eq!(restored.into_values().collect::<Vec<_>>(), report.records);
    let default = receipt
        .flows
        .iter()
        .find(|flow| flow.owner == LegacyFlowKey::Default)
        .unwrap();
    assert_eq!(
        session.projection().default_flow.as_ref(),
        Some(&default.instance)
    );
    drop(session);

    let snapshot = root
        .root()
        .join("sessions")
        .join(descriptor.id.to_string())
        .join("artifacts/legacy-events.jsonl");
    std::fs::write(&snapshot, b"damaged retained source").unwrap();
    assert!(
        root.archive_legacy(&path, source_session, descriptor.clone(), true)
            .is_err()
    );
    std::fs::write(&snapshot, &raw).unwrap();

    // A late write from an old program is a separate source branch, never a silent append.
    std::fs::write(&path, b"changed source\n").unwrap();
    assert_eq!(
        migration::destination_id(&path, source_session).unwrap(),
        descriptor.id
    );
    assert!(
        root.archive_legacy(&path, source_session, descriptor.clone(), true)
            .is_err()
    );
    let branch = SessionDescriptor {
        id: Uuid::now_v7(),
        ..descriptor.clone()
    };
    assert!(
        root.archive_legacy(&path, source_session, branch, true)
            .is_ok()
    );
    std::fs::remove_file(&path).unwrap();
    let session = root.open_session(descriptor.id).unwrap();
    assert_eq!(session.projection().migrated_from.as_ref(), Some(&receipt));
    assert_eq!(
        std::fs::read(session.dir().join("artifacts/legacy-events.jsonl")).unwrap(),
        raw
    );
}

#[test]
fn destination_conflicts_preserve_existing_sessions_and_empty_history_has_a_default() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("events.jsonl");
    std::fs::write(&path, []).unwrap();
    let root = RuntimeStore::open(temp.path()).unwrap();
    let source = Uuid::now_v7();
    let mut descriptor = SessionDescriptor {
        id: Uuid::now_v7(),
        project: None,
        configuration: Default::default(),
        title: "Occupied".into(),
    };
    let occupied = root.create_session(descriptor.clone()).unwrap();
    let before = std::fs::read(occupied.dir().join("events.jsonl")).unwrap();
    let occupied_path = occupied.dir().to_path_buf();
    drop(occupied);
    assert!(
        root.archive_legacy(&path, source, descriptor.clone(), true)
            .is_err()
    );
    assert_eq!(
        std::fs::read(occupied_path.join("events.jsonl")).unwrap(),
        before
    );
    descriptor.id = source;
    assert!(
        root.archive_legacy(&path, source, descriptor.clone(), true)
            .is_err()
    );
    descriptor.id = Uuid::now_v7();
    let receipt = root
        .archive_legacy(&path, source, descriptor.clone(), true)
        .unwrap();
    assert_eq!(receipt.records, 0);
    assert_eq!(receipt.flows.len(), 1);
    assert_eq!(receipt.flows[0].owner, LegacyFlowKey::Default);
    assert_eq!(receipt.flows[0].cursor.sequence, 1);
    assert_eq!(
        root.archive_legacy(&path, source, descriptor, true)
            .unwrap(),
        receipt
    );
}
