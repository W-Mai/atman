//! Shared MCP server config file management.
//!
//! Reads and writes `mcp_servers.json` in the standard
//! `{ "mcpServers": { ... } }` format (Claude Desktop / Cursor / Cline compatible).
//!
//! Used by the CLI (`atman mcp add/remove`) and the TUI MCP panel actions.

use std::path::{Path, PathBuf};

use crate::storage;

/// Path to `mcp_servers.json` in the atman config directory.
pub fn config_path() -> Result<PathBuf, String> {
    let dir = storage::config_dir().map_err(|e| e.to_string())?;
    Ok(dir.join("mcp_servers.json"))
}

/// Path to `mcp_servers.json` under an explicit config directory.
pub fn config_path_in(config_dir: &Path) -> PathBuf {
    config_dir.join("mcp_servers.json")
}

/// Read the raw JSON value from disk. Returns an empty `{"mcpServers": {}}`
/// if the file does not exist or cannot be parsed.
pub fn load_raw() -> serde_json::Value {
    let path = match config_path() {
        Ok(p) => p,
        Err(_) => return serde_json::json!({"mcpServers": {}}),
    };
    load_from(&path)
}

/// Read the raw JSON value from an explicit config directory.
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

/// Write the raw JSON value to disk, creating parent dirs as needed.
pub fn save_raw(root: &serde_json::Value) -> Result<(), String> {
    let path = config_path()?;
    save_to(&path, root)
}

/// Write the raw JSON value to an explicit config directory.
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

/// Remove a server from the config file. Returns `Ok(())` if removed,
/// `Err` if the file cannot be written or the server was not found.
pub fn remove(name: &str) -> Result<(), String> {
    let mut root = load_raw();
    remove_from(&mut root, name)?;
    save_raw(&root)
}

/// Remove a server from an explicit config directory.
pub fn remove_in(config_dir: &Path, name: &str) -> Result<(), String> {
    let mut root = load_raw_in(config_dir);
    remove_from(&mut root, name)?;
    save_raw_in(config_dir, &root)
}

fn remove_from(root: &mut serde_json::Value, name: &str) -> Result<(), String> {
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

/// Toggle the `disabled` flag on a server. Returns `Ok(new_disabled_value)`
/// on success, `Err` if the file cannot be written or the server was not
/// found. If the server has no `disabled` field, it is set to `true`.
pub fn toggle_disabled(name: &str) -> Result<bool, String> {
    let mut root = load_raw();
    let new_val = toggle_in(&mut root, name)?;
    save_raw(&root)?;
    Ok(new_val)
}

/// Toggle the `disabled` flag in an explicit config directory.
pub fn toggle_disabled_in(config_dir: &Path, name: &str) -> Result<bool, String> {
    let mut root = load_raw_in(config_dir);
    let new_val = toggle_in(&mut root, name)?;
    save_raw_in(config_dir, &root)?;
    Ok(new_val)
}

fn toggle_in(root: &mut serde_json::Value, name: &str) -> Result<bool, String> {
    let servers = root
        .as_object_mut()
        .and_then(|o| o.get_mut("mcpServers"))
        .and_then(|s| s.as_object_mut())
        .ok_or_else(|| "malformed mcp_servers.json".to_string())?;
    let server = servers
        .get_mut(name)
        .ok_or_else(|| format!("MCP server \"{name}\" not found in mcp_servers.json"))?;
    let server_obj = server
        .as_object_mut()
        .ok_or_else(|| format!("server \"{name}\" is not a JSON object"))?;
    let current = server_obj
        .get("disabled")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let new_val = !current;
    server_obj.insert("disabled".to_string(), serde_json::Value::Bool(new_val));
    Ok(new_val)
}

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
                "test-srv": {
                    "command": "echo",
                    "args": ["hello"],
                }
            }
        });
        save_raw_in(dir.path(), &root).unwrap();
        let loaded = load_raw_in(dir.path());
        assert_eq!(loaded, root);
    }

    #[test]
    fn remove_deletes_server() {
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
    fn remove_missing_server_errors() {
        let dir = tempfile::tempdir().unwrap();
        save_raw_in(
            dir.path(),
            &serde_json::json!({
                "mcpServers": { "a": { "command": "echo" } }
            }),
        )
        .unwrap();
        assert!(remove_in(dir.path(), "nonexistent").is_err());
    }

    #[test]
    fn toggle_disabled_flips_flag() {
        let dir = tempfile::tempdir().unwrap();
        save_raw_in(
            dir.path(),
            &serde_json::json!({
                "mcpServers": { "srv": { "command": "echo" } }
            }),
        )
        .unwrap();
        // No disabled field → toggle sets it to true
        let new_val = toggle_disabled_in(dir.path(), "srv").unwrap();
        assert!(new_val);
        let loaded = load_raw_in(dir.path());
        assert_eq!(
            loaded["mcpServers"]["srv"]["disabled"].as_bool(),
            Some(true)
        );
        // Toggle again → false
        let new_val = toggle_disabled_in(dir.path(), "srv").unwrap();
        assert!(!new_val);
        let loaded = load_raw_in(dir.path());
        assert_eq!(
            loaded["mcpServers"]["srv"]["disabled"].as_bool(),
            Some(false)
        );
    }

    #[test]
    fn toggle_disabled_missing_server_errors() {
        let dir = tempfile::tempdir().unwrap();
        save_raw_in(dir.path(), &serde_json::json!({"mcpServers": {}})).unwrap();
        assert!(toggle_disabled_in(dir.path(), "nonexistent").is_err());
    }
}
