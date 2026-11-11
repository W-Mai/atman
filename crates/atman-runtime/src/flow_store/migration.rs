//! Offline, resumable publication of legacy history into the instance namespace.

use std::collections::{BTreeMap, HashSet};
use std::fs::{self, File};
use std::io::{self, Read};
use std::num::NonZeroUsize;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{
    FlowContent, FlowDescriptor, FlowInstanceId, FlowRef, RuntimeStore, SessionDescriptor,
    SessionFact, SessionOwner, SessionProjection, SessionStore, ensure_dir,
    history::{LegacyArchive, LegacyHistoryRecord},
    invalid, write_json,
};
use crate::event_log::{
    journal::{CommitReceipt, private_file_options, sync_parent},
    legacy::{self, LegacyFlowKey, LegacyInspection, LegacySource},
};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MigratedFlow {
    pub owner: LegacyFlowKey,
    pub instance: FlowInstanceId,
    pub records: u64,
    pub cursor: CommitReceipt,
}

/// Durable provenance and exact committed positions of a completed history import.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LegacyMigration {
    pub session_id: Uuid,
    pub source: LegacySource,
    pub records: u64,
    pub blank_lines: u64,
    pub flows: Vec<MigratedFlow>,
}

impl LegacyMigration {
    pub(super) fn validate(&self, session: &SessionProjection) -> io::Result<()> {
        if session.migrated_from.is_some() || self.session_id == session.descriptor.id {
            return Err(invalid(
                "duplicate migration or reused legacy session identity",
            ));
        }
        LegacyArchive {
            session_id: self.session_id,
            source: self.source.clone(),
            owner: LegacyFlowKey::Default,
            records: self.records,
        }
        .validate()?;
        let mut ids = HashSet::new();
        let mut owners = std::collections::BTreeSet::new();
        let mut count = 0_u64;
        for flow in &self.flows {
            if !ids.insert(&flow.instance)
                || !owners.insert(&flow.owner)
                || !session
                    .flows
                    .iter()
                    .any(|registered| registered.id == flow.instance)
                || flow.cursor.journal_id != flow.instance.0.0
                || flow.cursor.sequence == 0
            {
                return Err(invalid("invalid migrated flow registration or receipt"));
            }
            count = count
                .checked_add(flow.records)
                .ok_or_else(|| invalid("record count overflow"))?;
            if flow.owner == LegacyFlowKey::Default
                && session.default_flow.as_ref() != Some(&flow.instance)
            {
                return Err(invalid("migration default flow mismatch"));
            }
        }
        if count != self.records
            || !owners.contains(&LegacyFlowKey::Default)
            || ids.len() != session.flows.len()
        {
            return Err(invalid("migration does not cover the session inventory"));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct MigrationPlan {
    version: u32,
    source_session: Uuid,
    source: LegacySource,
    destination: SessionDescriptor,
}

/// Stable destination for retries. Source edits retain this identity and are rejected as conflicts.
pub fn destination_id(source: &Path, source_session: Uuid) -> io::Result<Uuid> {
    stable_id(
        "atman-legacy-session",
        &(source.canonicalize()?, source_session),
    )
}

fn stable_id(domain: &str, value: &impl Serialize) -> io::Result<Uuid> {
    let mut hash = Sha256::new();
    hash.update(domain.as_bytes());
    hash.update([0]);
    hash.update(serde_json::to_vec(value).map_err(invalid)?);
    let bytes: [u8; 16] = hash.finalize()[..16].try_into().expect("digest prefix");
    Ok(uuid::Builder::from_custom_bytes(bytes).into_uuid())
}

impl RuntimeStore {
    /// Import a stopped legacy session without modifying its files or running any flow.
    ///
    /// The host must authenticate the operator and obtain explicit confirmation that the old executor stopped. The confirmation is not an OS lock over old binaries. Changed source bytes require a different destination selected by the operator; retries never silently import a branch.
    pub fn archive_legacy(
        &self,
        source: &Path,
        source_session: Uuid,
        destination: SessionDescriptor,
        old_executor_stopped: bool,
    ) -> io::Result<LegacyMigration> {
        if !old_executor_stopped {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "confirm the legacy executor has stopped before migration",
            ));
        }
        destination.validate()?;
        if destination.id == source_session {
            return Err(invalid("migration requires a new session identity"));
        }
        self.ensure_session_available(destination.id)?;
        let inspection = legacy::inspect(source, source_session)?;
        let plan = MigrationPlan {
            version: 1,
            source_session,
            source: inspection.source.clone(),
            destination,
        };
        let migration_dir = self
            .root
            .join(".migrations")
            .join(plan.destination.id.to_string());
        ensure_dir(&migration_dir)?;
        let _migration_lock = SessionOwner::lock(&migration_dir)?;
        let manifest = migration_dir.join("plan.json");
        match fs::read(&manifest) {
            Ok(bytes) => {
                let previous: MigrationPlan = serde_json::from_slice(&bytes).map_err(invalid)?;
                if previous != plan {
                    return Err(invalid(
                        "migration source or destination changed; select a new destination for a separate branch",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => write_json(&manifest, &plan)?,
            Err(error) => return Err(error),
        }
        let published = self
            .root
            .join("sessions")
            .join(plan.destination.id.to_string());
        if published.try_exists()? {
            let _catalog = self.lock_catalog()?;
            return self.completed_migration(&plan);
        }
        let stage = RuntimeStore {
            root: migration_dir.join("work"),
        };
        ensure_dir(&stage.root.join("sessions"))?;
        let staged_path = stage
            .root
            .join("sessions")
            .join(plan.destination.id.to_string());
        let mut session = if staged_path.try_exists()? {
            stage.open_session(plan.destination.id)?
        } else {
            stage.create_session(plan.destination.clone())?
        };
        if session.projection.descriptor != plan.destination {
            return Err(invalid("staged session descriptor changed"));
        }
        let snapshot = session.dir.join("artifacts").join("legacy-events.jsonl");
        retain_source(&plan.source, &snapshot)?;
        let completed = import_records(&mut session, &inspection, &snapshot)?;
        // Do not acknowledge a source that changed while records were being retained.
        plan.source.verify()?;
        match &session.projection.migrated_from {
            Some(existing) if existing == &completed => {}
            Some(_) => return Err(invalid("staged migration receipt changed")),
            None => {
                session.commit(vec![SessionFact::LegacyImported(completed.clone())])?;
            }
        }
        drop(session);
        // All journal handles are closed before rename, including on Windows.
        let _catalog = self.lock_catalog()?;
        self.ensure_session_available(plan.destination.id)?;
        if published.try_exists()? {
            return self.completed_migration(&plan);
        }
        plan.source.verify()?;
        fs::rename(&staged_path, &published)?;
        sync_parent(&published)?;
        sync_parent(&staged_path)?;
        Ok(completed)
    }

    fn completed_migration(&self, plan: &MigrationPlan) -> io::Result<LegacyMigration> {
        let session = self.open_session_locked(plan.destination.id)?;
        let receipt =
            session.projection.migrated_from.as_ref().ok_or_else(|| {
                invalid("destination exists without matching migration provenance")
            })?;
        if session.projection.descriptor != plan.destination
            || receipt.session_id != plan.source_session
            || receipt.source != plan.source
        {
            return Err(invalid("destination belongs to another migration"));
        }
        LegacySource {
            path: session.dir.join("artifacts/legacy-events.jsonl"),
            ..plan.source.clone()
        }
        .verify()?;
        for item in &receipt.flows {
            let flow = session.open_flow(&item.instance)?;
            let expected = LegacyArchive {
                session_id: plan.source_session,
                source: plan.source.clone(),
                owner: item.owner.clone(),
                records: item.records,
            };
            if flow.descriptor.content != FlowContent::History(expected)
                || flow.cursor().require_durable()? != &item.cursor
                || flow.history_progress().records != item.records
            {
                return Err(invalid(
                    "published history does not match its migration receipt",
                ));
            }
        }
        plan.source.verify()?;
        Ok(receipt.clone())
    }
}

fn retain_source(source: &LegacySource, snapshot: &Path) -> io::Result<()> {
    let retained = LegacySource {
        path: snapshot.to_path_buf(),
        ..source.clone()
    };
    if snapshot.try_exists()? {
        return retained.verify();
    }
    let temporary = snapshot.with_extension(format!("{}.tmp", Uuid::now_v7()));
    let result = (|| {
        let mut input = File::open(&source.path)?.take(source.bytes);
        let mut output = private_file_options()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        if io::copy(&mut input, &mut output)? != source.bytes {
            return Err(invalid("source changed while copying history"));
        }
        output.sync_all()?;
        drop(output);
        LegacySource {
            path: temporary.clone(),
            ..source.clone()
        }
        .verify()?;
        fs::rename(&temporary, snapshot)?;
        sync_parent(snapshot)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result
}

fn import_records(
    session: &mut SessionStore,
    inspection: &LegacyInspection,
    snapshot: &Path,
) -> io::Result<LegacyMigration> {
    let groups = inspection.grouped();
    let destination = session.projection.descriptor.id;
    let mut file = File::open(snapshot)?;
    let mut completed = LegacyMigration {
        session_id: inspection.session_id,
        source: inspection.source.clone(),
        records: inspection.records.len() as u64,
        blank_lines: inspection.blank_lines,
        flows: Vec::new(),
    };
    let mut retained_outputs = BTreeMap::new();
    for (owner, records) in groups {
        let instance =
            FlowInstanceId::new(stable_id("atman-legacy-flow", &(destination, &owner))?.into());
        let mut flow = session.create_flow(FlowDescriptor {
            address: FlowRef {
                session_id: destination.into(),
                instance_id: instance.clone(),
            },
            creation_request: stable_id("atman-legacy-creation", &(destination, &owner))?,
            // Original lineage remains in retained records; uncertain relationships confer no authority.
            created_by: None,
            content: FlowContent::History(LegacyArchive {
                session_id: inspection.session_id,
                source: inspection.source.clone(),
                owner: owner.clone(),
                records: records.len() as u64,
            }),
        })?;
        let consumed = usize::try_from(flow.history_progress().records).map_err(invalid)?;
        if consumed > records.len()
            || consumed > 0
                && records.get(consumed - 1).is_none_or(|record| {
                    record.line != flow.history_progress().last_line
                        || record.offset + record.bytes != flow.history_progress().end_offset
                })
        {
            return Err(invalid(
                "staged history prefix does not match the source map",
            ));
        }
        for batch in records[consumed..].chunks(64) {
            let mut output = Vec::with_capacity(batch.len());
            for record in batch {
                let bytes = record.read(&mut file, inspection.source.bytes)?;
                let mut retained = LegacyHistoryRecord::new((*record).clone(), &bytes);
                retained.retain_images(
                    inspection
                        .source
                        .path
                        .parent()
                        .ok_or_else(|| invalid("source has no session directory"))?,
                )?;
                retained.retain_outputs(
                    inspection
                        .source
                        .path
                        .parent()
                        .ok_or_else(|| invalid("source has no session directory"))?,
                )?;
                output.push(retained);
            }
            flow.append_legacy_history(output)?;
        }
        // Validate every retained index, including batches committed before a retry.
        let mut cursor = None;
        let mut verified = 0;
        loop {
            let page = flow.read_updates(cursor.as_ref(), NonZeroUsize::new(16).unwrap())?;
            for fact in page.batches.into_iter().flat_map(|batch| batch.facts) {
                if let super::view::FlowUpdate::LegacyRecords(batch) = fact {
                    for record in batch {
                        if records.get(verified).copied() != Some(&record.source) {
                            return Err(invalid("retained history differs from the source map"));
                        }
                        for (id, content) in &record.outputs {
                            let digest = Sha256::digest(content.as_bytes());
                            if retained_outputs
                                .insert(id.clone(), digest)
                                .is_some_and(|old| old != digest)
                            {
                                return Err(invalid(
                                    "registered output changed across retained history",
                                ));
                            }
                        }
                        verified += 1;
                    }
                }
            }
            cursor = page.next;
            if !page.has_more {
                break;
            }
        }
        if verified != records.len() {
            return Err(invalid("retained history is incomplete"));
        }
        completed.flows.push(MigratedFlow {
            owner,
            instance,
            records: records.len() as u64,
            cursor: flow.cursor().require_durable()?.clone(),
        });
    }
    Ok(completed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_resumes_committed_batches_and_publishes_only_complete_history() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("source.jsonl");
        let raw = b"retained invalid record\n".repeat(150);
        fs::write(&path, &raw).unwrap();
        let source = Uuid::now_v7();
        let inspection = legacy::inspect(&path, source).unwrap();
        let root = RuntimeStore::open(temp.path()).unwrap();
        let destination = SessionDescriptor {
            id: destination_id(&path, source).unwrap(),
            project: None,
            configuration: Default::default(),
            title: "Resume".into(),
        };
        let migration_dir = root
            .root
            .join(".migrations")
            .join(destination.id.to_string());
        let stage = RuntimeStore {
            root: migration_dir.join("work"),
        };
        ensure_dir(&stage.root.join("sessions")).unwrap();
        let mut session = stage.create_session(destination.clone()).unwrap();
        let snapshot = session.dir.join("artifacts/legacy-events.jsonl");
        retain_source(&inspection.source, &snapshot).unwrap();
        File::options()
            .write(true)
            .open(&snapshot)
            .unwrap()
            .set_len(inspection.records[70].offset)
            .unwrap();
        assert!(import_records(&mut session, &inspection, &snapshot).is_err());
        let progress = session
            .projection
            .flows
            .iter()
            .filter_map(|flow| {
                let flow = session.open_flow(&flow.id).unwrap();
                (flow.history_progress().records > 0).then(|| {
                    (
                        flow.descriptor.address.instance_id.clone(),
                        flow.cursor().clone(),
                    )
                })
            })
            .collect::<Vec<_>>();
        assert_eq!(progress.len(), 1);
        assert_eq!(
            session
                .open_flow(&progress[0].0)
                .unwrap()
                .history_progress()
                .records,
            64
        );
        drop(session);
        assert_eq!(fs::read_dir(root.root.join("sessions")).unwrap().count(), 0);
        // A corrupt retained source blocks publication without discarding the committed prefix.
        assert!(
            root.archive_legacy(&path, source, destination.clone(), true)
                .is_err()
        );
        assert_eq!(fs::read_dir(root.root.join("sessions")).unwrap().count(), 0);
        fs::write(&snapshot, &raw).unwrap();
        let receipt = root
            .archive_legacy(&path, source, destination.clone(), true)
            .unwrap();
        let imported = receipt
            .flows
            .iter()
            .find(|flow| flow.instance == progress[0].0)
            .unwrap();
        assert_eq!(imported.records, 150);
        // Creation plus three batches; the original 64 records were not appended twice.
        assert_eq!(imported.cursor.sequence, 4);
        assert_eq!(fs::read_dir(root.root.join("sessions")).unwrap().count(), 1);
        assert_eq!(fs::read(&path).unwrap(), raw);
        assert_eq!(
            root.archive_legacy(&path, source, destination.clone(), true)
                .unwrap(),
            receipt
        );
        let inspected = RuntimeStore::inspect_session(temp.path(), destination.id).unwrap();
        root.delete_session(&super::super::SessionRemoval {
            session_id: destination.id,
            expected: inspected.journal.cursor.unwrap(),
        })
        .unwrap();
        assert!(
            root.archive_legacy(&path, source, destination.clone(), true)
                .is_err()
        );
        assert!(
            !stage
                .root
                .join("sessions")
                .join(destination.id.to_string())
                .exists()
        );
        assert!(RuntimeStore::session_ids(temp.path()).unwrap().is_empty());
        assert_eq!(fs::read(&path).unwrap(), raw);
    }
}
