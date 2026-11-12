//! Shared client-side validation for JSON-RPC replies, capabilities and event pages.
//!
//! The daemon transports JSON-RPC over more than one connection type. These helpers keep
//! response validation and reconnect cursor handling identical for each transport without
//! depending on a particular HTTP or Unix-socket client.

use serde::de::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;
use uuid::Uuid;

use crate::{
    JsonRpcError, JsonRpcResponse,
    capabilities::{CapabilitiesRequest, CapabilitiesResponse, CapabilityMismatchKind},
    responses::{FlowEventBatch, FlowEventPage},
};

/// Errors raised while validating a JSON-RPC response before decoding its result body.
#[derive(Debug, Error)]
pub enum RpcDecodeError {
    #[error("unexpected JSON-RPC version {actual:?}")]
    Version { actual: String },
    #[error("JSON-RPC response id does not match the request")]
    IdMismatch {
        expected: Option<Value>,
        actual: Option<Value>,
    },
    #[error("JSON-RPC response contains both result and error")]
    ConflictingBody,
    #[error("JSON-RPC response contains neither result nor error")]
    MissingBody,
    #[error("JSON-RPC request failed: {0}")]
    Remote(#[source] JsonRpcError),
    #[error("invalid JSON-RPC result: {source}")]
    InvalidResult {
        #[source]
        source: serde_json::Error,
    },
}

/// Decode a typed JSON-RPC result after checking the envelope and request identity.
///
/// Pass the original request id to reject a late reply from another in-flight request. An
/// omitted expected id skips identity checking and is useful for transports that already
/// correlate replies outside the JSON-RPC envelope.
pub fn decode_response<T: DeserializeOwned>(
    response: JsonRpcResponse,
    expected_id: Option<&Value>,
) -> Result<T, RpcDecodeError> {
    if response.jsonrpc != crate::JSONRPC_VERSION {
        return Err(RpcDecodeError::Version {
            actual: response.jsonrpc,
        });
    }
    if let Some(expected) = expected_id {
        if response.id.as_ref() != Some(expected) {
            return Err(RpcDecodeError::IdMismatch {
                expected: Some(expected.clone()),
                actual: response.id,
            });
        }
    }
    match (response.result, response.error) {
        (Some(_), Some(_)) => Err(RpcDecodeError::ConflictingBody),
        (None, None) => Err(RpcDecodeError::MissingBody),
        (None, Some(error)) => Err(RpcDecodeError::Remote(error)),
        (Some(result), None) => serde_json::from_value(result)
            .map_err(|source| RpcDecodeError::InvalidResult { source }),
    }
}

/// Client-side view of the daemon capability catalog.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapabilitySet {
    response: CapabilitiesResponse,
}

impl CapabilitySet {
    pub fn new(response: CapabilitiesResponse) -> Self {
        Self { response }
    }

    pub fn response(&self) -> &CapabilitiesResponse {
        &self.response
    }

    /// Check a capability request before selecting a wire contract.
    ///
    /// Every requested method must be present at the requested revision. A lower daemon
    /// revision is rejected rather than silently falling back to a different request shape.
    pub fn require(
        &self,
        requirements: &CapabilitiesRequest,
    ) -> Result<(), CapabilityRequirementError> {
        if let Some(requested) = requirements.protocol_revision {
            if requested != self.response.protocol_revision {
                return Err(CapabilityRequirementError::revision(
                    CapabilityMismatchKind::ProtocolRevision,
                    requested,
                    Some(self.response.protocol_revision),
                    None,
                ));
            }
        }
        if let Some(requested) = requirements.schema_revision {
            if requested != self.response.schema_revision {
                return Err(CapabilityRequirementError::revision(
                    CapabilityMismatchKind::SchemaRevision,
                    requested,
                    Some(self.response.schema_revision),
                    None,
                ));
            }
        }
        for (method, requested) in &requirements.methods {
            let Some(capability) = self.response.methods.get(method) else {
                return Err(CapabilityRequirementError::revision(
                    CapabilityMismatchKind::MethodUnsupported,
                    *requested,
                    None,
                    Some(method.clone()),
                ));
            };
            if capability.revision != *requested {
                return Err(CapabilityRequirementError::revision(
                    CapabilityMismatchKind::MethodRevision,
                    *requested,
                    Some(capability.revision),
                    Some(method.clone()),
                ));
            }
        }
        Ok(())
    }
}

/// A client-side capability requirement mismatch.
#[derive(Clone, Debug, Eq, PartialEq, Error)]
#[error("capability mismatch: {kind:?} requested revision {requested}, supported {supported:?}")]
pub struct CapabilityRequirementError {
    pub kind: CapabilityMismatchKind,
    pub requested: u32,
    pub supported: Option<u32>,
    pub method: Option<String>,
}

impl CapabilityRequirementError {
    fn revision(
        kind: CapabilityMismatchKind,
        requested: u32,
        supported: Option<u32>,
        method: Option<String>,
    ) -> Self {
        Self {
            kind,
            requested,
            supported,
            method,
        }
    }
}

/// A reconnect-safe event page cursor.
///
/// Pages may repeat batches after a reconnect. Batches from the same journal at or before the
/// last accepted sequence are ignored; a different journal is rejected so a client cannot merge
/// events from unrelated Flow histories. The exact same sequence with a different digest is also
/// rejected as a cursor conflict.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FlowEventDeduper {
    last: Option<crate::flows::FlowCursor>,
}

impl FlowEventDeduper {
    pub fn new(after: Option<crate::flows::FlowCursor>) -> Self {
        Self { last: after }
    }

    pub fn last(&self) -> Option<&crate::flows::FlowCursor> {
        self.last.as_ref()
    }

    /// Return only newly observed batches and advance the durable cursor.
    pub fn ingest(
        &mut self,
        page: FlowEventPage,
    ) -> Result<Vec<FlowEventBatch>, FlowEventCursorError> {
        let mut fresh = Vec::new();
        let mut last = self.last.clone();
        for batch in page.batches {
            let cursor = &batch.receipt;
            if let Some(previous) = &last {
                if cursor.journal_id != previous.journal_id {
                    return Err(FlowEventCursorError::JournalChanged {
                        expected: previous.journal_id,
                        actual: cursor.journal_id,
                    });
                }
                if cursor.sequence < previous.sequence {
                    continue;
                }
                if cursor.sequence == previous.sequence {
                    if cursor.digest == previous.digest {
                        continue;
                    }
                    return Err(FlowEventCursorError::ConflictingDigest {
                        sequence: cursor.sequence,
                        expected: previous.digest.clone(),
                        actual: cursor.digest.clone(),
                    });
                }
            }
            last = Some(cursor.clone());
            fresh.push(batch);
        }
        self.last = last;
        Ok(fresh)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum FlowEventCursorError {
    #[error("event cursor changed journal from {expected} to {actual}")]
    JournalChanged { expected: Uuid, actual: Uuid },
    #[error("event cursor sequence {sequence} has conflicting digests")]
    ConflictingDigest {
        sequence: u64,
        expected: String,
        actual: String,
    },
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::{
        JSONRPC_VERSION,
        capabilities::{MethodCapability, PROTOCOL_REVISION, SCHEMA_REVISION},
        responses::PingResponse,
    };

    fn cursor(sequence: u64, digest: char) -> crate::flows::FlowCursor {
        crate::flows::FlowCursor {
            journal_id: Uuid::from_u128(1),
            sequence,
            digest: digest.to_string().repeat(64),
        }
    }

    fn event(sequence: u64, digest: char) -> FlowEventBatch {
        FlowEventBatch {
            receipt: cursor(sequence, digest),
            facts: vec![serde_json::json!({"sequence": sequence})],
        }
    }

    #[test]
    fn decode_response_validates_identity_and_body() {
        let id = serde_json::json!(7);
        let response = JsonRpcResponse::ok(
            Some(id.clone()),
            serde_json::json!({"pong": true, "version": "x"}),
        );
        let decoded: PingResponse = decode_response(response, Some(&id)).unwrap();
        assert!(decoded.pong);

        let wrong_id = JsonRpcResponse::ok(Some(serde_json::json!(8)), serde_json::json!({}));
        assert!(matches!(
            decode_response::<PingResponse>(wrong_id, Some(&id)),
            Err(RpcDecodeError::IdMismatch { .. })
        ));

        let mut wrong_version = JsonRpcResponse::ok(None, serde_json::json!({}));
        wrong_version.jsonrpc = "1.0".into();
        assert!(matches!(
            decode_response::<PingResponse>(wrong_version, None),
            Err(RpcDecodeError::Version { .. })
        ));
    }

    #[test]
    fn capabilities_reject_unknown_and_lower_revisions() {
        let mut methods = BTreeMap::new();
        methods.insert(
            "flow.events".into(),
            MethodCapability {
                revision: 1,
                request_schema: None,
                response_schema: Some("FlowEventPage".into()),
            },
        );
        let set = CapabilitySet::new(CapabilitiesResponse {
            protocol_revision: PROTOCOL_REVISION,
            schema_revision: SCHEMA_REVISION,
            host_generation: "host".into(),
            methods,
        });
        let mut requirements = CapabilitiesRequest::default();
        requirements.methods.insert("flow.events".into(), 2);
        let error = set.require(&requirements).unwrap_err();
        assert_eq!(error.kind, CapabilityMismatchKind::MethodRevision);
        requirements.methods.insert("flow.output".into(), 1);
        assert_eq!(
            set.require(&requirements).unwrap_err().kind,
            CapabilityMismatchKind::MethodUnsupported
        );
    }

    #[test]
    fn event_pages_skip_reconnect_duplicates_and_reject_conflicts() {
        let mut deduper = FlowEventDeduper::default();
        let page = FlowEventPage {
            batches: vec![event(1, 'a'), event(2, 'b')],
            next: Some(cursor(2, 'b')),
            has_more: true,
        };
        assert_eq!(deduper.ingest(page).unwrap().len(), 2);
        let reconnect = FlowEventPage {
            batches: vec![event(1, 'a'), event(2, 'b'), event(3, 'c')],
            next: Some(cursor(3, 'c')),
            has_more: false,
        };
        let fresh = deduper.ingest(reconnect).unwrap();
        assert_eq!(fresh.len(), 1);
        assert_eq!(deduper.last().unwrap().sequence, 3);

        let conflict = FlowEventPage {
            batches: vec![event(3, 'd')],
            next: None,
            has_more: false,
        };
        assert!(matches!(
            deduper.ingest(conflict),
            Err(FlowEventCursorError::ConflictingDigest { sequence: 3, .. })
        ));
        assert_eq!(JSONRPC_VERSION, "2.0");
    }
}
