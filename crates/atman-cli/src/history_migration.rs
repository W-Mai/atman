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
