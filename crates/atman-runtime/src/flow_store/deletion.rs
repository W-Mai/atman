//! Removal requires exclusive catalog access and closed, detached instances.

use std::{fs, io, path::Path};

use anyhow::{Context, Result, ensure};
use atman_rt::{InstanceState, TurnOutcome, TurnPhase};

use super::{
    FlowContent, FlowFact, FlowTransition, RuntimeStore, SessionInspection, SessionOwner,
    SessionStore, profile::ProfileStore,
};
use crate::{event_log::journal::sync_parent, session_meta::fingerprint_from_root};

/// A confirmed catalog version for deleting one Session and its recorded flows.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SessionRemoval {
    pub session_id: uuid::Uuid,
    pub expected: crate::event_log::journal::CommitReceipt,
}

impl RuntimeStore {
    /// Discover unfinished deletion cleanup without opening Session owners or changing data.
    pub fn pending_session_removals(data: &Path) -> io::Result<Vec<uuid::Uuid>> {
        pending_removals(
            &data.join(super::NAMESPACE),
            "sessions",
            &data.join(super::NAMESPACE).join("sessions"),
        )
    }

    /// Read the original deletion request, including after cleanup completed, without mutation.
    pub fn inspect_session_removal(
        data: &Path,
        id: uuid::Uuid,
    ) -> io::Result<Option<SessionRemoval>> {
        let root = Self {
            root: data.join(super::NAMESPACE),
        };
        let marker = root.deletion_path(id);
        if !marker.try_exists()? {
            return Ok(None);
        }
        let saved: SessionRemoval = serde_json::from_slice(&fs::read(marker.join("receipt.json"))?)
            .map_err(super::invalid)?;
        if saved.session_id != id || saved.expected.journal_id != id {
            return Err(super::invalid(
                "session deletion receipt belongs to another identity",
            ));
        }
        Ok(Some(saved))
    }
}

pub(super) fn pending_removals(
    root: &Path,
    kind: &str,
    source: &Path,
) -> io::Result<Vec<uuid::Uuid>> {
    let entries = match fs::read_dir(root.join(".deletions").join(kind)) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(vec![]),
        Err(error) => return Err(error),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir()
            && let Some(name) = entry.file_name().to_str()
            && let Ok(id) = uuid::Uuid::parse_str(name)
            && id.to_string() == name
            && (entry.path().join("data").try_exists()? || source.join(name).try_exists()?)
        {
            ids.push(id);
        }
    }
    ids.sort_unstable();
    Ok(ids)
}

impl RuntimeStore {
    /// Discard a detached startup Session containing only initialization. The caller supplies the catalog receipt captured at creation; older or explicitly retained Sessions must not be submitted here. A completed startup command without effects is initialization; user input, durable work, inherited content and active ownership retain the Session.
    pub fn discard_unused_session(&self, request: &SessionRemoval) -> Result<bool> {
        let _catalog = self.lock_catalog()?;
        let data = self.root.parent().unwrap();
        if let Some(saved) = Self::inspect_session_removal(data, request.session_id)? {
            ensure!(saved == *request, "session deletion request changed");
            self.retained_project(request)?;
            self.finish_session_deletion(request.session_id)?;
            return Ok(true);
        }
        let dir = self
            .root
            .join("sessions")
            .join(request.session_id.to_string());
        let owner = match SessionOwner::lock(&dir) {
            Ok(owner) => owner,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(error) => return Err(error.into()),
        };
        if ProfileStore::protected_session(self)? == Some(request.session_id) {
            return Ok(false);
        }
        let inspected = Self::inspect_session(data, request.session_id)?;
        if inspected.journal.cursor.as_ref() != Some(&request.expected)
            || inspected.journal.trailing_bytes != 0
            || inspected.projection.migrated_from.is_some()
        {
            return Ok(false);
        }
        let [entry] = inspected.projection.flows.as_slice() else {
            return Ok(false);
        };
        if inspected.projection.default_flow.as_ref() != Some(&entry.id) {
            return Ok(false);
        }
        // Structural startup execution is initialization, not user activity. Keep
        // every other fact, including cancelled input and effect diagnostics.
        let mut unused = true;
        let recorded = super::read_registered_flow(
            &inspected.dir,
            &inspected.projection,
            &entry.id,
            |_, facts| {
                unused &= facts.iter().all(|fact| match fact {
                    FlowFact::Created(descriptor) => descriptor.created_by.is_none(),
                    FlowFact::Activated(activation) => {
                        let input = &activation.input;
                        input.name.is_none()
                            && input.display_label.is_none()
                            && input.scope.is_none()
                            && input.workspace.is_none()
                            && input.initial_goal.is_none()
                            && input.inherited_context.is_empty()
                            && input.inherited_outputs.is_empty()
                            && input.continued_from.is_none()
                            && input.result_route.is_none()
                    }
                    FlowFact::Transition(FlowTransition::TurnStarted { .. }
                        | FlowTransition::PhaseChanged { phase: TurnPhase::Finishing, .. }) => true,
                    FlowFact::InputCommitted(input) => matches!(
                        &input.content,
                        super::inbox::InputContent::InvocationInput { messages, .. }
                            if messages.iter().all(|message| message.origin == crate::message::MessageOrigin::Internal)
                    ),
                    FlowFact::TurnCompleted(completion) => completion.outcome == TurnOutcome::Completed,
                    FlowFact::ExecutionEvents(events) => events.iter().all(|record| {
                        use crate::{event::Event, nodegraph::NodeKind};
                        matches!(&record.event,
                            Event::FlowStart { .. } | Event::FlowEnd { .. } | Event::FlowGraph { .. }
                            | Event::FlowNodeEnd { .. }
                            | Event::FlowNodeStart { kind: NodeKind::Return | NodeKind::When { .. } | NodeKind::Loop | NodeKind::Yield, .. }
                        )
                    }),
                    _ => false,
                });
                Ok(())
            },
        )?;
        if !unused || recorded.journal.trailing_bytes != 0 || !initialization_input_only(&recorded)
        {
            return Ok(false);
        }
        self.retain_session_removal(&inspected, request)?;
        drop(owner);
        self.finish_session_deletion(request.session_id)?;
        Ok(true)
    }

    pub(super) fn deletion_path(&self, id: uuid::Uuid) -> std::path::PathBuf {
        self.root
            .join(".deletions")
            .join("sessions")
            .join(id.to_string())
    }

    pub(super) fn ensure_session_available(&self, id: uuid::Uuid) -> io::Result<()> {
        if self.deletion_path(id).try_exists()? {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "session was deleted",
            ));
        }
        Ok(())
    }

    /// Delete a closed, detached Session under catalog ownership. Retrying the same request finishes interrupted cleanup; the retired identity cannot be created or migrated again. This blocking host operation never stops execution or releases live workspaces.
    pub fn delete_session(&self, request: &SessionRemoval) -> Result<()> {
        let _catalog = self.lock_catalog()?;
        let marker = self.deletion_path(request.session_id);
        let receipt = marker.join("receipt.json");
        match fs::read(&receipt) {
            Ok(bytes) => {
                let saved: SessionRemoval = serde_json::from_slice(&bytes)?;
                ensure!(saved == *request, "session deletion request changed");
                self.retained_project(request)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                let dir = self
                    .root
                    .join("sessions")
                    .join(request.session_id.to_string());
                let owner = SessionOwner::lock(&dir)
                    .context("session still has an owner; detach it before deletion")?;
                self.validate_session_removal(request)?;
                let inspected =
                    Self::inspect_session(self.root.parent().unwrap(), request.session_id)?;
                self.retain_session_removal(&inspected, request)?;
                // The durable receipt retires the identity before any data is removed.
                drop(owner);
            }
            Err(error) => return Err(error.into()),
        }
        self.finish_session_deletion(request.session_id)
    }

    fn validate_session_removal(&self, request: &SessionRemoval) -> Result<()> {
        self.ensure_session_available(request.session_id)?;
        ensure!(
            ProfileStore::protected_session(self)? != Some(request.session_id),
            "session is the management session; select another management session before deletion"
        );
        let inspected = Self::inspect_session(self.root.parent().unwrap(), request.session_id)?;
        ensure!(
            inspected.journal.cursor.as_ref() == Some(&request.expected),
            "session changed; refresh before deletion"
        );
        validate_closed_session(&inspected)
    }

    fn finish_session_deletion(&self, id: uuid::Uuid) -> Result<()> {
        let source = self.root.join("sessions").join(id.to_string());
        finish_removal(&source, &self.deletion_path(id))
    }
}

fn initialization_input_only(recorded: &super::RecordedFlow) -> bool {
    use super::{inbox::SenderIdentity, launch::EntryStart};

    let Some((activation, _)) = &recorded.replay.activation else {
        return false;
    };
    let Some((_, _, inbox)) = &recorded.replay.projection else {
        return false;
    };
    match &activation.input.request.start {
        EntryStart::Waiting => inbox.inputs().is_empty() && inbox.entries().is_empty(),
        EntryStart::Command { .. } => {
            let Some(binding) = &activation.input.execution else {
                return false;
            };
            let Some(envelope) = activation.envelope() else {
                return false;
            };
            let [input] = inbox.inputs() else {
                return false;
            };
            binding.session_start
                && matches!(envelope.request.sender, SenderIdentity::Host(_))
                && inbox.entries().len() == 1
                && input.message_ids == [envelope.id]
                && input.command == Some(envelope.id)
                && input.lease.turn == binding.first_turn
                && input.lease.run == binding.first_run
                && recorded
                    .replay
                    .completions
                    .get(&input.lease)
                    .is_some_and(|record| record.completion.outcome == TurnOutcome::Completed)
        }
        EntryStart::Invoke { .. } => false,
    }
}

pub(super) fn finish_removal(source: &Path, marker: &Path) -> Result<()> {
    let trash = marker.join("data");
    if source.try_exists()? {
        ensure!(
            !trash.try_exists()?,
            "session deletion has conflicting data directories"
        );
        fs::rename(source, &trash)?;
        sync_parent(source)?;
        sync_parent(&trash)?;
    }
    match fs::remove_dir_all(&trash) {
        Ok(()) => sync_parent(&trash)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).context("session deleted; retry to finish data cleanup");
        }
    }
    Ok(())
}

impl SessionStore {
    /// Check release under the catalog owner. Closed instances cannot resume; deletion revalidates after ownership is relinquished.
    pub(crate) fn validate_removal(&self, data: &Path, request: &SessionRemoval) -> Result<()> {
        let root = RuntimeStore {
            root: data.join(super::NAMESPACE),
        };
        ensure!(
            self.projection().descriptor.id == request.session_id
                && self.dir().canonicalize()?
                    == root
                        .root
                        .join("sessions")
                        .join(request.session_id.to_string())
                        .canonicalize()?,
            "session removal targets a different storage owner"
        );
        let _catalog = root.lock_catalog()?;
        root.validate_session_removal(request)
    }
}

fn validate_closed_session(inspected: &SessionInspection) -> Result<()> {
    ensure!(
        inspected.journal.trailing_bytes == 0,
        "session {} has an incomplete journal",
        inspected.projection.descriptor.id
    );
    for registered in &inspected.projection.flows {
        let flow = inspected.inspect_flow(&registered.id)?;
        ensure!(
            flow.journal.trailing_bytes == 0,
            "flow {} has an incomplete journal",
            registered.id
        );
        ensure!(
            matches!(flow.descriptor.content, FlowContent::History(_))
                || matches!(flow.state.state(), InstanceState::Closed),
            "flow {} is not closed; close it before deletion",
            registered.id
        );
        ensure!(
            flow.pending_sends == 0,
            "flow {} has pending deliveries; reconcile them before deletion",
            registered.id
        );
    }
    Ok(())
}

/// Keep membership stable until shared data and project catalog records have been updated.
pub(crate) struct ProjectDeletion {
    _catalog: crate::file_lock::FileLock,
}

impl ProjectDeletion {
    pub(crate) fn purge_closed_sessions(
        runtime: &RuntimeStore,
        project_root: &Path,
    ) -> Result<Self> {
        let guard = Self {
            _catalog: runtime.lock_catalog()?,
        };
        let data_dir = runtime.root.parent().unwrap();
        let fingerprint = fingerprint_from_root(project_root);
        // Finish previously authorized removals for this project before discovering remaining Sessions.
        for id in RuntimeStore::pending_session_removals(data_dir)? {
            let removal = RuntimeStore::inspect_session_removal(data_dir, id)?.unwrap();
            if runtime.retained_project(&removal)?.as_deref() == Some(fingerprint.as_str()) {
                runtime.finish_session_deletion(id)?;
            }
        }
        let management = ProfileStore::protected_session(runtime)?;
        let mut sessions = Vec::new();
        let mut owners = Vec::new();
        for id in RuntimeStore::session_ids(data_dir)? {
            let inspected = RuntimeStore::inspect_session(data_dir, id)
                .with_context(|| format!("inspect session {id} before deleting project"))?;
            if inspected
                .projection
                .project
                .as_ref()
                .is_none_or(|project| fingerprint_from_root(project.root()) != fingerprint)
            {
                ensure!(
                    !inspected.projection.flows.iter().any(|flow| {
                        flow.project.as_ref().is_some_and(|project| {
                            fingerprint_from_root(project.root()) == fingerprint
                        })
                    }),
                    "session {id} retains flows bound to this project; remove those retained histories before deleting project data"
                );
                continue;
            }
            ensure!(
                management != Some(id),
                "session {id} is the management session; select another management session before deleting this project"
            );
            owners.push(SessionOwner::lock(&inspected.dir).with_context(|| {
                format!("session {id} still has an owner; detach it before deleting this project")
            })?);
            // Read again after acquiring ownership: the preceding inspection may overlap a writer's last commit.
            let inspected = RuntimeStore::inspect_session(data_dir, id)?;
            validate_closed_session(&inspected)?;
            sessions.push(inspected);
        }

        // Every deletion retains the same request ledger and permanently retires the Session identity.
        for session in &sessions {
            runtime.retain_session_removal(
                session,
                &SessionRemoval {
                    session_id: session.projection.descriptor.id,
                    expected: session.journal.cursor.clone().unwrap(),
                },
            )?;
        }
        // Catalog ownership excludes new opens/creates. Release file locks before physical removal.
        drop(owners);
        for session in sessions {
            runtime.finish_session_deletion(session.projection.descriptor.id)?;
        }
        Ok(guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_store::{
        EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowTransition, SessionDescriptor,
    };
    use uuid::Uuid;

    #[test]
    fn deletion_requires_confirmed_closed_detached_data_and_retires_identity() {
        let data = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(data.path()).unwrap();
        let descriptor = SessionDescriptor {
            id: Uuid::now_v7(),
            project: None,
            configuration: Default::default(),
            title: "Delete example".into(),
        };
        let mut session = root.create_session(descriptor.clone()).unwrap();
        let mut flow = session
            .create_flow(FlowDescriptor {
                address: FlowRef {
                    session_id: descriptor.id.into(),
                    instance_id: FlowInstanceId::now(),
                },
                creation_request: Uuid::now_v7(),
                created_by: None,
                content: EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: crate::source_program::RevisionBundle {
                        entry: "main.at".into(),
                        sources: [("main.at".into(), "flow main() {}".into())].into(),
                        edges: Default::default(),
                    },
                }
                .into(),
            })
            .unwrap();
        let request = SessionRemoval {
            session_id: descriptor.id,
            expected: session.cursor().clone(),
        };
        assert!(
            root.delete_session(&request)
                .unwrap_err()
                .to_string()
                .contains("owner")
        );
        drop(session);
        assert!(root.delete_session(&request).is_err());
        drop(flow);
        assert!(
            root.delete_session(&request)
                .unwrap_err()
                .to_string()
                .contains("not closed")
        );
        let session = root.open_session(descriptor.id).unwrap();
        flow = session
            .open_flow(&session.projection().flows[0].id)
            .unwrap();
        for transition in [FlowTransition::CloseRequested, FlowTransition::Closed] {
            flow.transition(flow.state().revision(), transition)
                .unwrap();
        }
        let path = flow.dir().join("events.jsonl");
        drop((session, flow));
        let original = fs::read(&path).unwrap();
        let mut incomplete = original.clone();
        incomplete.extend_from_slice(b"{\"partial\":");
        fs::write(&path, &incomplete).unwrap();
        assert!(
            root.delete_session(&request)
                .unwrap_err()
                .to_string()
                .contains("incomplete")
        );
        assert_eq!(fs::read(&path).unwrap(), incomplete);
        fs::write(&path, original).unwrap();
        let mut stale = request.clone();
        stale.expected.sequence += 1;
        assert!(
            root.delete_session(&stale)
                .unwrap_err()
                .to_string()
                .contains("changed")
        );
        let mut profile = ProfileStore::open(&root).unwrap();
        let manager = profile
            .bootstrap(Default::default())
            .unwrap()
            .management
            .unwrap()
            .session
            .id;
        profile
            .select(
                Uuid::now_v7(),
                profile.snapshot().cursor.clone(),
                descriptor.id,
                vec![],
            )
            .unwrap();
        assert!(
            root.delete_session(&request)
                .unwrap_err()
                .to_string()
                .contains("management session")
        );
        profile
            .select(
                Uuid::now_v7(),
                profile.snapshot().cursor.clone(),
                manager,
                vec![],
            )
            .unwrap();
        root.delete_session(&request).unwrap();
        root.delete_session(&request).unwrap();
        assert!(root.delete_session(&stale).is_err());
        assert!(
            !root
                .root
                .join("sessions")
                .join(descriptor.id.to_string())
                .exists()
        );
        assert!(!root.deletion_path(descriptor.id).join("data").exists());
        assert!(
            root.deletion_path(descriptor.id)
                .join("receipt.json")
                .is_file()
        );
        assert!(root.create_session(descriptor.clone()).is_err());
        assert!(root.open_session(descriptor.id).is_err());
        assert!(RuntimeStore::inspect_session(data.path(), descriptor.id).is_err());
        assert_eq!(
            RuntimeStore::session_ids(data.path()).unwrap(),
            vec![manager]
        );
    }

    #[test]
    fn committed_removal_is_hidden_and_resumes_before_or_after_directory_move() {
        for moved in [false, true] {
            let data = tempfile::tempdir().unwrap();
            let root = RuntimeStore::open(data.path()).unwrap();
            let descriptor = SessionDescriptor {
                id: Uuid::now_v7(),
                project: None,
                configuration: Default::default(),
                title: "Example".into(),
            };
            let session = root.create_session(descriptor.clone()).unwrap();
            let request = SessionRemoval {
                session_id: descriptor.id,
                expected: session.cursor().clone(),
            };
            let source = session.dir().to_owned();
            drop(session);
            let marker = root.deletion_path(descriptor.id);
            root.retain_session_removal(
                &RuntimeStore::inspect_session(data.path(), descriptor.id).unwrap(),
                &request,
            )
            .unwrap();
            if moved {
                fs::rename(&source, marker.join("data")).unwrap();
            }
            let receipt_bytes = fs::read(marker.join("receipt.json")).unwrap();
            assert_eq!(
                RuntimeStore::pending_session_removals(data.path()).unwrap(),
                vec![descriptor.id]
            );
            assert_eq!(
                RuntimeStore::inspect_session_removal(data.path(), descriptor.id).unwrap(),
                Some(request.clone())
            );
            assert_eq!(
                fs::read(marker.join("receipt.json")).unwrap(),
                receipt_bytes
            );
            assert!(RuntimeStore::session_ids(data.path()).unwrap().is_empty());
            assert!(root.open_session(descriptor.id).is_err());
            assert!(root.create_session(descriptor.clone()).is_err());
            assert!(RuntimeStore::inspect_session(data.path(), descriptor.id).is_err());
            let reopened = RuntimeStore::open(data.path()).unwrap();
            reopened.delete_session(&request).unwrap();
            assert!(!source.exists());
            assert!(!marker.join("data").exists());
            assert!(marker.join("receipt.json").is_file());
            assert!(
                RuntimeStore::pending_session_removals(data.path())
                    .unwrap()
                    .is_empty()
            );
            assert_eq!(
                RuntimeStore::inspect_session_removal(data.path(), descriptor.id).unwrap(),
                Some(request)
            );
        }
    }
}
