//! Per-instance serialized storage work without blocking asynchronous executor threads.

pub mod access;
pub mod catalog;
pub(crate) mod context;
pub mod entry;
pub mod execution;
pub mod goal;
pub mod host;
pub mod input;
pub mod launch;
mod lifecycle;
pub mod live;
pub mod observation;
pub mod planning;
mod process_watch;
mod quiescence;
pub mod reply;
pub mod resources;
pub mod scheduler;
pub mod send;
mod trace;
mod view;
pub use view::{FlowView, FlowViewReader, FlowViewSnapshot};

use std::{io, sync::Arc};

use atman_rt::{FlowInstance, TurnLease};
use tokio::sync::{mpsc, oneshot, watch};

use crate::event::AtmanUuid;
use crate::event_log::journal::CommitReceipt;
use crate::flow_store::publication::{MessagePublication, PublishedMessage};
use crate::flow_store::{FlowRef, FlowStore, FlowTransition, invalid};
use crate::session::Session;
use crate::tool::ToolCtx;

type Acknowledge = Box<dyn FnOnce() + Send>;

/// An atomic scheduling read; pending input alone does not grant permission to execute.
#[derive(Clone, Debug)]
pub struct FlowSnapshot {
    pub state: FlowInstance<AtmanUuid>,
    pub cursor: CommitReceipt,
    pub pending: Vec<crate::flow_store::inbox::AcceptedEnvelope>,
    pub last_invocation: Option<entry::AdmittedInvocation>,
}

/// One committed status projection for host views. Scheduler ownership is observed separately by the session catalog.
#[derive(Clone, Debug)]
pub struct InstanceStatus {
    pub execution: FlowInstance<AtmanUuid>,
    pub goal: Option<crate::memory::goal::GoalRecord>,
    pub completion: Option<crate::flow_store::completion::CommittedCompletion>,
    pub cursor: CommitReceipt,
}

impl InstanceStatus {
    fn from_store(store: &FlowStore) -> Self {
        Self {
            execution: store.state().clone(),
            goal: store.goal().cloned(),
            completion: store.completion(None).cloned(),
            cursor: store.cursor().clone(),
        }
    }
}

enum Command {
    FlushTrace,
    Apply(Box<dyn FnOnce(&mut FlowStore) -> Acknowledge + Send>),
    Stop(oneshot::Sender<FlowStore>),
    ClaimExecution {
        lease: TurnLease<AtmanUuid>,
        reply: oneshot::Sender<io::Result<()>>,
    },
}

/// Owns a flow journal and its in-memory conversation projection.
///
/// Queued commands finish even if their awaiting caller disappears. Dropping all handles drains accepted commands and releases ownership; idle workers do not occupy a blocking thread.
#[derive(Clone)]
pub struct FlowWorker {
    host: Option<host::WeakRuntimeHost>,
    address: FlowRef,
    context: Arc<Session>,
    resources: Arc<resources::InstanceResources>,
    trace: Arc<trace::TraceBuffer>,
    live_output: Arc<live::LiveOutput>,
    commands: mpsc::Sender<Command>,
    state: watch::Receiver<FlowInstance<AtmanUuid>>,
    commits: watch::Receiver<CommitReceipt>,
    scheduler_owned: Arc<std::sync::atomic::AtomicBool>,
    scheduling_progress: watch::Sender<scheduler::SchedulingProgress>,
    stopping: Arc<std::sync::atomic::AtomicBool>,
    launch_gate: Arc<tokio::sync::Mutex<()>>,
    close_gate: Arc<tokio::sync::Mutex<()>>,
    close_error: watch::Sender<Option<String>>,
    observation_issues: watch::Sender<Vec<observation::ObservationIssue>>,
    observation_retry: watch::Sender<()>,
}

impl FlowWorker {
    pub(super) fn is_stopping(&self) -> bool {
        self.stopping.load(std::sync::atomic::Ordering::Acquire)
            || self
                .host
                .as_ref()
                .is_some_and(|host| host.upgrade().is_err())
    }

    pub(crate) fn is_stopped(&self) -> bool {
        self.commands.is_closed()
    }

    pub async fn start(store: FlowStore) -> io::Result<Self> {
        Self::start_with_redactor(store, None).await
    }

    /// Bind service diagnostic redaction for this writer's lifetime, including idle periods and late audit decisions.
    pub async fn start_with_redactor(
        store: FlowStore,
        redactor: Option<Arc<crate::redact::Redactor>>,
    ) -> io::Result<Self> {
        Self::start_owned(store, None, redactor).await
    }

    async fn start_owned(
        store: FlowStore,
        host: Option<host::WeakRuntimeHost>,
        redactor: Option<Arc<crate::redact::Redactor>>,
    ) -> io::Result<Self> {
        let trace = Arc::new(trace::TraceBuffer::default());
        let mut sink = crate::event::EventSink::new();
        if let Some(redactor) = redactor {
            sink = sink.with_redactor(redactor);
        }
        let sink = trace.bind(
            sink,
            crate::flow_store::trace::ExecutionScope::Instance(store.descriptor().address.clone()),
        );
        let (store, context) = tokio::task::spawn_blocking(move || {
            let mut store = store;
            store.lose_process_sources()?;
            let mut context = Session::restore_instance_context(
                sink,
                store.descriptor().address.clone(),
                store.session_descriptor().configuration.clone(),
                store.session_descriptor().project.clone(),
                store.context_messages(),
                store.history_messages(),
                store
                    .context_checkpoints()
                    .last()
                    .map(|saved| saved.checkpoint.messages.as_slice()),
                store.goal().cloned(),
            );
            context.bind_instance_directory(store.dir());
            let context = Arc::new(context);
            context.project_instance_planning(store.planning());
            let sequence = store
                .execution_events()
                .last()
                .and_then(|batch| batch.events.last())
                .map_or(0, |event| event.sequence);
            let next = context.sink().next_seq_peek();
            context
                .sink()
                .restore_seq(sequence.max(next.saturating_sub(1)));
            Ok::<_, io::Error>((store, context))
        })
        .await
        .map_err(io::Error::other)??;
        let address = store.descriptor().address.clone();
        let resources = Arc::new(resources::InstanceResources::default());
        if matches!(
            store.state().state(),
            atman_rt::InstanceState::Closing { .. } | atman_rt::InstanceState::Closed
        ) {
            resources.close();
        }
        let (state_tx, state) = watch::channel(store.state().clone());
        let (commits_tx, commits) = watch::channel(store.cursor().clone());
        let (commands, mut receiver) = mpsc::channel::<Command>(64);
        context.bind_instance_writer(context::ContextWriter::new(
            commands.downgrade(),
            Arc::downgrade(&context),
        ));
        let resource_owner = resources::ResourceOwner(resources.clone());
        let trace_owner = trace::TraceOwner(trace.clone());
        tokio::spawn(async move {
            let mut store = store;
            let mut stopped = None;
            let mut claimed = None;
            loop {
                let command = tokio::select! {
                    command = receiver.recv() => match command {
                        Some(command) => command,
                        None => break,
                    },
                    _ = trace_owner.0.wake.notified() => Command::FlushTrace,
                };
                let command = match command {
                    Command::FlushTrace => {
                        Box::new(|_: &mut FlowStore| Box::new(|| {}) as Acknowledge)
                            as Box<dyn FnOnce(&mut FlowStore) -> Acknowledge + Send>
                    }
                    Command::Apply(command) => command,
                    Command::Stop(reply) => {
                        receiver.close();
                        if stopped.is_none() {
                            stopped = Some(reply);
                        }
                        continue;
                    }
                    Command::ClaimExecution { lease, reply } => {
                        let result = if claimed.as_ref() == Some(&lease) {
                            Err(invalid("instance turn execution was already claimed"))
                        } else if matches!(store.state().state(), atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) if active.lease == lease && active.phase == atman_rt::TurnPhase::Running)
                        {
                            claimed = Some(lease);
                            Ok(())
                        } else {
                            Err(invalid("instance execution is not admitted"))
                        };
                        let _ = reply.send(result);
                        continue;
                    }
                };
                // One journal owner at a time, with no session-wide lock across disk or execution.
                let state_tx = state_tx.clone();
                let commits_tx = commits_tx.clone();
                let resources = resource_owner.0.clone();
                let trace = trace_owner.0.clone();
                match tokio::task::spawn_blocking(move || {
                    trace.flush(&mut store)?;
                    let acknowledge = command(&mut store);
                    if matches!(
                        store.state().state(),
                        atman_rt::InstanceState::Closing { .. } | atman_rt::InstanceState::Closed
                    ) {
                        resources.close();
                    }
                    state_tx.send_if_modified(|snapshot| {
                        if snapshot == store.state() {
                            return false;
                        }
                        *snapshot = store.state().clone();
                        true
                    });
                    commits_tx.send_if_modified(|cursor| {
                        if cursor == store.cursor() {
                            return false;
                        }
                        *cursor = store.cursor().clone();
                        true
                    });
                    acknowledge();
                    Ok::<_, io::Error>(store)
                })
                .await
                {
                    Ok(Ok(returned)) => store = returned,
                    Ok(Err(error)) => {
                        crate::notify!(error, "flow execution journal failed: {error}");
                        return;
                    }
                    Err(_) => return,
                }
            }
            resource_owner.0.shutdown().await;
            trace_owner.0.close();
            let trace = trace_owner.0.clone();
            let Ok(Ok(store)) = tokio::task::spawn_blocking(move || {
                trace.flush(&mut store)?;
                Ok::<_, io::Error>(store)
            })
            .await
            else {
                return;
            };
            if let Some(reply) = stopped {
                let _ = reply.send(store);
            }
        });
        let worker = Self {
            host,
            live_output: Arc::new(live::LiveOutput::new(address.clone())),
            address,
            context,
            resources,
            trace,
            commands,
            state,
            commits,
            scheduler_owned: Default::default(),
            scheduling_progress: watch::channel(scheduler::SchedulingProgress::Stopped).0,
            stopping: Default::default(),
            launch_gate: Default::default(),
            close_gate: Default::default(),
            close_error: watch::channel(None).0,
            observation_issues: watch::channel(Vec::new()).0,
            observation_retry: watch::channel(()).0,
        };
        if matches!(
            worker.state.borrow().state(),
            atman_rt::InstanceState::Closing { .. }
        ) {
            worker.continue_close();
        }
        Ok(worker)
    }

    pub fn address(&self) -> &FlowRef {
        &self.address
    }

    pub(crate) fn peer_client(
        &self,
        lease: TurnLease<AtmanUuid>,
    ) -> io::Result<Option<access::FlowClient>> {
        self.host
            .as_ref()
            .map(|host| access::FlowClient::bind(host.upgrade()?, self, lease))
            .transpose()
    }

    pub fn context(&self) -> Arc<Session> {
        self.context.clone()
    }

    pub fn resources(&self) -> &Arc<resources::InstanceResources> {
        &self.resources
    }

    pub(crate) fn bind_resources(&self, ctx: &mut ToolCtx) {
        self.resources.bind(ctx);
    }

    pub(super) async fn request<T: Send + 'static>(
        &self,
        command: impl FnOnce(&mut FlowStore) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        request(&self.commands, command).await
    }

    pub async fn state(&self) -> io::Result<FlowInstance<AtmanUuid>> {
        self.request(|store| Ok(store.state().clone())).await
    }

    pub async fn status(&self) -> io::Result<InstanceStatus> {
        self.request(|store| Ok(InstanceStatus::from_store(store)))
            .await
    }

    pub fn subscribe_state(&self) -> watch::Receiver<FlowInstance<AtmanUuid>> {
        self.state.clone()
    }

    /// Observe durable changes, including inbox arrival without a lifecycle transition.
    ///
    /// Subscribe before inspecting. Mark the current revision seen with `borrow_and_update`, inspect, then await `changed` if no work can be admitted. Notifications coalesce; the journal and a fresh snapshot determine actual work. Reads, rejected writes, and matching retries do not generate changes.
    pub fn subscribe_commits(&self) -> watch::Receiver<CommitReceipt> {
        self.commits.clone()
    }

    pub async fn snapshot(&self) -> io::Result<FlowSnapshot> {
        self.request(|store| {
            let command = store.last_command_entry()?.cloned();
            Ok(FlowSnapshot {
                state: store.state().clone(),
                cursor: store.cursor().clone(),
                pending: store.pending_inputs().cloned().collect(),
                last_invocation: store.last_invocation_input().map(|record| {
                    let crate::flow_store::inbox::InputContent::InvocationInput {
                        arguments,
                        invocation,
                        ..
                    } = &record.input.content
                    else {
                        unreachable!("invocation index points to invocation input")
                    };
                    entry::AdmittedInvocation {
                        command,
                        lease: record.input.lease.clone(),
                        arguments: arguments.clone(),
                        invocation: invocation.clone(),
                    }
                }),
            })
        })
        .await
    }

    /// Drain accepted work and return journal ownership. Other handles become unavailable.
    ///
    /// Stop active execution before handing off storage. Loss of storage cancels bound execution, but does not manufacture a terminal lifecycle fact.
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
        let resources = self.resources.clone();
        self.request(move |store| lifecycle::transition(store, &resources, revision, transition))
            .await
    }

    pub(crate) async fn validate_execution(&self, lease: TurnLease<AtmanUuid>) -> io::Result<()> {
        let (reply, received) = oneshot::channel();
        self.commands
            .send(Command::ClaimExecution { lease, reply })
            .await
            .map_err(|_| io::Error::other("flow storage worker stopped"))?;
        received
            .await
            .map_err(|_| io::Error::other("flow storage worker stopped before execution claim"))?
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

async fn request<T: Send + 'static>(
    commands: &mpsc::Sender<Command>,
    command: impl FnOnce(&mut FlowStore) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let (reply, received) = oneshot::channel();
    commands
        .send(Command::Apply(Box::new(move |store| {
            let result = command(store);
            Box::new(move || {
                let _ = reply.send(result);
            })
        })))
        .await
        .map_err(|_| io::Error::other("flow storage worker stopped"))?;
    received
        .await
        .map_err(|_| io::Error::other("flow storage worker stopped before acknowledgement"))?
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

    async fn cancel_queued<T: Send + std::fmt::Debug + 'static>(
        worker: &FlowWorker,
        operation: impl Future<Output = T> + Send + 'static,
    ) {
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
        let abandoned = tokio::spawn(operation);
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
    }

    #[tokio::test]
    async fn accepted_publication_survives_caller_cancellation_and_rejected_writes_stay_invisible()
    {
        let dir = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(dir.path()).unwrap();
        let session_id = Uuid::new_v4();
        let mut session = root
            .create_session(SessionDescriptor {
                configuration: Default::default(),
                id: session_id,
                project: None,
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
            message: Message::assistant_text(lease.turn.clone(), "committed ".repeat(50)),
        };
        let writer = worker.clone();
        let body = publication.clone();
        let destination = ctx.clone();
        cancel_queued(
            &worker,
            async move { writer.publish(body, destination).await },
        )
        .await;
        assert_eq!(context.messages().len(), 1);
        let record = worker
            .publish(publication.clone(), ctx.clone())
            .await
            .unwrap();
        assert_eq!(context.messages().len(), 1);
        let mut conflict = publication.clone();
        conflict.message.parts.clear();
        assert!(worker.publish(conflict, ctx.clone()).await.is_err());

        let original = context.messages().to_vec();
        let replacement = vec![Message::assistant_text(lease.turn.clone(), "short")];
        let checkpoint_context = context.clone();
        let checkpoint_messages = replacement.clone();
        cancel_queued(&worker, async move {
            checkpoint_context
                .commit_rewritten_window(&original, checkpoint_messages, 100, 1)
                .await
        })
        .await;
        assert_eq!(context.messages().to_vec(), replacement);
        assert_eq!(
            context.messages_full().as_slice(),
            std::slice::from_ref(&record.publication.message)
        );
        assert_eq!(
            context.context_epoch(),
            Some(crate::flow_store::context::context_digest(&replacement).unwrap(),)
        );
        let user = crate::flow_store::inbox::SenderIdentity::User(Uuid::new_v4());
        let goal_request = crate::flow_store::goal::GoalRequest {
            start_turn: false,
            id: Uuid::new_v4(),
            sender: user.clone(),
            expected: None,
            edit: crate::memory::goal::GoalEdit::Create {
                text: "durable objective".into(),
            },
        };
        let mut goals = context.subscribe_goal();
        let goal_writer = worker.clone();
        let abandoned_goal = goal_request.clone();
        cancel_queued(&worker, async move {
            goal_writer.edit_goal(abandoned_goal, |_| Ok(())).await
        })
        .await;
        assert_eq!(context.goal().as_deref(), Some("durable objective"));
        assert!(goals.has_changed().unwrap());
        assert_eq!(
            goals.borrow_and_update().as_deref(),
            Some("durable objective")
        );
        let goal = worker.goal().await.unwrap().goal.unwrap();
        assert_eq!(context.goal_control.record(), Some(goal.clone()));
        worker
            .edit_goal(
                crate::flow_store::goal::GoalRequest {
                    start_turn: false,
                    id: Uuid::new_v4(),
                    sender: user,
                    expected: Some((&goal).into()),
                    edit: crate::memory::goal::GoalEdit::Clear,
                },
                |_| Ok(()),
            )
            .await
            .unwrap();
        let retried = worker
            .edit_goal(goal_request, |_| unreachable!())
            .await
            .unwrap();
        assert_eq!(retried.goal, Some(goal));
        assert_eq!(context.goal(), None);
        assert_eq!(*goals.borrow(), None);
        assert_eq!(context.goal_control.record(), None);
        let request = crate::flow_store::planning::PlanningRequest {
            id: Uuid::new_v4(),
            sender: crate::flow_store::inbox::SenderIdentity::User(Uuid::new_v4()),
            expected_revision: Some(0),
            edit: crate::flow_store::planning::PlanningEdit::WritePlan {
                id: "route".into(),
                title: "durable plan".into(),
                steps: vec!["finish".into()],
            },
        };
        let mut plans = context.subscribe_plans();
        let planning_writer = worker.clone();
        let abandoned_plan = request.clone();
        cancel_queued(&worker, async move {
            planning_writer
                .edit_planning(abandoned_plan, |_| Ok(()))
                .await
        })
        .await;
        assert!(plans.has_changed().unwrap());
        assert_eq!(plans.borrow_and_update()[0].title, "durable plan");
        worker
            .edit_planning(request.clone(), |_| unreachable!())
            .await
            .unwrap();
        assert!(
            !plans.has_changed().unwrap(),
            "retry does not emit another projection"
        );
        let mut tick = request.clone();
        tick.id = Uuid::new_v4();
        tick.expected_revision = Some(1);
        tick.edit = crate::flow_store::planning::PlanningEdit::TickPlan {
            id: None,
            step_index: 0,
        };
        worker
            .edit_planning(tick.clone(), |_| Err(std::io::Error::other("denied")))
            .await
            .unwrap_err();
        assert!(!plans.has_changed().unwrap());
        worker.edit_planning(tick, |_| Ok(())).await.unwrap();
        assert!(plans.borrow_and_update()[0].steps[0].done);
        worker
            .edit_planning(request, |_| unreachable!())
            .await
            .unwrap();
        assert!(!plans.has_changed().unwrap());
        assert!(
            plans.borrow()[0].steps[0].done,
            "old receipt cannot roll back live planning"
        );
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
        assert_eq!(store.context_checkpoints().len(), 1);
        assert_eq!(store.goal_updates().len(), 2);
        assert_eq!(store.context_messages(), context.messages().to_vec());
        let weak = Arc::downgrade(&context);
        drop(clone);
        drop(context);
        assert!(
            weak.upgrade().is_none(),
            "context writer must not retain its owner"
        );
    }
}
