//! Per-instance serialized storage work without blocking asynchronous executor threads.

use std::{io, sync::Arc};

use atman_rt::{FlowInstance, TurnLease};
use tokio::sync::{mpsc, oneshot};

use super::publication::{MessagePublication, PublishedMessage};
use super::{FlowRef, FlowStore, FlowTransition, invalid};
use crate::event::AtmanUuid;
use crate::event_log::journal::CommitReceipt;
use crate::session::Session;
use crate::tool::ToolCtx;

enum Command {
    Apply(Box<dyn FnOnce(&mut FlowStore) + Send>),
    Stop(oneshot::Sender<FlowStore>),
}

/// Owns a flow journal and its in-memory conversation projection.
///
/// Queued commands finish even if their awaiting caller disappears. Dropping all handles drains accepted commands and releases ownership; idle workers do not occupy a blocking thread.
#[derive(Clone)]
pub struct FlowWorker {
    address: FlowRef,
    context: Arc<Session>,
    commands: mpsc::Sender<Command>,
}

impl FlowWorker {
    pub async fn start(store: FlowStore) -> io::Result<Self> {
        let (store, context) = tokio::task::spawn_blocking(move || {
            let context = Arc::new(Session::restore_instance_context(
                store.descriptor().address.clone(),
                store.context_messages(),
            ));
            (store, context)
        })
        .await
        .map_err(io::Error::other)?;
        let address = store.descriptor().address.clone();
        let (commands, mut receiver) = mpsc::channel::<Command>(64);
        tokio::spawn(async move {
            let mut store = store;
            let mut stopped = None;
            while let Some(command) = receiver.recv().await {
                let Command::Apply(command) = command else {
                    if let Command::Stop(reply) = command {
                        receiver.close();
                        if stopped.is_none() {
                            stopped = Some(reply);
                        }
                    }
                    continue;
                };
                // One journal owner at a time, with no session-wide lock across disk or execution.
                match tokio::task::spawn_blocking(move || {
                    command(&mut store);
                    store
                })
                .await
                {
                    Ok(returned) => store = returned,
                    Err(_) => return,
                }
            }
            if let Some(reply) = stopped {
                let _ = reply.send(store);
            }
        });
        Ok(Self {
            address,
            context,
            commands,
        })
    }

    pub fn address(&self) -> &FlowRef {
        &self.address
    }

    pub fn context(&self) -> Arc<Session> {
        self.context.clone()
    }

    async fn request<T: Send + 'static>(
        &self,
        command: impl FnOnce(&mut FlowStore) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let (reply, received) = oneshot::channel();
        self.commands
            .send(Command::Apply(Box::new(move |store| {
                let _ = reply.send(command(store));
            })))
            .await
            .map_err(|_| io::Error::other("flow storage worker stopped"))?;
        received
            .await
            .map_err(|_| io::Error::other("flow storage worker stopped before acknowledgement"))?
    }

    pub async fn state(&self) -> io::Result<FlowInstance<AtmanUuid>> {
        self.request(|store| Ok(store.state().clone())).await
    }

    /// Drain accepted work and return journal ownership. Other handles become unavailable.
    ///
    /// This stops storage service only; it neither cancels execution nor changes the recorded lifecycle.
    pub async fn shutdown(self) -> io::Result<FlowStore> {
        let (reply, received) = oneshot::channel();
        self.commands
            .send(Command::Stop(reply))
            .await
            .map_err(|_| io::Error::other("flow storage worker stopped"))?;
        received
            .await
            .map_err(|_| io::Error::other("flow storage worker stopped before handoff"))
    }

    pub async fn transition(
        &self,
        revision: u64,
        transition: FlowTransition,
    ) -> io::Result<CommitReceipt> {
        self.request(move |store| store.transition(revision, transition))
            .await
    }

    pub(crate) async fn validate_execution(&self, lease: TurnLease<AtmanUuid>) -> io::Result<()> {
        self.request(move |store| {
            if !store
                .state()
                .can_receive_at_boundary(&lease)
                .map_err(invalid)?
            {
                return Err(invalid("instance execution is not admitted"));
            }
            Ok(())
        })
        .await
    }

    pub(crate) async fn publish(
        &self,
        publication: MessagePublication,
        mut context: ToolCtx,
    ) -> io::Result<PublishedMessage> {
        if publication.lease.flow != self.address {
            return Err(invalid("publication targets another worker"));
        }
        // Commands must not retain the writer through their projection callback.
        context.instance_publication = None;
        self.request(move |store| publish_and_project(store, publication, &context))
            .await
    }

    pub(crate) async fn record_context(
        &self,
        lease: TurnLease<AtmanUuid>,
        mut context: ToolCtx,
        specs: Vec<crate::context_plan::ContextRecordSpec>,
    ) -> io::Result<Vec<crate::context_plan::ContextRecord>> {
        context.instance_publication = None;
        self.request(move |store| {
            let run = context
                .flow_run_id
                .clone()
                .ok_or_else(|| invalid("message producer is unavailable"))?;
            let records = {
                let messages = context
                    .session_messages_handle
                    .as_ref()
                    .ok_or_else(|| invalid("session message context is unavailable"))?;
                crate::context_plan::compile_context_records(&messages.lock().unwrap(), specs)
            };
            for record in &records {
                publish_and_project(
                    store,
                    MessagePublication {
                        id: uuid::Uuid::now_v7(),
                        lease: lease.clone(),
                        run: run.clone(),
                        context_run: matches!(
                            context.history_segment,
                            crate::tool::HistorySegment::Spawned
                        )
                        .then(|| run.clone()),
                        message: crate::message::Message::context_record(
                            lease.turn.clone(),
                            record.clone(),
                        ),
                    },
                    &context,
                )?;
            }
            Ok(records)
        })
        .await
    }
}

fn publish_and_project(
    store: &mut FlowStore,
    publication: MessagePublication,
    context: &ToolCtx,
) -> io::Result<PublishedMessage> {
    let before = store.cursor().sequence;
    let record = store.publish_message(publication)?;
    if record.receipt.sequence > before {
        // Projection completes before the acknowledgement, including when its receiver is gone.
        crate::tools::session::append_message_to_context(
            context,
            record.publication.message.clone(),
        )
        .map_err(io::Error::other)?;
    }
    Ok(record)
}

/// An invocation-owned writer binding; old callbacks keep their old lease.
#[derive(Clone)]
pub(crate) struct InstancePublication {
    pub worker: FlowWorker,
    pub lease: TurnLease<AtmanUuid>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::flow_store::{
        EntrySnapshot, FlowDescriptor, FlowInstanceId, RuntimeStore, SessionDescriptor,
    };
    use crate::message::Message;
    use crate::source_program::RevisionBundle;
    use atman_rt::{RunId, TurnId};
    use uuid::Uuid;

    #[tokio::test]
    async fn accepted_publication_survives_caller_cancellation_and_rejected_writes_stay_invisible()
    {
        let dir = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(dir.path()).unwrap();
        let session_id = Uuid::new_v4();
        let mut session = root
            .create_session(SessionDescriptor {
                id: session_id,
                project_root: None,
                title: "Worker".into(),
            })
            .unwrap();
        let address = FlowRef {
            session_id: session_id.into(),
            instance_id: FlowInstanceId::now(),
        };
        let mut store = session
            .create_flow(FlowDescriptor {
                address: address.clone(),
                creation_request: Uuid::new_v4(),
                created_by: None,
                entry: EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: RevisionBundle {
                        entry: "main.at".into(),
                        sources: [("main.at".into(), "flow main() { }".into())].into(),
                        edges: Default::default(),
                    },
                },
            })
            .unwrap();
        store
            .transition(store.state().revision(), FlowTransition::Activated)
            .unwrap();
        let lease = store
            .state()
            .clone()
            .begin_turn(TurnId::now(), RunId::now())
            .unwrap();
        store
            .transition(
                store.state().revision(),
                FlowTransition::TurnStarted {
                    turn: lease.turn.clone(),
                    run: lease.run.clone(),
                },
            )
            .unwrap();
        let worker = FlowWorker::start(store).await.unwrap();
        let context = worker.context();
        let ctx = ToolCtx::new().with_session_runtime(context.clone());
        let publication = MessagePublication {
            id: Uuid::new_v4(),
            lease: lease.clone(),
            run: lease.run.clone(),
            context_run: None,
            message: Message::assistant_text(lease.turn.clone(), "committed"),
        };

        let (started, waiting) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let blocking_worker = worker.clone();
        let blocker = tokio::spawn(async move {
            blocking_worker
                .request(move |_| {
                    let _ = started.send(());
                    blocked
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .unwrap();
                    Ok(())
                })
                .await
        });
        waiting.await.unwrap();
        let writer = worker.clone();
        let body = publication.clone();
        let destination = ctx.clone();
        let abandoned = tokio::spawn(async move { writer.publish(body, destination).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while worker.commands.capacity() == 64 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        abandoned.abort();
        assert!(abandoned.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        blocker.await.unwrap().unwrap();
        worker.state().await.unwrap();
        assert_eq!(context.messages().len(), 1);
        let record = worker
            .publish(publication.clone(), ctx.clone())
            .await
            .unwrap();
        assert_eq!(context.messages().len(), 1);
        let mut conflict = publication.clone();
        conflict.message.parts.clear();
        assert!(worker.publish(conflict, ctx.clone()).await.is_err());
        let mut cancelled = publication;
        cancelled.id = Uuid::new_v4();
        worker
            .transition(
                worker.state().await.unwrap().revision(),
                FlowTransition::PhaseChanged {
                    lease,
                    phase: atman_rt::TurnPhase::Cancelling,
                },
            )
            .await
            .unwrap();
        assert!(
            worker
                .publish(cancelled.clone(), ctx.clone())
                .await
                .is_err()
        );
        assert_eq!(context.messages().len(), 1);
        let clone = worker.clone();
        let store = worker.shutdown().await.unwrap();
        assert!(clone.publish(cancelled, ctx).await.is_err());
        assert_eq!(store.published_messages(), [record]);
        assert_eq!(store.context_messages(), context.messages().to_vec());
    }
}
