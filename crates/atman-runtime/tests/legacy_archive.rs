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
fn direct_history_and_migration_share_partitions_and_display_without_writes() {
    use atman_runtime::{
        TranscriptEntry,
        message::{Message, MessageOrigin},
        projection::flow::{ArchiveTranscript, LegacyHistory},
    };
    let temp = tempfile::tempdir().unwrap();
    let source = temp.path().join("old.jsonl");
    let id = Uuid::now_v7();
    let root = Uuid::now_v7();
    let child = Uuid::now_v7();
    let turn = atman_rt::TurnId::now();
    let mut final_message = Message::assistant_text(turn.clone(), "Child final reply");
    final_message.origin = MessageOrigin::FinalAnswer;
    let events = [
        serde_json::json!({"type":"flow_start","run_id":root,"flow_name":"agent","spawned":false}),
        serde_json::json!({"type":"user_msg","turn_id":turn,"message":Message::user_text(turn.clone(), "Root request")}),
        serde_json::json!({"type":"flow_start","run_id":child,"flow_name":"child","parent_run_id":root,"spawned":true}),
        serde_json::json!({"type":"assistant_msg","turn_id":turn,"flow_run_id":child,"message":final_message}),
        serde_json::json!({"type":"flow_end","run_id":child,"flow_name":"child","status":{"kind":"ok"}}),
        serde_json::json!({"type":"assistant_msg","turn_id":turn,"flow_run_id":root,"message":Message::assistant_text(turn.clone(), "Root reply")}),
    ];
    let mut raw = events
        .into_iter()
        .enumerate()
        .flat_map(|(index, mut value)| {
            // Display must not invent timestamps for older records.
            value["seq"] = serde_json::json!(index + 1);
            format!("{value}\n").into_bytes()
        })
        .collect::<Vec<_>>();
    raw.extend_from_slice(b"invalid\n\xfftorn");
    std::fs::write(&source, &raw).unwrap();
    let history = LegacyHistory::inspect(&source, id).unwrap();
    assert_eq!(history.inspection().source.bytes, raw.len() as u64);
    let views = history
        .flows()
        .iter()
        .map(|key| history.read_flow(key).unwrap())
        .collect::<Vec<_>>();
    assert_eq!(views.len(), 3);
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    assert_eq!(std::fs::read(&source).unwrap(), raw);
    assert_eq!(
        history
            .read_flow(&LegacyFlowKey::Agent {
                owner: "missing".into()
            })
            .err()
            .unwrap()
            .kind(),
        std::io::ErrorKind::NotFound
    );

    fn messages(view: &ArchiveTranscript) -> Vec<serde_json::Value> {
        view.entries
            .iter()
            .filter_map(|entry| match entry {
                TranscriptEntry::Message { message, .. } => {
                    Some(serde_json::to_value(message).unwrap())
                }
                _ => None,
            })
            .collect()
    }
    let root_view = views
        .iter()
        .find(|view| view.owner() == &LegacyFlowKey::Default)
        .unwrap();
    assert!(
        messages(root_view.transcript())
            .iter()
            .any(|message| message.to_string().contains("Root reply"))
    );
    assert!(
        !messages(root_view.transcript())
            .iter()
            .any(|message| message.to_string().contains("Child final reply"))
    );
    let child_view = views
        .iter()
        .find(|view| matches!(view.owner(), LegacyFlowKey::SpawnedRun { .. }))
        .unwrap();
    assert!(
        messages(child_view.transcript())
            .iter()
            .any(|message| message.to_string().contains("Child final reply"))
    );
    let unassigned = views
        .iter()
        .find(|view| view.owner() == &LegacyFlowKey::Unassigned)
        .unwrap();
    assert_eq!(
        unassigned
            .transcript()
            .unparsed
            .iter()
            .flat_map(|record| record.bytes().unwrap())
            .collect::<Vec<_>>(),
        b"invalid\n\xfftorn"
    );

    // Even a change outside the selected flow invalidates the old directory.
    let mut appended = raw.clone();
    appended.push(b'\n');
    std::fs::write(&source, appended).unwrap();
    assert!(history.read_flow(&LegacyFlowKey::Default).is_err());
    assert!(
        messages(root_view.transcript())
            .iter()
            .any(|message| message.to_string().contains("Root reply"))
    );
    std::fs::write(&source, &raw).unwrap();

    let store = RuntimeStore::open(temp.path()).unwrap();
    let destination = atman_runtime::flow_store::migration::destination_id(&source, id).unwrap();
    let receipt = store
        .archive_legacy(
            &source,
            id,
            SessionDescriptor {
                id: destination,
                project: None,
                configuration: Default::default(),
                title: "Imported".into(),
            },
            true,
        )
        .unwrap();
    std::fs::remove_file(&source).unwrap();
    assert!(history.read_flow(&LegacyFlowKey::Default).is_err());
    let read = RuntimeStore::inspect_session(temp.path(), destination).unwrap();
    for mapping in receipt.flows {
        let old = views
            .iter()
            .find(|view| view.owner() == &mapping.owner)
            .unwrap();
        assert_eq!(old.session_id(), id);
        assert_eq!(old.source(), &history.inspection().source);
        let stored = read.archived_view(&mapping.instance).unwrap().unwrap();
        let stored = stored.transcript().archived_transcript().unwrap();
        assert_eq!(messages(old.transcript()), messages(&stored));
        assert_eq!(old.transcript().unparsed, stored.unparsed);
    }
}

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
    let mut transcript =
        atman_runtime::projection::flow::FlowTranscript::new(descriptor.address.clone());
    let full = flow
        .read_updates(None, NonZeroUsize::new(10).unwrap())
        .unwrap();
    for batch in full.batches {
        transcript.apply(batch).unwrap();
    }
    let preserved = transcript
        .entries()
        .iter()
        .filter_map(|entry| match &entry.record {
            atman_runtime::projection::flow::FlowRecord::Legacy(record) => {
                Some(record.bytes().unwrap())
            }
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(preserved, raw[..records[0].source.bytes as usize]);
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
    for batch in &page.batches {
        transcript.apply(batch.clone()).unwrap();
    }
    let preserved = transcript
        .entries()
        .iter()
        .filter_map(|entry| match &entry.record {
            atman_runtime::projection::flow::FlowRecord::Legacy(record) => {
                Some(record.bytes().unwrap())
            }
            _ => None,
        })
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(preserved, raw);
    assert_eq!(transcript.cursor(), Some(&complete));
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
