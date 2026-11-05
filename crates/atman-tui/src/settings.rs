//! Configuration identity retained by a view and its settings panels.

use std::sync::Arc;

use atman_runtime::{auth_store::AuthStore, config_hub::ConfigHub, model_registry::ModelRegistry};

#[derive(Clone, Debug)]
pub enum SettingsMutation {
    ProjectStorage {
        scope: atman_runtime::storage::StorageScope,
    },
    MathRendering {
        enabled: bool,
    },
    CreateAlias {
        name: String,
        model: String,
    },
    UpdateAlias {
        original: Option<String>,
        name: String,
        model: String,
    },
    RemoveAlias {
        name: String,
    },
    DefaultModel {
        model: String,
    },
    SaveMcp {
        original: Option<String>,
        config: Box<atman_runtime::mcp::McpServerConfig>,
    },
    RemoveMcp {
        name: String,
    },
    ToggleMcp {
        name: String,
    },
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Preference {
    ProjectStorage,
    MathRendering,
}

impl Preference {
    fn selection(
        self,
        config: &ConfigHub,
        project: Option<&str>,
    ) -> Result<(atman_runtime::form::FormKind, usize), String> {
        use atman_runtime::{form::FormKind, storage::StorageScope};
        let (prompt, options, selected) = match self {
            Self::ProjectStorage => {
                let root = project.ok_or("Current session has no project")?;
                let scope = config
                    .try_storage_config(Some(std::path::Path::new(root)))
                    .map_err(|error| error.to_string())?
                    .scope
                    .unwrap_or_default();
                (
                    format!("Store Atman data for {root} where? Applies to new sessions."),
                    vec!["Global".into(), "Local (.atman)".into()],
                    usize::from(scope == StorageScope::Local),
                )
            }
            Self::MathRendering => (
                "Formula display. Restart Atman to apply changes.".into(),
                vec!["Rendered".into(), "Raw Markdown".into()],
                usize::from(
                    !config
                        .math_rendering_enabled()
                        .map_err(|error| error.to_string())?,
                ),
            ),
        };
        Ok((FormKind::SingleSelect { prompt, options }, selected))
    }

    pub(crate) fn mutation(self, index: usize) -> Result<SettingsMutation, String> {
        use atman_runtime::storage::StorageScope;
        match (self, index) {
            (Self::ProjectStorage, 0) => Ok(SettingsMutation::ProjectStorage {
                scope: StorageScope::Global,
            }),
            (Self::ProjectStorage, 1) => Ok(SettingsMutation::ProjectStorage {
                scope: StorageScope::Local,
            }),
            (Self::MathRendering, 0..=1) => Ok(SettingsMutation::MathRendering {
                enabled: index == 0,
            }),
            _ => Err("Invalid setting selection".into()),
        }
    }
}

#[derive(Clone, Debug)]
pub struct SettingsMutationRequest {
    pub id: uuid::Uuid,
    pub action: SettingsMutation,
}

impl SettingsMutationRequest {
    pub(crate) fn send(
        action: SettingsMutation,
        tx: Option<&crate::ControlSender>,
    ) -> Result<Self, String> {
        let request = Self {
            id: uuid::Uuid::now_v7(),
            action,
        };
        tx.ok_or("Settings host is disconnected")?
            .send(crate::TuiControl::MutateSettings(request.clone()))
            .map_err(|_| "Settings host is disconnected")?;
        Ok(request)
    }
}

mod preference;
mod projection;
pub(crate) use preference::PreferenceRead;

#[derive(Clone)]
/// Source-bound settings prepared in background workers. Each consumer applies its own immutable receipt before exposing refreshed values.
pub struct SettingsSource {
    store: Arc<projection::Store>,
    updates: tokio::sync::watch::Receiver<Arc<projection::Projection>>,
    applied: Arc<projection::Projection>,
}

impl std::fmt::Debug for SettingsSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SettingsSource")
            .finish_non_exhaustive()
    }
}

impl Default for SettingsSource {
    fn default() -> Self {
        Self::bind(projection::Origin::Global)
    }
}

impl From<ConfigHub> for SettingsSource {
    fn from(config: ConfigHub) -> Self {
        Self::bind(projection::Origin::Config(config))
    }
}

impl SettingsSource {
    fn bind(origin: projection::Origin) -> Self {
        let store = projection::Store::bind(origin);
        let mut updates = store.snapshot.subscribe();
        let applied = updates.borrow_and_update().clone();
        Self {
            applied,
            updates,
            store,
        }
    }

    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::bind(projection::Origin::Unavailable(reason.into()))
    }

    pub(crate) fn for_models(models: Arc<ModelRegistry>) -> Self {
        if Arc::ptr_eq(&models, atman_runtime::model_registry::default_registry()) {
            return Self::default();
        }
        Self::bind(projection::Origin::Models(models))
    }

    fn projection(&self) -> Arc<projection::Projection> {
        self.store.start();
        self.applied.clone()
    }

    pub(crate) fn models(&self) -> Arc<atman_runtime::model_registry::ModelRegistrySnapshot> {
        self.projection().data.models.clone()
    }

    pub(crate) fn matches_registry(&self, models: &Arc<ModelRegistry>) -> bool {
        self.store.matches_registry(models)
    }

    pub(crate) fn is_waiting(&self) -> bool {
        self.store.start();
        self.applied.loading || self.applied.generation < self.store.desired_generation()
    }

    pub(crate) fn initial_loading(&self) -> bool {
        self.is_waiting() && !self.projection().data.prepared
    }

    pub(crate) fn has_prepared_snapshot(&self) -> bool {
        self.projection().data.prepared
    }

    pub(crate) fn problem(&self) -> Option<String> {
        let projection = self.projection();
        projection.error.clone().or_else(|| {
            projection.data.config.as_ref().ok()?;
            projection
                .data
                .auth
                .as_ref()
                .err()
                .cloned()
                .or_else(|| projection.data.configured.as_ref().err().cloned())
        })
    }

    pub(crate) fn failure(&self) -> Option<String> {
        self.projection().error.clone()
    }

    pub(crate) fn invalidate(&self) {
        self.store.invalidate();
    }

    pub(crate) async fn changed(&mut self) {
        self.store.start();
        if self.updates.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
        self.applied = self.updates.borrow_and_update().clone();
    }

    pub(crate) fn take_changes(&mut self) -> bool {
        self.store.start();
        if self.updates.has_changed().unwrap_or(false) {
            self.applied = self.updates.borrow_and_update().clone();
            true
        } else {
            false
        }
    }

    pub(crate) fn config(&self) -> Result<ConfigHub, String> {
        if self.is_waiting() {
            return Err("Settings are loading".into());
        }
        if let Some(error) = self.failure() {
            return Err(error);
        }
        self.projection().data.config.clone()
    }

    pub(crate) fn load_auth(&self) -> Result<Arc<AuthStore>, String> {
        self.projection().data.auth.clone()
    }

    pub(crate) fn provider_display_name(&self, key: &str) -> String {
        atman_runtime::model_registry::provider_display_name_from_auth(
            key,
            &self.load_auth().unwrap_or_default(),
        )
    }

    pub(crate) fn enabled_provider_names(&self) -> std::collections::HashSet<String> {
        self.models()
            .enabled_provider_names(&self.load_auth().unwrap_or_default())
    }

    pub(crate) fn is_provider_enabled(&self, name: &str) -> bool {
        self.models()
            .is_provider_enabled(name, &self.load_auth().unwrap_or_default())
    }

    pub(crate) fn management_groups(&self) -> Vec<atman_runtime::model_registry::ProviderGroup> {
        self.projection().data.groups.clone()
    }

    pub(crate) fn configured_models(
        &self,
    ) -> Result<Arc<std::collections::HashSet<(String, String)>>, String> {
        self.projection().data.configured.clone()
    }

    pub(crate) fn local_mcp(&self) -> Result<Vec<atman_runtime::mcp::McpServerConfig>, String> {
        self.config()?;
        Ok(self.projection().data.local_mcp.clone())
    }

    pub(crate) async fn settled(&mut self) -> Result<(), String> {
        self.store.start();
        self.take_changes();
        while self.is_waiting() {
            self.updates
                .changed()
                .await
                .map_err(|_| "Settings source disconnected".to_owned())?;
            self.applied = self.updates.borrow_and_update().clone();
        }
        match self.failure() {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    #[cfg(test)]
    pub(crate) async fn ready(&mut self) {
        self.settled().await.unwrap();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn settings_subscription_coalesces_commits_without_consuming_another_view() {
        let temp = tempfile::tempdir().unwrap();
        let hub = ConfigHub::from_config_dir(temp.path().join("config"));
        let mut first = SettingsSource::from(hub.clone());
        first.ready().await;
        let mut second = first.clone();
        let mut foreign =
            SettingsSource::from(ConfigHub::from_config_dir(temp.path().join("other")));
        foreign.ready().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), first.changed())
                .await
                .is_err()
        );
        hub.add_alias("one", "example").unwrap();
        hub.add_alias("two", "example").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while first.models().all_aliases().len() != 2 {
                first.changed().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(first.models().all_aliases().len(), 2);
        assert!(!first.take_changes());
        assert!(second.take_changes());
        assert!(!second.take_changes());
        assert!(!foreign.take_changes());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), first.changed())
                .await
                .is_err()
        );
        let revision = first.models().model_catalog_revision();
        hub.models()
            .replace_provider_catalog(
                atman_runtime::model_registry::ProviderDescriptor {
                    provider_key: "catalog".into(),
                    provider_name: "Catalog".into(),
                    namespace: "catalog".into(),
                    wire_profile: atman_runtime::provider::ReasoningWireProfile::Unknown,
                },
                &[],
            )
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while first.models().model_catalog_revision() == revision {
                first.changed().await;
            }
        })
        .await
        .unwrap();
        assert!(second.take_changes());
        assert!(!foreign.take_changes());
    }

    #[tokio::test]
    async fn settings_panels_keep_their_bound_catalog_and_auth_source() {
        let temp = tempfile::tempdir().unwrap();
        let mut views = Vec::new();
        for name in ["first", "second"] {
            let hub = ConfigHub::from_config_dir(temp.path().join(name));
            std::fs::create_dir_all(hub.config_dir()).unwrap();
            std::fs::write(hub.config_dir().join("config.toml"), format!(
                "[providers.{name}]\ntype = 'openai'\napi_key = 'test-key'\n[models.model]\nprovider = '{name}'\nmodel = '{name}-model'\n[alias.smart]\nmodel = 'model'\n"
            )).unwrap();
            hub.reload().unwrap();
            hub.update_auth(|auth| {
                auth.add(atman_runtime::auth_store::StoredProvider {
                    id: "account".into(),
                    name: name.into(),
                    kind: atman_runtime::auth_store::ProviderKind::Custom,
                    access_token: "test-token".into(),
                    refresh_token: None,
                    expires_at: 0,
                    account: Some(format!("{name}@example.com")),
                    enabled: false,
                    model_cache: None,
                });
                Ok(())
            })
            .unwrap();
            let mut panels = crate::wm::modal::ModalManager::default();
            let mut source = SettingsSource::from(hub.clone());
            source.ready().await;
            panels.bind_settings(source);
            views.push((hub, panels));
        }
        for (index, (hub, panels)) in views.iter_mut().enumerate() {
            let name = ["first", "second"][index];
            panels.provider_manager.open();
            panels.alias_manager.open();
            panels.model_picker.open();
            assert_eq!(
                panels.alias_manager.aliases,
                [("smart".into(), "model".into())]
            );
            for source in [
                &panels.settings,
                &panels.provider_manager.settings,
                &panels.model_manager.settings,
                &panels.model_picker.settings,
                &panels.alias_manager.settings,
                &panels.onboarding.settings,
            ] {
                assert_eq!(source.config().unwrap().source(), hub.source());
                assert_eq!(
                    source.models().model_entry("model").unwrap().model,
                    format!("{name}-model")
                );
                assert_eq!(source.models().all_provider_groups()[0].provider_name, name);
                assert_eq!(
                    source.provider_display_name("account"),
                    format!("{name} · {name}@example.com")
                );
                assert!(!source.models().is_first_run(&AuthStore::default()));
            }
        }
        views[1].0.add_alias("other", "model").unwrap();
        views[0].1.alias_manager.open();
        assert_eq!(views[0].1.alias_manager.aliases.len(), 1);
    }

    #[tokio::test]
    async fn deferred_mcp_reads_keep_editor_intent_and_do_not_reopen_after_close() {
        use crate::{
            keys::KeyAction,
            wm::modal::{ModalKind, ModalManager},
        };
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigHub::from_config_dir(directory.path().join("config"));
        config
            .save_mcp(&[atman_runtime::mcp::McpServerConfig::stdio(
                "example",
                "fixture-command",
                vec![],
                atman_runtime::tool::Tier::One,
                1000,
            )])
            .unwrap();
        let mut source = SettingsSource::from(config);
        let mut panels = ModalManager::default();
        panels.bind_settings(source.clone());
        panels.open_mcp_edit("example").unwrap();
        assert!(panels.retains_settings_operation());
        assert!(panels.has_active_animation(ModalKind::McpEditor));
        source.ready().await;
        panels.bind_settings(source.clone());
        panels.settings_prepared();
        assert_eq!(panels.mcp_editor.name.buf(), "example");
        assert_eq!(panels.mcp_editor.command.buf(), "fixture-command");
        assert!(!panels.retains_settings_operation());
        assert!(!panels.has_active_animation(ModalKind::McpEditor));
        source.invalidate();
        panels.open_mcp_edit("example").unwrap();
        panels.handle_key_top(
            ModalKind::McpEditor,
            &KeyAction::Escape,
            &mut crate::app::AppState::default(),
            None,
        );
        source.ready().await;
        panels.bind_settings(source);
        panels.settings_prepared();
        assert!(!panels.mcp_editor.open);
        assert!(!panels.retains_settings_operation());
    }

    #[tokio::test]
    async fn publication_stays_loading_until_the_view_applies_its_settings_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let config = ConfigHub::from_config_dir(directory.path().join("config"));
        config.add_alias("smart", "example").unwrap();
        let mut prepared = SettingsSource::from(config);
        let mut pending = prepared.clone();
        let mut panels = crate::wm::modal::ModalManager::default();
        panels.bind_settings(pending.clone());
        panels.alias_manager.open();
        prepared.ready().await;
        assert!(pending.is_waiting());
        assert!(!pending.has_prepared_snapshot());
        assert!(pending.models().all_aliases().is_empty());
        assert!(panels.has_active_animation(crate::wm::modal::ModalKind::AliasManager));
        let mut app = crate::app::AppState::new("loading".into(), None);
        app.settings = pending.clone();
        let mut editor = crate::input::InputEditor::default();
        editor.insert_str("message");
        assert!(
            app.prepare_composer_submission(&editor.prepare_submission())
                .unwrap_err()
                .contains("Settings are loading")
        );
        assert_eq!(editor.buf(), "message");
        editor.replace_with(":status");
        assert_eq!(
            app.prepare_composer_submission(&editor.prepare_submission())
                .unwrap()
                .control_command(),
            Some(":status")
        );
        assert!(pending.take_changes());
        assert!(!pending.is_waiting());
        panels.bind_settings(pending);
        panels.alias_manager.catalog_changed();
        assert_eq!(
            panels.alias_manager.aliases,
            [("smart".into(), "example".into())]
        );
        assert!(!panels.has_active_animation(crate::wm::modal::ModalKind::AliasManager));
        app.settings = panels.settings.clone();
        app.settings.invalidate();
        assert!(app.settings.is_waiting());
        assert_eq!(
            app.model_registry().unwrap().resolve_alias("smart"),
            "example"
        );
        editor.replace_with("message");
        assert!(
            app.prepare_composer_submission(&editor.prepare_submission())
                .is_err()
        );
    }
}
