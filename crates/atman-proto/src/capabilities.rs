//! Capability discovery is independent of the package version and opens no execution owner.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

pub const PROTOCOL_REVISION: u32 = 1;
pub const SCHEMA_REVISION: u32 = 1;
pub const CAPABILITY_MISMATCH: i32 = -32001;

#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CapabilitiesRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol_revision: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema_revision: Option<u32>,
    /// Required method revisions. An absent method is unsupported, not revision zero.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub methods: BTreeMap<String, u32>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct MethodCapability {
    pub revision: u32,
    /// OpenAPI component name. Omission means no fixed request DTO is advertised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_schema: Option<String>,
    /// OpenAPI component name. Omission means no fixed response DTO is advertised.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct CapabilitiesResponse {
    pub protocol_revision: u32,
    pub schema_revision: u32,
    /// Existing daemon generation; it does not identify a durable journal.
    pub host_generation: String,
    pub methods: BTreeMap<String, MethodCapability>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityMismatchKind {
    ProtocolRevision,
    SchemaRevision,
    MethodRevision,
    MethodUnsupported,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct CapabilityMismatch {
    pub kind: CapabilityMismatchKind,
    pub requested: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
}
