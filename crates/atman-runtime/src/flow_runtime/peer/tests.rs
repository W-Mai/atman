use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

use super::*;
use crate::flow_runtime::{
    FlowWorker,
    host::{SessionHandle, WeakRuntimeHost},
};
use crate::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowTransition, SessionDescriptor, access::FlowGrant,
    inbox::DeliveryMode,
};

#[derive(Default)]
struct Directory(
    Mutex<HashMap<Uuid, (PeerIdentity, WeakRuntimeHost)>>,
    Mutex<Option<Arc<ChunkGate>>>,
);

struct ChunkGate {
    started: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    release: Arc<tokio::sync::Notify>,
}

struct Port {
    identity: PeerIdentity,
    directory: Arc<Directory>,
}

impl PeerPort for Port {
    fn identity(&self) -> PeerIdentity {
        self.identity.clone()
    }
    fn locate(&self, target: PeerTarget) -> BoxFuture<'static, io::Result<PeerIdentity>> {
        let directory = self.directory.clone();
        Box::pin(async move {
            directory
                .0
                .lock()
                .unwrap()
                .values()
                .find_map(|(identity, host)| {
                    let host = host.upgrade().ok()?;
                    let matches = match target {
                        PeerTarget::Runtime(id) => identity.runtime_generation == id,
                        PeerTarget::Profile => host.profile_handle().is_some(),
                        PeerTarget::Session(id) => host.session(id).is_ok(),
                    };
                    matches.then(|| identity.clone())
                })
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "peer owner is unavailable"))
        })
    }
    fn call(
        &self,
        owner: PeerIdentity,
        call: PeerCall,
    ) -> BoxFuture<'static, io::Result<PeerReply>> {
        let target = self
            .directory
            .0
            .lock()
            .unwrap()
            .get(&owner.runtime_generation)
            .map(|(_, host)| host.clone());
        let caller = self.identity.clone();
        let gate = if matches!(&call, PeerCall::Execute { operation, .. } if matches!(operation.as_ref(), PeerOperation::CaptureChunk { .. }))
        {
            self.directory.1.lock().unwrap().take()
        } else {
            None
        };
        Box::pin(async move {
            if let Some(gate) = gate {
                if let Some(started) = gate.started.lock().unwrap().take() {
                    let _ = started.send(());
                }
                gate.release.notified().await;
            }
            let host = target
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::NotFound, "peer owner is unavailable")
                })?
                .upgrade()?;
            let encoded = serde_json::to_vec(&call).map_err(io::Error::other)?;
            if encoded.len() > 16 * 1024 * 1024 {
                return Err(protocol("test transport rejects an oversized single call"));
            }
            let reply = handle_peer_call(
                host,
                caller,
                serde_json::from_slice(&encoded).map_err(io::Error::other)?,
            )
            .await?;
            let encoded = serde_json::to_vec(&reply).map_err(io::Error::other)?;
            if encoded.len() > 16 * 1024 * 1024 {
                return Err(protocol("test transport rejects an oversized single reply"));
            }
            serde_json::from_slice(&encoded).map_err(io::Error::other)
        })
    }
}

pub(super) struct Peers {
    directory: Arc<Directory>,
    pub data: tempfile::TempDir,
    pub source_host: RuntimeHost,
    pub target_host: RuntimeHost,
    pub source_session: SessionHandle,
    pub source: FlowWorker,
    pub target: FlowWorker,
    pub client: FlowClient,
    pub grant: Uuid,
}

impl Peers {
    pub async fn new() -> Self {
        let data = tempfile::tempdir().unwrap();
        let target_host = RuntimeHost::default();
        let profile = target_host
            .open_profile(data.path())
            .await
            .unwrap()
            .snapshot()
            .await
            .unwrap()
            .profile
            .profile_id;
        let source_host = RuntimeHost::default();
        let directory = Arc::new(Directory::default());
        for host in [&source_host, &target_host] {
            let identity = PeerIdentity {
                profile_id: profile,
                canonical_data_root: data.path().canonicalize().unwrap(),
                runtime_generation: host.generation(),
                transport_generation: Uuid::now_v7().to_string(),
            };
            directory
                .0
                .lock()
                .unwrap()
                .insert(host.generation(), (identity.clone(), host.downgrade()));
            host.configure_peer(
                identity.clone(),
                Arc::new(Port {
                    identity,
                    directory: directory.clone(),
                }),
            )
            .unwrap();
        }
        let (source_session, source) = flow(&source_host, data.path()).await;
        let (_, target) = flow(&target_host, data.path()).await;
        let grant = Uuid::now_v7();
        source_session
            .grant_access(
                source_session.snapshot().await.unwrap().cursor,
                FlowGrant {
                    id: grant,
                    grantee: source.address().clone(),
                    target: AccessTarget::Flow(target.address().clone()),
                    actions: [
                        FlowAction::Discover,
                        FlowAction::ReadOutput,
                        FlowAction::ReadContext,
                        FlowAction::Send,
                        FlowAction::Watch,
                        FlowAction::Control,
                        FlowAction::Rename,
                        FlowAction::ManageGoal,
                    ]
                    .into_iter()
                    .collect::<BTreeSet<_>>(),
                },
            )
            .await
            .unwrap();
        let lease = start(&source).await;
        let client = FlowClient::bind(source_host.clone(), &source, lease).unwrap();
        Self {
            directory,
            data,
            source_host,
            target_host,
            source_session,
            source,
            target,
            client,
            grant,
        }
    }
    pub async fn revoke(&self) {
        self.source_session
            .revoke_access(
                self.source_session.snapshot().await.unwrap().cursor,
                self.grant,
            )
            .await
            .unwrap();
    }
    pub fn block_next_chunk(
        &self,
    ) -> (tokio::sync::oneshot::Receiver<()>, Arc<tokio::sync::Notify>) {
        let (started, ready) = tokio::sync::oneshot::channel();
        let release = Arc::new(tokio::sync::Notify::new());
        *self.directory.1.lock().unwrap() = Some(Arc::new(ChunkGate {
            started: Mutex::new(Some(started)),
            release: release.clone(),
        }));
        (ready, release)
    }
    pub async fn shutdown(&self) {
        for worker in [&self.source, &self.target] {
            if worker.is_stopped() {
                continue;
            }
            let state = worker.state().await.unwrap();
            if let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) =
                state.state()
            {
                finish(worker, active.lease.clone()).await;
            }
        }
        self.source_host.shutdown().await.unwrap();
        self.target_host.shutdown().await.unwrap();
    }
}

async fn flow(host: &RuntimeHost, data: &std::path::Path) -> (SessionHandle, FlowWorker) {
    let id = Uuid::now_v7();
    let session = host
        .create_session(
            data,
            SessionDescriptor {
                id,
                title: "Peer authority".into(),
                project: None,
                configuration: Default::default(),
            },
        )
        .await
        .unwrap();
    let worker = session
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
                bundle: crate::source_program::RevisionBundle {
                    entry: "entry:main.at".into(),
                    edges: Default::default(),
                    sources: [(
                        "entry:main.at".into(),
                        "flow main(input: string) -> string { return input }".into(),
                    )]
                    .into(),
                },
            }
            .into(),
        })
        .await
        .unwrap();
    worker
        .transition(0, FlowTransition::Activated)
        .await
        .unwrap();
    (session, worker)
}

async fn start(worker: &FlowWorker) -> atman_rt::TurnLease<AtmanUuid> {
    let turn = atman_rt::TurnId::new(Uuid::now_v7().into());
    let run = atman_rt::RunId::new(Uuid::now_v7().into());
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::TurnStarted {
                turn: turn.clone(),
                run: run.clone(),
            },
        )
        .await
        .unwrap();
    let state = worker.state().await.unwrap();
    let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) = state.state()
    else {
        panic!("turn must be active")
    };
    active.lease.clone()
}

async fn finish(worker: &FlowWorker, lease: atman_rt::TurnLease<AtmanUuid>) {
    worker
        .transition(
            worker.state().await.unwrap().revision(),
            FlowTransition::TurnFinished {
                lease,
                outcome: atman_rt::TurnOutcome::Completed,
            },
        )
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_effects_keep_original_actor_receipts_and_durable_retry_after_turn_and_revocation() {
    let peers = Peers::new().await;
    let id = Uuid::now_v7();
    let accepted = peers
        .client
        .send(
            peers.target.address().clone(),
            id,
            "original payload".into(),
            DeliveryMode::AtBoundary,
            None,
        )
        .await
        .unwrap();
    assert!(accepted.pending_reason.is_none());
    assert_eq!(peers.target.snapshot().await.unwrap().pending.len(), 1);
    let original = match &accepted.record.status {
        crate::flow_store::outbox::SendStatus::Accepted { delivery, .. } => delivery.clone(),
        _ => panic!("delivery must be accepted"),
    };
    finish(&peers.source, peers.client.lease.clone()).await;
    peers.revoke().await;
    let retry = super::durable::deliver_outbox(&peers.source_host, &peers.source, &accepted.record)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(retry.message_id, original.message_id);
    assert_eq!(retry.receipt, original.receipt);
    assert_eq!(
        peers
            .client
            .read_context(peers.target.address().clone())
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(peers.target.snapshot().await.unwrap().pending.len(), 1);
    peers.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn persisted_remote_subscription_outlives_the_admitting_invocation_and_keeps_original_cursor()
{
    let peers = Peers::new().await;
    let subscription = FlowSubscription {
        id: Uuid::now_v7(),
        source: peers.target.address().clone(),
        filter: crate::flow_store::subscription::FlowEventFilter::TurnCompleted { turn: None },
        once: false,
        expires_at: None,
        purpose: crate::flow_store::subscription::SubscriptionPurpose::Watcher,
        reply: None,
    };
    let registered = peers
        .client
        .watch_events(subscription.clone())
        .await
        .unwrap();
    let retried = peers.client.watch_events(subscription).await.unwrap();
    assert_eq!(registered.started_after, retried.started_after);
    finish(&peers.source, peers.client.lease.clone()).await;
    let lease = start(&peers.target).await;
    finish(&peers.target, lease).await;
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if peers
                .source
                .snapshot()
                .await
                .unwrap()
                .pending
                .iter()
                .any(|entry| {
                    matches!(
                        entry.request.payload,
                        crate::flow_store::inbox::InboxPayload::FlowEvent { .. }
                    )
                })
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    let observed = peers.source.flow_subscriptions().await.unwrap();
    assert_eq!(observed[0].started_after, registered.started_after);
    assert!(observed[0].cursor > registered.cursor);
    peers.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_idle_watcher_controls_keep_owner_authorization_and_original_edit_receipts() {
    use crate::flow_store::{
        subscription::{FlowEventFilter, SubscriptionPurpose},
        watch_edit::WatchDefinition,
    };
    let peers = Peers::new().await;
    finish(&peers.source, peers.client.lease.clone()).await;
    let before = peers.source.state().await.unwrap();
    let owner = peers.source.address().clone();
    let subscription = FlowSubscription {
        id: Uuid::now_v7(),
        source: peers.target.address().clone(),
        filter: FlowEventFilter::FinalAnswers,
        once: false,
        expires_at: None,
        purpose: SubscriptionPurpose::Watcher,
        reply: None,
    };
    let expected_owner = owner.clone();
    peers
        .source_host
        .watch_events(owner.clone(), subscription.clone(), move |actual| {
            if actual == &expected_owner {
                Ok(())
            } else {
                Err(denied("wrong native watcher owner"))
            }
        })
        .await
        .unwrap();
    assert_eq!(peers.source.state().await.unwrap(), before);
    let replacement = FlowSubscription {
        id: Uuid::now_v7(),
        once: true,
        ..subscription.clone()
    };
    let expected = WatchDefinition::Flow(subscription);
    let next = WatchDefinition::Flow(replacement);
    let edited = peers
        .source_host
        .replace_watcher(owner.clone(), expected.clone(), next.clone(), |_| Ok(()))
        .await
        .unwrap();
    peers.revoke().await;
    let retried = peers
        .source_host
        .replace_watcher(owner.clone(), expected, next, |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(edited, retried);
    let unauthorized = FlowSubscription {
        id: Uuid::now_v7(),
        source: peers.target.address().clone(),
        filter: FlowEventFilter::FinalAnswers,
        once: false,
        expires_at: None,
        purpose: SubscriptionPurpose::Watcher,
        reply: None,
    };
    assert_eq!(
        peers
            .source_host
            .watch_events(owner, unauthorized, |_| Err(denied(
                "native owner rejected"
            )))
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    peers.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_scope_cancellation_preserves_receiver_acceptance_and_invalidates_original_input() {
    use crate::flow_store::task_scope::{TaskScopeDefinition, TaskScopeRef};
    let peers = Peers::new().await;
    let scope = Uuid::now_v7();
    peers
        .client
        .create_task_scope(TaskScopeDefinition {
            id: scope,
            name: "original cancellation owner".into(),
            goal: None,
        })
        .await
        .unwrap();
    let request = Uuid::now_v7();
    let accepted = peers
        .client
        .send(
            peers.target.address().clone(),
            request,
            "scoped payload".into(),
            DeliveryMode::AtBoundary,
            Some(TaskScopeRef {
                owner: peers.source.address().clone(),
                id: scope,
            }),
        )
        .await
        .unwrap();
    assert!(accepted.pending_reason.is_none());
    assert_eq!(peers.target.snapshot().await.unwrap().pending.len(), 1);
    let mut changed = peers.target.subscribe_commits();
    peers.client.cancel_task_scope(scope).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            changed.borrow_and_update();
            if peers.target.snapshot().await.unwrap().pending.is_empty() {
                break;
            }
            changed.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let original =
        super::durable::deliver_outbox(&peers.source_host, &peers.source, &accepted.record)
            .await
            .unwrap()
            .unwrap();
    let status = peers
        .client
        .observe_request(peers.target.address().clone(), request)
        .await
        .unwrap();
    assert_eq!(status["request_id"], serde_json::json!(request));
    assert_eq!(
        status["receiver"]["message_id"],
        serde_json::json!(original.message_id)
    );
    peers.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn remote_management_review_uses_original_brokers_and_registered_authority_without_importing_a_run()
 {
    use crate::{
        flow_authority::EffectiveAuthority,
        flow_store::profile::{ManagementBinding, ReplaceManagement},
        permission::{
            ApprovalAuthority, DecisionActor, PermissionIntent, PermissionResolution,
            SubmissionOutcome,
        },
        trust::{TrustConfig, TrustMode},
    };
    let peers = Peers::new().await;
    let manager_lease = start(&peers.target).await;
    for (worker, lease) in [
        (&peers.source, &peers.client.lease),
        (&peers.target, &manager_lease),
    ] {
        let address = worker.address().clone();
        let run = crate::event::FlowRunId::new(lease.run.0);
        let registry = worker.context().flow_registry.clone();
        worker
            .context()
            .apply_instance_effect(move || {
                registry.register_instance(
                    address,
                    run,
                    EffectiveAuthority::root(&TrustConfig::default(), true, None),
                )
            })
            .await
            .unwrap()
            .unwrap();
    }
    let profile = peers.target_host.profile_handle().unwrap();
    let manager = peers
        .target_host
        .session(peers.target.address().session_id.0)
        .unwrap()
        .snapshot()
        .await
        .unwrap()
        .catalog
        .descriptor;
    profile
        .replace(ReplaceManagement {
            request_id: Uuid::now_v7(),
            expected: profile.snapshot().await.unwrap().profile.cursor,
            binding: ManagementBinding {
                session: manager,
                grants: vec![FlowGrant {
                    id: Uuid::now_v7(),
                    grantee: peers.target.address().clone(),
                    target: AccessTarget::Flow(peers.source.address().clone()),
                    actions: [FlowAction::ApprovePermission].into(),
                }],
            },
        })
        .await
        .unwrap();
    let context = peers.source.context();
    let broker = context.permission_broker();
    let original_broker = broker.clone();
    let session = peers.source.address().session_id.to_string();
    let source_run = crate::event::FlowRunId::new(peers.client.lease.run.0);
    let pending = context
        .apply_instance_effect(move || {
            original_broker.submit_to(
                Some(&session),
                Some(&source_run),
                PermissionIntent::minimal("call-1", "fs.write", crate::tool::Tier::Two),
                false,
                ApprovalAuthority::User(original_broker.user_authority(&session, None)),
                &TrustConfig {
                    mode: TrustMode::Steady,
                    ..Default::default()
                },
            )
        })
        .await
        .unwrap()
        .unwrap();
    let SubmissionOutcome::Pending(pending) = pending else {
        panic!("request must remain with its original User broker")
    };
    let request = pending.request.clone();
    let original = request.clone();
    let source_client = peers.client.clone();
    let routing =
        tokio::spawn(async move { source_client.route_permission_review(&original).await });
    let mut commits = peers.target.subscribe_commits();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            commits.borrow_and_update();
            if peers
                .target
                .snapshot()
                .await
                .unwrap()
                .pending
                .iter()
                .any(|entry| entry.request.request_id == request.request_id.0)
            {
                break;
            }
            commits.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    let reviewer = FlowClient::bind(
        peers.target_host.clone(),
        &peers.target,
        manager_lease.clone(),
    )
    .unwrap();
    assert!(
        reviewer
            .resolve_permission(
                peers.source.address().clone(),
                request.request_id.clone(),
                request.revision,
                PermissionAction::Defer,
                None
            )
            .await
            .is_err()
    );
    reviewer
        .resolve_permission(
            peers.source.address().clone(),
            request.request_id,
            request.revision,
            PermissionAction::Approve,
            None,
        )
        .await
        .unwrap();
    let decision = pending.resolution.await.unwrap();
    assert!(
        matches!(decision, PermissionResolution::Decision(decision) if matches!(&decision.actor, DecisionActor::Flow { session_id, run_id } if session_id == &peers.target.address().session_id.to_string() && run_id.0 == manager_lease.run.0))
    );
    assert!(
        context
            .flow_registry
            .lookup_run(&crate::event::FlowRunId::new(manager_lease.run.0))
            .is_none()
    );
    tokio::time::timeout(Duration::from_secs(5), routing)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    peers.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn denied_cold_target_inspection_does_not_create_or_recover_an_execution_owner() {
    let peers = Peers::new().await;
    let store = peers.target.clone().shutdown().await.unwrap();
    let path = store.dir().unwrap().join("events.jsonl");
    drop(store);
    let before = std::fs::read(&path).unwrap();
    peers.revoke().await;
    assert_eq!(
        peers
            .client
            .inspect(peers.target.address().clone())
            .await
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(
        peers
            .target_host
            .session(peers.target.address().session_id.0)
            .unwrap()
            .loaded_flow(&peers.target.address().instance_id)
            .is_none()
    );
    assert_eq!(std::fs::read(path).unwrap(), before);
    peers.shutdown().await;
}
