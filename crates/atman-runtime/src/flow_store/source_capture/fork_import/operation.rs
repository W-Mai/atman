//! Durable acceptance and reconciliation of a Fork command across host ownership changes.

use std::path::Path;

use super::*;
use crate::{
    file_lock::FileLock,
    flow_store::{
        NAMESPACE, RuntimeStore, SessionDescriptor, ensure_dir, local_operator::LocalOperator,
    },
};

#[derive(Clone)]
pub struct ForkOperation {
    data: PathBuf,
    operator: Uuid,
    operation: Uuid,
}

#[derive(Clone, Debug, Eq, PartialEq, Deserialize, Serialize)]
pub struct ForkConfirmation {
    pub request_digest: String,
    pub source: FlowRef,
    pub descriptor: SessionDescriptor,
    pub working_directory: PathBuf,
}

pub enum ForkOperationStatus {
    Unseen,
    Preparing(ForkConfirmation),
    RetryRequired(ForkConfirmation),
    Ready {
        confirmation: ForkConfirmation,
        activation: CommitReceipt,
        publication_digest: blake3::Hash,
    },
}

pub struct ForkOperationGuard {
    operation: ForkOperation,
    _owner: FileLock,
}

impl ForkOperation {
    pub fn new(data: &Path, operator: LocalOperator, operation: Uuid) -> io::Result<Self> {
        if operator.id().is_nil() || operation.is_nil() {
            return Err(invalid(
                "Fork command requires authenticated operation identity",
            ));
        }
        let operator = if operator.owns(&LocalOperator::NATIVE.sender()) {
            LocalOperator::NATIVE.id()
        } else {
            operator.id()
        };
        Ok(Self {
            data: data.to_owned(),
            operator,
            operation,
        })
    }

    pub fn destination(&self) -> FlowRef {
        let id = |kind: &[u8]| {
            let mut hash = blake3::Hasher::new();
            hash.update(b"atman.fork-destination\0");
            hash.update(self.operator.as_bytes());
            hash.update(self.operation.as_bytes());
            hash.update(kind);
            uuid::Builder::from_custom_bytes(
                hash.finalize().as_bytes()[..16]
                    .try_into()
                    .expect("digest prefix"),
            )
            .into_uuid()
        };
        FlowRef {
            session_id: id(b"session").into(),
            instance_id: atman_rt::FlowInstanceId(id(b"flow").into()),
        }
    }

    pub fn sender(&self) -> crate::flow_store::inbox::SenderIdentity {
        LocalOperator::new(self.operator).sender()
    }

    fn directory(&self) -> PathBuf {
        self.data
            .join(NAMESPACE)
            .join("host")
            .join("fork-operations")
            .join(self.operator.to_string())
            .join(self.operation.to_string())
    }

    fn read(&self) -> io::Result<ForkOperationStatus> {
        let bytes = match crate::retained_file::read_bounded(
            &self.directory().join("confirmation.json"),
            MANIFEST_LIMIT,
        ) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ForkOperationStatus::Unseen);
            }
            Err(error) => return Err(error),
        };
        let confirmation: ForkConfirmation = serde_json::from_slice(&bytes).map_err(invalid)?;
        self.validate(&confirmation)?;
        let destination = self.destination();
        let session = match RuntimeStore::inspect_session(&self.data, destination.session_id.0) {
            Ok(session) => session,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return self.pending(confirmation);
            }
            Err(error) => return Err(error),
        };
        if session.projection.descriptor != confirmation.descriptor {
            return Err(invalid(
                "Fork target was created with a different Session descriptor",
            ));
        }
        let Some(import) = session.inspect_fork_import_publication(self.operation)? else {
            return self.pending(confirmation);
        };
        if import.plan.schema != seed::SCHEMA
            || import.plan.source.flow != confirmation.source
            || import.plan.project != confirmation.descriptor.project
            || import.plan.working_directory != confirmation.working_directory
            || import.publication.destination != destination
        {
            return Err(invalid(
                "Fork publication conflicts with its accepted command",
            ));
        }
        Ok(ForkOperationStatus::Ready {
            confirmation,
            activation: import.publication.destination_receipt,
            publication_digest: import.digest,
        })
    }

    fn pending(&self, confirmation: ForkConfirmation) -> io::Result<ForkOperationStatus> {
        let file = match fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(self.directory().join("owner.lock"))
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(ForkOperationStatus::RetryRequired(confirmation));
            }
            Err(error) => return Err(error),
        };
        match FileLock::try_exclusive(file) {
            Ok(_idle) => Ok(ForkOperationStatus::RetryRequired(confirmation)),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                Ok(ForkOperationStatus::Preparing(confirmation))
            }
            Err(error) => Err(error),
        }
    }

    pub async fn observe(&self) -> io::Result<ForkOperationStatus> {
        let operation = self.clone();
        crate::read_work::capture(move || operation.read())
            .await
            .map_err(io::Error::other)?
    }

    pub async fn try_lock(&self) -> io::Result<ForkOperationGuard> {
        let operation = self.clone();
        crate::read_work::coordinate(move || {
            let directory = operation.directory();
            ensure_dir(&directory)?;
            let file = crate::event_log::journal::private_file_options()
                .create(true)
                .read(true)
                .write(true)
                .open(directory.join("owner.lock"))?;
            Ok(ForkOperationGuard {
                operation,
                _owner: FileLock::try_exclusive(file)?,
            })
        })
        .await
        .map_err(io::Error::other)?
    }

    fn validate(&self, confirmation: &ForkConfirmation) -> io::Result<()> {
        confirmation.descriptor.validate()?;
        if confirmation.descriptor.id != self.destination().session_id.0
            || confirmation.source.session_id.0.is_nil()
            || confirmation.source.instance_id.0.0.is_nil()
            || confirmation.source.session_id.0 == confirmation.descriptor.id
            || blake3::Hash::from_hex(&confirmation.request_digest).is_err()
            || !confirmation.working_directory.is_absolute()
            || confirmation
                .working_directory
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir))
        {
            return Err(invalid("invalid durable Fork confirmation"));
        }
        Ok(())
    }
}

impl ForkOperationGuard {
    /// Persist the accepted command before creating a destination Session.
    pub async fn accept(self, confirmation: ForkConfirmation) -> io::Result<Self> {
        tokio::task::spawn_blocking(move || {
            self.operation.validate(&confirmation)?;
            let bytes = serde_json::to_vec(&confirmation).map_err(invalid)?;
            if bytes.len() > MANIFEST_LIMIT {
                return Err(invalid("Fork confirmation exceeds its storage limit"));
            }
            crate::retained_file::publish(
                &self.operation.directory().join("confirmation.json"),
                &bytes,
            )?;
            Ok(self)
        })
        .await
        .map_err(io::Error::other)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn accepted_fork_recovers_same_destination_after_its_owner_disappears() {
        let data = tempfile::tempdir().unwrap();
        let request = Uuid::now_v7();
        let operation =
            ForkOperation::new(data.path(), LocalOperator::LEGACY_RPC, request).unwrap();
        let recovered = ForkOperation::new(data.path(), LocalOperator::NATIVE, request).unwrap();
        assert_eq!(operation.destination(), recovered.destination());
        assert!(matches!(
            operation.observe().await.unwrap(),
            ForkOperationStatus::Unseen
        ));
        assert!(data.path().read_dir().unwrap().next().is_none());
        let confirmation = ForkConfirmation {
            request_digest: "ab".repeat(32),
            source: FlowRef {
                session_id: Uuid::now_v7().into(),
                instance_id: crate::flow_store::FlowInstanceId::now(),
            },
            descriptor: SessionDescriptor {
                id: operation.destination().session_id.0,
                project: None,
                configuration: Default::default(),
                title: "Recovered Fork".into(),
            },
            working_directory: data.path().canonicalize().unwrap(),
        };
        let owner = operation
            .try_lock()
            .await
            .unwrap()
            .accept(confirmation.clone())
            .await
            .unwrap();
        assert_eq!(
            recovered.try_lock().await.err().unwrap().kind(),
            io::ErrorKind::WouldBlock
        );
        assert!(
            matches!(recovered.observe().await.unwrap(), ForkOperationStatus::Preparing(value) if value == confirmation)
        );
        drop(owner);
        assert!(
            matches!(recovered.observe().await.unwrap(), ForkOperationStatus::RetryRequired(value) if value == confirmation)
        );
        let mut altered = confirmation.clone();
        altered.descriptor.title = "Changed request".into();
        assert!(
            recovered
                .try_lock()
                .await
                .unwrap()
                .accept(altered)
                .await
                .is_err()
        );
        assert!(
            matches!(recovered.observe().await.unwrap(), ForkOperationStatus::RetryRequired(value) if value == confirmation)
        );
    }
}
