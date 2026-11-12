//! Durable, restart-safe admission metadata for a native Fork destination.
//!
//! The manifest records a reviewed source boundary and destination identity before a
//! destination Flow is created. It is deliberately not a Flow journal, execution lease,
//! approval grant, or LegacyArchive replacement.

use std::{io, path::PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{ForkSnapshot, ResolvedForkBoundary, SourceSelection};
use crate::{
    event_log::journal::CommitReceipt,
    flow_store::{FlowRef, SessionStore, invalid},
    storage::ProjectContext,
};

const MANIFEST_LIMIT: usize = 256 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ForkImportPlan {
    pub schema: u32,
    pub operation: Uuid,
    pub source: SourceSelection,
    pub boundary: ResolvedForkBoundary,
    pub destination: FlowRef,
    pub project: Option<ProjectContext>,
    pub working_directory: PathBuf,
    pub snapshot_digest: String,
    pub asset_bytes: usize,
}

impl ForkImportPlan {
    pub fn from_snapshot(
        operation: Uuid,
        destination: FlowRef,
        project: Option<ProjectContext>,
        working_directory: PathBuf,
        snapshot: &ForkSnapshot,
        asset_bytes: usize,
    ) -> io::Result<Self> {
        let snapshot_digest = blake3::hash(&serde_json::to_vec(snapshot).map_err(invalid)?)
            .to_hex()
            .to_string();
        let plan = Self {
            schema: 1,
            operation,
            source: snapshot.boundary.observed.clone(),
            boundary: snapshot.boundary.clone(),
            destination,
            project,
            working_directory,
            snapshot_digest,
            asset_bytes,
        };
        plan.validate()?;
        Ok(plan)
    }

    pub fn validate(&self) -> io::Result<()> {
        if self.schema != 1
            || self.operation.is_nil()
            || self.source.flow.session_id.0.is_nil()
            || self.source.flow.instance_id.0.0.is_nil()
            || self.source.cursor.sequence == 0
            || self.destination.session_id.0.is_nil()
            || self.destination.instance_id.0.0.is_nil()
            || self.destination.session_id == self.source.flow.session_id
            || self.boundary.observed != self.source
            || self.boundary.source.flow != self.source.flow
            || self.snapshot_digest.len() != 64
            || !self
                .snapshot_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || !self.working_directory.is_absolute()
            || self
                .working_directory
                .components()
                .any(|component| matches!(component, std::path::Component::ParentDir))
        {
            return Err(invalid("invalid native Fork import plan"));
        }
        if let Some(project) = &self.project {
            project.validate().map_err(invalid)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub enum ForkImportState {
    Prepared,
    Published { destination_receipt: CommitReceipt },
    Abandoned,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ForkImportManifest {
    pub plan: ForkImportPlan,
    pub state: ForkImportState,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ForkImportReceipt {
    pub operation: Uuid,
    pub destination: FlowRef,
    pub manifest_digest: String,
    pub state: ForkImportState,
}

impl ForkImportManifest {
    fn receipt(&self, bytes: &[u8]) -> ForkImportReceipt {
        ForkImportReceipt {
            operation: self.plan.operation,
            destination: self.plan.destination.clone(),
            manifest_digest: blake3::hash(bytes).to_hex().to_string(),
            state: self.state.clone(),
        }
    }
}

impl SessionStore {
    /// Persist a reviewed Fork destination identity before any Flow execution is admitted.
    /// Repeating the same operation is idempotent; a changed plan is rejected.
    pub fn prepare_fork_import(&self, plan: ForkImportPlan) -> io::Result<ForkImportReceipt> {
        plan.validate()?;
        if plan.destination.session_id.0 != self.projection.descriptor.id {
            return Err(invalid(
                "Fork import destination belongs to another Session",
            ));
        }
        if self
            .projection
            .flows
            .iter()
            .any(|flow| flow.id == plan.destination.instance_id)
        {
            return Err(invalid("Fork import destination Flow already exists"));
        }
        let manifest = ForkImportManifest {
            plan,
            state: ForkImportState::Prepared,
        };
        let bytes = serde_json::to_vec(&manifest).map_err(invalid)?;
        if bytes.len() > MANIFEST_LIMIT {
            return Err(invalid("Fork import manifest exceeds its metadata limit"));
        }
        let path = self
            .dir
            .join("fork-imports")
            .join(manifest.plan.operation.to_string())
            .join("manifest.json");
        crate::retained_file::publish(&path, &bytes)?;
        let stored = crate::retained_file::read_bounded(&path, MANIFEST_LIMIT)?;
        let stored: ForkImportManifest = serde_json::from_slice(&stored).map_err(invalid)?;
        if stored != manifest {
            return Err(invalid("Fork import manifest changed during publication"));
        }
        Ok(stored.receipt(&bytes))
    }

    pub fn inspect_fork_import(&self, operation: Uuid) -> io::Result<Option<ForkImportManifest>> {
        if operation.is_nil() {
            return Err(invalid("Fork import operation identity is empty"));
        }
        let path = self
            .dir
            .join("fork-imports")
            .join(operation.to_string())
            .join("manifest.json");
        let bytes = match crate::retained_file::read_bounded(&path, MANIFEST_LIMIT) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let manifest: ForkImportManifest = serde_json::from_slice(&bytes).map_err(invalid)?;
        if manifest.plan.operation != operation {
            return Err(invalid(
                "Fork import manifest operation does not match its path",
            ));
        }
        manifest.plan.validate()?;
        Ok(Some(manifest))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_store::{FlowInstanceId, RuntimeStore, SessionDescriptor};

    fn plan(destination_session: Uuid) -> ForkImportPlan {
        let source = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let cursor = CommitReceipt {
            journal_id: source.instance_id.0.0,
            sequence: 2,
            digest: "source-cursor".into(),
        };
        let observed = SourceSelection {
            flow: source.clone(),
            branch: super::super::ConversationBranchId::initial(&source, cursor.journal_id),
            cursor: cursor.clone(),
        };
        let boundary = ResolvedForkBoundary {
            observed: observed.clone(),
            head: crate::flow_store::branches::HistoryHead::initial(&source, cursor.journal_id),
            source: observed.clone(),
            boundary: None,
            excluded_turns: 0,
        };
        ForkImportPlan {
            schema: 1,
            operation: Uuid::now_v7(),
            source: observed,
            boundary,
            destination: FlowRef {
                session_id: destination_session.into(),
                instance_id: FlowInstanceId::now(),
            },
            project: None,
            working_directory: std::env::current_dir().unwrap(),
            snapshot_digest: "a".repeat(64),
            asset_bytes: 12,
        }
    }

    #[test]
    fn prepare_is_idempotent_and_rejects_changed_target_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let runtime = RuntimeStore::open(root.path()).unwrap();
        let destination_id = Uuid::now_v7();
        let session = runtime
            .create_session(SessionDescriptor {
                id: destination_id,
                project: None,
                configuration: Default::default(),
                title: "Fork target".into(),
            })
            .unwrap();
        let plan = plan(destination_id);
        let receipt = session.prepare_fork_import(plan.clone()).unwrap();
        let retry = session.prepare_fork_import(plan.clone()).unwrap();
        assert_eq!(retry, receipt);
        drop(session);
        let session = runtime.open_session(destination_id).unwrap();
        assert_eq!(
            session
                .inspect_fork_import(plan.operation)
                .unwrap()
                .unwrap()
                .plan,
            plan
        );
        let mut changed = plan;
        changed.destination.instance_id = FlowInstanceId::now();
        assert!(session.prepare_fork_import(changed).is_err());
    }

    #[test]
    fn import_plan_rejects_same_session_and_unsafe_working_directory() {
        let session = Uuid::now_v7();
        let mut same_session = plan(session);
        same_session.destination.session_id = same_session.source.flow.session_id;
        assert!(same_session.validate().is_err());
        let mut unsafe_directory = plan(Uuid::now_v7());
        unsafe_directory.working_directory = PathBuf::from("relative");
        assert!(unsafe_directory.validate().is_err());
    }
}
