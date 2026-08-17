use std::fmt;
use std::path::{Path, PathBuf};
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
    config_toml: PathBuf,
}

impl ConfigHub {
    pub fn global() -> Result<Self, ConfigError> {
        let dir = crate::storage::config_dir()
            .map_err(|error| ConfigError::Invalid(format!("config dir: {error}")))?;
        Ok(Self::from_config_dir(dir))
    }

    pub fn from_config_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            config_toml: dir.into().join("config.toml"),
        }
    }

    pub fn config_toml_path(&self) -> &Path {
        &self.config_toml
    }

    pub fn read_config_toml(&self) -> Result<String, ConfigError> {
        match std::fs::read_to_string(&self.config_toml) {
            Ok(text) => Ok(text),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error.into()),
        }
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

    pub fn migrate_model_config_if_needed(&self) -> Result<bool, ConfigError> {
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let text = self.read_config_toml()?;
        if !crate::model_registry::needs_migration(&text) {
            return Ok(false);
        }
        let migrated = crate::model_registry::migrate_config(&text)
            .ok_or_else(|| ConfigError::Invalid("migrate config.toml".into()))?;
        let backup = self.config_toml.with_file_name("config.toml.bak");
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
        let dir = self
            .config_toml
            .parent()
            .ok_or_else(|| ConfigError::Invalid("config.toml has no parent directory".into()))?;
        std::fs::create_dir_all(dir)?;
        let tmp = dir.join(".config.toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, &self.config_toml)?;
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
