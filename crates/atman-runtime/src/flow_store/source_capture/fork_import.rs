//! Durable, restart-safe admission metadata for a native Fork destination.
//!
//! The manifest records a reviewed source boundary and destination identity before a
//! destination Flow is created. It is deliberately not a Flow journal, execution lease,
//! approval grant, or LegacyArchive replacement.

use std::{fs, io, path::PathBuf};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    ForkBaseline, ForkSnapshot, ResolvedForkBoundary, RetainedForkSnapshot, SourceSelection,
};
use crate::{
    event_log::journal::{CommitReceipt, sync_parent},
    flow_store::{
        FlowDescriptor, FlowRef, FlowStore, SessionStore, invalid,
        launch::{EntryStart, LaunchInput, LaunchRecord},
    },
    message::Message,
    projection::flow::ImportedHistory,
    storage::ProjectContext,
};

const MANIFEST_LIMIT: usize = 256 * 1024;
const PUBLICATION_LIMIT: usize = 64 * 1024 * 1024;

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
    /// Digest of the reviewed source descriptor and EntrySnapshot. Older manifests without
    /// this binding cannot be used for destination creation or receipt publication.
    #[serde(default)]
    pub source_descriptor_digest: Option<String>,
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
            source_descriptor_digest: None,
        };
        plan.validate()?;
        Ok(plan)
    }

    /// Bind an immutable source descriptor to this import operation before persistence.
    pub fn bind_source_descriptor(mut self, descriptor: &FlowDescriptor) -> io::Result<Self> {
        if descriptor.address != self.source.flow {
            return Err(invalid(
                "Fork import source descriptor does not match the reviewed boundary",
            ));
        }
        let digest = descriptor_digest(descriptor)?;
        if self
            .source_descriptor_digest
            .as_deref()
            .is_some_and(|previous| previous != digest)
        {
            return Err(invalid(
                "Fork import source descriptor conflicts with the reviewed binding",
            ));
        }
        self.source_descriptor_digest = Some(digest);
        self.validate_bound()?;
        Ok(self)
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
            || self
                .source_descriptor_digest
                .as_ref()
                .is_some_and(|digest| {
                    digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                })
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

    fn validate_bound(&self) -> io::Result<()> {
        self.validate()?;
        if self.source_descriptor_digest.is_none() {
            return Err(invalid(
                "Fork import plan has no reviewed source descriptor binding",
            ));
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

/// Result of creating and activating a destination-owned Waiting Flow.
///
/// The destination Flow is already durably registered and activated when this
/// value is returned. Waiting activation never starts a turn; the caller must
/// explicitly bind scheduling and submit input later.
#[derive(Clone, Debug)]
pub struct ForkImportActivation {
    pub destination: FlowRef,
    pub launch: LaunchRecord,
    pub receipt: ForkImportReceipt,
}

/// Durable, destination-owned Fork history and model windows bound to one published receipt.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ForkImportPublication {
    pub operation: Uuid,
    pub destination: FlowRef,
    pub destination_receipt: CommitReceipt,
    pub snapshot_digest: String,
    pub assets_digest: String,
    pub history: ImportedHistory,
    pub baseline: Option<ForkBaseline>,
    pub model_context: Vec<Message>,
    pub asset_bytes: usize,
}

impl ForkImportPublication {
    fn validate(&self) -> io::Result<()> {
        if self.operation.is_nil()
            || self.destination.session_id.0.is_nil()
            || self.destination.instance_id.0.0.is_nil()
            || self.destination_receipt.journal_id != self.destination.instance_id.0.0
            || self.destination_receipt.sequence == 0
            || self.snapshot_digest.len() != 64
            || !self
                .snapshot_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || self.assets_digest.len() != 64
            || !self
                .assets_digest
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || self.history.receipt != self.destination_receipt
        {
            return Err(invalid("invalid Fork import publication"));
        }
        self.history.validate_for_destination(&self.destination)?;
        Ok(())
    }
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

    /// Create or resume a destination-owned Waiting Flow from an immutable source descriptor.
    ///
    /// Only the source entry content is copied. Source activation, leases, routes, inherited
    /// context and continuation state are never rebased into the destination. The caller owns
    /// authorization for the new activation; a matching retry returns the committed activation
    /// without invoking that callback or starting execution.
    pub fn create_waiting_fork_import(
        &mut self,
        plan: ForkImportPlan,
        source: &FlowStore,
        input: LaunchInput,
        authorize_new: impl FnOnce(&FlowDescriptor, &LaunchInput) -> io::Result<()>,
    ) -> io::Result<ForkImportActivation> {
        let source_digest = descriptor_digest(source.descriptor())?;
        let plan = plan.bind_source_descriptor(source.descriptor())?;
        plan.validate_bound()?;
        if plan.destination.session_id.0 != self.projection.descriptor.id {
            return Err(invalid(
                "Fork import destination belongs to another Session",
            ));
        }
        if source.descriptor().address != plan.source.flow {
            return Err(invalid(
                "Fork import source Flow does not match the reviewed boundary",
            ));
        }
        if plan.source_descriptor_digest.as_deref() != Some(source_digest.as_str()) {
            return Err(invalid(
                "Fork import source descriptor changed after review",
            ));
        }
        if !matches!(
            source.descriptor().content,
            super::super::FlowContent::Entry(_)
        ) {
            return Err(invalid(
                "Fork import source must have executable entry content",
            ));
        }
        if input.request.request_id != plan.operation
            || input.request.recipient != plan.destination
            || !matches!(input.request.start, EntryStart::Waiting)
            || input.initial_goal.is_some()
            || !input.inherited_context.is_empty()
            || !input.inherited_outputs.is_empty()
            || input.continued_from.is_some()
            || input.result_route.is_some()
        {
            return Err(invalid(
                "Fork import activation must be a destination-owned Waiting request",
            ));
        }
        let input_execution = input
            .execution
            .as_ref()
            .ok_or_else(|| invalid("Fork import Waiting activation has no execution binding"))?;
        if let Some(source_launch) = source.launch_record()
            && let Some(source_execution) = source_launch.input.execution.as_ref()
            && (input_execution.first_turn == source_execution.first_turn
                || input_execution.first_run == source_execution.first_run)
        {
            return Err(invalid(
                "Fork import cannot reuse the source execution identity",
            ));
        }

        // A prepared manifest is the reviewed operation boundary. Once a Flow exists, retries
        // must inspect that manifest instead of attempting a second preparation.
        let manifest = match self.inspect_fork_import(plan.operation)? {
            Some(manifest) => {
                if manifest.plan != plan {
                    return Err(invalid(
                        "Fork import manifest conflicts with the reviewed plan",
                    ));
                }
                manifest
            }
            None => {
                self.prepare_fork_import(plan.clone())?;
                self.inspect_fork_import(plan.operation)?
                    .ok_or_else(|| invalid("Fork import manifest disappeared after preparation"))?
            }
        };
        if matches!(manifest.state, ForkImportState::Abandoned) {
            return Err(invalid("Fork import manifest has been abandoned"));
        }
        if matches!(manifest.state, ForkImportState::Published { .. })
            && !self
                .projection
                .flows
                .iter()
                .any(|flow| flow.id == plan.destination.instance_id)
        {
            return Err(invalid(
                "published Fork import is missing its destination Flow",
            ));
        }

        let descriptor = FlowDescriptor {
            address: plan.destination.clone(),
            creation_request: plan.operation,
            content: source.descriptor().content.clone(),
            // Source provenance is retained in the reviewed plan and imported records; it is
            // intentionally not installed as destination Flow ancestry or execution authority.
            created_by: None,
        };
        let mut destination = self.create_flow(descriptor.clone())?;
        let launch =
            destination.activate_entry(input, None, |input| authorize_new(&descriptor, input))?;
        let destination_receipt = destination.cursor().require_durable()?.clone();
        drop(destination);
        let receipt = self.publish_fork_import_receipt(plan, destination_receipt)?;
        Ok(ForkImportActivation {
            destination: descriptor.address,
            launch,
            receipt,
        })
    }

    /// Publish the prepared Fork history and independently retained assets after the destination
    /// Waiting Flow has a real durable receipt. The publication is an immutable destination
    /// artifact; it does not replay source facts or start execution.
    pub fn publish_fork_snapshot(
        &self,
        plan: ForkImportPlan,
        snapshot: &RetainedForkSnapshot,
        destination_receipt: CommitReceipt,
    ) -> io::Result<ForkImportPublication> {
        let _ = self.validate_fork_snapshot(&plan, snapshot, &destination_receipt)?;
        let data = snapshot.data();
        let receipt = self.publish_fork_import_receipt(plan.clone(), destination_receipt)?;
        let ForkImportState::Published {
            destination_receipt,
        } = receipt.state
        else {
            return Err(invalid("Fork import receipt was not published"));
        };
        let destination = self.open_flow(&plan.destination.instance_id)?;
        let destination_dir = destination.require_directory()?.to_owned();
        drop(destination);

        let mut data = data.clone();
        snapshot.publish_assets(&destination_dir, &mut data)?;
        let history = ImportedHistory::bind_for_destination(
            &plan.destination,
            destination_receipt.clone(),
            data.records.clone(),
        )?;
        if history
            .records
            .iter()
            .any(|record| record.source.flow != plan.source.flow)
        {
            return Err(invalid(
                "Fork import history contains a foreign source Flow",
            ));
        }
        let publication = ForkImportPublication {
            operation: plan.operation,
            destination: plan.destination.clone(),
            destination_receipt,
            snapshot_digest: plan.snapshot_digest.clone(),
            assets_digest: snapshot.assets().identity_digest(),
            history,
            baseline: data.baseline,
            model_context: data.model_context,
            asset_bytes: snapshot.assets().bytes(),
        };
        publication.validate()?;
        let bytes = serde_json::to_vec(&publication).map_err(invalid)?;
        if bytes.len() > PUBLICATION_LIMIT {
            return Err(invalid(
                "Fork import publication exceeds its metadata limit",
            ));
        }
        let path = self
            .dir
            .join("fork-imports")
            .join(plan.operation.to_string())
            .join("publication.json");
        match crate::retained_file::read_bounded(&path, PUBLICATION_LIMIT) {
            Ok(stored) => {
                if stored != bytes {
                    return Err(invalid(
                        "Fork import publication conflicts with the accepted destination",
                    ));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                crate::retained_file::publish(&path, &bytes)?;
            }
            Err(error) => return Err(error),
        }
        let stored = crate::retained_file::read_bounded(&path, PUBLICATION_LIMIT)?;
        let stored: ForkImportPublication = serde_json::from_slice(&stored).map_err(invalid)?;
        stored.validate()?;
        Ok(stored)
    }

    /// Validate an accepted immutable publication, then reject replay into a regular Flow.
    ///
    /// Imported records contain source leases and source journal receipts. They are valid
    /// historical display data only; no FlowFact can safely rebase them into the destination
    /// execution journal. This method intentionally performs no journal or asset mutation.
    pub fn replay_fork_publication(&self, publication: &ForkImportPublication) -> io::Result<()> {
        publication.validate()?;
        self.validate_destination_receipt(
            &publication.destination,
            &publication.destination_receipt,
        )?;
        Err(unsupported_fork_replay())
    }

    /// Validate a retained snapshot against the accepted destination boundary, then reject
    /// replay into a regular Flow without changing the destination cursor or assets.
    pub fn replay_fork_snapshot(
        &self,
        plan: ForkImportPlan,
        snapshot: &RetainedForkSnapshot,
        destination_receipt: CommitReceipt,
    ) -> io::Result<()> {
        self.validate_fork_snapshot(&plan, snapshot, &destination_receipt)?;
        Err(unsupported_fork_replay())
    }

    fn validate_fork_snapshot(
        &self,
        plan: &ForkImportPlan,
        snapshot: &RetainedForkSnapshot,
        destination_receipt: &CommitReceipt,
    ) -> io::Result<ImportedHistory> {
        plan.validate_bound()?;
        let data = snapshot.data();
        if data.boundary.observed != plan.source
            || data.boundary.source.flow != plan.source.flow
            || blake3::hash(&serde_json::to_vec(data).map_err(invalid)?)
                .to_hex()
                .to_string()
                != plan.snapshot_digest
            || snapshot.assets().bytes() > plan.asset_bytes
        {
            return Err(invalid("Fork snapshot changed after import review"));
        }
        self.validate_destination_receipt(&plan.destination, destination_receipt)?;
        let history = ImportedHistory::bind_for_destination(
            &plan.destination,
            destination_receipt.clone(),
            data.records.clone(),
        )?;
        if history
            .records
            .iter()
            .any(|record| record.source.flow != plan.source.flow)
        {
            return Err(invalid(
                "Fork import history contains a foreign source Flow",
            ));
        }
        Ok(history)
    }

    fn validate_destination_receipt(
        &self,
        destination: &FlowRef,
        receipt: &CommitReceipt,
    ) -> io::Result<()> {
        if destination.session_id.0 != self.projection.descriptor.id {
            return Err(invalid(
                "Fork import destination belongs to another Session",
            ));
        }
        let inspection = self.inspect_flow(&destination.instance_id)?;
        if inspection.descriptor.address != *destination
            || inspection.journal.trailing_bytes != 0
            || inspection.journal.cursor.as_ref() != Some(receipt)
        {
            return Err(invalid(
                "Fork import destination receipt does not match the durable Flow",
            ));
        }
        Ok(())
    }

    pub fn inspect_fork_import_publication(
        &self,
        operation: Uuid,
    ) -> io::Result<Option<ForkImportPublication>> {
        if operation.is_nil() {
            return Err(invalid("Fork import operation identity is empty"));
        }
        let path = self
            .dir
            .join("fork-imports")
            .join(operation.to_string())
            .join("publication.json");
        let bytes = match crate::retained_file::read_bounded(&path, PUBLICATION_LIMIT) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let publication: ForkImportPublication = serde_json::from_slice(&bytes).map_err(invalid)?;
        publication.validate()?;
        if publication.operation != operation
            || publication.destination.session_id.0 != self.projection.descriptor.id
        {
            return Err(invalid(
                "Fork import publication identity does not match its path",
            ));
        }
        Ok(Some(publication))
    }

    /// Record the real destination journal receipt after a caller has durably created its
    /// Waiting Flow. This method only publishes the idempotent manifest state; it never creates
    /// or starts the Flow and never accepts a source lease as destination authority.
    pub fn publish_fork_import_receipt(
        &self,
        plan: ForkImportPlan,
        destination_receipt: CommitReceipt,
    ) -> io::Result<ForkImportReceipt> {
        plan.validate_bound()?;
        if plan.destination.session_id.0 != self.projection.descriptor.id {
            return Err(invalid(
                "Fork import destination belongs to another Session",
            ));
        }
        if destination_receipt.sequence == 0 || destination_receipt.journal_id.is_nil() {
            return Err(invalid("Fork import destination receipt is empty"));
        }
        let destination = self.open_flow(&plan.destination.instance_id)?;
        let launch = destination
            .launch_record()
            .ok_or_else(|| invalid("Fork import destination Flow is not activated"))?;
        if !matches!(
            launch.input.request.start,
            super::super::launch::EntryStart::Waiting
        ) {
            return Err(invalid(
                "Fork import destination Flow must be activated in Waiting mode",
            ));
        }
        let actual_receipt = destination.cursor().require_durable()?.clone();
        if actual_receipt != destination_receipt {
            return Err(invalid(
                "Fork import destination receipt does not match the Waiting Flow",
            ));
        }
        let path = self
            .dir
            .join("fork-imports")
            .join(plan.operation.to_string())
            .join("manifest.json");
        let manifest = match crate::retained_file::read_bounded(&path, MANIFEST_LIMIT) {
            Ok(bytes) => {
                let manifest: ForkImportManifest =
                    serde_json::from_slice(&bytes).map_err(invalid)?;
                if manifest.plan != plan {
                    return Err(invalid(
                        "Fork import manifest conflicts with the reviewed plan",
                    ));
                }
                manifest
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                self.prepare_fork_import(plan.clone())?;
                let bytes = crate::retained_file::read_bounded(&path, MANIFEST_LIMIT)?;
                serde_json::from_slice(&bytes).map_err(invalid)?
            }
            Err(error) => return Err(error),
        };
        match &manifest.state {
            ForkImportState::Published {
                destination_receipt: previous,
            } if previous == &destination_receipt => {
                let bytes = serde_json::to_vec(&manifest).map_err(invalid)?;
                return Ok(manifest.receipt(&bytes));
            }
            ForkImportState::Published { .. } => {
                return Err(invalid(
                    "Fork import destination receipt conflicts with the accepted state",
                ));
            }
            ForkImportState::Abandoned => {
                return Err(invalid("Fork import manifest has been abandoned"));
            }
            ForkImportState::Prepared => {}
        }
        let published = ForkImportManifest {
            plan,
            state: ForkImportState::Published {
                destination_receipt,
            },
        };
        let bytes = serde_json::to_vec(&published).map_err(invalid)?;
        if bytes.len() > MANIFEST_LIMIT {
            return Err(invalid("Fork import manifest exceeds its metadata limit"));
        }
        replace_manifest(&path, &bytes)?;
        let stored = crate::retained_file::read_bounded(&path, MANIFEST_LIMIT)?;
        let stored: ForkImportManifest = serde_json::from_slice(&stored).map_err(invalid)?;
        if stored != published {
            return Err(invalid("Fork import receipt changed during publication"));
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

fn unsupported_fork_replay() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "native Fork history cannot be replayed into a regular Flow journal",
    )
}

fn descriptor_digest(descriptor: &FlowDescriptor) -> io::Result<String> {
    Ok(
        blake3::hash(&serde_json::to_vec(descriptor).map_err(invalid)?)
            .to_hex()
            .to_string(),
    )
}

fn replace_manifest(path: &std::path::Path, bytes: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| invalid("Fork import manifest has no parent"))?;
    fs::create_dir_all(parent)?;
    crate::platform::atomic_replace(path, bytes)?;
    sync_parent(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        flow_store::{
            EntrySnapshot, FlowContent, FlowDescriptor, FlowInstanceId, RuntimeStore,
            SessionDescriptor,
            inbox::SenderIdentity,
            launch::{EntryBinding, EntryRequest, EntryStart, LaunchInput},
        },
        source_program::RevisionBundle,
    };
    use atman_rt::{RunId, TurnId};
    use std::collections::BTreeMap;

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
            source_descriptor_digest: None,
        }
    }

    fn entry_content() -> FlowContent {
        FlowContent::Entry(EntrySnapshot {
            name: "fork-target".into(),
            path: "/missing/fork-target.at".into(),
            bundle: RevisionBundle {
                entry: "entry:main.at".into(),
                sources: BTreeMap::from([(
                    "entry:main.at".into(),
                    "flow main() -> int { return 1 }".into(),
                )]),
                edges: BTreeMap::new(),
            },
        })
    }

    fn waiting_input(destination: &FlowRef, operation: Uuid) -> LaunchInput {
        LaunchInput {
            name: None,
            display_label: None,
            scope: None,
            workspace: None,
            initial_goal: None,
            request: EntryRequest {
                request_id: operation,
                sender: SenderIdentity::User(Uuid::now_v7()),
                recipient: destination.clone(),
                arguments: Default::default(),
                invocation: Default::default(),
                start: EntryStart::Waiting,
            },
            inherited_context: Vec::new(),
            inherited_outputs: Vec::new(),
            continued_from: None,
            result_route: None,
            execution: Some(EntryBinding {
                user_routes: false,
                session_start: false,
                first_turn: TurnId::now(),
                first_run: RunId::now(),
                authority: crate::flow_authority::EffectiveAuthority::root(
                    &Default::default(),
                    true,
                    None,
                ),
            }),
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
    fn published_destination_receipt_is_restart_safe_and_idempotent() {
        let root = tempfile::tempdir().unwrap();
        let runtime = RuntimeStore::open(root.path()).unwrap();
        let destination_id = Uuid::now_v7();
        let mut session = runtime
            .create_session(SessionDescriptor {
                id: destination_id,
                project: None,
                configuration: Default::default(),
                title: "Fork target".into(),
            })
            .unwrap();
        let plan = plan(destination_id);
        let source_descriptor = FlowDescriptor {
            address: plan.source.flow.clone(),
            creation_request: Uuid::now_v7(),
            created_by: None,
            content: entry_content(),
        };
        let plan = plan.bind_source_descriptor(&source_descriptor).unwrap();
        session.prepare_fork_import(plan.clone()).unwrap();
        let mut destination = session
            .create_flow(FlowDescriptor {
                address: plan.destination.clone(),
                creation_request: Uuid::now_v7(),
                created_by: None,
                content: entry_content(),
            })
            .unwrap();
        let turn = TurnId::now();
        let run = RunId::now();
        destination
            .activate_entry(
                LaunchInput {
                    name: None,
                    display_label: None,
                    scope: None,
                    workspace: None,
                    initial_goal: None,
                    request: EntryRequest {
                        request_id: destination.descriptor().creation_request,
                        sender: SenderIdentity::User(Uuid::now_v7()),
                        recipient: plan.destination.clone(),
                        arguments: Default::default(),
                        invocation: Default::default(),
                        start: EntryStart::Waiting,
                    },
                    inherited_context: Vec::new(),
                    inherited_outputs: Vec::new(),
                    continued_from: None,
                    result_route: None,
                    execution: Some(EntryBinding {
                        user_routes: false,
                        session_start: false,
                        first_turn: turn,
                        first_run: run,
                        authority: crate::flow_authority::EffectiveAuthority::root(
                            &Default::default(),
                            true,
                            None,
                        ),
                    }),
                },
                None,
                |_| Ok(()),
            )
            .unwrap();
        let destination_receipt = destination.cursor().require_durable().unwrap().clone();
        drop(destination);
        let published = session
            .publish_fork_import_receipt(plan.clone(), destination_receipt.clone())
            .unwrap();
        assert!(matches!(
            published.state,
            ForkImportState::Published { destination_receipt: ref value } if value == &destination_receipt
        ));
        assert_eq!(
            session
                .publish_fork_import_receipt(plan.clone(), destination_receipt.clone())
                .unwrap(),
            published
        );
        drop(session);
        let session = runtime.open_session(destination_id).unwrap();
        let manifest = session
            .inspect_fork_import(plan.operation)
            .unwrap()
            .unwrap();
        assert!(matches!(
            manifest.state,
            ForkImportState::Published { destination_receipt: value } if value == destination_receipt
        ));
    }

    #[test]
    fn create_waiting_import_owns_destination_and_retries_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let runtime = RuntimeStore::open(root.path()).unwrap();
        let destination_id = Uuid::now_v7();
        let plan = plan(destination_id);
        let source_id = plan.source.flow.session_id.0;
        let mut source_session = runtime
            .create_session(SessionDescriptor {
                id: source_id,
                project: None,
                configuration: Default::default(),
                title: "Fork source".into(),
            })
            .unwrap();
        let source_flow = source_session
            .create_flow(FlowDescriptor {
                address: plan.source.flow.clone(),
                creation_request: Uuid::now_v7(),
                created_by: None,
                content: entry_content(),
            })
            .unwrap();
        let plan = plan
            .bind_source_descriptor(source_flow.descriptor())
            .unwrap();
        let mut destination = runtime
            .create_session(SessionDescriptor {
                id: destination_id,
                project: None,
                configuration: Default::default(),
                title: "Fork destination".into(),
            })
            .unwrap();
        let input = waiting_input(&plan.destination, plan.operation);
        let expected_content = source_flow.descriptor().content.clone();
        let activation = destination
            .create_waiting_fork_import(plan.clone(), &source_flow, input.clone(), |_, _| Ok(()))
            .unwrap();
        assert_eq!(activation.destination, plan.destination);
        assert!(matches!(
            activation.launch.input.request.start,
            EntryStart::Waiting
        ));
        let flow = destination
            .open_flow(&plan.destination.instance_id)
            .unwrap();
        assert_eq!(flow.descriptor().content, expected_content);
        assert_eq!(flow.descriptor().created_by, None);
        assert!(matches!(
            flow.state().state(),
            atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Idle)
        ));
        drop(flow);
        drop(destination);

        let mut destination = runtime.open_session(destination_id).unwrap();
        let retry = destination
            .create_waiting_fork_import(plan.clone(), &source_flow, input, |_, _| {
                panic!("matching activation must not reauthorize")
            })
            .unwrap();
        assert_eq!(retry.receipt, activation.receipt);
        assert_eq!(retry.launch.receipt, activation.launch.receipt);
    }

    #[test]
    fn publish_rejects_ghost_destination() {
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
        let source_descriptor = FlowDescriptor {
            address: plan.source.flow.clone(),
            creation_request: Uuid::now_v7(),
            created_by: None,
            content: entry_content(),
        };
        let plan = plan.bind_source_descriptor(&source_descriptor).unwrap();
        session.prepare_fork_import(plan.clone()).unwrap();
        let receipt = CommitReceipt {
            journal_id: plan.destination.instance_id.0.0,
            sequence: 1,
            digest: "ghost".into(),
        };
        assert!(session.publish_fork_import_receipt(plan, receipt).is_err());
    }

    #[test]
    fn source_descriptor_binding_rejects_changed_content() {
        let plan = plan(Uuid::now_v7());
        let descriptor = FlowDescriptor {
            address: plan.source.flow.clone(),
            creation_request: Uuid::now_v7(),
            created_by: None,
            content: entry_content(),
        };
        let bound = plan.bind_source_descriptor(&descriptor).unwrap();
        let mut changed = descriptor;
        if let FlowContent::Entry(entry) = &mut changed.content {
            entry.name.push_str("-changed");
        }
        assert!(bound.bind_source_descriptor(&changed).is_err());
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
