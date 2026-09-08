use std::collections::BTreeSet;
use std::time::Duration;

use crate::error::RuntimeError;
use crate::mcp::{McpServerState, McpServerStatus};
use crate::tool::{ApprovalLevel, BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub const CONTROL_TOOL_NAMES: &[&str] = &["mcp.status", "mcp.tools", "mcp.await", "mcp.call"];

pub struct McpStatus;
pub struct McpTools;
pub struct McpAwait;
pub struct McpCall;

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
    await_selectors(&selectors, ctx, Duration::from_secs(120)).await
}

async fn await_selectors(
    selectors: &[String],
    ctx: &ToolCtx,
    timeout: Duration,
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
                    return Err(RuntimeError::ToolFailed(format!(
                        "MCP server `{server}` is disabled"
                    )));
                }
                Some(McpServerState::Error { message })
                | Some(McpServerState::Disconnected { message })
                | Some(McpServerState::Timeout { message }) => {
                    return Err(RuntimeError::ToolFailed(format!(
                        "MCP server `{server}` is unavailable: {message}"
                    )));
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
                return Err(RuntimeError::ToolFailed(format!(
                    "timed out waiting for MCP server(s): {}",
                    pending.join(", ")
                )));
            }
            changed = context.changed() => {
                if changed.is_err() {
                    return Err(RuntimeError::ToolFailed(
                        "MCP readiness channel closed before connection completed".into(),
                    ));
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

fn status_value(status: &McpServerStatus) -> Value {
    let (state, tool_count, message) = match &status.state {
        McpServerState::Disabled => ("disabled", 0, None),
        McpServerState::Pending => ("pending", 0, None),
        McpServerState::Connecting => ("connecting", 0, None),
        McpServerState::Connected { tool_count, .. } => ("connected", *tool_count, None),
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

fn call_target(args: &ToolArgs) -> Result<(String, ToolArgs), RuntimeError> {
    let server = string_arg(args, "server")?;
    let tool = string_arg(args, "tool")?;
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
    Ok((format!("mcp.{server}.{tool}"), target_args))
}

impl Tool for McpStatus {
    fn name(&self) -> &str {
        "mcp.status"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some("Inspect MCP connection readiness before selecting or calling a server tool.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"server":{"type":"string"}},"additionalProperties":false})
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
                .map(status_value)
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
        Some("List currently registered MCP tools, optionally restricted to one server.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"server":{"type":"string"}},"additionalProperties":false})
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let registry = ctx.registry.as_ref().ok_or_else(|| {
                RuntimeError::ToolFailed("mcp.tools: no tool registry available".into())
            })?;
            let prefix = match args.named("server") {
                Some(Value::Str(server)) => format!("mcp.{server}."),
                Some(value) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "string".into(),
                        actual: value.kind_name().into(),
                    });
                }
                None => "mcp.".into(),
            };
            let mut names = registry
                .names()
                .into_iter()
                .filter(|name| {
                    name.starts_with(&prefix) && !CONTROL_TOOL_NAMES.contains(&name.as_str())
                })
                .collect::<Vec<_>>();
            names.sort();
            Ok(Value::List(names.into_iter().map(Value::Str).collect()))
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
        Some("Wait until one MCP server is connected or reports a terminal connection error.")
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"server":{"type":"string"},"timeout":{"type":"integer","minimum":1,"default":120}},"required":["server"],"additionalProperties":false})
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let server = string_arg(&args, "server")?;
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
    fn approval_level(&self, args: &ToolArgs, ctx: &ToolCtx) -> ApprovalLevel {
        let Ok((target, target_args)) = call_target(args) else {
            return ApprovalLevel::Dangerous;
        };
        ctx.registry
            .as_ref()
            .and_then(|registry| registry.get(&target))
            .map_or(ApprovalLevel::Dangerous, |tool| {
                tool.approval_level(&target_args, ctx)
            })
    }
    fn description(&self) -> Option<&str> {
        Some(
            "Call a connected MCP tool directly from At code without routing the operation through an LLM.",
        )
    }
    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type":"object","properties":{"server":{"type":"string"},"tool":{"type":"string"},"input":{"type":"object","default":{}}},"required":["server","tool"],"additionalProperties":false})
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let (target, target_args) = call_target(&args)?;
            let server = string_arg(&args, "server")?;
            await_selectors(&[format!("mcp.{server}.*")], ctx, Duration::from_secs(120)).await?;
            let tool = ctx
                .registry
                .as_ref()
                .and_then(|registry| registry.get(&target))
                .ok_or_else(|| RuntimeError::UndefinedTool(target.clone()))?;
            tool.call(target_args, ctx).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_waits_for_all_enabled_servers() {
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

        await_selectors(&["mcp.alpha.search".into()], &ctx, Duration::from_secs(1))
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
        let file = atman_dsl::parse::parse_file(source).unwrap();
        let registry = crate::tool::ToolRegistry::new();
        registry.register(std::sync::Arc::new(McpStatus));
        registry.register(std::sync::Arc::new(McpTools));
        registry.register(std::sync::Arc::new(McpAwait));
        registry.register(std::sync::Arc::new(McpCall));

        crate::validate::validate(&file.flows[0], &registry).unwrap();
    }
}
