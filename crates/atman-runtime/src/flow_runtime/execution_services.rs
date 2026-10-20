//! Host-injected execution services shared by frontend entry points and peer creation.

use std::{io, sync::Arc};

use super::SessionHandle;
use crate::{Executor, storage::ProjectContext, tool::BoxFut};

/// Resolve a project's services from the host's existing cache. Resolvers must not retain their owning RuntimeHost or SessionHandle; use weak references when calling back into a frontend.
pub type ExecutorResolver =
    Arc<dyn Fn(Option<ProjectContext>) -> BoxFut<'static, io::Result<Executor>> + Send + Sync>;

impl SessionHandle {
    /// Install this owner's resolver once. Subsequent attachments retain the original service owner.
    pub fn configure_executor(
        &self,
        initialize: impl FnOnce() -> ExecutorResolver,
    ) -> io::Result<()> {
        if self.is_stopped() {
            return Err(io::Error::other("session owner has stopped"));
        }
        self.executor_resolver.get_or_init(initialize);
        Ok(())
    }

    /// Resolve explicit project services without loading a flow, claiming input, or starting execution.
    pub async fn executor_for(&self, project: Option<ProjectContext>) -> io::Result<Executor> {
        let snapshot = self.snapshot().await?;
        let resolver = self.executor_resolver.get().ok_or_else(|| {
            io::Error::other("host has not installed a project execution resolver")
        })?;
        let executor = resolver(project.clone()).await?;
        executor
            .validate_session_binding(&snapshot.catalog.descriptor.configuration, project.as_ref())
            .map_err(io::Error::other)?;
        if self.is_stopped() {
            return Err(io::Error::other(
                "session owner stopped while resolving execution services",
            ));
        }
        Ok(executor)
    }
}
