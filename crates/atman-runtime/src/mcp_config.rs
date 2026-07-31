//! MCP server config file management — single source of truth.
//!
//! Reads and writes `mcp_servers.json` and `config.toml [[mcp]]` blocks.
//! All operations go through [`McpServerConfig`]:
//!
//! - [`load`] → `Vec<McpServerConfig>` (from JSON + TOML + Claude Desktop)
//! - [`save`] → writes to `mcp_servers.json`
//! - [`toggle_disabled`] = load → toggle → save
//! - [`remove`] = load → filter → save

use std::path::{Path, PathBuf};

use crate::mcp::{McpServerConfig, TransportKind};
use crate::storage;
use crate::tool::Tier;

pub fn json_path() -> Result<PathBuf, String> {
    let dir = storage::config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("mcp_servers.json"))
}

pub fn json_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("mcp_servers.json")
}

fn toml_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("config.toml")
}

/// Load all MCP server configs from all sources: `config.toml` [[mcp]]
/// blocks, `mcp_servers.json`, and Claude Desktop's config (read-only).
/// Later entries override earlier ones by name.
pub fn load(config_dir: Option<&Path>) -> Vec<McpServerConfig> {
    let mut configs = load_in(config_dir.unwrap_or(Path::new("")));

    // Claude Desktop auto-discover (read-only, macOS only)
    if let Ok(home) = std::env::var("HOME") {
        let claude_path = PathBuf::from(home)
            .join("Library/Application Support/Claude/claude_desktop_config.json");
        if claude_path.exists() {
            if let Ok(text) = std::fs::read_to_string(&claude_path) {
                configs.extend(parse_mcp_json(&text));
            }
        }
    }

    dedup_keep_last(configs)
}

/// Load from a specific config directory only (no Claude Desktop discovery).
pub fn load_in(config_dir: &Path) -> Vec<McpServerConfig> {
    let mut configs = Vec::new();

    let toml_path = toml_path_in(config_dir);
    if toml_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&toml_path) {
            configs.extend(parse_mcp_toml(&text));
        }
    }

    let json_path = json_path_in(config_dir);
    if json_path.exists() {
        if let Ok(text) = std::fs::read_to_string(&json_path) {
            configs.extend(parse_mcp_json(&text));
        }
    }

    dedup_keep_last(configs)
}

/// Deduplicate by name, keeping the LAST occurrence (later sources override).
fn dedup_keep_last(configs: Vec<McpServerConfig>) -> Vec<McpServerConfig> {
    let mut seen = std::collections::HashSet::new();
    let mut result: Vec<McpServerConfig> = Vec::with_capacity(configs.len());
    for cfg in configs.into_iter().rev() {
        if seen.insert(cfg.name.clone()) {
            result.push(cfg);
        }
    }
    result.reverse();
    result
}

/// Parse standard `mcpServers` JSON format.
pub fn parse_mcp_json(text: &str) -> Vec<McpServerConfig> {
    #[derive(serde::Deserialize)]
    struct JsonConfigFile {
        #[serde(default, rename = "mcpServers")]
        mcp_servers: std::collections::HashMap<String, JsonServerConfig>,
    }

    #[derive(serde::Deserialize)]
    struct JsonServerConfig {
        #[serde(default)]
        r#type: Option<String>,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default, rename = "authToken")]
        auth_token: Option<String>,
        #[serde(default)]
        headers: std::collections::HashMap<String, String>,
        #[serde(default)]
        disabled: bool,
        #[serde(default)]
        tier: Option<String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
    }

    let file: JsonConfigFile = match serde_json::from_str(text) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    file.mcp_servers
        .into_iter()
        .map(|(name, raw)| {
            let (transport, command, url) = match raw.r#type.as_deref() {
                Some("sse") => (TransportKind::Sse, String::new(), raw.url),
                Some("http") => (TransportKind::Http, String::new(), raw.url),
                _ => (TransportKind::Stdio, raw.command.unwrap_or_default(), None),
            };
            McpServerConfig {
                name,
                transport,
                command,
                args: raw.args,
                env: raw.env.into_iter().collect(),
                url,
                auth_token: raw.auth_token,
                headers: raw.headers.into_iter().collect(),
                tier: parse_tier_str(raw.tier.as_deref()),
                timeout_ms: raw.timeout_ms.unwrap_or(30_000),
                disabled: raw.disabled,
            }
        })
        .collect()
}

/// Parse `config.toml` `[[mcp]]` blocks.
pub fn parse_mcp_toml(text: &str) -> Vec<McpServerConfig> {
    #[derive(Debug, serde::Deserialize)]
    struct RawMcpConfigFile {
        #[serde(default)]
        mcp: Vec<RawMcpConfig>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct RawMcpConfig {
        name: String,
        #[serde(default)]
        transport: Option<String>,
        #[serde(default)]
        command: Option<String>,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        env: std::collections::HashMap<String, String>,
        #[serde(default)]
        url: Option<String>,
        #[serde(default)]
        auth_token: Option<String>,
        #[serde(default)]
        headers: std::collections::HashMap<String, String>,
        #[serde(default)]
        tier: Option<u8>,
        #[serde(default)]
        timeout_ms: Option<u64>,
        #[serde(default)]
        disabled: bool,
    }

    let file: RawMcpConfigFile = match toml::from_str(text) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    file.mcp
        .into_iter()
        .map(|raw| {
            let transport = match raw.transport.as_deref() {
                Some("http") => TransportKind::Http,
                Some("sse") => TransportKind::Sse,
                _ => TransportKind::Stdio,
            };
            McpServerConfig {
                name: raw.name,
                transport,
                command: raw.command.unwrap_or_default(),
                args: raw.args,
                env: raw.env.into_iter().collect(),
                url: raw.url,
                auth_token: raw.auth_token,
                headers: raw.headers.into_iter().collect(),
                tier: tier_from_int(raw.tier.unwrap_or(3)),
                timeout_ms: raw.timeout_ms.unwrap_or(30_000),
                disabled: raw.disabled,
            }
        })
        .collect()
}

fn parse_tier_str(s: Option<&str>) -> Tier {
    match s {
        Some("Zero") | Some("zero") | Some("0") => Tier::Zero,
        Some("One") | Some("one") | Some("1") => Tier::One,
        Some("Two") | Some("two") | Some("2") => Tier::Two,
        Some("Three") | Some("three") | Some("3") => Tier::Three,
        Some("Four") | Some("four") | Some("4") => Tier::Four,
        _ => Tier::Three,
    }
}

fn tier_from_int(n: u8) -> Tier {
    match n {
        0 => Tier::Zero,
        1 => Tier::One,
        2 => Tier::Two,
        3 => Tier::Three,
        _ => Tier::Four,
    }
}

fn tier_to_str(t: Tier) -> &'static str {
    match t {
        Tier::Zero => "Zero",
        Tier::One => "One",
        Tier::Two => "Two",
        Tier::Three => "Three",
        Tier::Four => "Four",
    }
}

/// Save configs to `mcp_servers.json`. Overwrites the file entirely.
pub fn save(configs: &[McpServerConfig]) -> Result<(), String> {
    let dir = storage::config_dir().map_err(|e| e.to_string())?;
    save_in(&dir, configs)
}

/// Save configs to `mcp_servers.json` in an explicit config directory.
pub fn save_in(config_dir: &Path, configs: &[McpServerConfig]) -> Result<(), String> {
    let path = json_path_in(config_dir);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let root = configs_to_json(configs);
    let json = serde_json::to_string_pretty(&root).map_err(|e| e.to_string())?;
    std::fs::write(&path, json + "\n").map_err(|e| e.to_string())?;
    Ok(())
}

fn configs_to_json(configs: &[McpServerConfig]) -> serde_json::Value {
    let mut servers = serde_json::Map::new();
    for cfg in configs {
        servers.insert(cfg.name.clone(), config_to_json_value(cfg));
    }
    serde_json::json!({ "mcpServers": serde_json::Value::Object(servers) })
}

fn config_to_json_value(cfg: &McpServerConfig) -> serde_json::Value {
    let mut obj = serde_json::Map::new();
    match cfg.transport {
        TransportKind::Sse => {
            obj.insert("type".into(), "sse".into());
        }
        TransportKind::Http => {
            obj.insert("type".into(), "http".into());
        }
        TransportKind::Stdio => {}
    }
    if !cfg.command.is_empty() {
        obj.insert("command".into(), cfg.command.clone().into());
    }
    if !cfg.args.is_empty() {
        obj.insert(
            "args".into(),
            serde_json::Value::Array(cfg.args.iter().map(|a| a.clone().into()).collect()),
        );
    }
    if !cfg.env.is_empty() {
        obj.insert(
            "env".into(),
            serde_json::Value::Object(
                cfg.env
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone().into()))
                    .collect(),
            ),
        );
    }
    if let Some(url) = &cfg.url {
        obj.insert("url".into(), url.clone().into());
    }
    if let Some(token) = &cfg.auth_token {
        obj.insert("authToken".into(), token.clone().into());
    }
    if !cfg.headers.is_empty() {
        obj.insert(
            "headers".into(),
            serde_json::Value::Object(
                cfg.headers
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone().into()))
                    .collect(),
            ),
        );
    }
    obj.insert("tier".into(), tier_to_str(cfg.tier).into());
    obj.insert("timeout_ms".into(), cfg.timeout_ms.into());
    if cfg.disabled {
        obj.insert("disabled".into(), true.into());
    }
    serde_json::Value::Object(obj)
}

/// Toggle the `disabled` flag on a server. Returns the new disabled value.
pub fn toggle_disabled(name: &str) -> Result<bool, String> {
    let dir = storage::config_dir().map_err(|e| e.to_string())?;
    toggle_disabled_in(&dir, name)
}

/// Toggle `disabled` in an explicit config directory.
pub fn toggle_disabled_in(config_dir: &Path, name: &str) -> Result<bool, String> {
    let mut configs = load_in(config_dir);
    let cfg = configs
        .iter_mut()
        .find(|c| c.name == name)
        .ok_or_else(|| format!("MCP server \"{name}\" not found"))?;
    cfg.disabled = !cfg.disabled;
    let new_val = cfg.disabled;
    save_in(config_dir, &configs)?;
    Ok(new_val)
}

/// Remove a server from the config.
pub fn remove(name: &str) -> Result<(), String> {
    let dir = storage::config_dir().map_err(|e| e.to_string())?;
    remove_in(&dir, name)
}

/// Remove a server from an explicit config directory.
pub fn remove_in(config_dir: &Path, name: &str) -> Result<(), String> {
    let mut configs = load_in(config_dir);
    let before = configs.len();
    configs.retain(|c| c.name != name);
    if configs.len() == before {
        return Err(format!("MCP server \"{name}\" not found"));
    }
    save_in(config_dir, &configs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_empty_when_no_files() {
        let dir = tempfile::tempdir().unwrap();
        let configs = load_in(dir.path());
        assert!(configs.is_empty());
    }

    #[test]
    fn save_then_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let original = vec![
            McpServerConfig::stdio("srv-a", "echo", vec!["hello".into()], Tier::Two, 30_000),
            McpServerConfig::http(
                "srv-b",
                "https://api.example.com",
                Some("secret".into()),
                Tier::Three,
                30_000,
            ),
        ];
        save_in(dir.path(), &original).unwrap();
        let loaded = load_in(dir.path());
        assert_eq!(loaded.len(), 2);
        let a = loaded.iter().find(|c| c.name == "srv-a").unwrap();
        assert_eq!(a.command, "echo");
        assert_eq!(a.transport, TransportKind::Stdio);
        let b = loaded.iter().find(|c| c.name == "srv-b").unwrap();
        assert_eq!(b.transport, TransportKind::Http);
        assert_eq!(b.url.as_deref(), Some("https://api.example.com"));
    }

    #[test]
    fn toggle_disabled_load_modify_save() {
        let dir = tempfile::tempdir().unwrap();
        save_in(
            dir.path(),
            &[McpServerConfig::stdio(
                "srv",
                "echo",
                vec![],
                Tier::Two,
                30_000,
            )],
        )
        .unwrap();

        assert!(toggle_disabled_in(dir.path(), "srv").unwrap());
        let loaded = load_in(dir.path());
        assert!(loaded[0].disabled);

        assert!(!toggle_disabled_in(dir.path(), "srv").unwrap());
        let loaded = load_in(dir.path());
        assert!(!loaded[0].disabled);
    }

    #[test]
    fn toggle_disabled_on_toml_config() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            toml_path_in(dir.path()),
            "[[mcp]]\nname = \"exa\"\ncommand = \"exa-mcp-server\"\ntimeout_ms = 30000\n",
        )
        .unwrap();

        assert!(toggle_disabled_in(dir.path(), "exa").unwrap());
        let loaded = load_in(dir.path());
        assert_eq!(loaded.len(), 1);
        assert!(loaded[0].disabled);
        assert!(json_path_in(dir.path()).exists());
    }

    #[test]
    fn remove_server() {
        let dir = tempfile::tempdir().unwrap();
        save_in(
            dir.path(),
            &[
                McpServerConfig::stdio("a", "echo", vec![], Tier::Two, 30_000),
                McpServerConfig::stdio("b", "ls", vec![], Tier::Two, 30_000),
            ],
        )
        .unwrap();

        remove_in(dir.path(), "a").unwrap();
        let loaded = load_in(dir.path());
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "b");
    }

    #[test]
    fn remove_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        save_in(
            dir.path(),
            &[McpServerConfig::stdio(
                "a",
                "echo",
                vec![],
                Tier::Two,
                30_000,
            )],
        )
        .unwrap();
        assert!(remove_in(dir.path(), "nonexistent").is_err());
    }

    #[test]
    fn toggle_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        save_in(
            dir.path(),
            &[McpServerConfig::stdio(
                "a",
                "echo",
                vec![],
                Tier::Two,
                30_000,
            )],
        )
        .unwrap();
        assert!(toggle_disabled_in(dir.path(), "nonexistent").is_err());
    }

    #[test]
    fn dedup_keeps_last() {
        let configs = vec![
            McpServerConfig::stdio("a", "from-toml", vec![], Tier::Two, 30_000),
            McpServerConfig::stdio("a", "from-json", vec![], Tier::Two, 30_000),
        ];
        let deduped = dedup_keep_last(configs);
        assert_eq!(deduped.len(), 1);
        assert_eq!(deduped[0].command, "from-json");
    }
}
