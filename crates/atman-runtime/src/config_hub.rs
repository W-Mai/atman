use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::model_registry::ModelConfigUpdate;

static CONFIG_WRITE_LOCK: Mutex<()> = Mutex::new(());
static AUTH_WRITE_LOCK: Mutex<()> = Mutex::new(());
static ROUTES_WRITE_LOCK: Mutex<()> = Mutex::new(());
static LAYOUT_MIGRATION_LOCK: Mutex<()> = Mutex::new(());

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DaemonConfig {
    pub auth_token: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThemePreference {
    Auto,
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum DiffLayout {
    #[default]
    Split,
    Unified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterjectionMode {
    Off,
    Rule,
    Llm,
    Unknown(String),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RedactConfig {
    pub enabled: bool,
    pub partial: bool,
    pub allowlist: Vec<String>,
    pub custom_patterns: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxConfig {
    pub enabled: bool,
    pub strict: bool,
    pub extra_read: Vec<PathBuf>,
    pub extra_write: Vec<PathBuf>,
    pub template_path: Option<PathBuf>,
    pub allow_network: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            strict: false,
            extra_read: Vec::new(),
            extra_write: Vec::new(),
            template_path: None,
            allow_network: false,
        }
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
    pub reasoning_format: Option<crate::providers::openai::OpenAiReasoningFormat>,
    pub prompt_cache_key: Option<bool>,
    pub enabled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProviderConfigWriteMode {
    Upsert,
    Create,
    Update,
}

pub struct AuthTokenUpdate {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: i64,
    pub account: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthModelCacheCommit {
    Updated,
    Missing,
    Disabled,
    Changed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthProviderInsertCommit {
    Inserted,
    Changed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AuthProviderRuntimeCommit {
    Applied { auth_changed: bool },
    Missing,
    Disabled,
    Changed,
}

#[derive(Debug, Clone)]
pub(crate) struct AuthProviderRuntimeState {
    pub provider: crate::auth_store::StoredProvider,
    pub catalog_snapshot: crate::auth_store::AuthProviderCatalogSnapshot,
    pub model_namespace: Option<String>,
    pub model_cache: Option<Vec<crate::provider::DiscoveredModelDetails>>,
    pub model_cache_freshness: crate::auth_store::ModelCacheFreshness,
    pub provider_ids: Vec<String>,
}

pub(crate) struct AuthModelCacheUpdate<'a> {
    pub expected: &'a crate::auth_store::StoredProvider,
    pub expected_catalog: &'a crate::auth_store::AuthProviderCatalogSnapshot,
    pub expected_provider_ids: Option<&'a [String]>,
    pub model_namespace: &'a str,
    pub fetched_at: i64,
    pub models: &'a [crate::provider::DiscoveredModelDetails],
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AuthProviderRuntimeDescriptor {
    pub id: String,
    pub kind: crate::auth_store::ProviderKind,
    pub enabled: bool,
}

#[derive(Debug, Clone)]
pub struct ConfigHub {
    config_dir: PathBuf,
    daemon_config_path: Option<PathBuf>,
    auth_path: PathBuf,
}

impl ConfigHub {
    pub fn global() -> Result<Self, ConfigError> {
        let dir = crate::storage::config_dir()
            .map_err(|error| ConfigError::Invalid(format!("config dir: {error}")))?;
        Ok(Self::from_config_dir(dir))
    }

    pub fn from_config_dir(dir: impl Into<PathBuf>) -> Self {
        let config_dir = dir.into();
        let auth_path = config_dir.join("auth.json");
        Self {
            config_dir,
            daemon_config_path: None,
            auth_path,
        }
    }

    pub fn config_dir(&self) -> &Path {
        &self.config_dir
    }

    pub(crate) fn auth_path(&self) -> &Path {
        &self.auth_path
    }

    pub fn config_toml_path(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn validate_setting_mutation(&self, key: &str, value: &str) -> Result<(), ConfigError> {
        crate::settings_catalog::validate_mutation(key, value)
            .map_err(|error| ConfigError::Invalid(error.to_string()))
    }

    pub fn routes_at_path(&self) -> PathBuf {
        self.config_dir.join("routes.at")
    }

    pub fn migrate_legacy_layout(
        &self,
        legacy_data_dir: &Path,
    ) -> Result<Option<crate::config_migration::MigrationReport>, ConfigError> {
        use fs2::FileExt;
        let _guard = LAYOUT_MIGRATION_LOCK.lock().unwrap();
        if legacy_data_dir == self.config_dir || !legacy_data_dir.exists() {
            return Ok(None);
        }
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(legacy_data_dir.join(".config-migration.lock"))?;
        lock.lock_exclusive()?;
        let _config_file_lock = lock_file(&self.config_dir.join(".config.toml.lock"))?;
        let daemon_lock_path = self
            .daemon_config_path
            .as_deref()
            .map(lock_path_for)
            .unwrap_or_else(|| self.config_dir.join(".daemon.toml.lock"));
        let _daemon_file_lock = lock_file(&daemon_lock_path)?;
        let _routes_file_lock = lock_file(&self.config_dir.join(".routes.at.lock"))?;
        crate::config_migration::relocate_legacy_layout(
            &self.config_dir,
            self.daemon_config_path.as_deref(),
            legacy_data_dir,
        )
        .map_err(|error| ConfigError::Invalid(error.to_string()))
    }

    pub fn storage_config(&self, project_root: Option<&Path>) -> crate::storage::StorageConfig {
        let global =
            crate::storage::StorageConfig::load_from(&self.config_toml_path()).unwrap_or_default();
        let project = project_root
            .map(|root| {
                crate::storage::StorageConfig::load_from(&root.join(".atman/config.toml"))
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        crate::storage::StorageConfig::merge(global, project)
    }

    pub fn set_project_storage_scope(
        &self,
        project_root: &Path,
        scope: crate::storage::StorageScope,
    ) -> Result<(), ConfigError> {
        let path = project_root.join(".atman/config.toml");
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let _file_lock = lock_file(&lock_path_for(&path))?;
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        let mut doc = if text.trim().is_empty() {
            toml_edit::DocumentMut::new()
        } else {
            text.parse()?
        };
        if doc.get("storage").is_none() {
            doc.insert("storage", toml_edit::Item::Table(toml_edit::Table::new()));
        }
        let storage = doc
            .get_mut("storage")
            .and_then(toml_edit::Item::as_table_mut)
            .ok_or_else(|| ConfigError::Invalid("storage is not a table".into()))?;
        let value = match scope {
            crate::storage::StorageScope::Global => "global",
            crate::storage::StorageScope::Local => "local",
        };
        storage.insert("scope", toml_edit::value(value));
        write_unique_atomic(&path, doc.to_string().as_bytes())
    }

    pub fn load_routes_source(&self) -> Result<Option<String>, ConfigError> {
        match std::fs::read_to_string(self.routes_at_path()) {
            Ok(source) => Ok(Some(source)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(ConfigError::Io(error)),
        }
    }

    pub fn append_dsl_route(&self, flow_name: &str, trigger: &str) -> Result<(), ConfigError> {
        use fs2::FileExt;

        let route = dsl_route_source(flow_name, trigger)?;
        let _guard = ROUTES_WRITE_LOCK.lock().unwrap();
        std::fs::create_dir_all(&self.config_dir)?;
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.config_dir.join(".routes.at.lock"))?;
        lock.lock_exclusive()?;

        let path = self.routes_at_path();
        let source = match std::fs::read_to_string(&path) {
            Ok(source) => source,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        parse_routes_source("existing routes.at", &source)?;

        let mut combined = source;
        if !combined.is_empty() && !combined.ends_with('\n') {
            combined.push('\n');
        }
        combined.push_str(&route);
        parse_routes_source("updated routes.at", &combined)?;
        write_unique_atomic(&path, combined.as_bytes())
    }

    pub fn mcp_json_path(&self) -> PathBuf {
        self.config_dir.join("mcp_servers.json")
    }

    pub fn from_daemon_config_path(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let config_dir = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Self::from_config_dir(config_dir).with_daemon_config_path(path)
    }

    pub fn with_daemon_config_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.daemon_config_path = Some(path.into());
        self
    }

    pub fn from_auth_path(path: impl Into<PathBuf>) -> Self {
        let auth_path = path.into();
        let config_dir = auth_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        Self {
            config_dir,
            daemon_config_path: None,
            auth_path,
        }
    }

    pub fn load_auth(&self) -> Result<crate::auth_store::AuthStore, ConfigError> {
        load_auth_from_path(&self.auth_path)
    }

    pub fn load_auth_model_cache_details(
        &self,
        id: &str,
    ) -> Result<Option<Vec<crate::provider::DiscoveredModelDetails>>, ConfigError> {
        Ok(load_auth_document_from_path(&self.auth_path)?.model_cache_details(id))
    }

    #[cfg(test)]
    pub(crate) fn load_or_create_auth_provider_catalog_state(
        &self,
        id: &str,
    ) -> Result<
        Option<(
            crate::auth_store::StoredProvider,
            crate::auth_store::AuthProviderCatalogSnapshot,
        )>,
        ConfigError,
    > {
        self.update_auth_document_conditionally(|document| {
            let Some((state, changed)) = document.ensure_provider_catalog_state(id) else {
                return Ok((None, false));
            };
            Ok((Some(state), changed))
        })
    }

    pub(crate) fn load_or_create_auth_provider_runtime_state(
        &self,
        id: &str,
    ) -> Result<Option<AuthProviderRuntimeState>, ConfigError> {
        self.load_or_create_auth_provider_runtime_state_at(id, chrono::Utc::now().timestamp())
    }

    pub(crate) fn load_or_create_auth_provider_runtime_state_at(
        &self,
        id: &str,
        now: i64,
    ) -> Result<Option<AuthProviderRuntimeState>, ConfigError> {
        self.update_auth_document_conditionally(|document| {
            let Some(((provider, catalog_snapshot), changed)) =
                document.ensure_provider_catalog_state(id)
            else {
                return Ok((None, false));
            };
            let model_namespace = document.model_namespace(id);
            let model_cache = document.model_cache_details(id);
            let model_cache_freshness = document
                .model_cache_freshness(
                    id,
                    now,
                    crate::auth_store::MODEL_CACHE_FRESHNESS_WINDOW_SECONDS,
                )
                .ok_or_else(|| {
                    ConfigError::Invalid(format!(
                        "auth provider `{id}` disappeared while reading its model cache"
                    ))
                })?;
            let provider_ids = sorted_auth_provider_ids(&document.legacy_view());
            Ok((
                Some(AuthProviderRuntimeState {
                    provider,
                    catalog_snapshot,
                    model_namespace,
                    model_cache,
                    model_cache_freshness,
                    provider_ids,
                }),
                changed,
            ))
        })
    }

    pub(crate) fn load_or_create_auth_provider_credential_state(
        &self,
        id: &str,
    ) -> Result<
        Option<(
            crate::auth_store::StoredProvider,
            crate::auth_store::AuthProviderCredentialSnapshot,
        )>,
        ConfigError,
    > {
        match load_auth_document_from_path(&self.auth_path)?.provider_credential_state(id) {
            None => return Ok(None),
            Some((provider, Some(snapshot))) => return Ok(Some((provider, snapshot))),
            Some((_provider, None)) => {}
        }
        self.update_auth_document_conditionally(|document| {
            let Some((state, changed)) = document.ensure_provider_credential_state(id) else {
                return Ok((None, false));
            };
            Ok((Some(state), changed))
        })
    }

    pub fn load_auth_model_namespace(&self, id: &str) -> Result<Option<String>, ConfigError> {
        Ok(load_auth_document_from_path(&self.auth_path)?.model_namespace(id))
    }

    pub fn ensure_auth_model_namespace(
        &self,
        id: &str,
        model_namespace: &str,
    ) -> Result<(), ConfigError> {
        if let Some(existing) = self.load_auth_model_namespace(id)? {
            if existing == model_namespace {
                return Ok(());
            }
            return Err(ConfigError::Invalid(format!(
                "provider `{id}` model namespace is already `{existing}`"
            )));
        }
        self.update_auth_document(|document| {
            document
                .ensure_model_namespace(id, model_namespace)
                .map(|_| ())
                .map_err(ConfigError::Invalid)
        })
    }

    pub fn update_auth<T>(
        &self,
        mutate: impl FnOnce(&mut crate::auth_store::AuthStore) -> Result<T, ConfigError>,
    ) -> Result<T, ConfigError> {
        self.update_auth_document(|document| {
            let mut store = document.legacy_view();
            let result = mutate(&mut store)?;
            document.merge_legacy_view(store);
            Ok(result)
        })
    }

    fn update_auth_document<T>(
        &self,
        mutate: impl FnOnce(&mut crate::auth_store::AuthStoreDocument) -> Result<T, ConfigError>,
    ) -> Result<T, ConfigError> {
        self.update_auth_document_conditionally(|document| {
            mutate(document).map(|result| (result, true))
        })
    }

    fn update_auth_document_conditionally<T>(
        &self,
        mutate: impl FnOnce(&mut crate::auth_store::AuthStoreDocument) -> Result<(T, bool), ConfigError>,
    ) -> Result<T, ConfigError> {
        self.update_auth_document_conditionally_and_then(mutate, |_| ())
            .map(|(result, ())| result)
    }

    fn update_auth_document_conditionally_and_then<T, U>(
        &self,
        mutate: impl FnOnce(&mut crate::auth_store::AuthStoreDocument) -> Result<(T, bool), ConfigError>,
        after_write: impl FnOnce(&T) -> U,
    ) -> Result<(T, U), ConfigError> {
        use fs2::FileExt;

        let _guard = AUTH_WRITE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let parent = self.auth_path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let lock_path = parent.join(".auth.json.lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;
        set_sensitive_file_permissions(
            &self
                .auth_path
                .parent()
                .unwrap_or_else(|| Path::new("."))
                .join(".auth.json.lock"),
        )?;
        lock.lock_exclusive()?;
        let mut document = load_auth_document_from_path(&self.auth_path)?;
        let (result, changed) = mutate(&mut document)?;
        if changed {
            self.write_auth_document(&document)?;
        }
        let follow_up = after_write(&result);
        Ok((result, follow_up))
    }

    pub fn add_auth_provider(
        &self,
        provider: crate::auth_store::StoredProvider,
    ) -> Result<(), ConfigError> {
        self.update_auth(|store| {
            if store
                .providers
                .iter()
                .any(|existing| existing.id == provider.id)
            {
                return Err(ConfigError::Invalid(format!(
                    "auth provider id {:?} already exists",
                    provider.id
                )));
            }
            store.providers.push(provider);
            Ok(())
        })
    }

    #[cfg(test)]
    pub(crate) fn add_auth_provider_with_model_cache_details(
        &self,
        provider: crate::auth_store::StoredProvider,
        model_namespace: &str,
        fetched_at: i64,
        models: &[crate::provider::DiscoveredModelDetails],
    ) -> Result<(), ConfigError> {
        let (commit, result) = self
            .add_auth_provider_with_model_cache_details_if_provider_ids_and_then(
                provider,
                None,
                model_namespace,
                fetched_at,
                models,
                || (),
            )?;
        match (commit, result) {
            (AuthProviderInsertCommit::Inserted, Some(())) => Ok(()),
            (AuthProviderInsertCommit::Changed, _) => {
                unreachable!("provider ID snapshot was not supplied")
            }
            (AuthProviderInsertCommit::Inserted, None) => {
                unreachable!("provider insertion callback was not run")
            }
        }
    }

    pub(crate) fn add_auth_provider_with_model_cache_details_if_provider_ids_and_then<T>(
        &self,
        provider: crate::auth_store::StoredProvider,
        expected_provider_ids: Option<&[String]>,
        model_namespace: &str,
        fetched_at: i64,
        models: &[crate::provider::DiscoveredModelDetails],
        after_write: impl FnOnce() -> T,
    ) -> Result<(AuthProviderInsertCommit, Option<T>), ConfigError> {
        self.update_auth_document_conditionally_and_then(
            |document| {
                let provider_id = provider.id.clone();
                let mut store = document.legacy_view();
                if expected_provider_ids
                    .is_some_and(|expected| sorted_auth_provider_ids(&store) != expected)
                {
                    return Ok((AuthProviderInsertCommit::Changed, false));
                }
                if store
                    .providers
                    .iter()
                    .any(|existing| existing.id == provider_id)
                {
                    return Err(ConfigError::Invalid(format!(
                        "auth provider id {provider_id:?} already exists"
                    )));
                }
                store.providers.push(provider);
                document.merge_legacy_view(store);
                let updated = document
                    .update_model_cache_details(&provider_id, model_namespace, fetched_at, models)
                    .map_err(ConfigError::Invalid)?;
                if !updated {
                    return Err(ConfigError::Invalid(format!(
                        "auth provider `{provider_id}` disappeared during insertion"
                    )));
                }
                Ok((AuthProviderInsertCommit::Inserted, true))
            },
            |commit| (*commit == AuthProviderInsertCommit::Inserted).then(after_write),
        )
    }

    pub fn remove_auth_provider(&self, id: &str) -> Result<bool, ConfigError> {
        self.update_auth(|store| Ok(store.remove(id)))
    }

    pub fn set_auth_provider_enabled(&self, id: &str, enabled: bool) -> Result<bool, ConfigError> {
        Ok(self
            .set_auth_provider_enabled_with_change(id, enabled)?
            .is_some())
    }

    pub(crate) fn set_auth_provider_enabled_with_change(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<Option<bool>, ConfigError> {
        self.update_auth_document_conditionally(|document| {
            let Some(changed) = document.set_provider_enabled(id, enabled) else {
                return Ok((None, false));
            };
            Ok((Some(changed), changed))
        })
    }

    pub fn update_auth_tokens(
        &self,
        id: &str,
        update: AuthTokenUpdate,
    ) -> Result<bool, ConfigError> {
        self.update_auth(|store| {
            let Some(provider) = store
                .providers
                .iter_mut()
                .find(|provider| provider.id == id)
            else {
                return Ok(false);
            };
            provider.access_token = update.access_token;
            provider.expires_at = update.expires_at;
            if update.refresh_token.is_some() {
                provider.refresh_token = update.refresh_token;
            }
            if update.account.is_some() {
                provider.account = update.account;
            }
            Ok(true)
        })
    }

    pub(crate) fn update_auth_tokens_if_current(
        &self,
        id: &str,
        expected: &crate::auth_store::AuthProviderCredentialSnapshot,
        update: AuthTokenUpdate,
    ) -> Result<crate::auth_store::AuthCredentialCommit, ConfigError> {
        self.update_auth_document_conditionally(|document| {
            let commit = document.update_provider_credentials(
                id,
                expected,
                update.access_token,
                update.refresh_token,
                update.expires_at,
                update.account,
            );
            let changed = matches!(
                commit,
                crate::auth_store::AuthCredentialCommit::Updated { .. }
            );
            Ok((commit, changed))
        })
    }

    pub fn update_auth_model_cache(
        &self,
        id: &str,
        cache: crate::auth_store::ModelCache,
    ) -> Result<bool, ConfigError> {
        self.update_auth_document_conditionally(|document| {
            let updated = document.update_model_cache(id, cache);
            Ok((updated, updated))
        })
    }

    pub(crate) fn update_auth_model_cache_details(
        &self,
        id: &str,
        model_namespace: &str,
        fetched_at: i64,
        models: &[crate::provider::DiscoveredModelDetails],
    ) -> Result<bool, ConfigError> {
        self.update_auth_document(|document| {
            document
                .update_model_cache_details(id, model_namespace, fetched_at, models)
                .map_err(ConfigError::Invalid)
        })
    }

    #[cfg(test)]
    pub(crate) fn update_auth_model_cache_details_if_enabled(
        &self,
        expected: &crate::auth_store::StoredProvider,
        expected_catalog: &crate::auth_store::AuthProviderCatalogSnapshot,
        model_namespace: &str,
        fetched_at: i64,
        models: &[crate::provider::DiscoveredModelDetails],
    ) -> Result<AuthModelCacheCommit, ConfigError> {
        self.update_auth_model_cache_details_if_enabled_and_then(
            AuthModelCacheUpdate {
                expected,
                expected_catalog,
                expected_provider_ids: None,
                model_namespace,
                fetched_at,
                models,
            },
            || (),
        )
        .map(|(commit, _)| commit)
    }

    pub(crate) fn update_auth_model_cache_details_if_enabled_and_then<T>(
        &self,
        update: AuthModelCacheUpdate<'_>,
        after_update: impl FnOnce() -> T,
    ) -> Result<(AuthModelCacheCommit, Option<T>), ConfigError> {
        let AuthModelCacheUpdate {
            expected,
            expected_catalog,
            expected_provider_ids,
            model_namespace,
            fetched_at,
            models,
        } = update;
        self.update_auth_document_conditionally_and_then(
            |document| {
                let store = document.legacy_view();
                let Some(provider) = store
                    .providers
                    .iter()
                    .find(|provider| provider.id == expected.id)
                else {
                    return Ok((AuthModelCacheCommit::Missing, false));
                };
                if !provider.enabled {
                    return Ok((AuthModelCacheCommit::Disabled, false));
                }
                if !auth_provider_matches(provider, expected)
                    || document.provider_catalog_snapshot(&expected.id).as_ref()
                        != Some(expected_catalog)
                {
                    return Ok((AuthModelCacheCommit::Changed, false));
                }
                if expected_provider_ids
                    .is_some_and(|expected| sorted_auth_provider_ids(&store) != expected)
                {
                    return Ok((AuthModelCacheCommit::Changed, false));
                }
                let updated = document
                    .update_model_cache_details(&expected.id, model_namespace, fetched_at, models)
                    .map_err(ConfigError::Invalid)?;
                if !updated {
                    return Ok((AuthModelCacheCommit::Missing, false));
                }
                Ok((AuthModelCacheCommit::Updated, true))
            },
            |commit| (*commit == AuthModelCacheCommit::Updated).then(after_update),
        )
    }

    pub(crate) fn commit_auth_provider_runtime_if_current_and_then<T>(
        &self,
        expected: &crate::auth_store::StoredProvider,
        expected_catalog: &crate::auth_store::AuthProviderCatalogSnapshot,
        enable_if_disabled: bool,
        model_namespace: Option<&str>,
        expected_provider_ids: Option<&[String]>,
        after_commit: impl FnOnce() -> T,
    ) -> Result<(AuthProviderRuntimeCommit, Option<T>), ConfigError> {
        self.update_auth_document_conditionally_and_then(
            |document| {
                let store = document.legacy_view();
                let Some(provider) = store
                    .providers
                    .iter()
                    .find(|provider| provider.id == expected.id)
                else {
                    return Ok((AuthProviderRuntimeCommit::Missing, false));
                };
                if !auth_provider_matches(provider, expected)
                    || document.provider_catalog_snapshot(&expected.id).as_ref()
                        != Some(expected_catalog)
                {
                    return Ok((AuthProviderRuntimeCommit::Changed, false));
                }
                if !provider.enabled && !enable_if_disabled {
                    return Ok((AuthProviderRuntimeCommit::Disabled, false));
                }
                if let Some(expected_provider_ids) = expected_provider_ids {
                    let provider_ids = sorted_auth_provider_ids(&store);
                    if provider_ids != expected_provider_ids {
                        return Ok((AuthProviderRuntimeCommit::Changed, false));
                    }
                }

                let enabled_changed = if enable_if_disabled {
                    document
                        .set_provider_enabled(&expected.id, true)
                        .ok_or_else(|| {
                            ConfigError::Invalid(format!(
                                "auth provider `{}` disappeared during activation",
                                expected.id
                            ))
                        })?
                } else {
                    false
                };
                let namespace_changed = match model_namespace {
                    Some(namespace) => document
                        .ensure_model_namespace(&expected.id, namespace)
                        .map_err(ConfigError::Invalid)?,
                    None => false,
                };
                Ok((
                    AuthProviderRuntimeCommit::Applied {
                        auth_changed: enabled_changed || namespace_changed,
                    },
                    enabled_changed || namespace_changed,
                ))
            },
            |commit| matches!(commit, AuthProviderRuntimeCommit::Applied { .. }).then(after_commit),
        )
    }

    pub(crate) fn with_auth_provider_runtime_descriptors<T>(
        &self,
        inspect: impl FnOnce(&[AuthProviderRuntimeDescriptor]) -> T,
    ) -> Result<T, ConfigError> {
        self.update_auth_document_conditionally_and_then(
            |document| {
                let providers = document
                    .legacy_view()
                    .providers
                    .into_iter()
                    .map(|provider| AuthProviderRuntimeDescriptor {
                        id: provider.id,
                        kind: provider.kind,
                        enabled: provider.enabled,
                    })
                    .collect::<Vec<_>>();
                Ok((providers, false))
            },
            |providers| inspect(providers),
        )
        .map(|(_, result)| result)
    }

    pub fn load_or_init_daemon_config(&self) -> Result<DaemonConfig, ConfigError> {
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let path = self
            .daemon_config_path
            .as_deref()
            .ok_or_else(|| ConfigError::Invalid("daemon config path is not configured".into()))?;
        let _file_lock = lock_file(&lock_path_for(path))?;
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|error| {
                ConfigError::Invalid(format!("parse {}: {error}", path.display()))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let config = DaemonConfig {
                    auth_token: generate_daemon_token(),
                };
                self.write_daemon_config(&config)?;
                Ok(config)
            }
            Err(error) => Err(error.into()),
        }
    }

    pub fn rotate_daemon_config(&self) -> Result<DaemonConfig, ConfigError> {
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let path = self
            .daemon_config_path
            .as_deref()
            .ok_or_else(|| ConfigError::Invalid("daemon config path is not configured".into()))?;
        let _file_lock = lock_file(&lock_path_for(path))?;
        if !path.exists() {
            return Err(ConfigError::Invalid(format!(
                "no daemon config at {} — nothing to rotate. Run `atman daemon start` once to generate one.",
                path.display()
            )));
        }
        let config = DaemonConfig {
            auth_token: generate_daemon_token(),
        };
        self.write_daemon_config(&config)?;
        Ok(config)
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

    pub fn diff_layout(&self) -> Result<DiffLayout, ConfigError> {
        let text = self.read_config_toml()?;
        if text.trim().is_empty() {
            return Ok(DiffLayout::Split);
        }
        let document = text.parse::<toml_edit::DocumentMut>()?;
        let Some(diff) = document.get("diff") else {
            return Ok(DiffLayout::Split);
        };
        let Some(diff) = diff.as_table() else {
            return Err(ConfigError::Invalid("diff is not a table".into()));
        };
        let Some(layout) = diff.get("layout") else {
            return Ok(DiffLayout::Split);
        };
        let Some(layout) = layout.as_str() else {
            return Err(ConfigError::Invalid("diff.layout is not a string".into()));
        };
        match layout.to_ascii_lowercase().as_str() {
            "split" => Ok(DiffLayout::Split),
            "unified" => Ok(DiffLayout::Unified),
            other => Err(ConfigError::Invalid(format!(
                "diff.layout must be `split` or `unified`, got `{other}`"
            ))),
        }
    }

    pub fn math_rendering_enabled(&self) -> Result<bool, ConfigError> {
        let text = self.read_config_toml()?;
        if text.trim().is_empty() {
            return Ok(true);
        }
        let document = text.parse::<toml_edit::DocumentMut>()?;
        let Some(render) = document.get("render") else {
            return Ok(true);
        };
        let Some(render) = render.as_table() else {
            return Err(ConfigError::Invalid("render is not a table".into()));
        };
        let Some(math) = render.get("math") else {
            return Ok(true);
        };
        math.as_bool()
            .ok_or_else(|| ConfigError::Invalid("render.math is not a boolean".into()))
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

    pub fn auto_snapshot(&self) -> Result<Option<bool>, ConfigError> {
        let text = self.read_config_toml()?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        let document = text.parse::<toml_edit::DocumentMut>()?;
        let Some(registry) = document.get("registry") else {
            return Ok(None);
        };
        let Some(registry) = registry.as_table() else {
            return Err(ConfigError::Invalid("registry is not a table".into()));
        };
        let Some(auto_snapshot) = registry.get("auto_snapshot") else {
            return Ok(None);
        };
        if let Some(value) = auto_snapshot.as_bool() {
            return Ok(Some(value));
        }
        if let Some(value) = auto_snapshot.as_integer() {
            return Ok(Some(value == 1));
        }
        if let Some(value) = auto_snapshot.as_str() {
            return Ok(Some(value == "true"));
        }
        Err(ConfigError::Invalid(
            "registry.auto_snapshot has an unsupported type".into(),
        ))
    }

    pub fn compact_review_mode(&self) -> Result<Option<crate::CompactReviewMode>, ConfigError> {
        let text = self.read_config_toml()?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        let document = text.parse::<toml_edit::DocumentMut>()?;
        let Some(compaction) = document.get("compaction") else {
            return Ok(None);
        };
        let Some(compaction) = compaction.as_table() else {
            return Err(ConfigError::Invalid("compaction is not a table".into()));
        };
        let Some(review) = compaction.get("review") else {
            return Ok(None);
        };
        let Some(review) = review.as_str() else {
            return Err(ConfigError::Invalid(
                "compaction.review is not a string".into(),
            ));
        };
        crate::CompactReviewMode::parse(review)
            .map(Some)
            .ok_or_else(|| ConfigError::Invalid(format!("invalid compaction.review: {review:?}")))
    }

    pub fn suggest_model(&self) -> Result<Option<String>, ConfigError> {
        let text = self.read_config_toml()?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        let document = text.parse::<toml_edit::DocumentMut>()?;
        let Some(suggest) = document.get("suggest") else {
            return Ok(None);
        };
        let Some(suggest) = suggest.as_table() else {
            return Err(ConfigError::Invalid("suggest is not a table".into()));
        };
        let Some(model) = suggest.get("model") else {
            return Ok(None);
        };
        let Some(model) = model.as_str() else {
            return Err(ConfigError::Invalid("suggest.model is not a string".into()));
        };
        Ok(Some(model.to_string()))
    }

    pub fn interjection_mode(&self) -> Result<Option<InterjectionMode>, ConfigError> {
        let text = self.read_config_toml()?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        let document = text.parse::<toml_edit::DocumentMut>()?;
        let Some(interjection) = document.get("interjection") else {
            return Ok(None);
        };
        let Some(interjection) = interjection.as_table() else {
            return Err(ConfigError::Invalid("interjection is not a table".into()));
        };
        let Some(classifier) = interjection.get("classifier") else {
            return Ok(None);
        };
        let Some(classifier) = classifier.as_str() else {
            return Err(ConfigError::Invalid(
                "interjection.classifier is not a string".into(),
            ));
        };
        Ok(Some(match classifier {
            "off" => InterjectionMode::Off,
            "rule" => InterjectionMode::Rule,
            "llm" => InterjectionMode::Llm,
            other => InterjectionMode::Unknown(other.to_string()),
        }))
    }

    pub fn tool_output_budget(
        &self,
    ) -> Result<crate::tools::tool_output::ToolOutputBudget, ConfigError> {
        #[derive(Debug, serde::Deserialize, Default)]
        struct RawToolOutput {
            #[serde(default)]
            max_lines: Option<usize>,
            #[serde(default)]
            max_bytes: Option<usize>,
            #[serde(default)]
            max_line_bytes: Option<usize>,
        }
        #[derive(Debug, serde::Deserialize, Default)]
        struct RawFile {
            #[serde(default)]
            tool_output: RawToolOutput,
        }
        let text = self.read_config_toml()?;
        if text.trim().is_empty() {
            return Ok(Default::default());
        }
        let raw: RawFile = toml::from_str(&text)
            .map_err(|error| ConfigError::Invalid(format!("tool_output config: {error}")))?;
        let defaults = crate::tools::tool_output::ToolOutputBudget::default();
        let budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: raw.tool_output.max_lines.unwrap_or(defaults.max_lines),
            max_bytes: raw.tool_output.max_bytes.unwrap_or(defaults.max_bytes),
            max_line_bytes: raw
                .tool_output
                .max_line_bytes
                .unwrap_or(defaults.max_line_bytes),
        };
        if budget.max_lines == 0 || budget.max_bytes == 0 || budget.max_line_bytes == 0 {
            return Err(ConfigError::Invalid(
                "tool_output budgets must be positive".into(),
            ));
        }
        Ok(budget)
    }

    pub fn web_fetch_config(&self) -> Result<crate::tools::web::WebConfig, ConfigError> {
        #[derive(Debug, serde::Deserialize, Default)]
        struct RawWeb {
            #[serde(default)]
            max_bytes: Option<usize>,
            #[serde(default)]
            url_allowlist: Vec<String>,
            #[serde(default)]
            url_denylist: Vec<String>,
        }
        #[derive(Debug, serde::Deserialize, Default)]
        struct RawWebFile {
            #[serde(default)]
            web: RawWeb,
        }

        let text = self.read_config_toml()?;
        let mut config = crate::tools::web::WebConfig::default();
        if text.trim().is_empty() {
            return Ok(config);
        }
        let file: RawWebFile = toml::from_str(&text)
            .map_err(|error| ConfigError::Invalid(format!("parse web fetch config: {error}")))?;
        if let Some(value) = file.web.max_bytes {
            config.max_bytes = value;
        }
        if !file.web.url_allowlist.is_empty() {
            config.url_allowlist = file.web.url_allowlist;
        }
        if !file.web.url_denylist.is_empty() {
            config.url_denylist = file.web.url_denylist;
        }
        Ok(config)
    }

    pub fn web_search_config(&self) -> Result<crate::tools::web::SearchConfig, ConfigError> {
        #[derive(Debug, serde::Deserialize, Default)]
        struct RawWeb {
            #[serde(default)]
            search: Option<crate::tools::web::SearchConfig>,
        }
        #[derive(Debug, serde::Deserialize, Default)]
        struct RawWebFile {
            #[serde(default)]
            web: RawWeb,
        }

        let text = self.read_config_toml()?;
        if text.trim().is_empty() {
            return Ok(crate::tools::web::SearchConfig::default());
        }
        let file: RawWebFile = toml::from_str(&text)
            .map_err(|error| ConfigError::Invalid(format!("parse web search config: {error}")))?;
        Ok(file.web.search.unwrap_or_default())
    }

    pub fn trust_config(&self) -> Result<crate::trust::TrustConfig, ConfigError> {
        #[derive(Debug, serde::Deserialize, Default)]
        struct RawTrustFile {
            #[serde(default)]
            trust: crate::trust::TrustConfig,
        }

        let text = self.read_config_toml()?;
        if text.trim().is_empty() {
            return Ok(crate::trust::TrustConfig::default());
        }
        let file: RawTrustFile = toml::from_str(&text)
            .map_err(|error| ConfigError::Invalid(format!("parse trust config: {error}")))?;
        Ok(file.trust)
    }

    pub fn set_trust_config(&self, trust: &crate::trust::TrustConfig) -> Result<(), ConfigError> {
        let serialized = toml::to_string(trust)
            .map_err(|error| ConfigError::Invalid(format!("serialize trust config: {error}")))?;
        let trust_doc = serialized.parse::<toml_edit::DocumentMut>()?;
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let _file_lock = self.lock_config_file()?;
        let text = self.read_config_toml()?;
        let mut doc = if text.trim().is_empty() {
            toml_edit::DocumentMut::new()
        } else {
            text.parse()?
        };
        let mut table = toml_edit::Table::new();
        for (key, item) in trust_doc.iter() {
            table.insert(key, item.clone());
        }
        doc.insert("trust", toml_edit::Item::Table(table));
        self.write_config_toml(&doc.to_string())
    }

    pub fn preview_config(&self) -> Result<crate::tools::preview::PreviewConfig, ConfigError> {
        #[derive(Debug, serde::Deserialize, Default)]
        struct RawPreview {
            #[serde(default)]
            base_url: Option<String>,
            #[serde(default)]
            timeout_ms: Option<u64>,
            #[serde(default)]
            project_abs_path: Option<String>,
            #[serde(default)]
            project_hint_slug: Option<String>,
            #[serde(default)]
            max_body_bytes: Option<usize>,
        }
        #[derive(Debug, serde::Deserialize, Default)]
        struct RawPreviewFile {
            #[serde(default)]
            preview: RawPreview,
        }

        let text = self.read_config_toml()?;
        let mut config = crate::tools::preview::PreviewConfig::default();
        if text.trim().is_empty() {
            return Ok(config);
        }
        let file: RawPreviewFile = toml::from_str(&text)
            .map_err(|error| ConfigError::Invalid(format!("parse preview config: {error}")))?;
        if let Some(value) = file.preview.base_url {
            config.base_url = value;
        }
        if let Some(value) = file.preview.timeout_ms {
            config.timeout_ms = value;
        }
        if let Some(value) = file.preview.project_abs_path {
            config.project_abs_path = value;
        }
        if let Some(value) = file.preview.project_hint_slug {
            config.project_hint_slug = Some(value);
        }
        if let Some(value) = file.preview.max_body_bytes {
            config.max_body_bytes = value;
        }
        Ok(config)
    }

    pub fn sandbox_config(&self) -> Result<SandboxConfig, ConfigError> {
        #[derive(Debug, serde::Deserialize, Default)]
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
            #[serde(default)]
            allow_network: Option<bool>,
        }
        #[derive(Debug, serde::Deserialize, Default)]
        struct RawSandboxFile {
            #[serde(default)]
            sandbox: RawSandbox,
        }

        let text = self.read_config_toml()?;
        if text.trim().is_empty() {
            return Ok(SandboxConfig::default());
        }
        let file: RawSandboxFile = toml::from_str(&text)
            .map_err(|error| ConfigError::Invalid(format!("parse sandbox config: {error}")))?;
        Ok(SandboxConfig {
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
            allow_network: file.sandbox.allow_network.unwrap_or(false),
        })
    }

    pub fn redact_config(&self) -> Result<RedactConfig, ConfigError> {
        #[derive(Debug, serde::Deserialize, Default)]
        struct RawPattern {
            kind: String,
            regex: String,
        }
        #[derive(Debug, serde::Deserialize, Default)]
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
        #[derive(Debug, serde::Deserialize, Default)]
        struct RawRedactFile {
            #[serde(default)]
            redact: RawRedact,
        }

        let text = self.read_config_toml()?;
        if text.trim().is_empty() {
            return Ok(RedactConfig::default());
        }
        let file: RawRedactFile = toml::from_str(&text)
            .map_err(|error| ConfigError::Invalid(format!("parse redact config: {error}")))?;
        Ok(RedactConfig {
            enabled: file.redact.enabled,
            partial: file.redact.mode.as_deref() == Some("partial"),
            allowlist: file.redact.allowlist,
            custom_patterns: file
                .redact
                .custom_patterns
                .into_iter()
                .map(|pattern| (pattern.kind, pattern.regex))
                .collect(),
        })
    }

    pub fn upsert_model(&self, update: ModelConfigUpdate<'_>) -> Result<(), ConfigError> {
        self.update_config_toml(|doc| {
            validate_model_name(doc, update.old_name, update.name)?;
            crate::model_registry::apply_model_config_update(doc, update)
                .map_err(|error| ConfigError::Invalid(error.to_string()))
        })
    }

    pub fn remove_model(&self, name: &str) -> Result<(), ConfigError> {
        self.update_config_toml(|doc| {
            let aliases = table_entries(doc, "alias")?;
            let mut dependents = aliases
                .into_iter()
                .filter_map(|(alias, entry)| {
                    (entry.get("model").and_then(toml_edit::Item::as_str) == Some(name))
                        .then_some(alias)
                })
                .collect::<Vec<_>>();
            dependents.sort();
            if !dependents.is_empty() {
                return Err(ConfigError::Invalid(format!(
                    "model `{name}` is referenced by aliases: {}",
                    dependents.join(", ")
                )));
            }
            let models = required_table_mut(doc, "models")?;
            if models.remove(name).is_none() {
                return Err(ConfigError::Invalid(format!(
                    "config model `{name}` does not exist"
                )));
            }
            Ok(())
        })
    }

    pub fn upsert_provider(&self, update: ProviderConfigUpdate<'_>) -> Result<(), ConfigError> {
        crate::provider_lifecycle::upsert_config_provider_for_hub(self, update)
    }

    pub fn remove_provider(&self, name: &str) -> Result<(), ConfigError> {
        crate::provider_lifecycle::remove_config_provider_for_hub(self, name)
    }

    #[cfg(test)]
    pub(crate) fn create_provider(
        &self,
        update: ProviderConfigUpdate<'_>,
    ) -> Result<crate::model_registry::ProviderEntry, ConfigError> {
        self.write_provider_config(update, ProviderConfigWriteMode::Create)
    }

    #[cfg(test)]
    pub(crate) fn update_provider(
        &self,
        update: ProviderConfigUpdate<'_>,
    ) -> Result<crate::model_registry::ProviderEntry, ConfigError> {
        self.write_provider_config(update, ProviderConfigWriteMode::Update)
    }

    #[cfg(test)]
    fn write_provider_config(
        &self,
        update: ProviderConfigUpdate<'_>,
        mode: ProviderConfigWriteMode,
    ) -> Result<crate::model_registry::ProviderEntry, ConfigError> {
        self.write_provider_config_and_then(update, mode, |_| ())
            .map(|(entry, ())| entry)
    }

    pub(crate) fn write_provider_config_and_then<T>(
        &self,
        update: ProviderConfigUpdate<'_>,
        mode: ProviderConfigWriteMode,
        after_commit: impl FnOnce(&crate::model_registry::ProviderEntry) -> T,
    ) -> Result<(crate::model_registry::ProviderEntry, T), ConfigError> {
        self.update_config_toml_and_then(
            |doc| {
                if doc.get("providers").is_none() {
                    doc.insert("providers", toml_edit::Item::Table(toml_edit::Table::new()));
                }
                let providers = doc
                    .get_mut("providers")
                    .and_then(toml_edit::Item::as_table_mut)
                    .ok_or_else(|| ConfigError::Invalid("providers is not a table".into()))?;
                let exists = providers.contains_key(update.name);
                match (mode, exists) {
                    (ProviderConfigWriteMode::Create, true) => {
                        return Err(ConfigError::NameConflict {
                            name: update.name.to_string(),
                            domain: "providers",
                        });
                    }
                    (ProviderConfigWriteMode::Update, false) => {
                        return Err(ConfigError::Invalid(format!(
                            "config provider `{}` does not exist",
                            update.name
                        )));
                    }
                    _ => {}
                }
                let reasoning_format = match update.reasoning_format {
                    Some(value) => Some(value),
                    None => providers
                        .get(update.name)
                        .and_then(toml_edit::Item::as_table)
                        .and_then(|entry| entry.get("reasoning_format"))
                        .and_then(toml_edit::Item::as_str)
                        .map(str::parse)
                        .transpose()
                        .map_err(ConfigError::Invalid)?,
                };
                let prompt_cache_key = update.prompt_cache_key.or_else(|| {
                    providers
                        .get(update.name)
                        .and_then(toml_edit::Item::as_table)
                        .and_then(|entry| entry.get("prompt_cache_key"))
                        .and_then(toml_edit::Item::as_bool)
                });
                let mut entry = toml_edit::Table::new();
                entry.insert("kind", toml_edit::value(update.kind));
                insert_nonempty(&mut entry, "api_key", update.api_key);
                insert_nonempty(&mut entry, "api_key_env", update.api_key_env);
                insert_nonempty(&mut entry, "base_url", update.base_url);
                if let Some(value) = update.max_tokens {
                    entry.insert("max_tokens", toml_edit::value(i64::from(value)));
                }
                if let Some(value) = reasoning_format {
                    entry.insert("reasoning_format", toml_edit::value(value.to_string()));
                }
                if let Some(value) = prompt_cache_key {
                    entry.insert("prompt_cache_key", toml_edit::value(value));
                }
                entry.insert("enabled", toml_edit::value(update.enabled));
                providers.insert(update.name, toml_edit::Item::Table(entry));
                Ok(crate::model_registry::ProviderEntry {
                    name: update.name.to_string(),
                    kind: update.kind.to_string(),
                    api_key: update
                        .api_key
                        .filter(|value| !value.is_empty())
                        .map(str::to_string),
                    api_key_env: update
                        .api_key_env
                        .filter(|value| !value.is_empty())
                        .map(str::to_string),
                    base_url: update
                        .base_url
                        .filter(|value| !value.is_empty())
                        .map(str::to_string),
                    max_tokens: update.max_tokens,
                    reasoning_format,
                    prompt_cache_key,
                    enabled: Some(update.enabled),
                })
            },
            after_commit,
        )
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
            let Some(aliases) = doc.get_mut("alias") else {
                return Err(ConfigError::Invalid(format!(
                    "config alias `{alias}` does not exist"
                )));
            };
            let aliases = aliases
                .as_table_mut()
                .ok_or_else(|| ConfigError::Invalid("alias is not a table".into()))?;
            if aliases.remove(alias).is_none() {
                return Err(ConfigError::Invalid(format!(
                    "config alias `{alias}` does not exist"
                )));
            }
            Ok(())
        })
    }

    pub(crate) fn remove_provider_config_and_then<T>(
        &self,
        name: &str,
        after_commit: impl FnOnce() -> T,
    ) -> Result<T, ConfigError> {
        self.update_config_toml_and_then(
            |doc| {
                let models = table_entries(doc, "models")?;
                let mut dependents = models
                    .into_iter()
                    .filter_map(|(model, entry)| {
                        (entry.get("provider").and_then(toml_edit::Item::as_str) == Some(name))
                            .then_some(model)
                    })
                    .collect::<Vec<_>>();
                dependents.sort();
                if !dependents.is_empty() {
                    return Err(ConfigError::Invalid(format!(
                        "provider `{name}` is referenced by models: {}",
                        dependents.join(", ")
                    )));
                }
                let Some(providers) = doc.get_mut("providers") else {
                    return Err(ConfigError::Invalid(format!(
                        "config provider `{name}` does not exist"
                    )));
                };
                let providers = providers
                    .as_table_mut()
                    .ok_or_else(|| ConfigError::Invalid("providers is not a table".into()))?;
                if providers.remove(name).is_none() {
                    return Err(ConfigError::Invalid(format!(
                        "config provider `{name}` does not exist"
                    )));
                }
                Ok(())
            },
            |_| after_commit(),
        )
        .map(|(_, result)| result)
    }

    pub fn reload(&self) -> Result<(), ConfigError> {
        crate::provider_lifecycle::reload_config_providers_for_hub(self)
    }

    pub(crate) fn reload_and_then<T>(
        &self,
        apply: impl FnOnce(crate::model_registry::ProviderConfig) -> T,
    ) -> Result<T, ConfigError> {
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let _file_lock = self.lock_config_file()?;
        let text = self.read_config_toml()?;
        let prepared = crate::model_registry::prepare_config_text(&text)
            .map_err(|error| ConfigError::Invalid(error.to_string()))?;
        let snapshot = prepared.snapshot();
        crate::model_registry::commit_prepared_config(prepared);
        Ok(apply(snapshot))
    }

    pub fn model_config(
        &self,
    ) -> Result<Option<crate::model_registry::ProviderConfig>, ConfigError> {
        let text = self.read_config_toml()?;
        if text.trim().is_empty() {
            return Ok(None);
        }
        let document = text.parse::<toml_edit::DocumentMut>()?;
        if document.get("providers").is_none()
            && document.get("models").is_none()
            && document.get("alias").is_none()
        {
            return Ok(None);
        }
        let has_model_entries = ["providers", "models", "alias"].iter().any(|section| {
            document
                .get(section)
                .and_then(toml_edit::Item::as_table)
                .is_some_and(|table| !table.is_empty())
        });
        if !has_model_entries {
            return Ok(None);
        }
        crate::model_registry::parse_config(&text)
            .ok_or_else(|| ConfigError::Invalid("invalid model configuration".into()))
            .map(Some)
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

    pub fn replace_mcp(
        &self,
        original_name: &str,
        config: crate::mcp::McpServerConfig,
    ) -> Result<(), ConfigError> {
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let mut configs = self.load_local_mcp();
        let index = configs
            .iter()
            .position(|current| current.name == original_name)
            .ok_or_else(|| {
                ConfigError::Invalid(format!("MCP server {original_name:?} not found"))
            })?;
        if config.name != original_name && configs.iter().any(|current| current.name == config.name)
        {
            return Err(ConfigError::NameConflict {
                domain: "MCP servers",
                name: config.name,
            });
        }
        configs[index] = config;
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

    pub fn migrate_and_reload_models(
        &self,
    ) -> Result<crate::model_registry::ModelMigrationOutcome, ConfigError> {
        let outcome = self.migrate_model_config_if_needed()?;
        self.reload()?;
        Ok(outcome)
    }

    pub fn migrate_model_config_if_needed(
        &self,
    ) -> Result<crate::model_registry::ModelMigrationOutcome, ConfigError> {
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let _file_lock = self.lock_config_file()?;
        let text = self.read_config_toml()?;
        let Some(migrated) = crate::model_registry::migrate_config_if_needed(&text)? else {
            return Ok(crate::model_registry::ModelMigrationOutcome::NotNeeded);
        };
        let backup = self.config_dir.join("config.toml.bak");
        write_sensitive_create_new_or_same(&backup, text.as_bytes())?;
        self.write_config_toml(&migrated)?;
        Ok(crate::model_registry::ModelMigrationOutcome::Migrated { backup })
    }

    fn lock_config_file(&self) -> Result<std::fs::File, ConfigError> {
        lock_file(&self.config_dir.join(".config.toml.lock"))
    }

    fn update_config_toml<T>(
        &self,
        mutate: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<T, ConfigError>,
    ) -> Result<T, ConfigError> {
        self.update_config_toml_and_then(mutate, |_| ())
            .map(|(result, ())| result)
    }

    fn update_config_toml_and_then<T, U>(
        &self,
        mutate: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<T, ConfigError>,
        after_commit: impl FnOnce(&T) -> U,
    ) -> Result<(T, U), ConfigError> {
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let _file_lock = self.lock_config_file()?;
        let text = self.read_config_toml()?;
        let mut doc = if text.trim().is_empty() {
            toml_edit::DocumentMut::new()
        } else {
            text.parse()?
        };
        let result = mutate(&mut doc)?;
        let new_text = doc.to_string();
        let prepared = crate::model_registry::prepare_config_text(&new_text)
            .map_err(|error| ConfigError::Invalid(error.to_string()))?;
        self.write_config_toml(&new_text)?;
        crate::model_registry::commit_prepared_config(prepared);
        let after_commit = after_commit(&result);
        Ok((result, after_commit))
    }

    fn write_config_toml(&self, text: &str) -> Result<(), ConfigError> {
        write_unique_atomic(&self.config_toml_path(), text.as_bytes())
    }

    fn write_daemon_config(&self, config: &DaemonConfig) -> Result<(), ConfigError> {
        let text = toml::to_string(config)
            .map_err(|error| ConfigError::Invalid(format!("serialize daemon config: {error}")))?;
        let path = self
            .daemon_config_path
            .as_deref()
            .ok_or_else(|| ConfigError::Invalid("daemon config path is not configured".into()))?;
        write_sensitive_atomic(path, text.as_bytes())
    }

    fn write_auth_document(
        &self,
        document: &crate::auth_store::AuthStoreDocument,
    ) -> Result<(), ConfigError> {
        let json = serde_json::to_vec_pretty(document)
            .map_err(|error| ConfigError::Invalid(format!("serialize auth store: {error}")))?;
        write_sensitive_atomic(&self.auth_path, &json)
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

fn auth_provider_matches(
    current: &crate::auth_store::StoredProvider,
    expected: &crate::auth_store::StoredProvider,
) -> bool {
    current.id == expected.id && current.name == expected.name && current.kind == expected.kind
}

fn sorted_auth_provider_ids(store: &crate::auth_store::AuthStore) -> Vec<String> {
    let mut provider_ids = store
        .providers
        .iter()
        .map(|provider| provider.id.clone())
        .collect::<Vec<_>>();
    provider_ids.sort();
    provider_ids
}

fn dsl_route_source(flow_name: &str, trigger: &str) -> Result<String, ConfigError> {
    if syn_identifier(flow_name).is_none() {
        return Err(ConfigError::Invalid(format!(
            "route flow {flow_name:?} is not a valid DSL identifier"
        )));
    }
    if trigger.is_empty() {
        return Err(ConfigError::Invalid(
            "route trigger must not be empty".to_string(),
        ));
    }
    let trigger = format!("{trigger:?}");
    let route = format!("route {trigger} {{ flow: {flow_name} }}\n");
    parse_routes_source("generated route", &route)?;
    Ok(route)
}

fn syn_identifier(value: &str) -> Option<()> {
    let source = format!("flow {value}() {{}}\n");
    atman_dsl::parse::parse_file(&source).ok().map(|_| ())
}

fn parse_routes_source(context: &str, source: &str) -> Result<(), ConfigError> {
    if source.is_empty() {
        return Ok(());
    }
    atman_dsl::parse::parse_file(source)
        .map(|_| ())
        .map_err(|error| ConfigError::Invalid(format!("parse {context}: {error}")))
}

fn lock_path_for(path: &Path) -> PathBuf {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    parent.join(format!(".{name}.lock"))
}

fn lock_file(path: &Path) -> Result<std::fs::File, ConfigError> {
    use fs2::FileExt;
    std::fs::create_dir_all(path.parent().unwrap_or_else(|| Path::new(".")))?;
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    lock.lock_exclusive()?;
    Ok(lock)
}

fn write_unique_atomic(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    use std::io::Write;

    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let tmp = parent.join(format!(".{filename}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| -> Result<(), ConfigError> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn load_auth_from_path(path: &Path) -> Result<crate::auth_store::AuthStore, ConfigError> {
    Ok(load_auth_document_from_path(path)?.legacy_view())
}

fn load_auth_document_from_path(
    path: &Path,
) -> Result<crate::auth_store::AuthStoreDocument, ConfigError> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| ConfigError::Invalid(format!("parse {}: {error}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(crate::auth_store::AuthStoreDocument::default())
        }
        Err(error) => Err(error.into()),
    }
}

fn write_sensitive_create_new_or_same(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    match std::fs::read(path) {
        Ok(existing) if existing == contents => return Ok(()),
        Ok(_) => {
            return Err(ConfigError::Invalid(format!(
                "backup conflict at {}",
                path.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options.open(path)?;
    set_sensitive_file_permissions(path)?;
    if let Err(error) = file.write_all(contents).and_then(|_| file.sync_all()) {
        drop(file);
        let _ = std::fs::remove_file(path);
        return Err(error.into());
    }
    Ok(())
}

fn write_sensitive_atomic(path: &Path, contents: &[u8]) -> Result<(), ConfigError> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("sensitive-config");
    let tmp = parent.join(format!(".{filename}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    set_sensitive_file_permissions(&tmp)?;
    use std::io::Write;
    file.write_all(contents)?;
    drop(file);
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn generate_daemon_token() -> String {
    let first = uuid::Uuid::new_v4().simple().to_string();
    let second = uuid::Uuid::new_v4().simple().to_string();
    format!("{first}{second}")
}

fn set_sensitive_file_permissions(path: &Path) -> Result<(), ConfigError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
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

fn table_entries<'a>(
    doc: &'a toml_edit::DocumentMut,
    table: &'static str,
) -> Result<Vec<(String, &'a toml_edit::Table)>, ConfigError> {
    match doc.get(table) {
        None => Ok(Vec::new()),
        Some(item) => item
            .as_table()
            .ok_or_else(|| ConfigError::Invalid(format!("{table} is not a table")))?
            .iter()
            .map(|(name, item)| {
                item.as_table()
                    .map(|entry| (name.to_string(), entry))
                    .ok_or_else(|| ConfigError::Invalid(format!("{table}.{name} is not a table")))
            })
            .collect(),
    }
}

fn required_table_mut<'a>(
    doc: &'a mut toml_edit::DocumentMut,
    table: &'static str,
) -> Result<&'a mut toml_edit::Table, ConfigError> {
    doc.get_mut(table)
        .and_then(toml_edit::Item::as_table_mut)
        .ok_or_else(|| ConfigError::Invalid(format!("{table} is not a table")))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ProviderRegistryReset;

    impl Drop for ProviderRegistryReset {
        fn drop(&mut self) {
            crate::model_registry::set_provider_config(Default::default());
        }
    }

    fn temp_hub() -> (tempfile::TempDir, ConfigHub) {
        let dir = tempfile::tempdir().unwrap();
        let hub = ConfigHub::from_config_dir(dir.path());
        (dir, hub)
    }

    #[test]
    fn settings_mutation_validation_is_centralized() {
        let (_dir, hub) = temp_hub();
        assert!(hub.validate_setting_mutation("trust.mode", "allow").is_ok());
        assert!(hub.validate_setting_mutation("trust.mode", " ").is_err());
        assert!(hub.validate_setting_mutation("missing", "x").is_err());
    }

    #[test]
    fn provider_reasoning_format_is_written_and_preserved() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (_dir, hub) = temp_hub();
        let base = ProviderConfigUpdate {
            name: "gateway",
            kind: "openai-compat",
            api_key: None,
            api_key_env: Some("GATEWAY_KEY"),
            base_url: Some("https://gateway.example/v1"),
            max_tokens: None,
            reasoning_format: Some(crate::providers::openai::OpenAiReasoningFormat::Official),
            prompt_cache_key: Some(true),
            enabled: true,
        };
        hub.upsert_provider(base).unwrap();
        hub.upsert_provider(ProviderConfigUpdate {
            reasoning_format: None,
            prompt_cache_key: None,
            enabled: false,
            ..base
        })
        .unwrap();

        let config = std::fs::read_to_string(hub.config_toml_path()).unwrap();
        assert!(config.contains("reasoning_format = \"reasoning-effort\""));
        assert!(config.contains("prompt_cache_key = true"));
        assert!(config.contains("enabled = false"));
        assert_eq!(
            hub.model_config().unwrap().unwrap().providers["gateway"].prompt_cache_key,
            Some(true)
        );
    }

    #[test]
    fn provider_create_returns_the_committed_snapshot() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _reset = ProviderRegistryReset;
        let (_dir, hub) = temp_hub();

        let snapshot = hub
            .create_provider(ProviderConfigUpdate {
                name: "gateway",
                kind: "openai-compat",
                api_key: Some("inline-key"),
                api_key_env: Some("GATEWAY_KEY"),
                base_url: Some("https://gateway.example/v1"),
                max_tokens: Some(16_384),
                reasoning_format: Some(crate::providers::openai::OpenAiReasoningFormat::Official),
                prompt_cache_key: None,
                enabled: false,
            })
            .unwrap();

        assert_eq!(snapshot.name, "gateway");
        assert_eq!(snapshot.kind, "openai-compat");
        assert_eq!(snapshot.api_key.as_deref(), Some("inline-key"));
        assert_eq!(snapshot.api_key_env.as_deref(), Some("GATEWAY_KEY"));
        assert_eq!(
            snapshot.base_url.as_deref(),
            Some("https://gateway.example/v1")
        );
        assert_eq!(snapshot.max_tokens, Some(16_384));
        assert_eq!(
            snapshot.reasoning_format,
            Some(crate::providers::openai::OpenAiReasoningFormat::Official)
        );
        assert_eq!(snapshot.enabled, Some(false));

        let committed = hub.model_config().unwrap().unwrap().providers["gateway"].clone();
        assert_eq!(committed.name, snapshot.name);
        assert_eq!(committed.kind, snapshot.kind);
        assert_eq!(committed.api_key, snapshot.api_key);
        assert_eq!(committed.api_key_env, snapshot.api_key_env);
        assert_eq!(committed.base_url, snapshot.base_url);
        assert_eq!(committed.max_tokens, snapshot.max_tokens);
        assert_eq!(committed.reasoning_format, snapshot.reasoning_format);
        assert_eq!(committed.enabled, snapshot.enabled);
    }

    #[test]
    fn provider_create_conflict_does_not_overwrite_the_existing_entry() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _reset = ProviderRegistryReset;
        let (_dir, hub) = temp_hub();
        let initial = ProviderConfigUpdate {
            name: "gateway",
            kind: "openai-compat",
            api_key: Some("first-key"),
            api_key_env: None,
            base_url: Some("https://first.example/v1"),
            max_tokens: Some(8_192),
            reasoning_format: None,
            prompt_cache_key: None,
            enabled: true,
        };
        hub.create_provider(initial).unwrap();
        let before = hub.read_config_toml().unwrap();

        let error = hub
            .create_provider(ProviderConfigUpdate {
                kind: "anthropic",
                api_key: Some("second-key"),
                base_url: Some("https://second.example/v1"),
                ..initial
            })
            .unwrap_err();

        assert!(matches!(
            error,
            ConfigError::NameConflict {
                ref name,
                domain: "providers"
            } if name == "gateway"
        ));
        assert_eq!(hub.read_config_toml().unwrap(), before);
        let committed = &hub.model_config().unwrap().unwrap().providers["gateway"];
        assert_eq!(committed.kind, "openai-compat");
        assert_eq!(committed.api_key.as_deref(), Some("first-key"));
        assert_eq!(
            committed.base_url.as_deref(),
            Some("https://first.example/v1")
        );
    }

    #[test]
    fn provider_update_missing_does_not_create_an_entry() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _reset = ProviderRegistryReset;
        let (_dir, hub) = temp_hub();
        let before = hub.read_config_toml().unwrap();

        let error = hub
            .update_provider(ProviderConfigUpdate {
                name: "missing",
                kind: "openai-compat",
                api_key: None,
                api_key_env: None,
                base_url: Some("https://gateway.example/v1"),
                max_tokens: None,
                reasoning_format: None,
                prompt_cache_key: None,
                enabled: true,
            })
            .unwrap_err();

        assert!(error.to_string().contains("does not exist"));
        assert_eq!(hub.read_config_toml().unwrap(), before);
        assert!(hub.model_config().unwrap().is_none());
    }

    #[test]
    fn reload_reads_and_commits_under_the_config_write_lock() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _reset = ProviderRegistryReset;
        let (_dir, hub) = temp_hub();
        std::fs::write(
            hub.config_toml_path(),
            "[providers.gateway]\nkind = \"openai-compat\"\napi_key = \"old-key\"\nenabled = true\n",
        )
        .unwrap();

        let config_guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let reload_hub = hub.clone();
        let reload = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            finished_tx.send(reload_hub.reload()).unwrap();
        });
        started_rx.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(matches!(
            finished_rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        ));
        std::fs::write(
            hub.config_toml_path(),
            "[providers.gateway]\nkind = \"openai-compat\"\napi_key = \"new-key\"\nenabled = false\n",
        )
        .unwrap();
        drop(config_guard);

        finished_rx.recv().unwrap().unwrap();
        reload.join().unwrap();
        let projected = crate::model_registry::all_provider_entries()
            .into_iter()
            .find(|(name, _)| name == "gateway")
            .map(|(_, entry)| entry)
            .unwrap();
        assert_eq!(projected.api_key.as_deref(), Some("new-key"));
        assert_eq!(projected.enabled, Some(false));
    }

    #[test]
    fn tool_output_budget_uses_defaults_and_reads_overrides() {
        let (dir, hub) = temp_hub();
        assert_eq!(
            hub.tool_output_budget().unwrap(),
            crate::tools::tool_output::ToolOutputBudget {
                max_lines: 256,
                max_bytes: 10 * 1024,
                max_line_bytes: 10 * 1024,
            }
        );
        write_config(
            &hub,
            "[tool_output]\nmax_lines = 7\nmax_bytes = 777\nmax_line_bytes = 111\n",
        );
        assert_eq!(
            hub.tool_output_budget().unwrap(),
            crate::tools::tool_output::ToolOutputBudget {
                max_lines: 7,
                max_bytes: 777,
                max_line_bytes: 111,
            }
        );
        let _ = dir;
    }

    #[test]
    fn tool_output_budget_rejects_zero_values() {
        let (_dir, hub) = temp_hub();
        write_config(&hub, "[tool_output]\nmax_bytes = 0\n");
        assert!(hub.tool_output_budget().is_err());
    }

    #[test]
    fn storage_config_merges_only_typed_storage_projection() {
        let (dir, hub) = temp_hub();
        std::fs::write(
            dir.path().join("config.toml"),
            "[storage]\nscope = \"local\"\n[theme]\nmode = \"dark\"\n",
        )
        .unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join(".atman")).unwrap();
        std::fs::write(
            project.path().join(".atman/config.toml"),
            "[storage]\nscope = \"global\"\n[theme]\nmode = \"light\"\n",
        )
        .unwrap();

        assert_eq!(
            hub.storage_config(Some(project.path())).scope,
            Some(crate::storage::StorageScope::Global)
        );
    }

    #[test]
    fn set_project_storage_scope_preserves_other_project_config() {
        let (_dir, hub) = temp_hub();
        let project = tempfile::tempdir().unwrap();
        let config_dir = project.path().join(".atman");
        std::fs::create_dir(&config_dir).unwrap();
        std::fs::write(
            config_dir.join("config.toml"),
            "# keep this comment\n[theme]\nmode = \"dark\"\n",
        )
        .unwrap();

        hub.set_project_storage_scope(project.path(), crate::storage::StorageScope::Local)
            .unwrap();
        let text = std::fs::read_to_string(config_dir.join("config.toml")).unwrap();
        assert!(text.contains("# keep this comment"));
        assert!(text.contains("[theme]"));
        assert_eq!(
            hub.storage_config(Some(project.path())).scope,
            Some(crate::storage::StorageScope::Local)
        );

        hub.set_project_storage_scope(project.path(), crate::storage::StorageScope::Global)
            .unwrap();
        assert_eq!(
            hub.storage_config(Some(project.path())).scope,
            Some(crate::storage::StorageScope::Global)
        );
    }

    #[test]
    fn storage_config_isolates_invalid_global_and_project_layers() {
        let (dir, hub) = temp_hub();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join(".atman")).unwrap();
        std::fs::write(dir.path().join("config.toml"), "not valid [").unwrap();
        std::fs::write(
            project.path().join(".atman/config.toml"),
            "[storage]\nscope = \"local\"\n",
        )
        .unwrap();
        assert_eq!(
            hub.storage_config(Some(project.path())).scope,
            Some(crate::storage::StorageScope::Local)
        );

        std::fs::write(
            dir.path().join("config.toml"),
            "[storage]\nscope = \"global\"\n",
        )
        .unwrap();
        std::fs::write(project.path().join(".atman/config.toml"), "not valid [").unwrap();
        assert_eq!(
            hub.storage_config(Some(project.path())).scope,
            Some(crate::storage::StorageScope::Global)
        );
    }

    #[test]
    fn storage_config_treats_read_errors_as_empty_layers() {
        let (dir, hub) = temp_hub();
        std::fs::create_dir(dir.path().join("config.toml")).unwrap();
        let project = tempfile::tempdir().unwrap();
        std::fs::create_dir(project.path().join(".atman")).unwrap();
        std::fs::write(
            project.path().join(".atman/config.toml"),
            "[storage]\nscope = \"local\"\n",
        )
        .unwrap();

        assert_eq!(
            hub.storage_config(Some(project.path())).scope,
            Some(crate::storage::StorageScope::Local)
        );
    }

    fn write_config(hub: &ConfigHub, text: &str) {
        std::fs::write(hub.config_toml_path(), text).unwrap();
    }

    #[test]
    fn append_dsl_route_creates_missing_file_and_escapes_trigger() {
        let (_dir, hub) = temp_hub();
        hub.append_dsl_route("review_code", "say \"hi\"\\now\n")
            .unwrap();

        let source = std::fs::read_to_string(hub.routes_at_path()).unwrap();
        let parsed = atman_dsl::parse::parse_file(&source).unwrap();
        assert_eq!(parsed.routes.len(), 1);
        assert_eq!(parsed.routes[0].pattern, "say \"hi\"\\now\n");
        assert_eq!(parsed.routes[0].flow.name, "review_code");
    }

    #[test]
    fn append_dsl_route_preserves_existing_source_exactly() {
        let (_dir, hub) = temp_hub();
        let original = "// keep this comment\nroute \"old \" { flow: old_flow }";
        std::fs::write(hub.routes_at_path(), original).unwrap();

        hub.append_dsl_route("new_flow", "new ").unwrap();

        assert_eq!(
            std::fs::read_to_string(hub.routes_at_path()).unwrap(),
            format!("{original}\nroute \"new \" {{ flow: new_flow }}\n")
        );
    }

    #[test]
    fn append_dsl_route_does_not_overwrite_invalid_existing_source() {
        let (_dir, hub) = temp_hub();
        let invalid = "route invalid";
        std::fs::write(hub.routes_at_path(), invalid).unwrap();

        let error = hub.append_dsl_route("new_flow", "new ").unwrap_err();

        assert!(error.to_string().contains("parse existing routes.at"));
        assert_eq!(
            std::fs::read_to_string(hub.routes_at_path()).unwrap(),
            invalid
        );
    }

    #[test]
    fn append_dsl_route_rejects_invalid_flow_without_writing() {
        let (_dir, hub) = temp_hub();
        let error = hub.append_dsl_route("bad-name", "new ").unwrap_err();
        assert!(error.to_string().contains("valid DSL identifier"));
        assert!(!hub.routes_at_path().exists());
    }

    #[test]
    fn concurrent_dsl_route_appends_do_not_lose_updates() {
        let (_dir, hub) = temp_hub();
        let mut workers = Vec::new();
        for index in 0..12 {
            let hub = hub.clone();
            workers.push(std::thread::spawn(move || {
                hub.append_dsl_route(&format!("flow_{index}"), &format!("{index} "))
                    .unwrap();
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }

        let source = std::fs::read_to_string(hub.routes_at_path()).unwrap();
        let parsed = atman_dsl::parse::parse_file(&source).unwrap();
        assert_eq!(parsed.routes.len(), 12);
        for index in 0..12 {
            assert!(parsed.routes.iter().any(|route| {
                route.flow.name == format!("flow_{index}") && route.pattern == format!("{index} ")
            }));
        }
        assert!(!std::fs::read_dir(hub.config_dir()).unwrap().any(|entry| {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            name.starts_with(".routes.at.") && name.ends_with(".tmp")
        }));
    }

    #[test]
    fn append_dsl_route_waits_for_external_file_lock() {
        use fs2::FileExt;
        use std::sync::mpsc::TryRecvError;

        let (_dir, hub) = temp_hub();
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(hub.config_dir().join(".routes.at.lock"))
            .unwrap();
        lock.lock_exclusive().unwrap();

        let worker_hub = hub.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            worker_hub.append_dsl_route("blocked", "wait ").unwrap();
            tx.send(()).unwrap();
        });
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        FileExt::unlock(&lock).unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn model_config_projection_handles_missing_valid_and_invalid_files() {
        let (_dir, hub) = temp_hub();
        assert!(hub.model_config().unwrap().is_none());

        write_config(
            &hub,
            "[providers.openai]\nkind = \"openai\"\n[models.fast]\nmodel = \"gpt-4o-mini\"\n[alias.default]\nmodel = \"fast\"\n",
        );
        let config = hub.model_config().unwrap().unwrap();
        assert_eq!(config.providers["openai"].kind, "openai");
        assert_eq!(config.models["fast"].model, "gpt-4o-mini");
        assert_eq!(config.aliases["default"].model, "fast");

        write_config(&hub, "[models]\n");
        assert!(hub.model_config().unwrap().is_none());

        write_config(&hub, "[models\n");
        assert!(hub.model_config().is_err());
    }

    #[test]
    fn config_crud_validates_model_semantics_before_disk_or_registry_changes() {
        let _registry = crate::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut current = crate::model_registry::ProviderConfig::default();
        current.models.insert(
            "current".into(),
            crate::model_registry::ModelEntry {
                model: "api/current".into(),
                ..Default::default()
            },
        );
        crate::model_registry::set_provider_config(current);

        let (_dir, hub) = temp_hub();
        let invalid = "[models.broken]\nmodel = \"api/broken\"\ncontext_budget = \"large\"\n";
        write_config(&hub, invalid);
        let before = std::fs::read(hub.config_toml_path()).unwrap();

        let error = hub.add_alias("smart", "current").unwrap_err();

        assert!(error.to_string().contains("parse config.toml"));
        assert_eq!(std::fs::read(hub.config_toml_path()).unwrap(), before);
        assert!(crate::model_registry::model_entry("current").is_some());
        assert!(crate::model_registry::model_entry("broken").is_none());
        crate::model_registry::set_provider_config(Default::default());
    }

    #[test]
    fn theme_preference_defaults_to_auto_when_config_is_missing() {
        let (_dir, hub) = temp_hub();

        assert_eq!(hub.theme_preference().unwrap(), ThemePreference::Auto);
    }

    #[test]
    fn math_rendering_defaults_on_and_accepts_boolean_override() {
        let (_dir, hub) = temp_hub();
        assert!(hub.math_rendering_enabled().unwrap());

        write_config(&hub, "[render]\nmath = false\n");
        assert!(!hub.math_rendering_enabled().unwrap());

        write_config(&hub, "[render]\nmath = \"false\"\n");
        assert!(hub.math_rendering_enabled().is_err());
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

    fn auth_provider(id: &str) -> crate::auth_store::StoredProvider {
        crate::auth_store::StoredProvider {
            id: id.into(),
            name: id.into(),
            kind: crate::auth_store::ProviderKind::Codex,
            access_token: "old-access".into(),
            refresh_token: Some("old-refresh".into()),
            expires_at: 1,
            account: Some("old-account".into()),
            enabled: true,
            model_cache: None,
        }
    }

    fn catalog_snapshot(
        hub: &ConfigHub,
        id: &str,
    ) -> crate::auth_store::AuthProviderCatalogSnapshot {
        hub.load_or_create_auth_provider_catalog_state(id)
            .unwrap()
            .unwrap()
            .1
    }

    fn credential_snapshot(
        hub: &ConfigHub,
        id: &str,
    ) -> crate::auth_store::AuthProviderCredentialSnapshot {
        hub.load_or_create_auth_provider_credential_state(id)
            .unwrap()
            .unwrap()
            .1
    }

    fn token_update(access_token: &str, refresh_token: &str) -> AuthTokenUpdate {
        AuthTokenUpdate {
            access_token: access_token.into(),
            refresh_token: Some(refresh_token.into()),
            expires_at: 99,
            account: Some("account@example.com".into()),
        }
    }

    #[test]
    fn auth_transactions_preserve_independent_concurrent_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let hub = ConfigHub::from_auth_path(&path);
        hub.add_auth_provider(auth_provider("provider")).unwrap();

        let cache_hub = hub.clone();
        let cache = std::thread::spawn(move || {
            cache_hub
                .update_auth_model_cache_details(
                    "provider",
                    "stable-provider",
                    10,
                    &[crate::provider::DiscoveredModelDetails {
                        slug: "cached-model".into(),
                        context_budget: Some(8192),
                        capability_knowledge: crate::provider::CapabilityKnowledge::Advertised(
                            crate::provider::ModelCapabilities::default(),
                        ),
                    }],
                )
                .unwrap();
        });
        let token_hub = hub.clone();
        let tokens = std::thread::spawn(move || {
            token_hub
                .update_auth_tokens(
                    "provider",
                    AuthTokenUpdate {
                        access_token: "new-access".into(),
                        refresh_token: Some("new-refresh".into()),
                        expires_at: 99,
                        account: None,
                    },
                )
                .unwrap();
        });
        let enabled_hub = hub.clone();
        let enabled = std::thread::spawn(move || {
            enabled_hub
                .set_auth_provider_enabled("provider", false)
                .unwrap();
        });
        cache.join().unwrap();
        tokens.join().unwrap();
        enabled.join().unwrap();

        let store = hub.load_auth().unwrap();
        let provider = &store.providers[0];
        assert_eq!(provider.access_token, "new-access");
        assert_eq!(provider.refresh_token.as_deref(), Some("new-refresh"));
        assert_eq!(provider.expires_at, 99);
        assert_eq!(provider.account.as_deref(), Some("old-account"));
        assert!(!provider.enabled);
        assert_eq!(
            provider.model_cache.as_ref().unwrap().models[0].slug,
            "cached-model"
        );
        assert!(matches!(
            hub.load_auth_model_cache_details("provider")
                .unwrap()
                .unwrap()[0]
                .capability_knowledge,
            crate::provider::CapabilityKnowledge::Advertised(_)
        ));
        assert_eq!(
            hub.load_auth_model_namespace("provider")
                .unwrap()
                .as_deref(),
            Some("stable-provider")
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        assert!(!std::fs::read_dir(dir.path()).unwrap().any(|entry| {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            name.starts_with(".auth.json.") && name.ends_with(".tmp")
        }));
    }

    #[test]
    fn auth_transaction_waits_for_external_file_lock() {
        use fs2::FileExt;
        use std::sync::mpsc::TryRecvError;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let lock_path = dir.path().join(".auth.json.lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)
            .unwrap();
        lock.lock_exclusive().unwrap();

        let hub = ConfigHub::from_auth_path(&path);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            hub.add_auth_provider(auth_provider("blocked")).unwrap();
            tx.send(()).unwrap();
        });
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        FileExt::unlock(&lock).unwrap();
        rx.recv_timeout(std::time::Duration::from_secs(2)).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn auth_transaction_error_rolls_back_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let hub = ConfigHub::from_auth_path(&path);
        hub.add_auth_provider(auth_provider("original")).unwrap();
        let before = std::fs::read(&path).unwrap();

        let result: Result<(), ConfigError> = hub.update_auth(|store| {
            store.providers.push(auth_provider("discarded"));
            Err(ConfigError::Invalid("reject mutation".into()))
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn auth_provider_ids_are_unique_and_duplicate_adds_do_not_write() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let hub = ConfigHub::from_auth_path(&path);
        hub.add_auth_provider(auth_provider("stable-id")).unwrap();
        let before = std::fs::read(&path).unwrap();

        let error = hub
            .add_auth_provider(auth_provider("stable-id"))
            .unwrap_err();

        assert!(error.to_string().contains("already exists"));
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert_eq!(hub.load_auth().unwrap().providers.len(), 1);
    }

    #[test]
    fn auth_provider_and_typed_model_cache_are_added_in_one_transaction() {
        let (_dir, hub) = temp_hub();
        let models = [crate::provider::DiscoveredModelDetails {
            slug: "gpt-test".into(),
            context_budget: Some(32_000),
            capability_knowledge: crate::provider::CapabilityKnowledge::Advertised(
                crate::provider::ModelCapabilities::default(),
            ),
        }];

        hub.add_auth_provider_with_model_cache_details(
            auth_provider("provider"),
            "provider@account",
            42,
            &models,
        )
        .unwrap();

        let stored = hub.load_auth().unwrap().providers.remove(0);
        assert_eq!(stored.model_cache.unwrap().fetched_at, 42);
        assert_eq!(
            hub.load_auth_model_namespace("provider")
                .unwrap()
                .as_deref(),
            Some("provider@account")
        );
        assert_eq!(
            hub.load_auth_model_cache_details("provider").unwrap(),
            Some(models.to_vec())
        );
    }

    #[test]
    fn conditional_auth_cache_update_rejects_stale_provider_state() {
        let (_dir, hub) = temp_hub();
        let initial = [crate::provider::DiscoveredModelDetails {
            slug: "initial".into(),
            context_budget: Some(8_192),
            capability_knowledge: crate::provider::CapabilityKnowledge::Legacy { thinking: false },
        }];
        hub.add_auth_provider_with_model_cache_details(
            auth_provider("provider"),
            "provider@account",
            1,
            &initial,
        )
        .unwrap();
        let catalog_snapshot = catalog_snapshot(&hub, "provider");
        hub.set_auth_provider_enabled("provider", false).unwrap();
        let before = std::fs::read(hub.auth_path.clone()).unwrap();

        let replacement = [crate::provider::DiscoveredModelDetails {
            slug: "replacement".into(),
            context_budget: Some(16_384),
            capability_knowledge: crate::provider::CapabilityKnowledge::Legacy { thinking: true },
        }];
        assert_eq!(
            hub.update_auth_model_cache_details_if_enabled(
                &auth_provider("provider"),
                &catalog_snapshot,
                "provider@account",
                2,
                &replacement,
            )
            .unwrap(),
            AuthModelCacheCommit::Disabled
        );
        assert_eq!(std::fs::read(hub.auth_path.clone()).unwrap(), before);
        assert_eq!(
            hub.load_auth_model_cache_details("provider").unwrap(),
            Some(initial.to_vec())
        );
    }

    #[test]
    fn catalog_revision_tracks_catalog_changes_but_not_credential_rotation() {
        let (_dir, hub) = temp_hub();
        hub.add_auth_provider(auth_provider("provider")).unwrap();
        let initial = catalog_snapshot(&hub, "provider");

        assert!(
            hub.update_auth_tokens(
                "provider",
                AuthTokenUpdate {
                    access_token: "rotated-access".into(),
                    refresh_token: Some("rotated-refresh".into()),
                    expires_at: 123,
                    account: Some("rotated@example.com".into()),
                },
            )
            .unwrap()
        );
        assert_eq!(catalog_snapshot(&hub, "provider"), initial);

        hub.set_auth_provider_enabled("provider", false).unwrap();
        let disabled = catalog_snapshot(&hub, "provider");
        assert_ne!(disabled, initial);
        hub.set_auth_provider_enabled("provider", true).unwrap();
        let enabled_again = catalog_snapshot(&hub, "provider");
        assert_ne!(enabled_again, disabled);
        assert_ne!(enabled_again, initial);
    }

    #[test]
    fn catalog_cache_update_does_not_invalidate_credential_snapshot() {
        let (_dir, hub) = temp_hub();
        hub.add_auth_provider(auth_provider("provider")).unwrap();
        let expected = credential_snapshot(&hub, "provider");

        assert!(
            hub.update_auth_model_cache_details(
                "provider",
                "provider@account",
                10,
                &[crate::provider::DiscoveredModelDetails {
                    slug: "cached-model".into(),
                    context_budget: Some(16_384),
                    capability_knowledge: crate::provider::CapabilityKnowledge::Advertised(
                        crate::provider::ModelCapabilities::default(),
                    ),
                }],
            )
            .unwrap()
        );
        assert_eq!(credential_snapshot(&hub, "provider"), expected);

        match hub
            .update_auth_tokens_if_current(
                "provider",
                &expected,
                token_update("fresh-access", "fresh-refresh"),
            )
            .unwrap()
        {
            crate::auth_store::AuthCredentialCommit::Updated { provider, .. } => {
                assert_eq!(provider.access_token, "fresh-access");
                assert_eq!(provider.refresh_token.as_deref(), Some("fresh-refresh"));
                assert_eq!(
                    provider.model_cache.as_ref().unwrap().models[0].slug,
                    "cached-model"
                );
            }
            other => panic!("expected updated credential commit, got {other:?}"),
        }
    }

    #[test]
    fn credential_snapshot_rejects_token_aba() {
        let (_dir, hub) = temp_hub();
        hub.add_auth_provider(auth_provider("provider")).unwrap();
        let original = credential_snapshot(&hub, "provider");

        assert!(
            hub.update_auth_tokens(
                "provider",
                token_update("intermediate-access", "intermediate-refresh"),
            )
            .unwrap()
        );
        assert!(
            hub.update_auth_tokens(
                "provider",
                AuthTokenUpdate {
                    access_token: "old-access".into(),
                    refresh_token: Some("old-refresh".into()),
                    expires_at: 1,
                    account: Some("old-account".into()),
                },
            )
            .unwrap()
        );
        assert_ne!(credential_snapshot(&hub, "provider"), original);
        let before = std::fs::read(hub.auth_path()).unwrap();

        assert!(matches!(
            hub.update_auth_tokens_if_current(
                "provider",
                &original,
                token_update("stale-access", "stale-refresh"),
            )
            .unwrap(),
            crate::auth_store::AuthCredentialCommit::Changed
        ));
        assert_eq!(std::fs::read(hub.auth_path()).unwrap(), before);
    }

    #[test]
    fn provider_toggle_aba_preserves_credential_snapshot_but_replacement_invalidates_it() {
        let (_dir, hub) = temp_hub();
        hub.add_auth_provider(auth_provider("provider")).unwrap();
        let before_toggle = credential_snapshot(&hub, "provider");

        assert!(hub.set_auth_provider_enabled("provider", false).unwrap());
        assert!(hub.set_auth_provider_enabled("provider", true).unwrap());
        assert_eq!(credential_snapshot(&hub, "provider"), before_toggle);
        let commit = hub
            .update_auth_tokens_if_current(
                "provider",
                &before_toggle,
                token_update("fresh-toggle-access", "fresh-toggle-refresh"),
            )
            .unwrap();
        assert!(matches!(
            commit,
            crate::auth_store::AuthCredentialCommit::Updated { .. }
        ));

        let before_replacement = credential_snapshot(&hub, "provider");
        assert!(hub.remove_auth_provider("provider").unwrap());
        hub.add_auth_provider(auth_provider("provider")).unwrap();
        assert_ne!(credential_snapshot(&hub, "provider"), before_replacement);
        assert!(matches!(
            hub.update_auth_tokens_if_current(
                "provider",
                &before_replacement,
                token_update("stale-replacement-access", "stale-replacement-refresh"),
            )
            .unwrap(),
            crate::auth_store::AuthCredentialCommit::Changed
        ));
    }

    #[test]
    fn credential_commit_persists_rotation_while_disabled_and_reports_missing_provider() {
        let (_dir, hub) = temp_hub();
        hub.add_auth_provider(auth_provider("provider")).unwrap();
        let expected = credential_snapshot(&hub, "provider");

        assert!(hub.set_auth_provider_enabled("provider", false).unwrap());
        assert_eq!(credential_snapshot(&hub, "provider"), expected);
        match hub
            .update_auth_tokens_if_current(
                "provider",
                &expected,
                token_update("disabled-access", "disabled-refresh"),
            )
            .unwrap()
        {
            crate::auth_store::AuthCredentialCommit::Updated { provider } => {
                assert!(!provider.enabled);
                assert_eq!(provider.access_token, "disabled-access");
                assert_eq!(provider.refresh_token.as_deref(), Some("disabled-refresh"));
            }
            other => panic!("expected disabled credential rotation to persist, got {other:?}"),
        }
        let persisted = hub.load_auth().unwrap().providers.remove(0);
        assert!(!persisted.enabled);
        assert_eq!(persisted.access_token, "disabled-access");
        assert_eq!(persisted.refresh_token.as_deref(), Some("disabled-refresh"));

        assert!(hub.remove_auth_provider("provider").unwrap());
        let before_missing = std::fs::read(hub.auth_path()).unwrap();
        assert!(matches!(
            hub.update_auth_tokens_if_current(
                "provider",
                &expected,
                token_update("missing-access", "missing-refresh"),
            )
            .unwrap(),
            crate::auth_store::AuthCredentialCommit::Missing
        ));
        assert_eq!(std::fs::read(hub.auth_path()).unwrap(), before_missing);
    }

    #[test]
    fn legacy_credential_revision_is_lazily_persisted_without_changing_public_auth_shape() {
        let (_dir, hub) = temp_hub();
        std::fs::write(
            hub.auth_path(),
            r#"{
                "providers": [{
                    "id": "legacy",
                    "name": "Legacy",
                    "kind": "custom",
                    "access_token": "access",
                    "refresh_token": "refresh",
                    "expires_at": 1,
                    "account": "account@example.com",
                    "enabled": true
                }]
            }"#,
        )
        .unwrap();
        let legacy = std::fs::read(hub.auth_path()).unwrap();

        let public_before = hub.load_auth().unwrap();
        assert_eq!(std::fs::read(hub.auth_path()).unwrap(), legacy);
        assert_eq!(public_before.providers[0].access_token, "access");

        let first = credential_snapshot(&hub, "legacy");
        let migrated = std::fs::read(hub.auth_path()).unwrap();
        assert_ne!(migrated, legacy);
        assert!(
            serde_json::from_slice::<serde_json::Value>(&migrated).unwrap()["providers"][0]
                .get("credential_revision")
                .is_some()
        );

        let peer = ConfigHub::from_auth_path(hub.auth_path());
        assert_eq!(credential_snapshot(&peer, "legacy"), first);
        assert_eq!(std::fs::read(hub.auth_path()).unwrap(), migrated);

        let public_after = peer.load_auth().unwrap();
        assert_eq!(public_after.providers[0].id, "legacy");
        assert_eq!(public_after.providers[0].access_token, "access");
        let public_json = serde_json::to_value(public_after).unwrap();
        assert!(
            public_json["providers"][0]
                .get("credential_revision")
                .is_none()
        );
        let parsed_legacy_view: crate::auth_store::AuthStore =
            serde_json::from_slice(&migrated).unwrap();
        assert_eq!(parsed_legacy_view.providers[0].id, "legacy");
    }

    #[test]
    fn legacy_catalog_revision_is_persisted_once_and_shared_by_hubs() {
        let (_dir, hub) = temp_hub();
        std::fs::write(
            hub.auth_path(),
            r#"{
                "providers": [{
                    "id": "legacy",
                    "name": "Legacy",
                    "kind": "codex",
                    "access_token": "access",
                    "expires_at": 1,
                    "enabled": true
                }]
            }"#,
        )
        .unwrap();
        let before = std::fs::read(hub.auth_path()).unwrap();

        let first = catalog_snapshot(&hub, "legacy");
        let migrated = std::fs::read(hub.auth_path()).unwrap();
        assert_ne!(migrated, before);
        let peer = ConfigHub::from_auth_path(hub.auth_path());
        assert_eq!(catalog_snapshot(&peer, "legacy"), first);
        assert_eq!(std::fs::read(hub.auth_path()).unwrap(), migrated);
    }

    #[test]
    fn catalog_revision_advances_for_equal_cache_commits_but_not_equal_enable_writes() {
        let (_dir, hub) = temp_hub();
        let models = [crate::provider::DiscoveredModelDetails {
            slug: "same".into(),
            context_budget: Some(8_192),
            capability_knowledge: crate::provider::CapabilityKnowledge::Legacy { thinking: false },
        }];
        hub.add_auth_provider_with_model_cache_details(
            auth_provider("provider"),
            "provider@account",
            1,
            &models,
        )
        .unwrap();
        let initial = catalog_snapshot(&hub, "provider");

        assert!(
            hub.update_auth_model_cache_details("provider", "provider@account", 1, &models,)
                .unwrap()
        );
        let refreshed = catalog_snapshot(&hub, "provider");
        assert_ne!(refreshed, initial);
        let before_equal_enable = std::fs::read(hub.auth_path()).unwrap();
        assert!(hub.set_auth_provider_enabled("provider", true).unwrap());
        assert_eq!(catalog_snapshot(&hub, "provider"), refreshed);
        assert_eq!(std::fs::read(hub.auth_path()).unwrap(), before_equal_enable);
    }

    #[test]
    fn removing_and_readding_the_same_provider_invalidates_old_catalog_snapshot() {
        let (_dir, hub) = temp_hub();
        let models = [crate::provider::DiscoveredModelDetails {
            slug: "same".into(),
            context_budget: Some(8_192),
            capability_knowledge: crate::provider::CapabilityKnowledge::Legacy { thinking: false },
        }];
        hub.add_auth_provider_with_model_cache_details(
            auth_provider("provider"),
            "provider@account",
            1,
            &models,
        )
        .unwrap();
        let original = catalog_snapshot(&hub, "provider");
        assert!(hub.remove_auth_provider("provider").unwrap());
        hub.add_auth_provider_with_model_cache_details(
            auth_provider("provider"),
            "provider@account",
            1,
            &models,
        )
        .unwrap();
        assert_ne!(catalog_snapshot(&hub, "provider"), original);
        let before = std::fs::read(hub.auth_path()).unwrap();

        assert_eq!(
            hub.update_auth_model_cache_details_if_enabled(
                &auth_provider("provider"),
                &original,
                "provider@account",
                2,
                &models,
            )
            .unwrap(),
            AuthModelCacheCommit::Changed
        );
        assert_eq!(std::fs::read(hub.auth_path()).unwrap(), before);
    }

    #[test]
    fn cache_write_and_follow_up_complete_before_a_peer_auth_write() {
        let (_dir, hub) = temp_hub();
        let initial = [crate::provider::DiscoveredModelDetails {
            slug: "initial".into(),
            context_budget: Some(8_192),
            capability_knowledge: crate::provider::CapabilityKnowledge::Legacy { thinking: false },
        }];
        hub.add_auth_provider_with_model_cache_details(
            auth_provider("provider"),
            "provider@account",
            1,
            &initial,
        )
        .unwrap();
        let expected = hub.load_auth().unwrap().providers.remove(0);
        let expected_catalog = catalog_snapshot(&hub, "provider");
        let replacement = vec![crate::provider::DiscoveredModelDetails {
            slug: "replacement".into(),
            context_budget: Some(16_384),
            capability_knowledge: crate::provider::CapabilityKnowledge::Legacy { thinking: true },
        }];
        let (follow_up_entered_tx, follow_up_entered_rx) = std::sync::mpsc::channel();
        let (release_follow_up_tx, release_follow_up_rx) = std::sync::mpsc::channel();
        let transaction_hub = hub.clone();
        let transaction = std::thread::spawn(move || {
            transaction_hub.update_auth_model_cache_details_if_enabled_and_then(
                AuthModelCacheUpdate {
                    expected: &expected,
                    expected_catalog: &expected_catalog,
                    expected_provider_ids: None,
                    model_namespace: "provider@account",
                    fetched_at: 2,
                    models: &replacement,
                },
                || {
                    assert_eq!(
                        transaction_hub
                            .load_auth_model_cache_details("provider")
                            .unwrap()
                            .unwrap()[0]
                            .slug,
                        "replacement"
                    );
                    follow_up_entered_tx.send(()).unwrap();
                    release_follow_up_rx.recv().unwrap();
                    "catalog-committed"
                },
            )
        });
        follow_up_entered_rx.recv().unwrap();

        let (peer_started_tx, peer_started_rx) = std::sync::mpsc::channel();
        let (peer_done_tx, peer_done_rx) = std::sync::mpsc::channel();
        let peer_hub = hub.clone();
        let peer = std::thread::spawn(move || {
            peer_started_tx.send(()).unwrap();
            peer_hub
                .update_auth_model_cache_details(
                    "provider",
                    "provider@account",
                    3,
                    &[crate::provider::DiscoveredModelDetails {
                        slug: "peer".into(),
                        context_budget: Some(32_768),
                        capability_knowledge: crate::provider::CapabilityKnowledge::Legacy {
                            thinking: false,
                        },
                    }],
                )
                .unwrap();
            peer_done_tx.send(()).unwrap();
        });
        peer_started_rx.recv().unwrap();
        assert!(
            peer_done_rx
                .recv_timeout(std::time::Duration::from_millis(50))
                .is_err()
        );

        release_follow_up_tx.send(()).unwrap();
        assert_eq!(
            transaction.join().unwrap().unwrap(),
            (AuthModelCacheCommit::Updated, Some("catalog-committed"))
        );
        peer_done_rx.recv().unwrap();
        peer.join().unwrap();
        assert_eq!(
            hub.load_auth_model_cache_details("provider")
                .unwrap()
                .unwrap()[0]
                .slug,
            "peer"
        );
    }

    #[test]
    fn assigning_a_model_namespace_does_not_refresh_an_existing_cache() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let hub = ConfigHub::from_auth_path(&path);
        hub.add_auth_provider(auth_provider("provider")).unwrap();
        assert!(
            hub.update_auth_model_cache(
                "provider",
                crate::auth_store::ModelCache {
                    fetched_at: 7,
                    models: vec![],
                },
            )
            .unwrap()
        );

        hub.ensure_auth_model_namespace("provider", "stable-provider")
            .unwrap();

        let provider = hub.load_auth().unwrap().providers.remove(0);
        assert_eq!(provider.model_cache.unwrap().fetched_at, 7);
        assert_eq!(
            hub.load_auth_model_namespace("provider")
                .unwrap()
                .as_deref(),
            Some("stable-provider")
        );
        let before = std::fs::read(&path).unwrap();
        assert!(
            hub.ensure_auth_model_namespace("provider", "changed")
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn auth_transaction_does_not_overwrite_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let corrupt = b"{not-json";
        std::fs::write(&path, corrupt).unwrap();
        let hub = ConfigHub::from_auth_path(&path);

        let err = hub.add_auth_provider(auth_provider("new")).unwrap_err();
        assert!(err.to_string().contains("parse"));
        assert_eq!(std::fs::read(&path).unwrap(), corrupt);
    }

    #[test]
    fn auth_load_defaults_when_file_is_missing() {
        let dir = tempfile::tempdir().unwrap();
        let hub = ConfigHub::from_auth_path(dir.path().join("auth.json"));
        assert!(hub.load_auth().unwrap().providers.is_empty());
    }

    #[test]
    fn daemon_config_initializes_reuses_and_rotates_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("daemon.toml");
        let hub = ConfigHub::from_daemon_config_path(&path);

        let first = hub.load_or_init_daemon_config().unwrap();
        assert_eq!(first.auth_token.len(), 64);
        assert!(first.auth_token.chars().all(|c| c.is_ascii_hexdigit()));
        let second = hub.load_or_init_daemon_config().unwrap();
        assert_eq!(second, first);
        assert!(!std::fs::read_dir(dir.path()).unwrap().any(|entry| {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            name.starts_with(".daemon.toml.") && name.ends_with(".tmp")
        }));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        let rotated = hub.rotate_daemon_config().unwrap();
        assert_ne!(rotated.auth_token, first.auth_token);
        assert_eq!(hub.load_or_init_daemon_config().unwrap(), rotated);
        assert!(!std::fs::read_dir(dir.path()).unwrap().any(|entry| {
            let name = entry.unwrap().file_name();
            let name = name.to_string_lossy();
            name.starts_with(".daemon.toml.") && name.ends_with(".tmp")
        }));
    }

    #[test]
    fn daemon_config_waits_for_external_file_lock() {
        use std::sync::mpsc::TryRecvError;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("custom-daemon.toml");
        let lock = lock_file(&lock_path_for(&path)).unwrap();
        let hub = ConfigHub::from_daemon_config_path(&path);
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            tx.send(hub.load_or_init_daemon_config()).unwrap();
        });

        std::thread::sleep(std::time::Duration::from_millis(25));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        lock.unlock().unwrap();
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(1))
                .unwrap()
                .is_ok()
        );
        worker.join().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn legacy_daemon_config_uses_custom_path_and_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let config = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let daemon_path = config.path().join("daemon/custom.toml");
        std::fs::write(data.path().join("daemon.toml"), "auth_token = \"legacy\"\n").unwrap();

        let report = ConfigHub::from_config_dir(config.path())
            .with_daemon_config_path(&daemon_path)
            .migrate_legacy_layout(data.path())
            .unwrap()
            .unwrap();

        assert!(report.moved.iter().any(|path| path == "daemon.toml"));
        assert_eq!(
            std::fs::read_to_string(&daemon_path).unwrap(),
            "auth_token = \"legacy\"\n"
        );
        assert!(!config.path().join("daemon.toml").exists());
        assert_eq!(
            std::fs::metadata(&daemon_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn daemon_config_rotation_requires_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.toml");
        let err = ConfigHub::from_daemon_config_path(&path)
            .rotate_daemon_config()
            .unwrap_err();
        assert!(err.to_string().contains("no daemon config"));
    }

    #[test]
    fn web_configs_default_when_config_or_section_is_missing() {
        for text in [None, Some("[theme]\nmode = \"dark\"\n")] {
            let (_dir, hub) = temp_hub();
            if let Some(text) = text {
                write_config(&hub, text);
            }

            let fetch = hub.web_fetch_config().unwrap();
            assert_eq!(fetch.max_bytes, 1_000_000);
            assert!(fetch.url_allowlist.is_empty());
            assert!(fetch.url_denylist.is_empty());
            let search = hub.web_search_config().unwrap();
            assert_eq!(search.provider_name(), "tavily");
        }
    }

    #[test]
    fn web_configs_parse_fetch_and_search_fields() {
        let (_dir, hub) = temp_hub();
        write_config(
            &hub,
            r#"
[web]
max_bytes = 4096
url_allowlist = ["https://ok.example"]
url_denylist = ["https://ok.example/private"]

[web.search]
provider = "searxng"
base_url = "http://localhost:8080"
max_results = 6
"#,
        );

        let fetch = hub.web_fetch_config().unwrap();
        assert_eq!(fetch.max_bytes, 4096);
        assert_eq!(fetch.url_allowlist, vec!["https://ok.example"]);
        assert_eq!(fetch.url_denylist, vec!["https://ok.example/private"]);
        assert_eq!(hub.web_search_config().unwrap().provider_name(), "searxng");
    }

    #[test]
    fn web_fetch_schema_error_does_not_break_valid_search() {
        let (_dir, hub) = temp_hub();
        write_config(
            &hub,
            "[web]\nmax_bytes = \"large\"\n[web.search]\nprovider = \"none\"\n",
        );

        assert!(matches!(
            hub.web_fetch_config(),
            Err(ConfigError::Invalid(_))
        ));
        assert_eq!(hub.web_search_config().unwrap().provider_name(), "none");
    }

    #[test]
    fn web_search_schema_error_does_not_break_valid_fetch() {
        let (_dir, hub) = temp_hub();
        write_config(
            &hub,
            "[web]\nmax_bytes = 2048\n[web.search]\nprovider = \"unknown\"\n",
        );

        assert_eq!(hub.web_fetch_config().unwrap().max_bytes, 2048);
        assert!(matches!(
            hub.web_search_config(),
            Err(ConfigError::Invalid(_))
        ));
    }

    #[test]
    fn trust_config_defaults_when_config_or_section_is_missing() {
        for text in [None, Some("[theme]\nmode = \"dark\"\n")] {
            let (_dir, hub) = temp_hub();
            if let Some(text) = text {
                write_config(&hub, text);
            }

            let config = hub.trust_config().unwrap();
            assert_eq!(config.mode, crate::trust::TrustMode::Steady);
            assert_eq!(config.theme, crate::trust::Theme::Default);
            assert_eq!(config.escalation, crate::trust::EscalationPolicy::Ask);
        }
    }

    #[test]
    fn trust_config_parses_mode_theme_and_escalation() {
        let (_dir, hub) = temp_hub();
        write_config(
            &hub,
            "[trust]\nmode = \"eager\"\ntheme = \"weather\"\nescalation = \"deny\"\n",
        );

        let config = hub.trust_config().unwrap();
        assert_eq!(config.mode, crate::trust::TrustMode::Eager);
        assert_eq!(config.theme, crate::trust::Theme::Weather);
        assert_eq!(config.escalation, crate::trust::EscalationPolicy::Deny);
    }

    #[test]
    fn trust_config_parses_new_policy() {
        use crate::tool::Tier;
        use crate::trust::{EscalationPolicy, PolicyAction, RiskKind};

        let (_dir, hub) = temp_hub();
        write_config(
            &hub,
            "[trust]\nmode = \"eager\"\nescalation = \"allow\"\n\
             [trust.tiers.eager]\ntier4 = \"deny\"\n\
             [trust.risks.eager]\nnetwork = \"deny\"\nfilesystem_write = \"auto\"\noutside_workspace = \"auto\"\n",
        );

        let config = hub.trust_config().unwrap();
        assert_eq!(config.escalation, EscalationPolicy::Allow);
        assert_eq!(config.resolve_tier(Tier::Four), PolicyAction::Deny);
        assert_eq!(config.resolve_risk(RiskKind::Network), PolicyAction::Deny);
        assert_eq!(
            config.resolve_risk(RiskKind::WorkspaceExternal),
            PolicyAction::Auto
        );
        assert_eq!(
            config.resolve_risk(RiskKind::FilesystemWrite),
            PolicyAction::Auto
        );
        assert_eq!(config.resolve_policy(Tier::Four, []), PolicyAction::Deny);
        assert_eq!(
            config.resolve_policy(Tier::Zero, [RiskKind::Network]),
            PolicyAction::Deny
        );
    }

    #[test]
    fn config_hub_rejects_obsolete_trust_outside() {
        let (_dir, hub) = temp_hub();
        write_config(&hub, "[trust]\noutside = \"allow\"\n");

        assert!(matches!(
            hub.trust_config(),
            Err(ConfigError::Invalid(message))
                if message.contains("parse trust config") && message.contains("outside")
        ));
    }

    #[test]
    fn config_hub_rejects_obsolete_nested_trust_risk() {
        let (_dir, hub) = temp_hub();
        write_config(
            &hub,
            "[trust.risks.eager]\nsandbox_violation = \"deny\"\noutside_workspace = \"deny\"\n",
        );

        assert!(matches!(
            hub.trust_config(),
            Err(ConfigError::Invalid(message))
                if message.contains("parse trust config")
                    && message.contains("sandbox_violation")
        ));
    }

    #[test]
    fn trust_config_rejects_invalid_enum() {
        let (_dir, hub) = temp_hub();
        write_config(&hub, "[trust]\nescalation = \"sometimes\"\n");

        assert!(matches!(
            hub.trust_config(),
            Err(ConfigError::Invalid(message)) if message.contains("parse trust config")
        ));
    }

    #[test]
    fn preview_config_defaults_when_config_or_section_is_missing() {
        for text in [None, Some("[theme]\nmode = \"dark\"\n")] {
            let (_dir, hub) = temp_hub();
            if let Some(text) = text {
                write_config(&hub, text);
            }

            let config = hub.preview_config().unwrap();
            let expected = crate::tools::preview::PreviewConfig::default();
            assert_eq!(config.base_url, expected.base_url);
            assert_eq!(config.timeout_ms, expected.timeout_ms);
            assert_eq!(config.project_abs_path, expected.project_abs_path);
            assert_eq!(config.project_hint_slug, expected.project_hint_slug);
            assert_eq!(config.max_body_bytes, expected.max_body_bytes);
        }
    }

    #[test]
    fn preview_config_parses_all_supported_fields() {
        let (_dir, hub) = temp_hub();
        write_config(
            &hub,
            r#"
[preview]
base_url = "http://127.0.0.1:9000"
timeout_ms = 4500
project_abs_path = "/tmp/project"
project_hint_slug = "project"
max_body_bytes = 2048
"#,
        );

        let config = hub.preview_config().unwrap();
        assert_eq!(config.base_url, "http://127.0.0.1:9000");
        assert_eq!(config.timeout_ms, 4500);
        assert_eq!(config.project_abs_path, "/tmp/project");
        assert_eq!(config.project_hint_slug.as_deref(), Some("project"));
        assert_eq!(config.max_body_bytes, 2048);
    }

    #[test]
    fn preview_config_rejects_invalid_schema() {
        let (_dir, hub) = temp_hub();
        write_config(&hub, "[preview]\ntimeout_ms = \"slow\"\n");

        assert!(matches!(
            hub.preview_config(),
            Err(ConfigError::Invalid(message)) if message.contains("parse preview config")
        ));
    }

    #[test]
    fn sandbox_config_defaults_when_config_or_section_is_missing() {
        for text in [None, Some("[theme]\nmode = \"dark\"\n")] {
            let (_dir, hub) = temp_hub();
            if let Some(text) = text {
                write_config(&hub, text);
            }

            assert_eq!(hub.sandbox_config().unwrap(), SandboxConfig::default());
        }
    }

    #[test]
    fn sandbox_config_preserves_paths_and_defaults_missing_enabled() {
        let (_dir, hub) = temp_hub();
        write_config(
            &hub,
            r#"
[sandbox]
strict = true
extra_read = ["../read"]
extra_write = ["/tmp/write"]
template_path = "profiles/custom.sb"
allow_network = true
"#,
        );

        assert_eq!(
            hub.sandbox_config().unwrap(),
            SandboxConfig {
                enabled: true,
                strict: true,
                extra_read: vec![PathBuf::from("../read")],
                extra_write: vec![PathBuf::from("/tmp/write")],
                template_path: Some(PathBuf::from("profiles/custom.sb")),
                allow_network: true,
            }
        );
    }

    #[test]
    fn sandbox_config_allows_explicit_opt_out() {
        let (_dir, hub) = temp_hub();
        write_config(&hub, "[sandbox]\nenabled = false\n");

        assert!(!hub.sandbox_config().unwrap().enabled);
    }

    #[test]
    fn sandbox_config_rejects_invalid_schema() {
        let (_dir, hub) = temp_hub();
        write_config(&hub, "[sandbox]\nextra_read = \"/tmp\"\n");

        assert!(matches!(
            hub.sandbox_config(),
            Err(ConfigError::Invalid(message)) if message.contains("parse sandbox config")
        ));
    }

    #[test]
    fn redact_config_defaults_when_config_is_missing_or_section_is_missing() {
        for text in [None, Some("[theme]\nmode = \"dark\"\n")] {
            let (_dir, hub) = temp_hub();
            if let Some(text) = text {
                write_config(&hub, text);
            }

            assert_eq!(hub.redact_config().unwrap(), RedactConfig::default());
        }
    }

    #[test]
    fn redact_config_parses_mode_patterns_and_allowlist() {
        let (_dir, hub) = temp_hub();
        write_config(
            &hub,
            r#"
[redact]
enabled = true
mode = "partial"
allowlist = ["safe@example.com"]
custom_patterns = [{ kind = "ticket", regex = "T-[0-9]+" }]
"#,
        );

        assert_eq!(
            hub.redact_config().unwrap(),
            RedactConfig {
                enabled: true,
                partial: true,
                allowlist: vec!["safe@example.com".into()],
                custom_patterns: vec![("ticket".into(), "T-[0-9]+".into())],
            }
        );
    }

    #[test]
    fn redact_config_treats_unknown_mode_as_full() {
        let (_dir, hub) = temp_hub();
        write_config(&hub, "[redact]\nenabled = true\nmode = \"unknown\"\n");

        let config = hub.redact_config().unwrap();
        assert!(config.enabled);
        assert!(!config.partial);
    }

    #[test]
    fn redact_config_rejects_invalid_schema() {
        let (_dir, hub) = temp_hub();
        write_config(&hub, "[redact]\nenabled = \"yes\"\n");

        assert!(matches!(
            hub.redact_config(),
            Err(ConfigError::Invalid(message)) if message.contains("parse redact config")
        ));
    }

    #[test]
    fn interjection_mode_defaults_to_none_when_config_or_value_is_missing() {
        for text in [
            None,
            Some("[theme]\nmode = \"dark\"\n"),
            Some("[interjection]\n"),
        ] {
            let (_dir, hub) = temp_hub();
            if let Some(text) = text {
                write_config(&hub, text);
            }

            assert_eq!(hub.interjection_mode().unwrap(), None);
        }
    }

    #[test]
    fn interjection_mode_parses_supported_and_unknown_values() {
        for (value, expected) in [
            ("off", InterjectionMode::Off),
            ("rule", InterjectionMode::Rule),
            ("llm", InterjectionMode::Llm),
            ("custom", InterjectionMode::Unknown("custom".into())),
        ] {
            let (_dir, hub) = temp_hub();
            write_config(&hub, &format!("[interjection]\nclassifier = {value:?}\n"));

            assert_eq!(hub.interjection_mode().unwrap(), Some(expected));
        }
    }

    #[test]
    fn interjection_mode_rejects_non_string_value() {
        let (_dir, hub) = temp_hub();
        write_config(&hub, "[interjection]\nclassifier = true\n");

        assert!(matches!(
            hub.interjection_mode(),
            Err(ConfigError::Invalid(message)) if message.contains("interjection.classifier")
        ));
    }

    #[test]
    fn suggest_model_defaults_to_none_when_config_or_value_is_missing() {
        for text in [
            None,
            Some("[theme]\nmode = \"dark\"\n"),
            Some("[suggest]\n"),
        ] {
            let (_dir, hub) = temp_hub();
            if let Some(text) = text {
                write_config(&hub, text);
            }

            assert_eq!(hub.suggest_model().unwrap(), None);
        }
    }

    #[test]
    fn suggest_model_returns_configured_string_including_empty() {
        for value in ["smart", ""] {
            let (_dir, hub) = temp_hub();
            write_config(&hub, &format!("[suggest]\nmodel = {value:?}\n"));

            assert_eq!(hub.suggest_model().unwrap().as_deref(), Some(value));
        }
    }

    #[test]
    fn suggest_model_rejects_non_string_value() {
        let (_dir, hub) = temp_hub();
        write_config(&hub, "[suggest]\nmodel = 42\n");

        assert!(matches!(
            hub.suggest_model(),
            Err(ConfigError::Invalid(message)) if message.contains("suggest.model")
        ));
    }

    #[test]
    fn compact_review_mode_defaults_to_none_when_config_or_value_is_missing() {
        for text in [
            None,
            Some("[theme]\nmode = \"dark\"\n"),
            Some("[compaction]\n"),
        ] {
            let (_dir, hub) = temp_hub();
            if let Some(text) = text {
                write_config(&hub, text);
            }

            assert_eq!(hub.compact_review_mode().unwrap(), None);
        }
    }

    #[test]
    fn compact_review_mode_parses_supported_values() {
        for (value, expected) in [
            ("always", crate::CompactReviewMode::Always),
            ("manual-only", crate::CompactReviewMode::ManualOnly),
            ("manual_only", crate::CompactReviewMode::ManualOnly),
            ("never", crate::CompactReviewMode::Never),
        ] {
            let (_dir, hub) = temp_hub();
            write_config(&hub, &format!("[compaction]\nreview = {value:?}\n"));

            assert_eq!(hub.compact_review_mode().unwrap(), Some(expected));
        }
    }

    #[test]
    fn compact_review_mode_rejects_unknown_or_non_string_value() {
        for value in ["\"sometimes\"", "true"] {
            let (_dir, hub) = temp_hub();
            write_config(&hub, &format!("[compaction]\nreview = {value}\n"));

            assert!(matches!(
                hub.compact_review_mode(),
                Err(ConfigError::Invalid(message)) if message.contains("compaction.review")
            ));
        }
    }

    #[test]
    fn auto_snapshot_defaults_to_none_when_config_or_value_is_missing() {
        for text in [
            None,
            Some("[theme]\nmode = \"dark\"\n"),
            Some("[registry]\n"),
        ] {
            let (_dir, hub) = temp_hub();
            if let Some(text) = text {
                write_config(&hub, text);
            }

            assert_eq!(hub.auto_snapshot().unwrap(), None);
        }
    }

    #[test]
    fn auto_snapshot_reads_boolean_values() {
        for value in [true, false] {
            let (_dir, hub) = temp_hub();
            write_config(&hub, &format!("[registry]\nauto_snapshot = {value}\n"));

            assert_eq!(hub.auto_snapshot().unwrap(), Some(value));
        }
    }

    #[test]
    fn auto_snapshot_reads_integer_values() {
        for (value, expected) in [(1, true), (0, false)] {
            let (_dir, hub) = temp_hub();
            write_config(&hub, &format!("[registry]\nauto_snapshot = {value}\n"));

            assert_eq!(hub.auto_snapshot().unwrap(), Some(expected));
        }
    }

    #[test]
    fn auto_snapshot_only_enables_exact_true_string() {
        for (value, expected) in [("true", true), ("yes", false)] {
            let (_dir, hub) = temp_hub();
            write_config(&hub, &format!("[registry]\nauto_snapshot = {value:?}\n"));

            assert_eq!(hub.auto_snapshot().unwrap(), Some(expected));
        }
    }

    #[test]
    fn auto_snapshot_rejects_unsupported_type() {
        let (_dir, hub) = temp_hub();
        write_config(&hub, "[registry]\nauto_snapshot = [true]\n");

        assert!(matches!(
            hub.auto_snapshot(),
            Err(ConfigError::Invalid(message)) if message.contains("registry.auto_snapshot")
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
            reasoning: crate::provider::ReasoningSelection::Disabled,
            capabilities: None,
            image_detail: None,
            max_tokens: None,
            enabled: true,
        }
    }

    #[test]
    fn model_migration_preserves_existing_provider_name() {
        let (_dir, hub) = temp_hub();
        write_config(
            &hub,
            r#"[providers.openai]
kind = "openai"
api_key = "existing"

[models.legacy]
model = "gpt"
provider = "openai"
api_key = "legacy"
"#,
        );

        let outcome = hub.migrate_model_config_if_needed().unwrap();
        assert!(matches!(
            outcome,
            crate::model_registry::ModelMigrationOutcome::Migrated { .. }
        ));
        let text = hub.read_config_toml().unwrap();
        assert!(text.contains("[providers.openai]"));
        assert!(text.contains("api_key = \"existing\""));
        assert!(text.contains("[providers.openai-2]"));
        assert!(text.contains("provider = \"openai-2\""));
    }

    #[test]
    fn model_migration_preserves_unversioned_provider_reference() {
        let (dir, hub) = temp_hub();
        let text = r#"[providers.openai]
kind = "openai"
api_key = "existing"

[models.current]
model = "gpt"
provider = "openai"
"#;
        write_config(&hub, text);

        assert_eq!(
            hub.migrate_model_config_if_needed().unwrap(),
            crate::model_registry::ModelMigrationOutcome::NotNeeded
        );
        assert_eq!(hub.read_config_toml().unwrap(), text);
        assert!(!dir.path().join("config.toml.bak").exists());
    }

    #[test]
    fn model_migration_rejects_invalid_and_future_versions() {
        for version in ["\"2\"", "3"] {
            let (_dir, hub) = temp_hub();
            let text = format!(
                "config_version = {version}\n[models.legacy]\nmodel = \"gpt\"\nprovider = \"openai\"\n"
            );
            write_config(&hub, &text);
            assert!(hub.migrate_model_config_if_needed().is_err());
            assert_eq!(hub.read_config_toml().unwrap(), text);
        }
    }

    #[test]
    fn model_migration_backup_conflict_preserves_source() {
        let (dir, hub) = temp_hub();
        let text = "[models.legacy]\nmodel = \"gpt\"\nprovider = \"openai\"\n";
        write_config(&hub, text);
        std::fs::write(dir.path().join("config.toml.bak"), "older backup").unwrap();

        assert!(matches!(
            hub.migrate_model_config_if_needed(),
            Err(ConfigError::Invalid(message)) if message.contains("backup conflict")
        ));
        assert_eq!(hub.read_config_toml().unwrap(), text);
    }

    #[cfg(unix)]
    #[test]
    fn model_migration_backup_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let (dir, hub) = temp_hub();
        write_config(
            &hub,
            "[models.legacy]\nmodel = \"gpt\"\nprovider = \"openai\"\napi_key = \"secret\"\n",
        );

        hub.migrate_model_config_if_needed().unwrap();

        let mode = std::fs::metadata(dir.path().join("config.toml.bak"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
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
    fn model_delete_rejects_alias_dependents_without_writing() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK.lock().unwrap();
        let (_dir, hub) = temp_hub();
        hub.upsert_model(model(None, "chat", "provider/chat"))
            .unwrap();
        hub.add_alias("fast", "chat").unwrap();
        let before = hub.read_config_toml().unwrap();

        let error = hub.remove_model("chat").unwrap_err();

        assert!(matches!(
            error,
            ConfigError::Invalid(message) if message.contains("referenced by aliases: fast")
        ));
        assert_eq!(hub.read_config_toml().unwrap(), before);
    }

    #[test]
    fn provider_delete_rejects_model_dependents_without_writing() {
        let _registry_lock = crate::model_registry::MODEL_CONFIG_LOCK.lock().unwrap();
        let (_dir, hub) = temp_hub();
        write_config(
            &hub,
            "[providers.gateway]\nkind = \"openai-compatible\"\n\n[models.chat]\nmodel = \"chat\"\nprovider = \"gateway\"\n",
        );
        let before = hub.read_config_toml().unwrap();

        let error = hub.remove_provider("gateway").unwrap_err();

        assert!(matches!(
            error,
            ConfigError::Invalid(message) if message.contains("referenced by models: chat")
        ));
        assert_eq!(hub.read_config_toml().unwrap(), before);
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
    fn mcp_replace_renames_atomically_and_rejects_conflicts() {
        let (_dir, hub) = temp_hub();
        let mut first = crate::mcp::McpServerConfig::http(
            "first",
            "https://old.example",
            Some("secret".into()),
            crate::tool::Tier::Three,
            45_000,
        );
        first.headers = vec![("X-Test".into(), "value".into())];
        first.disabled = true;
        hub.save_mcp(&[
            first.clone(),
            crate::mcp::McpServerConfig::stdio(
                "second",
                "echo",
                vec![],
                crate::tool::Tier::Two,
                30_000,
            ),
        ])
        .unwrap();

        first.name = "renamed".into();
        first.url = Some("https://new.example".into());
        hub.replace_mcp("first", first).unwrap();

        let configs = hub.load_local_mcp();
        let renamed = configs
            .iter()
            .find(|config| config.name == "renamed")
            .unwrap();
        assert_eq!(renamed.auth_token.as_deref(), Some("secret"));
        assert_eq!(renamed.headers, [("X-Test".into(), "value".into())]);
        assert_eq!(renamed.timeout_ms, 45_000);
        assert!(renamed.disabled);
        assert!(configs.iter().all(|config| config.name != "first"));
        assert!(configs.iter().any(|config| config.name == "second"));

        let before = std::fs::read_to_string(hub.mcp_json_path()).unwrap();
        let conflict = crate::mcp::McpServerConfig::stdio(
            "second",
            "false",
            vec![],
            crate::tool::Tier::One,
            1,
        );
        assert!(matches!(
            hub.replace_mcp("renamed", conflict),
            Err(ConfigError::NameConflict {
                domain: "MCP servers",
                ..
            })
        ));
        assert_eq!(
            std::fs::read_to_string(hub.mcp_json_path()).unwrap(),
            before
        );
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
