//! Weak lifecycle publication through the original storage owner.

use std::{
    io,
    sync::{Arc, Weak},
};
use tokio::sync::mpsc;

use super::{Command, FlowWorker, StorageFailure, request};

#[derive(Clone)]
pub(crate) struct LifecycleWriter {
    commands: mpsc::WeakSender<Command>,
    admission: Arc<super::host::SessionAdmission>,
    failure: Arc<StorageFailure>,
    resources: Weak<super::resources::InstanceResources>,
    runtime: tokio::runtime::Handle,
}

impl LifecycleWriter {
    pub fn runtime_handle(&self) -> tokio::runtime::Handle {
        self.runtime.clone()
    }
    pub async fn apply<T: Send + 'static>(
        &self,
        operation: impl FnOnce() -> T + Send + 'static,
    ) -> io::Result<T> {
        let commands = self
            .commands
            .upgrade()
            .ok_or_else(|| io::Error::other("lifecycle owner is unavailable"))?;
        request(&commands, &self.failure, &self.admission, move |_| {
            Ok(operation())
        })
        .await
    }

    pub fn token(&self) -> Option<tokio_util::task::task_tracker::TaskTrackerToken> {
        self.resources
            .upgrade()
            .map(|owner| owner.executions.token())
    }

    pub fn spawn(&self, task: impl std::future::Future<Output = ()> + Send + 'static) {
        self.runtime.spawn(task);
    }
}

impl FlowWorker {
    pub(crate) fn lifecycle_writer(&self) -> LifecycleWriter {
        LifecycleWriter {
            commands: self.commands.downgrade(),
            admission: self.admission.clone(),
            failure: self.failure.clone(),
            resources: Arc::downgrade(&self.resources),
            runtime: tokio::runtime::Handle::current(),
        }
    }
}
