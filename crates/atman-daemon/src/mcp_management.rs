use anyhow::{Context, Result};
use atman_proto::{
    ListMcpPromptsResponse, ListMcpResourcesResponse, McpPrompt, McpPromptArg, McpResource,
    ProbeResponse,
};

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
