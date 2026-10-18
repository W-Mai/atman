//! Configuration identity retained by a view and its settings panels.

use std::sync::Arc;

use atman_runtime::{auth_store::AuthStore, config_hub::ConfigHub, model_registry::ModelRegistry};

#[derive(Clone, Debug)]
pub enum SettingsMutation {
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

#[derive(Clone, Debug)]
pub struct SettingsSource {
    config: Result<ConfigHub, String>,
    models: Arc<ModelRegistry>,
    updates: Option<tokio::sync::watch::Receiver<()>>,
    catalog: tokio::sync::watch::Receiver<u64>,
}

impl Default for SettingsSource {
    fn default() -> Self {
        match ConfigHub::global() {
            Ok(config) => Self::from(config),
            Err(error) => {
                let models = Arc::new(ModelRegistry::new());
                Self {
                    config: Err(error.to_string()),
                    catalog: models.subscribe_model_catalog(),
                    models,
                    updates: None,
                }
            }
        }
    }
}

impl From<ConfigHub> for SettingsSource {
    fn from(config: ConfigHub) -> Self {
        Self {
            updates: Some(config.subscribe_changes()),
            catalog: config.models().subscribe_model_catalog(),
            models: config.models().clone(),
            config: Ok(config),
        }
    }
}

impl std::ops::Deref for SettingsSource {
    type Target = ModelRegistry;
    fn deref(&self) -> &Self::Target {
        &self.models
    }
}

impl SettingsSource {
    pub(crate) async fn changed(&mut self) {
        tokio::select! {
            _ = wait_change(self.updates.as_mut()) => {}
            _ = wait_change(Some(&mut self.catalog)) => {}
        }
        self.take_changes();
    }

    pub(crate) fn take_changes(&mut self) -> bool {
        let config = self.updates.as_mut().is_some_and(take_change);
        take_change(&mut self.catalog) || config
    }

    pub(crate) fn config(&self) -> Result<&ConfigHub, String> {
        self.config.as_ref().map_err(Clone::clone)
    }

    pub(crate) fn load_auth(&self) -> Result<AuthStore, String> {
        self.config()?
            .load_auth()
            .map_err(|error| error.to_string())
    }

    pub(crate) fn provider_display_name(&self, key: &str) -> String {
        atman_runtime::model_registry::provider_display_name_from_auth(
            key,
            &self.load_auth().unwrap_or_default(),
        )
    }

    pub(crate) fn enabled_provider_names(&self) -> std::collections::HashSet<String> {
        self.models
            .enabled_provider_names(&self.load_auth().unwrap_or_default())
    }

    pub(crate) fn is_provider_enabled(&self, name: &str) -> bool {
        self.models
            .is_provider_enabled(name, &self.load_auth().unwrap_or_default())
    }

    pub(crate) fn management_groups(&self) -> Vec<atman_runtime::model_registry::ProviderGroup> {
        self.models.all_provider_groups_for_management(
            &self.load_auth().unwrap_or_default(),
            self.config().ok(),
        )
    }
}

fn take_change<T>(rx: &mut tokio::sync::watch::Receiver<T>) -> bool {
    if rx.has_changed().unwrap_or(false) {
        rx.borrow_and_update();
        true
    } else {
        false
    }
}

async fn wait_change<T>(rx: Option<&mut tokio::sync::watch::Receiver<T>>) {
    if let Some(rx) = rx
        && rx.changed().await.is_ok()
    {
        return;
    }
    std::future::pending::<()>().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn settings_subscription_coalesces_commits_without_consuming_another_view() {
        let temp = tempfile::tempdir().unwrap();
        let hub = ConfigHub::from_config_dir(temp.path().join("config"));
        let mut first = SettingsSource::from(hub.clone());
        let mut second = first.clone();
        let mut foreign =
            SettingsSource::from(ConfigHub::from_config_dir(temp.path().join("other")));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), first.changed())
                .await
                .is_err()
        );
        hub.add_alias("one", "example").unwrap();
        hub.add_alias("two", "example").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), first.changed())
            .await
            .unwrap();
        assert_eq!(first.all_aliases().len(), 2);
        assert!(!first.take_changes());
        assert!(second.take_changes());
        assert!(!second.take_changes());
        assert!(!foreign.take_changes());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), first.changed())
                .await
                .is_err()
        );
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
        tokio::time::timeout(std::time::Duration::from_secs(1), first.changed())
            .await
            .unwrap();
        assert!(second.take_changes());
        assert!(!foreign.take_changes());
    }

    #[test]
    fn settings_panels_keep_their_bound_catalog_and_auth_source() {
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
            panels.bind_settings(hub.clone().into());
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
                    source.model_entry("model").unwrap().model,
                    format!("{name}-model")
                );
                assert_eq!(source.all_provider_groups()[0].provider_name, name);
                assert_eq!(
                    source.provider_display_name("account"),
                    format!("{name} · {name}@example.com")
                );
                assert!(!source.is_first_run(&AuthStore::default()));
            }
        }
        views[1].0.add_alias("other", "model").unwrap();
        views[0].1.alias_manager.open();
        assert_eq!(views[0].1.alias_manager.aliases.len(), 1);
    }
}
