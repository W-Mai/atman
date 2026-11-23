//! Durable workspace-open receipts owned by the Profile writer.

use std::{
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
    time::Instant,
};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ProfileStore, RuntimeStore, SessionConfiguration, SessionDescriptor, invalid};
use crate::{
    config_hub::ConfigHub,
    event_log::journal::{CommitReceipt, sync_parent},
    flow_store::{NAMESPACE, ensure_dir, read_registered_flow},
    project_catalog::{
        ProjectCatalogStore,
        workspace::{BrowseError, WorkspaceGrant, WorkspaceReader, WorkspaceSelection},
    },
    storage::ProjectContext,
};

const RECORD_BYTES: u64 = 64 * 1024;

pub struct WorkspaceOpenCandidate {
    pub selection: WorkspaceSelection,
    pub expires: Instant,
}

pub enum WorkspaceOpenAction {
    Open {
        operation: Uuid,
        selection_id: Uuid,
        candidate: Option<WorkspaceOpenCandidate>,
        reader: WorkspaceReader,
    },
    Complete {
        operation: Uuid,
        selection_id: Uuid,
        conversation: CommitReceipt,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkspaceOpenFailure {
    SelectionExpired,
    DirectoryChanged,
    ReadOnlyRoot,
    DirectoryUnavailable,
    InvalidConfiguration,
    ProjectUnavailable,
    SessionUnavailable,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "stage", rename_all = "snake_case", deny_unknown_fields)]
enum Stage {
    Prepared,
    ProjectPinned {
        fingerprint: String,
    },
    SessionCreated {
        fingerprint: String,
        catalog: CommitReceipt,
    },
    ConversationReady {
        fingerprint: String,
        catalog: CommitReceipt,
        conversation: CommitReceipt,
    },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WorkspaceProgress {
    Unseen,
    Prepared,
    ProjectPinned {
        fingerprint: String,
    },
    SessionCreated {
        fingerprint: String,
        catalog: CommitReceipt,
    },
    ConversationReady {
        fingerprint: String,
        catalog: CommitReceipt,
        conversation: CommitReceipt,
    },
}

pub struct WorkspaceOpenOutcome {
    pub progress: WorkspaceProgress,
    pub failure: Option<WorkspaceOpenFailure>,
    pub bootstrap: Option<WorkspaceBootstrap>,
}

#[derive(Clone)]
pub struct WorkspaceBootstrap {
    descriptor: SessionDescriptor,
    grant: WorkspaceGrant,
    profile: Uuid,
}

impl WorkspaceBootstrap {
    pub fn matches(&self, catalog: &super::SessionProjection) -> bool {
        catalog.descriptor == self.descriptor && catalog.project == self.descriptor.project
    }

    pub async fn verify(&self, reader: WorkspaceReader) -> io::Result<()> {
        let proof = self.clone();
        crate::read_work::capture(move || {
            let path = reader
                .verify_grant(&proof.grant, proof.profile)
                .map_err(|_| io::Error::from(io::ErrorKind::PermissionDenied))?;
            if proof
                .descriptor
                .project
                .as_ref()
                .is_none_or(|project| project.root() != path)
            {
                return Err(io::Error::from(io::ErrorKind::PermissionDenied));
            }
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u8,
    operation: Uuid,
    selection: Uuid,
    profile: Uuid,
    grant: WorkspaceGrant,
    descriptor: SessionDescriptor,
    opened_at: DateTime<Utc>,
    stage: Stage,
}

impl Record {
    fn report(&self, failure: Option<WorkspaceOpenFailure>) -> WorkspaceOpenOutcome {
        let progress = match &self.stage {
            Stage::Prepared => WorkspaceProgress::Prepared,
            Stage::ProjectPinned { fingerprint } => WorkspaceProgress::ProjectPinned {
                fingerprint: fingerprint.clone(),
            },
            Stage::SessionCreated {
                fingerprint,
                catalog,
            } => WorkspaceProgress::SessionCreated {
                fingerprint: fingerprint.clone(),
                catalog: catalog.clone(),
            },
            Stage::ConversationReady {
                fingerprint,
                catalog,
                conversation,
            } => WorkspaceProgress::ConversationReady {
                fingerprint: fingerprint.clone(),
                catalog: catalog.clone(),
                conversation: conversation.clone(),
            },
        };
        WorkspaceOpenOutcome {
            progress,
            failure,
            bootstrap: None,
        }
    }

    fn expected_selection(&self, selection: Uuid) -> io::Result<()> {
        if self.selection != selection {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "Workspace operation identity conflicts with the recorded selection",
            ));
        }
        Ok(())
    }

    fn verify(&self, reader: &WorkspaceReader) -> Result<(), WorkspaceOpenFailure> {
        let path = reader
            .verify_grant(&self.grant, self.profile)
            .map_err(open_failure)?;
        if self
            .descriptor
            .project
            .as_ref()
            .is_none_or(|project| project.root() != path)
        {
            return Err(WorkspaceOpenFailure::DirectoryChanged);
        }
        Ok(())
    }
}

impl ProfileStore {
    pub(crate) fn workspace_open(
        &self,
        action: WorkspaceOpenAction,
    ) -> io::Result<WorkspaceOpenOutcome> {
        if self.write_failed() {
            return Err(io::Error::other("Profile writer is unavailable"));
        }
        match action {
            WorkspaceOpenAction::Open {
                operation,
                selection_id,
                candidate,
                reader,
            } => self.open_workspace(operation, selection_id, candidate, reader),
            WorkspaceOpenAction::Complete {
                operation,
                selection_id,
                conversation,
            } => self.complete_workspace(operation, selection_id, conversation),
        }
    }

    fn open_workspace(
        &self,
        operation: Uuid,
        selection: Uuid,
        candidate: Option<WorkspaceOpenCandidate>,
        reader: WorkspaceReader,
    ) -> io::Result<WorkspaceOpenOutcome> {
        validate_ids(operation, selection)?;
        let profile = self.snapshot().profile_id;
        let mut record = match load(self.data_dir(), profile, operation)? {
            Some(record) => {
                record.expected_selection(selection)?;
                record
            }
            None => {
                let Some(candidate) =
                    candidate.filter(|candidate| candidate.expires > Instant::now())
                else {
                    return Ok(unseen(Some(WorkspaceOpenFailure::SelectionExpired)));
                };
                let grant = match reader.grant(
                    candidate.selection,
                    profile,
                    self.snapshot().cursor.sequence == 1,
                ) {
                    Ok(grant) => grant,
                    Err(error) => return Ok(unseen(Some(open_failure(error)))),
                };
                let path = match reader.verify_grant(&grant, profile) {
                    Ok(path) => path,
                    Err(error) => return Ok(unseen(Some(open_failure(error)))),
                };
                let hub = ConfigHub::from_config_dir(reader.config_dir());
                let project = match ProjectContext::resolve(&hub, &path, self.data_dir()) {
                    Ok(project) if project.root() == path => project,
                    Ok(_) => return Ok(unseen(Some(WorkspaceOpenFailure::DirectoryChanged))),
                    Err(_) => return Ok(unseen(Some(WorkspaceOpenFailure::InvalidConfiguration))),
                };
                let record = Record {
                    version: 1,
                    operation,
                    selection,
                    profile,
                    grant,
                    descriptor: SessionDescriptor {
                        id: Uuid::new_v4(),
                        title: "Untitled Session".into(),
                        project: Some(project),
                        configuration: SessionConfiguration::Atman(hub.source()),
                    },
                    opened_at: Utc::now(),
                    stage: Stage::Prepared,
                };
                if let Err(failure) = record.verify(&reader) {
                    return Ok(unseen(Some(failure)));
                }
                save(self.data_dir(), &record)?;
                record
            }
        };
        if matches!(record.stage, Stage::ConversationReady { .. }) {
            return Ok(record.report(None));
        }
        if let Err(failure) = record.verify(&reader) {
            return Ok(record.report(Some(failure)));
        }
        if matches!(record.stage, Stage::Prepared) {
            let project = record
                .descriptor
                .project
                .as_ref()
                .expect("a workspace has a project");
            let pinned = ProjectCatalogStore::new(self.data_dir())
                .register_pinned(project.root(), record.opened_at);
            let fingerprint = match pinned {
                Ok(pinned) if pinned.root == project.root() => pinned.fingerprint,
                _ => return Ok(record.report(Some(WorkspaceOpenFailure::ProjectUnavailable))),
            };
            record.stage = Stage::ProjectPinned { fingerprint };
            save(self.data_dir(), &record)?;
        }
        if let Stage::ProjectPinned { fingerprint } = &record.stage {
            if let Err(failure) = record.verify(&reader) {
                return Ok(record.report(Some(failure)));
            }
            let catalog = match self.create_workspace_session(&record.descriptor) {
                Ok(catalog) => catalog,
                Err(_) => return Ok(record.report(Some(WorkspaceOpenFailure::SessionUnavailable))),
            };
            record.stage = Stage::SessionCreated {
                fingerprint: fingerprint.clone(),
                catalog,
            };
            save(self.data_dir(), &record)?;
        }
        let mut outcome = record.report(None);
        if matches!(record.stage, Stage::SessionCreated { .. }) {
            outcome.bootstrap = Some(WorkspaceBootstrap {
                descriptor: record.descriptor,
                grant: record.grant,
                profile: record.profile,
            });
        }
        Ok(outcome)
    }

    fn create_workspace_session(
        &self,
        descriptor: &SessionDescriptor,
    ) -> io::Result<CommitReceipt> {
        match self.runtime.create_session(descriptor.clone()) {
            Ok(session) => Ok(session.cursor().clone()),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                let session = RuntimeStore::inspect_session(self.data_dir(), descriptor.id)?;
                if session.projection.descriptor != *descriptor
                    || session.journal.trailing_bytes != 0
                {
                    return Err(invalid(
                        "Workspace Session differs from its recorded creation",
                    ));
                }
                session
                    .journal
                    .cursor
                    .ok_or_else(|| invalid("Workspace Session has no catalog receipt"))
            }
            Err(error) => Err(error),
        }
    }

    fn complete_workspace(
        &self,
        operation: Uuid,
        selection: Uuid,
        conversation: CommitReceipt,
    ) -> io::Result<WorkspaceOpenOutcome> {
        validate_ids(operation, selection)?;
        let mut record = load(self.data_dir(), self.snapshot().profile_id, operation)?
            .ok_or_else(|| invalid("Workspace operation does not exist"))?;
        record.expected_selection(selection)?;
        if let Stage::ConversationReady {
            conversation: original,
            ..
        } = &record.stage
        {
            return if *original == conversation {
                Ok(record.report(None))
            } else {
                Err(invalid(
                    "Workspace Conversation differs from its recorded launch",
                ))
            };
        }
        let Stage::SessionCreated {
            fingerprint,
            catalog,
        } = &record.stage
        else {
            return Err(invalid("Workspace Session has not been created"));
        };
        let id = record.descriptor.id;
        if conversation.journal_id != id || !valid_receipt(&conversation) {
            return Err(invalid(
                "Workspace Conversation receipt has an unrelated owner",
            ));
        }
        let session = RuntimeStore::inspect_session(self.data_dir(), id)?;
        if session.projection.descriptor != record.descriptor {
            return Err(invalid(
                "Workspace Session differs from its recorded creation",
            ));
        }
        let flow = read_registered_flow(
            &session.dir,
            &session.projection,
            &atman_rt::FlowInstanceId(id.into()),
            |_, _| Ok(()),
        )?;
        let canonical = flow
            .replay
            .activation
            .as_ref()
            .is_some_and(|(activation, receipt)| {
                receipt
                    .require_durable()
                    .is_ok_and(|receipt| receipt == &conversation)
                    && activation.input.is_canonical_session_start(id)
                    && activation.input.request.initial_input().is_none()
            });
        if !canonical {
            return Err(invalid(
                "Workspace Conversation differs from its recorded launch",
            ));
        }
        record.stage = Stage::ConversationReady {
            fingerprint: fingerprint.clone(),
            catalog: catalog.clone(),
            conversation,
        };
        save(self.data_dir(), &record)?;
        Ok(record.report(None))
    }
}

/// Inspect an exact operation without acquiring a writer or creating Profile storage.
pub async fn observe_workspace_open(
    data: PathBuf,
    operation: Uuid,
) -> io::Result<WorkspaceOpenOutcome> {
    if operation.is_nil() {
        return Err(invalid("Workspace operation identity must not be nil"));
    }
    crate::read_work::capture(move || {
        let profile = match ProfileStore::inspect_identity(&data) {
            Ok(profile) => profile,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(unseen(None)),
            Err(error) => return Err(error),
        };
        Ok(load(&data, profile, operation)?
            .map_or_else(|| unseen(None), |record| record.report(None)))
    })
    .await
    .map_err(io::Error::other)?
}

fn unseen(failure: Option<WorkspaceOpenFailure>) -> WorkspaceOpenOutcome {
    WorkspaceOpenOutcome {
        progress: WorkspaceProgress::Unseen,
        failure,
        bootstrap: None,
    }
}

impl From<BrowseError> for WorkspaceOpenFailure {
    fn from(error: BrowseError) -> Self {
        open_failure(error)
    }
}

fn open_failure(error: BrowseError) -> WorkspaceOpenFailure {
    match error {
        BrowseError::Changed | BrowseError::UnknownRoot => WorkspaceOpenFailure::DirectoryChanged,
        BrowseError::ReadOnlyRoot => WorkspaceOpenFailure::ReadOnlyRoot,
        _ => WorkspaceOpenFailure::DirectoryUnavailable,
    }
}

fn record_path(data: &Path, operation: Uuid) -> PathBuf {
    data.join(NAMESPACE)
        .join("host")
        .join("workspace-opens")
        .join(format!("{operation}.json"))
}

fn load(data: &Path, profile: Uuid, operation: Uuid) -> io::Result<Option<Record>> {
    let path = record_path(data, operation);
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut bytes = Vec::new();
    file.take(RECORD_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > RECORD_BYTES {
        return Err(invalid("Workspace operation exceeds its storage limit"));
    }
    let record: Record = serde_json::from_slice(&bytes).map_err(invalid)?;
    if record.version != 1
        || record.profile != profile
        || record.operation != operation
        || record.descriptor.id.is_nil()
        || record.descriptor.project.is_none()
    {
        return Err(invalid(
            "Workspace operation belongs to an unrelated Profile or Session",
        ));
    }
    validate_ids(operation, record.selection)?;
    record.descriptor.validate()?;
    let catalog = match &record.stage {
        Stage::SessionCreated { catalog, .. } | Stage::ConversationReady { catalog, .. } => {
            Some(catalog)
        }
        _ => None,
    };
    if catalog.is_some_and(|catalog| {
        catalog.journal_id != record.descriptor.id || !valid_receipt(catalog)
    }) || matches!(&record.stage, Stage::ConversationReady { conversation, .. }
            if conversation.journal_id != record.descriptor.id || !valid_receipt(conversation))
    {
        return Err(invalid("Workspace operation has an unrelated receipt"));
    }
    Ok(Some(record))
}

fn save(data: &Path, record: &Record) -> io::Result<()> {
    let path = record_path(data, record.operation);
    let bytes = serde_json::to_vec(record).map_err(invalid)?;
    if bytes.len() as u64 > RECORD_BYTES {
        return Err(invalid("Workspace operation exceeds its storage limit"));
    }
    ensure_dir(path.parent().expect("operation file has a directory"))?;
    crate::platform::atomic_replace(&path, &bytes)?;
    sync_parent(&path)
}

fn validate_ids(operation: Uuid, selection: Uuid) -> io::Result<()> {
    if operation.is_nil() || selection.is_nil() || operation == selection {
        return Err(invalid(
            "Workspace operation requires independent non-nil identities",
        ));
    }
    Ok(())
}

fn valid_receipt(receipt: &CommitReceipt) -> bool {
    receipt.sequence > 0
        && receipt.digest.len() == 64
        && receipt.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}
