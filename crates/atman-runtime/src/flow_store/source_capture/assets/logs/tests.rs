use super::*;
use crate::flow_store::{
    completion::InvocationResult,
    source_capture::{
        ForkBoundaryRequest, ForkCutoff, ForkReadBudget, SourceSelection,
        tests::{fixture, publish},
    },
};

fn history() -> (
    tempfile::TempDir,
    FlowRef,
    PathBuf,
    Vec<ConversationHistoryRecord>,
) {
    let (directory, mut flow, lease) = fixture();
    let flow_dir = flow.dir().unwrap().to_owned();
    let handle = format!("bg_{}_7", lease.flow.session_id);
    let path = flow_dir.join(format!("bg_{handle}.log"));
    std::fs::write(&path, b"original log\n").unwrap();
    for _ in 0..2 {
        publish(
            &mut flow,
            &lease,
            Message {
                parts: vec![MessagePart::ToolUse {
                    id: "same-call".into(),
                    name: "bash.status".into(),
                    input: serde_json::json!({"handle": handle}),
                    intent: None,
                }],
                ..Message::assistant_text(lease.turn.clone(), "")
            },
        );
        let content = serde_json::json!({"handle": handle, "log_path": path, "status": "running"})
            .to_string();
        let (id, mut message) =
            super::super::tests::output_message(&OutputStore::at(&flow_dir), &content);
        assert!(!id.is_empty());
        let MessagePart::ToolResult { tool_use_id, .. } = &mut message.parts[0] else {
            panic!("result")
        };
        *tool_use_id = "same-call".into();
        message.role = MessageRole::Tool;
        message.turn_id = lease.turn.clone();
        publish(&mut flow, &lease, message);
    }
    flow.finish_turn(
        lease.clone(),
        InvocationResult::Unobserved(atman_rt::TurnOutcome::Completed),
    )
    .unwrap();
    let request = ForkBoundaryRequest {
        observed: SourceSelection {
            flow: lease.flow.clone(),
            branch: flow.history_head().branch,
            cursor: flow.cursor().require_durable().unwrap().clone(),
        },
        expected_head: flow.history_head(),
        cutoff: ForkCutoff::LatestSafe,
    };
    let snapshot = flow
        .capture_fork_conversation(&request, ForkReadBudget::default(), |_| Ok(()))
        .unwrap()
        .prepare()
        .unwrap();
    (
        directory,
        lease.flow,
        flow_dir,
        snapshot.transcript.into_history_records().unwrap(),
    )
}

fn assets(
    records: &[ConversationHistoryRecord],
    source: &Path,
    staging: &Path,
) -> RetainedSourceAssets {
    RetainedSourceAssets::capture(
        records
            .iter()
            .flat_map(|entry| entry.record.shared_messages()),
        &OutputStore::at(source),
        staging,
        8192,
    )
    .unwrap()
}

#[test]
fn paired_native_log_capture_survives_source_deletion_without_process_ownership() {
    let (source, flow, path, records) = history();
    let staging = tempfile::tempdir().unwrap();
    let assets = assets(&records, &path, staging.path())
        .retain_bash_logs(&flow, &path, &records, 8192)
        .unwrap();
    assert_eq!(assets.logs().len(), 1);
    let log = &assets.logs()[0];
    assert_eq!(log.bytes, 13);
    assert_eq!(
        log.digest,
        blake3::hash(b"original log\n").to_hex().as_str()
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(&log.source_path)
        .unwrap()
        .write_all(b"later output\n")
        .unwrap();
    drop(source);
    assert_eq!(
        std::fs::read(assets.directory().join(&log.filename)).unwrap(),
        b"original log\n"
    );
    assert!(assets.bytes() > log.bytes);
    assert_eq!(
        serde_json::from_str::<RetainedBashLog>(&serde_json::to_string(log).unwrap()).unwrap(),
        *log
    );
}

#[test]
fn process_log_authority_requires_original_owner_pair_scope_and_managed_path() {
    let (_source, flow, path, records) = history();
    let staging = tempfile::tempdir().unwrap();
    let assets = assets(&records, &path, staging.path());
    let expected = log_references(&flow, &path, &records, assets.directory()).unwrap();
    assert_eq!(expected.len(), 1);
    assert!(log_references(&flow, staging.path(), &records, assets.directory()).is_err());
    let mut foreign = flow.clone();
    foreign.instance_id = crate::flow_store::FlowInstanceId::now();
    assert!(log_references(&foreign, &path, &records, assets.directory()).is_err());
    let mut unpaired = records.clone();
    for entry in &mut unpaired {
        if let FlowRecord::Publication(publication) = &mut entry.record
            && publication.message.role == MessageRole::Assistant
        {
            publication.context_run = Some(atman_rt::RunId::now());
        }
    }
    assert!(
        log_references(&flow, &path, &unpaired, assets.directory())
            .unwrap()
            .is_empty()
    );
    for entry in &mut unpaired {
        if let FlowRecord::Publication(publication) = &mut entry.record {
            publication.context_run = None;
            for part in &mut publication.message.parts {
                if let MessagePart::ToolUse { name, .. } = part {
                    *name = "file.read".into();
                }
            }
        }
    }
    assert!(
        log_references(&flow, &path, &unpaired, assets.directory())
            .unwrap()
            .is_empty()
    );
    let budget = assets.bytes() + 12;
    assert!(
        assets
            .retain_bash_logs(&flow, &path, &records, budget)
            .is_err()
    );
    assert_eq!(
        std::fs::read(expected.first().unwrap()).unwrap(),
        b"original log\n"
    );
}

use std::io::Write;
