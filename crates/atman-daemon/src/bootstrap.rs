use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use atman_runtime::event::EventSink;
use atman_runtime::providers::anthropic::AnthropicProvider;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::providers::openai::OpenAiProvider;
use atman_runtime::{Executor, Value, tools};
use serde::Deserialize;

pub struct BootstrapOptions {
    pub events: EventSink,
    pub mock: bool,
    pub config_dir: Option<PathBuf>,
    pub project_root: PathBuf,
    pub home_dir: Option<PathBuf>,
}

pub struct BootstrapOutcome {
    pub executor: Executor,
    pub mcp_status: Vec<Result<atman_runtime::mcp::McpClientStatus, String>>,
}

pub async fn build_executor(opts: BootstrapOptions) -> Result<BootstrapOutcome> {
    let mut executor = Executor::with_events(opts.events);

    let fetch_rule = build_fetch_rule(&opts.project_root, opts.home_dir.as_deref()).await;
    tools::register_tier_zero_with_rules(&mut executor.tools, fetch_rule);
    tools::register_shell(&mut executor.tools);
    tools::register_preview(
        &mut executor.tools,
        load_preview_config(opts.config_dir.as_deref()),
    );
    register_providers_from_env(&mut executor);
    let mcp_configs = load_mcp_configs(opts.config_dir.as_deref());
    let mcp_status_raw =
        atman_runtime::mcp::register_from_configs(&mut executor.tools, &mcp_configs).await;
    let mcp_status = mcp_status_raw
        .into_iter()
        .map(|r| r.map_err(|e| e.to_string()))
        .collect();
    if opts.mock {
        executor.providers.register(Arc::new(
            MockProvider::new("mock").with_fallback(Value::Str("[mock response]".into())),
        ));
    }
    Ok(BootstrapOutcome {
        executor,
        mcp_status,
    })
}

async fn build_fetch_rule(
    project_root: &Path,
    home: Option<&Path>,
) -> atman_runtime::tools::memory_stubs::FetchRule {
    let fetch_rule = atman_runtime::tools::memory_stubs::FetchRule::new();
    if std::env::var("ATMAN_DISABLE_MIGRATION").is_ok() {
        return fetch_rule;
    }
    let Some(home) = home else {
        return fetch_rule;
    };
    let rules = atman_runtime::migration::scan_migrated_rules(project_root, home);
    fetch_rule.set_migrated(rules).await;
    fetch_rule
}

fn register_providers_from_env(executor: &mut Executor) {
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        let mut p = AnthropicProvider::new("anthropic", key);
        if let Ok(url) = std::env::var("ANTHROPIC_BASE_URL") {
            p = p.with_base_url(url);
        }
        executor.providers.register(Arc::new(p));
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        let mut p = OpenAiProvider::new("openai", key);
        if let Ok(url) = std::env::var("OPENAI_BASE_URL") {
            p = p.with_base_url(url);
        }
        executor.providers.register(Arc::new(p));
    }
}

pub fn load_preview_config(
    config_dir: Option<&Path>,
) -> atman_runtime::tools::preview::PreviewConfig {
    let cfg = atman_runtime::tools::preview::PreviewConfig::default();
    let Some(dir) = config_dir else {
        return cfg;
    };
    let path = dir.join("config.toml");
    if !path.exists() {
        return cfg;
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return cfg;
    };
    parse_preview_config(&text, cfg)
}

pub fn load_mcp_configs(config_dir: Option<&Path>) -> Vec<atman_runtime::mcp::McpServerConfig> {
    let Some(dir) = config_dir else {
        return Vec::new();
    };
    let path = dir.join("config.toml");
    if !path.exists() {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    parse_mcp_configs(&text)
}

#[derive(Debug, Deserialize)]
struct RawMcpConfigFile {
    #[serde(default)]
    mcp: Vec<RawMcpConfig>,
}

#[derive(Debug, Deserialize)]
struct RawMcpConfig {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    tier: Option<u8>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub fn parse_mcp_configs(text: &str) -> Vec<atman_runtime::mcp::McpServerConfig> {
    let file: RawMcpConfigFile = match toml::from_str(text) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    file.mcp
        .into_iter()
        .map(|raw| atman_runtime::mcp::McpServerConfig {
            name: raw.name,
            command: raw.command,
            args: raw.args,
            tier: tier_from_int(raw.tier.unwrap_or(3)),
            timeout_ms: raw.timeout_ms.unwrap_or(30_000),
        })
        .collect()
}

fn tier_from_int(n: u8) -> atman_runtime::Tier {
    match n {
        0 => atman_runtime::Tier::Zero,
        1 => atman_runtime::Tier::One,
        2 => atman_runtime::Tier::Two,
        3 => atman_runtime::Tier::Three,
        _ => atman_runtime::Tier::Four,
    }
}

pub fn parse_preview_config(
    text: &str,
    mut cfg: atman_runtime::tools::preview::PreviewConfig,
) -> atman_runtime::tools::preview::PreviewConfig {
    let mut in_section = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[')
            && let Some(name) = rest.strip_suffix(']')
        {
            in_section = name.trim() == "preview";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        let val = v.trim().trim_matches('"');
        match key {
            "base_url" => cfg.base_url = val.to_string(),
            "timeout_ms" => {
                if let Ok(n) = val.parse::<u64>() {
                    cfg.timeout_ms = n;
                }
            }
            "project_abs_path" => cfg.project_abs_path = val.to_string(),
            "project_hint_slug" => cfg.project_hint_slug = Some(val.to_string()),
            "max_body_bytes" => {
                if let Ok(n) = val.parse::<usize>() {
                    cfg.max_body_bytes = n;
                }
            }
            _ => {}
        }
    }
    cfg
}

pub fn default_config_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "atman", "atman")
        .context("no home dir for atman config")?;
    Ok(dirs.config_dir().to_path_buf())
}

pub fn default_data_dir() -> Result<PathBuf> {
    let dirs = directories::ProjectDirs::from("com", "atman", "atman")
        .context("no home dir for atman data")?;
    Ok(dirs.data_dir().to_path_buf())
}
