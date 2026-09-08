use crate::error::RuntimeError;
use crate::storage;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;
use atman_dsl::ast::{Expr, FlowDecl, Literal, Stmt, TypeExpr};
use std::path::Path;

const DEFAULT_SEARCH_LIMIT: usize = 10;
const MAX_SEARCH_LIMIT: usize = 50;

pub struct FlowList;
pub struct FlowInstances;
pub struct FlowSearch;
pub struct FlowDescribe;

#[derive(Clone)]
struct FlowParameter {
    name: String,
    ty: String,
    default: Option<Value>,
}

#[derive(Clone)]
struct FlowEntry {
    name: String,
    reference: String,
    version: String,
    summary: String,
    params: Vec<FlowParameter>,
}

struct FlowFile {
    name: String,
    description: String,
    flows: Vec<FlowEntry>,
}

struct FlowCatalog {
    fingerprint: String,
    files: Vec<FlowFile>,
}

impl FlowCatalog {
    fn load() -> Result<Self, RuntimeError> {
        let config_dir = storage::config_dir()
            .map_err(|error| RuntimeError::ToolFailed(format!("flow catalog: {error}")))?;
        Self::load_from(&config_dir.join("commands"))
    }

    fn load_from(commands_dir: &Path) -> Result<Self, RuntimeError> {
        let mut files = Vec::new();
        if commands_dir.is_dir() {
            let read = std::fs::read_dir(commands_dir).map_err(|error| {
                RuntimeError::ToolFailed(format!(
                    "flow catalog: read {}: {error}",
                    commands_dir.display()
                ))
            })?;
            for entry in read.flatten() {
                let path = entry.path();
                if path.extension().and_then(|extension| extension.to_str()) != Some("at") {
                    continue;
                }
                if let Ok(file) = scan_flow_file(&path) {
                    files.push(file);
                }
            }
        }
        files.sort_by(|left, right| left.name.cmp(&right.name));
        let mut hasher = blake3::Hasher::new();
        for file in &files {
            for flow in &file.flows {
                hasher.update(flow.reference.as_bytes());
                hasher.update(&[0]);
                hasher.update(flow.version.as_bytes());
                hasher.update(&[0]);
            }
        }
        Ok(Self {
            fingerprint: format!("blake3:{}", hasher.finalize().to_hex()),
            files,
        })
    }

    fn searchable_entries(&self) -> Vec<&FlowEntry> {
        let mut entries = self
            .files
            .iter()
            .flat_map(|file| file.flows.iter())
            .filter(|flow| flow.name != "describe")
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| left.reference.cmp(&right.reference));
        entries
    }

    fn find(&self, flow_ref: &str) -> Option<&FlowEntry> {
        if flow_ref.contains('@') {
            let normalized = normalize_flow_ref(flow_ref)?;
            return self
                .files
                .iter()
                .flat_map(|file| file.flows.iter())
                .find(|flow| flow.reference == normalized);
        }
        let file_name = normalize_flow_file(flow_ref)?;
        self.files
            .iter()
            .find(|file| file.name == file_name)?
            .flows
            .iter()
            .find(|flow| flow.name != "describe")
    }

    fn legacy_value(&self) -> Value {
        Value::List(
            self.files
                .iter()
                .map(|file| {
                    Value::Struct(vec![
                        ("file".into(), Value::Str(file.name.clone())),
                        ("description".into(), Value::Str(file.description.clone())),
                        (
                            "flows".into(),
                            Value::List(file.flows.iter().map(legacy_flow_value).collect()),
                        ),
                    ])
                })
                .collect(),
        )
    }
}

impl Tool for FlowList {
    fn name(&self) -> &str {
        "flow.list"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Return the complete flow catalog for compatibility. The result is unbounded; use \
             flow.search followed by flow.describe for model-driven discovery.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}})
    }

    fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move { Ok(FlowCatalog::load()?.legacy_value()) })
    }
}

impl Tool for FlowInstances {
    fn name(&self) -> &str {
        "flow.instances"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "List spawned flow instances visible to the current session and return a single-use spawn_token. \
             Inspect running work, reuse suitable instances, and kill obsolete flows before passing the token to flow.spawn.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object", "properties": {}, "additionalProperties": false})
    }

    fn call<'a>(&'a self, _args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let registry = ctx.flow_registry.as_ref().ok_or_else(|| {
                RuntimeError::ToolFailed("flow.instances: no flow registry available".into())
            })?;
            let identity = ctx.flow_identity.as_ref().ok_or_else(|| {
                RuntimeError::ToolFailed(
                    "flow.instances: trusted caller flow identity is unavailable".into(),
                )
            })?;
            let (mut instances, token) = registry.inspect_for_spawn(identity);
            const MAX_INSTANCES: usize = 50;
            let truncated = instances.len().saturating_sub(MAX_INSTANCES);
            if truncated > 0 {
                instances.drain(..truncated);
            }
            let items = instances
                .into_iter()
                .map(|instance| {
                    Value::Struct(vec![
                        ("handle".into(), Value::Str(instance.handle)),
                        ("goal".into(), Value::Str(instance.goal)),
                        ("model".into(), Value::Str(instance.model)),
                        ("status".into(), Value::Str(instance.status)),
                        (
                            "run_id".into(),
                            Value::Str(instance.child_run_id.0.to_string()),
                        ),
                        (
                            "started_at".into(),
                            Value::Str(instance.started_at.to_rfc3339()),
                        ),
                    ])
                })
                .collect();
            Ok(Value::Struct(vec![
                ("instances".into(), Value::List(items)),
                ("spawn_token".into(), Value::Str(token)),
                ("truncated".into(), Value::Int(truncated as i64)),
            ]))
        })
    }
}

impl Tool for FlowSearch {
    fn name(&self) -> &str {
        "flow.search"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Search installed DSL flows by ranked keywords without loading the complete catalog into context. \
             Results contain an exact flow ref, a source fingerprint, and a short summary. \
             Use flow.describe on one result before flow.spawn when its parameters are unknown.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Case-insensitive keywords ranked across flow name, ref, and summary. Empty string lists the first page."},
                "limit": {"type": "integer", "minimum": 1, "maximum": MAX_SEARCH_LIMIT, "default": DEFAULT_SEARCH_LIMIT},
                "cursor": {"type": "string", "minLength": 1, "description": "Pagination only. Omit this field for the first page. For a later page, pass the non-empty next_cursor returned by the immediately preceding flow.search call verbatim; never send an empty or invented value."}
            },
            "required": ["query"],
            "additionalProperties": false
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let query = string_arg(&args, "query", "flow.search")?;
            let limit = limit_arg(&args, "flow.search")?;
            let cursor = optional_string_arg(&args, "cursor", "flow.search")?;
            search_catalog(&FlowCatalog::load()?, query, limit, cursor.as_deref())
        })
    }
}

impl Tool for FlowDescribe {
    fn name(&self) -> &str {
        "flow.describe"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Describe one installed DSL flow. Returns its exact ref, current source fingerprint, \
             summary, and parameter contract. Accepts an exact ref or installed flow-file shorthand. \
             An optional version rejects stale search results.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "ref": {"type": "string", "description": "Exact flow ref returned by flow.search, or installed flow file such as subagent.at."},
                "version": {"type": "string", "minLength": 1, "description": "Staleness guard. Pass the non-empty source fingerprint returned by the current flow.search result verbatim. Omit this field when no fingerprint is available; never send an empty or invented value."}
            },
            "required": ["ref"],
            "additionalProperties": false
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let flow_ref = string_arg(&args, "ref", "flow.describe")?;
            let expected_version = optional_string_arg(&args, "version", "flow.describe")?;
            describe_catalog_entry(&FlowCatalog::load()?, flow_ref, expected_version.as_deref())
        })
    }
}

fn search_catalog(
    catalog: &FlowCatalog,
    query: &str,
    limit: usize,
    cursor: Option<&str>,
) -> ToolResult {
    let normalized_query = query.trim().to_lowercase();
    let search_fingerprint = search_fingerprint(&catalog.fingerprint, &normalized_query);
    let offset = cursor
        .map(|cursor| parse_cursor(cursor, &search_fingerprint))
        .transpose()?
        .unwrap_or(0);
    let mut entries = catalog
        .searchable_entries()
        .into_iter()
        .filter_map(|flow| search_score(flow, &normalized_query).map(|score| (score, flow)))
        .collect::<Vec<_>>();
    entries.sort_by(|(left_score, left), (right_score, right)| {
        right_score
            .cmp(left_score)
            .then_with(|| left.reference.cmp(&right.reference))
    });
    if offset > entries.len() {
        return Err(RuntimeError::ToolFailed(
            "flow.search: cursor offset is outside the result set".into(),
        ));
    }
    let end = (offset + limit).min(entries.len());
    let items = entries[offset..end]
        .iter()
        .map(|(_, flow)| {
            Value::Struct(vec![
                ("ref".into(), Value::Str(flow.reference.clone())),
                ("version".into(), Value::Str(flow.version.clone())),
                ("summary".into(), Value::Str(flow.summary.clone())),
            ])
        })
        .collect();
    let next_cursor = if end < entries.len() {
        Value::Str(format!("{search_fingerprint}:{end}"))
    } else {
        Value::Unit
    };
    Ok(Value::Struct(vec![
        ("items".into(), Value::List(items)),
        ("next_cursor".into(), next_cursor),
        ("total".into(), Value::Int(entries.len() as i64)),
        (
            "catalog_fingerprint".into(),
            Value::Str(catalog.fingerprint.clone()),
        ),
    ]))
}

fn search_score(flow: &FlowEntry, query: &str) -> Option<u32> {
    if query.is_empty() {
        return Some(0);
    }
    let name = flow.name.to_lowercase();
    let reference = flow.reference.to_lowercase();
    let summary = flow.summary.to_lowercase();
    let mut score = if name.contains(query) || reference.contains(query) || summary.contains(query)
    {
        100
    } else {
        0
    };
    for token in query
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
    {
        if name == token {
            score += 24;
        } else if name.contains(token) {
            score += 12;
        }
        if reference.contains(token) {
            score += 8;
        }
        if summary.contains(token) {
            score += 4;
        }
    }
    (score > 0).then_some(score)
}

fn describe_catalog_entry(
    catalog: &FlowCatalog,
    flow_ref: &str,
    expected_version: Option<&str>,
) -> ToolResult {
    let flow = catalog.find(flow_ref).ok_or_else(|| {
        RuntimeError::ToolFailed(format!("flow.describe: flow `{flow_ref}` not found"))
    })?;
    if expected_version.is_some_and(|version| version != flow.version) {
        return Err(RuntimeError::ToolFailed(format!(
            "flow.describe: stale version for `{}`; search again",
            flow.reference
        )));
    }
    Ok(flow_detail_value(flow))
}

fn scan_flow_file(path: &Path) -> Result<FlowFile, RuntimeError> {
    let source = std::fs::read_to_string(path).map_err(|error| {
        RuntimeError::ToolFailed(format!("flow catalog: read {}: {error}", path.display()))
    })?;
    let parsed = atman_dsl::parse::parse_file(&source).map_err(|error| {
        RuntimeError::ToolFailed(format!("flow catalog: parse {}: {error}", path.display()))
    })?;
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_string();
    let description = parsed
        .flows
        .iter()
        .find(|flow| flow.name.name == "describe")
        .and_then(extract_return_string_literal)
        .unwrap_or_default();
    let version = format!("blake3:{}", blake3::hash(source.as_bytes()).to_hex());
    let flows = parsed
        .flows
        .iter()
        .map(|flow| flow_entry(&file_name, &description, &version, flow))
        .collect();
    Ok(FlowFile {
        name: file_name,
        description,
        flows,
    })
}

fn flow_entry(file_name: &str, description: &str, version: &str, flow: &FlowDecl) -> FlowEntry {
    FlowEntry {
        name: flow.name.name.clone(),
        reference: format!("{file_name}@{}", flow.name.name),
        version: version.to_string(),
        summary: description.to_string(),
        params: flow
            .params
            .iter()
            .map(|parameter| FlowParameter {
                name: parameter.name.name.clone(),
                ty: render_type(&parameter.ty),
                default: parameter.default.as_ref().map(expr_to_value),
            })
            .collect(),
    }
}

fn flow_detail_value(flow: &FlowEntry) -> Value {
    Value::Struct(vec![
        ("name".into(), Value::Str(flow.name.clone())),
        ("ref".into(), Value::Str(flow.reference.clone())),
        ("version".into(), Value::Str(flow.version.clone())),
        ("summary".into(), Value::Str(flow.summary.clone())),
        (
            "params".into(),
            Value::List(
                flow.params
                    .iter()
                    .map(|parameter| {
                        Value::Struct(vec![
                            ("name".into(), Value::Str(parameter.name.clone())),
                            ("type".into(), Value::Str(parameter.ty.clone())),
                            ("required".into(), Value::Bool(parameter.default.is_none())),
                            (
                                "default".into(),
                                parameter.default.clone().unwrap_or(Value::Unit),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

fn legacy_flow_value(flow: &FlowEntry) -> Value {
    Value::Struct(vec![
        ("name".into(), Value::Str(flow.name.clone())),
        ("ref".into(), Value::Str(flow.reference.clone())),
        (
            "params".into(),
            Value::List(
                flow.params
                    .iter()
                    .map(|parameter| {
                        Value::Struct(vec![
                            ("name".into(), Value::Str(parameter.name.clone())),
                            ("ty".into(), Value::Str(parameter.ty.clone())),
                            (
                                "default".into(),
                                parameter.default.clone().unwrap_or(Value::Unit),
                            ),
                        ])
                    })
                    .collect(),
            ),
        ),
    ])
}

fn normalize_flow_ref(flow_ref: &str) -> Option<String> {
    let (file, flow) = flow_ref.split_once('@')?;
    if file.is_empty() || flow.is_empty() {
        return None;
    }
    let file = normalize_flow_file(file)?;
    Some(format!("{file}@{flow}"))
}

fn normalize_flow_file(file: &str) -> Option<String> {
    let file = file.trim();
    if file.is_empty() {
        return None;
    }
    Some(if file.ends_with(".at") {
        file.to_string()
    } else {
        format!("{file}.at")
    })
}

fn search_fingerprint(catalog_fingerprint: &str, query: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(catalog_fingerprint.as_bytes());
    hasher.update(&[0]);
    hasher.update(query.as_bytes());
    format!("blake3:{}", hasher.finalize().to_hex())
}

fn parse_cursor(cursor: &str, expected_fingerprint: &str) -> Result<usize, RuntimeError> {
    let (fingerprint, offset) = cursor.rsplit_once(':').ok_or_else(|| {
        RuntimeError::ToolFailed("flow.search: malformed cursor; search again".into())
    })?;
    if fingerprint != expected_fingerprint {
        return Err(RuntimeError::ToolFailed(
            "flow.search: stale cursor; search again".into(),
        ));
    }
    offset
        .parse::<usize>()
        .map_err(|_| RuntimeError::ToolFailed("flow.search: malformed cursor; search again".into()))
}

fn string_arg<'a>(args: &'a ToolArgs, name: &str, tool: &str) -> Result<&'a str, RuntimeError> {
    match args.named(name) {
        Some(Value::Str(value)) => Ok(value),
        Some(other) => Err(RuntimeError::ToolFailed(format!(
            "{tool}: `{name}` must be a string, got {}",
            other.kind_name()
        ))),
        None => Err(RuntimeError::MissingArg(name.to_string())),
    }
}

fn optional_string_arg(
    args: &ToolArgs,
    name: &str,
    tool: &str,
) -> Result<Option<String>, RuntimeError> {
    match args.named(name) {
        Some(Value::Str(value)) if value.trim().is_empty() => Ok(None),
        Some(Value::Str(value)) => Ok(Some(value.clone())),
        Some(Value::Unit) | None => Ok(None),
        Some(other) => Err(RuntimeError::ToolFailed(format!(
            "{tool}: `{name}` must be a string, got {}",
            other.kind_name()
        ))),
    }
}

fn limit_arg(args: &ToolArgs, tool: &str) -> Result<usize, RuntimeError> {
    match args.named("limit") {
        None | Some(Value::Unit) => Ok(DEFAULT_SEARCH_LIMIT),
        Some(Value::Int(limit)) if (1..=MAX_SEARCH_LIMIT as i64).contains(limit) => {
            Ok(*limit as usize)
        }
        Some(Value::Int(_)) => Err(RuntimeError::ToolFailed(format!(
            "{tool}: `limit` must be between 1 and {MAX_SEARCH_LIMIT}"
        ))),
        Some(other) => Err(RuntimeError::ToolFailed(format!(
            "{tool}: `limit` must be an integer, got {}",
            other.kind_name()
        ))),
    }
}

fn extract_return_string_literal(flow: &FlowDecl) -> Option<String> {
    flow.body.iter().find_map(|statement| match statement {
        Stmt::Return {
            value: Expr::Literal(Literal::Str(value)),
        } => Some(value.clone()),
        _ => None,
    })
}

pub(super) fn render_type(ty: &TypeExpr) -> String {
    match ty {
        TypeExpr::Named(name) => name.name.clone(),
        TypeExpr::List(inner) => format!("[{}]", render_type(inner)),
        TypeExpr::Struct(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(name, ty)| format!("{}: {}", name.name, render_type(ty)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn expr_to_value(expr: &Expr) -> Value {
    match expr {
        Expr::Literal(Literal::Int(value)) => Value::Int(*value),
        Expr::Literal(Literal::Float(value)) => Value::Float(*value),
        Expr::Literal(Literal::Bool(value)) => Value::Bool(*value),
        Expr::Literal(Literal::Str(value)) => Value::Str(value.clone()),
        _ => Value::Unit,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_flow(dir: &Path, name: &str, source: &str) {
        std::fs::write(dir.join(name), source).unwrap();
    }

    fn next_cursor(value: &Value) -> Option<&str> {
        match value.field("next_cursor") {
            Some(Value::Str(cursor)) => Some(cursor),
            _ => None,
        }
    }

    #[test]
    fn optional_discovery_values_treat_blank_strings_as_omitted() {
        for value in ["", " ", "\t\n"] {
            let args = ToolArgs {
                named: vec![("value".into(), Value::Str(value.into()))],
                ..Default::default()
            };
            assert_eq!(
                optional_string_arg(&args, "value", "flow.test").unwrap(),
                None
            );
        }

        let args = ToolArgs {
            named: vec![("value".into(), Value::Str("blake3:123".into()))],
            ..Default::default()
        };
        assert_eq!(
            optional_string_arg(&args, "value", "flow.test").unwrap(),
            Some("blake3:123".into())
        );
    }

    #[test]
    fn search_is_bounded_and_cursor_is_bound_to_catalog_revision() {
        let dir = tempfile::tempdir().unwrap();
        write_flow(
            dir.path(),
            "agents.at",
            r#"
flow describe() -> string { return "Delegated work" }
flow alpha(goal: string) -> string { return goal }
flow beta(goal: string) -> string { return goal }
flow gamma(goal: string) -> string { return goal }
"#,
        );
        let catalog = FlowCatalog::load_from(dir.path()).unwrap();
        let first = search_catalog(&catalog, "", 2, None).unwrap();
        let cursor = next_cursor(&first).unwrap().to_string();
        assert_eq!(
            first.field("items").and_then(|items| match items {
                Value::List(items) => Some(items.len()),
                _ => None,
            }),
            Some(2)
        );
        let second = search_catalog(&catalog, "", 2, Some(&cursor)).unwrap();
        assert!(next_cursor(&second).is_none());

        write_flow(
            dir.path(),
            "extra.at",
            "flow delta(goal: string) -> string { return goal }",
        );
        let changed = FlowCatalog::load_from(dir.path()).unwrap();
        let error = search_catalog(&changed, "", 2, Some(&cursor)).unwrap_err();
        assert!(error.to_string().contains("stale cursor"));
    }

    #[test]
    fn describe_returns_parameter_contract_and_rejects_stale_version() {
        let dir = tempfile::tempdir().unwrap();
        write_flow(
            dir.path(),
            "review.at",
            r#"
flow describe() -> string { return "Review code" }
flow review(goal: string, retries: int = 3) -> string { return goal }
"#,
        );
        let catalog = FlowCatalog::load_from(dir.path()).unwrap();
        let flow = catalog.find("review@review").unwrap();
        let described =
            describe_catalog_entry(&catalog, "review.at@review", Some(&flow.version)).unwrap();
        assert_eq!(
            described.field("summary").and_then(as_str),
            Some("Review code")
        );
        let params = match described.field("params") {
            Some(Value::List(params)) => params,
            _ => panic!("params must be a list"),
        };
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].field("required").and_then(as_bool), Some(true));
        assert_eq!(params[1].field("default").and_then(as_int), Some(3));

        let error =
            describe_catalog_entry(&catalog, "review@review", Some("blake3:stale")).unwrap_err();
        assert!(error.to_string().contains("stale version"));
    }

    #[test]
    fn search_ranks_partial_natural_language_matches() {
        let dir = tempfile::tempdir().unwrap();
        write_flow(
            dir.path(),
            "subagent.at",
            r#"
flow describe() -> string { return "Sub-agent flows for isolated research, verification, implementation, and review. The research role reads files without making changes." }
flow subagent(goal: string, role: string = "research") -> string { return goal }
"#,
        );
        write_flow(
            dir.path(),
            "review.at",
            r#"
flow describe() -> string { return "Review files" }
flow review(goal: string) -> string { return goal }
"#,
        );
        let catalog = FlowCatalog::load_from(dir.path()).unwrap();
        let result = search_catalog(&catalog, "subagent research read files", 5, None).unwrap();
        let items = match result.field("items") {
            Some(Value::List(items)) => items,
            _ => panic!("items must be a list"),
        };
        assert_eq!(
            items[0].field("ref").and_then(as_str),
            Some("subagent.at@subagent")
        );
    }

    #[test]
    fn describe_accepts_the_same_file_shorthand_as_spawn() {
        let dir = tempfile::tempdir().unwrap();
        write_flow(
            dir.path(),
            "subagent.at",
            r#"
flow describe() -> string { return "Delegated work" }
flow subagent(goal: string) -> string { return goal }
flow research_loop(goal: string) -> string { return goal }
"#,
        );
        let catalog = FlowCatalog::load_from(dir.path()).unwrap();
        let described = describe_catalog_entry(&catalog, "subagent.at", None).unwrap();
        assert_eq!(
            described.field("ref").and_then(as_str),
            Some("subagent.at@subagent")
        );
    }

    fn as_str(value: &Value) -> Option<&str> {
        match value {
            Value::Str(value) => Some(value),
            _ => None,
        }
    }

    fn as_bool(value: &Value) -> Option<bool> {
        match value {
            Value::Bool(value) => Some(*value),
            _ => None,
        }
    }

    fn as_int(value: &Value) -> Option<i64> {
        match value {
            Value::Int(value) => Some(*value),
            _ => None,
        }
    }
}
