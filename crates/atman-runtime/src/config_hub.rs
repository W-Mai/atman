use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use crate::model_registry::ModelConfigUpdate;

static CONFIG_WRITE_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml_edit::TomlError),
    Invalid(String),
    NameConflict { name: String, domain: &'static str },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "config I/O: {error}"),
            Self::Parse(error) => write!(f, "parse config.toml: {error}"),
            Self::Invalid(message) => f.write_str(message),
            Self::NameConflict { name, domain } => {
                write!(f, "config name {name:?} already exists in {domain}")
            }
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<std::io::Error> for ConfigError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<toml_edit::TomlError> for ConfigError {
    fn from(error: toml_edit::TomlError) -> Self {
        Self::Parse(error)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemePreference {
    Auto,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderConfigUpdate<'a> {
    pub name: &'a str,
    pub kind: &'a str,
    pub api_key: Option<&'a str>,
    pub api_key_env: Option<&'a str>,
    pub base_url: Option<&'a str>,
    pub max_tokens: Option<u32>,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct ConfigHub {
    config_dir: PathBuf,
}

impl ConfigHub {
    pub fn global() -> Result<Self, ConfigError> {
        let dir = crate::storage::config_dir()
            .map_err(|error| ConfigError::Invalid(format!("config dir: {error}")))?;
        Ok(Self::from_config_dir(dir))
    }

    pub fn from_config_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            config_dir: dir.into(),
        }
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub fn config_toml_path(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn mcp_json_path(&self) -> PathBuf {
        self.config_dir.join("mcp_servers.json")
    }

    pub fn read_config_toml(&self) -> Result<String, ConfigError> {
        match std::fs::read_to_string(self.config_toml_path()) {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn theme_preference(&self) -> Result<ThemePreference, ConfigError> {
        let text = self.read_config_toml()?;
        if text.trim().is_empty() {
            return Ok(ThemePreference::Auto);
        }
        let document = text.parse::<toml_edit::DocumentMut>()?;
        let Some(theme) = document.get("theme") else {
            return Ok(ThemePreference::Auto);
        };
        let Some(theme) = theme.as_table() else {
            return Err(ConfigError::Invalid("theme is not a table".into()));
        };
        let Some(mode) = theme.get("mode") else {
            return Ok(ThemePreference::Auto);
        };
        let Some(mode) = mode.as_str() else {
            return Err(ConfigError::Invalid("theme.mode is not a string".into()));
        };
        match mode.to_ascii_lowercase().as_str() {
            "auto" => Ok(ThemePreference::Auto),
            "light" => Ok(ThemePreference::Light),
            "dark" => Ok(ThemePreference::Dark),
            _ => Err(ConfigError::Invalid(format!(
                "invalid theme.mode: {mode:?}"
            ))),
        }
    }

    pub fn fs_access_mode(&self) -> Result<Option<crate::fs_access::FsAccessMode>, ConfigError> {
        let text = self.read_config_toml()?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        let document = text.parse::<toml_edit::DocumentMut>()?;
        let Some(fs_access) = document.get("fs_access") else {
            return Ok(None);
        };
        let Some(fs_access) = fs_access.as_table() else {
            return Err(ConfigError::Invalid("fs_access is not a table".into()));
        };
        let Some(mode) = fs_access.get("mode") else {
            return Ok(None);
        };
        let Some(mode) = mode.as_str() else {
            return Err(ConfigError::Invalid(
                "fs_access.mode is not a string".into(),
            ));
        };
        crate::fs_access::FsAccessMode::from_str(mode)
            .map(Some)
            .map_err(ConfigError::Invalid)
    }

    pub fn upsert_model(&self, update: ModelConfigUpdate<'_>) -> Result<(), ConfigError> {
        self.update_config_toml(|doc| {
            validate_model_name(doc, update.old_name, update.name)?;
            crate::model_registry::apply_model_config_update(doc, update)
                .map_err(|error| ConfigError::Invalid(error.to_string()))
        })
    }

    pub fn upsert_provider(&self, update: ProviderConfigUpdate<'_>) -> Result<(), ConfigError> {
        self.update_config_toml(|doc| {
            if doc.get("providers").is_none() {
                doc.insert("providers", toml_edit::Item::Table(toml_edit::Table::new()));
            }
            let providers = doc
                .get_mut("providers")
                .and_then(toml_edit::Item::as_table_mut)
                .ok_or_else(|| ConfigError::Invalid("providers is not a table".into()))?;
            let mut entry = toml_edit::Table::new();
            entry.insert("kind", toml_edit::value(update.kind));
            insert_nonempty(&mut entry, "api_key", update.api_key);
            insert_nonempty(&mut entry, "api_key_env", update.api_key_env);
            insert_nonempty(&mut entry, "base_url", update.base_url);
            if let Some(value) = update.max_tokens {
                entry.insert("max_tokens", toml_edit::value(i64::from(value)));
            }
            entry.insert("enabled", toml_edit::value(update.enabled));
            providers.insert(update.name, toml_edit::Item::Table(entry));
            Ok(())
        })
    }

    pub fn add_alias(&self, alias: &str, model: &str) -> Result<(), ConfigError> {
        self.update_alias(None, alias, model)
    }

    pub fn bind_default_model(&self, model: &str) -> Result<(), ConfigError> {
        self.update_config_toml(|doc| {
            if table_contains(doc, "models", "smart")? {
                return Err(ConfigError::NameConflict {
                    name: "smart".into(),
                    domain: "models",
                });
            }
            ensure_alias_table(doc)?;
            let aliases = doc
                .get_mut("alias")
                .and_then(toml_edit::Item::as_table_mut)
                .ok_or_else(|| ConfigError::Invalid("alias is not a table".into()))?;
            set_alias_model(aliases, "smart", model);
            if !aliases.contains_key("cheap") {
                set_alias_model(aliases, "cheap", "smart");
            }
            Ok(())
        })
    }

    pub fn update_alias(
        &self,
        old_alias: Option<&str>,
        new_alias: &str,
        model: &str,
    ) -> Result<(), ConfigError> {
        self.update_config_toml(|doc| {
            validate_alias_name(doc, old_alias, new_alias)?;
            if doc.get("alias").is_none() {
                doc.insert("alias", toml_edit::Item::Table(toml_edit::Table::new()));
            }
            let aliases = doc
                .get_mut("alias")
                .and_then(toml_edit::Item::as_table_mut)
                .ok_or_else(|| ConfigError::Invalid("alias is not a table".into()))?;
            if let Some(old) = old_alias.filter(|old| *old != new_alias) {
                aliases.remove(old);
            }
            let mut entry = toml_edit::Table::new();
            entry.insert("model", toml_edit::value(model));
            aliases.insert(new_alias, toml_edit::Item::Table(entry));
            Ok(())
        })
    }

    pub fn remove_alias(&self, alias: &str) -> Result<(), ConfigError> {
        self.update_config_toml(|doc| {
            if let Some(aliases) = doc.get_mut("alias").and_then(toml_edit::Item::as_table_mut) {
                aliases.remove(alias);
            }
            Ok(())
        })
    }

    pub fn reload(&self) -> Result<(), ConfigError> {
        let text = self.read_config_toml()?;
        crate::model_registry::reload_from_text(&text)
            .map_err(|error| ConfigError::Invalid(error.to_string()))
    }

    pub fn load_mcp(&self) -> Vec<crate::mcp::McpServerConfig> {
        crate::mcp_config::load_from_dir(self.config_dir(), true)
    }

    pub fn load_local_mcp(&self) -> Vec<crate::mcp::McpServerConfig> {
        crate::mcp_config::load_from_dir(self.config_dir(), false)
    }

    pub fn save_mcp(&self, configs: &[crate::mcp::McpServerConfig]) -> Result<(), ConfigError> {
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        self.write_mcp(configs)
    }

    pub fn upsert_mcp(&self, config: crate::mcp::McpServerConfig) -> Result<(), ConfigError> {
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let mut configs = self.load_local_mcp();
        configs.retain(|current| current.name != config.name);
        configs.push(config);
        self.write_mcp(&configs)
    }

    pub fn toggle_mcp(&self, name: &str) -> Result<bool, ConfigError> {
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let mut configs = self.load_local_mcp();
        let config = configs
            .iter_mut()
            .find(|config| config.name == name)
            .ok_or_else(|| ConfigError::Invalid(format!("MCP server {name:?} not found")))?;
        config.disabled = !config.disabled;
        let disabled = config.disabled;
        self.write_mcp(&configs)?;
        Ok(disabled)
    }

    pub fn remove_mcp(&self, name: &str) -> Result<(), ConfigError> {
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let mut configs = self.load_local_mcp();
        let before = configs.len();
        configs.retain(|config| config.name != name);
        if configs.len() == before {
            return Err(ConfigError::Invalid(format!(
                "MCP server {name:?} not found"
            )));
        }
        self.write_mcp(&configs)
    }

    pub fn migrate_model_config_if_needed(&self) -> Result<bool, ConfigError> {
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let text = self.read_config_toml()?;
        if !crate::model_registry::needs_migration(&text) {
            return Ok(false);
        }
        let migrated = crate::model_registry::migrate_config(&text)
            .ok_or_else(|| ConfigError::Invalid("migrate config.toml".into()))?;
        let backup = self.config_dir.join("config.toml.bak");
        std::fs::write(backup, text)?;
        self.write_config_toml(&migrated)?;
        crate::model_registry::reload_from_text(&migrated)
            .map_err(|error| ConfigError::Invalid(error.to_string()))?;
        Ok(true)
    }

    fn update_config_toml(
        &self,
        mutate: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<(), ConfigError>,
    ) -> Result<(), ConfigError> {
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let text = self.read_config_toml()?;
        let mut doc = if text.trim().is_empty() {
            toml_edit::DocumentMut::new()
        } else {
            text.parse()?
        };
        mutate(&mut doc)?;
        let new_text = doc.to_string();
        self.write_config_toml(&new_text)?;
        crate::model_registry::reload_from_text(&new_text)
            .map_err(|error| ConfigError::Invalid(error.to_string()))
    }

    fn write_config_toml(&self, text: &str) -> Result<(), ConfigError> {
        self.write_atomic("config.toml", ".config.toml.tmp", text)
    }

    fn write_mcp(&self, configs: &[crate::mcp::McpServerConfig]) -> Result<(), ConfigError> {
        let json = crate::mcp_config::serialize(configs)
            .map_err(|error| ConfigError::Invalid(format!("serialize mcp config: {error}")))?;
        self.write_atomic("mcp_servers.json", ".mcp_servers.json.tmp", &json)
    }

    fn write_atomic(
        &self,
        filename: &str,
        temp_filename: &str,
        text: &str,
    ) -> Result<(), ConfigError> {
        std::fs::create_dir_all(&self.config_dir)?;
        let tmp = self.config_dir.join(temp_filename);
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, self.config_dir.join(filename))?;
        Ok(())
    }
}

fn insert_nonempty(table: &mut toml_edit::Table, key: &str, value: Option<&str>) {
    if let Some(value) = value.filter(|value| !value.is_empty()) {
        table.insert(key, toml_edit::value(value));
    }
}

fn ensure_alias_table(doc: &mut toml_edit::DocumentMut) -> Result<(), ConfigError> {
    if doc.get("alias").is_none() {
        doc.insert("alias", toml_edit::Item::Table(toml_edit::Table::new()));
    }
    if doc
        .get("alias")
        .and_then(toml_edit::Item::as_table)
        .is_none()
    {
        return Err(ConfigError::Invalid("alias is not a table".into()));
    }
    Ok(())
}

fn set_alias_model(table: &mut toml_edit::Table, alias: &str, model: &str) {
    let mut entry = toml_edit::Table::new();
    entry.insert("model", toml_edit::value(model));
    table.insert(alias, toml_edit::Item::Table(entry));
}

fn validate_model_name(
    doc: &toml_edit::DocumentMut,
    old_name: Option<&str>,
    name: &str,
) -> Result<(), ConfigError> {
    if old_name != Some(name) && table_contains(doc, "models", name)? {
        return Err(ConfigError::NameConflict {
            name: name.into(),
            domain: "models",
        });
    }
    if table_contains(doc, "alias", name)? {
        return Err(ConfigError::NameConflict {
            name: name.into(),
            domain: "alias",
        });
    }
    Ok(())
}

fn validate_alias_name(
    doc: &toml_edit::DocumentMut,
    old_name: Option<&str>,
    name: &str,
) -> Result<(), ConfigError> {
    if table_contains(doc, "models", name)? {
        return Err(ConfigError::NameConflict {
            name: name.into(),
            domain: "models",
        });
    }
    if old_name != Some(name) && table_contains(doc, "alias", name)? {
        return Err(ConfigError::NameConflict {
            name: name.into(),
            domain: "alias",
        });
    }
    Ok(())
}

fn table_contains(
    doc: &toml_edit::DocumentMut,
    table: &'static str,
    name: &str,
) -> Result<bool, ConfigError> {
    match doc.get(table) {
        None => Ok(false),
        Some(item) => item
            .as_table()
            .map(|items| items.contains_key(name))
            .ok_or_else(|| ConfigError::Invalid(format!("{table} is not a table"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_hub() -> (tempfile::TempDir, ConfigHub) {
        let dir = tempfile::tempdir().unwrap();
        let hub = ConfigHub::from_config_dir(dir.path());
        (dir, hub)
    }

    fn write_config(hub: &ConfigHub, text: &str) {
        std::fs::write(hub.config_toml_path(), text).unwrap();
    }

    #[test]
    fn theme_preference_defaults_to_auto_when_config_is_missing() {
        let (_dir, hub) = temp_hub();

        assert_eq!(hub.theme_preference().unwrap(), ThemePreference::Auto);
    }

    #[test]
    fn theme_preference_defaults_to_auto_when_mode_is_missing() {
        let (_dir, hub) = temp_hub();
        write_config(&hub, "[theme]\n");

        assert_eq!(hub.theme_preference().unwrap(), ThemePreference::Auto);
    }

    #[test]
    fn theme_preference_parses_supported_modes() {
        for (mode, expected) in [
            ("auto", ThemePreference::Auto),
            ("light", ThemePreference::Light),
            ("LiGhT", ThemePreference::Light),
            ("dark", ThemePreference::Dark),
        ] {
            let (_dir, hub) = temp_hub();
            write_config(&hub, &format!("[theme]\nmode = {mode:?}\n"));

            assert_eq!(hub.theme_preference().unwrap(), expected);
        }
    }

    #[test]
    fn theme_preference_rejects_unknown_mode() {
        let (_dir, hub) = temp_hub();
        write_config(&hub, "[theme]\nmode = \"sepia\"\n");

        assert!(matches!(
            hub.theme_preference(),
            Err(ConfigError::Invalid(message)) if message.contains("theme.mode")
        ));
    }

    #[test]
    fn fs_access_mode_defaults_to_none_when_config_is_missing() {
        let (_dir, hub) = temp_hub();

        assert_eq!(hub.fs_access_mode().unwrap(), None);
    }

    #[test]
    fn fs_access_mode_defaults_to_none_when_section_or_mode_is_missing() {
        for text in ["[theme]\nmode = \"dark\"\n", "[fs_access]\n"] {
            let (_dir, hub) = temp_hub();
            write_config(&hub, text);

            assert_eq!(hub.fs_access_mode().unwrap(), None);
        }
    }

    #[test]
    fn fs_access_mode_parses_canonical_and_alias_values() {
        for (mode, expected) in [
            ("read-only", crate::fs_access::FsAccessMode::ReadOnly),
            ("ws", crate::fs_access::FsAccessMode::WorkspaceWrite),
            (
                "danger-full-access",
                crate::fs_access::FsAccessMode::DangerFullAccess,
            ),
        ] {
            let (_dir, hub) = temp_hub();
            write_config(&hub, &format!("[fs_access]\nmode = {mode:?}\n"));

            assert_eq!(hub.fs_access_mode().unwrap(), Some(expected));
        }
    }

    #[test]
    fn fs_access_mode_rejects_unknown_mode() {
        let (_dir, hub) = temp_hub();
        write_config(&hub, "[fs_access]\nmode = \"chaos\"\n");

        assert!(matches!(
            hub.fs_access_mode(),
            Err(ConfigError::Invalid(message)) if message.contains("unknown fs access mode")
        ));
    }

    #[test]
    fn fs_access_mode_rejects_non_string_mode() {
        let (_dir, hub) = temp_hub();
        write_config(&hub, "[fs_access]\nmode = true\n");

        assert!(matches!(
            hub.fs_access_mode(),
            Err(ConfigError::Invalid(message)) if message.contains("fs_access.mode")
        ));
    }

    fn model<'a>(
        old_name: Option<&'a str>,
        name: &'a str,
        model: &'a str,
    ) -> ModelConfigUpdate<'a> {
        ModelConfigUpdate {
            old_name,
            name,
            model,
            provider: Some("test"),
            context_budget: 100_000,
            thinking: false,
            max_tokens: None,
            enabled: true,
        }
    }

    #[test]
    fn duplicate_model_name_is_rejected_without_writing() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK.lock().unwrap();
        let (_dir, hub) = temp_hub();
        hub.upsert_model(model(None, "shared", "provider/a"))
            .unwrap();
        let before = hub.read_config_toml().unwrap();

        let error = hub
            .upsert_model(model(None, "shared", "provider/b"))
            .unwrap_err();

        assert!(matches!(
            error,
            ConfigError::NameConflict {
                domain: "models",
                ..
            }
        ));
        assert_eq!(hub.read_config_toml().unwrap(), before);
    }

    #[test]
    fn model_rename_conflict_is_rejected_without_removing_source() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK.lock().unwrap();
        let (_dir, hub) = temp_hub();
        hub.upsert_model(model(None, "first", "provider/a"))
            .unwrap();
        hub.upsert_model(model(None, "second", "provider/b"))
            .unwrap();
        let before = hub.read_config_toml().unwrap();

        let error = hub
            .upsert_model(model(Some("first"), "second", "provider/a"))
            .unwrap_err();

        assert!(matches!(error, ConfigError::NameConflict { .. }));
        assert_eq!(hub.read_config_toml().unwrap(), before);
    }

    #[test]
    fn model_and_alias_share_a_namespace() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK.lock().unwrap();
        let (_dir, hub) = temp_hub();
        hub.upsert_model(model(None, "smart", "provider/a"))
            .unwrap();
        assert!(matches!(
            hub.add_alias("smart", "provider/a"),
            Err(ConfigError::NameConflict {
                domain: "models",
                ..
            })
        ));

        hub.add_alias("cheap", "provider/a").unwrap();
        assert!(matches!(
            hub.upsert_model(model(None, "cheap", "provider/b")),
            Err(ConfigError::NameConflict {
                domain: "alias",
                ..
            })
        ));
    }

    #[test]
    fn alias_rename_conflict_is_rejected_without_removing_source() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK.lock().unwrap();
        let (_dir, hub) = temp_hub();
        hub.add_alias("first", "provider/a").unwrap();
        hub.add_alias("second", "provider/b").unwrap();
        let before = hub.read_config_toml().unwrap();

        let error = hub
            .update_alias(Some("first"), "second", "provider/a")
            .unwrap_err();

        assert!(matches!(
            error,
            ConfigError::NameConflict {
                domain: "alias",
                ..
            }
        ));
        assert_eq!(hub.read_config_toml().unwrap(), before);
    }

    #[test]
    fn bind_default_model_rebinds_smart_atomically_and_keeps_cheap() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK.lock().unwrap();
        let (_dir, hub) = temp_hub();
        hub.add_alias("smart", "provider/old").unwrap();

        hub.bind_default_model("provider/new").unwrap();

        let text = hub.read_config_toml().unwrap();
        assert!(text.contains("[alias.smart]"));
        assert!(text.contains("model = \"provider/new\""));
        assert!(text.contains("[alias.cheap]"));
        assert!(text.contains("model = \"smart\""));
    }

    #[test]
    fn bind_default_model_preserves_existing_cheap_alias() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK.lock().unwrap();
        let (_dir, hub) = temp_hub();
        hub.add_alias("smart", "provider/old").unwrap();
        hub.add_alias("cheap", "provider/custom-cheap").unwrap();

        hub.bind_default_model("provider/new").unwrap();

        let cfg = crate::model_registry::parse_config(&hub.read_config_toml().unwrap()).unwrap();
        assert_eq!(cfg.aliases["smart"].model, "provider/new");
        assert_eq!(cfg.aliases["cheap"].model, "provider/custom-cheap");
    }

    #[test]
    fn bind_default_model_does_not_overwrite_smart_model() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK.lock().unwrap();
        let (_dir, hub) = temp_hub();
        hub.upsert_model(model(None, "smart", "provider/model"))
            .unwrap();
        let before = hub.read_config_toml().unwrap();

        assert!(matches!(
            hub.bind_default_model("provider/new"),
            Err(ConfigError::NameConflict {
                domain: "models",
                ..
            })
        ));
        assert_eq!(hub.read_config_toml().unwrap(), before);
    }

    #[test]
    fn distinct_names_may_use_the_same_provider_model_id() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK.lock().unwrap();
        let (_dir, hub) = temp_hub();
        hub.upsert_model(model(None, "first", "provider/shared"))
            .unwrap();
        hub.upsert_model(model(None, "second", "provider/shared"))
            .unwrap();

        let text = hub.read_config_toml().unwrap();
        assert!(text.contains("[models.first]"));
        assert!(text.contains("[models.second]"));
    }

    #[test]
    fn mcp_upsert_preserves_existing_json_servers_and_overrides_toml_by_name() {
        let (_dir, hub) = temp_hub();
        std::fs::write(
            hub.config_toml_path(),
            "[[mcp]]\nname = \"shared\"\ncommand = \"from-toml\"\n",
        )
        .unwrap();
        hub.save_mcp(&[crate::mcp::McpServerConfig::stdio(
            "existing",
            "existing-command",
            vec![],
            crate::tool::Tier::Two,
            30_000,
        )])
        .unwrap();

        hub.upsert_mcp(crate::mcp::McpServerConfig::stdio(
            "shared",
            "from-json",
            vec![],
            crate::tool::Tier::Three,
            30_000,
        ))
        .unwrap();

        let configs = hub.load_local_mcp();
        assert_eq!(configs.len(), 2);
        assert_eq!(
            configs
                .iter()
                .find(|cfg| cfg.name == "shared")
                .unwrap()
                .command,
            "from-json"
        );
        assert!(configs.iter().any(|cfg| cfg.name == "existing"));
        assert!(!hub.config_dir().join(".mcp_servers.json.tmp").exists());
    }

    #[test]
    fn mcp_toggle_toml_server_persists_json_override() {
        let (_dir, hub) = temp_hub();
        std::fs::write(
            hub.config_toml_path(),
            "[[mcp]]\nname = \"exa\"\ncommand = \"exa-mcp-server\"\n",
        )
        .unwrap();

        assert!(hub.toggle_mcp("exa").unwrap());

        let configs = hub.load_local_mcp();
        assert!(
            configs
                .iter()
                .find(|cfg| cfg.name == "exa")
                .unwrap()
                .disabled
        );
        assert!(hub.mcp_json_path().exists());
    }

    #[test]
    fn mcp_remove_updates_json_atomically() {
        let (_dir, hub) = temp_hub();
        hub.save_mcp(&[
            crate::mcp::McpServerConfig::stdio(
                "first",
                "echo",
                vec![],
                crate::tool::Tier::Two,
                30_000,
            ),
            crate::mcp::McpServerConfig::stdio(
                "second",
                "ls",
                vec![],
                crate::tool::Tier::Two,
                30_000,
            ),
        ])
        .unwrap();

        hub.remove_mcp("first").unwrap();

        let configs = hub.load_local_mcp();
        assert_eq!(configs.len(), 1);
        assert_eq!(configs[0].name, "second");
        assert!(!hub.config_dir().join(".mcp_servers.json.tmp").exists());
    }

    #[test]
    fn alias_updates_preserve_comments_and_other_sections() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK.lock().unwrap();
        let (_dir, hub) = temp_hub();
        std::fs::write(
            hub.config_toml_path(),
            "# keep me\n[theme]\nname = \"dark\"\n\n[alias.old]\nmodel = \"provider/a\"\n",
        )
        .unwrap();

        hub.update_alias(Some("old"), "new", "provider/b").unwrap();

        let text = hub.read_config_toml().unwrap();
        assert!(text.contains("# keep me"));
        assert!(text.contains("[theme]"));
        assert!(text.contains("[alias.new]"));
        assert!(!text.contains("[alias.old]"));
    }
}
