//! Read-only discovery and explicit recovery of conversations retained only in project indexes.

use std::{
    collections::BTreeMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{
    config_hub::ConfigHub,
    event::Event,
    flow_store::{RuntimeStore, SessionConfiguration, SessionDescriptor, migration},
    index::AnchorIndex,
    message::{ImageData, MessagePart},
    project_catalog::ProjectCatalogStore,
    storage::ProjectContext,
};

mod discovery;
pub use discovery::DiscoveryCache;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedSource {
    pub index: PathBuf,
    pub session: Uuid,
}

#[derive(Clone, Debug)]
pub struct IndexedSession {
    pub source: IndexedSource,
    pub project: Option<PathBuf>,
    pub user_messages: u64,
    pub updated_at: String,
    pub snippet: String,
}

impl IndexedSession {
    pub fn title(&self) -> String {
        format!(
            "Indexed history · {}",
            if self.snippet.is_empty() {
                self.source.session.to_string()[..8].to_owned()
            } else {
                self.snippet.clone()
            }
        )
    }
}

#[derive(Clone, Debug)]
pub struct IndexReview {
    pub source: IndexedSource,
    pub records: u64,
    pub gaps: u64,
    pub missing_images: u64,
    pub missing_outputs: u64,
    pub digest: String,
    pub preview: Vec<String>,
}

fn directories(data: &Path) -> Result<BTreeMap<PathBuf, Option<PathBuf>>> {
    let mut directories = BTreeMap::new();
    let projects = ProjectCatalogStore::new(data).load()?.projects;
    let global = data.join("projects");
    match fs::read_dir(&global) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry?;
                if entry.file_type()?.is_dir() && entry.path().join("index.db").is_file() {
                    let project = projects
                        .iter()
                        .find(|project| entry.file_name().to_str() == Some(&project.fingerprint))
                        .map(|project| project.root.clone());
                    directories.insert(entry.path().canonicalize()?, project);
                }
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    for project in projects {
        let local = project.root.join(".atman");
        if local.join("index.db").is_file() {
            directories.insert(local.canonicalize()?, Some(project.root));
        }
    }
    Ok(directories)
}

fn original_exists(data: &Path, session: Uuid) -> Result<bool> {
    Ok(data
        .join("sessions")
        .join(session.to_string())
        .join("events.jsonl")
        .try_exists()?)
}

/// List surviving conversations without importing records, rebuilding indexes, or creating sessions.
pub fn discover(data: &Path) -> Result<Vec<IndexedSession>> {
    DiscoveryCache::default().discover(data)
}

fn discover_directory(directory: &Path, project: Option<PathBuf>) -> Result<Vec<IndexedSession>> {
    let mut sessions = Vec::new();
    let Some(index) = AnchorIndex::open_project_read_only(directory)? else {
        return Ok(sessions);
    };
    let connection = index.conn();
    if !connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'events')",
        [],
        |row| row.get::<_, bool>(0),
    )? {
        return Ok(sessions);
    }
    let mut statement = connection.prepare("SELECT session_id, COUNT(*), MAX(ts), MIN(seq) FROM events WHERE kind = 'user_msg' GROUP BY session_id")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, u64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, u64>(3)?,
        ))
    })?;
    for row in rows {
        let (id, user_messages, updated_at, first) = row?;
        let Ok(session) = Uuid::parse_str(&id) else {
            continue;
        };
        if session.to_string() != id {
            continue;
        }
        let payload: String = connection.query_row(
            "SELECT payload FROM events WHERE session_id = ? AND seq = ?",
            rusqlite::params![id, first],
            |row| row.get(0),
        )?;
        sessions.push(IndexedSession {
            source: IndexedSource {
                index: index.path().to_owned(),
                session,
            },
            project: project.clone(),
            user_messages,
            updated_at,
            snippet: crate::history_search::extract_event_text("user_msg", &payload)
                .unwrap_or_default()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .chars()
                .take(72)
                .collect(),
        });
    }
    Ok(sessions)
}

fn open(data: &Path, source: &IndexedSource) -> Result<(AnchorIndex, Option<PathBuf>)> {
    ensure!(
        !original_exists(data, source.session)?,
        "Original history is available; open its existing session instead"
    );
    let parent = source
        .index
        .parent()
        .context("Index source has no directory")?;
    let project = directories(data)?
        .remove(parent)
        .context("Index source is not in this history directory")?;
    ensure!(
        source.index == parent.join("index.db"),
        "Invalid index source"
    );
    let index =
        AnchorIndex::open_project_read_only(parent)?.context("History index is unavailable")?;
    Ok((index, project))
}

/// Inspect one SQLite snapshot. The review digest binds every retained row and its original payload.
pub fn review(data: &Path, source: &IndexedSource) -> Result<IndexReview> {
    let (index, _) = open(data, source)?;
    snapshot(&index, source, &mut std::io::sink())
}

fn snapshot(
    index: &AnchorIndex,
    source: &IndexedSource,
    output: &mut impl Write,
) -> Result<IndexReview> {
    let connection = index.conn();
    let transaction = connection.unchecked_transaction()?;
    let mut statement = transaction
        .prepare("SELECT seq, ts, kind, payload FROM events WHERE session_id = ? ORDER BY seq")?;
    let mut rows = statement.query([source.session.to_string()])?;
    let mut result = IndexReview {
        source: source.clone(),
        records: 0,
        gaps: 0,
        missing_images: 0,
        missing_outputs: 0,
        digest: String::new(),
        preview: Vec::new(),
    };
    let mut hash = Sha256::new();
    let mut previous: Option<u64> = None;
    while let Some(row) = rows.next()? {
        let seq: u64 = row.get(0)?;
        let ts: String = row.get(1)?;
        let kind: String = row.get(2)?;
        let payload: String = row.get(3)?;
        let original = serde_json::to_vec(&(seq, &ts, &kind, &payload))?;
        hash.update((original.len() as u64).to_le_bytes());
        hash.update(original);
        result.records += 1;
        if let Some(previous) = previous {
            result.gaps += seq.saturating_sub(previous.saturating_add(1));
        }
        previous = Some(seq);
        if let Some(text) = crate::history_search::extract_event_text(&kind, &payload) {
            result.preview.push(format!(
                "{kind}: {}",
                text.chars().take(240).collect::<String>()
            ));
            if result.preview.len() > 3 {
                result.preview.remove(0);
            }
        }
        let mut value = if let Ok(mut event) = serde_json::from_str::<Event>(&payload) {
            normalize_assets(&mut event, &mut result);
            serde_json::to_value(event)?
        } else {
            serde_json::from_str::<serde_json::Value>(&payload)
                .ok()
                .filter(|value| value.is_object())
                .unwrap_or_else(|| serde_json::json!({"type": "unparsed_index_record"}))
        };
        value["seq"] = seq.into();
        value["ts"] = ts.into();
        value["indexed_from"] = serde_json::json!({"index": source.index, "session_id": source.session, "kind": kind, "payload": payload});
        serde_json::to_writer(&mut *output, &value)?;
        output.write_all(b"\n")?;
    }
    ensure!(
        result.records > 0,
        "Indexed conversation is empty or no longer available"
    );
    result.digest = format!("{:x}", hash.finalize());
    Ok(result)
}

fn normalize_assets(event: &mut Event, review: &mut IndexReview) {
    let messages = match event {
        Event::UserMsg { message, .. }
        | Event::AssistantMsg { message, .. }
        | Event::ToolResultMsg { message, .. }
        | Event::SystemMsg { message, .. }
        | Event::DeferredFormApplied { message, .. } => std::slice::from_mut(message),
        Event::Checkpoint { messages, .. } => messages.as_mut_slice(),
        _ => return,
    };
    for message in messages {
        for part in &mut message.parts {
            match part {
                MessagePart::Image { source }
                    if !matches!(source.data, ImageData::Base64 { .. }) =>
                {
                    review.missing_images += 1;
                    *part = MessagePart::Text {
                        text: "[Image bytes are unavailable in this indexed history.]".into(),
                    };
                }
                MessagePart::ToolResult { content, .. }
                    if crate::tools::tool_output::output_reference(content).is_some() =>
                {
                    review.missing_outputs += 1;
                    // Preserve the preview while making its missing output reference non-executable.
                    content
                        .push_str("\n\nFull tool output unavailable in recovered index history.");
                }
                _ => {}
            }
        }
    }
}

/// Retain the reviewed index snapshot as an independent read-only archive. Original indexes and logs stay untouched.
pub fn recover(data: &Path, expected: &IndexReview, configuration: &ConfigHub) -> Result<Uuid> {
    let (index, project) = open(data, &expected.source)?;
    let key = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&expected.source)?)
    );
    let directory = data.join("runtime-v1/.index-recovery").join(key);
    fs::create_dir_all(&directory)?;
    let _lock = crate::file_lock::FileLock::exclusive(
        crate::event_log::journal::private_file_options()
            .create(true)
            .truncate(false)
            .open(directory.join(".lock"))?,
    )?;
    let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
    let current = snapshot(&index, &expected.source, temporary.as_file_mut())?;
    ensure!(
        current.digest == expected.digest,
        "Indexed history changed; review it again before recovery"
    );
    temporary.as_file().sync_all()?;
    let source = directory.join("events.jsonl");
    let inspected = crate::event_log::legacy::inspect(temporary.path(), expected.source.session)?;
    match fs::hard_link(temporary.path(), &source) {
        Ok(()) => crate::event_log::journal::sync_parent(&source)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            crate::event_log::legacy::LegacySource {
                path: source.clone(),
                ..inspected.source
            }
            .verify()
            .context("A different index snapshot is already retained; it was not overwritten")?;
        }
        Err(error) => return Err(error.into()),
    }
    let source_id = migration::destination_id(&source, expected.source.session)?;
    let destination = migration::destination_id(&source, source_id)?;
    let descriptor = SessionDescriptor {
        id: destination,
        title: format!(
            "Indexed history · {} · partial",
            &expected.source.session.to_string()[..8]
        ),
        project: project
            .as_deref()
            .map(|project| ProjectContext::resolve(configuration, project, data))
            .transpose()?,
        configuration: SessionConfiguration::Atman(configuration.source()),
    };
    RuntimeStore::open(data)?.archive_legacy(&source, source_id, descriptor, true)?;
    fs::remove_file(&source)?;
    crate::event_log::journal::sync_parent(&source)?;
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{index::ProjectEventInsert, message::Message};

    #[test]
    fn index_recovery_preserves_source_and_requires_review_without_fabricating_originals() {
        let data = tempfile::tempdir().unwrap();
        let session = Uuid::now_v7();
        let directory = data.path().join("projects/retained");
        let index = AnchorIndex::open_project(&directory).unwrap();
        let turn = atman_rt::TurnId::now();
        let mut message = Message::user_text(turn.clone(), "retained question");
        message.parts.push(MessagePart::Image {
            source: crate::message::ImageSource {
                media_type: "image/png".into(),
                detail: Default::default(),
                data: ImageData::Path {
                    path: data.path().join("missing.png"),
                },
            },
        });
        let output_id = "out_0123456789abcdef0123456789abcdef";
        for content in [
            format!("retained preview\n\n[Output truncated: total_bytes=1000, output_id={output_id}.]"),
            serde_json::json!({"content": "retained preview", "output_id": output_id, "total_bytes": 1000, "next": {"mode": "bytes", "offset": 20, "has_more": true}}).to_string(),
        ] {
            assert!(crate::tools::tool_output::output_reference(&content).is_some());
            message.parts.push(MessagePart::ToolResult {
                tool_use_id: "retained-output".into(),
                content,
                is_error: false,
            });
        }
        let event = Event::UserMsg {
            turn_id: turn.clone(),
            flow_run_id: None,
            message,
            presentation: None,
            injection_id: None,
        };
        let payload = serde_json::to_string(&event).unwrap();
        for seq in [1, 3] {
            index
                .insert_project_event_raw(ProjectEventInsert {
                    session_id: &session.to_string(),
                    seq,
                    ts: "2026-10-01T00:00:00Z",
                    kind: "user_msg",
                    turn_id: None,
                    flow_run_id: None,
                    text_content: "retained question",
                    payload_json: &payload,
                })
                .unwrap();
        }
        drop(index);
        let damaged = data.path().join("projects/damaged");
        fs::create_dir_all(&damaged).unwrap();
        fs::write(damaged.join("index.db"), b"not a database").unwrap();
        let original = fs::read(directory.join("index.db")).unwrap();
        let found = discover(data.path()).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].source.session, session);
        let reviewed = review(data.path(), &found[0].source).unwrap();
        assert_eq!(
            (reviewed.records, reviewed.gaps, reviewed.missing_images),
            (2, 1, 2)
        );
        assert_eq!(reviewed.missing_outputs, 4);
        assert!(!data.path().join("runtime-v1").exists());
        let hub = ConfigHub::from_config_dir(data.path().join("config"));
        let mut stale = reviewed.clone();
        stale.digest = "stale".into();
        assert!(recover(data.path(), &stale, &hub).is_err());
        assert!(RuntimeStore::session_ids(data.path()).unwrap().is_empty());
        let id = recover(data.path(), &reviewed, &hub).unwrap();
        assert_ne!(id, session);
        assert_eq!(recover(data.path(), &reviewed, &hub).unwrap(), id);
        let read = RuntimeStore::inspect_session(data.path(), id).unwrap();
        let view = read
            .archived_view(read.projection.default_flow.as_ref().unwrap())
            .unwrap()
            .unwrap();
        let archive = view.transcript().archived_transcript().unwrap();
        assert_eq!(archive.context.len(), 2);
        for part in archive.context.iter().flat_map(|message| &message.parts) {
            if let MessagePart::ToolResult { content, .. } = part {
                assert!(content.contains("retained preview"));
                assert!(crate::tools::tool_output::output_reference(content).is_none());
            }
        }
        assert!(archive.context.iter().all(|message| {
            message
                .text_concat()
                .contains("Image bytes are unavailable")
        }));
        let retained = fs::read_to_string(read.dir.join("artifacts/legacy-events.jsonl")).unwrap();
        for line in retained.lines() {
            let raw: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(raw["indexed_from"]["payload"], payload);
            assert_eq!(raw["indexed_from"]["session_id"], session.to_string());
        }
        assert_eq!(fs::read(directory.join("index.db")).unwrap(), original);
        assert!(!data.path().join("sessions").exists());
        let mut foreign = reviewed.source.clone();
        foreign.index = data.path().join("unregistered/index.db");
        assert!(review(data.path(), &foreign).is_err());
    }
}
