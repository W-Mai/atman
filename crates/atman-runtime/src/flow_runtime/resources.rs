//! Process resources owned by a persistent flow, independently of its turns.

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use crate::task_registry::{TaskFilter, TaskKind, TaskRegistry};
use crate::tools::{bash_bg::BgRegistry, term::TermRegistry};

pub struct InstanceResources {
    pub(super) observers: super::process_watch::ProcessObservers,
    pub(super) lifetime: CancellationToken,
    tasks: TaskRegistry,
    background: Arc<BgRegistry>,
    terminals: Arc<TermRegistry>,
}

impl Default for InstanceResources {
    fn default() -> Self {
        let tasks = TaskRegistry::new();
        let lifetime = CancellationToken::new();
        Self {
            observers: Default::default(),
            lifetime: lifetime.clone(),
            background: Arc::new(
                BgRegistry::new()
                    .with_task_registry(tasks.clone())
                    .with_lifetime(lifetime.clone()),
            ),
            terminals: Arc::new(
                TermRegistry::new()
                    .with_task_registry(tasks.clone())
                    .with_lifetime(lifetime),
            ),
            tasks,
        }
    }
}

impl InstanceResources {
    pub fn tasks(&self) -> &TaskRegistry {
        &self.tasks
    }

    pub fn background(&self) -> &Arc<BgRegistry> {
        &self.background
    }

    pub fn terminals(&self) -> &Arc<TermRegistry> {
        &self.terminals
    }

    pub(super) fn bind(&self, ctx: &mut crate::tool::ToolCtx) {
        ctx.bg_registry = Some(self.background.clone());
        ctx.term_registry = Some(self.terminals.clone());
        ctx.task_registry = Some(self.tasks.clone());
    }

    pub(super) fn close(&self) {
        self.lifetime.cancel();
        self.background.close();
        self.terminals.close();
    }

    pub(super) fn is_stopped(&self) -> bool {
        self.observers.stopped()
            && self
                .tasks
                .list(&TaskFilter::all())
                .iter()
                .all(|task| task.kind == TaskKind::Flow || task.status.is_terminal())
    }

    pub(super) async fn shutdown(&self) {
        let mut changes = self.tasks.subscribe();
        self.close();
        self.observers.join().await;
        while !self.is_stopped() {
            if matches!(
                changes.recv().await,
                Err(tokio::sync::broadcast::error::RecvError::Closed)
            ) {
                break;
            }
        }
    }
}

pub(super) struct ResourceOwner(pub Arc<InstanceResources>);

impl Drop for ResourceOwner {
    fn drop(&mut self) {
        self.0.close();
    }
}
