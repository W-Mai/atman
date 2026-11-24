//! Inspect an immutable Fork publication using only its destination storage.

use std::{collections::HashSet, num::NonZeroUsize, path::Path};

use crate::{
    event_log::journal::{InspectionBudget, InspectionStep, JournalInspectionReader, ReadStamp},
    flow_store::{FlowFact, SessionInspection, SessionProjection},
    retained_file,
};

use super::*;

/// Destination evidence from the immutable, fully published snapshot.
pub struct InspectedForkImport {
    pub publication: ForkImportPublication,
    pub digest: blake3::Hash,
    pub(crate) encoded_bytes: usize,
    pub(crate) file: PathBuf,
    pub(crate) stamp: ReadStamp,
    pub(super) plan: ForkImportPlan,
}

impl InspectedForkImport {
    pub(crate) fn sources(&self) -> impl Iterator<Item = &FlowRef> {
        self.plan.sources()
    }
}

impl SessionInspection {
    /// Read the completed Fork and its destination proof without opening either execution owner.
    pub fn inspect_fork_import_publication(
        &self,
        operation: Uuid,
    ) -> io::Result<Option<InspectedForkImport>> {
        inspect(&self.dir, &self.projection, operation)
    }

    pub(crate) fn inspect_flow_fork_import(
        &self,
        flow: &FlowRef,
    ) -> io::Result<Option<InspectedForkImport>> {
        if self.projection.descriptor.id != flow.session_id.0 {
            return Err(invalid("Fork display belongs to another Session"));
        }
        let Some(registration) = self
            .projection
            .flows
            .iter()
            .find(|row| row.id == flow.instance_id)
        else {
            return Ok(None);
        };
        let value = self.inspect_fork_import_publication(registration.creation_request)?;
        if value
            .as_ref()
            .is_some_and(|value| &value.publication.destination != flow)
        {
            return Err(invalid("Fork publication belongs to another Flow"));
        }
        Ok(value)
    }
}

pub(super) fn inspect(
    directory: &Path,
    catalog: &SessionProjection,
    operation: Uuid,
) -> io::Result<Option<InspectedForkImport>> {
    inspect_at(
        directory,
        catalog.descriptor.id,
        operation,
        false,
        |destination| {
            catalog.flows.iter().any(|flow| {
                flow.id == destination.instance_id && flow.creation_request == operation
            })
        },
    )
}

/// The originating owner captured this native descriptor and the activation receipt.
pub(in crate::flow_store::source_capture) fn inspect_source_publication(
    flow_directory: &Path,
    flow: &FlowRef,
    operation: Uuid,
) -> io::Result<Option<InspectedForkImport>> {
    let directory = flow_directory
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| invalid("Fork source has no Session directory"))?;
    inspect_at(
        directory,
        flow.session_id.0,
        operation,
        true,
        |destination| destination == flow,
    )
}

fn inspect_at(
    directory: &Path,
    session: Uuid,
    operation: Uuid,
    require_publication: bool,
    registered: impl FnOnce(&FlowRef) -> bool,
) -> io::Result<Option<InspectedForkImport>> {
    if operation.is_nil() {
        return Ok(None);
    }
    let Some(manifest) = read_manifest(directory, session, operation)? else {
        return Ok(None);
    };
    let pending = || {
        if require_publication {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "Inherited Fork publication is unavailable",
            ))
        } else {
            Ok(None)
        }
    };
    let ForkImportState::Published {
        destination_receipt,
    } = &manifest.state
    else {
        return pending();
    };
    let path = import_directory(directory, operation)?.join("publication.json");
    let stamp = match ReadStamp::open(&path) {
        Ok(stamp) => stamp,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return pending(),
        Err(error) => return Err(error),
    };
    let Some(bytes) = read_existing(&path, PUBLICATION_LIMIT)? else {
        return pending();
    };
    let publication: ForkImportPublication = serde_json::from_slice(&bytes).map_err(invalid)?;
    publication.validate()?;
    manifest.plan.validate_bound()?;
    if publication.operation != operation
        || publication.destination != manifest.plan.destination
        || &publication.destination_receipt != destination_receipt
        || !manifest
            .plan
            .matches_publication(&publication.snapshot_digest, &publication.assets_digest)
        || publication.asset_bytes > manifest.plan.asset_bytes
        || !registered(&publication.destination)
    {
        return Err(invalid(
            "Fork publication does not match its destination manifest",
        ));
    }
    manifest.plan.validate_record_sources(
        publication
            .history
            .records
            .iter()
            .map(|record| &record.source.flow),
    )?;
    validate_logs(&publication, &manifest.plan)?;
    verify_activation(
        directory,
        &publication,
        manifest.plan.schema == seed::SCHEMA,
    )?;
    if stamp.length() != bytes.len() as u64 || ReadStamp::open(&path)? != stamp {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Fork publication changed during inspection",
        ));
    }
    Ok(Some(InspectedForkImport {
        publication,
        digest: blake3::hash(&bytes),
        encoded_bytes: bytes.len(),
        file: path,
        stamp,
        plan: manifest.plan,
    }))
}

pub(super) fn validate_logs(
    publication: &ForkImportPublication,
    plan: &ForkImportPlan,
) -> io::Result<()> {
    let mut handles = HashSet::new();
    let mut remaining = publication.asset_bytes;
    for log in &publication.logs {
        if log.origin(plan.sources()).is_none()
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
    publication: &ForkImportPublication,
    seeded: bool,
) -> io::Result<()> {
    let id = &publication.destination.instance_id;
    let file = directory
        .join("flows")
        .join(id.to_string())
        .join("events.jsonl");
    let expected = &publication.destination_receipt;
    let mut scanner = JournalInspectionReader::<FlowFact>::open(&file, id.0.0)?;
    let mut checked = 0_u64;
    let mut created = false;
    let mut waiting = false;
    let maximum = NonZeroUsize::new(if seeded {
        PUBLICATION_LIMIT
    } else {
        8 * 1024 * 1024
    })
    .unwrap();
    loop {
        let batches = NonZeroUsize::new((expected.sequence - checked).min(64) as usize)
            .ok_or_else(|| invalid("Fork destination activation is missing"))?;
        let budget = InspectionBudget {
            max_batches: batches,
            work_bytes: maximum,
            max_record_bytes: if seeded {
                maximum
            } else {
                NonZeroUsize::new(4 * 1024 * 1024).unwrap()
            },
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
                        && matches!(entry.input.request.start, EntryStart::Waiting)
                        && (!seeded || entry.input.inherited_context == publication.model_context)));
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
