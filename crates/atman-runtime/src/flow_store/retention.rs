//! Durable transfer of a frozen host-local history without executing its entries.

use std::{io, path::Path};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    FlowDescriptor, FlowFact, FlowJournal, FlowJournalBackend, FlowRef, FlowRegistration,
    FlowStore, SessionFact, SessionProjection, SessionStore, initialize_dir, invalid,
    position::{FlowPosition, TransientPosition},
    transient::TransientSnapshot,
};
use crate::{
    event_log::journal::{CommitReceipt, Journal},
    storage::ProjectContext,
    tools::tool_output::MemoryOutputSnapshot,
};

const MANIFEST: &str = "retention.json";
const MANIFEST_LIMIT: usize = 128 * 1024;

/// Acknowledged correspondence between a frozen memory boundary and durable history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RetentionReceipt {
    pub request: Uuid,
    pub flow: FlowRef,
    pub source: TransientPosition,
    pub destination: CommitReceipt,
    pub outputs_digest: String,
}

impl RetentionReceipt {
    pub(super) fn validate(&self) -> io::Result<()> {
        if self.request.is_nil()
            || self.flow.session_id.0.is_nil()
            || self.flow.instance_id.0.0.is_nil()
            || self.source.host_generation.is_nil()
            || self.source.flow_id != self.flow.instance_id.0.0
            || self.source.sequence == 0
            || self.destination.journal_id != self.source.flow_id
            || self.destination.sequence != self.source.sequence
            || !digest(&self.source.digest)
            || !digest(&self.destination.digest)
            || !self
                .outputs_digest
                .strip_prefix("blake3:")
                .is_some_and(digest)
        {
            return Err(invalid("invalid retained history correspondence"));
        }
        Ok(())
    }

    fn matches(&self, plan: &RetentionPlan) -> bool {
        self.request == plan.request
            && self.flow == plan.flow
            && self.source == plan.source
            && self.outputs_digest == plan.outputs_digest
    }
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
struct RetentionPlan {
    schema: u32,
    request: Uuid,
    flow: FlowRef,
    source: TransientPosition,
    creation_request: Uuid,
    descriptor_digest: String,
    project: Option<ProjectContext>,
    outputs_digest: String,
}

#[derive(Deserialize, Serialize)]
struct RetentionManifest {
    plan: RetentionPlan,
    receipt: RetentionReceipt,
}

/// Immutable transfer input. The original memory owner remains frozen until the host completes or abandons its handoff.
pub struct RetentionCapture {
    plan: RetentionPlan,
    history: TransientSnapshot,
    outputs: MemoryOutputSnapshot,
}

impl RetentionCapture {
    pub fn request(&self) -> Uuid {
        self.plan.request
    }
    pub fn flow(&self) -> &FlowRef {
        &self.plan.flow
    }
    pub fn source(&self) -> &TransientPosition {
        &self.plan.source
    }
    pub fn freeze_ticket(&self) -> Uuid {
        self.history.id
    }
    pub fn project(&self) -> Option<&ProjectContext> {
        self.plan.project.as_ref()
    }

    fn descriptor(&self) -> io::Result<FlowDescriptor> {
        let first = self.history.map_batch(0, |_| Ok(()))?;
        match first.facts.as_slice() {
            [FlowFact::Created(descriptor)] => Ok(descriptor.as_ref().clone()),
            _ => Err(invalid("retention capture has no original creation batch")),
        }
    }
}

pub struct RetainedFlow {
    pub store: FlowStore,
    pub receipt: RetentionReceipt,
}

impl FlowStore {
    /// Capture the exact acknowledged boundary after the host has fenced scheduling and drained producers.
    pub fn capture_retention(
        &mut self,
        request: Uuid,
        expected: &FlowPosition,
    ) -> io::Result<RetentionCapture> {
        if request.is_nil() || expected != self.cursor() {
            return Err(invalid("retention request or source boundary changed"));
        }
        let FlowPosition::Transient { transient: source } = expected else {
            return Err(invalid("retention capture requires a transient history"));
        };
        let outputs = self.output_store.memory_snapshot()?;
        let plan = RetentionPlan {
            schema: 1,
            request,
            flow: self.descriptor.address.clone(),
            source: source.clone(),
            creation_request: self.descriptor.creation_request,
            descriptor_digest: blake3::hash(
                &serde_json::to_vec(&self.descriptor).map_err(invalid)?,
            )
            .to_hex()
            .to_string(),
            project: self.project.clone(),
            outputs_digest: outputs.identity_digest(),
        };
        let history = self.freeze_transient()?;
        Ok(RetentionCapture {
            plan,
            history,
            outputs,
        })
    }
}

impl SessionStore {
    /// Resolve a retained request through the acknowledged catalog without acquiring its Flow writer.
    ///
    /// An uncertain catalog writer is reconciled under its existing lock before a result is returned. The host must publish this store's current access projection after the call, including on an operation error.
    pub fn retention_receipt(
        &mut self,
        request: Uuid,
        expected_source: &FlowPosition,
    ) -> io::Result<Option<RetentionReceipt>> {
        self.reconcile_retention_catalog()?;
        let FlowPosition::Transient { transient: source } = expected_source else {
            return Err(invalid(
                "retention lookup requires its original transient boundary",
            ));
        };
        if request.is_nil()
            || source.host_generation.is_nil()
            || source.flow_id.is_nil()
            || source.sequence == 0
            || !digest(&source.digest)
        {
            return Err(invalid("retention lookup identity is invalid"));
        }
        let previous = self
            .projection
            .flows
            .iter()
            .filter_map(|flow| flow.retention.as_ref())
            .find(|receipt| receipt.request == request || receipt.source.flow_id == source.flow_id);
        match previous {
            Some(receipt) if receipt.request == request && receipt.source == *source => {
                Ok(Some(receipt.clone()))
            }
            Some(_) => Err(invalid(
                "retention request conflicts with the recorded transfer",
            )),
            None => Ok(None),
        }
    }

    /// Publish a frozen history and its captured assets, then register their original project binding.
    ///
    /// Failure never releases the memory capture. Retrying the same capture reconciles published directories and uncertain catalog writes; the host reuses an already installed durable worker through `retention_receipt`.
    pub fn retain_flow(
        &mut self,
        capture: &RetentionCapture,
        expected_catalog: &CommitReceipt,
    ) -> io::Result<RetainedFlow> {
        self.reconcile_retention_catalog()?;
        let plan = &capture.plan;
        if plan.flow.session_id.0 != self.projection.descriptor.id {
            return Err(invalid("retention targets another Session"));
        }
        let previous = self.retention_receipt(
            plan.request,
            &FlowPosition::Transient {
                transient: plan.source.clone(),
            },
        )?;
        if previous.is_none() {
            if expected_catalog != self.cursor() {
                return Err(invalid("Session inventory changed before retention"));
            }
            if self.projection.flows.iter().any(|registered| {
                registered.id == plan.flow.instance_id
                    || registered.creation_request == plan.creation_request
            }) {
                return Err(invalid("retention conflicts with a registered Flow"));
            }
        }
        let plan_bytes = serde_json::to_vec(plan).map_err(invalid)?;
        if plan_bytes.len() > MANIFEST_LIMIT {
            return Err(invalid("retention identity exceeds its metadata limit"));
        }
        crate::retained_file::publish(
            &self
                .dir
                .join("retentions")
                .join(format!("{}.json", plan.request)),
            &plan_bytes,
        )?;
        let directory = self
            .dir
            .join("flows")
            .join(plan.flow.instance_id.to_string());
        match initialize_dir(&directory, |stage| write_history(stage, capture)) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let manifest: RetentionManifest = serde_json::from_slice(
            &crate::retained_file::read_bounded(&directory.join(MANIFEST), MANIFEST_LIMIT)?,
        )
        .map_err(invalid)?;
        if manifest.plan != *plan || !manifest.receipt.matches(plan) {
            return Err(invalid("retained destination belongs to another transfer"));
        }
        manifest.receipt.validate()?;
        let store = FlowStore::open(
            &directory,
            self.owner.clone(),
            &plan.flow,
            plan.project.clone(),
        )?;
        if store.cursor().require_durable()? != &manifest.receipt.destination
            || store.descriptor() != &capture.descriptor()?
        {
            return Err(invalid(
                "retained history differs from its completed transfer",
            ));
        }
        // Compare every captured output again before acknowledging an existing destination.
        capture.outputs.persist_to(&directory)?;
        if let Some(previous) = previous {
            if previous != manifest.receipt {
                return Err(invalid("catalog retention differs from the destination"));
            }
        } else {
            let mut facts = vec![SessionFact::FlowRetained(FlowRegistration {
                id: plan.flow.instance_id.clone(),
                creation_request: plan.creation_request,
                project: plan.project.clone(),
                retention: Some(manifest.receipt.clone()),
            })];
            if self.projection.default_flow.is_none() {
                facts.push(SessionFact::DefaultFlowSelected(
                    plan.flow.instance_id.clone(),
                ));
            }
            self.commit(facts)?;
        }
        Ok(RetainedFlow {
            store,
            receipt: manifest.receipt,
        })
    }

    fn reconcile_retention_catalog(&mut self) -> io::Result<()> {
        if !self.journal.needs_reconciliation() {
            return Ok(());
        }
        let id = self.projection.descriptor.id;
        let descriptor = self.owner.descriptor.clone();
        let mut projection = None;
        let mut cursor = None;
        let recovered =
            self.journal
                .reconcile_owned(&self.dir.join("events.jsonl"), |receipt, facts| {
                    SessionProjection::replay(&mut projection, id, facts)?;
                    if projection
                        .as_ref()
                        .is_none_or(|state| state.descriptor != descriptor)
                    {
                        return Err(invalid(
                            "Session owner identity changed during reconciliation",
                        ));
                    }
                    cursor = Some(receipt);
                    Ok(())
                })?;
        self.projection =
            projection.ok_or_else(|| invalid("reconciled catalog has no creation"))?;
        self.cursor =
            cursor.ok_or_else(|| invalid("reconciled catalog has no committed boundary"))?;
        if recovered.is_some() {
            self.recovered_tail = recovered;
        }
        self.refresh_projection();
        Ok(())
    }
}

fn write_history(stage: &Path, capture: &RetentionCapture) -> io::Result<()> {
    let plan = &capture.plan;
    let journal = Journal::create(&stage.join("events.jsonl"), plan.flow.instance_id.0.0)?;
    let mut destination = FlowJournal {
        backend: FlowJournalBackend::Durable {
            journal,
            directory: stage.into(),
        },
        history_only: false,
        activity: None,
    };
    capture.outputs.persist_to(stage)?;
    let mut positions = Vec::<(FlowPosition, FlowPosition)>::with_capacity(capture.history.len());
    for index in 0..capture.history.len() {
        let batch = capture.history.map_batch(index, |source| {
            let index = source
                .sequence()
                .checked_sub(1)
                .and_then(|value| usize::try_from(value).ok())
                .ok_or_else(|| invalid("retention source position is invalid"))?;
            let (expected, durable) = positions
                .get(index)
                .ok_or_else(|| invalid("retention references an uncommitted or future batch"))?;
            if source != expected {
                return Err(invalid("retention references another history"));
            }
            *source = durable.clone();
            Ok(())
        })?;
        let durable = destination.commit(&batch.facts)?;
        positions.push((batch.receipt, durable));
    }
    let (source, destination_position) = positions
        .last()
        .ok_or_else(|| invalid("retention source is empty"))?;
    if source != &capture.history.cursor || source.sequence() != destination_position.sequence() {
        return Err(invalid(
            "retention did not preserve the full source boundary",
        ));
    }
    let receipt = RetentionReceipt {
        request: plan.request,
        flow: plan.flow.clone(),
        source: plan.source.clone(),
        destination: destination_position.require_durable()?.clone(),
        outputs_digest: plan.outputs_digest.clone(),
    };
    receipt.validate()?;
    drop(destination);
    // Full replay validates typed references and state transitions before directory publication.
    let mut replay = super::replay::FlowReplay::default();
    let (journal, tail) = Journal::open(
        &stage.join("events.jsonl"),
        plan.source.flow_id,
        |receipt, facts| replay.apply(stage, &plan.flow, receipt.into(), facts),
    )?;
    if tail.is_some()
        || replay
            .cursor
            .as_ref()
            .is_none_or(|position| position.require_durable().ok() != Some(&receipt.destination))
    {
        return Err(invalid("retention destination failed complete replay"));
    }
    drop(journal);
    let bytes = serde_json::to_vec(&RetentionManifest {
        plan: plan.clone(),
        receipt,
    })
    .map_err(invalid)?;
    if bytes.len() > MANIFEST_LIMIT {
        return Err(invalid("retention receipt exceeds its metadata limit"));
    }
    crate::retained_file::publish(&stage.join(MANIFEST), &bytes)
}

#[cfg(test)]
mod tests;
