//! Operator directory presentation without execution ownership or history migration.

use std::{io, path::Path};

use anyhow::{Result, ensure};
use atman_runtime::{
    flow_runtime::catalog::{SessionCatalog, SessionSummary},
    session_meta::SessionMeta,
};
use atman_tui::{
    flow_hub::{SessionDirectory, SessionRow},
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
    targets.extend(tokio::task::spawn_blocking(move || legacy_ids(&data)).await??);
    targets.sort_by_cached_key(order);
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
            SessionTarget::Persistent(id) => catalog
                .overview(id)
                .await
                .map(|view| view.summary)
                .unwrap_or_else(|error| SessionSummary::unavailable(id, error))
                .into(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::flow_store::{RuntimeStore, SessionDescriptor};

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
