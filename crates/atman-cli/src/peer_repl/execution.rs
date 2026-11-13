//! Project-bound execution services shared by views of the same Session.

use super::*;
use atman_runtime::{flow_store::SessionProjection, storage::ProjectContext};

pub(super) type ScopeExecutors = Arc<ExecutionServices>;

#[derive(Default)]
pub(super) struct ExecutionServices {
    pub(super) slots:
        std::sync::Mutex<HashMap<Option<ProjectContext>, Arc<tokio::sync::OnceCell<Executor>>>>,
    refreshes: std::sync::Mutex<tokio::task::JoinSet<()>>,
    pub(super) restored: tokio::sync::OnceCell<()>,
}

impl ExecutionServices {
    fn refresh(&self, task: impl Future<Output = ()> + Send + 'static) {
        let mut refreshes = self.refreshes.lock().unwrap();
        while refreshes.try_join_next().is_some() {}
        refreshes.spawn(task);
    }
}

pub(super) fn configure(
    session: &SessionHandle,
    executors: &ScopeExecutors,
    config: &ConfigHub,
    prompts: &Arc<prompts::Prompts>,
) -> Result<()> {
    session.configure_executor(|| {
        let executors = executors.clone();
        let config = config.clone();
        let prompts = prompts.clone();
        Arc::new(move |project| {
            let executors = executors.clone();
            let config = config.clone();
            let prompts = prompts.clone();
            Box::pin(async move {
                executor(&executors, &config, project.as_ref(), &prompts)
                    .await
                    .map_err(std::io::Error::other)
            })
        })
    })?;
    Ok(())
}

pub(super) fn selected_project(
    catalog: &SessionProjection,
    instance: Option<&FlowInstanceId>,
) -> Result<Option<ProjectContext>> {
    let Some(id) = instance.or(catalog.default_flow.as_ref()) else {
        return Ok(catalog.project.clone());
    };
    Ok(catalog
        .flows
        .iter()
        .find(|flow| &flow.id == id)
        .context("selected flow is not registered")?
        .project
        .clone())
}

pub(super) async fn executor(
    executors: &ScopeExecutors,
    config: &ConfigHub,
    project: Option<&ProjectContext>,
    prompts: &Arc<prompts::Prompts>,
) -> Result<Executor> {
    let slot = executors
        .slots
        .lock()
        .unwrap()
        .entry(project.cloned())
        .or_default()
        .clone();
    Ok(slot
        .get_or_try_init(|| async {
            let mut events = EventSink::new();
            if let Some(redactor) =
                atman_daemon::bootstrap::build_redactor(Some(config.config_dir()))
            {
                events = events.with_redactor(redactor);
            }
            let runtime = AtmanRuntime::build(AtmanRuntimeOptions {
                events,
                mock: false,
                config: config.clone(),
                project: project.cloned(),
                home_dir: atman_runtime::platform::user_home_dir(),
                workspace_generation: Uuid::now_v7().to_string(),
            })
            .await?;
            if let Some(lifecycle) = runtime.executor.provider_lifecycle() {
                executors.refresh(crate::consume_provider_catalog_refresh_plan(
                    runtime.provider_catalog_refresh_plan,
                    move |id| {
                        let lifecycle = lifecycle.clone();
                        async move { lifecycle.refresh_models_if_stale(&id).await }
                    },
                ));
            }
            let mut executor = runtime.executor;
            executor.tool_ctx.prompt_resolver = Some(prompts.clone());
            Ok::<_, anyhow::Error>(executor)
        })
        .await?
        .clone())
}

impl Attachment {
    pub(super) async fn for_project(&self, project: Option<ProjectContext>) -> Result<Self> {
        if self.project == project {
            return Ok(self.clone());
        }
        let mut scope = self.clone();
        configure(&self.session, &self.executors, &self.config, &self.prompts)?;
        scope.executor = self.session.executor_for(project.clone()).await?;
        scope.executor.tool_ctx.prompt_resolver = self.executor.tool_ctx.prompt_resolver.clone();
        scope.project = project;
        Ok(scope)
    }

    pub(super) async fn for_worker(&self, worker: &FlowWorker) -> Result<Self> {
        ensure!(
            worker.address().session_id.0 == self.session.id(),
            "flow and Session scope differ"
        );
        self.for_project(worker.project().await?).await
    }

    pub(super) fn source_roots(&self) -> atman_runtime::source_program::SourceRoots {
        self.descriptor.source_roots_for(self.project.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn catalog_refresh_does_not_block_services_and_stops_with_owner() {
        let services = ExecutionServices::default();
        let (started, ready) = tokio::sync::oneshot::channel();
        let (held, released) = tokio::sync::oneshot::channel::<()>();
        services.refresh(async move {
            let _held = held;
            let _ = started.send(());
            std::future::pending::<()>().await;
        });
        ready.await.unwrap();
        assert!(services.slots.lock().unwrap().is_empty());
        drop(services);
        assert!(released.await.is_err());
    }
}
