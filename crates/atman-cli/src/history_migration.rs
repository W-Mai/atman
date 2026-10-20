//! Operator-driven legacy history import without starting execution services.

use anyhow::{Context, Result, ensure};
use atman_runtime::{
    config_hub::ConfigHub,
    flow_store::{RuntimeStore, SessionConfiguration, SessionDescriptor, migration},
    session_meta::SessionMeta,
    storage::ProjectContext,
};
use serde::Serialize;
use uuid::Uuid;

/// Publish the source adapter's text snapshot through the immutable history transaction.
pub(crate) fn import_messages(
    root: &std::path::Path,
    source_tag: &str,
    source_session: &str,
    messages: &[crate::migrate_source::ImportedMessage],
) -> Result<Uuid> {
    use crate::migrate_source::MessageRole;
    use atman_runtime::message::{Message, MessageOrigin, MessagePart};
    use std::io::Write;

    ensure!(!messages.is_empty(), "cannot import an empty transcript");
    // This private snapshot has no executor. The archive retains its full bytes
    // before publication, independently of this temporary file's lifetime.
    let mut snapshot = tempfile::NamedTempFile::new()?;
    let source_id = Uuid::now_v7();
    let id = Uuid::now_v7();
    let mut turn = atman_rt::TurnId::<atman_runtime::event::AtmanUuid>::now();
    for (index, imported) in messages.iter().enumerate() {
        if imported.role == MessageRole::User {
            turn = atman_rt::TurnId::now();
        }
        let message = match imported.role {
            MessageRole::User => Message::user_text(turn.clone(), imported.text.clone()),
            MessageRole::Assistant => Message::assistant_text(turn.clone(), imported.text.clone()),
            MessageRole::System => Message::system_text(turn.clone(), imported.text.clone()),
            MessageRole::Tool => Message {
                role: atman_runtime::message::MessageRole::Tool,
                parts: vec![MessagePart::Text {
                    text: imported.text.clone(),
                }],
                turn_id: turn.clone(),
                origin: MessageOrigin::User,
            },
        };
        let kind = match imported.role {
            MessageRole::User => "user_msg",
            MessageRole::Assistant => "assistant_msg",
            MessageRole::System => "system_msg",
            MessageRole::Tool => "tool_result_msg",
        };
        let mut record = serde_json::json!({
            "seq": index + 1, "type": kind, "turn_id": turn, "message": message,
            "imported_from": {
                "source": source_tag, "session_id": source_session, "agent": imported.agent,
                "model": imported.model, "created_ms": imported.created_ms,
            },
        });
        if imported.created_ms != 0 {
            record["ts"] = serde_json::to_value(
                chrono::DateTime::from_timestamp_millis(imported.created_ms)
                    .context("source message timestamp is outside the supported range")?,
            )?;
        }
        serde_json::to_writer(snapshot.as_file_mut(), &record)?;
        snapshot.write_all(b"\n")?;
    }
    snapshot.as_file().sync_all()?;
    RuntimeStore::open(root)?.archive_legacy(
        snapshot.path(),
        source_id,
        SessionDescriptor {
            id,
            project: None,
            configuration: SessionConfiguration::HostProvided,
            title: format!("{source_tag} · {source_session}"),
        },
        true,
    )?;
    Ok(id)
}

#[derive(Serialize)]
pub(crate) struct ArchiveReport {
    session_id: Uuid,
    migrated_from: migration::LegacyMigration,
}

pub(crate) fn archive(
    session: &str,
    into: Option<Uuid>,
    confirm_stopped: bool,
) -> Result<ArchiveReport> {
    ensure!(confirm_stopped, "--confirm-stopped is required");
    let root = crate::data_dir()?;
    let session = crate::resolve_session_prefix(&root, session)?;
    let source_session = Uuid::parse_str(&session).context("legacy session ID must be a UUID")?;
    let source_dir = root.join("sessions").join(&session);
    let events = source_dir.join("events.jsonl");
    // Missing metadata is explicit; malformed metadata must not silently lose the project binding.
    let meta: SessionMeta = match std::fs::read(source_dir.join("meta.json")) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("read legacy session metadata")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => SessionMeta::default(),
        Err(error) => return Err(error.into()),
    };
    let hub = ConfigHub::global()?;
    let project = meta
        .project_root
        .as_ref()
        .map(|path| {
            ensure!(path.is_absolute(), "legacy project root must be absolute");
            ProjectContext::resolve(&hub, path, &root)
        })
        .transpose()?;
    let id = match into {
        Some(id) => id,
        None => migration::destination_id(&events, source_session)?,
    };
    let descriptor = SessionDescriptor {
        id,
        project,
        configuration: SessionConfiguration::Atman(hub.source()),
        title: meta
            .title
            .unwrap_or_else(|| format!("Legacy session {}", &session[..8])),
    };
    let store = RuntimeStore::open(&root)?;
    let migrated_from = store
        .archive_legacy(&events, source_session, descriptor, confirm_stopped)
        .with_context(|| format!("archive legacy session {session}"))?;
    Ok(ArchiveReport {
        session_id: id,
        migrated_from,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate_source::{ImportedMessage, MessageRole};

    #[test]
    fn external_history_preserves_roles_turns_and_unknown_times_without_execution() {
        let data = tempfile::tempdir().unwrap();
        let roles = [
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::System,
            MessageRole::User,
        ];
        let messages: Vec<_> = roles
            .into_iter()
            .enumerate()
            .map(|(i, role)| ImportedMessage {
                role,
                text: format!("literal body {i}"),
                agent: Some("external-agent".into()),
                model: Some("vendor/model".into()),
                created_ms: 0,
            })
            .collect();
        let id = import_messages(data.path(), "external", "external-session", &messages).unwrap();
        let session = RuntimeStore::inspect_session(data.path(), id).unwrap();
        let instance = session.projection.default_flow.as_ref().unwrap();
        let view = session.archived_view(instance).unwrap().unwrap();
        let archive = view.transcript().archived_transcript().unwrap();
        assert!(archive.unparsed.is_empty());
        let projected: Vec<_> = archive
            .entries
            .iter()
            .filter_map(|entry| match entry {
                atman_runtime::TranscriptEntry::Message { message, .. } => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(projected.len(), messages.len());
        for (actual, expected) in projected.iter().zip(&messages) {
            assert_eq!(
                serde_json::to_value(actual.role).unwrap(),
                expected.role.as_str()
            );
            assert_eq!(actual.text_concat(), expected.text);
        }
        assert_eq!(projected[0].turn_id, projected[3].turn_id);
        assert_ne!(projected[0].turn_id, projected[4].turn_id);
        for entry in view.transcript().entries() {
            let atman_runtime::projection::flow::FlowRecord::Legacy(record) = &entry.record else {
                panic!("imported history contains an execution record");
            };
            assert!(record.source.timestamp.is_none());
            let raw: serde_json::Value = serde_json::from_slice(&record.bytes().unwrap()).unwrap();
            assert_eq!(raw["imported_from"]["created_ms"], 0);
            assert_eq!(raw["imported_from"]["session_id"], "external-session");
        }
        let store = RuntimeStore::open(data.path()).unwrap();
        let writer = store.open_session(id).unwrap();
        let flow = writer.open_flow(instance).unwrap();
        assert_eq!(flow.state().state(), &atman_rt::InstanceState::Closed);
        assert!(!data.path().join("sessions").exists());
        assert!(!store.root().join("host").exists());
    }

    #[test]
    fn invalid_external_history_does_not_publish_partial_sessions() {
        let data = tempfile::tempdir().unwrap();
        let messages = [0, i64::MAX].map(|created_ms| ImportedMessage {
            role: MessageRole::User,
            text: "Retain original".into(),
            agent: None,
            model: None,
            created_ms,
        });
        assert!(import_messages(data.path(), "external", "source", &messages).is_err());
        assert!(import_messages(data.path(), "external", "source", &[]).is_err());
        assert!(RuntimeStore::session_ids(data.path()).unwrap().is_empty());
        assert_eq!(std::fs::read_dir(data.path()).unwrap().count(), 0);
    }
}
