use std::path::Path;

use atman_runtime::Event;
use atman_runtime::event::EventEnvelope;
use atman_runtime::event_log::legacy::{self, LegacyOwner, RecordIssue, UnassignedReason};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

fn start(run: Uuid, parent: Option<Uuid>, spawned: bool) -> Value {
    json!({"type":"flow_start", "run_id":run, "flow_name":"worker", "parent_run_id":parent, "spawned":spawned})
}

fn owner(run: Uuid, owner: &str) -> Value {
    json!({"type":"goal_turn_started", "run_id":run, "owner":owner})
}

fn end(run: Uuid) -> Value {
    json!({"type":"flow_end", "run_id":run, "flow_name":"worker", "status":{"kind":"ok"}})
}

fn write_events(path: &Path, events: Vec<Value>) {
    let mut bytes = Vec::new();
    for (i, value) in events.into_iter().enumerate() {
        let event: Event = serde_json::from_value(value).unwrap();
        let mut envelope = EventEnvelope::new(i as u64 + 1, event);
        envelope.ts = "2026-10-16T08:00:00Z".parse().unwrap();
        serde_json::to_writer(&mut bytes, &envelope).unwrap();
        bytes.push(b'\n');
    }
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn legacy_mapping_preserves_independent_agents_and_continuations_without_event_order_guesses() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("events.jsonl");
    let session = Uuid::now_v7();
    let root = Uuid::now_v7();
    let child = Uuid::now_v7();
    let continuation = Uuid::now_v7();
    let inline = Uuid::now_v7();
    let sibling = Uuid::now_v7();
    let turn = atman_rt::TurnId::now();
    // Reverse/interleaved events force mapping from explicit facts, not temporal proximity.
    write_events(
        &path,
        vec![
            end(inline),
            owner(continuation, "child-agent"),
            start(inline, Some(child), false),
            start(continuation, Some(child), true),
            start(child, Some(root), true),
            start(sibling, Some(root), true),
            owner(child, "child-agent"),
            owner(root, &session.to_string()),
            start(root, None, false),
            json!({"type":"goal_changed", "owner":"child-agent", "goal":null}),
            serde_json::to_value(Event::AssistantMsg {
                turn_id: turn.clone(),
                flow_run_id: None,
                message: atman_runtime::message::Message::assistant_text(turn, "private answer"),
            })
            .unwrap(),
            json!({"type":"mermaid_diagram", "source":"private graph"}),
        ],
    );
    let before = std::fs::read(&path).unwrap();
    let report = legacy::inspect(&path, session).unwrap();
    let agent = LegacyOwner::Agent {
        owner: "child-agent".into(),
    };
    for index in [0, 1, 2, 3, 4, 6, 9] {
        assert_eq!(report.records[index].owner, agent, "record {index}");
    }
    assert_eq!(
        report.records[5].owner,
        LegacyOwner::SpawnedRun {
            run_id: sibling.to_string()
        }
    );
    for index in [7, 8, 10] {
        assert_eq!(report.records[index].owner, LegacyOwner::Default);
    }
    assert_eq!(
        report.records[11].owner,
        LegacyOwner::Unassigned {
            reason: UnassignedReason::Unscoped
        }
    );
    assert_eq!(report, legacy::inspect(&path, session).unwrap());
    assert_eq!(before, std::fs::read(&path).unwrap());
    assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 1);
    let printed = serde_json::to_string(&report).unwrap();
    assert!(!printed.contains("private answer"));
    assert!(!printed.contains("private graph"));
}

#[test]
fn every_nonempty_record_retains_original_bytes_and_missing_metadata_stays_missing() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("events.jsonl");
    let session = Uuid::now_v7();
    let bytes = b" \r\n{\"type\":\"mermaid_diagram\",\"source\":\"hello\"}\r\nnot json\n{\"type\":\"future_event\",\"seq\":7,\"ts\":\"2026-10-16T16:00:00+08:00\"}\n{\"type\":\"mermaid_diagram\",\"source\":\"world\",\"seq\":-1,\"ts\":\"invalid\"}\n{\"type\":\"flow_start\"";
    std::fs::write(&path, bytes).unwrap();
    let report = legacy::inspect(&path, session).unwrap();
    assert_eq!(report.blank_lines, 1);
    assert_eq!(report.records.len(), 5);
    assert_eq!(report.source.bytes, bytes.len() as u64);
    assert_eq!(report.source.sha256, format!("{:x}", Sha256::digest(bytes)));
    let mut offset = 3;
    for record in &report.records {
        assert_eq!(record.offset, offset);
        let raw = &bytes[record.offset as usize..(record.offset + record.bytes) as usize];
        assert_eq!(record.sha256, format!("{:x}", Sha256::digest(raw)));
        offset += record.bytes;
    }
    assert_eq!(offset, bytes.len() as u64);
    assert_eq!(report.records[0].timestamp, None);
    assert_eq!(report.records[0].sequence, None);
    assert_eq!(
        report.records[0].issues,
        [RecordIssue::MissingSequence, RecordIssue::MissingTimestamp]
    );
    assert_eq!(report.records[1].issues, [RecordIssue::InvalidJson]);
    assert_eq!(report.records[2].issues, [RecordIssue::UnsupportedEvent]);
    assert_eq!(
        report.records[2].timestamp.unwrap().to_rfc3339(),
        "2026-10-16T08:00:00+00:00"
    );
    assert_eq!(
        report.records[3].issues,
        [RecordIssue::InvalidSequence, RecordIssue::InvalidTimestamp]
    );
    assert_eq!(
        report.records[4].issues,
        [RecordIssue::UnterminatedLine, RecordIssue::InvalidJson]
    );
    assert_eq!(report, legacy::inspect(&path, session).unwrap());
    // Same-length replacement must be detected even without relying on mtime or file size.
    let mut changed = bytes.to_vec();
    changed[0] = b'\t';
    std::fs::write(&path, changed).unwrap();
    assert_eq!(
        report.source.verify().unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    // Appends and truncations also invalidate the bounded snapshot.
    std::fs::write(&path, [bytes.as_slice(), b"\n"].concat()).unwrap();
    assert_eq!(
        report.source.verify().unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
    std::fs::write(&path, &bytes[..bytes.len() - 1]).unwrap();
    assert_eq!(
        report.source.verify().unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[test]
fn contradictory_missing_and_cyclic_lineages_remain_unassigned_including_descendants() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("events.jsonl");
    let session = Uuid::now_v7();
    let root = Uuid::now_v7();
    let conflict = Uuid::now_v7();
    let owner_conflict = Uuid::now_v7();
    let descendant = Uuid::now_v7();
    let cycle_a = Uuid::now_v7();
    let cycle_b = Uuid::now_v7();
    let orphan = Uuid::now_v7();
    let missing = Uuid::now_v7();
    write_events(
        &path,
        vec![
            start(root, None, false),
            start(conflict, Some(root), true),
            start(conflict, Some(root), false),
            start(owner_conflict, Some(root), true),
            owner(owner_conflict, "one"),
            owner(owner_conflict, "two"),
            start(descendant, Some(conflict), false),
            start(cycle_a, Some(cycle_b), true),
            start(cycle_b, Some(cycle_a), true),
            start(orphan, Some(missing), false),
            end(missing),
        ],
    );
    let report = legacy::inspect(&path, session).unwrap();
    for record in &report.records[1..] {
        assert!(
            matches!(record.owner, LegacyOwner::Unassigned { .. }),
            "{record:?}"
        );
    }
    assert_eq!(
        report.records[6].owner,
        LegacyOwner::Unassigned {
            reason: UnassignedReason::ConflictingRun
        }
    );
    assert_eq!(
        report.records[7].owner,
        LegacyOwner::Unassigned {
            reason: UnassignedReason::CyclicLineage
        }
    );
    assert_eq!(
        report.records[10].owner,
        LegacyOwner::Unassigned {
            reason: UnassignedReason::UnknownRun
        }
    );
}

#[test]
fn deep_inline_history_resolves_without_recursive_stack_growth() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("events.jsonl");
    let depth = 10_000;
    let events = (1..=depth)
        .map(|id| {
            start(
                Uuid::from_u128(id),
                (id < depth).then(|| Uuid::from_u128(id + 1)),
                false,
            )
        })
        .collect();
    write_events(&path, events);
    let report = legacy::inspect(&path, Uuid::now_v7()).unwrap();
    assert_eq!(report.runs.len(), depth as usize);
    assert!(
        report
            .runs
            .iter()
            .all(|run| run.owner == LegacyOwner::Default)
    );
}
