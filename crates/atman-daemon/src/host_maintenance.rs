//! Original host authority recovery outside effect and presentation paths.

use std::{io, sync::Arc, time::Duration};

use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::DaemonState;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct MaintenanceStatus {
    pub profile_available: bool,
    pub handoff_pending: bool,
    pub unavailable: Option<String>,
}

/// One cancellable observer. Already accepted handoffs retain their original owned continuation.
pub struct HostMaintenance {
    stop: CancellationToken,
    status: watch::Receiver<MaintenanceStatus>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl HostMaintenance {
    pub fn start(state: &Arc<DaemonState>) -> Self {
        let stop = state.shutdown_signal().child_token();
        let cancelled = stop.clone();
        let state = Arc::downgrade(state);
        let (published, status) = watch::channel(MaintenanceStatus::default());
        let task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! { biased; _ = cancelled.cancelled() => break, _ = interval.tick() => {} }
                let Some(state) = state.upgrade() else {
                    break;
                };
                if state.is_stopping() {
                    break;
                }
                let profile = state.ensure_profile_port().await;
                let mut next = MaintenanceStatus {
                    profile_available: profile.is_ok(),
                    handoff_pending: state.handoff_pending(),
                    unavailable: profile.err().map(|error| error.to_string()),
                };
                if next.profile_available && !cancelled.is_cancelled() {
                    if let Err(error) = state.recover_handoffs(cancelled.child_token()).await {
                        if error.kind() != io::ErrorKind::Interrupted {
                            next.unavailable = Some(error.to_string());
                        }
                    }
                }
                next.handoff_pending = state.handoff_pending();
                published.send_if_modified(|current| {
                    if *current == next {
                        false
                    } else {
                        *current = next;
                        true
                    }
                });
            }
        });
        Self {
            stop,
            status,
            task: Some(task),
        }
    }

    pub fn status(&self) -> watch::Receiver<MaintenanceStatus> {
        self.status.clone()
    }

    pub async fn shutdown(mut self) -> io::Result<()> {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            task.await.map_err(io::Error::other)?;
        }
        Ok(())
    }
}

impl Drop for HostMaintenance {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
