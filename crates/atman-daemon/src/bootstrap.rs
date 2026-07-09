use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use atman_runtime::event::EventSink;
use atman_runtime::providers::anthropic::AnthropicProvider;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::providers::openai::OpenAiProvider;
use atman_runtime::sandbox::Sandbox;
use atman_runtime::{Executor, Value, tools};
use serde::Deserialize;

pub struct BootstrapOptions {
    pub events: EventSink,
    pub mock: bool,
    pub config_dir: Option<PathBuf>,
    pub project_root: PathBuf,
    pub home_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct SandboxConfig {
    pub enabled: bool,
    pub strict: bool,
    pub extra_read: Vec<PathBuf>,
    pub extra_write: Vec<PathBuf>,
    pub template_path: Option<PathBuf>,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            strict: false,
            extra_read: Vec::new(),
            extra_write: Vec::new(),
            template_path: None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct RedactConfig {
    pub enabled: bool,
    pub partial: bool,
    pub allowlist: Vec<String>,
    pub custom_patterns: Vec<(String, String)>,
}

pub fn load_redact_config(config_dir: Option<&Path>) -> RedactConfig {
    let Some(dir) = config_dir else {
        return RedactConfig::default();
    };
    let path = dir.join("config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return RedactConfig::default();
    };
    parse_redact_config(&text)
}

pub fn parse_redact_config(text: &str) -> RedactConfig {
    #[derive(Debug, Deserialize, Default)]
    struct RawPattern {
        kind: String,
        regex: String,
    }
    #[derive(Debug, Deserialize, Default)]
    struct RawRedact {
        #[serde(default)]
        enabled: bool,
        #[serde(default)]
        mode: Option<String>,
        #[serde(default)]
        allowlist: Vec<String>,
        #[serde(default)]
        custom_patterns: Vec<RawPattern>,
    }
    #[derive(Debug, Deserialize, Default)]
    struct RawRedactFile {
        #[serde(default)]
        redact: RawRedact,
    }
    let file: RawRedactFile = match toml::from_str(text) {
        Ok(f) => f,
        Err(_) => return RedactConfig::default(),
    };
    RedactConfig {
        enabled: file.redact.enabled,
        partial: file.redact.mode.as_deref() == Some("partial"),
        allowlist: file.redact.allowlist,
        custom_patterns: file
            .redact
            .custom_patterns
            .into_iter()
            .map(|p| (p.kind, p.regex))
            .collect(),
    }
}

pub fn build_redactor(config_dir: Option<&Path>) -> Option<Arc<atman_runtime::redact::Redactor>> {
    let cfg = load_redact_config(config_dir);
    if !cfg.enabled {
        return None;
    }
    let mut pairs: Vec<(&str, &str)> = atman_runtime::redact::BUILTIN_PATTERNS.to_vec();
    for (k, r) in &cfg.custom_patterns {
        pairs.push((k.as_str(), r.as_str()));
    }
    let mode = if cfg.partial {
        atman_runtime::redact::RedactMode::Partial
    } else {
        atman_runtime::redact::RedactMode::Full
    };
    let redactor =
        atman_runtime::redact::Redactor::from_pairs(&pairs, mode).with_allowlist(cfg.allowlist);
    Some(Arc::new(redactor))
}

pub struct BootstrapOutcome {
    pub executor: Executor,
    pub mcp_status: Vec<Result<atman_runtime::mcp::McpClientStatus, String>>,
}

pub async fn build_executor(opts: BootstrapOptions) -> Result<BootstrapOutcome> {
    let events = opts.events.clone();
    let mut executor = Executor::with_events(events);

    let fetch_rule = build_fetch_rule(&opts.project_root, opts.home_dir.as_deref()).await;
    tools::register_tier_zero_with_rules(&mut executor.tools, fetch_rule);
    tools::register_shell(&mut executor.tools);
    tools::register_preview(
        &mut executor.tools,
        load_preview_config(opts.config_dir.as_deref()),
    );
    register_providers_from_env(&mut executor);
    if let Some(sandbox) =
        build_sandbox(&opts.project_root, opts.config_dir.as_deref()).context("sandbox init")?
    {
        executor.tool_ctx = executor.tool_ctx.clone().with_sandbox(sandbox);
    }
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

fn build_sandbox(
    project_root: &Path,
    config_dir: Option<&Path>,
) -> Result<Option<Arc<dyn atman_runtime::sandbox::Sandbox>>> {
    let cfg = load_sandbox_config(config_dir);
    if !cfg.enabled {
        return Ok(None);
    }
    let template = match &cfg.template_path {
        Some(p) => std::fs::read_to_string(p)
            .with_context(|| format!("read sandbox template {}", p.display()))?,
        None => atman_runtime::sandbox::DEFAULT_PROFILE.to_string(),
    };
    let sandbox = atman_runtime::sandbox::SandboxExec::new(project_root)
        .with_extra_read(cfg.extra_read.clone())
        .with_extra_write(cfg.extra_write.clone())
        .with_template(template);
    if !sandbox.is_available() {
        if cfg.strict {
            anyhow::bail!("sandbox enabled + strict, but sandbox-exec not available on this host");
        }
        eprintln!(
            "[atman] sandbox enabled but sandbox-exec not available; falling back to no-sandbox path"
        );
        return Ok(None);
    }
    Ok(Some(Arc::new(sandbox)))
}

pub fn load_sandbox_config(config_dir: Option<&Path>) -> SandboxConfig {
    let Some(dir) = config_dir else {
        return SandboxConfig::default();
    };
    let path = dir.join("config.toml");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return SandboxConfig::default();
    };
    parse_sandbox_config(&text)
}

pub fn parse_sandbox_config(text: &str) -> SandboxConfig {
    #[derive(Debug, Deserialize, Default)]
    struct RawSandbox {
        #[serde(default)]
        enabled: Option<bool>,
        #[serde(default)]
        strict: bool,
        #[serde(default)]
        extra_read: Vec<String>,
        #[serde(default)]
        extra_write: Vec<String>,
        #[serde(default)]
        template_path: Option<String>,
    }
    #[derive(Debug, Deserialize, Default)]
    struct RawSandboxFile {
        #[serde(default)]
        sandbox: RawSandbox,
    }
    let file: RawSandboxFile = match toml::from_str(text) {
        Ok(f) => f,
        Err(_) => return SandboxConfig::default(),
    };
    SandboxConfig {
        enabled: file.sandbox.enabled.unwrap_or(true),
        strict: file.sandbox.strict,
        extra_read: file
            .sandbox
            .extra_read
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        extra_write: file
            .sandbox
            .extra_write
            .into_iter()
            .map(PathBuf::from)
            .collect(),
        template_path: file.sandbox.template_path.map(PathBuf::from),
    }
}

pub fn attach_memory_stores(
    executor: &mut Executor,
    session_dir: &Path,
    project_scope_root: &Path,
) {
    attach_memory_stores_with_redactor(executor, session_dir, project_scope_root, None, None);
}

pub fn attach_memory_stores_with_redactor(
    executor: &mut Executor,
    session_dir: &Path,
    project_scope_root: &Path,
    redactor: Option<Arc<atman_runtime::redact::Redactor>>,
    project_index: Option<Arc<atman_runtime::index::AnchorIndex>>,
) {
    let confession_root = project_scope_root.join("confessions");
    let spec_root = project_scope_root.join("specs");
    let _ = std::fs::create_dir_all(&confession_root);
    let _ = std::fs::create_dir_all(&spec_root);
    let todo_store = Arc::new(atman_runtime::memory::todo::TodoStore::at(session_dir));
    let goal_store = Arc::new(atman_runtime::memory::goal::GoalStore::at(session_dir));
    let plan_store = Arc::new(atman_runtime::memory::plan::PlanStore::at(session_dir));
    let mut confession_store =
        atman_runtime::memory::confession::ConfessionStore::at(&confession_root);
    let mut spec_store = atman_runtime::memory::spec::SpecStore::new(spec_root);
    if let Some(idx) = &project_index {
        confession_store = confession_store.with_index(idx.clone());
        spec_store = spec_store.with_index(idx.clone());
    }
    if let Some(r) = &redactor {
        confession_store = confession_store.with_redactor(r.clone());
    }
    let confession_store = Arc::new(confession_store);
    let spec_store = Arc::new(spec_store);
    tools::register_memory(
        &mut executor.tools,
        todo_store,
        confession_store,
        goal_store,
        plan_store,
    );
    tools::register_spec_memory(&mut executor.tools, spec_store);
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
    #[serde(default)]
    transport: Option<String>,
    #[serde(default)]
    command: Option<String>,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    auth_token: Option<String>,
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
        .map(|raw| {
            let transport = match raw.transport.as_deref() {
                Some("http") => atman_runtime::mcp::TransportKind::Http,
                _ => atman_runtime::mcp::TransportKind::Stdio,
            };
            atman_runtime::mcp::McpServerConfig {
                name: raw.name,
                transport,
                command: raw.command.unwrap_or_default(),
                args: raw.args,
                url: raw.url,
                auth_token: raw.auth_token,
                tier: tier_from_int(raw.tier.unwrap_or(3)),
                timeout_ms: raw.timeout_ms.unwrap_or(30_000),
            }
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
    atman_runtime::storage::config_dir()
}

pub fn default_data_dir() -> Result<PathBuf> {
    atman_runtime::storage::data_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_config_defaults_to_enabled() {
        let cfg = SandboxConfig::default();
        assert!(cfg.enabled);
        assert!(!cfg.strict);
    }

    #[test]
    fn parse_sandbox_config_defaults_missing_enabled_to_true() {
        let cfg = parse_sandbox_config("[sandbox]\nstrict = true\n");
        assert!(cfg.enabled);
        assert!(cfg.strict);
    }

    #[test]
    fn parse_sandbox_config_defaults_missing_section_to_enabled() {
        let cfg = parse_sandbox_config("[preview]\ntimeout_ms = 1000\n");
        assert!(cfg.enabled);
        assert!(!cfg.strict);
    }

    #[test]
    fn parse_sandbox_config_allows_explicit_opt_out() {
        let cfg = parse_sandbox_config("[sandbox]\nenabled = false\n");
        assert!(!cfg.enabled);
    }
}
