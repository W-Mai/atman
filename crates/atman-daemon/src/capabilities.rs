//! Capability negotiation observes the compiled dispatcher without initializing services.

use atman_proto::{
    JsonRpcError,
    capabilities::{
        CAPABILITY_MISMATCH, CapabilitiesRequest, CapabilitiesResponse, CapabilityMismatch,
        CapabilityMismatchKind, PROTOCOL_REVISION, SCHEMA_REVISION,
    },
    methods::Method,
};

pub(crate) fn query(
    state: &crate::DaemonState,
    params: serde_json::Value,
    principal: &str,
) -> Result<CapabilitiesResponse, JsonRpcError> {
    crate::flow_service::authorize_operator(principal)?;
    let request: CapabilitiesRequest = serde_json::from_value(params)
        .map_err(|error| JsonRpcError::invalid_params(error.to_string()))?;
    if request.methods.len() > 128 || request.methods.keys().any(|name| name.len() > 128) {
        return Err(JsonRpcError::invalid_params(
            "capability requirements exceed 128 methods or a 128-byte method name",
        ));
    }
    for (requested, supported, kind) in [
        (
            request.protocol_revision,
            PROTOCOL_REVISION,
            CapabilityMismatchKind::ProtocolRevision,
        ),
        (
            request.schema_revision,
            SCHEMA_REVISION,
            CapabilityMismatchKind::SchemaRevision,
        ),
    ] {
        if let Some(requested) = requested
            && requested != supported
        {
            return Err(mismatch(kind, requested, Some(supported), None));
        }
    }
    for (name, requested) in request.methods {
        let Some(method) = Method::from_name(&name) else {
            return Err(mismatch(
                CapabilityMismatchKind::MethodUnsupported,
                requested,
                None,
                Some(name),
            ));
        };
        let supported = method.capability().revision;
        if requested != supported {
            return Err(mismatch(
                CapabilityMismatchKind::MethodRevision,
                requested,
                Some(supported),
                Some(name),
            ));
        }
    }
    Ok(CapabilitiesResponse {
        protocol_revision: PROTOCOL_REVISION,
        schema_revision: SCHEMA_REVISION,
        host_generation: state.daemon_generation().to_owned(),
        methods: Method::ALL
            .iter()
            .map(|method| (method.name().to_owned(), method.capability()))
            .collect(),
    })
}

fn mismatch(
    kind: CapabilityMismatchKind,
    requested: u32,
    supported: Option<u32>,
    method: Option<String>,
) -> JsonRpcError {
    JsonRpcError {
        code: CAPABILITY_MISMATCH,
        message: "required capability is unsupported".into(),
        data: Some(serde_json::json!(CapabilityMismatch {
            kind,
            requested,
            supported,
            method
        })),
    }
}
