use std::io;

use tokio::sync::watch;

use crate::{
    config_hub::ConfigHub,
    flow_store::{FlowStore, SessionConfiguration, SessionProjection, SessionStore},
    user_input::preferences::EffectiveInputPreferences,
};

pub(super) struct InputPreferences {
    pub config: Result<Option<ConfigHub>, String>,
    pub snapshot: watch::Sender<EffectiveInputPreferences>,
}

impl InputPreferences {
    pub fn for_flow(
        &self,
        catalog: &SessionProjection,
        flow: &FlowStore,
    ) -> Option<watch::Receiver<EffectiveInputPreferences>> {
        if !matches!(
            catalog.descriptor.configuration,
            SessionConfiguration::Atman(_)
        ) || flow.transient_retention().is_some()
            || catalog.flows.iter().any(|row| {
                row.id == flow.descriptor().address.instance_id && row.retention.is_some()
            })
        {
            return None;
        }
        Some(self.snapshot.subscribe())
    }

    pub async fn start(
        catalog: &SessionProjection,
        snapshot: watch::Sender<EffectiveInputPreferences>,
    ) -> Self {
        let config = if let SessionConfiguration::Atman(source) = &catalog.descriptor.configuration
        {
            let source = source.clone();
            tokio::task::spawn_blocking(move || ConfigHub::from_source(&source).map(Some))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result.map_err(|error| error.to_string()))
        } else {
            Ok(None)
        };
        Self { config, snapshot }
    }

    pub async fn refresh(
        &self,
        mut store: SessionStore,
        reload: bool,
    ) -> Result<(SessionStore, io::Result<()>), tokio::task::JoinError> {
        let config = match &self.config {
            Ok(Some(config)) => config.clone(),
            Ok(None) => return Ok((store, Ok(()))),
            Err(error) => return Ok((store, Err(io::Error::other(error.clone())))),
        };
        let (store, settings) = tokio::task::spawn_blocking(move || {
            let settings = (|| {
                if reload {
                    // Only explicit refresh owns write admission to settle an interrupted edit.
                    store.reconcile_input_preferences(&config)?;
                    config.reload().map_err(io::Error::other)?;
                }
                let (reasoning, trust) = config.input_defaults().map_err(io::Error::other)?;
                Ok(store
                    .projection()
                    .input_preferences
                    .resolve(reasoning, trust))
            })();
            (store, settings)
        })
        .await?;
        Ok((store, settings.map(|settings| self.publish(settings))))
    }

    pub fn publish(&self, settings: EffectiveInputPreferences) {
        self.snapshot.send_if_modified(|current| {
            if *current == settings {
                return false;
            }
            *current = settings;
            true
        });
    }
}
