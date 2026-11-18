//! Export compiled daemon schemas and client method metadata as JSON.

use std::collections::BTreeMap;
use std::io::{self, Write};

use atman_daemon::openapi::AtmanOpenApi;
use atman_proto::{
    capabilities::{CapabilitiesResponse, PROTOCOL_REVISION, SCHEMA_REVISION},
    methods::Method,
};
use utoipa::OpenApi;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let capabilities = CapabilitiesResponse {
        protocol_revision: PROTOCOL_REVISION,
        schema_revision: SCHEMA_REVISION,
        host_generation: uuid::Uuid::nil().to_string(),
        methods: Method::ALL
            .iter()
            .map(|method| (method.name().to_owned(), method.capability()))
            .collect(),
    };
    let operations = Method::ALL
        .iter()
        .map(|method| (method.name(), method.client_operation()))
        .collect::<BTreeMap<_, _>>();
    let export = serde_json::json!({
        "openapi": AtmanOpenApi::openapi(),
        "capabilities": capabilities,
        "operations": operations,
    });
    let mut output = io::stdout().lock();
    serde_json::to_writer_pretty(&mut output, &export)?;
    writeln!(output)?;
    Ok(())
}
