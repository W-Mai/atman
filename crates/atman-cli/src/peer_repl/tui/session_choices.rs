//! Conversation choices use catalog identities and never attach execution while browsing.

use super::*;
use atman_runtime::session_meta::{SessionMeta, SessionStats};
use atman_tui::{
    SessionPickerRow,
    project_hub::SessionTarget,
    session_switcher::{RenameChoice, SessionSelection},
};

impl scopes::Scopes {
    pub(super) async fn auto_name_session_choice(
        &self,
        selection: SessionSelection,
    ) -> Result<RenameChoice> {
        let SessionTarget::Persistent(id) = &selection.target else {
            bail!("Continue this historical session before requesting an automatic name")
        };
        let expected = selection
            .expected
            .as_ref()
            .context("Session catalog is unavailable; reopen the picker")?;
        let scope = self.open(*id).await?;
        let snapshot = scope.session.snapshot().await?;
        ensure!(
            &snapshot.cursor == expected,
            "Session catalog changed; reopen the picker"
        );
        let entry = snapshot
            .catalog
            .default_flow
            .context("Selected session has no default conversation")?;
        let worker = scope.session.open_flow(entry).await?;
        let context = worker.context();
        let title = super::super::operations::session_title(
            &scope.executor,
            context.goal().as_deref(),
            &context.messages_full(),
        )
        .await?;
        let selection = self
            .rename_session_choice(RenameChoice {
                selection,
                title: Some(title.clone()),
            })
            .await?;
        // Loading naming context must not restore scheduling or move the active view.
        Ok(RenameChoice {
            selection,
            title: Some(title),
        })
    }

    pub(super) async fn resolve_default_session(&self, session: Uuid) -> Result<ViewAddress> {
        let view = self.catalog.overview(session).await?;
        let instance_id = view
            .summary
            .default_flow
            .context("This session has no default conversation")?;
        Ok(atman_runtime::flow_store::FlowRef {
            session_id: session.into(),
            instance_id,
        }
        .into())
    }

    pub(super) async fn resolve_legacy_session(&self, session: Uuid) -> Result<ViewAddress> {
        let path = self
            .root
            .join("sessions")
            .join(session.to_string())
            .join("events.jsonl");
        crate::tui_panels::read_only(move || {
            let history = LegacyHistory::inspect(&path, session)?;
            Ok(ViewAddress::Legacy {
                session,
                owner: legacy_default_owner(&history),
            })
        })
        .await?
    }

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
            crate::tui_panels::read_only(move || -> Result<Vec<SessionPickerRow>> {
                let mut rows = Vec::new();
                for target in directory::legacy_ids(&root)? {
                    let SessionTarget::Legacy(id) = &target else {
                        unreachable!()
                    };
                    let path = root.join("sessions").join(id);
                    let meta = SessionMeta::load(&path).unwrap_or_default();
                    let stats = SessionStats::inspect(&path);
                    let unavailable = stats.as_ref().err().map(ToString::to_string);
                    let session = Uuid::parse_str(id)?;
                    rows.push(SessionPickerRow {
                        id: format!("legacy:{id}"),
                        is_current: matches!(&current, ViewAddress::Legacy { session: current, .. } if *current == session),
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
                            // Resolve ownership only when opening the selected history.
                            destination: None,
                            expected: None,
                            unavailable,
                        }),
                    });
                }
                Ok(rows)
            })
            .await??,
        );
        let root = self.root.clone();
        rows.extend(
            crate::tui_panels::read_only(move || -> Result<Vec<SessionPickerRow>> {
                Ok(atman_runtime::history_index::discover(&root)?
                    .into_iter()
                    .map(|session| SessionPickerRow {
                        id: format!(
                            "indexed:{}:{}",
                            session.source.index.display(),
                            session.source.session
                        ),
                        is_current: false,
                        name: Some(session.title()),
                        project: session.project.map(|path| path.display().to_string()),
                        message_count: session.user_messages as usize,
                        updated_at: session.updated_at,
                        goal: None,
                        selection: Some(SessionSelection {
                            target: SessionTarget::Indexed(session.source),
                            destination: None,
                            expected: None,
                            unavailable: None,
                        }),
                    })
                    .collect())
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
            SessionTarget::Indexed(_) => {
                bail!("Recover this indexed transcript before renaming its independent copy")
            }
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
