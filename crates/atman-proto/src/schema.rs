use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use utoipa::ToSchema;

use crate::{
    PROJECTION_EVENT_SCHEMA_VERSION, PROTOCOL_VERSION, RpcKind, RpcMethod, SNAPSHOT_SCHEMA_VERSION,
};

const JSON_SCHEMA_DIALECT: &str = "https://json-schema.org/draft/2020-12/schema";
const PROTOCOL_SCHEMA_ID: &str = "https://atman.run/schemas/daemon-protocol-v1.json";

#[derive(Debug, thiserror::Error)]
pub enum ProtocolSchemaError {
    #[error("could not serialize protocol schema: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error("schema name `{name}` resolves to incompatible definitions")]
    ConflictingDefinition { name: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct MethodPayloadSchema {
    pub rust_type: String,
    pub schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize)]
pub struct MethodManifest {
    pub name: String,
    pub kind: RpcKind,
    pub revision: u32,
    pub params: MethodPayloadSchema,
    pub result: MethodPayloadSchema,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProtocolManifest {
    pub protocol_version: u32,
    pub snapshot_schema_version: u32,
    pub event_schema_version: u32,
    pub methods: Vec<MethodManifest>,
}

#[derive(Debug, Clone, Serialize)]
struct ProtocolSchemaDocument {
    #[serde(rename = "$schema")]
    dialect: &'static str,
    #[serde(rename = "$id")]
    id: &'static str,
    title: &'static str,
    #[serde(rename = "anyOf")]
    any_of: Vec<serde_json::Value>,
    #[serde(rename = "$defs")]
    definitions: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone)]
pub struct ProtocolArtifacts {
    pub manifest: String,
    pub schema: String,
}

pub(crate) struct RpcMethodSchema {
    manifest: MethodManifest,
    definitions: BTreeMap<String, serde_json::Value>,
}

pub fn generate_protocol_artifacts() -> Result<ProtocolArtifacts, ProtocolSchemaError> {
    let mut methods = Vec::with_capacity(crate::methods::ALL.len());
    let mut definitions = BTreeMap::new();
    let mut payload_schemas = Vec::with_capacity(crate::methods::ALL.len() * 2);
    let mut payload_schema_ids = BTreeSet::new();

    for descriptor in crate::methods::ALL {
        let method = descriptor.materialize_schema()?;
        merge_definitions(&mut definitions, method.definitions)?;
        for schema in [
            &method.manifest.params.schema,
            &method.manifest.result.schema,
        ] {
            if payload_schema_ids.insert(serde_json::to_string(schema)?) {
                payload_schemas.push(schema.clone());
            }
        }
        methods.push(method.manifest);
    }
    for (payload, standalone_definitions) in [
        materialize_type::<crate::JsonRpcRequest>("JsonRpcRequest")?,
        materialize_type::<crate::JsonRpcResponse>("JsonRpcResponse")?,
        materialize_type::<crate::JsonRpcError>("JsonRpcError")?,
        materialize_type::<crate::ServerEventEnvelope>("ServerEventEnvelope")?,
        materialize_type::<crate::ProjectionEventEnvelope>("ProjectionEventEnvelope")?,
    ] {
        merge_definitions(&mut definitions, standalone_definitions)?;
        if payload_schema_ids.insert(serde_json::to_string(&payload.schema)?) {
            payload_schemas.push(payload.schema);
        }
    }

    let manifest = ProtocolManifest {
        protocol_version: PROTOCOL_VERSION,
        snapshot_schema_version: SNAPSHOT_SCHEMA_VERSION,
        event_schema_version: PROJECTION_EVENT_SCHEMA_VERSION,
        methods,
    };
    let schema = ProtocolSchemaDocument {
        dialect: JSON_SCHEMA_DIALECT,
        id: PROTOCOL_SCHEMA_ID,
        title: "atman daemon protocol payloads",
        any_of: payload_schemas,
        definitions,
    };

    Ok(ProtocolArtifacts {
        manifest: pretty_json(&manifest)?,
        schema: pretty_json(&schema)?,
    })
}

pub fn protocol_openapi_components()
-> Result<utoipa::openapi::schema::Components, ProtocolSchemaError> {
    let document: serde_json::Value = serde_json::from_str(&generate_protocol_artifacts()?.schema)?;
    let definitions = document["$defs"]
        .as_object()
        .expect("generated protocol schema must contain object definitions");
    let mut schemas = Vec::with_capacity(definitions.len());
    for (name, definition) in definitions {
        let mut definition = definition.clone();
        rewrite_definition_refs(&mut definition, "#/$defs/", "#/components/schemas/");
        rewrite_openapi_any_value_schemas(&mut definition);
        schemas.push((
            name.clone(),
            serde_json::from_value::<utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>>(
                definition,
            )?,
        ));
    }
    Ok(utoipa::openapi::schema::ComponentsBuilder::new()
        .schemas_from_iter(schemas)
        .build())
}

fn rewrite_openapi_any_value_schemas(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) if fields.is_empty() => {
            *value = serde_json::json!({
                "type": ["object", "array", "string", "number", "integer", "boolean", "null"]
            });
        }
        serde_json::Value::Object(fields) => {
            for value in fields.values_mut() {
                rewrite_openapi_any_value_schemas(value);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                rewrite_openapi_any_value_schemas(item);
            }
        }
        _ => {}
    }
}

pub(crate) fn materialize_method<M: RpcMethod>() -> Result<RpcMethodSchema, ProtocolSchemaError> {
    let method_name = schema_name(M::NAME);
    let (params, mut definitions) = materialize_type::<M::Params>(&format!("{method_name}Params"))?;
    let (result, result_definitions) =
        materialize_type::<M::Output>(&format!("{method_name}Result"))?;
    merge_definitions(&mut definitions, result_definitions)?;
    Ok(RpcMethodSchema {
        manifest: MethodManifest {
            name: M::NAME.to_owned(),
            kind: M::KIND,
            revision: M::REVISION,
            params,
            result,
        },
        definitions,
    })
}

fn materialize_type<T: ToSchema>(
    fallback_name: &str,
) -> Result<(MethodPayloadSchema, BTreeMap<String, serde_json::Value>), ProtocolSchemaError> {
    let mut referenced = Vec::new();
    T::schemas(&mut referenced);
    let mut definitions = BTreeMap::new();
    for (name, schema) in referenced {
        insert_definition(&mut definitions, name, schema)?;
    }

    let rust_type = T::name().into_owned();
    let definition_name = if std::any::type_name::<T>().contains('<') {
        fallback_name.to_owned()
    } else {
        rust_type.clone()
    };
    insert_definition(&mut definitions, definition_name.clone(), T::schema())?;
    let schema = serde_json::json!({ "$ref": format!("#/$defs/{definition_name}") });
    Ok((MethodPayloadSchema { rust_type, schema }, definitions))
}

fn schema_name(method: &str) -> String {
    let mut name = String::new();
    for segment in method.split(|character: char| !character.is_ascii_alphanumeric()) {
        let mut characters = segment.chars();
        if let Some(first) = characters.next() {
            name.extend(first.to_uppercase());
            name.extend(characters);
        }
    }
    name
}

fn insert_definition(
    definitions: &mut BTreeMap<String, serde_json::Value>,
    name: String,
    schema: utoipa::openapi::RefOr<utoipa::openapi::schema::Schema>,
) -> Result<(), ProtocolSchemaError> {
    let mut schema = serde_json::to_value(schema)?;
    rewrite_component_refs(&mut schema);
    if let Some(existing) = definitions.get(&name)
        && existing != &schema
    {
        return Err(ProtocolSchemaError::ConflictingDefinition { name });
    }
    definitions.insert(name, schema);
    Ok(())
}

fn merge_definitions(
    target: &mut BTreeMap<String, serde_json::Value>,
    incoming: BTreeMap<String, serde_json::Value>,
) -> Result<(), ProtocolSchemaError> {
    for (name, schema) in incoming {
        if let Some(existing) = target.get(&name)
            && existing != &schema
        {
            return Err(ProtocolSchemaError::ConflictingDefinition { name });
        }
        target.insert(name, schema);
    }
    Ok(())
}

fn rewrite_component_refs(value: &mut serde_json::Value) {
    rewrite_definition_refs(value, "#/components/schemas/", "#/$defs/");
}

fn rewrite_definition_refs(value: &mut serde_json::Value, from: &str, to: &str) {
    match value {
        serde_json::Value::Object(fields) => {
            if let Some(serde_json::Value::String(reference)) = fields.get_mut("$ref")
                && let Some(name) = reference.strip_prefix(from)
            {
                *reference = format!("{to}{name}");
            }
            for value in fields.values_mut() {
                rewrite_definition_refs(value, from, to);
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                rewrite_definition_refs(item, from, to);
            }
        }
        _ => {}
    }
}

fn pretty_json(value: &impl Serialize) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(value).map(|mut output| {
        output.push('\n');
        output
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_protocol_artifacts_are_current() {
        let generated = generate_protocol_artifacts().unwrap();
        assert_eq!(
            generated.manifest,
            include_str!("../schema/method-manifest.json")
        );
        assert_eq!(
            generated.schema,
            include_str!("../schema/protocol.schema.json")
        );
    }

    #[test]
    fn every_method_schema_reference_resolves() {
        let generated = generate_protocol_artifacts().unwrap();
        let schema: serde_json::Value = serde_json::from_str(&generated.schema).unwrap();
        let manifest: serde_json::Value = serde_json::from_str(&generated.manifest).unwrap();
        let definitions = schema["$defs"].as_object().unwrap();
        let mut references = Vec::new();
        collect_references(&schema, &mut references);

        let expected_roots = manifest["methods"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|method| {
                [
                    &method["params"]["schema"]["$ref"],
                    &method["result"]["schema"]["$ref"],
                ]
            })
            .filter_map(serde_json::Value::as_str)
            .chain([
                "#/$defs/JsonRpcRequest",
                "#/$defs/JsonRpcResponse",
                "#/$defs/JsonRpcError",
                "#/$defs/ServerEventEnvelope",
                "#/$defs/ProjectionEventEnvelope",
            ])
            .collect::<BTreeSet<_>>();
        let actual_roots = schema["anyOf"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|schema| schema["$ref"].as_str())
            .collect::<BTreeSet<_>>();
        assert_eq!(actual_roots, expected_roots);
        for reference in references {
            let name = reference.strip_prefix("#/$defs/").unwrap();
            assert!(
                definitions.contains_key(name),
                "missing schema definition `{name}`"
            );
        }
    }

    #[test]
    fn arbitrary_json_wire_fields_are_not_restricted_to_objects() {
        let generated = generate_protocol_artifacts().unwrap();
        let schema: serde_json::Value = serde_json::from_str(&generated.schema).unwrap();
        for pointer in [
            "/$defs/JsonRpcRequest/properties/id",
            "/$defs/JsonRpcResponse/properties/id",
            "/$defs/JsonRpcResponse/properties/result",
            "/$defs/JsonRpcError/properties/data",
            "/$defs/ResolvePromptRequest/properties/answer",
        ] {
            assert_eq!(schema.pointer(pointer), Some(&serde_json::json!({})));
        }
    }

    #[test]
    fn arbitrary_json_wire_fields_remain_valid_openapi_components() {
        let components = protocol_openapi_components().unwrap();
        let components = serde_json::to_value(components).unwrap();
        let types = components
            .pointer("/schemas/JsonRpcResponse/properties/result/type")
            .and_then(serde_json::Value::as_array)
            .unwrap();
        assert!(types.iter().any(|value| value == "array"));
        assert!(types.iter().any(|value| value == "object"));
        assert!(types.iter().any(|value| value == "string"));
        assert!(types.iter().any(|value| value == "null"));
    }

    fn collect_references<'a>(value: &'a serde_json::Value, references: &mut Vec<&'a str>) {
        match value {
            serde_json::Value::Object(fields) => {
                if let Some(reference) = fields.get("$ref").and_then(serde_json::Value::as_str) {
                    references.push(reference);
                }
                for value in fields.values() {
                    collect_references(value, references);
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    collect_references(item, references);
                }
            }
            _ => {}
        }
    }
}
