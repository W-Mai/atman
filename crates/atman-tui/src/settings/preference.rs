//! One source-bound preference read retained by its original form.

use atman_runtime::form::FormKind;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::{Preference, SettingsSource};

type Choice = Result<(FormKind, usize), String>;

pub(crate) struct PreferenceRead {
    preference: Preference,
    source: SettingsSource,
    project: Option<String>,
    worker: Option<JoinHandle<Choice>>,
    cancellation: CancellationToken,
}

impl Drop for PreferenceRead {
    fn drop(&mut self) {
        self.cancellation.cancel();
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
    }
}

impl PreferenceRead {
    pub(crate) fn start(
        preference: Preference,
        source: SettingsSource,
        project: Option<String>,
    ) -> Result<Self, String> {
        let mut read = Self {
            preference,
            source,
            project,
            worker: None,
            cancellation: CancellationToken::new(),
        };
        read.launch(false)?;
        Ok(read)
    }

    pub(crate) fn retry(&mut self) -> Result<(), String> {
        self.launch(true)
    }

    fn launch(&mut self, refresh: bool) -> Result<(), String> {
        self.cancellation.cancel();
        if let Some(worker) = self.worker.take() {
            worker.abort();
        }
        self.cancellation = CancellationToken::new();
        let runtime = tokio::runtime::Handle::try_current().map_err(|error| error.to_string())?;
        let mut source = self.source.clone();
        if refresh || source.failure().is_some() {
            source.invalidate();
        }
        let project = self.project.clone();
        let preference = self.preference;
        let cancelled = self.cancellation.clone();
        self.worker = Some(runtime.spawn(async move {
            // Waiting for a shared source must not consume a filesystem slot needed to prepare it.
            tokio::select! {
                _ = cancelled.cancelled() => return Err("Preference read cancelled".into()),
                result = source.settled() => result?,
            }
            let config = source.config()?;
            super::projection::blocking(&cancelled, move || {
                preference.selection(&config, project.as_deref())
            })
            .await
            .unwrap_or_else(|| Err("Preference read cancelled".into()))
        }));
        Ok(())
    }

    pub(crate) fn is_waiting(&self) -> bool {
        self.worker.is_some()
    }

    pub(crate) async fn next(&mut self) -> Choice {
        let Some(worker) = self.worker.as_mut() else {
            return std::future::pending().await;
        };
        let result = worker
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
        self.worker = None;
        result
    }
}
