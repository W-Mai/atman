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
    pub enabled: bool,
}

pub struct AuthTokenUpdate {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: i64,
    pub account: Option<String>,
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

    pub fn update_auth<T>(
        &self,
        mutate: impl FnOnce(&mut crate::auth_store::AuthStore) -> Result<T, ConfigError>,
    ) -> Result<T, ConfigError> {
        use fs2::FileExt;

        let _guard = AUTH_WRITE_LOCK.lock().unwrap();
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
        let mut store = load_auth_from_path(&self.auth_path)?;
        let result = mutate(&mut store)?;
        self.write_auth(&store)?;
        Ok(result)
    }

    pub fn add_auth_provider(
        &self,
        provider: crate::auth_store::StoredProvider,
    ) -> Result<(), ConfigError> {
        self.update_auth(|store| {
            store.providers.push(provider);
            Ok(())
        })
    }

    pub fn remove_auth_provider(&self, id: &str) -> Result<bool, ConfigError> {
        self.update_auth(|store| Ok(store.remove(id)))
    }

    pub fn set_auth_provider_enabled(&self, id: &str, enabled: bool) -> Result<bool, ConfigError> {
        self.update_auth(|store| {
            let Some(provider) = store
                .providers
                .iter_mut()
                .find(|provider| provider.id == id)
            else {
                return Ok(false);
            };
            provider.enabled = enabled;
            Ok(true)
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

    pub fn update_auth_model_cache(
        &self,
        id: &str,
        cache: crate::auth_store::ModelCache,
    ) -> Result<bool, ConfigError> {
        self.update_auth(|store| Ok(store.update_model_cache(id, cache)))
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

    fn update_config_toml(
        &self,
        mutate: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<(), ConfigError>,
    ) -> Result<(), ConfigError> {
        let _guard = CONFIG_WRITE_LOCK.lock().unwrap();
        let _file_lock = self.lock_config_file()?;
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

    fn write_auth(&self, store: &crate::auth_store::AuthStore) -> Result<(), ConfigError> {
        let json = serde_json::to_vec_pretty(store)
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
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|error| ConfigError::Invalid(format!("parse {}: {error}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(crate::auth_store::AuthStore::default())
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

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn auth_transactions_preserve_independent_concurrent_updates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let hub = ConfigHub::from_auth_path(&path);
        hub.add_auth_provider(auth_provider("provider")).unwrap();

        let cache_hub = hub.clone();
        let cache = std::thread::spawn(move || {
            cache_hub
                .update_auth_model_cache(
                    "provider",
                    crate::auth_store::ModelCache {
                        fetched_at: 10,
                        models: vec![crate::auth_store::CachedModel {
                            slug: "cached-model".into(),
                            context_budget: Some(8192),
                            thinking: true,
                        }],
                    },
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
            thinking: false,
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
