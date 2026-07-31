//! Shared MCP server config file management.
//!
//! Reads and writes `mcp_servers.json` (standard `{ "mcpServers": { ... } }`
//! format) and `config.toml` `[[mcp]]` blocks. Tries JSON first, falls back
//! to TOML.

use std::path::{Path, PathBuf};

use crate::storage;

// ── Path helpers ───────────────────────────────────────────────────

pub fn config_path() -> Result<PathBuf, String> {
    let dir = storage::config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("mcp_servers.json"))
}

pub fn config_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("mcp_servers.json")
}

fn config_toml_path() -> Result<PathBuf, String> {
    let dir = storage::config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("config.toml"))
}

fn config_toml_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("config.toml")
}

// ── JSON load/save ─────────────────────────────────────────────────

pub fn load_raw() -> serde_json::Value {
    let path = match config_path() {
        Ok(p) => p,
        Err(_) => return serde_json::json!({"mcpServers": {}}),
    };
    load_from(&path)
}

pub fn load_raw_in(config_dir: &Path) -> serde_json::Value {
    load_from(&config_path_in(config_dir))
}

fn load_from(path: &Path) -> serde_json::Value {
    if !path.exists() {
        return serde_json::json!({"mcpServers": {}});
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(_) => return serde_json::json!({"mcpServers": {}}),
    };
    serde_json::from_str(&text).unwrap_or_else(|_| serde_json::json!({"mcpServers": {}}))
}

pub fn save_raw(root: &serde_json::Value) -> Result<(), String> {
    let path = config_path()?;
    save_to(&path, root)
}

pub fn save_raw_in(config_dir: &Path, root: &serde_json::Value) -> Result<(), String> {
    save_to(&config_path_in(config_dir), root)
}

fn save_to(path: &Path, root: &serde_json::Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(root).map_err(|e| e.to_string())?;
    std::fs::write(path, json + "\n").map_err(|e| e.to_string())?;
    Ok(())
}

// ── Remove ─────────────────────────────────────────────────────────

/// Remove a server. Tries `mcp_servers.json` first, then `config.toml`.
pub fn remove(name: &str) -> Result<(), String> {
    let dir = storage::config_dir().map_err(|e| e.to_string())?;
    remove_in(&dir, name)
}

pub fn remove_in(config_dir: &Path, name: &str) -> Result<(), String> {
    // Try JSON first
    let mut root = load_raw_in(config_dir);
    if json_has_server(&root, name) {
        json_remove(&mut root, name)?;
        return save_raw_in(config_dir, &root);
    }
    // Fall back to TOML
    let toml_path = config_toml_path_in(config_dir);
    if toml_path.exists() {
        let text = std::fs::read_to_string(&toml_path).map_err(|e| e.to_string())?;
        let new_text = remove_mcp_block(&text, name)?;
        std::fs::write(&toml_path, new_text).map_err(|e| e.to_string())?;
        return Ok(());
    }
    Err(format!(
        "MCP server \"{name}\" not found in mcp_servers.json or config.toml"
    ))
}

// ── Toggle disabled ────────────────────────────────────────────────

/// Toggle `disabled` flag. Tries `mcp_servers.json` first, then `config.toml`.
pub fn toggle_disabled(name: &str) -> Result<bool, String> {
    let dir = storage::config_dir().map_err(|e| e.to_string())?;
    toggle_disabled_in(&dir, name)
}

pub fn toggle_disabled_in(config_dir: &Path, name: &str) -> Result<bool, String> {
    // Try JSON first
    let mut root = load_raw_in(config_dir);
    if json_has_server(&root, name) {
        let new_val = json_toggle(&mut root, name)?;
        save_raw_in(config_dir, &root)?;
        return Ok(new_val);
    }
    // Fall back to TOML
    let toml_path = config_toml_path_in(config_dir);
    if toml_path.exists() {
        let text = std::fs::read_to_string(&toml_path).map_err(|e| e.to_string())?;
        let (new_text, new_disabled) = toggle_mcp_block(&text, name)?;
        std::fs::write(&toml_path, new_text).map_err(|e| e.to_string())?;
        return Ok(new_disabled);
    }
    Err(format!(
        "MCP server \"{name}\" not found in mcp_servers.json or config.toml"
    ))
}

// ── JSON helpers ───────────────────────────────────────────────────

fn json_has_server(root: &serde_json::Value, name: &str) -> bool {
    root.get("mcpServers")
        .and_then(|s| s.as_object())
        .is_some_and(|s| s.contains_key(name))
}

fn json_remove(root: &mut serde_json::Value, name: &str) -> Result<(), String> {
    let removed = root
        .as_object_mut()
        .and_then(|o| o.get_mut("mcpServers"))
        .and_then(|s| s.as_object_mut())
        .is_some_and(|s| s.remove(name).is_some());
    if !removed {
        return Err(format!(
            "MCP server \"{name}\" not found in mcp_servers.json"
        ));
    }
    Ok(())
}

fn json_toggle(root: &mut serde_json::Value, name: &str) -> Result<bool, String> {
    let server = root
        .as_object_mut()
        .and_then(|o| o.get_mut("mcpServers"))
        .and_then(|s| s.as_object_mut())
        .and_then(|s| s.get_mut(name))
        .ok_or_else(|| format!("MCP server \"{name}\" not found in mcp_servers.json"))?;
    let obj = server
        .as_object_mut()
        .ok_or_else(|| format!("server \"{name}\" is not a JSON object"))?;
    let current = obj
        .get("disabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let new_val = !current;
    obj.insert("disabled".to_string(), serde_json::Value::Bool(new_val));
    Ok(new_val)
}

// ── TOML text-based editing ────────────────────────────────────────

/// Toggle `disabled` in a `[[mcp]]` block. Returns (new_text, new_disabled).
fn toggle_mcp_block(text: &str, name: &str) -> Result<(String, bool), String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut result: Vec<String> = Vec::with_capacity(lines.len());
    let mut i = 0;
    let mut found = false;
    let mut new_disabled = false;

    while i < lines.len() {
        if lines[i].trim() == "[[mcp]]" {
            let block_start = i;
            let mut block_end = i + 1;
            while block_end < lines.len() && !lines[block_end].trim().starts_with("[[") {
                block_end += 1;
            }
            let block_lines = &lines[block_start..block_end];
            let block_text = block_lines.join("\n");

            if extract_toml_string_field(&block_text, "name").as_deref() == Some(name) {
                found = true;
                let current = extract_toml_bool_field(&block_text, "disabled").unwrap_or(false);
                new_disabled = !current;

                let mut rebuilt: Vec<String> = Vec::new();
                let mut disabled_set = false;
                for bl in block_lines {
                    if bl.trim().starts_with("disabled") && bl.contains('=') {
                        rebuilt.push(format!("disabled = {new_disabled}"));
                        disabled_set = true;
                    } else {
                        rebuilt.push(bl.to_string());
                    }
                }
                if !disabled_set {
                    let name_idx = rebuilt
                        .iter()
                        .position(|l| l.trim().starts_with("name") && l.contains('='))
                        .unwrap_or(0);
                    rebuilt.insert(name_idx + 1, format!("disabled = {new_disabled}"));
                }
                result.extend(rebuilt);
                i = block_end;
                continue;
            }
            for bl in block_lines {
                result.push(bl.to_string());
            }
            i = block_end;
        } else {
            result.push(lines[i].to_string());
            i += 1;
        }
    }

    if !found {
        return Err(format!("MCP server \"{name}\" not found in config.toml"));
    }
    Ok((result.join("\n"), new_disabled))
}

/// Remove the `[[mcp]]` block with matching name.
fn remove_mcp_block(text: &str, name: &str) -> Result<String, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut result: Vec<String> = Vec::new();
    let mut i = 0;
    let mut found = false;

    while i < lines.len() {
        if lines[i].trim() == "[[mcp]]" {
            let block_start = i;
            let mut block_end = i + 1;
            while block_end < lines.len() && !lines[block_end].trim().starts_with("[[") {
                block_end += 1;
            }
            let block_lines = &lines[block_start..block_end];
            let block_text = block_lines.join("\n");

            if extract_toml_string_field(&block_text, "name").as_deref() == Some(name) {
                found = true;
                i = block_end;
                if i < lines.len() && lines[i].trim().is_empty() {
                    i += 1;
                }
                continue;
            }
            for bl in block_lines {
                result.push(bl.to_string());
            }
            i = block_end;
        } else {
            result.push(lines[i].to_string());
            i += 1;
        }
    }

    if !found {
        return Err(format!("MCP server \"{name}\" not found in config.toml"));
    }
    Ok(result.join("\n"))
}

fn extract_toml_string_field(text: &str, field: &str) -> Option<String> {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(field) {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                return Some(rest.trim().trim_matches('"').to_string());
            }
        }
    }
    None
}

fn extract_toml_bool_field(text: &str, field: &str) -> Option<bool> {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(field) {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                return Some(rest.trim().starts_with("true"));
            }
        }
    }
    None
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_raw_in_returns_empty_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = load_raw_in(dir.path());
        assert!(root.get("mcpServers").is_some());
        assert!(root["mcpServers"].as_object().unwrap().is_empty());
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let root = serde_json::json!({
            "mcpServers": {
                "test-srv": { "command": "echo", "args": ["hello"] }
            }
        });
        save_raw_in(dir.path(), &root).unwrap();
        let loaded = load_raw_in(dir.path());
        assert_eq!(loaded, root);
    }

    #[test]
    fn remove_from_json() {
        let dir = tempfile::tempdir().unwrap();
        save_raw_in(
            dir.path(),
            &serde_json::json!({
                "mcpServers": {
                    "a": { "command": "echo" },
                    "b": { "command": "ls" },
                }
            }),
        )
        .unwrap();
        remove_in(dir.path(), "a").unwrap();
        let loaded = load_raw_in(dir.path());
        assert!(loaded["mcpServers"].as_object().unwrap().get("a").is_none());
        assert!(loaded["mcpServers"].as_object().unwrap().get("b").is_some());
    }

    #[test]
    fn remove_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        save_raw_in(
            dir.path(),
            &serde_json::json!({"mcpServers": {"a": {"command": "echo"}}}),
        )
        .unwrap();
        assert!(remove_in(dir.path(), "nonexistent").is_err());
    }

    #[test]
    fn toggle_disabled_json() {
        let dir = tempfile::tempdir().unwrap();
        save_raw_in(
            dir.path(),
            &serde_json::json!({"mcpServers": {"srv": {"command": "echo"}}}),
        )
        .unwrap();
        assert!(toggle_disabled_in(dir.path(), "srv").unwrap());
        let loaded = load_raw_in(dir.path());
        assert_eq!(
            loaded["mcpServers"]["srv"]["disabled"].as_bool(),
            Some(true)
        );
        assert!(!toggle_disabled_in(dir.path(), "srv").unwrap());
    }

    // ── TOML tests ──

    #[test]
    fn toggle_disabled_toml() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = config_toml_path_in(dir.path());
        std::fs::write(
            &toml_path,
            "[compaction]\nreview = \"manual-only\"\n\n[[mcp]]\nname = \"exa\"\ncommand = \"exa-mcp-server\"\ntimeout_ms = 30000\n\n[[mcp]]\nname = \"feishu\"\ncommand = \"node\"\n",
        )
        .unwrap();

        // Toggle exa → disabled = true
        let new_val = toggle_disabled_in(dir.path(), "exa").unwrap();
        assert!(new_val);
        let text = std::fs::read_to_string(&toml_path).unwrap();
        assert!(text.contains("disabled = true"));
        assert!(text.contains("[compaction]")); // other content preserved
        assert!(text.contains("name = \"feishu\"")); // other mcp block preserved

        // Toggle exa again → disabled = false
        let new_val = toggle_disabled_in(dir.path(), "exa").unwrap();
        assert!(!new_val);
        let text = std::fs::read_to_string(&toml_path).unwrap();
        assert!(text.contains("disabled = false"));
    }

    #[test]
    fn remove_from_toml() {
        let dir = tempfile::tempdir().unwrap();
        let toml_path = config_toml_path_in(dir.path());
        std::fs::write(
            &toml_path,
            "[compaction]\nreview = \"manual-only\"\n\n[[mcp]]\nname = \"exa\"\ncommand = \"exa-mcp-server\"\n\n[[mcp]]\nname = \"feishu\"\ncommand = \"node\"\n",
        )
        .unwrap();

        remove_in(dir.path(), "exa").unwrap();
        let text = std::fs::read_to_string(&toml_path).unwrap();
        assert!(!text.contains("exa"));
        assert!(text.contains("name = \"feishu\""));
        assert!(text.contains("[compaction]"));
    }

    #[test]
    fn toggle_toml_missing_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            config_toml_path_in(dir.path()),
            "[[mcp]]\nname = \"other\"\n",
        )
        .unwrap();
        assert!(toggle_disabled_in(dir.path(), "nonexistent").is_err());
    }
}
