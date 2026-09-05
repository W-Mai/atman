use anyhow::{Context, Result};
use atman_proto::{
    ListMcpPromptsResponse, ListMcpResourcesResponse, ListMcpServersResponse, ListMcpToolsResponse,
    McpKeyValue, McpPrompt, McpPromptArg, McpResource, McpServerInput, McpServerMutation,
    McpServerSummary, McpTool, MutateMcpServerResponse, ProbeResponse,
};

pub fn list_servers(launcher: &crate::run::RunLauncher) -> Result<ListMcpServersResponse> {
    let mut servers = launcher
        .mcp_configs()?
        .into_iter()
        .map(server_summary)
        .collect::<Vec<_>>();
    servers.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(ListMcpServersResponse { servers })
}

pub fn mutate(
    launcher: &crate::run::RunLauncher,
    mutation: McpServerMutation,
) -> Result<MutateMcpServerResponse> {
    let hub = crate::bootstrap::resolve_config_hub(launcher.config_dir.as_deref())?;
    match mutation {
        McpServerMutation::Upsert { server } => {
            let name = server.name.clone();
            hub.upsert_mcp(server_config(server)?)?;
            Ok(MutateMcpServerResponse {
                name,
                removed: false,
            })
        }
        McpServerMutation::Remove { name } => {
            hub.remove_mcp(&name)?;
            Ok(MutateMcpServerResponse {
                name,
                removed: true,
            })
        }
    }
}

pub async fn probe(launcher: &crate::run::RunLauncher, name: &str) -> Result<ProbeResponse> {
    let config = configured_server(launcher, name)?;
    let registry = atman_runtime::ToolRegistry::new();
    let mut results = atman_runtime::mcp::register_from_configs(&registry, &[config]).await;
    Ok(match results.pop() {
        Some(Ok(status)) => ProbeResponse {
            message: format!("{} tools discovered", status.tool_count),
            ok: true,
        },
        Some(Err(error)) => ProbeResponse {
            message: error.error.to_string(),
            ok: false,
        },
        None => ProbeResponse {
            message: "MCP probe returned no result".into(),
            ok: false,
        },
    })
}

pub async fn list_resources(
    launcher: &crate::run::RunLauncher,
    name: &str,
) -> Result<ListMcpResourcesResponse> {
    let client = connect(launcher, name).await?;
    let resources = client
        .list_resources()
        .await?
        .into_iter()
        .map(|resource| McpResource {
            uri: resource.uri,
            name: resource.name,
            description: resource.description,
            mime_type: resource.mime_type,
        })
        .collect();
    Ok(ListMcpResourcesResponse { resources })
}

pub async fn list_tools(
    launcher: &crate::run::RunLauncher,
    name: &str,
) -> Result<ListMcpToolsResponse> {
    let client = connect(launcher, name).await?;
    let tools = client
        .tool_snapshot()
        .tools
        .iter()
        .map(|tool| McpTool {
            name: tool.name.clone(),
            description: tool.description.clone(),
        })
        .collect();
    Ok(ListMcpToolsResponse { tools })
}

pub async fn list_prompts(
    launcher: &crate::run::RunLauncher,
    name: &str,
) -> Result<ListMcpPromptsResponse> {
    let client = connect(launcher, name).await?;
    let prompts = client
        .list_prompts()
        .await?
        .into_iter()
        .map(|prompt| McpPrompt {
            name: prompt.name,
            description: prompt.description,
            arguments: prompt
                .arguments
                .into_iter()
                .map(|argument| McpPromptArg {
                    name: argument.name,
                    description: argument.description,
                    required: argument.required,
                })
                .collect(),
        })
        .collect();
    Ok(ListMcpPromptsResponse { prompts })
}

fn configured_server(
    launcher: &crate::run::RunLauncher,
    name: &str,
) -> Result<atman_runtime::mcp::McpServerConfig> {
    launcher
        .mcp_configs()?
        .into_iter()
        .find(|config| config.name == name)
        .with_context(|| format!("MCP server `{name}` is not configured"))
}

async fn connect(
    launcher: &crate::run::RunLauncher,
    name: &str,
) -> Result<atman_runtime::mcp::McpClient> {
    let config = configured_server(launcher, name)?;
    match config.transport {
        atman_runtime::mcp::TransportKind::Stdio => atman_runtime::mcp::McpClient::connect_stdio(
            &config.name,
            &config.command,
            &config.args,
            &config.env,
            config.timeout_ms,
        )
        .await
        .map_err(Into::into),
        _ => {
            let url = config
                .url
                .as_deref()
                .with_context(|| format!("MCP server `{name}` requires a URL"))?;
            atman_runtime::mcp::McpClient::connect_http(
                &config.name,
                url,
                config.auth_token,
                config.timeout_ms,
            )
            .await
            .map_err(Into::into)
        }
    }
}

fn server_summary(config: atman_runtime::mcp::McpServerConfig) -> McpServerSummary {
    McpServerSummary {
        name: config.name,
        transport: transport_name(config.transport).into(),
        command: config.command,
        args: config.args,
        env_count: config.env.len(),
        url: config.url,
        header_count: config.headers.len(),
        tier: tier_number(config.tier),
        timeout_ms: config.timeout_ms,
        disabled: config.disabled,
    }
}

fn server_config(input: McpServerInput) -> Result<atman_runtime::mcp::McpServerConfig> {
    let transport = match input.transport.as_str() {
        "stdio" => atman_runtime::mcp::TransportKind::Stdio,
        "http" => atman_runtime::mcp::TransportKind::Http,
        "sse" => atman_runtime::mcp::TransportKind::Sse,
        other => anyhow::bail!("unknown MCP transport `{other}`"),
    };
    match transport {
        atman_runtime::mcp::TransportKind::Stdio => {
            anyhow::ensure!(
                !input.command.trim().is_empty(),
                "stdio MCP command is required"
            );
        }
        _ => {
            anyhow::ensure!(
                input
                    .url
                    .as_deref()
                    .is_some_and(|url| !url.trim().is_empty()),
                "HTTP MCP URL is required"
            );
        }
    }
    Ok(atman_runtime::mcp::McpServerConfig {
        name: input.name,
        transport,
        command: input.command,
        args: input.args,
        env: key_values(input.env),
        url: input.url,
        auth_token: input.auth_token,
        headers: key_values(input.headers),
        tier: tier(input.tier)?,
        timeout_ms: input.timeout_ms,
        disabled: input.disabled,
    })
}

fn key_values(values: Vec<McpKeyValue>) -> Vec<(String, String)> {
    values
        .into_iter()
        .map(|entry| (entry.name, entry.value))
        .collect()
}

fn transport_name(transport: atman_runtime::mcp::TransportKind) -> &'static str {
    match transport {
        atman_runtime::mcp::TransportKind::Stdio => "stdio",
        atman_runtime::mcp::TransportKind::Http => "http",
        atman_runtime::mcp::TransportKind::Sse => "sse",
    }
}

fn tier_number(tier: atman_runtime::Tier) -> u8 {
    match tier {
        atman_runtime::Tier::Zero => 0,
        atman_runtime::Tier::One => 1,
        atman_runtime::Tier::Two => 2,
        atman_runtime::Tier::Three => 3,
        atman_runtime::Tier::Four => 4,
    }
}

fn tier(value: u8) -> Result<atman_runtime::Tier> {
    match value {
        0 => Ok(atman_runtime::Tier::Zero),
        1 => Ok(atman_runtime::Tier::One),
        2 => Ok(atman_runtime::Tier::Two),
        3 => Ok(atman_runtime::Tier::Three),
        4 => Ok(atman_runtime::Tier::Four),
        _ => anyhow::bail!("MCP tier must be between 0 and 4"),
    }
}
