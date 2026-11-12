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
    flows::{FlowAddress, FlowCursor},
    responses::{FlowEventBatch, FlowEventPage, FlowOutputArtifact, FlowOutputDetail},
};

/// Errors raised while validating a JSON-RPC response before decoding its result body.
#[derive(Debug, Error)]
pub enum RpcDecodeError {
    #[error("unexpected JSON-RPC version {actual:?}")]
    Version { actual: String },
    #[error("JSON-RPC response id does not match the request")]
    IdMismatch {
        expected: Option<Box<Value>>,
        actual: Option<Box<Value>>,
    },
    #[error("JSON-RPC response contains both result and error")]
    ConflictingBody,
    #[error("JSON-RPC response contains neither result nor error")]
    MissingBody,
    #[error("JSON-RPC request failed: {0}")]
    Remote(#[source] Box<JsonRpcError>),
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
                expected: Some(Box::new(expected.clone())),
                actual: response.id.map(Box::new),
            });
        }
    }
    match (response.result, response.error) {
        (Some(_), Some(_)) => Err(RpcDecodeError::ConflictingBody),
        (None, None) => Err(RpcDecodeError::MissingBody),
        (None, Some(error)) => Err(RpcDecodeError::Remote(Box::new(error))),
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

/// A publication detail bound to the Flow that requested it.
///
/// `flow.output_detail` intentionally keeps the response body small and does not repeat the
/// request address. The client retains that source identity here so a late response cannot be
/// handed to a different Flow view.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BoundFlowOutputDetail {
    flow: FlowAddress,
    detail: FlowOutputDetail,
}

impl BoundFlowOutputDetail {
    pub fn flow(&self) -> &FlowAddress {
        &self.flow
    }

    pub fn detail(&self) -> &FlowOutputDetail {
        &self.detail
    }

    pub fn into_parts(self) -> (FlowAddress, FlowOutputDetail) {
        (self.flow, self.detail)
    }
}

/// Validate and bind a `flow.output_detail` reply to its request identity.
pub fn bind_flow_output_detail(
    flow: FlowAddress,
    publication_id: Uuid,
    expected_receipt: Option<&FlowCursor>,
    detail: FlowOutputDetail,
) -> Result<BoundFlowOutputDetail, FlowOutputDetailError> {
    if detail.publication_id != publication_id {
        return Err(FlowOutputDetailError::PublicationMismatch {
            expected: publication_id,
            actual: detail.publication_id,
        });
    }
    if detail.receipt.journal_id != flow.instance_id {
        return Err(FlowOutputDetailError::ReceiptSourceMismatch {
            expected: flow.instance_id,
            actual: detail.receipt.journal_id,
        });
    }
    if let Some(expected) = expected_receipt
        && &detail.receipt != expected
    {
        return Err(FlowOutputDetailError::ReceiptMismatch {
            expected: expected.clone(),
            actual: detail.receipt,
        });
    }
    if detail.has_artifact == detail.artifacts.is_empty() {
        return Err(FlowOutputDetailError::ArtifactFlagMismatch {
            has_artifact: detail.has_artifact,
            count: detail.artifacts.len(),
        });
    }
    validate_artifacts(&detail.artifacts)?;
    Ok(BoundFlowOutputDetail { flow, detail })
}

fn validate_artifacts(artifacts: &[FlowOutputArtifact]) -> Result<(), FlowOutputDetailError> {
    let mut ids = std::collections::BTreeSet::new();
    for artifact in artifacts {
        if artifact.id.trim().is_empty() {
            return Err(FlowOutputDetailError::InvalidArtifact {
                reason: "artifact id is empty".into(),
            });
        }
        if !ids.insert(&artifact.id) {
            return Err(FlowOutputDetailError::InvalidArtifact {
                reason: format!("duplicate artifact id {}", artifact.id),
            });
        }
        if artifact.digest.len() != 64
            || !artifact.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(FlowOutputDetailError::InvalidArtifact {
                reason: format!("artifact {} has an invalid digest", artifact.id),
            });
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq, Error)]
pub enum FlowOutputDetailError {
    #[error("output publication changed from {expected} to {actual}")]
    PublicationMismatch { expected: Uuid, actual: Uuid },
    #[error("output publication receipt does not match the request")]
    ReceiptMismatch {
        expected: FlowCursor,
        actual: FlowCursor,
    },
    #[error("output publication receipt belongs to another Flow journal")]
    ReceiptSourceMismatch { expected: Uuid, actual: Uuid },
    #[error("output artifact flag {has_artifact} does not match {count} artifacts")]
    ArtifactFlagMismatch { has_artifact: bool, count: usize },
    #[error("invalid output artifact: {reason}")]
    InvalidArtifact { reason: String },
}

/// A source-bound event subscription with reconnect-safe deduplication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FlowEventSubscription {
    flow: FlowAddress,
    deduper: FlowEventDeduper,
}

impl FlowEventSubscription {
    pub fn new(flow: FlowAddress, after: Option<FlowCursor>) -> Self {
        Self {
            flow,
            deduper: FlowEventDeduper::new(after),
        }
    }

    pub fn flow(&self) -> &FlowAddress {
        &self.flow
    }

    pub fn last(&self) -> Option<&FlowCursor> {
        self.deduper.last()
    }

    pub fn ingest(
        &mut self,
        page: FlowEventPage,
    ) -> Result<Vec<FlowEventBatch>, FlowEventCursorError> {
        self.deduper.ingest(page)
    }
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
        requirements.methods.clear();
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

    #[test]
    fn output_detail_is_bound_to_source_and_validated_before_install() {
        let flow = FlowAddress {
            session_id: crate::SessionId(Uuid::from_u128(2)),
            instance_id: Uuid::from_u128(3),
        };
        let publication_id = Uuid::from_u128(4);
        let receipt = FlowCursor {
            journal_id: flow.instance_id,
            sequence: 8,
            digest: "f".repeat(64),
        };
        let detail = FlowOutputDetail {
            publication_id,
            receipt: receipt.clone(),
            artifacts: vec![FlowOutputArtifact {
                id: "out-1".into(),
                bytes: 12,
                digest: "a".repeat(64),
            }],
            has_artifact: true,
        };
        let bound =
            bind_flow_output_detail(flow.clone(), publication_id, Some(&receipt), detail).unwrap();
        assert_eq!(bound.flow(), &flow);
        assert_eq!(bound.detail().artifacts[0].bytes, 12);

        let invalid = FlowOutputDetail {
            publication_id,
            receipt,
            artifacts: vec![FlowOutputArtifact {
                id: "out-1".into(),
                bytes: 12,
                digest: "not-a-digest".into(),
            }],
            has_artifact: true,
        };
        assert!(matches!(
            bind_flow_output_detail(flow, publication_id, None, invalid),
            Err(FlowOutputDetailError::InvalidArtifact { .. })
        ));
    }

    #[test]
    fn event_subscription_keeps_flow_identity_across_reconnects() {
        let flow = FlowAddress {
            session_id: crate::SessionId(Uuid::from_u128(5)),
            instance_id: Uuid::from_u128(6),
        };
        let mut subscription = FlowEventSubscription::new(flow.clone(), None);
        assert_eq!(subscription.flow(), &flow);
        assert_eq!(
            subscription
                .ingest(FlowEventPage {
                    batches: vec![event(1, 'a')],
                    next: Some(cursor(1, 'a')),
                    has_more: false,
                })
                .unwrap()
                .len(),
            1
        );
        assert_eq!(subscription.last().unwrap().sequence, 1);
    }
}
