//! Inspect an immutable Fork publication using only its destination storage.

use std::{collections::HashSet, num::NonZeroUsize, path::Path};

use crate::{
    event_log::journal::{InspectionBudget, InspectionStep, JournalInspectionReader},
    flow_store::{FlowFact, SessionInspection, SessionProjection},
    retained_file,
    tools::bash_bg::BgHandle,
};

use super::*;

/// Destination evidence from the immutable, fully published snapshot.
pub struct InspectedForkImport {
    pub publication: ForkImportPublication,
    pub digest: blake3::Hash,
}

impl SessionInspection {
    /// Read the completed Fork and its destination proof without opening either execution owner.
    pub fn inspect_fork_import_publication(
        &self,
        operation: Uuid,
    ) -> io::Result<Option<InspectedForkImport>> {
        inspect(&self.dir, &self.projection, operation)
    }
}

pub(super) fn inspect(
    directory: &Path,
    catalog: &SessionProjection,
    operation: Uuid,
) -> io::Result<Option<InspectedForkImport>> {
    let Some(manifest) = read_manifest(directory, catalog.descriptor.id, operation)? else {
        return Ok(None);
    };
    let ForkImportState::Published {
        destination_receipt,
    } = &manifest.state
    else {
        return Ok(None);
    };
    let path = import_directory(directory, operation)?.join("publication.json");
    let Some(bytes) = read_existing(&path, PUBLICATION_LIMIT)? else {
        return Ok(None);
    };
    let publication: ForkImportPublication = serde_json::from_slice(&bytes).map_err(invalid)?;
    publication.validate()?;
    manifest.plan.validate_bound()?;
    if publication.operation != operation
        || publication.destination != manifest.plan.destination
        || &publication.destination_receipt != destination_receipt
        || publication.snapshot_digest != manifest.plan.snapshot_digest
        || publication.asset_bytes > manifest.plan.asset_bytes
        || publication
            .history
            .records
            .iter()
            .any(|record| record.source.flow != manifest.plan.source.flow)
    {
        return Err(invalid(
            "Fork publication does not match its destination manifest",
        ));
    }
    validate_logs(&publication, &manifest.plan.source.flow)?;
    verify_activation(directory, catalog, &publication)?;
    Ok(Some(InspectedForkImport {
        publication,
        digest: blake3::hash(&bytes),
    }))
}

pub(super) fn validate_logs(
    publication: &ForkImportPublication,
    source: &FlowRef,
) -> io::Result<()> {
    let session = source.session_id.to_string();
    let mut handles = HashSet::new();
    let mut remaining = publication.asset_bytes;
    for log in &publication.logs {
        if BgHandle::managed_log_name(&log.handle, &session).is_none()
            || blake3::Hash::from_hex(&log.digest).is_err()
            || !handles.insert(&log.handle)
        {
            return Err(invalid("Fork has an invalid retained log identity"));
        }
        remaining = remaining
            .checked_sub(log.bytes)
            .ok_or_else(|| invalid("Fork retained logs exceed the publication size"))?;
    }
    Ok(())
}

fn verify_activation(
    directory: &Path,
    catalog: &SessionProjection,
    publication: &ForkImportPublication,
) -> io::Result<()> {
    let id = &publication.destination.instance_id;
    if !catalog
        .flows
        .iter()
        .any(|flow| &flow.id == id && flow.creation_request == publication.operation)
    {
        return Err(invalid(
            "Fork destination is not registered for this operation",
        ));
    }
    let file = directory
        .join("flows")
        .join(id.to_string())
        .join("events.jsonl");
    let expected = &publication.destination_receipt;
    let mut scanner = JournalInspectionReader::<FlowFact>::open(&file, id.0.0)?;
    let mut checked = 0_u64;
    let mut created = false;
    let mut waiting = false;
    loop {
        let batches = NonZeroUsize::new((expected.sequence - checked).min(64) as usize)
            .ok_or_else(|| invalid("Fork destination activation is missing"))?;
        let budget = InspectionBudget {
            max_batches: batches,
            work_bytes: NonZeroUsize::new(8 * 1024 * 1024).unwrap(),
            max_record_bytes: NonZeroUsize::new(4 * 1024 * 1024).unwrap(),
        };
        let step = scanner.step(budget, |receipt, facts| {
            checked = receipt.sequence;
            if receipt.sequence == 1 {
                created = facts.iter().any(|fact| matches!(fact, FlowFact::Created(descriptor)
                    if descriptor.address == publication.destination && descriptor.creation_request == publication.operation));
            }
            if checked == expected.sequence {
                waiting = &receipt == expected && facts.iter().any(|fact| matches!(fact, FlowFact::Activated(entry)
                    if entry.input.request.recipient == publication.destination
                        && entry.input.request.request_id == publication.operation
                        && matches!(entry.input.request.start, EntryStart::Waiting)));
            }
            Ok(())
        })?;
        if created && waiting {
            return Ok(());
        }
        if !matches!(step, InspectionStep::Loading(_)) {
            return Err(invalid(
                "Fork destination has no matching Waiting activation",
            ));
        }
    }
}

pub(super) fn read_manifest(
    directory: &Path,
    session: Uuid,
    operation: Uuid,
) -> io::Result<Option<ForkImportManifest>> {
    let path = import_directory(directory, operation)?.join("manifest.json");
    let Some(bytes) = read_existing(&path, MANIFEST_LIMIT)? else {
        return Ok(None);
    };
    let manifest: ForkImportManifest = serde_json::from_slice(&bytes).map_err(invalid)?;
    manifest.plan.validate()?;
    if manifest.plan.operation != operation || manifest.plan.destination.session_id.0 != session {
        return Err(invalid("Fork manifest does not match its destination path"));
    }
    Ok(Some(manifest))
}

fn import_directory(directory: &Path, operation: Uuid) -> io::Result<PathBuf> {
    if operation.is_nil() {
        return Err(invalid("Fork import operation identity is empty"));
    }
    Ok(directory.join("fork-imports").join(operation.to_string()))
}

fn read_existing(path: &Path, limit: usize) -> io::Result<Option<Vec<u8>>> {
    match retained_file::read_bounded(path, limit) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}
