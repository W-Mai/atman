//! Typed delivery facts and their single inbox projection.

use std::collections::{BTreeMap, HashMap};
use std::io;

use atman_rt::inbox::PendingBatch;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{FlowRef, invalid};
use crate::event_log::journal::CommitReceipt;
use crate::message::ImageSource;
use crate::user_input::UserInputPresentation;

/// Authenticated provenance supplied by the host, never an authorization grant by itself.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", content = "identity", rename_all = "snake_case")]
pub enum SenderIdentity {
    User(Uuid),
    Flow(FlowRef),
    Host(Uuid),
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryMode {
    AtBoundary,
    NextTurn,
}

/// Stable source position; separate events in one committed batch retain separate identities.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FlowEventRef {
    pub flow: FlowRef,
    pub sequence: u64,
    pub index: u32,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum InboxPayload {
    UserSubmission {
        text: String,
        images: Vec<ImageSource>,
        invocation: BTreeMap<String, Value>,
        presentation: Option<UserInputPresentation>,
    },
    Interjection {
        text: String,
    },
    FlowEvent {
        source: FlowEventRef,
        event: String,
        content: Value,
    },
    Watcher {
        subscription: Uuid,
        source: FlowEventRef,
        content: Value,
    },
    Timer {
        timer: Uuid,
        due_at: DateTime<Utc>,
        content: Value,
    },
    Internal {
        kind: String,
        content: Value,
    },
}

/// The host authenticates `sender` before calling storage, including on retries.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DeliveryRequest {
    pub request_id: Uuid,
    pub sender: SenderIdentity,
    pub recipient: FlowRef,
    pub delivery: DeliveryMode,
    pub cause: Option<FlowEventRef>,
    pub correlation: Option<Uuid>,
    pub scope: Option<Uuid>,
    pub payload: InboxPayload,
}

impl DeliveryRequest {
    pub(crate) fn validate_source(&self) -> io::Result<()> {
        let valid = match (&self.sender, &self.payload) {
            (
                SenderIdentity::User(_),
                InboxPayload::UserSubmission { .. } | InboxPayload::Interjection { .. },
            ) => true,
            (SenderIdentity::Flow(_), InboxPayload::Interjection { .. }) => true,
            (SenderIdentity::Flow(sender), InboxPayload::FlowEvent { source, .. }) => {
                sender == &source.flow
            }
            (
                SenderIdentity::Host(_),
                InboxPayload::Watcher { .. }
                | InboxPayload::Timer { .. }
                | InboxPayload::Internal { .. },
            ) => true,
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(invalid(
                "delivery payload does not match authenticated sender",
            ))
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct AcceptedEnvelope {
    pub id: Uuid,
    pub accepted_at: DateTime<Utc>,
    pub request: DeliveryRequest,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum InboxDisposition {
    Pending,
    Cancelled { by: CommitReceipt },
}

/// Acceptance is distinct from handling; retries retain the original acceptance receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Acceptance {
    pub message_id: Uuid,
    pub receipt: CommitReceipt,
    pub disposition: InboxDisposition,
}

#[derive(Clone, Debug, Serialize)]
pub struct InboxEntry {
    pub envelope: AcceptedEnvelope,
    pub acceptance: Acceptance,
}

#[derive(Default)]
pub(super) struct Inbox {
    entries: Vec<InboxEntry>,
    requests: HashMap<(SenderIdentity, Uuid), usize>,
    messages: HashMap<Uuid, usize>,
    pending: PendingBatch<Uuid>,
}

impl Inbox {
    pub fn entries(&self) -> &[InboxEntry] {
        &self.entries
    }

    pub fn pending(&self) -> impl Iterator<Item = &AcceptedEnvelope> {
        self.pending
            .iter()
            .map(|id| &self.entries[self.messages[id]].envelope)
    }

    pub fn previous(&self, request: &DeliveryRequest) -> io::Result<Option<Acceptance>> {
        let Some(&index) = self
            .requests
            .get(&(request.sender.clone(), request.request_id))
        else {
            return Ok(None);
        };
        let entry = &self.entries[index];
        if entry.envelope.request != *request {
            return Err(invalid(
                "delivery request ID was reused with different data",
            ));
        }
        Ok(Some(entry.acceptance.clone()))
    }

    pub fn validate(&self, envelope: &AcceptedEnvelope) -> io::Result<()> {
        envelope.request.validate_source()?;
        if self.messages.contains_key(&envelope.id) || self.previous(&envelope.request)?.is_some() {
            return Err(invalid("duplicate acceptance fact"));
        }
        Ok(())
    }

    /// Called only after validation and a successful durable append, or during validated replay.
    pub fn apply(&mut self, envelope: AcceptedEnvelope, receipt: CommitReceipt) -> Acceptance {
        let id = envelope.id;
        let acceptance = Acceptance {
            message_id: id,
            receipt,
            disposition: InboxDisposition::Pending,
        };
        self.requests.insert(
            (envelope.request.sender.clone(), envelope.request.request_id),
            self.entries.len(),
        );
        self.messages.insert(id, self.entries.len());
        self.entries.push(InboxEntry {
            envelope,
            acceptance: acceptance.clone(),
        });
        self.pending.push_back(id);
        acceptance
    }

    pub fn close(&mut self, receipt: &CommitReceipt) {
        for id in self.pending.iter() {
            self.entries[self.messages[id]].acceptance.disposition = InboxDisposition::Cancelled {
                by: receipt.clone(),
            };
        }
        self.pending.clear();
    }
}
