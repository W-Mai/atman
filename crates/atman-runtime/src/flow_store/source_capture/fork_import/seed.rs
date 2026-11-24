//! Seed a destination Waiting entry and publish its independent historical data.

use super::*;
use crate::flow_store::{FlowInstanceId, inbox::SenderIdentity, launch::EntryBinding};

pub(super) const SCHEMA: u32 = 3;

/// Immutable entry provenance captured from the original owner.
pub struct ForkSourceEntry {
    pub descriptor: FlowDescriptor,
    pub execution: Option<EntryBinding>,
}

impl FlowStore {
    pub fn fork_source_entry(&self) -> ForkSourceEntry {
        ForkSourceEntry {
            descriptor: self.descriptor().clone(),
            execution: self
                .activation
                .as_ref()
                .and_then(|(activation, _)| activation.input.execution.clone()),
        }
    }
}

impl SessionStore {
    /// Install only destination-owned model and display data. The original Waiting activation
    /// remains the execution boundary and contains no new input.
    pub fn create_seeded_fork_import(
        &mut self,
        mut plan: ForkImportPlan,
        source: &ForkSourceEntry,
        snapshot: &RetainedForkSnapshot,
        mut input: LaunchInput,
        authorize: impl FnOnce(&FlowDescriptor, &LaunchInput) -> io::Result<()>,
    ) -> io::Result<ForkImportPublication> {
        let source_descriptor = &source.descriptor;
        let binding = input
            .execution
            .as_ref()
            .ok_or_else(|| invalid("Fork requires a destination execution binding"))?;
        if plan.schema != 2
            || plan.destination.session_id.0 != self.projection.descriptor.id
            || !matches!(
                source_descriptor.content,
                super::super::super::FlowContent::Entry(_)
            )
            || input.request.request_id != plan.operation
            || input.request.recipient != plan.destination
            || !matches!(input.request.sender, SenderIdentity::User(_))
            || !matches!(input.request.start, EntryStart::Waiting)
            || input.scope.is_some()
            || input.workspace.is_some()
            || input.initial_goal.is_some()
            || input.continued_from.is_some()
            || input.result_route.is_some()
            || !input.inherited_context.is_empty()
            || !input.inherited_outputs.is_empty()
            || binding.session_start
            || binding.authority.workspace_root.as_ref() != Some(&plan.working_directory)
            || source.execution.as_ref().is_some_and(|old| {
                old.first_turn == binding.first_turn || old.first_run == binding.first_run
            })
        {
            return Err(invalid("Fork must seed a fresh destination Waiting entry"));
        }
        input.validate()?;
        plan = plan.bind_source_descriptor(source_descriptor)?;
        let mut identity = blake3::Hasher::new();
        identity.update(b"atman.seeded-fork-entry\0");
        serde_json::to_writer(&mut identity, &(&plan.source_descriptor_digest, &input))
            .map_err(invalid)?;
        plan.schema = SCHEMA;
        plan.source_descriptor_digest = Some(identity.finalize().to_hex().to_string());
        let assets_digest = Self::validate_fork_assets(&plan, snapshot)?;
        if let Some(existing) = inspection::inspect(&self.dir, &self.projection, plan.operation)? {
            if existing.plan != plan {
                return Err(invalid(
                    "Fork activation conflicts with the published operation",
                ));
            }
            return Ok(existing.publication);
        }
        if self.projection.project != plan.project {
            return Err(invalid(
                "Fork destination project changed before publication",
            ));
        }
        let descriptor = FlowDescriptor {
            address: plan.destination.clone(),
            creation_request: plan.operation,
            content: source_descriptor.content.clone(),
            created_by: None,
        };
        authorize(&descriptor, &input)?;
        match self.inspect_fork_import(plan.operation)? {
            Some(manifest)
                if manifest.plan == plan
                    && !matches!(manifest.state, ForkImportState::Abandoned) => {}
            Some(_) => {
                return Err(invalid(
                    "Fork activation conflicts with its prepared operation",
                ));
            }
            None if self.projection.flows.is_empty() => {
                self.prepare_fork_import(plan.clone())?;
            }
            None => return Err(invalid("Fork destination Session already contains a Flow")),
        }

        let mut destination = self.create_flow_registration(descriptor, None, None)?;
        let mut data = snapshot.data().clone();
        snapshot.publish_assets(destination.require_directory()?, &mut data)?;
        input.inherited_outputs = destination
            .output_store
            .capture_references_with_budget(&data.model_context, plan.asset_bytes)
            .map_err(invalid)?;
        input.inherited_context = data.model_context.clone();
        let launch = destination.activate_entry(input, None, |_| Ok(()))?;
        let receipt = launch.receipt.require_durable()?.clone();
        drop(destination);
        self.record_fork_import_receipt(plan.clone(), receipt.clone())?;
        self.write_fork_publication(&plan, snapshot, data, receipt, assets_digest)
    }

    /// Ordinary Flow opens must wait for the final publication of a seeded Fork.
    pub(in crate::flow_store) fn require_published_fork(
        &self,
        id: &FlowInstanceId,
        operation: Uuid,
    ) -> io::Result<()> {
        if operation.is_nil() {
            return Ok(());
        }
        let Some(manifest) = self.inspect_fork_import(operation)? else {
            return Ok(());
        };
        if manifest.plan.schema != SCHEMA {
            return Ok(());
        }
        if &manifest.plan.destination.instance_id != id {
            return Err(invalid(
                "Fork operation belongs to another destination Flow",
            ));
        }
        if matches!(manifest.state, ForkImportState::Published { .. }) {
            let publication = self
                .dir
                .join("fork-imports")
                .join(operation.to_string())
                .join("publication.json");
            match fs::symlink_metadata(publication) {
                Ok(metadata) if metadata.is_file() => return Ok(()),
                Ok(_) => return Err(invalid("Fork publication is not a managed regular file")),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "Fork destination publication is pending",
        ))
    }
}
