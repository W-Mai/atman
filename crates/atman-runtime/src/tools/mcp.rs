use std::collections::BTreeSet;
use std::time::Duration;

use crate::error::RuntimeError;
use crate::mcp::{McpServerState, McpServerStatus};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::tool_schedule::SchedulingClaims;
use crate::value::{Value, ValueJson};

pub const CONTROL_TOOL_NAMES: &[&str] = &["mcp.status", "mcp.tools", "mcp.await", "mcp.call"];

pub struct McpStatus;
pub struct McpTools;
pub struct McpAwait;
pub struct McpCall;

const OPTIONAL_DISCOVERY_GRACE: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReadinessMode {
    Required,
    Optional,
}

pub async fn await_requested_tools(args: &ToolArgs, ctx: &ToolCtx) -> Result<(), RuntimeError> {
    let Some(Value::List(items)) = args.named("tools") else {
        return Ok(());
    };
    let selectors = items
        .iter()
        .filter_map(|item| match item {
            Value::Str(selector) if selector.starts_with("mcp.") => Some(selector.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let required = selectors
        .iter()
        .filter(|selector| selector.as_str() != "mcp.*")
        .cloned()
        .collect::<Vec<_>>();
    await_selectors(
        &required,
        ctx,
        Duration::from_secs(120),
        ReadinessMode::Required,
    )
    .await?;
    if selectors.iter().any(|selector| selector == "mcp.*") {
        await_selectors(
            &["mcp.*".into()],
            ctx,
            OPTIONAL_DISCOVERY_GRACE,
            ReadinessMode::Optional,
        )
        .await?;
    }
    Ok(())
}

async fn await_selectors(
    selectors: &[String],
    ctx: &ToolCtx,
    timeout: Duration,
    mode: ReadinessMode,
) -> Result<(), RuntimeError> {
    if selectors.is_empty() {
        return Ok(());
    }
    let Some(session) = ctx.session_runtime.as_ref() else {
        return Ok(());
    };
    let mut context = session.subscribe_context();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let statuses = context.borrow().mcp_servers.clone();
        let required = required_servers(selectors, &statuses);
        if required.is_empty() {
            return Ok(());
        }
        let mut pending = Vec::new();
        for server in required {
            let status = statuses.iter().find(|status| status.name == server);
            match status.map(|status| &status.state) {
                Some(McpServerState::Connected { .. }) => {}
                Some(McpServerState::Pending | McpServerState::Connecting) => {
                    pending.push(server);
                }
                Some(McpServerState::Disabled) => {
                    if mode == ReadinessMode::Required {
                        return Err(RuntimeError::ToolFailed(format!(
                            "MCP server `{server}` is disabled"
                        )));
                    }
                }
                Some(McpServerState::Error { message })
                | Some(McpServerState::Disconnected { message })
                | Some(McpServerState::Timeout { message }) => {
                    if mode == ReadinessMode::Required {
                        return Err(RuntimeError::ToolFailed(format!(
                            "MCP server `{server}` is unavailable: {message}"
                        )));
                    }
                }
                None => {}
            }
        }
        if pending.is_empty() {
            return Ok(());
        }
        tokio::select! {
            _ = ctx.cancel.cancelled() => {
                return Err(RuntimeError::Cancelled("MCP readiness wait cancelled".into()));
            }
            _ = tokio::time::sleep_until(deadline) => {
                return if mode == ReadinessMode::Optional {
                    Ok(())
                } else {
                    Err(RuntimeError::ToolFailed(format!(
                        "timed out waiting for MCP server(s): {}",
                        pending.join(", ")
                    )))
                };
            }
            changed = context.changed() => {
                if changed.is_err() {
                    return if mode == ReadinessMode::Optional {
                        Ok(())
                    } else {
                        Err(RuntimeError::ToolFailed(
                            "MCP readiness channel closed before connection completed".into(),
                        ))
                    };
                }
            }
        }
    }
}

fn required_servers(selectors: &[String], statuses: &[McpServerStatus]) -> BTreeSet<String> {
    let mut required = BTreeSet::new();
    for selector in selectors {
        if CONTROL_TOOL_NAMES.contains(&selector.as_str()) {
            continue;
        }
        if selector == "mcp.*" {
            required.extend(
                statuses
                    .iter()
                    .filter(|status| !matches!(status.state, McpServerState::Disabled))
                    .map(|status| status.name.clone()),
            );
            continue;
        }
        if let Some(status) = statuses
            .iter()
            .filter(|status| selector.starts_with(&format!("mcp.{}.", status.name)))
            .max_by_key(|status| status.name.len())
        {
            required.insert(status.name.clone());
        }
    }
    required
}

fn status_value(status: &McpServerStatus, scope: Option<&crate::tool::McpToolScope>) -> Value {
    let (state, tool_count, message) = match &status.state {
        McpServerState::Disabled => ("disabled", 0, None),
        McpServerState::Pending => ("pending", 0, None),
        McpServerState::Connecting => ("connecting", 0, None),
        McpServerState::Connected { tool_count, tools } => {
            let count = match scope {
                Some(scope @ crate::tool::McpToolScope::Selectors(_)) => tools
                    .iter()
                    .filter(|tool| scope.permits(&format!("mcp.{}.{}", status.name, tool.name)))
                    .count(),
                _ => *tool_count,
            };
            ("connected", count, None)
        }
        McpServerState::Error { message } => ("error", 0, Some(message.clone())),
        McpServerState::Disconnected { message } => ("disconnected", 0, Some(message.clone())),
        McpServerState::Timeout { message } => ("timeout", 0, Some(message.clone())),
    };
    Value::Struct(vec![
        ("name".into(), Value::Str(status.name.clone())),
        ("state".into(), Value::Str(state.into())),
        ("tool_count".into(), Value::Int(tool_count as i64)),
        (
            "message".into(),
            message.map(Value::Str).unwrap_or(Value::Unit),
        ),
    ])
}

fn string_arg(args: &ToolArgs, name: &str) -> Result<String, RuntimeError> {
    match args.named(name) {
        Some(Value::Str(value)) => Ok(value.clone()),
        Some(value) => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: value.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg(name.into())),
    }
}

fn optional_string_arg<'a>(
    args: &'a ToolArgs,
    name: &str,
) -> Result<Option<&'a str>, RuntimeError> {
    match args.named(name) {
        Some(Value::Str(value)) => Ok(Some(value)),
        Some(value) => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: value.kind_name().into(),
        }),
        None => Ok(None),
    }
}

fn call_target(args: &ToolArgs, ctx: &ToolCtx) -> Result<(String, ToolArgs), RuntimeError> {
    let server = string_arg(args, "server")?;
    let tool = string_arg(args, "tool")?;
    let target = format!("mcp.{server}.{tool}");
    if ctx
        .model_mcp_scope
        .as_ref()
        .is_some_and(|scope| !scope.permits(&target))
    {
        return Err(RuntimeError::ToolFailed(format!(
            "MCP tool `{target}` was not allowed by the LLM request that produced this call"
        )));
    }
    let target_args = match args.named("input") {
        None | Some(Value::Unit) => ToolArgs::default(),
        Some(Value::Struct(fields)) => ToolArgs {
            positional: Vec::new(),
            named: fields.clone(),
        },
        Some(value) => {
            return Err(RuntimeError::TypeMismatch {
                expected: "struct".into(),
                actual: value.kind_name().into(),
            });
        }
    };
    Ok((target, target_args))
}

pub(crate) fn resolve_registered_call(
    args: &ToolArgs,
    ctx: &ToolCtx,
) -> Result<(std::sync::Arc<dyn Tool>, ToolArgs), RuntimeError> {
    let (target, target_args) = call_target(args, ctx)?;
    let server = string_arg(args, "server")?;
    let tool = ctx
        .registry
        .as_ref()
        .and_then(|registry| registry.get(&target))
        .ok_or_else(|| {
            RuntimeError::ToolFailed(format!(
                "MCP tool `{target}` is not ready or unavailable; inspect `mcp.status` or use `mcp.await` before calling it"
            ))
        })?;
    let tool_name = string_arg(args, "tool")?;
    if tool.mcp_identity() != Some((server.as_str(), tool_name.as_str())) {
        return Err(RuntimeError::ToolFailed(format!(
            "tool `{target}` is not a registered MCP server tool"
        )));
    }
    Ok((tool, target_args))
}

pub(crate) async fn resolve_call(
    args: &ToolArgs,
    ctx: &ToolCtx,
) -> Result<(std::sync::Arc<dyn Tool>, ToolArgs), RuntimeError> {
    // Reject invalid or out-of-scope targets before waiting for their server.
    call_target(args, ctx)?;
    let server = string_arg(args, "server")?;
    await_selectors(
        &[format!("mcp.{server}.*")],
        ctx,
        Duration::from_secs(120),
        ReadinessMode::Required,
    )
    .await?;
    resolve_registered_call(args, ctx)
}

impl Tool for McpStatus {
    fn name(&self) -> &str {
        "mcp.status"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Inspect connection readiness and permitted tool counts for MCP servers in the current request's scope.",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"server":{"type":"string"}},"additionalProperties":false})
    }
    fn scheduling_claims(
        &self,
        _args: &ToolArgs,
        _ctx: &ToolCtx,
    ) -> Result<SchedulingClaims, RuntimeError> {
        Ok(SchedulingClaims::declared([]))
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let server = match args.named("server") {
                Some(Value::Str(value)) => Some(value.as_str()),
                Some(value) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "string".into(),
                        actual: value.kind_name().into(),
                    });
                }
                None => None,
            };
            let session = ctx.session_runtime.as_ref().ok_or_else(|| {
                RuntimeError::ToolFailed("mcp.status: no session available".into())
            })?;
            let snapshot = session.subscribe_context().borrow().clone();
            let statuses = snapshot
                .mcp_servers
                .iter()
                .filter(|status| server.is_none_or(|server| status.name == server))
                .filter(|status| {
                    ctx.model_mcp_scope
                        .as_ref()
                        .is_none_or(|scope| scope.permits_server(&status.name))
                })
                .map(|status| status_value(status, ctx.model_mcp_scope.as_ref()))
                .collect();
            Ok(Value::List(statuses))
        })
    }
}

impl Tool for McpTools {
    fn name(&self) -> &str {
        "mcp.tools"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Search available MCP tools with query and cursor pagination. Set server and tool to retrieve that tool's complete input schema before calling mcp.call.",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type":"object",
            "properties": {
                "server":{"type":"string","description":"Restrict discovery to this MCP server."},
                "tool":{"type":"string","description":"Exact MCP tool name within server; returns its complete input_schema. Requires server."},
                "query":{"type":"string","description":"Search tool names and descriptions for every whitespace-separated term."},
                "cursor":{"type":"string","description":"Continue using next_cursor from the previous page with the same filters."},
                "limit":{"type":"integer","minimum":1,"maximum":100,"default":20}
            },
            "additionalProperties":false
        })
    }
    fn scheduling_claims(
        &self,
        _args: &ToolArgs,
        _ctx: &ToolCtx,
    ) -> Result<SchedulingClaims, RuntimeError> {
        Ok(SchedulingClaims::declared([]))
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let registry = ctx.registry.as_ref().ok_or_else(|| {
                RuntimeError::ToolFailed("mcp.tools: no tool registry available".into())
            })?;
            let server = optional_string_arg(&args, "server")?;
            let selected_tool = optional_string_arg(&args, "tool")?;
            let query = optional_string_arg(&args, "query")?
                .unwrap_or("")
                .to_lowercase();
            let cursor = optional_string_arg(&args, "cursor")?;
            if selected_tool.is_some() {
                if server.is_none() {
                    return Err(RuntimeError::MissingArg("server".into()));
                }
                if !query.is_empty() || cursor.is_some() {
                    return Err(RuntimeError::ToolFailed(
                        "mcp.tools: tool cannot be combined with query or cursor".into(),
                    ));
                }
            }
            let limit = match args.named("limit") {
                None => 20,
                Some(Value::Int(limit @ 1..=100)) => *limit as usize,
                _ => {
                    return Err(RuntimeError::ToolFailed(
                        "mcp.tools: limit must be an integer from 1 to 100".into(),
                    ));
                }
            };
            let mut matches = registry
                .names()
                .into_iter()
                .filter(|name| {
                    ctx.model_mcp_scope
                        .as_ref()
                        .is_none_or(|scope| scope.permits(name))
                })
                .filter_map(|name| registry.get(&name).map(|tool| (name, tool)))
                .filter(|(name, tool)| {
                    let Some((tool_server, tool_name)) = tool.mcp_identity() else {
                        return false;
                    };
                    server.is_none_or(|server| server == tool_server)
                        && selected_tool.is_none_or(|selected| selected == tool_name)
                        && query.split_whitespace().all(|term| {
                            name.to_lowercase().contains(term)
                                || tool.description().is_some_and(|description| {
                                    description.to_lowercase().contains(term)
                                })
                        })
                })
                .collect::<Vec<_>>();
            matches.sort_by(|(left, _), (right, _)| left.cmp(right));
            if selected_tool.is_some() && matches.is_empty() {
                return Err(RuntimeError::ToolFailed(
                    "mcp.tools: no matching tool is available in the current scope".into(),
                ));
            }
            let total = matches.len();
            let start = cursor.map_or(0, |cursor| {
                matches.partition_point(|(name, _)| name.as_str() <= cursor)
            });
            let page = &matches[start..matches.len().min(start + limit)];
            let next_cursor = if start + page.len() < matches.len() {
                Value::Str(page.last().expect("nonempty page").0.clone())
            } else {
                Value::Unit
            };
            let tools = page
                .iter()
                .map(|(name, tool)| {
                    let (server, tool_name) =
                        tool.mcp_identity().expect("MCP identity was filtered");
                    let mut fields = vec![
                        ("name".into(), Value::Str(name.clone())),
                        ("server".into(), Value::Str(server.into())),
                        ("tool".into(), Value::Str(tool_name.into())),
                        (
                            "description".into(),
                            tool.description()
                                .map(|text| Value::Str(text.into()))
                                .unwrap_or(Value::Unit),
                        ),
                    ];
                    if selected_tool.is_some() {
                        fields.push(("input_schema".into(), Value::from_json(tool.input_schema())));
                    }
                    Value::Struct(fields)
                })
                .collect();
            Ok(Value::Struct(vec![
                ("tools".into(), Value::List(tools)),
                ("next_cursor".into(), next_cursor),
                ("total".into(), Value::Int(total as i64)),
            ]))
        })
    }
}

impl Tool for McpAwait {
    fn name(&self) -> &str {
        "mcp.await"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Wait until one MCP server in the current request's scope is connected or reports a terminal connection error.",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"server":{"type":"string"},"timeout":{"type":"integer","minimum":1,"default":120}},"required":["server"],"additionalProperties":false})
    }
    fn scheduling_claims(
        &self,
        args: &ToolArgs,
        _ctx: &ToolCtx,
    ) -> Result<SchedulingClaims, RuntimeError> {
        Ok(SchedulingClaims::declared([
            crate::tool_schedule::ResourceAccess::read(
                crate::tool_schedule::ResourceKey::McpServer(string_arg(args, "server")?),
            ),
        ]))
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let server = string_arg(&args, "server")?;
            if ctx
                .model_mcp_scope
                .as_ref()
                .is_some_and(|scope| !scope.permits_server(&server))
            {
                return Err(RuntimeError::ToolFailed(format!(
                    "MCP server `{server}` was not allowed by the LLM request that produced this call"
                )));
            }
            let session = ctx.session_runtime.as_ref().ok_or_else(|| {
                RuntimeError::ToolFailed("mcp.await: no session available".into())
            })?;
            if !session
                .subscribe_context()
                .borrow()
                .mcp_servers
                .iter()
                .any(|status| status.name == server)
            {
                return Err(RuntimeError::ToolFailed(format!(
                    "MCP server `{server}` is not configured"
                )));
            }
            let timeout = match args.named("timeout") {
                None => 120,
                Some(Value::Int(value)) if *value > 0 => *value as u64,
                Some(value) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "positive int".into(),
                        actual: value.kind_name().into(),
                    });
                }
            };
            await_selectors(
                &[format!("mcp.{server}.*")],
                ctx,
                Duration::from_secs(timeout),
                ReadinessMode::Required,
            )
            .await?;
            Ok(Value::Bool(true))
        })
    }
}

impl Tool for McpCall {
    fn name(&self) -> &str {
        "mcp.call"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Call a ready MCP tool by server and tool name using its declared input parameters. Use mcp.await before calling a server that is still connecting.",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"server":{"type":"string"},"tool":{"type":"string"},"input":{"type":"object","default":{}}},"required":["server","tool"],"additionalProperties":false})
    }
    fn scheduling_claims(
        &self,
        args: &ToolArgs,
        _ctx: &ToolCtx,
    ) -> Result<SchedulingClaims, RuntimeError> {
        Ok(crate::mcp::mcp_server_scheduling_claim(&string_arg(
            args, "server",
        )?))
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (tool, target_args) = resolve_call(&args, ctx).await?;
            let id = ctx
                .tool_use_id
                .clone()
                .unwrap_or_else(|| uuid::Uuid::now_v7().to_string());
            let invocation_ctx = ctx
                .clone()
                .for_tool_invocation(tool.tier())
                .with_tool_use_id(id.clone());
            let call_ctx = crate::approval::authorize_tool_invocation(
                &invocation_ctx,
                &id,
                tool.name(),
                &target_args,
                tool.as_ref(),
            )
            .await
            .map_err(|reason| {
                RuntimeError::ToolFailed(format!("tool `{}` denied: {reason}", tool.name()))
            })?;
            tool.call(target_args, &call_ctx).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::value::ValueJson;

    struct CatalogTool {
        name: &'static str,
        server: &'static str,
        tool: &'static str,
    }

    impl Tool for CatalogTool {
        fn name(&self) -> &str {
            self.name
        }
        fn tier(&self) -> Tier {
            Tier::Zero
        }
        fn mcp_identity(&self) -> Option<(&str, &str)> {
            Some((self.server, self.tool))
        }
        fn description(&self) -> Option<&str> {
            Some("Read documents")
        }
        fn input_schema(&self) -> serde_json::Value {
            serde_json::json!({"type":"object", "properties":{"id":{"type":"integer"}}, "required":["id"]})
        }
        fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
            Box::pin(async { Ok(Value::Unit) })
        }
    }

    #[tokio::test]
    async fn readiness_observation_respects_the_requests_mcp_scope() {
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        for name in ["docs", "hidden"] {
            session.update_mcp_server(McpServerStatus {
                name: name.into(),
                transport: crate::mcp::TransportKind::Stdio,
                state: McpServerState::Connected {
                    tool_count: 2,
                    tools: ["read", "write"]
                        .into_iter()
                        .map(|name| crate::mcp::McpToolInfo {
                            name: name.into(),
                            description: None,
                        })
                        .collect(),
                },
            });
        }
        session.update_mcp_server(McpServerStatus {
            name: "pending".into(),
            transport: crate::mcp::TransportKind::Stdio,
            state: McpServerState::Connecting,
        });
        let mut ctx = ToolCtx::new().with_session_runtime(session);
        ctx.model_mcp_scope = Some(crate::tool::McpToolScope::Selectors(
            ["mcp.docs.read".into(), "mcp.pending.*".into()].into(),
        ));
        let statuses = McpStatus
            .call(ToolArgs::default(), &ctx)
            .await
            .unwrap()
            .to_json()
            .unwrap();
        let statuses = statuses.as_array().unwrap();
        assert_eq!(statuses.len(), 2);
        assert!(
            statuses
                .iter()
                .any(|status| { status["name"] == "docs" && status["tool_count"] == 1 })
        );
        assert!(
            statuses
                .iter()
                .any(|status| { status["name"] == "pending" && status["state"] == "connecting" })
        );
        let args = |server: &str| ToolArgs {
            named: vec![("server".into(), Value::Str(server.into()))],
            ..ToolArgs::default()
        };
        assert!(
            matches!(McpStatus.call(args("hidden"), &ctx).await.unwrap(), Value::List(values) if values.is_empty())
        );
        assert!(
            McpAwait
                .call(args("hidden"), &ctx)
                .await
                .unwrap_err()
                .to_string()
                .contains("not allowed")
        );
        assert!(matches!(
            McpAwait.call(args("docs"), &ctx).await.unwrap(),
            Value::Bool(true)
        ));
        ctx.model_mcp_scope = None;
        assert!(matches!(
            McpAwait.call(args("hidden"), &ctx).await.unwrap(),
            Value::Bool(true)
        ));
    }

    #[test]
    fn broker_observation_does_not_serialize_unrelated_tool_execution() {
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            named: vec![("server".into(), Value::Str("slow".into()))],
            ..ToolArgs::default()
        };
        let waiting = McpAwait.scheduling_claims(&args, &ctx).unwrap();
        let ready = crate::mcp::mcp_server_scheduling_claim("ready");
        assert!(!waiting.conflicts_with(&ready));
        assert!(waiting.conflicts_with(&crate::mcp::mcp_server_scheduling_claim("slow")));
        for observer in [&McpStatus as &dyn Tool, &McpTools] {
            let claims = observer
                .scheduling_claims(&ToolArgs::default(), &ctx)
                .unwrap();
            assert!(!claims.conflicts_with(&ready));
        }
        let graph = crate::tool_schedule::SchedulingGraph::from_claims(&[waiting, ready]);
        assert_eq!(graph.ready_wave(), vec![0, 1]);
    }

    #[tokio::test]
    async fn mcp_discovery_scopes_search_pages_and_exact_schema_before_results() {
        let registry = std::sync::Arc::new(crate::tool::ToolRegistry::new());
        for tool in [
            CatalogTool {
                name: "mcp.team.tools.z.read",
                server: "team.tools",
                tool: "z.read",
            },
            CatalogTool {
                name: "mcp.hidden.read",
                server: "hidden",
                tool: "read",
            },
            CatalogTool {
                name: "mcp.team.tools.a.read",
                server: "team.tools",
                tool: "a.read",
            },
        ] {
            registry.register(std::sync::Arc::new(tool));
        }
        registry.register(std::sync::Arc::new(McpCall));
        registry
            .register_fn(
                crate::tool::ToolDefinition::new("mcp.team.tools.fake", Tier::Zero),
                |_, _| async { Ok(Value::Unit) },
            )
            .unwrap();
        let mut ctx = ToolCtx::new().with_registry(registry);
        ctx.model_mcp_scope = Some(crate::tool::McpToolScope::Selectors(
            ["mcp.team.tools.*".into()].into(),
        ));
        let search = ToolArgs {
            named: vec![
                ("query".into(), Value::Str("READ document".into())),
                ("limit".into(), Value::Int(1)),
            ],
            positional: Vec::new(),
        };
        let first = McpTools
            .call(search.clone(), &ctx)
            .await
            .unwrap()
            .to_json()
            .unwrap();
        assert_eq!(first["total"], 2);
        assert_eq!(first["tools"][0]["name"], "mcp.team.tools.a.read");
        assert_eq!(first["tools"][0]["server"], "team.tools");
        assert_eq!(first["tools"][0]["tool"], "a.read");
        assert!(first["tools"][0].get("input_schema").is_none());
        let mut next = search;
        next.named.push((
            "cursor".into(),
            Value::from_json(first["next_cursor"].clone()),
        ));
        let second = McpTools.call(next, &ctx).await.unwrap().to_json().unwrap();
        assert_eq!(second["tools"].as_array().unwrap().len(), 1);
        assert_eq!(second["tools"][0]["name"], "mcp.team.tools.z.read");
        assert!(second["next_cursor"].is_null());
        let mut exact = ToolArgs {
            named: vec![
                ("server".into(), Value::Str("team.tools".into())),
                ("tool".into(), Value::Str("z.read".into())),
            ],
            positional: Vec::new(),
        };
        let detail = McpTools
            .call(exact.clone(), &ctx)
            .await
            .unwrap()
            .to_json()
            .unwrap();
        assert_eq!(
            detail["tools"][0]["input_schema"]["required"],
            serde_json::json!(["id"])
        );
        exact.named[0].1 = Value::Str("hidden".into());
        exact.named[1].1 = Value::Str("read".into());
        assert!(McpTools.call(exact, &ctx).await.is_err());
        let fake = ToolArgs {
            named: vec![
                ("server".into(), Value::Str("team.tools".into())),
                ("tool".into(), Value::Str("fake".into())),
            ],
            positional: Vec::new(),
        };
        assert!(
            resolve_call(&fake, &ctx)
                .await
                .err()
                .unwrap()
                .to_string()
                .contains("not a registered MCP")
        );
    }

    #[test]
    fn mcp_call_claims_the_explicit_server_without_parsing_qualified_names() {
        let args = ToolArgs {
            named: vec![
                ("server".into(), Value::Str("team.tools".into())),
                ("tool".into(), Value::Str("files.read".into())),
            ],
            ..ToolArgs::default()
        };
        let claims = McpCall.scheduling_claims(&args, &ToolCtx::new()).unwrap();
        assert_eq!(
            claims,
            crate::mcp::mcp_server_scheduling_claim("team.tools")
        );
    }

    #[tokio::test]
    async fn wildcard_skips_failed_server() {
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        session.update_mcp_server(McpServerStatus {
            name: "broken".into(),
            transport: crate::mcp::TransportKind::Stdio,
            state: McpServerState::Error {
                message: "startup failed".into(),
            },
        });
        session.update_mcp_server(McpServerStatus {
            name: "healthy".into(),
            transport: crate::mcp::TransportKind::Http,
            state: McpServerState::Connected {
                tool_count: 1,
                tools: Vec::new(),
            },
        });
        let ctx = ToolCtx::new().with_session_runtime(session);
        let args = ToolArgs {
            positional: Vec::new(),
            named: vec![(
                "tools".into(),
                Value::List(vec![Value::Str("mcp.*".into())]),
            )],
        };
        await_requested_tools(&args, &ctx).await.unwrap();
    }

    #[tokio::test]
    async fn wildcard_discovery_timeout_does_not_fail_a_flow() {
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        session.update_mcp_server(McpServerStatus {
            name: "slow".into(),
            transport: crate::mcp::TransportKind::Stdio,
            state: McpServerState::Connecting,
        });
        let ctx = ToolCtx::new().with_session_runtime(session);
        await_selectors(
            &["mcp.*".into()],
            &ctx,
            Duration::from_millis(1),
            ReadinessMode::Optional,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn wildcard_discovery_still_honors_cancellation() {
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        session.update_mcp_server(McpServerStatus {
            name: "slow".into(),
            transport: crate::mcp::TransportKind::Stdio,
            state: McpServerState::Connecting,
        });
        let ctx = ToolCtx::new().with_session_runtime(session);
        ctx.cancel.cancel();
        assert!(matches!(
            await_selectors(
                &["mcp.*".into()],
                &ctx,
                Duration::from_secs(1),
                ReadinessMode::Optional,
            )
            .await,
            Err(RuntimeError::Cancelled(_))
        ));
    }

    #[tokio::test]
    async fn named_selector_still_fails_for_an_unavailable_server() {
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        session.update_mcp_server(McpServerStatus {
            name: "broken".into(),
            transport: crate::mcp::TransportKind::Stdio,
            state: McpServerState::Error {
                message: "startup failed".into(),
            },
        });
        let ctx = ToolCtx::new().with_session_runtime(session);
        let args = ToolArgs {
            positional: Vec::new(),
            named: vec![(
                "tools".into(),
                Value::List(vec![Value::Str("mcp.broken.search".into())]),
            )],
        };
        assert!(matches!(
            await_requested_tools(&args, &ctx).await,
            Err(RuntimeError::ToolFailed(message)) if message.contains("broken")
        ));
    }

    #[test]
    fn wildcard_discovers_all_enabled_servers() {
        let statuses = vec![
            McpServerStatus {
                name: "alpha".into(),
                transport: crate::mcp::TransportKind::Stdio,
                state: McpServerState::Pending,
            },
            McpServerStatus {
                name: "beta".into(),
                transport: crate::mcp::TransportKind::Http,
                state: McpServerState::Disabled,
            },
        ];
        assert_eq!(
            required_servers(&["mcp.*".into()], &statuses),
            BTreeSet::from(["alpha".into()])
        );
    }

    #[test]
    fn exact_tool_waits_only_for_its_server() {
        let statuses = vec![
            McpServerStatus {
                name: "mi-jira-phone".into(),
                transport: crate::mcp::TransportKind::Stdio,
                state: McpServerState::Pending,
            },
            McpServerStatus {
                name: "other".into(),
                transport: crate::mcp::TransportKind::Http,
                state: McpServerState::Pending,
            },
        ];
        assert_eq!(
            required_servers(&["mcp.mi-jira-phone.jira_search".into()], &statuses),
            BTreeSet::from(["mi-jira-phone".into()])
        );
    }

    #[test]
    fn exact_tool_uses_the_longest_matching_server_name() {
        let statuses = vec![
            McpServerStatus {
                name: "jira".into(),
                transport: crate::mcp::TransportKind::Stdio,
                state: McpServerState::Pending,
            },
            McpServerStatus {
                name: "jira.cloud".into(),
                transport: crate::mcp::TransportKind::Http,
                state: McpServerState::Pending,
            },
        ];
        assert_eq!(
            required_servers(&["mcp.jira.cloud.search".into()], &statuses),
            BTreeSet::from(["jira.cloud".into()])
        );
    }

    #[tokio::test]
    async fn readiness_wait_unblocks_after_requested_server_connects() {
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        session.update_mcp_server(McpServerStatus {
            name: "alpha".into(),
            transport: crate::mcp::TransportKind::Stdio,
            state: McpServerState::Pending,
        });
        let ctx = ToolCtx::new().with_session_runtime(session.clone());
        let update = session.clone();
        let task = tokio::spawn(async move {
            tokio::task::yield_now().await;
            update.update_mcp_server(McpServerStatus {
                name: "alpha".into(),
                transport: crate::mcp::TransportKind::Stdio,
                state: McpServerState::Connected {
                    tool_count: 1,
                    tools: Vec::new(),
                },
            });
        });

        await_selectors(
            &["mcp.alpha.search".into()],
            &ctx,
            Duration::from_secs(1),
            ReadinessMode::Required,
        )
        .await
        .unwrap();
        task.await.unwrap();
    }

    #[test]
    fn direct_mcp_calls_are_valid_at_nodes() {
        let source = r#"
flow invoke() {
    ready = mcp.await(server: "jira")
    tools = mcp.tools(server: "jira")
    result = mcp.call(server: "jira", tool: "search", input: {query: "open"})
    return {ready: ready, tools: tools, result: result}
}
"#;
        let file = atman_rt::parse_file(source).unwrap();
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(McpStatus));
        registry.register(std::sync::Arc::new(McpTools));
        registry.register(std::sync::Arc::new(McpAwait));
        registry.register(std::sync::Arc::new(McpCall));

        crate::validate::validate(&file.flows[0], &registry).unwrap();
    }
}
