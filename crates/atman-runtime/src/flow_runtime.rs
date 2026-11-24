//! Per-instance serialized storage work without blocking asynchronous executor threads.

pub mod access;
pub mod activity;
pub mod catalog;
pub(crate) mod context;
pub mod entry;
pub mod execution;
pub mod goal;
mod handoff_storage;
mod history_operation;
pub mod host;
pub mod idle_context;
pub mod input;
mod input_preferences;
mod interactions;
pub mod launch;
mod lifecycle;
pub(crate) mod lifecycle_admission;
pub mod live;
pub mod observation;
pub mod peer;
pub mod peer_profile;
pub mod planning;
mod process_watch;
pub mod question;
pub use process_watch::ProcessWatchSource;
mod quiescence;
pub mod reply;
pub mod resources;
pub mod scheduler;
pub mod send;
pub mod source_capture;
pub mod task_scope;
pub mod timer;
mod trace;
mod view;
pub use view::{FlowView, FlowViewReader, FlowViewSnapshot, PENDING_VIEW_PAGE_SIZE};

use std::{io, sync::Arc};

use atman_rt::{FlowInstance, TurnLease};
use tokio::sync::{mpsc, oneshot, watch};

use crate::event::AtmanUuid;
use crate::flow_store::position::FlowPosition;
use crate::flow_store::publication::{MessagePublication, PublishedMessage};
use crate::flow_store::{FlowRef, FlowStore, FlowTransition, invalid};
use crate::session::Session;
use crate::tool::ToolCtx;

type Acknowledge = Box<dyn FnOnce() + Send>;
type StorageCommand =
    Box<dyn FnOnce(&mut FlowStore, io::Result<&task_scope::ScopeSnapshot>) -> Acknowledge + Send>;

enum PublicationWitness {
    Provider(live::LiveCall),
    Deferred(live::DeferredProviderDraft),
    Final(live::PreparedFinalDraft),
}
type StorageFailure = std::sync::OnceLock<Arc<io::Error>>;

fn storage_error(failure: &StorageFailure, fallback: io::Error) -> io::Error {
    failure.get().map_or(fallback, |cause| {
        io::Error::new(cause.kind(), cause.clone())
    })
}

fn record_storage_failure(failure: &StorageFailure, error: io::Error) -> io::Error {
    let cause = failure.get_or_init(|| Arc::new(error));
    io::Error::new(cause.kind(), cause.clone())
}

/// An atomic scheduling read; pending input alone does not grant permission to execute.
#[derive(Clone, Debug)]
pub struct FlowSnapshot {
    pub state: FlowInstance<AtmanUuid>,
    pub cursor: FlowPosition,
    pub pending: Vec<crate::flow_store::inbox::AcceptedEnvelope>,
    pub last_invocation: Option<entry::AdmittedInvocation>,
}

impl FlowSnapshot {
    fn state_only(store: &FlowStore) -> Self {
        Self {
            state: store.state().clone(),
            cursor: store.cursor().clone(),
            pending: Vec::new(),
            last_invocation: None,
        }
    }

    fn read_pending(
        store: &mut FlowStore,
        eligible: impl Fn(&crate::flow_store::inbox::InputEnvelope) -> bool,
    ) -> io::Result<Self> {
        let ids: Vec<_> = store
            .pending_inputs_at_head()
            .filter(|input| eligible(input))
            .map(|input| input.id)
            .collect();
        let pending = ids
            .into_iter()
            .map(|id| {
                store
                    .inbox_entry(id)?
                    .map(|entry| entry.envelope)
                    .ok_or_else(|| invalid("pending input disappeared"))
            })
            .collect::<io::Result<Vec<_>>>()?;
        let command = store.last_command_entry()?;
        Ok(Self {
            pending,
            last_invocation: store.last_invocation_input()?.map(|record| {
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
            ..Self::state_only(store)
        })
    }
}

/// One committed status projection for host views. Scheduler ownership is observed separately by the session catalog.
#[derive(Clone, Debug)]
pub struct InstanceStatus {
    pub history_head: Option<crate::flow_store::branches::HistoryHead>,
    pub execution: FlowInstance<AtmanUuid>,
    pub goal: Option<crate::memory::goal::GoalRecord>,
    pub completion: Option<crate::flow_store::completion::CommittedCompletion>,
    pub cursor: FlowPosition,
}

impl InstanceStatus {
    fn from_store(store: &FlowStore) -> Self {
        Self {
            history_head: store.conversation_head(),
            execution: store.state().clone(),
            goal: store.goal().cloned(),
            completion: store.completion(None).cloned(),
            cursor: store.cursor().clone(),
        }
    }
}

enum Command {
    Apply {
        scope: task_scope::ScopeAdmission,
        command: StorageCommand,
    },
    Inspect(Box<dyn FnOnce(&FlowStore) -> Acknowledge + Send>),
    Stop {
        reply: oneshot::Sender<FlowStore>,
        admission: Option<host::AdmissionPermit>,
        authority: Option<Arc<crate::peer_authority::AuthorityGuard>>,
    },
    SuspendHandoff(oneshot::Sender<io::Result<handoff_storage::FlowHandoffSuspension>>),
    ClaimExecution {
        lease: TurnLease<AtmanUuid>,
        scope: task_scope::ScopeAdmission,
        reply: oneshot::Sender<io::Result<tokio_util::task::task_tracker::TaskTrackerToken>>,
        admission: host::AdmissionPermit,
        authority: Option<Arc<crate::peer_authority::AuthorityGuard>>,
    },
}

/// Owns a flow journal and its in-memory conversation projection.
///
/// Queued commands finish even if their awaiting caller disappears. Dropping all handles drains accepted commands and releases ownership; idle workers do not occupy a blocking thread.
#[derive(Clone)]
pub struct FlowWorker {
    pub(crate) preference_binding: uuid::Uuid,
    scope_state: Arc<std::sync::RwLock<task_scope::ScopeState>>,
    host: Option<host::WeakRuntimeHost>,
    address: FlowRef,
    context: Arc<Session>,
    resources: Arc<resources::InstanceResources>,
    trace: Arc<trace::TraceBuffer>,
    live_output: Arc<live::LiveOutput>,
    commands: mpsc::Sender<Command>,
    admission: Arc<host::SessionAdmission>,
    failure: Arc<StorageFailure>,
    state: watch::Receiver<FlowInstance<AtmanUuid>>,
    commits: watch::Receiver<FlowPosition>,
    name: watch::Receiver<String>,
    scheduler_owned: Arc<std::sync::atomic::AtomicBool>,
    scheduling_progress: watch::Sender<scheduler::SchedulingProgress>,
    stopping: Arc<std::sync::atomic::AtomicBool>,
    launch_gate: Arc<tokio::sync::Mutex<()>>,
    close_gate: Arc<tokio::sync::Mutex<()>>,
    close_error: watch::Sender<Option<String>>,
    observation_issues: watch::Sender<Vec<observation::ObservationIssue>>,
    delivery_issues: watch::Sender<Vec<send::DeliveryIssue>>,
    observation_retry: watch::Sender<()>,
    reply_claims: Arc<reply::ReplyClaims>,
}

impl FlowWorker {
    pub fn is_transient(&self) -> bool {
        self.context.storage_directory().is_none()
    }

    pub(crate) fn is_stopping(&self) -> bool {
        self.stopping.load(std::sync::atomic::Ordering::Acquire)
            || self
                .host
                .as_ref()
                .is_some_and(|host| host.upgrade().is_err())
    }

    /// Whether this storage owner has stopped accepting commands. Reopen through the Session owner to obtain a new handle.
    pub fn is_stopped(&self) -> bool {
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
        Self::start_owned(store, None, redactor, None).await
    }

    async fn start_owned(
        store: FlowStore,
        host: Option<host::WeakRuntimeHost>,
        redactor: Option<Arc<crate::redact::Redactor>>,
        input_preferences: Option<
            watch::Receiver<crate::user_input::preferences::EffectiveInputPreferences>,
        >,
    ) -> io::Result<Self> {
        let resources = Arc::new(resources::InstanceResources::default());
        let retain_compact_reviews = host
            .as_ref()
            .and_then(|host| host.upgrade().ok())
            .is_some_and(|host| host.retains_compact_reviews());
        let review_lifetime = resources.lifetime.clone();
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
            store.reconcile_history_stop()?;
            let mut context = Session::restore_instance_context(
                sink,
                store.descriptor().address.clone(),
                store.session_descriptor().configuration.clone(),
                store.project().cloned(),
                store.context_messages()?,
                store.history_messages()?,
                store
                    .latest_context_checkpoint()
                    .map(|saved| saved.checkpoint.messages.as_slice()),
                store.goal().cloned(),
                Some(store.history_head()),
            );
            context.bind_instance_storage(store.directory(), store.output_store());
            context.bind_input_preferences(input_preferences);
            if store.transient_retention().is_none() {
                context.configure_instance_compact_reviews(review_lifetime, retain_compact_reviews);
            }
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
        let admission = host
            .as_ref()
            .map(|host| {
                host.upgrade()?
                    .session(address.session_id.0)
                    .map(|session| session.admission())
            })
            .transpose()?
            .unwrap_or_default();
        if matches!(
            store.state().state(),
            atman_rt::InstanceState::Closing { .. } | atman_rt::InstanceState::Closed
        ) {
            resources.close();
        }
        let (state_tx, state) = watch::channel(store.state().clone());
        let (commits_tx, commits) = watch::channel(store.cursor().clone());
        let (name_tx, name) = watch::channel(store.display_title());
        let (commands, mut receiver) = mpsc::channel::<Command>(64);
        let failure = Arc::new(StorageFailure::new());
        let scope_state = Arc::new(std::sync::RwLock::new(task_scope::ScopeState::from_store(
            &store,
        )));
        let scope_owner = scope_state.clone();
        context.bind_instance_writer(context::ContextWriter::new(
            commands.downgrade(),
            Arc::downgrade(&context),
            failure.clone(),
            admission.clone(),
        ));
        let resource_owner = resources::ResourceOwner(resources.clone());
        let trace_owner = trace::TraceOwner(trace.clone());
        let trace_sender = commands.downgrade();
        let trace_flusher = trace.clone();
        let trace_admission = admission.clone();
        let trace_failure = failure.clone();
        let trace_task = tokio_util::task::AbortOnDropHandle::new(tokio::spawn(async move {
            loop {
                trace_flusher.wake.notified().await;
                let Some(sender) = trace_sender.upgrade() else {
                    break;
                };
                if request(&sender, &trace_failure, &trace_admission, |_| Ok(()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
        }));
        let live_output = Arc::new(live::LiveOutput::new(address.clone()));
        live_output.sync_store(&store);
        let live_owner = live_output.clone();
        let failure_owner = failure.clone();
        tokio::spawn(async move {
            let _trace_task = trace_task;
            let mut store = store;
            let mut stopped = None;
            let mut claimed = None;
            loop {
                let Some(command) = receiver.recv().await else {
                    break;
                };
                let (scope, command) = match command {
                    Command::Inspect(inspect) => {
                        inspect(&store)();
                        continue;
                    }
                    Command::SuspendHandoff(reply) => {
                        trace_owner.0.close();
                        let trace = trace_owner.0.clone();
                        let checked = tokio::task::spawn_blocking(move || {
                            let result = trace.flush(&mut store);
                            (store, result)
                        })
                        .await;
                        let (returned, result) = match checked {
                            Ok(checked) => checked,
                            Err(error) => {
                                let error =
                                    record_storage_failure(&failure_owner, io::Error::other(error));
                                let _ = reply.send(Err(error));
                                return;
                            }
                        };
                        store = returned;
                        if let Err(error) = result {
                            let error = record_storage_failure(&failure_owner, error);
                            let _ = reply.send(Err(error));
                            return;
                        }
                        commits_tx.send_if_modified(|cursor| {
                            if cursor == store.cursor() {
                                return false;
                            }
                            *cursor = store.cursor().clone();
                            true
                        });
                        let (suspended, resumed) = handoff_storage::suspend(
                            store,
                            resource_owner.0.clone(),
                            trace_owner.0.clone(),
                        );
                        if let Err(returned) = reply.send(Ok(suspended)) {
                            drop(returned);
                        }
                        match resumed.await {
                            Ok(Some(returned)) => {
                                store = returned;
                                continue;
                            }
                            _ => {
                                receiver.close();
                                resource_owner.0.shutdown().await;
                                return;
                            }
                        }
                    }
                    Command::Apply { scope, command } => (scope, command),
                    Command::Stop {
                        reply,
                        admission,
                        authority,
                    } => {
                        receiver.close();
                        if stopped.is_none() {
                            stopped = Some((reply, admission, authority));
                        }
                        continue;
                    }
                    Command::ClaimExecution {
                        lease,
                        scope,
                        reply,
                        admission: _admission,
                        authority: _authority,
                    } => {
                        let result = if resource_owner.0.lifetime.is_cancelled() {
                            Err(invalid("instance resources are stopping"))
                        } else if claimed.as_ref() == Some(&lease) {
                            Err(invalid("instance turn execution was already claimed"))
                        } else if matches!(store.state().state(), atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) if active.lease == lease && active.phase == atman_rt::TurnPhase::Running)
                        {
                            let resources = resource_owner.0.clone();
                            let owner = store.state().address().clone();
                            let scope_owner = scope_owner.clone();
                            let result = tokio::task::spawn_blocking(move || {
                                let transaction = task_scope::ScopeTransaction::acquire(
                                    &scope,
                                    &owner,
                                    &scope_owner,
                                )?;
                                scope.check(&transaction.snapshot(&scope))?;
                                Ok(resources.executions.token())
                            })
                            .await
                            .map_err(io::Error::other)
                            .and_then(|result| result);
                            if result.is_ok() {
                                claimed = Some(lease);
                            }
                            result
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
                let name_tx = name_tx.clone();
                let resources = resource_owner.0.clone();
                let trace = trace_owner.0.clone();
                let scope_owner = scope_owner.clone();
                let live = live_owner.clone();
                let failure = failure_owner.clone();
                match tokio::task::spawn_blocking(move || {
                    let owner = store.state().address().clone();
                    let mut transaction =
                        match task_scope::ScopeTransaction::acquire(&scope, &owner, &scope_owner) {
                            Ok(transaction) => transaction,
                            Err(error) => {
                                command(&mut store, Err(error))();
                                return Ok(store);
                            }
                        };
                    trace
                        .flush(&mut store)
                        .map_err(|error| record_storage_failure(&failure, error))?;
                    store.observe_retention_limit()?;
                    let snapshot = transaction.snapshot(&scope);
                    let acknowledge = command(&mut store, Ok(&snapshot));
                    store.reconcile_history_stop()?;
                    store.observe_retention_limit()?;
                    transaction.publish(&store);
                    drop(transaction);
                    if matches!(
                        store.state().state(),
                        atman_rt::InstanceState::Closing { .. } | atman_rt::InstanceState::Closed
                    ) {
                        resources.close();
                    }
                    live.sync_store(&store);
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
                    name_tx.send_if_modified(|name| {
                        let current = store.display_title();
                        if *name == current {
                            return false;
                        }
                        *name = current;
                        true
                    });
                    acknowledge();
                    Ok::<_, io::Error>(store)
                })
                .await
                {
                    Ok(Ok(returned)) => store = returned,
                    Ok(Err(error)) => {
                        let error = record_storage_failure(&failure_owner, error);
                        crate::notify!(error, "flow execution journal failed: {error}");
                        return;
                    }
                    Err(error) => {
                        record_storage_failure(&failure_owner, io::Error::other(error));
                        return;
                    }
                }
            }
            resource_owner.0.shutdown().await;
            trace_owner.0.close();
            let trace = trace_owner.0.clone();
            let store = match tokio::task::spawn_blocking(move || {
                trace.flush(&mut store)?;
                Ok::<_, io::Error>(store)
            })
            .await
            {
                Ok(Ok(store)) => store,
                Ok(Err(error)) => {
                    record_storage_failure(&failure_owner, error);
                    return;
                }
                Err(error) => {
                    record_storage_failure(&failure_owner, io::Error::other(error));
                    return;
                }
            };
            commits_tx.send_replace(store.cursor().clone());
            // Teardown publishes the authoritative cursor without repairing failed scope authority.
            // Recovering the value preserves the poison flag; new scope transactions still reject it.
            *scope_owner
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner) =
                task_scope::ScopeState::from_store(&store);
            if let Some((reply, _admission, _authority)) = stopped {
                let _ = reply.send(store);
            }
        });
        let worker = Self {
            preference_binding: uuid::Uuid::new_v4(),
            scope_state,
            host,
            live_output,
            address,
            context,
            resources,
            trace,
            commands,
            admission,
            failure,
            state,
            commits,
            name,
            scheduler_owned: Default::default(),
            scheduling_progress: watch::channel(scheduler::SchedulingProgress::Stopped).0,
            stopping: Default::default(),
            launch_gate: Default::default(),
            close_gate: Default::default(),
            close_error: watch::channel(None).0,
            observation_issues: watch::channel(Vec::new()).0,
            delivery_issues: watch::channel(Vec::new()).0,
            observation_retry: watch::channel(()).0,
            reply_claims: Default::default(),
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
        request(&self.commands, &self.failure, &self.admission, command).await
    }

    pub(crate) async fn request_with_authority<T: Send + 'static>(
        &self,
        authority: Arc<crate::peer_authority::AuthorityGuard>,
        command: impl FnOnce(&mut FlowStore) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        self.request_scoped_with_authority(authority, Default::default(), move |store, _| {
            command(store)
        })
        .await
    }

    pub(crate) async fn query_with_authority<T: Send + 'static>(
        &self,
        authority: Arc<crate::peer_authority::AuthorityGuard>,
        query: impl FnOnce(&FlowStore) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let admission = self.admission.enter().await?;
        self.admission.check_read_authority(&authority)?;
        self.inspect((admission, authority), query).await
    }

    async fn query<T: Send + 'static>(
        &self,
        query: impl FnOnce(&FlowStore) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        self.inspect(self.admission.enter().await?, query).await
    }

    async fn inspect<T: Send + 'static>(
        &self,
        guard: impl Send + 'static,
        query: impl FnOnce(&FlowStore) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let (reply, received) = oneshot::channel();
        self.commands
            .send(Command::Inspect(Box::new(move |store| {
                let result = query(store);
                Box::new(move || {
                    let _guard = guard;
                    let _ = reply.send(result);
                })
            })))
            .await
            .map_err(|_| self.storage_error(invalid("Flow read owner stopped")))?;
        received
            .await
            .map_err(|_| self.storage_error(invalid("Flow read did not acknowledge")))?
    }

    pub(crate) async fn request_scoped_with_authority<T: Send + 'static>(
        &self,
        authority: Arc<crate::peer_authority::AuthorityGuard>,
        scope: task_scope::ScopeAdmission,
        command: impl FnOnce(&mut FlowStore, &task_scope::ScopeSnapshot) -> io::Result<T>
        + Send
        + 'static,
    ) -> io::Result<T> {
        request_scoped(
            &self.commands,
            &self.failure,
            Some(&self.admission),
            Some(authority),
            scope,
            command,
        )
        .await
    }

    pub(super) async fn request_optional_authority<T: Send + 'static>(
        &self,
        authority: Option<Arc<crate::peer_authority::AuthorityGuard>>,
        scope: task_scope::ScopeAdmission,
        command: impl FnOnce(&mut FlowStore, &task_scope::ScopeSnapshot) -> io::Result<T>
        + Send
        + 'static,
    ) -> io::Result<T> {
        request_scoped(
            &self.commands,
            &self.failure,
            Some(&self.admission),
            authority,
            scope,
            command,
        )
        .await
    }

    async fn request_scoped<T: Send + 'static>(
        &self,
        scope: task_scope::ScopeAdmission,
        command: impl FnOnce(&mut FlowStore, &task_scope::ScopeSnapshot) -> io::Result<T>
        + Send
        + 'static,
    ) -> io::Result<T> {
        request_scoped(
            &self.commands,
            &self.failure,
            Some(&self.admission),
            None,
            scope,
            command,
        )
        .await
    }

    /// Called only by the frozen Session owner, after normal admission has drained.
    pub(super) async fn handoff_snapshot(
        &self,
    ) -> io::Result<(FlowInstance<AtmanUuid>, FlowPosition, bool)> {
        request_scoped(
            &self.commands,
            &self.failure,
            None,
            None,
            task_scope::ScopeAdmission::default(),
            |store, _| {
                Ok((
                    store.state().clone(),
                    store.cursor().clone(),
                    store.process_watches().any(|record| {
                        record.status
                            == crate::flow_store::process_watch::ProcessWatchStatus::Active
                    }),
                ))
            },
        )
        .await
    }

    fn storage_error(&self, error: io::Error) -> io::Error {
        storage_error(&self.failure, error)
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

    /// Observe display-name changes without waking on unrelated transcript commits.
    pub fn subscribe_name(&self) -> watch::Receiver<String> {
        self.name.clone()
    }

    /// Observe durable changes, including inbox arrival without a lifecycle transition.
    ///
    /// Subscribe before inspecting. Mark the current revision seen with `borrow_and_update`, inspect, then await `changed` if no work can be admitted. Notifications coalesce; the journal and a fresh snapshot determine actual work. Reads, rejected writes, and matching retries do not generate changes.
    pub fn subscribe_commits(&self) -> watch::Receiver<FlowPosition> {
        self.commits.clone()
    }

    pub async fn snapshot(&self) -> io::Result<FlowSnapshot> {
        self.request(|store| FlowSnapshot::read_pending(store, |_| true))
            .await
    }

    /// Drain accepted work and return journal ownership. Other handles become unavailable.
    ///
    /// Stop active execution before handing off storage. Loss of storage cancels bound execution, but does not manufacture a terminal lifecycle fact.
    pub async fn shutdown(self) -> io::Result<FlowStore> {
        let admission = self.admission.enter().await?;
        self.shutdown_owned(Some(admission)).await
    }

    async fn shutdown_owned(
        self,
        admission: Option<host::AdmissionPermit>,
    ) -> io::Result<FlowStore> {
        let authority = self
            .admission
            .prepare_authority(Default::default(), None)
            .await?;
        self.shutdown_prepared(admission, authority).await
    }

    pub(super) async fn shutdown_prepared(
        self,
        admission: Option<host::AdmissionPermit>,
        authority: Option<Arc<crate::peer_authority::AuthorityGuard>>,
    ) -> io::Result<FlowStore> {
        let (reply, received) = oneshot::channel();
        self.commands
            .send(Command::Stop {
                reply,
                admission,
                authority,
            })
            .await
            .map_err(|_| self.storage_error(io::Error::other("flow storage worker stopped")))?;
        received.await.map_err(|_| {
            self.storage_error(io::Error::other(
                "flow storage worker stopped before handoff",
            ))
        })
    }

    pub async fn transition(
        &self,
        revision: u64,
        transition: FlowTransition,
    ) -> io::Result<FlowPosition> {
        let resources = self.resources.clone();
        let scope = if matches!(
            transition,
            FlowTransition::Activated
                | FlowTransition::TurnStarted { .. }
                | FlowTransition::ExecutionResumed { .. }
                | FlowTransition::InterruptionRecovered
        ) {
            Some(self.scope_admission().await?)
        } else {
            None
        };
        self.request_scoped(
            scope.clone().unwrap_or_default(),
            move |store, snapshot| match scope {
                Some(scope) => {
                    scope.validate_member(store)?;
                    scope.check(snapshot)?;
                    lifecycle::transition(store, &resources, revision, transition)
                }
                None => lifecycle::transition(store, &resources, revision, transition),
            },
        )
        .await
    }

    pub(crate) async fn validate_execution(
        &self,
        lease: TurnLease<AtmanUuid>,
    ) -> io::Result<tokio_util::task::task_tracker::TaskTrackerToken> {
        let (reply, received) = oneshot::channel();
        let mut scope = self.scope_admission().await?;
        let admission = self.admission.enter().await?;
        let authority = self
            .admission
            .prepare_authority(scope.clone(), None)
            .await?;
        if let Some(authority) = &authority {
            scope.prepare_with_authority(authority.clone()).await?;
        }
        self.commands
            .send(Command::ClaimExecution {
                lease,
                scope,
                reply,
                admission,
                authority,
            })
            .await
            .map_err(|_| self.storage_error(io::Error::other("flow storage worker stopped")))?;
        received.await.map_err(|_| {
            self.storage_error(io::Error::other(
                "flow storage worker stopped before execution claim",
            ))
        })?
    }

    pub(crate) async fn publish(
        &self,
        publication: MessagePublication,
        context: ToolCtx,
    ) -> io::Result<PublishedMessage> {
        self.publish_with_live(publication, context, None).await
    }

    pub(crate) async fn publish_with_live(
        &self,
        publication: MessagePublication,
        context: ToolCtx,
        live_call: Option<live::LiveCall>,
    ) -> io::Result<PublishedMessage> {
        let witness = live_call
            .filter(|call| call.belongs_to(&self.live_output))
            .map(PublicationWitness::Provider);
        self.publish_observed(publication, context, witness).await
    }

    pub(crate) async fn publish_final(
        &self,
        publication: MessagePublication,
        context: ToolCtx,
    ) -> io::Result<PublishedMessage> {
        let witness = self
            .live_output
            .prepare_final(&publication)
            .map(PublicationWitness::Final);
        self.publish_observed(publication, context, witness).await
    }

    pub(crate) async fn publish_deferred(
        &self,
        publication: MessagePublication,
        context: ToolCtx,
        producer: live::DeferredProviderDraft,
    ) -> io::Result<PublishedMessage> {
        let witness = producer
            .belongs_to(&self.live_output)
            .then_some(PublicationWitness::Deferred(producer));
        self.publish_observed(publication, context, witness).await
    }

    async fn publish_observed(
        &self,
        publication: MessagePublication,
        mut context: ToolCtx,
        witness: Option<PublicationWitness>,
    ) -> io::Result<PublishedMessage> {
        if publication.lease.flow != self.address {
            return Err(invalid("publication targets another worker"));
        }
        // Commands must not retain the writer through their projection callback.
        context.instance_publication = None;
        let (saved, _guard) = self
            .request(move |store| {
                let before = store.cursor().sequence();
                let saved = publish_and_project(store, publication, &context)?;
                if saved.receipt.sequence() > before && &saved.receipt == store.cursor() {
                    match &witness {
                        Some(PublicationWitness::Provider(call)) => {
                            call.witness_publication(&saved)
                        }
                        Some(PublicationWitness::Deferred(call)) => {
                            call.witness_publication(&saved)
                        }
                        Some(PublicationWitness::Final(answer)) => {
                            answer.witness_publication(&saved)
                        }
                        None => {}
                    }
                }
                // The response owns the guard until the worker has installed the committed
                // boundary, even when the original caller has dropped its awaiter.
                Ok((saved, witness))
            })
            .await?;
        Ok(saved)
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
                        tool_artifacts: Vec::new(),
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
    failure: &StorageFailure,
    admission: &Arc<host::SessionAdmission>,
    command: impl FnOnce(&mut FlowStore) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    request_scoped(
        commands,
        failure,
        Some(admission),
        None,
        task_scope::ScopeAdmission::default(),
        move |store, _| command(store),
    )
    .await
}

async fn request_scoped<T: Send + 'static>(
    commands: &mpsc::Sender<Command>,
    failure: &StorageFailure,
    admission: Option<&Arc<host::SessionAdmission>>,
    authority: Option<Arc<crate::peer_authority::AuthorityGuard>>,
    mut scope: task_scope::ScopeAdmission,
    command: impl FnOnce(&mut FlowStore, &task_scope::ScopeSnapshot) -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    let authority_owner = admission;
    let admission = match admission {
        Some(admission) => Some(admission.enter().await?),
        None => None,
    };
    let authority = match authority_owner {
        Some(owner) => owner.prepare_authority(scope.clone(), authority).await?,
        None => authority,
    };
    if let Some(authority) = &authority {
        scope.prepare_with_authority(authority.clone()).await?;
    }
    let (reply, received) = oneshot::channel();
    commands
        .send(Command::Apply {
            scope,
            command: Box::new(move |store, snapshot| {
                let result = snapshot.and_then(|snapshot| command(store, snapshot));
                Box::new(move || {
                    let _admission = admission;
                    let _authority = authority;
                    let _ = reply.send(result);
                })
            }),
        })
        .await
        .map_err(|_| storage_error(failure, io::Error::other("flow storage worker stopped")))?;
    received.await.map_err(|_| {
        storage_error(
            failure,
            io::Error::other("flow storage worker stopped before acknowledgement"),
        )
    })?
}

fn publish_and_project(
    store: &mut FlowStore,
    publication: MessagePublication,
    context: &ToolCtx,
) -> io::Result<PublishedMessage> {
    let before = store.cursor().sequence();
    let record = store.publish_message(publication)?;
    if record.receipt.sequence() > before {
        // Projection completes before the acknowledgement, including when its receiver is gone.
        crate::tools::session::append_message_to_context(
            context,
            record.publication.message.clone(),
        )
        .map_err(io::Error::other)?;
    }
    Ok(record)
}

fn project_committed_input(
    store: &mut FlowStore,
    receipt: &FlowPosition,
    context: &ToolCtx,
) -> io::Result<usize> {
    let record = store
        .committed_input(receipt)?
        .ok_or_else(|| io::Error::other("committed input disappeared"))?;
    let messages = record.input.content.messages();
    for message in messages {
        crate::tools::session::append_message_to_context(context, message.clone())
            .map_err(io::Error::other)?;
    }
    Ok(messages.len())
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
    async fn fatal_storage_failure_reaches_waiters_and_readers_without_poisoning_reopen() {
        use crate::event::{Event, EventSink};

        let dir = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(dir.path()).unwrap();
        let session_id = Uuid::new_v4();
        let mut session = root
            .create_session(SessionDescriptor {
                configuration: Default::default(),
                id: session_id,
                project: None,
                title: "Storage failure".into(),
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
        let sink = worker.execution_sink(lease, &EventSink::new());
        sink.emit(Event::MermaidDiagram {
            source: "retained diagnostic".into(),
        });
        let committed = worker.execution_events().await.unwrap();
        assert_eq!(committed.len(), 1);
        let mut reader = worker.subscribe_view(1.try_into().unwrap()).await.unwrap();
        reader.refresh().await.unwrap();

        // Keep a request queued before the fatal flush; its reply must not lose the cause.
        let (entered, waiting) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let blocking_worker = worker.clone();
        let blocker = tokio::spawn(async move {
            blocking_worker
                .request(move |_| {
                    entered.send(()).unwrap();
                    blocked
                        .recv_timeout(std::time::Duration::from_secs(10))
                        .unwrap();
                    Err::<(), _>(io::Error::other("independent request failure"))
                })
                .await
        });
        waiting.await.unwrap();
        let waiting_worker = worker.clone();
        let request = tokio::spawn(async move { waiting_worker.state().await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while worker.commands.capacity() == 64 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        sink.restore_seq(0);
        sink.emit(Event::MermaidDiagram {
            source: "invalid duplicate sequence".into(),
        });
        release.send(()).unwrap();

        let expected = "execution event has an invalid owner, sequence, or kind";
        let assert_cause = |error: io::Error| {
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(error.to_string(), expected);
        };
        assert_cause(request.await.unwrap().unwrap_err());
        worker.commands.closed().await;
        assert_eq!(
            blocker.await.unwrap().unwrap_err().to_string(),
            "independent request failure"
        );
        assert!(worker.is_stopped());
        assert_cause(worker.state().await.unwrap_err());
        assert_cause(reader.next().await.err().unwrap());
        assert_cause(reader.refresh().await.err().unwrap());
        assert_cause(worker.suspend_for_handoff().await.err().unwrap());
        assert_cause(worker.clone().shutdown().await.err().unwrap());

        let reopened = FlowWorker::start(session.open_flow(&address.instance_id).unwrap())
            .await
            .unwrap();
        assert!(!reopened.is_stopped());
        assert_eq!(
            serde_json::to_value(reopened.execution_events().await.unwrap()).unwrap(),
            serde_json::to_value(committed).unwrap()
        );
        assert_cause(worker.state().await.unwrap_err());
        reopened.clone().shutdown().await.unwrap();
        assert!(reopened.failure.get().is_none());
        assert_eq!(
            reopened.state().await.unwrap_err().to_string(),
            "flow storage worker stopped"
        );
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
        use crate::flow_store::inbox::{
            DeliveryMode, DeliveryRequest, InboxChange, InboxEditRequest, InboxPayload,
            SenderIdentity, UserContent,
        };
        let editor = SenderIdentity::User(Uuid::new_v4());
        let input = worker
            .accept_input(
                DeliveryRequest {
                    expected_head: None,
                    request_id: Uuid::new_v4(),
                    sender: editor.clone(),
                    recipient: address.clone(),
                    delivery: DeliveryMode::NextTurn,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload: InboxPayload::Interjection {
                        text: "queued".into(),
                    },
                },
                |_| Ok(()),
            )
            .await
            .unwrap();
        let edit = InboxEditRequest {
            expected_head: None,
            id: Uuid::new_v4(),
            sender: editor,
            message: input.message_id,
            expected: input.receipt,
            change: InboxChange::Replace {
                content: UserContent {
                    text: "edited".into(),
                    images: vec![],
                    presentation: None,
                },
                delivery: DeliveryMode::NextTurn,
            },
        };
        let editing_worker = worker.clone();
        let abandoned_edit = edit.clone();
        cancel_queued(&worker, async move {
            editing_worker.edit_input(abandoned_edit, |_| Ok(())).await
        })
        .await;
        let snapshot = worker.view_snapshot().await.unwrap();
        assert!(
            matches!(&snapshot.pending[0].envelope.request.payload, InboxPayload::Interjection { text } if text == "edited")
        );
        let receipt = snapshot.pending[0].revision.clone();
        let mut commits = worker.subscribe_commits();
        commits.borrow_and_update();
        assert_eq!(
            worker.edit_input(edit.clone(), |_| Ok(())).await.unwrap(),
            receipt
        );
        assert!(!commits.has_changed().unwrap());
        let editing_worker = worker.clone();
        let rejected = InboxEditRequest {
            expected_head: None,
            id: Uuid::new_v4(),
            expected: receipt.clone(),
            change: InboxChange::Cancel,
            ..edit
        };
        cancel_queued(&worker, async move {
            editing_worker
                .edit_input(rejected, |_| Err(io::Error::other("revoked while queued")))
                .await
        })
        .await;
        assert_eq!(
            worker.view_snapshot().await.unwrap().pending[0].revision,
            receipt
        );
        let publication = MessagePublication {
            tool_artifacts: Vec::new(),
            id: Uuid::new_v4(),
            lease: lease.clone(),
            run: lease.run.clone(),
            context_run: None,
            message: Message::assistant_text(lease.turn.clone(), "committed ".repeat(50)),
        };
        let writer = worker.clone();
        let body = publication.clone();
        let destination = ctx.clone();
        let draft = worker
            .begin_live_call(lease.clone(), lease.run.clone(), "example".into())
            .unwrap();
        draft.observe(&crate::event::NodeEvent::LlmChunk {
            text: publication.message.text_concat(),
            cumulative_tokens: 1,
        });
        let mut observation = live::DraftObservationReader::observe_for_test(&worker);
        let initial = observation.snapshot().unwrap();
        let draft_id = initial.calls[0].parts[0].id.clone();
        assert!(initial.handoffs.is_empty());
        cancel_queued(&worker, async move {
            writer
                .publish_with_live(body, destination, Some(draft))
                .await
        })
        .await;
        assert_eq!(context.messages().len(), 1);
        let record = worker
            .publish(publication.clone(), ctx.clone())
            .await
            .unwrap();
        let observed = observation.snapshot().unwrap();
        assert!(observed.calls.is_empty());
        assert_eq!(observed.handoffs.len(), 1);
        let witness = &observed.handoffs[0];
        assert_eq!(witness.publication, publication.id);
        assert_eq!(witness.receipt, record.receipt);
        assert_eq!(witness.record.commit, record.receipt.sequence());
        assert_eq!(witness.parts[0].live, draft_id);
        assert_eq!(context.messages().len(), 1);
        let mut conflict = publication.clone();
        conflict.message.parts.clear();
        assert!(worker.publish(conflict, ctx.clone()).await.is_err());

        let original = context.messages();
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
            expected_head: None,
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
                    expected_head: None,
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
        {
            use crate::{
                Value, ValueJson,
                event::NodeEvent,
                flow_store::completion::StoredValue,
                message::{MessageOrigin, MessagePart, MessageRole},
                tool::{Tool, ToolArgs},
                tools::session::SessionPush,
                value::AtmanPayload,
            };

            let draft = worker
                .begin_live_call(lease.clone(), lease.run.clone(), "example".into())
                .unwrap();
            let mut parts = Vec::new();
            for index in 0..4 {
                let id = format!("call-{index}");
                let name = format!("tool-{index}");
                draft.observe(&NodeEvent::ToolCallDraft {
                    index,
                    call_id: id.clone(),
                    name: name.clone(),
                    arguments_delta: "not retained by the observation".into(),
                });
                parts.push(MessagePart::ToolUse {
                    id,
                    name,
                    input: serde_json::Value::Null,
                    intent: None,
                });
            }
            let message = Message {
                role: MessageRole::Assistant,
                turn_id: lease.turn.clone(),
                parts,
                origin: MessageOrigin::User,
            };
            let producer = draft.defer_provider_draft().unwrap();
            let original =
                Value::Host(AtmanPayload::from_provider(message.clone(), Some(producer)));
            let projected = Value::Host(AtmanPayload::Message(message.clone()));
            let copied = original.clone();
            let streaming_id = observation.snapshot().unwrap().calls[0].parts[0].id.clone();
            drop((draft, original));
            assert!(worker.subscribe_live_output().snapshot().calls.is_empty());
            assert_eq!(
                observation.snapshot().unwrap().calls[0].parts[0].id,
                streaming_id
            );
            assert_eq!(copied.to_json().unwrap(), projected.to_json().unwrap());
            assert_eq!(
                StoredValue::capture(&copied),
                StoredValue::capture(&projected)
            );

            let mut push_ctx = ctx
                .clone()
                .with_anchors(Some(lease.turn.clone()), Some(lease.run.clone()), None)
                .with_session_messages_handle(context.messages_handle());
            push_ctx.instance_publication = Some(InstancePublication {
                worker: worker.clone(),
                lease: lease.clone(),
            });
            cancel_queued(&worker, async move {
                SessionPush
                    .call(
                        ToolArgs {
                            positional: vec![copied],
                            named: Vec::new(),
                        },
                        &push_ctx,
                    )
                    .await
            })
            .await;
            let handoffs = observation.snapshot().unwrap();
            assert!(handoffs.calls.is_empty());
            let witnessed = handoffs.handoffs.last().unwrap();
            assert_eq!(witnessed.parts[0].live, streaming_id);
            assert_eq!(witnessed.parts[0].published_parts, vec![0, 1, 2, 3]);
            assert_eq!(context.messages().last(), Some(&message));
        }
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
        assert_eq!(context.messages().len(), 2);
        let clone = worker.clone();
        let mut store = worker.shutdown().await.unwrap();
        assert!(clone.publish(cancelled, ctx).await.is_err());
        assert_eq!(
            store.published_message(record.publication.id).unwrap(),
            Some(record)
        );
        assert_eq!(store.context_checkpoints().len(), 1);
        assert_eq!(store.goal_updates().len(), 2);
        assert_eq!(
            store.context_messages().unwrap(),
            context.messages().to_vec()
        );
        let weak = Arc::downgrade(&context);
        drop(clone);
        drop(context);
        assert!(
            weak.upgrade().is_none(),
            "context writer must not retain its owner"
        );
    }
}
