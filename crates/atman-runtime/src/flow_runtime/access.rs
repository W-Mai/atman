//! Invocation-bound peer operations with live capability checks.

use std::{
    io,
    sync::{Arc, RwLock},
};

use atman_rt::{FlowInstance, InstanceActivity, InstanceState, TurnLease, TurnPhase};
use sha2::{Digest, Sha256};
use tokio::sync::watch;
use uuid::Uuid;

use super::{host::RuntimeHost, send::SendAttempt};
use crate::event::AtmanUuid;
use crate::event_log::journal::CommitReceipt;
use crate::flow_store::access::{AccessTarget, FlowAction, SessionAccess};
use crate::flow_store::inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity};
use crate::flow_store::publication::PublishedMessage;
use crate::flow_store::subscription::{FlowEventFilter, FlowSubscription, SubscriptionRecord};
use crate::flow_store::{FlowInstanceId, FlowRef, FlowTransition, invalid};
use crate::message::{Message, MessageOrigin};

/// Peer controls cannot remove another controller's pause key or forge lifecycle completion.
#[derive(Clone, Copy, Debug)]
pub enum PeerControl {
    Pause,
    Resume,
    CancelTurn,
    Recover,
    Close,
}

/// An opaque host-issued identity for one admitted invocation, not a serializable model argument.
#[derive(Clone)]
pub struct FlowClient {
    host: RuntimeHost,
    lease: TurnLease<AtmanUuid>,
    state: watch::Receiver<FlowInstance<AtmanUuid>>,
    access: Arc<RwLock<SessionAccess>>,
}

/// One owner snapshot of externally registered watchers. Internal result routes are excluded.
pub struct OwnedWatchers {
    pub processes: Vec<crate::flow_store::process_watch::ProcessWatchRecord>,
    pub flows: Vec<SubscriptionRecord>,
}

/// Output patterns accept process or Flow sources; structured events require a Flow source.
pub enum WatchSelection {
    OutputPattern(String),
    Flow(FlowEventFilter),
}

/// A bounded catalog page. Execution states are sampled independently of catalog membership.
pub struct InstancePage {
    pub instances: Vec<InstanceSummary>,
    pub next: Option<FlowInstanceId>,
    pub total: usize,
    pub catalog: CommitReceipt,
}

pub struct InstanceSummary {
    pub address: FlowRef,
    pub entry: Option<String>,
    pub state: Option<FlowInstance<AtmanUuid>>,
    /// A registered instance remains discoverable when its journal cannot be opened.
    pub unavailable: Option<String>,
    pub loaded: bool,
    pub trailing_bytes: Option<u64>,
    pub activity: Option<crate::flow_store::activity::RecordedActivity>,
}

pub struct InstanceObservation {
    pub entry: String,
    pub state: FlowInstance<AtmanUuid>,
    pub loaded: bool,
    pub trailing_bytes: u64,
    pub activity: Option<crate::flow_store::activity::RecordedActivity>,
}

impl RuntimeHost {
    /// Bind only after authenticating the invocation in the trusted execution adapter.
    pub async fn bind_invocation(&self, lease: TurnLease<AtmanUuid>) -> io::Result<FlowClient> {
        let source = self.flow(&lease.flow).await?;
        FlowClient::bind(self.clone(), &source, lease)
    }
}

impl super::host::SessionHandle {
    /// Enumerate one bounded page without requiring an executing caller. The host authenticates callers; authorization is checked around each independently sampled instance.
    pub async fn instances(
        &self,
        before: Option<FlowInstanceId>,
        limit: usize,
        authorize: impl Fn(&FlowRef) -> io::Result<()>,
    ) -> io::Result<InstancePage> {
        instance_page(self.snapshot().await?, before, limit, authorize, |id| {
            self.describe_flow(id)
        })
        .await
    }
}

pub(crate) async fn instance_page<
    Fut: std::future::Future<Output = io::Result<InstanceObservation>>,
>(
    snapshot: super::host::SessionSnapshot,
    before: Option<FlowInstanceId>,
    limit: usize,
    authorize: impl Fn(&FlowRef) -> io::Result<()>,
    observe: impl Fn(FlowInstanceId) -> Fut,
) -> io::Result<InstancePage> {
    if !(1..=50).contains(&limit) {
        return Err(invalid("instance page limit must be between 1 and 50"));
    }
    let registrations = &snapshot.catalog.flows;
    let end = match before {
        Some(id) => registrations
            .iter()
            .position(|entry| entry.id == id)
            .ok_or_else(|| invalid("instance page cursor is not in this session"))?,
        None => registrations.len(),
    };
    let start = end.saturating_sub(limit);
    let mut instances = Vec::with_capacity(end - start);
    for registration in registrations[start..end].iter().rev() {
        let address = FlowRef {
            session_id: snapshot.catalog.descriptor.id.into(),
            instance_id: registration.id.clone(),
        };
        authorize(&address)?;
        let summary = observe(registration.id.clone()).await;
        // An expired caller must not turn a failed authorization into an unavailable row.
        authorize(&address)?;
        let (entry, state, unavailable, loaded, trailing_bytes, activity) = match summary {
            Ok(view) => (
                Some(view.entry),
                Some(view.state),
                None,
                view.loaded,
                Some(view.trailing_bytes),
                view.activity,
            ),
            Err(error) => (None, None, Some(error.to_string()), false, None, None),
        };
        instances.push(InstanceSummary {
            address,
            entry,
            state,
            unavailable,
            loaded,
            trailing_bytes,
            activity,
        });
    }
    Ok(InstancePage {
        next: (start > 0).then(|| registrations[start].id.clone()),
        instances,
        total: registrations.len(),
        catalog: snapshot.cursor,
    })
}

impl FlowClient {
    /// Resolve a persistent flow address relative to the invoking session.
    pub fn resolve_address(&self, handle: &str) -> io::Result<FlowRef> {
        let (session, instance) = match handle.split_once('/') {
            Some((session, instance)) => (Uuid::parse_str(session).map_err(invalid)?, instance),
            None => (self.source().session_id.0, handle),
        };
        Ok(FlowRef {
            session_id: session.into(),
            instance_id: FlowInstanceId::new(Uuid::parse_str(instance).map_err(invalid)?.into()),
        })
    }

    /// Register observation using the source's existing owner and durable subscription path.
    pub async fn watch(
        &self,
        id: Uuid,
        handle: String,
        selection: WatchSelection,
        once: bool,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> io::Result<()> {
        self.authenticate()?;
        let worker = self.host.flow(self.source()).await?;
        if worker.resources.tasks().lookup_by_handle(&handle).is_some() {
            let WatchSelection::OutputPattern(pattern) = selection else {
                return Err(invalid("structured watch events require a Flow source"));
            };
            self.watch_process(id, handle, pattern, once, expires_at)
                .await?;
        } else {
            let filter = match selection {
                WatchSelection::OutputPattern(pattern) => FlowEventFilter::AnswerContains(pattern),
                WatchSelection::Flow(filter) => filter,
            };
            self.watch_events(FlowSubscription {
                id,
                source: self.resolve_address(&handle)?,
                filter,
                once,
                expires_at,
                purpose: crate::flow_store::subscription::SubscriptionPurpose::Watcher,
                reply: None,
            })
            .await?;
        }
        Ok(())
    }

    /// Observe an owned process through the instance resource owner. Registration survives the calling turn.
    pub async fn watch_process(
        &self,
        id: Uuid,
        handle: String,
        pattern: String,
        once: bool,
        expires_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> io::Result<crate::flow_store::process_watch::ProcessWatchRecord> {
        self.authenticate()?;
        let client = self.clone();
        self.host
            .flow(self.source())
            .await?
            .watch_process(id, handle, pattern, once, expires_at, move |_| {
                client.authenticate()
            })
            .await
    }

    pub async fn watchers(&self) -> io::Result<OwnedWatchers> {
        self.authenticate()?;
        let client = self.clone();
        self.host
            .flow(self.source())
            .await?
            .request(move |store| {
                client.authenticate()?;
                Ok(OwnedWatchers {
                    processes: store.process_watches().cloned().collect(),
                    flows: store
                        .flow_subscriptions()
                        .filter(|record| {
                            record.subscription.purpose
                                == crate::flow_store::subscription::SubscriptionPurpose::Watcher
                        })
                        .cloned()
                        .collect(),
                })
            })
            .await
    }

    pub async fn unwatch(&self, id: Uuid) -> io::Result<CommitReceipt> {
        self.authenticate()?;
        let client = self.clone();
        let worker = self.host.flow(self.source()).await?;
        worker.unwatch(id, move |_| client.authenticate()).await
    }

    pub async fn read_planning(
        &self,
        target: FlowRef,
    ) -> io::Result<crate::flow_store::planning::PlanningSnapshot> {
        self.authorize(&target, FlowAction::ReadContext)?;
        let client = self.clone();
        self.host
            .flow(&target)
            .await?
            .request(move |store| {
                client.authorize(store.state().address(), FlowAction::ReadContext)?;
                Ok(store.planning().clone())
            })
            .await
    }

    /// Tool actions edit only their own instance, under the admitted turn's live identity.
    pub async fn edit_planning(
        &self,
        request_id: Uuid,
        expected_revision: Option<u64>,
        edit: crate::flow_store::planning::PlanningEdit,
    ) -> io::Result<crate::flow_store::planning::CommittedPlanningUpdate> {
        self.authenticate_context()?;
        let client = self.clone();
        self.host
            .flow(self.source())
            .await?
            .edit_planning(
                crate::flow_store::planning::PlanningRequest {
                    id: request_id,
                    sender: SenderIdentity::Flow(self.source().clone()),
                    expected_revision,
                    edit,
                },
                move |_| client.authenticate_context(),
            )
            .await
    }

    /// Submit an entry policy's suggestion for this instance's next turn.
    pub async fn schedule_goal(
        &self,
        goal: crate::flow_store::goal::GoalVersion,
        text: String,
    ) -> io::Result<Option<crate::flow_store::inbox::Acceptance>> {
        self.authorize(self.source(), FlowAction::ManageGoal)?;
        let client = self.clone();
        self.host
            .flow(self.source())
            .await?
            .request(move |store| {
                store.schedule_goal(client.lease.clone(), goal, text, |_| {
                    client.authorize(client.source(), FlowAction::ManageGoal)
                })
            })
            .await
    }

    pub async fn read_goal(&self, target: FlowRef) -> io::Result<super::goal::GoalView> {
        self.authorize_goal_read(&target)?;
        let client = self.clone();
        self.host
            .flow(&target)
            .await?
            .request(move |store| {
                client.authorize_goal_read(store.state().address())?;
                Ok(super::goal::GoalView {
                    goal: store.goal().cloned(),
                    execution: store.state().clone(),
                })
            })
            .await
    }

    fn authorize_goal_read(&self, target: &FlowRef) -> io::Result<()> {
        self.authorize(target, FlowAction::ReadContext)
            .or_else(|_| self.authorize(target, FlowAction::ManageGoal))
    }

    /// Actor identity comes from the authenticated invocation, not tool arguments.
    pub async fn edit_goal(
        &self,
        target: FlowRef,
        request_id: Uuid,
        expected: Option<crate::flow_store::goal::GoalVersion>,
        edit: crate::memory::goal::GoalEdit,
    ) -> io::Result<crate::flow_store::goal::CommittedGoalUpdate> {
        use crate::memory::goal::{GoalActor, GoalEdit};
        self.authorize(&target, FlowAction::ManageGoal)?;
        if matches!(&edit, GoalEdit::Pause { actor, .. } | GoalEdit::Resume { actor } if *actor != GoalActor::Agent)
        {
            return Err(denied("peer goal edits cannot impersonate another actor"));
        }
        let client = self.clone();
        let authorize_target = target.clone();
        self.host
            .flow(&target)
            .await?
            .edit_goal(
                crate::flow_store::goal::GoalRequest {
                    start_turn: false,
                    id: request_id,
                    sender: SenderIdentity::Flow(self.source().clone()),
                    expected,
                    edit,
                },
                move |_| client.authorize(&authorize_target, FlowAction::ManageGoal),
            )
            .await
    }

    /// Prepare continuation ownership before the corresponding subscription or request is published.
    pub async fn prepare_reply(
        &self,
        source: FlowRef,
        correlation: Uuid,
    ) -> io::Result<super::reply::ReplyWait> {
        self.authorize(&source, FlowAction::Watch)?;
        let worker = self.host.flow(self.source()).await?;
        let waiter = crate::flow_store::waiter::ReplyWaiter {
            correlation,
            lease: self.lease.clone(),
            sender: SenderIdentity::Flow(source),
        };
        let waiting_worker = worker.clone();
        let client = self.clone();
        worker
            .request(move |store| {
                client.authenticate()?;
                let record = store.register_reply_waiter(waiter)?;
                if matches!(
                    record.status,
                    crate::flow_store::waiter::ReplyStatus::Cancelled { .. }
                ) {
                    return Err(invalid("reply wait was already cancelled"));
                }
                // The owner creates the guard only after registration succeeds. A rejected request
                // cannot cancel another waiter; losing the acknowledgement drops this guard safely.
                Ok(super::reply::ReplyWait::new(waiting_worker, record.waiter))
            })
            .await
    }

    /// Product spawn entry: creation stays in the authenticated caller's session. Tool admission supplies the execution ceiling; provenance never grants control over the new instance.
    pub(crate) async fn spawn(
        &self,
        descriptor: crate::flow_store::FlowDescriptor,
        input: crate::flow_store::launch::LaunchInput,
        execution: super::launch::EntryExecution,
    ) -> io::Result<super::launch::LaunchedFlow> {
        self.authorize(self.source(), FlowAction::Send)?;
        if descriptor.address.session_id != self.source().session_id
            || descriptor.created_by.as_ref() != Some(self.source())
            || input.request.sender != SenderIdentity::Flow(self.source().clone())
            || input
                .result_route
                .as_ref()
                .is_some_and(|route| &route.recipient != self.source())
        {
            return Err(denied("spawn identity does not match its admitted caller"));
        }
        let client = self.clone();
        self.host
            .launch_entry(descriptor, input, execution, move |_, _| {
                client.authorize(client.source(), FlowAction::Send)
            })
            .await
    }

    pub(super) fn bind(
        host: RuntimeHost,
        source: &super::FlowWorker,
        lease: TurnLease<AtmanUuid>,
    ) -> io::Result<Self> {
        let state = source.subscribe_state();
        if source.address() != &lease.flow
            || state.has_changed().is_err()
            || !matches!(state.borrow().state(), InstanceState::Open(InstanceActivity::Active(active)) if active.lease == lease)
        {
            return Err(denied("peer client requires an admitted instance turn"));
        }
        // Binding is identity, not operation admission. Pauses are checked when using the client.
        Ok(Self {
            access: host.session(lease.flow.session_id.0)?.access,
            host,
            state,
            lease,
        })
    }

    pub fn source(&self) -> &FlowRef {
        &self.lease.flow
    }

    fn authenticate(&self) -> io::Result<()> {
        if self.state.has_changed().is_err()
            || !self
                .state
                .borrow()
                .can_receive_at_boundary(&self.lease)
                .unwrap_or(false)
        {
            return Err(denied(
                "invocation is no longer admitted for peer operations",
            ));
        }
        Ok(())
    }

    fn authorize(&self, target: &FlowRef, action: FlowAction) -> io::Result<()> {
        if target == self.source()
            && matches!(
                action,
                FlowAction::ReadContext | FlowAction::ReadOutput | FlowAction::ManageGoal
            )
        {
            self.authenticate_context()?;
        } else {
            self.authenticate()?;
        }
        self.authorize_delivery(target, action)
    }

    /// Owned cleanup may settle its conversation and planning without admitting peer work.
    fn authenticate_context(&self) -> io::Result<()> {
        if self.state.has_changed().is_ok()
            && matches!(self.state.borrow().state(), InstanceState::Open(InstanceActivity::Active(active)) if active.lease == self.lease && active.phase == atman_rt::TurnPhase::Finishing)
        {
            self.source_session()?;
            return Ok(());
        }
        self.authenticate()
    }

    fn authorize_delivery(&self, target: &FlowRef, action: FlowAction) -> io::Result<()> {
        let session = self.source_session()?;
        if self
            .host
            .allows_access(&session, &self.lease.flow, target, action)
        {
            Ok(())
        } else {
            Err(denied("flow action is outside the current access grants"))
        }
    }

    fn source_session(&self) -> io::Result<super::host::SessionHandle> {
        let session = self.host.session(self.lease.flow.session_id.0)?;
        if session.is_stopped() || !Arc::ptr_eq(&session.access, &self.access) {
            return Err(denied("source access owner is unavailable or replaced"));
        }
        Ok(session)
    }

    fn discovers_session(&self, id: Uuid) -> io::Result<bool> {
        self.authenticate()?;
        Ok(self.host.allows_scope(
            &self.source_session()?,
            self.source(),
            &AccessTarget::Session(id),
            FlowAction::Discover,
        ))
    }

    /// Page authorized Session metadata, including detached catalogs. Only Session-wide discovery grants permit browsing a foreign container.
    pub async fn sessions(
        &self,
        before: Option<Uuid>,
        limit: usize,
    ) -> io::Result<super::catalog::SessionPage> {
        self.authenticate()?;
        let page = self
            .host
            .session_catalog()
            .page(before, limit, |id| self.discovers_session(id))
            .await?;
        self.authenticate()?;
        Ok(page)
    }

    pub async fn inspect(&self, target: FlowRef) -> io::Result<FlowInstance<AtmanUuid>> {
        self.authorize(&target, FlowAction::Discover)?;
        let client = self.clone();
        self.host
            .flow(&target)
            .await?
            .request(move |store| {
                client.authorize(store.state().address(), FlowAction::Discover)?;
                Ok(store.state().clone())
            })
            .await
    }

    /// Discover the current session, newest registration first. A stable instance cursor avoids repeating earlier entries when another flow is registered between pages. This does not expose private message history, start execution, or grant authority to control the listed instances.
    pub async fn instances(
        &self,
        before: Option<FlowInstanceId>,
        limit: usize,
    ) -> io::Result<InstancePage> {
        self.instances_in(self.source().session_id.0, before, limit)
            .await
    }

    pub async fn instances_in(
        &self,
        session: Uuid,
        before: Option<FlowInstanceId>,
        limit: usize,
    ) -> io::Result<InstancePage> {
        if !self.discovers_session(session)? {
            return Err(denied(
                "session discovery is outside the current access grants",
            ));
        }
        let page = self
            .host
            .session_catalog()
            .instances(session, before, limit, |target| {
                self.authorize(target, FlowAction::Discover)
            })
            .await?;
        if !self.discovers_session(session)? {
            return Err(denied("session discovery authority changed"));
        }
        Ok(page)
    }

    pub async fn read_output(&self, target: FlowRef) -> io::Result<Vec<PublishedMessage>> {
        self.authorize(&target, FlowAction::ReadOutput)?;
        let client = self.clone();
        self.host
            .flow(&target)
            .await?
            .read_output(move |address| client.authorize(address, FlowAction::ReadOutput))
            .await
    }

    /// Read one committed entry result under the same authority as public output.
    /// An absent record is not evidence that an invocation succeeded or failed.
    pub async fn read_result(
        &self,
        target: FlowRef,
        turn: Option<crate::event::TurnId>,
    ) -> io::Result<Option<crate::flow_store::completion::CommittedCompletion>> {
        self.authorize(&target, FlowAction::ReadOutput)?;
        let client = self.clone();
        self.host
            .flow(&target)
            .await?
            .request(move |store| {
                client.authorize(store.state().address(), FlowAction::ReadOutput)?;
                Ok(store.completion(turn.as_ref()).cloned())
            })
            .await
    }

    pub async fn read_context(&self, target: FlowRef) -> io::Result<Vec<Message>> {
        self.authorize(&target, FlowAction::ReadContext)?;
        let client = self.clone();
        self.host
            .flow(&target)
            .await?
            .request(move |store| {
                client.authorize(store.state().address(), FlowAction::ReadContext)?;
                Ok(store.context_messages())
            })
            .await
    }

    /// Register before returning. The subscription outlives this invocation and keeps its original
    /// source position on retry; current watch grants are rechecked for every subsequent delivery.
    pub async fn watch_events(
        &self,
        subscription: FlowSubscription,
    ) -> io::Result<SubscriptionRecord> {
        self.authenticate()?;
        let client = self.clone();
        self.host
            .watch_events(self.source().clone(), subscription, move |_| {
                client.authenticate()?;
                client.source_session()?;
                Ok(())
            })
            .await
    }

    /// Cancel owned observation even if access to its source has since been revoked.
    pub async fn unwatch_events(&self, id: Uuid) -> io::Result<CommitReceipt> {
        self.authenticate()?;
        let client = self.clone();
        self.host
            .flow(self.source())
            .await?
            .request(move |store| {
                client.authenticate()?;
                store.unsubscribe_events(id)
            })
            .await
    }

    pub async fn send(
        &self,
        target: FlowRef,
        request_id: Uuid,
        text: String,
        delivery: DeliveryMode,
    ) -> io::Result<SendAttempt> {
        // Authenticate before consulting the durable intent. Matching retries retain their receipt;
        // only a new intent and a new receiver admission require current send authority.
        self.authenticate()?;
        let source = self.clone();
        let receiver = self.clone();
        self.host
            .send(
                self.lease.clone(),
                DeliveryRequest {
                    request_id,
                    sender: SenderIdentity::Flow(self.lease.flow.clone()),
                    recipient: target,
                    delivery,
                    cause: None,
                    correlation: None,
                    scope: None,
                    payload: InboxPayload::Interjection { text },
                },
                None,
                move |request| source.authorize(&request.recipient, FlowAction::Send),
                // A committed send can outlive its originating turn, but new receiver admission still checks current grants.
                move |request| receiver.authorize_delivery(&request.recipient, FlowAction::Send),
            )
            .await
    }

    pub async fn control(
        &self,
        target: FlowRef,
        expected_revision: u64,
        control: PeerControl,
    ) -> io::Result<CommitReceipt> {
        self.authorize(&target, FlowAction::Control)?;
        let client = self.clone();
        let key = pause_key(&self.lease.flow, &target);
        self.host
            .flow(&target)
            .await?
            .control(expected_revision, key, control, move |address| {
                client.authorize(address, FlowAction::Control)
            })
            .await
    }
}

impl super::FlowWorker {
    /// Cancel only the named active invocation, atomically with checking its identity.
    pub async fn cancel_run(
        &self,
        run: crate::event::FlowRunId,
        authorize: impl FnOnce(&FlowRef) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Option<CommitReceipt>> {
        self.request(move |store| {
            authorize(store.state().address())?;
            let InstanceState::Open(InstanceActivity::Active(active)) = store.state().state()
            else {
                return Ok(None);
            };
            if active.lease.run != run {
                return Ok(None);
            }
            let transition = FlowTransition::PhaseChanged {
                lease: active.lease.clone(),
                phase: TurnPhase::Cancelling,
            };
            store
                .transition(store.state().revision(), transition)
                .map(Some)
        })
        .await
    }

    /// Read committed final answers under the writer's authorization boundary.
    pub async fn read_output(
        &self,
        authorize: impl FnOnce(&FlowRef) -> io::Result<()> + Send + 'static,
    ) -> io::Result<Vec<PublishedMessage>> {
        self.request(move |store| {
            authorize(store.state().address())?;
            Ok(store
                .published_messages()
                .iter()
                .filter(|record| {
                    record.publication.context_run.is_none()
                        && record.publication.message.origin == MessageOrigin::FinalAnswer
                })
                .cloned()
                .collect())
        })
        .await
    }

    /// Apply an authenticated host control under the instance writer. The pause key belongs to the controller, not the target or its Goal.
    pub async fn control(
        &self,
        expected_revision: u64,
        key: AtmanUuid,
        control: PeerControl,
        authorize: impl FnOnce(&FlowRef) -> io::Result<()> + Send + 'static,
    ) -> io::Result<CommitReceipt> {
        if matches!(control, PeerControl::Close) {
            return self.request_close(expected_revision, authorize).await;
        }
        self.request(move |store| {
            authorize(store.state().address())?;
            if store.state().revision() != expected_revision {
                return Err(invalid("target execution revision changed"));
            }
            let transition = match control {
                PeerControl::Pause => FlowTransition::ExecutionPaused { key },
                PeerControl::Resume => FlowTransition::ExecutionResumed { key },
                PeerControl::Recover => FlowTransition::InterruptionRecovered,
                PeerControl::Close => unreachable!("closure is owned by the lifecycle coordinator"),
                PeerControl::CancelTurn => match store.state().state() {
                    InstanceState::Open(InstanceActivity::Active(active)) => {
                        FlowTransition::PhaseChanged {
                            lease: active.lease.clone(),
                            phase: TurnPhase::Cancelling,
                        }
                    }
                    _ => return Ok(store.cursor().clone()),
                },
            };
            store.transition(expected_revision, transition)
        })
        .await
    }
}

fn pause_key(source: &FlowRef, target: &FlowRef) -> AtmanUuid {
    let mut hash = Sha256::new();
    hash.update(b"atman.peer.execution-pause\0");
    for id in [
        source.session_id.0,
        source.instance_id.0.0,
        target.session_id.0,
        target.instance_id.0.0,
    ] {
        hash.update(id.as_bytes());
    }
    let digest = hash.finalize();
    let mut key = [0; 16];
    key.copy_from_slice(&digest[..16]);
    Uuid::from_bytes(key).into()
}

fn denied(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, message)
}
