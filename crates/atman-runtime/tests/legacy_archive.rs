use std::num::NonZeroUsize;

use atman_rt::InstanceState;
use atman_runtime::event_log::legacy::{self, LegacyFlowKey};
use atman_runtime::flow_store::{
    FlowContent, FlowDescriptor, FlowInstanceId, FlowRef, FlowTransition, RuntimeStore,
    SessionDescriptor,
    history::{LegacyArchive, LegacyHistoryRecord},
    view::FlowUpdate,
};
use uuid::Uuid;

#[test]
fn archives_reopen_raw_history_without_an_executable_entry_or_original_source() {
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("old.jsonl");
    let raw = b"{\"type\":\"mermaid_diagram\",\"source\":\"hello\"}\ninvalid\n\xfftorn";
    std::fs::write(&source, raw).unwrap();
    let old_session = Uuid::now_v7();
    let inspection = legacy::inspect(&source, old_session).unwrap();
    let store = RuntimeStore::open(temp.path()).unwrap();
    let id = Uuid::now_v7();
    let mut session = store
        .create_session(SessionDescriptor {
            id,
            project: None,
            configuration: Default::default(),
            title: "Imported history".into(),
        })
        .unwrap();
    let descriptor = FlowDescriptor {
        address: FlowRef {
            session_id: id.into(),
            instance_id: FlowInstanceId::now(),
        },
        creation_request: Uuid::now_v7(),
        created_by: None,
        content: FlowContent::History(LegacyArchive {
            session_id: old_session,
            source: inspection.source,
            owner: LegacyFlowKey::Unassigned,
            records: 3,
        }),
    };
    let records = inspection
        .records
        .into_iter()
        .map(|record| {
            let bytes = &raw[record.offset as usize..(record.offset + record.bytes) as usize];
            LegacyHistoryRecord::new(record, bytes)
        })
        .collect::<Vec<_>>();
    let mut flow = session.create_flow(descriptor.clone()).unwrap();
    assert_eq!(flow.state().state(), &InstanceState::Closed);
    assert!(flow.descriptor().entry().is_err());
    assert!(
        flow.transition(flow.state().revision(), FlowTransition::Activated)
            .is_err()
    );
    assert!(flow.completion(None).is_none());
    let created = flow.cursor().clone();
    let receipt = flow.append_legacy_history(records[..1].to_vec()).unwrap();
    let mut bad = records[1].clone();
    bad.raw_base64 = "YWRkaXRpb25hbCBjb250ZW50".into();
    assert!(flow.append_legacy_history(vec![bad]).is_err());
    assert!(flow.append_legacy_history(records[..1].to_vec()).is_err());
    assert_eq!(flow.cursor(), &receipt);
    drop(flow);
    drop(session);
    let session = store.open_session(id).unwrap();
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    assert_eq!(flow.history_progress().records, 1);
    flow.append_legacy_history(records[1..].to_vec()).unwrap();
    assert_eq!(flow.history_progress().records, 3);
    let complete = flow.cursor().clone();
    assert!(flow.append_legacy_history(records[2..].to_vec()).is_err());
    assert_eq!(flow.cursor(), &complete);
    std::fs::remove_file(source).unwrap();
    drop(flow);
    drop(session);
    let session = store.open_session(id).unwrap();
    let mut flow = session.open_flow(&descriptor.address.instance_id).unwrap();
    let page = flow
        .read_updates(Some(&created), NonZeroUsize::new(10).unwrap())
        .unwrap();
    assert_eq!(page.batches.len(), 2);
    let restored = page
        .batches
        .into_iter()
        .flat_map(|batch| batch.facts)
        .flat_map(|fact| match fact {
            FlowUpdate::LegacyRecords(records) => records,
            _ => panic!("history acquired an execution fact"),
        })
        .collect::<Vec<_>>();
    assert_eq!(records, restored);
    assert_eq!(
        restored
            .iter()
            .flat_map(|r| r.bytes().unwrap())
            .collect::<Vec<_>>(),
        raw
    );
    assert_eq!(flow.state().state(), &InstanceState::Closed);
    assert_eq!(flow.cursor(), &complete);
}
