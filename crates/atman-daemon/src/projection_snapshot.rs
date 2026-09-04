use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use atman_proto::{EventCursor, SessionId};
use atman_runtime::event::EventEnvelope;
use atman_runtime::event_writer::EventWriterWatermark;
use serde::{Deserialize, Serialize};

use crate::projection::{RestoredProjection, SessionProjector};

const SNAPSHOT_SCHEMA_VERSION: u32 = 2;
const SNAPSHOT_DIR: &str = ".projection-snapshots";
const SNAPSHOT_PREFIX: &str = "projection-";
const SNAPSHOT_SUFFIX: &str = ".json";
const RETAINED_SNAPSHOTS: usize = 2;
const MAX_SNAPSHOT_BYTES: u64 = 512 * 1024 * 1024;
const BOUNDARY_WINDOW_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
struct EventLogCoverage {
    seq: u64,
    offset: u64,
}

#[derive(Serialize, Deserialize)]
struct ProjectionSnapshotDocument {
    schema_version: u32,
    session_id: SessionId,
    coverage: EventLogCoverage,
    #[serde(default)]
    event_cursor: Option<EventCursor>,
    boundary_digest: String,
    projector_digest: String,
    projector: serde_json::Value,
}

struct DecodedProjectionSnapshot {
    coverage: EventLogCoverage,
    event_cursor: EventCursor,
    projector: SessionProjector,
}

pub(crate) fn save(
    session_id: &SessionId,
    session_dir: &Path,
    watermark: EventWriterWatermark,
    event_cursor: EventCursor,
    projector: &SessionProjector,
    redactor: Option<&atman_runtime::redact::Redactor>,
) -> Result<()> {
    anyhow::ensure!(
        projector.last_runtime_seq() == watermark.seq,
        "projection covers runtime event {}, durable log covers {}",
        projector.last_runtime_seq(),
        watermark.seq
    );
    anyhow::ensure!(
        event_cursor.0 >= projector.projection().revision.0,
        "event cursor {} trails projection revision {}",
        event_cursor.0,
        projector.projection().revision.0
    );
    let events_path = session_dir.join("events.jsonl");
    let boundary_digest = boundary_digest(&events_path, watermark.offset)?;
    let coverage = EventLogCoverage {
        seq: watermark.seq,
        offset: watermark.offset,
    };
    let mut projector_value =
        serde_json::to_value(projector).context("serialize projection snapshot state")?;
    if let Some(redactor) = redactor {
        redactor.redact_json(&mut projector_value);
    }
    let projector_digest = value_digest(&projector_value)?;
    let document = ProjectionSnapshotDocument {
        schema_version: SNAPSHOT_SCHEMA_VERSION,
        session_id: session_id.clone(),
        coverage,
        event_cursor: Some(event_cursor),
        boundary_digest,
        projector_digest,
        projector: projector_value,
    };
    let snapshots_dir = ensure_snapshot_dir(session_dir)?;
    let file_name = format!(
        "{SNAPSHOT_PREFIX}{:020}-{:020}-{}{SNAPSHOT_SUFFIX}",
        coverage.seq,
        coverage.offset,
        uuid::Uuid::now_v7()
    );
    let target = snapshots_dir.join(file_name);
    let temporary = snapshots_dir.join(format!(".writing-{}.tmp", uuid::Uuid::now_v7()));
    let write_result = write_atomic_snapshot(&temporary, &target, &document);
    if write_result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    write_result?;
    prune_old_snapshots(&snapshots_dir)?;
    Ok(())
}

pub(crate) fn load(
    session_id: &SessionId,
    session_dir: &Path,
) -> Result<Option<RestoredProjection>> {
    let events_path = session_dir.join("events.jsonl");
    let snapshots_dir = session_dir.join(SNAPSHOT_DIR);
    let metadata = match fs::symlink_metadata(&snapshots_dir) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => return Ok(None),
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("inspect projection snapshot directory"),
    };
    debug_assert!(metadata.is_dir());

    let mut candidates = snapshot_candidates(&snapshots_dir)?;
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    for (seq, offset, path) in candidates {
        let Ok(document) = load_candidate(&path, session_id, &events_path, seq, offset) else {
            continue;
        };
        let Ok(tail) = read_tail(&events_path, document.coverage) else {
            continue;
        };
        let mut projector = document.projector;
        let mut event_cursor = document.event_cursor;
        for event in tail {
            if projector.apply_envelope(&event).is_some() {
                event_cursor.0 = event_cursor.0.saturating_add(1);
            }
        }
        return Ok(Some(RestoredProjection {
            projector,
            event_cursor,
        }));
    }
    Ok(None)
}

fn ensure_snapshot_dir(session_dir: &Path) -> Result<PathBuf> {
    let path = session_dir.join(SNAPSHOT_DIR);
    match fs::symlink_metadata(&path) {
        Ok(metadata) => anyhow::ensure!(
            metadata.is_dir() && !metadata.file_type().is_symlink(),
            "projection snapshot path is not a real directory: {}",
            path.display()
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&path).context("create projection snapshot directory")?;
        }
        Err(error) => return Err(error).context("inspect projection snapshot directory"),
    }
    Ok(path)
}

fn write_atomic_snapshot<T: Serialize>(temporary: &Path, target: &Path, value: &T) -> Result<()> {
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(temporary)
        .context("create temporary projection snapshot")?;
    serde_json::to_writer(&mut file, value).context("write projection snapshot")?;
    file.write_all(b"\n")
        .context("finish projection snapshot")?;
    file.sync_all().context("sync projection snapshot")?;
    drop(file);
    fs::rename(temporary, target).context("publish projection snapshot")?;
    sync_directory(target.parent().expect("snapshot target has parent"))?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    #[cfg(unix)]
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .context("sync projection snapshot directory")?;
    Ok(())
}

fn prune_old_snapshots(snapshots_dir: &Path) -> Result<()> {
    let mut candidates = snapshot_candidates(snapshots_dir)?;
    candidates.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    for (_, _, path) in candidates.into_iter().skip(RETAINED_SNAPSHOTS) {
        fs::remove_file(path).context("remove obsolete projection snapshot")?;
    }
    sync_directory(snapshots_dir)
}

fn snapshot_candidates(path: &Path) -> Result<Vec<(u64, u64, PathBuf)>> {
    let mut candidates = Vec::new();
    for entry in fs::read_dir(path).context("list projection snapshots")? {
        let entry = entry.context("read projection snapshot entry")?;
        let file_type = entry
            .file_type()
            .context("inspect projection snapshot entry")?;
        if !file_type.is_file() || file_type.is_symlink() {
            continue;
        }
        let Some((seq, offset)) = parse_snapshot_name(&entry.file_name().to_string_lossy()) else {
            continue;
        };
        candidates.push((seq, offset, entry.path()));
    }
    Ok(candidates)
}

fn parse_snapshot_name(name: &str) -> Option<(u64, u64)> {
    let body = name
        .strip_prefix(SNAPSHOT_PREFIX)?
        .strip_suffix(SNAPSHOT_SUFFIX)?;
    let mut parts = body.splitn(3, '-');
    let seq = parts.next()?.parse().ok()?;
    let offset = parts.next()?.parse().ok()?;
    let id = parts.next()?;
    uuid::Uuid::parse_str(id).ok()?;
    Some((seq, offset))
}

fn load_candidate(
    path: &Path,
    expected_session_id: &SessionId,
    events_path: &Path,
    expected_seq: u64,
    expected_offset: u64,
) -> Result<DecodedProjectionSnapshot> {
    let metadata = fs::symlink_metadata(path).context("inspect projection snapshot")?;
    anyhow::ensure!(metadata.is_file() && metadata.len() <= MAX_SNAPSHOT_BYTES);
    let document: ProjectionSnapshotDocument =
        serde_json::from_reader(BufReader::new(File::open(path)?))
            .context("decode projection snapshot")?;
    anyhow::ensure!(matches!(
        document.schema_version,
        1 | SNAPSHOT_SCHEMA_VERSION
    ));
    anyhow::ensure!(&document.session_id == expected_session_id);
    anyhow::ensure!(
        document.coverage
            == (EventLogCoverage {
                seq: expected_seq,
                offset: expected_offset,
            })
    );
    anyhow::ensure!(document.projector_digest == value_digest(&document.projector)?);
    let projector: SessionProjector =
        serde_json::from_value(document.projector).context("decode projection snapshot state")?;
    let event_cursor = document
        .event_cursor
        .unwrap_or(EventCursor(projector.projection().revision.0));
    anyhow::ensure!(event_cursor.0 >= projector.projection().revision.0);
    anyhow::ensure!(projector.last_runtime_seq() == document.coverage.seq);
    anyhow::ensure!(
        projector.projection().metadata.id == *expected_session_id,
        "projection snapshot session identity mismatch"
    );
    anyhow::ensure!(
        document.boundary_digest == boundary_digest(events_path, document.coverage.offset)?,
        "projection snapshot event boundary changed"
    );
    Ok(DecodedProjectionSnapshot {
        coverage: document.coverage,
        event_cursor,
        projector,
    })
}

fn value_digest(value: &serde_json::Value) -> Result<String> {
    let bytes = serde_json::to_vec(value).context("encode projection snapshot digest")?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn boundary_digest(events_path: &Path, offset: u64) -> Result<String> {
    let mut file = File::open(events_path).context("open session event log")?;
    let length = file.metadata()?.len();
    anyhow::ensure!(
        offset <= length,
        "projection snapshot is ahead of the event log"
    );
    if offset == 0 {
        return Ok(blake3::hash(&[]).to_hex().to_string());
    }
    file.seek(SeekFrom::Start(offset - 1))?;
    let mut newline = [0_u8; 1];
    file.read_exact(&mut newline)?;
    anyhow::ensure!(
        newline[0] == b'\n',
        "projection snapshot offset is not an event boundary"
    );
    let start = offset.saturating_sub(BOUNDARY_WINDOW_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0; (offset - start) as usize];
    file.read_exact(&mut bytes)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn read_tail(events_path: &Path, coverage: EventLogCoverage) -> Result<Vec<EventEnvelope>> {
    let mut file = File::open(events_path).context("open session event log tail")?;
    anyhow::ensure!(coverage.offset <= file.metadata()?.len());
    file.seek(SeekFrom::Start(coverage.offset))?;
    let mut tail = Vec::new();
    for line in BufReader::new(file).lines() {
        let line = line?;
        let text = line.trim();
        if text.is_empty() {
            continue;
        }
        let Ok(envelope) = serde_json::from_str::<EventEnvelope>(text) else {
            continue;
        };
        anyhow::ensure!(
            envelope.seq > coverage.seq,
            "event log tail overlaps projection snapshot"
        );
        tail.push(envelope);
    }
    Ok(tail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::event::{Event, FlowRunId, FlowStatus, TurnId};
    use atman_runtime::message::Message;

    fn append_event(path: &Path, seq: u64, event: Event) -> u64 {
        append_envelope(path, &EventEnvelope::new(seq, event))
    }

    fn append_envelope(path: &Path, envelope: &EventEnvelope) -> u64 {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        serde_json::to_writer(&mut file, envelope).unwrap();
        file.write_all(b"\n").unwrap();
        file.sync_all().unwrap();
        file.metadata().unwrap().len()
    }

    #[test]
    fn newest_corrupt_snapshot_falls_back_and_replays_only_the_tail() {
        let session_dir = tempfile::tempdir().unwrap();
        let session_id = SessionId(uuid::Uuid::now_v7());
        let events_path = session_dir.path().join("events.jsonl");
        let first = EventEnvelope::new(
            1,
            Event::TurnStart {
                turn_id: TurnId::now(),
            },
        );
        let offset = append_event(&events_path, first.seq, first.event.clone());
        let projector = SessionProjector::from_events(session_id.clone(), None, &[first]);
        save(
            &session_id,
            session_dir.path(),
            EventWriterWatermark { seq: 1, offset },
            EventCursor(projector.projection().revision.0),
            &projector,
            None,
        )
        .unwrap();
        append_event(
            &events_path,
            2,
            Event::TurnEnd {
                turn_id: TurnId::now(),
            },
        );

        let snapshots_dir = session_dir.path().join(SNAPSHOT_DIR);
        let corrupt = snapshots_dir.join(format!(
            "{SNAPSHOT_PREFIX}{:020}-{:020}-{}{SNAPSHOT_SUFFIX}",
            2,
            offset + 1,
            uuid::Uuid::now_v7()
        ));
        fs::write(corrupt, b"not-json").unwrap();

        let loaded = load(&session_id, session_dir.path()).unwrap().unwrap();
        assert_eq!(loaded.projector.last_runtime_seq(), 2);
    }

    #[test]
    fn changed_covered_prefix_invalidates_snapshot() {
        let session_dir = tempfile::tempdir().unwrap();
        let session_id = SessionId(uuid::Uuid::now_v7());
        let events_path = session_dir.path().join("events.jsonl");
        let event = EventEnvelope::new(
            1,
            Event::TurnStart {
                turn_id: TurnId::now(),
            },
        );
        let offset = append_event(&events_path, event.seq, event.event.clone());
        let projector = SessionProjector::from_events(session_id.clone(), None, &[event]);
        save(
            &session_id,
            session_dir.path(),
            EventWriterWatermark { seq: 1, offset },
            EventCursor(projector.projection().revision.0),
            &projector,
            None,
        )
        .unwrap();

        let mut bytes = fs::read(&events_path).unwrap();
        let changed = bytes.iter().position(|byte| *byte == b'1').unwrap();
        bytes[changed] = b'2';
        fs::write(&events_path, bytes).unwrap();

        assert!(load(&session_id, session_dir.path()).unwrap().is_none());
    }

    #[test]
    fn pruning_keeps_two_newest_atomic_generations() {
        let session_dir = tempfile::tempdir().unwrap();
        let session_id = SessionId(uuid::Uuid::now_v7());
        let events_path = session_dir.path().join("events.jsonl");
        let mut projector = SessionProjector::new(session_id.clone(), None);
        for seq in 1..=3 {
            let event = EventEnvelope::new(
                seq,
                Event::TurnStart {
                    turn_id: TurnId::now(),
                },
            );
            let offset = append_event(&events_path, seq, event.event.clone());
            projector.apply_envelope(&event);
            save(
                &session_id,
                session_dir.path(),
                EventWriterWatermark { seq, offset },
                EventCursor(projector.projection().revision.0),
                &projector,
                None,
            )
            .unwrap();
        }

        assert_eq!(
            snapshot_candidates(&session_dir.path().join(SNAPSHOT_DIR))
                .unwrap()
                .len(),
            2
        );
        assert!(
            !session_dir
                .path()
                .join(SNAPSHOT_DIR)
                .read_dir()
                .unwrap()
                .any(|entry| entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".writing-"))
        );
    }

    #[test]
    fn persisted_projection_is_redacted_without_changing_coverage_identity() {
        let session_dir = tempfile::tempdir().unwrap();
        let session_id = SessionId(uuid::Uuid::now_v7());
        let events_path = session_dir.path().join("events.jsonl");
        let turn_id = TurnId::now();
        let secret = "sk-abcdefghijklmnop1234567890";
        let event = EventEnvelope::new(
            1,
            Event::UserMsg {
                turn_id: turn_id.clone(),
                flow_run_id: None,
                message: Message::user_text(turn_id, format!("token={secret}")),
            },
        );
        let offset = append_event(&events_path, event.seq, event.event.clone());
        let projector = SessionProjector::from_events(session_id.clone(), None, &[event]);
        save(
            &session_id,
            session_dir.path(),
            EventWriterWatermark { seq: 1, offset },
            EventCursor(projector.projection().revision.0),
            &projector,
            Some(&atman_runtime::redact::Redactor::builtin()),
        )
        .unwrap();

        let (_, _, snapshot_path) = snapshot_candidates(&session_dir.path().join(SNAPSHOT_DIR))
            .unwrap()
            .pop()
            .unwrap();
        let persisted = fs::read_to_string(snapshot_path).unwrap();
        assert!(!persisted.contains(secret));
        assert!(persisted.contains("REDACTED"));
        assert_eq!(
            load(&session_id, session_dir.path())
                .unwrap()
                .unwrap()
                .projector
                .last_runtime_seq(),
            1
        );
    }

    #[test]
    fn legacy_snapshot_defaults_event_cursor_to_projection_revision() {
        let session_dir = tempfile::tempdir().unwrap();
        let session_id = SessionId(uuid::Uuid::now_v7());
        let events_path = session_dir.path().join("events.jsonl");
        let event = EventEnvelope::new(
            1,
            Event::TurnStart {
                turn_id: TurnId::now(),
            },
        );
        let offset = append_envelope(&events_path, &event);
        let projector = SessionProjector::from_events(session_id.clone(), None, &[event]);
        save(
            &session_id,
            session_dir.path(),
            EventWriterWatermark { seq: 1, offset },
            EventCursor(projector.projection().revision.0),
            &projector,
            None,
        )
        .unwrap();
        let (_, _, snapshot_path) = snapshot_candidates(&session_dir.path().join(SNAPSHOT_DIR))
            .unwrap()
            .pop()
            .unwrap();
        let mut document: serde_json::Value =
            serde_json::from_slice(&fs::read(&snapshot_path).unwrap()).unwrap();
        document["schema_version"] = serde_json::json!(1);
        document.as_object_mut().unwrap().remove("event_cursor");
        fs::write(&snapshot_path, serde_json::to_vec(&document).unwrap()).unwrap();

        let loaded = load(&session_id, session_dir.path()).unwrap().unwrap();
        assert_eq!(
            loaded.event_cursor,
            EventCursor(loaded.projector.projection().revision.0)
        );
    }

    #[test]
    fn snapshot_plus_tail_matches_a_full_projection_replay() {
        let session_dir = tempfile::tempdir().unwrap();
        let session_id = SessionId(uuid::Uuid::now_v7());
        let events_path = session_dir.path().join("events.jsonl");
        let turn_id = TurnId::now();
        let run_id = FlowRunId::now();
        let events = vec![
            EventEnvelope::new(
                1,
                Event::TurnStart {
                    turn_id: turn_id.clone(),
                },
            ),
            EventEnvelope::new(
                2,
                Event::FlowStart {
                    turn_id: None,
                    run_id: run_id.clone(),
                    flow_name: "agent".into(),
                    parent_run_id: None,
                    parent_node_id: None,
                    spawned: false,
                },
            ),
            EventEnvelope::new(
                3,
                Event::UserMsg {
                    turn_id: turn_id.clone(),
                    flow_run_id: Some(run_id.clone()),
                    message: Message::user_text(turn_id.clone(), "inspect"),
                },
            ),
            EventEnvelope::new(
                4,
                Event::FlowEnd {
                    run_id,
                    flow_name: "agent".into(),
                    status: FlowStatus::Ok,
                },
            ),
            EventEnvelope::new(5, Event::TurnEnd { turn_id }),
        ];
        let mut offset = 0;
        for event in &events[..2] {
            offset = append_envelope(&events_path, event);
        }
        let projector = SessionProjector::from_events(session_id.clone(), None, &events[..2]);
        let snapshot_revision = projector.projection().revision.0;
        save(
            &session_id,
            session_dir.path(),
            EventWriterWatermark { seq: 2, offset },
            EventCursor(20),
            &projector,
            None,
        )
        .unwrap();
        for event in &events[2..] {
            append_envelope(&events_path, event);
        }

        let restored = load(&session_id, session_dir.path()).unwrap().unwrap();
        let rebuilt = SessionProjector::from_events(session_id, None, &events);
        assert_eq!(
            restored.event_cursor,
            EventCursor(20 + rebuilt.projection().revision.0 - snapshot_revision)
        );
        assert_eq!(
            restored.projector.last_runtime_seq(),
            rebuilt.last_runtime_seq()
        );
        assert_eq!(restored.projector.projection(), rebuilt.projection());
    }
}
