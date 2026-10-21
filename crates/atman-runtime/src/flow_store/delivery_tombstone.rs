//! Minimal acceptance evidence retained when a Session's conversation data is deleted.

use std::{fs, io, path::Path};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{
    FlowRef, RuntimeStore, SessionInspection, SessionRemoval,
    inbox::{Acceptance, DeliveryRequest, SenderIdentity},
    invalid,
    outbox::{DeliveryFailure, RecipientRejected},
};

#[derive(Deserialize, Serialize)]
struct RetainedAcceptance {
    sender: SenderIdentity,
    request_id: Uuid,
    digest: String,
    acceptance: Acceptance,
}

#[derive(Deserialize, Serialize)]
struct RetiredFlow {
    address: FlowRef,
    accepted: Vec<RetainedAcceptance>,
}

#[derive(Deserialize, Serialize)]
struct DeliveryTombstone {
    removal: SessionRemoval,
    project: Option<String>,
    flows: Vec<RetiredFlow>,
}

impl RuntimeStore {
    /// Called under catalog and detached Session ownership, before any journal is removed.
    pub(super) fn retain_session_removal(
        &self,
        inspected: &SessionInspection,
        request: &SessionRemoval,
    ) -> io::Result<()> {
        let mut flows = Vec::new();
        for registration in &inspected.projection.flows {
            let recorded = super::read_registered_flow(
                &inspected.dir,
                &inspected.projection,
                &registration.id,
                |_, _| Ok(()),
            )?;
            let (descriptor, _, inbox) = recorded.replay.projection.as_ref().unwrap();
            let accepted = inbox
                .entries()
                .iter()
                .map(|entry| {
                    let original = &entry.envelope.request;
                    Ok(RetainedAcceptance {
                        sender: original.sender.clone(),
                        request_id: original.request_id,
                        digest: inbox.original_fingerprint(entry)?.to_hex().to_string(),
                        acceptance: entry.acceptance.clone(),
                    })
                })
                .collect::<io::Result<Vec<_>>>()?;
            flows.push(RetiredFlow {
                address: descriptor.address.clone(),
                accepted,
            });
        }
        let marker = self.deletion_path(request.session_id);
        super::ensure_dir(marker.parent().unwrap())?;
        super::initialize_dir(&marker, |dir| {
            super::write_json(
                &dir.join("deliveries.json"),
                &DeliveryTombstone {
                    removal: request.clone(),
                    project: inspected
                        .projection
                        .project
                        .as_ref()
                        .map(|project| crate::session_meta::fingerprint_from_root(project.root())),
                    flows,
                },
            )?;
            super::write_json(&dir.join("receipt.json"), request)
        })
    }

    /// Reconcile an authenticated request without restoring deleted conversation data. None means this storage has no deletion receipt for the target.
    pub fn inspect_deleted_delivery(
        data: &Path,
        request: &DeliveryRequest,
    ) -> io::Result<Option<Acceptance>> {
        let Some(removal) = Self::inspect_session_removal(data, request.recipient.session_id.0)?
        else {
            return Ok(None);
        };
        let root = Self {
            root: data.join(super::NAMESPACE),
        };
        let retained = root.read_delivery_tombstone(&removal)?;
        if let Some(entry) = retained
            .flows
            .iter()
            .find(|flow| flow.address == request.recipient)
            .and_then(|flow| {
                flow.accepted.iter().find(|entry| {
                    entry.sender == request.sender && entry.request_id == request.request_id
                })
            })
        {
            if entry.digest
                != super::inbox::request_fingerprint(request)?
                    .to_hex()
                    .as_str()
            {
                return Err(invalid(
                    "delivery request ID was reused with different data",
                ));
            }
            if entry.acceptance.receipt.journal_id != request.recipient.instance_id.0.0 {
                return Err(invalid(
                    "delivery tombstone has a different receiver journal",
                ));
            }
            return Ok(Some(entry.acceptance.clone()));
        }
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            RecipientRejected(DeliveryFailure::RecipientDeleted {
                observed: removal.expected,
            }),
        ))
    }

    fn read_delivery_tombstone(&self, removal: &SessionRemoval) -> io::Result<DeliveryTombstone> {
        let retained: DeliveryTombstone = serde_json::from_slice(&fs::read(
            self.deletion_path(removal.session_id)
                .join("deliveries.json"),
        )?)
        .map_err(invalid)?;
        if retained.removal != *removal {
            return Err(invalid(
                "delivery tombstone belongs to another deletion receipt",
            ));
        }
        Ok(retained)
    }

    pub(super) fn retained_project(&self, removal: &SessionRemoval) -> io::Result<Option<String>> {
        self.read_delivery_tombstone(removal)
            .map(|retained| retained.project)
    }
}
