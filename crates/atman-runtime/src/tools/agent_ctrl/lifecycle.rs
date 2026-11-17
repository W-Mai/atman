//! Guarded registration and owned asynchronous finalization for instance-bound identities.

use super::*;
use std::io;

impl FlowRegistry {
    pub fn principal_available(&self, run: &FlowRunId) -> bool {
        self.lookup_run(run)
            .is_some_and(|identity| identity.principal_available())
    }

    pub(crate) async fn register_owned_instance(
        self: &Arc<Self>,
        worker: &crate::flow_runtime::FlowWorker,
        run: FlowRunId,
        authority: crate::flow_authority::EffectiveAuthority,
    ) -> Result<(Arc<crate::flow_authority::FlowIdentity>, FlowLifecycleGuard), RuntimeError> {
        self.owned_self.get_or_init(|| Arc::downgrade(self));
        let writer = worker.lifecycle_writer();
        let retained = writer.clone();
        let address = worker.address().clone();
        let registry = self.clone();
        writer
            .apply(move || {
                let identity = registry.register_instance(address, run.clone(), authority)?;
                registry
                    .lifecycle_writers
                    .lock()
                    .unwrap()
                    .insert(run.clone(), retained);
                let lifecycle = registry.lifecycle_guard(&run);
                Ok((identity, lifecycle))
            })
            .await
            .map_err(|error| RuntimeError::ToolFailed(error.to_string()))?
    }

    pub(crate) async fn register_child_owned(
        self: &Arc<Self>,
        parent: &FlowRunId,
        child: FlowRunId,
        invocation: crate::flow_authority::InvocationKind,
        shell: bool,
        workspace: crate::flow_authority::ChildWorkspaceAuthority,
    ) -> Result<(Arc<crate::flow_authority::FlowIdentity>, FlowLifecycleGuard), RuntimeError> {
        let writer = self.lifecycle_writers.lock().unwrap().get(parent).cloned();
        let Some(writer) = writer else {
            let identity =
                self.register_child(parent, child.clone(), invocation, shell, workspace)?;
            return Ok((identity, self.lifecycle_guard(&child)));
        };
        let retained = writer.clone();
        let registry = self.clone();
        let parent = parent.clone();
        writer
            .apply(move || {
                let identity = registry.with_lifecycle_arbitration(|| {
                    registry.register_child_locked(
                        &parent,
                        child.clone(),
                        invocation,
                        shell,
                        workspace,
                    )
                })?;
                registry
                    .lifecycle_writers
                    .lock()
                    .unwrap()
                    .insert(child.clone(), retained);
                let lifecycle = registry.lifecycle_guard(&child);
                Ok((identity, lifecycle))
            })
            .await
            .map_err(|error| RuntimeError::ToolFailed(error.to_string()))?
    }

    pub(crate) async fn block_on_descendant_owned(
        self: &Arc<Self>,
        parent: &FlowRunId,
        child: &FlowRunId,
    ) -> Result<DescendantBlockGuard, RuntimeError> {
        let writer = self.lifecycle_writers.lock().unwrap().get(parent).cloned();
        let Some(writer) = writer else {
            return self.block_on_descendant(parent, child);
        };
        let registry = self.clone();
        let parent = parent.clone();
        let child = child.clone();
        writer
            .apply(move || {
                registry.with_lifecycle_arbitration(|| {
                    registry.block_on_descendant_locked(&parent, &child)
                })
            })
            .await
            .map_err(|error| RuntimeError::ToolFailed(error.to_string()))?
    }

    fn terminal_now(&self, run: &FlowRunId) {
        let completions = self.with_lifecycle_arbitration(|| self.mark_terminal_locked(run));
        for completion in completions {
            completion();
        }
        self.lifecycle_writers.lock().unwrap().remove(run);
    }

    pub(super) fn queue_terminal(
        self: &Arc<Self>,
        run: FlowRunId,
        token: Option<tokio_util::task::task_tracker::TaskTrackerToken>,
    ) {
        if self.lookup_run(&run).is_some_and(|identity| {
            matches!(
                identity.execution_state(),
                crate::flow_authority::FlowExecutionState::Terminal
            )
        }) {
            return;
        }
        if let Some(identity) = self.lookup_run(&run) {
            identity.close_principal_admission();
        }
        let writer = self.lifecycle_writers.lock().unwrap().get(&run).cloned();
        let Some(writer) = writer else {
            self.terminal_now(&run);
            return;
        };
        let registry = self.clone();
        let retained = writer.clone();
        let token = token.or_else(|| writer.token());
        writer.spawn(async move {
            let _token = token;
            if let Err(error) = retained.apply(move || registry.terminal_now(&run)).await {
                crate::notify!(error, "flow lifecycle finalization failed: {error}");
            }
        });
    }

    async fn finalize_terminal(
        self: &Arc<Self>,
        run: FlowRunId,
        token: Option<tokio_util::task::task_tracker::TaskTrackerToken>,
    ) -> io::Result<()> {
        if self.lookup_run(&run).is_some_and(|identity| {
            matches!(
                identity.execution_state(),
                crate::flow_authority::FlowExecutionState::Terminal
            )
        }) {
            return Ok(());
        }
        if let Some(identity) = self.lookup_run(&run) {
            identity.close_principal_admission();
        }
        let writer = self.lifecycle_writers.lock().unwrap().get(&run).cloned();
        let Some(writer) = writer else {
            self.terminal_now(&run);
            return Ok(());
        };
        let registry = self.clone();
        tokio::spawn(async move {
            let _token = token;
            writer.apply(move || registry.terminal_now(&run)).await
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(super) fn finish_descendant(self: &Arc<Self>, parent: FlowRunId, child: FlowRunId) {
        let writer = self.lifecycle_writers.lock().unwrap().get(&parent).cloned();
        let Some(writer) = writer else {
            self.unblock_descendant(&parent, &child);
            return;
        };
        let token = writer.token();
        let retained = writer.clone();
        let registry = self.clone();
        writer.spawn(async move {
            let _token = token;
            if let Err(error) = retained
                .apply(move || registry.unblock_descendant(&parent, &child))
                .await
            {
                crate::notify!(error, "flow descendant finalization failed: {error}");
            }
        });
    }

    async fn finalize_descendant(
        self: &Arc<Self>,
        parent: FlowRunId,
        child: FlowRunId,
    ) -> io::Result<()> {
        let writer = self.lifecycle_writers.lock().unwrap().get(&parent).cloned();
        let Some(writer) = writer else {
            self.unblock_descendant(&parent, &child);
            return Ok(());
        };
        let token = writer.token();
        let registry = self.clone();
        tokio::spawn(async move {
            let _token = token;
            writer
                .apply(move || registry.unblock_descendant(&parent, &child))
                .await
        })
        .await
        .map_err(io::Error::other)?
    }
}

impl FlowLifecycleGuard {
    pub(crate) fn close_admission(&self) {
        if let Some(identity) = self.registry.lookup_run(&self.run_id) {
            identity.close_principal_admission();
        }
    }
    pub(crate) fn runtime_handle(&self) -> Option<tokio::runtime::Handle> {
        self.runtime.clone()
    }
    pub(crate) fn take_cleanup_token(
        &mut self,
    ) -> Option<tokio_util::task::task_tracker::TaskTrackerToken> {
        self.token.take()
    }

    pub(crate) async fn finish(mut self) -> io::Result<()> {
        self.active = false;
        self.registry
            .finalize_terminal(self.run_id.clone(), self.token.take())
            .await
    }
}

impl DescendantBlockGuard {
    pub(crate) async fn finish(mut self) -> io::Result<()> {
        self.active = false;
        self.registry
            .finalize_descendant(self.parent_run_id.clone(), self.child_run_id.clone())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        flow_authority::{
            ChildWorkspaceAuthority, EffectiveAuthority, FlowExecutionState, InvocationKind,
        },
        flow_runtime::FlowWorker,
        flow_store::{
            EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, RuntimeStore, SessionDescriptor,
        },
        source_program::RevisionBundle,
    };

    async fn fixture() -> (tempfile::TempDir, FlowWorker) {
        let directory = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(directory.path()).unwrap();
        let id = Uuid::now_v7();
        let mut session = root
            .create_session(SessionDescriptor {
                id,
                title: "Lifecycle cancellation".into(),
                project: None,
                configuration: Default::default(),
            })
            .unwrap();
        let store = session
            .create_flow(FlowDescriptor {
                address: FlowRef {
                    session_id: id.into(),
                    instance_id: FlowInstanceId::now(),
                },
                creation_request: Uuid::now_v7(),
                created_by: None,
                content: EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: RevisionBundle {
                        entry: "main.at".into(),
                        sources: [("main.at".into(), "flow main() { }".into())].into(),
                        edges: Default::default(),
                    },
                }
                .into(),
            })
            .unwrap();
        (directory, FlowWorker::start(store).await.unwrap())
    }

    async fn hold_writer(
        worker: &FlowWorker,
    ) -> (
        std::sync::mpsc::Sender<()>,
        tokio::task::JoinHandle<io::Result<()>>,
    ) {
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let worker = worker.clone();
        let pending = tokio::spawn(async move {
            worker
                .request(move |_| {
                    entered.send(()).unwrap();
                    blocked.recv_timeout(Duration::from_secs(10)).unwrap();
                    Ok(())
                })
                .await
        });
        ready.await.unwrap();
        (release, pending)
    }

    #[tokio::test]
    async fn cancelled_registration_and_descendant_acks_retain_their_original_cleanup() {
        let (_directory, worker) = fixture().await;
        let registry = Arc::new(FlowRegistry::new());
        let authority = || EffectiveAuthority::root(&Default::default(), false, None);
        let parent = FlowRunId::now();
        let (parent_identity, parent_lifecycle) = registry
            .register_owned_instance(&worker, parent.clone(), authority())
            .await
            .unwrap();
        for child in [false, true] {
            for before_ack in [false, true] {
                let run = FlowRunId::now();
                let (release, blocked) = hold_writer(&worker).await;
                let mut registration = Box::pin(async {
                    if child {
                        registry
                            .register_child_owned(
                                &parent,
                                run.clone(),
                                InvocationKind::InlineSubflow,
                                false,
                                ChildWorkspaceAuthority::Inherit,
                            )
                            .await
                    } else {
                        registry
                            .register_owned_instance(&worker, run.clone(), authority())
                            .await
                    }
                });
                assert!(futures::poll!(registration.as_mut()).is_pending());
                if before_ack {
                    // Cancellation after queue admission must dispose the result on the writer.
                    drop(registration);
                    release.send(()).unwrap();
                    blocked.await.unwrap().unwrap();
                } else {
                    release.send(()).unwrap();
                    blocked.await.unwrap().unwrap();
                    // This FIFO round trip proves registration committed while its ACK is unread.
                    worker.state().await.unwrap();
                    assert!(registry.principal_available(&run));
                    drop(registration);
                    assert!(!registry.principal_available(&run));
                }
                tokio::time::timeout(Duration::from_secs(5), async {
                    while !registry.lookup_run(&run).is_some_and(|identity| {
                        matches!(identity.execution_state(), FlowExecutionState::Terminal)
                    }) {
                        tokio::task::yield_now().await;
                    }
                })
                .await
                .unwrap();
                worker.state().await.unwrap();
                assert!(
                    !registry
                        .lifecycle_writers
                        .lock()
                        .unwrap()
                        .contains_key(&run)
                );
            }
        }
        let child = FlowRunId::now();
        let (_, child_lifecycle) = registry
            .register_child_owned(
                &parent,
                child.clone(),
                InvocationKind::InlineSubflow,
                false,
                ChildWorkspaceAuthority::Inherit,
            )
            .await
            .unwrap();
        let (release, blocked) = hold_writer(&worker).await;
        let mut waiting = Box::pin(registry.block_on_descendant_owned(&parent, &child));
        assert!(futures::poll!(waiting.as_mut()).is_pending());
        release.send(()).unwrap();
        blocked.await.unwrap().unwrap();
        worker.state().await.unwrap();
        assert!(matches!(
            parent_identity.execution_state(),
            FlowExecutionState::BlockedOnDescendants { .. }
        ));
        drop(waiting);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !matches!(
                parent_identity.execution_state(),
                FlowExecutionState::Running
            ) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        child_lifecycle.finish().await.unwrap();
        parent_lifecycle.finish().await.unwrap();
        assert!(registry.lifecycle_writers.lock().unwrap().is_empty());
        worker.shutdown().await.unwrap();
    }
}
