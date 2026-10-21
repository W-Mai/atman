//! Conversation choices use catalog identities and never attach execution while browsing.

use super::*;
use atman_runtime::session_meta::{SessionMeta, SessionStats};
use atman_tui::{
    SessionPickerRow,
    project_hub::SessionTarget,
    session_switcher::{RenameChoice, SessionSelection},
};

impl scopes::Scopes {
    pub(super) async fn session_choices(
        &self,
        current: ViewAddress,
    ) -> Result<Vec<SessionPickerRow>> {
        let mut rows = Vec::new();
        for id in self.catalog.ids().await? {
            let mut row = SessionPickerRow {
                selection: Some(SessionSelection {
                    target: SessionTarget::Persistent(id),
                    destination: None,
                    expected: None,
                    unavailable: None,
                }),
                id: format!("instance:{id}"),
                is_current: false,
                name: Some(format!("Session {}", &id.to_string()[..8])),
                project: None,
                message_count: 0,
                updated_at: String::new(),
                goal: None,
            };
            let selected = row.selection.as_mut().unwrap();
            match self.catalog.overview(id).await {
                Ok(view) => {
                    row.name = view.summary.title;
                    row.project = view
                        .summary
                        .project_root
                        .map(|path| path.display().to_string());
                    selected.expected = view.summary.catalog;
                    if let Some(instance_id) = view.summary.default_flow {
                        let flow = atman_runtime::flow_store::FlowRef {
                            session_id: id.into(),
                            instance_id,
                        };
                        match self.catalog.inspect_flow(&flow, |_| Ok(())).await {
                            Ok(read) => {
                                row.message_count = read.message_count;
                                row.updated_at = read
                                    .activity
                                    .map(|activity| activity.at.to_rfc3339())
                                    .unwrap_or_default();
                                row.goal = read.goal.map(|goal| goal.objective);
                                selected.destination = Some(flow.into());
                            }
                            Err(error) => selected.unavailable = Some(error.to_string()),
                        }
                    }
                }
                Err(error) => selected.unavailable = Some(error.to_string()),
            }
            row.is_current = selected.destination.as_ref() == Some(&current);
            rows.push(row);
        }
        let root = self.root.clone();
        rows.extend(
            tokio::task::spawn_blocking(move || -> Result<Vec<SessionPickerRow>> {
                let mut rows = Vec::new();
                for target in directory::legacy_ids(&root)? {
                    let SessionTarget::Legacy(id) = &target else {
                        unreachable!()
                    };
                    let path = root.join("sessions").join(id);
                    let meta = SessionMeta::load(&path).unwrap_or_default();
                    let stats = SessionStats::load_or_rebuild(&path);
                    let mut unavailable = stats.as_ref().err().map(ToString::to_string);
                    let session = Uuid::parse_str(id)?;
                    let owner = match LegacyHistory::inspect(&path.join("events.jsonl"), session) {
                        Ok(history) => legacy_default_owner(&history),
                        Err(error) => {
                            unavailable = Some(error.to_string());
                            atman_runtime::event_log::legacy::LegacyFlowKey::Default
                        }
                    };
                    let destination = ViewAddress::Legacy { session, owner };
                    rows.push(SessionPickerRow {
                        id: format!("legacy:{id}"),
                        is_current: current == destination,
                        name: meta
                            .title
                            .or_else(|| Some(format!("Original history {}", &id[..8]))),
                        project: meta.project_root.map(|path| path.display().to_string()),
                        message_count: stats
                            .map(|stats| stats.message_count as usize)
                            .unwrap_or_default(),
                        updated_at: path
                            .join("events.jsonl")
                            .metadata()
                            .and_then(|meta| meta.modified())
                            .ok()
                            .map(|time| chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339())
                            .unwrap_or_default(),
                        goal: atman_runtime::memory::goal::GoalStore::at(&path).get().ok(),
                        selection: Some(SessionSelection {
                            target,
                            destination: Some(destination),
                            expected: None,
                            unavailable,
                        }),
                    });
                }
                Ok(rows)
            })
            .await??,
        );
        Ok(rows)
    }

    pub(super) async fn rename_session_choice(
        &self,
        change: RenameChoice,
    ) -> Result<SessionSelection> {
        let mut selection = change.selection;
        match &selection.target {
            SessionTarget::Persistent(id) => {
                let expected = selection
                    .expected
                    .clone()
                    .context("Session catalog is unavailable; reopen the picker")?;
                let session = self.host.open_session(&self.root, *id).await?;
                selection.expected = Some(session.set_title(expected, change.title).await?);
            }
            SessionTarget::Legacy(id) => {
                ensure!(
                    Uuid::parse_str(id)?.to_string() == *id,
                    "Invalid session identity"
                );
                let root = self.root.clone();
                let id = Uuid::parse_str(id)?;
                tokio::task::spawn_blocking(move || -> Result<()> {
                    RuntimeStore::open(&root)?.rename_legacy_session(id, change.title)?;
                    Ok(())
                })
                .await??;
            }
        }
        Ok(selection)
    }
}
