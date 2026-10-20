//! Operator directory presentation without execution ownership or history migration.

use std::{io, path::Path};

use anyhow::{Result, ensure};
use atman_runtime::{
    flow_runtime::catalog::{SessionCatalog, SessionSummary},
    flow_store::{RuntimeStore, SessionRemoval},
    session_meta::SessionMeta,
};
use atman_tui::{
    flow_hub::{SessionDirectory, SessionRemovalRequest, SessionRow},
    project_hub::SessionTarget,
};
use uuid::Uuid;

fn order(target: &SessionTarget) -> (String, bool) {
    match target {
        SessionTarget::Persistent(id) => (id.to_string(), true),
        SessionTarget::Legacy(id) => (id.clone(), false),
    }
}

fn legacy_ids(root: &Path) -> io::Result<Vec<SessionTarget>> {
    let entries = match std::fs::read_dir(root.join("sessions")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(name) = entry.file_name().to_str()
            && let Ok(id) = Uuid::parse_str(name)
            && id.to_string() == name
        {
            ids.push(SessionTarget::Legacy(name.to_owned()));
        }
    }
    Ok(ids)
}

fn legacy_row(root: &Path, id: String) -> SessionRow {
    let directory = root.join("sessions").join(&id);
    let meta = SessionMeta::load(&directory).unwrap_or_default();
    let availability = match std::fs::symlink_metadata(directory.join("events.jsonl")) {
        Ok(meta) if meta.is_file() => "Original history · read-only snapshot".into(),
        Ok(_) => "Unavailable · original history is not a regular file".into(),
        Err(error) => format!("Unavailable · {error}"),
    };
    SessionRow {
        cleanup_pending: false,
        target: SessionTarget::Legacy(id.clone()),
        title: meta.title.unwrap_or_else(|| format!("Legacy session {id}")),
        detail: format!(
            "{id} · {availability} · {}",
            meta.project_root
                .map_or_else(|| "No project".into(), |path| path.display().to_string())
        ),
    }
}

pub(super) async fn sessions(
    catalog: &SessionCatalog,
    root: &Path,
    before: Option<SessionTarget>,
    limit: usize,
) -> Result<SessionDirectory> {
    ensure!(
        (1..=atman_tui::flow_hub::PAGE_SIZE).contains(&limit),
        "session page limit must be between 1 and 50"
    );
    if let Some(SessionTarget::Legacy(id)) = &before {
        ensure!(
            Uuid::parse_str(id)?.to_string() == *id,
            "noncanonical legacy session cursor"
        );
    }
    let mut targets = catalog
        .ids()
        .await?
        .into_iter()
        .map(SessionTarget::Persistent)
        .collect::<Vec<_>>();
    let data = root.to_owned();
    let (legacy, cleanup, legacy_cleanup) = tokio::task::spawn_blocking(move || {
        Ok::<_, io::Error>((
            legacy_ids(&data)?,
            RuntimeStore::pending_session_removals(&data)?,
            RuntimeStore::pending_legacy_removals(&data)?,
        ))
    })
    .await??;
    targets.extend(legacy);
    targets.extend(cleanup.iter().copied().map(SessionTarget::Persistent));
    targets.extend(
        legacy_cleanup
            .iter()
            .map(|id| SessionTarget::Legacy(id.to_string())),
    );
    targets.sort_by_cached_key(order);
    targets.dedup();
    let total = targets.len();
    let boundary = before.as_ref().map(order);
    let eligible = targets
        .into_iter()
        .rev()
        .filter(|target| {
            boundary
                .as_ref()
                .is_none_or(|boundary| order(target) < *boundary)
        })
        .take(limit + 1)
        .collect::<Vec<_>>();
    let next = (eligible.len() > limit).then(|| eligible[limit - 1].clone());
    let mut sessions = Vec::new();
    for target in eligible.into_iter().take(limit) {
        sessions.push(match target {
            SessionTarget::Persistent(id) if cleanup.contains(&id) => SessionRow {
                target: SessionTarget::Persistent(id),
                title: "Session cleanup pending".into(),
                detail: format!("{id} · Deleted · Enter to finish interrupted cleanup"),
                cleanup_pending: true,
            },
            SessionTarget::Persistent(id) => catalog
                .overview(id)
                .await
                .map(|view| view.summary)
                .unwrap_or_else(|error| SessionSummary::unavailable(id, error))
                .into(),
            SessionTarget::Legacy(id)
                if legacy_cleanup
                    .iter()
                    .any(|pending| pending.to_string() == id) =>
            {
                SessionRow {
                    target: SessionTarget::Legacy(id.clone()),
                    title: "Legacy cleanup pending".into(),
                    detail: format!(
                        "{id} · Original history · Enter to finish interrupted cleanup"
                    ),
                    cleanup_pending: true,
                }
            }
            SessionTarget::Legacy(id) => {
                let data = root.to_owned();
                tokio::task::spawn_blocking(move || legacy_row(&data, id)).await?
            }
        });
    }
    Ok(SessionDirectory {
        sessions,
        next,
        total,
    })
}

pub(super) async fn removal(
    root: &Path,
    target: SessionTarget,
) -> Result<Box<atman_tui::flow_hub::SessionRemovalPage>> {
    let data = root.to_owned();
    tokio::task::spawn_blocking(move || {
        let id = match target {
            SessionTarget::Persistent(id) => id,
            SessionTarget::Legacy(id) => {
                let parsed = Uuid::parse_str(&id)?;
                ensure!(
                    parsed.to_string() == id,
                    "noncanonical legacy session identity"
                );
                let saved = RuntimeStore::inspect_legacy_removal(&data, parsed)?;
                let cleanup = saved.is_some();
                let request = saved
                    .map(Ok)
                    .unwrap_or_else(|| RuntimeStore::review_legacy_removal(&data, parsed))?;
                let row = legacy_row(&data, id);
                return Ok(Box::new(atman_tui::flow_hub::SessionRemovalPage {
                    detail: request.directory.display().to_string(),
                    request: SessionRemovalRequest::Legacy(request),
                    title: row.title,
                    cleanup,
                }));
            }
        };
        if let Some(request) = RuntimeStore::inspect_session_removal(&data, id)? {
            return Ok(Box::new(atman_tui::flow_hub::SessionRemovalPage {
                request: SessionRemovalRequest::Persistent(request),
                title: "Deleted session".into(),
                cleanup: true,
                detail: "Continue the original cleanup request; the session cannot execute".into(),
            }));
        }
        let inspected = RuntimeStore::inspect_session(&data, id)?;
        ensure!(
            inspected.journal.trailing_bytes == 0,
            "Session journal is incomplete"
        );
        Ok(Box::new(atman_tui::flow_hub::SessionRemovalPage {
            request: SessionRemovalRequest::Persistent(SessionRemoval {
                session_id: id,
                expected: inspected
                    .journal
                    .cursor
                    .ok_or_else(|| anyhow::anyhow!("Session has no creation receipt"))?,
            }),
            title: inspected
                .projection
                .title
                .unwrap_or_else(|| "Untitled session".into()),
            detail: format!(
                "{} flows · {}",
                inspected.projection.flows.len(),
                inspected.dir.display()
            ),
            cleanup: false,
        }))
    })
    .await?
}

#[cfg(test)]
async fn remove(root: &Path, request: SessionRemovalRequest) -> Result<()> {
    let data = root.to_owned();
    tokio::task::spawn_blocking(move || match request {
        SessionRemovalRequest::Persistent(request) => {
            RuntimeStore::open(&data)?.delete_session(&request)
        }
        SessionRemovalRequest::Legacy(request) => {
            RuntimeStore::open(&data)?.delete_legacy_session(&request, true)
        }
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::flow_store::{RuntimeStore, SessionDescriptor};

    #[tokio::test]
    async fn original_history_review_and_cleanup_keep_native_identity_separate() {
        let temp = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(temp.path()).unwrap();
        let id = Uuid::now_v7();
        let source = temp.path().join("sessions").join(id.to_string());
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("events.jsonl"), "original\n").unwrap();
        let native = store
            .create_session(SessionDescriptor {
                id,
                project: None,
                configuration: Default::default(),
                title: "Persistent".into(),
            })
            .unwrap();
        let target = SessionTarget::Legacy(id.to_string());
        let page = removal(temp.path(), target.clone()).await.unwrap();
        assert_eq!(page.request.target(), target);
        assert!(!page.cleanup);
        let catalog = SessionCatalog::new(None, Some(temp.path().to_owned()));
        assert_eq!(
            sessions(&catalog, temp.path(), None, 10)
                .await
                .unwrap()
                .total,
            2
        );
        remove(temp.path(), page.request.clone()).await.unwrap();
        assert!(native.dir().exists());
        assert!(!source.exists());
        let staged = store
            .root()
            .join(".deletions/legacy-sessions")
            .join(id.to_string())
            .join("data");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("partial"), "pending").unwrap();
        let rows = sessions(&catalog, temp.path(), None, 10).await.unwrap();
        assert_eq!(rows.total, 2);
        assert!(
            rows.sessions
                .iter()
                .any(|row| row.target == target && row.cleanup_pending)
        );
        let retry = removal(temp.path(), target).await.unwrap();
        assert!(retry.cleanup);
        assert_eq!(retry.request, page.request);
        remove(temp.path(), retry.request).await.unwrap();
        let rows = sessions(&catalog, temp.path(), None, 10).await.unwrap();
        assert_eq!(rows.total, 1);
        assert_eq!(rows.sessions[0].target, SessionTarget::Persistent(id));
    }

    #[tokio::test]
    async fn session_removal_review_preserves_versions_and_discovers_interrupted_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(temp.path()).unwrap();
        let id = Uuid::now_v7();
        let mut writer = store
            .create_session(SessionDescriptor {
                id,
                title: "Original".into(),
                project: None,
                configuration: Default::default(),
            })
            .unwrap();
        let initial = removal(temp.path(), SessionTarget::Persistent(id))
            .await
            .unwrap();
        assert_eq!(initial.title, "Original");
        assert!(!initial.cleanup);
        assert!(
            remove(temp.path(), initial.request.clone())
                .await
                .unwrap_err()
                .to_string()
                .contains("owner")
        );
        writer
            .set_title(&writer.cursor().clone(), Some("Renamed".into()))
            .unwrap();
        drop(writer);
        assert!(
            remove(temp.path(), initial.request)
                .await
                .unwrap_err()
                .to_string()
                .contains("changed")
        );
        let current = removal(temp.path(), SessionTarget::Persistent(id))
            .await
            .unwrap();
        assert_eq!(current.title, "Renamed");
        remove(temp.path(), current.request.clone()).await.unwrap();
        // Simulate data cleanup interrupted after its receipt and directory move committed.
        let data = store
            .root()
            .join(".deletions/sessions")
            .join(id.to_string())
            .join("data");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("partial"), "retained until retry").unwrap();
        let catalog = SessionCatalog::new(None, Some(temp.path().to_owned()));
        let listed = sessions(&catalog, temp.path(), None, 1).await.unwrap();
        assert_eq!(listed.total, 1);
        assert_eq!(listed.sessions[0].target, SessionTarget::Persistent(id));
        assert!(listed.sessions[0].cleanup_pending);
        assert!(data.join("partial").exists());
        let retry = removal(temp.path(), SessionTarget::Persistent(id))
            .await
            .unwrap();
        assert!(retry.cleanup);
        assert_eq!(retry.request, current.request);
        remove(temp.path(), retry.request).await.unwrap();
        assert!(!data.exists());
        assert_eq!(
            sessions(&catalog, temp.path(), None, 1)
                .await
                .unwrap()
                .total,
            0
        );
        assert!(RuntimeStore::inspect_session(temp.path(), id).is_err());
        // A late duplicate result uses the same receipt and cannot recreate a session.
        remove(temp.path(), current.request).await.unwrap();
        assert!(!temp.path().join("runtime-v1/host").exists());
    }

    #[tokio::test]
    async fn mixed_pages_preserve_source_identity_without_requiring_metadata_or_reading_bodies() {
        let temp = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(temp.path()).unwrap();
        let mut expected = Vec::new();
        let ids = [Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7()];
        for id in ids {
            let _session = store
                .create_session(SessionDescriptor {
                    id,
                    title: "Persistent".into(),
                    project: None,
                    configuration: Default::default(),
                })
                .unwrap();
            let directory = temp.path().join("sessions").join(id.to_string());
            std::fs::create_dir_all(&directory).unwrap();
            // Invalid UTF-8 proves directory browsing does not parse the log or rebuild counts.
            std::fs::write(directory.join("events.jsonl"), [0xff, 0x00]).unwrap();
            expected.extend([
                SessionTarget::Legacy(id.to_string()),
                SessionTarget::Persistent(id),
            ]);
        }
        let missing = Uuid::now_v7();
        std::fs::create_dir_all(temp.path().join("sessions").join(missing.to_string())).unwrap();
        expected.push(SessionTarget::Legacy(missing.to_string()));
        let broken = Uuid::now_v7();
        std::fs::create_dir_all(
            temp.path()
                .join("runtime-v1/sessions")
                .join(broken.to_string()),
        )
        .unwrap();
        expected.push(SessionTarget::Persistent(broken));
        std::fs::create_dir_all(temp.path().join("sessions/not-a-session")).unwrap();
        expected.sort_by_cached_key(order);
        expected.reverse();
        let catalog = SessionCatalog::new(None, Some(temp.path().to_owned()));
        let mut before = None;
        let mut found = Vec::new();
        loop {
            let page = sessions(&catalog, temp.path(), before, 1).await.unwrap();
            assert_eq!(page.total, expected.len());
            assert_eq!(page.sessions.len(), 1);
            let row = page.sessions.into_iter().next().unwrap();
            if row.target == SessionTarget::Legacy(missing.to_string())
                || row.target == SessionTarget::Persistent(broken)
            {
                assert!(row.detail.contains("Unavailable") || row.title.contains("Unavailable"));
            }
            found.push(row.target);
            before = page.next;
            if before.is_none() {
                break;
            }
            assert!(found.len() < expected.len());
        }
        assert_eq!(found, expected);
        for id in ids {
            let directory = temp.path().join("sessions").join(id.to_string());
            assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
            assert_eq!(
                std::fs::read(directory.join("events.jsonl")).unwrap(),
                [0xff, 0x00]
            );
        }
        assert!(!temp.path().join("runtime-v1/host").exists());
        assert!(
            sessions(
                &catalog,
                temp.path(),
                Some(SessionTarget::Legacy("../invalid".into())),
                1
            )
            .await
            .is_err()
        );
    }
}
