use super::*;
use crate::flow_runtime::launch::EntryExecution;
use crate::flow_store::{
    EntrySnapshot, FlowInstanceId, FlowTransition,
    access::FlowGrant,
    inbox::SenderIdentity,
    launch::{EntryBinding, EntryRequest, EntryStart, SynchronousReply},
    subscription::SubscriptionPurpose,
};

fn launch(
    peers: &super::super::tests::Peers,
) -> (FlowDescriptor, LaunchInput, crate::event::TurnId) {
    let descriptor = FlowDescriptor {
        address: FlowRef {
            session_id: peers.source.address().session_id,
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
                    "flow main() -> int { return 42 }".into(),
                )]
                .into(),
            },
        }
        .into(),
    };
    let turn = crate::event::TurnId::now();
    let input = LaunchInput {
        name: None,
        display_label: None,
        scope: None,
        workspace: None,
        initial_goal: None,
        inherited_context: vec![],
        continued_from: None,
        inherited_outputs: vec![],
        request: EntryRequest {
            request_id: descriptor.creation_request,
            sender: SenderIdentity::User(Uuid::now_v7()),
            recipient: descriptor.address.clone(),
            arguments: Default::default(),
            invocation: Default::default(),
            start: EntryStart::Invoke { user: None },
        },
        result_route: Some(ResultRoute {
            recipient: peers.target.address().clone(),
            subscription: Uuid::now_v7(),
            synchronous: None,
        }),
        execution: Some(EntryBinding {
            user_routes: false,
            session_start: false,
            first_turn: turn.clone(),
            first_run: crate::event::FlowRunId::now(),
            authority: EffectiveAuthority::root(&Default::default(), false, None),
        }),
    };
    (descriptor, input, turn)
}

async fn grant(peers: &super::super::tests::Peers, publisher: &FlowRef) -> Uuid {
    let session = peers
        .target_host
        .session(peers.target.address().session_id.0)
        .unwrap();
    let id = Uuid::now_v7();
    session
        .grant_access(
            session.snapshot().await.unwrap().cursor,
            FlowGrant {
                id,
                grantee: peers.target.address().clone(),
                target: AccessTarget::Flow(publisher.clone()),
                actions: [FlowAction::Watch].into(),
            },
        )
        .await
        .unwrap();
    id
}

async fn execution(peers: &super::super::tests::Peers) -> EntryExecution {
    EntryExecution {
        executor: crate::Executor::new(),
        authority: EffectiveAuthority::root(&Default::default(), false, None),
        trust: Default::default(),
        yield_interval: std::num::NonZeroUsize::new(16).unwrap(),
        catalog: peers.source_session.snapshot().await.unwrap().cursor,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cross_session_host_launch_registers_original_result_and_completion_before_activation() {
    let peers = super::super::tests::Peers::new().await;
    let (descriptor, mut input, turn) = launch(&peers);
    let grant_id = grant(&peers, &descriptor.address).await;
    peers
        .target
        .transition(
            peers.target.state().await.unwrap().revision(),
            FlowTransition::TurnStarted {
                turn: crate::event::TurnId::now(),
                run: crate::event::FlowRunId::now(),
            },
        )
        .await
        .unwrap();
    let state = peers.target.state().await.unwrap();
    let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) = state.state()
    else {
        panic!("recipient must have its original active continuation")
    };
    let client = FlowClient::bind(
        peers.target_host.clone(),
        &peers.target,
        active.lease.clone(),
    )
    .unwrap();
    let waiter = client
        .prepare_reply(descriptor.address.clone(), descriptor.creation_request)
        .await
        .unwrap();
    input.result_route.as_mut().unwrap().synchronous = Some(SynchronousReply {
        turn: turn.clone(),
        subscription: Uuid::now_v7(),
        waiter: waiter.waiter().clone(),
    });
    let requested = input.clone();
    let created = descriptor.clone();
    let launched = peers
        .source_host
        .launch_entry(
            descriptor.clone(),
            input.clone(),
            execution(&peers).await,
            move |actual_descriptor, actual_input| {
                if actual_descriptor == &created && actual_input == &requested {
                    Ok(())
                } else {
                    Err(denied("original accepted launch changed"))
                }
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        tokio::time::timeout(Duration::from_secs(5), waiter.completion(turn))
            .await
            .unwrap()
            .unwrap(),
        crate::Value::Int(42)
    ));
    let routes = peers.target.flow_subscriptions().await.unwrap();
    let route = input.result_route.as_ref().unwrap();
    let answers = routes
        .iter()
        .find(|record| record.subscription.id == route.subscription)
        .unwrap();
    assert_eq!(answers.subscription.source, descriptor.address);
    assert_eq!(
        answers.subscription.purpose,
        SubscriptionPurpose::SpawnResult
    );
    assert!(answers.started_after.sequence() < launched.record.receipt.sequence());
    assert_eq!(launched.record.input.request.sender, input.request.sender);
    let target_session = peers
        .target_host
        .session(peers.target.address().session_id.0)
        .unwrap();
    target_session
        .revoke_access(target_session.snapshot().await.unwrap().cursor, grant_id)
        .await
        .unwrap();
    let retried = peers
        .source_host
        .launch_entry(descriptor, input, execution(&peers).await, |_, _| {
            Err(denied(
                "a committed retry must not require a new authorization",
            ))
        })
        .await
        .unwrap();
    assert_eq!(retried.record.receipt, launched.record.receipt);
    assert_eq!(
        peers.target.flow_subscriptions().await.unwrap().len(),
        routes.len()
    );
    peers.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pending_launch_rejection_keeps_prepared_owner_retryable_without_forging_a_live_principal()
{
    let peers = super::super::tests::Peers::new().await;
    let (descriptor, input, _) = launch(&peers);
    let denied_launch = peers
        .source_host
        .launch_entry(
            descriptor.clone(),
            input.clone(),
            execution(&peers).await,
            |_, _| Ok(()),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(denied_launch.kind(), io::ErrorKind::PermissionDenied);
    let worker = peers.source_host.flow(&descriptor.address).await.unwrap();
    assert_eq!(
        worker.state().await.unwrap().state(),
        &atman_rt::InstanceState::Prepared
    );
    assert!(worker.launch_record().await.unwrap().is_none());
    assert!(peers.target.flow_subscriptions().await.unwrap().is_empty());
    grant(&peers, &descriptor.address).await;
    let anchor = worker
        .request(|store| Ok(store.observation_anchor()))
        .await
        .unwrap();
    let operation = PeerOperation::RegisterLaunchResult {
        request: PendingLaunchRequest {
            source: descriptor.address.clone(),
            creation_request: descriptor.creation_request,
            creator: None,
            route: input.result_route.as_ref().unwrap().clone(),
            anchor: anchor.clone().into(),
            launch_digest: "0".repeat(64),
        },
    };
    let forged = PeerCall::Execute {
        principal: PeerPrincipal::PendingLaunch(PendingLaunchPrincipal {
            owner: peers.source_host.generation(),
            nonce: Uuid::now_v7(),
            source: descriptor.address.clone(),
        }),
        binding: operation.binding(peers.target_host.generation()).unwrap(),
        operation: Box::new(operation.clone()),
    };
    assert_eq!(
        handle_peer_call(
            peers.target_host.clone(),
            peers.source_host.peer_identity().unwrap(),
            forged,
        )
        .await
        .unwrap_err()
        .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(
        peers
            .client
            .peer_operation(operation)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    let unrelated = PeerOperation::Anchor {
        recipient: peers.target.address().clone(),
        source: descriptor.address.clone(),
    };
    assert_eq!(
        handle_peer_call(
            peers.source_host.clone(),
            peers.source_host.peer_identity().unwrap(),
            PeerCall::Execute {
                principal: PeerPrincipal::PendingLaunch(PendingLaunchPrincipal {
                    owner: peers.source_host.generation(),
                    nonce: Uuid::now_v7(),
                    source: descriptor.address.clone(),
                }),
                binding: unrelated.binding(peers.source_host.generation()).unwrap(),
                operation: Box::new(unrelated),
            },
        )
        .await
        .unwrap_err()
        .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(peers.target.flow_subscriptions().await.unwrap().is_empty());
    let accepted = peers
        .source_host
        .launch_entry(descriptor, input, execution(&peers).await, |_, _| Ok(()))
        .await
        .unwrap();
    assert!(accepted.record.acceptance.is_some());
    assert_eq!(peers.target.flow_subscriptions().await.unwrap().len(), 1);
    peers.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn retired_pending_launch_cannot_attach_to_a_reopened_prepared_worker() {
    let peers = super::super::tests::Peers::new().await;
    let (descriptor, input, _) = launch(&peers);
    let worker = peers
        .source_session
        .create_flow(descriptor.clone())
        .await
        .unwrap();
    let anchor = worker
        .request(|store| Ok(store.observation_anchor()))
        .await
        .unwrap();
    let operation = PeerOperation::RegisterLaunchResult {
        request: PendingLaunchRequest {
            source: descriptor.address.clone(),
            creation_request: descriptor.creation_request,
            creator: None,
            route: input.result_route.as_ref().unwrap().clone(),
            anchor: anchor.into(),
            launch_digest: blake3::hash(&serde_json::to_vec(&(&descriptor, &input)).unwrap())
                .to_hex()
                .to_string(),
        },
    };
    let binding = operation.binding(peers.target_host.generation()).unwrap();
    let principal = PendingLaunchPrincipal {
        owner: peers.source_host.generation(),
        nonce: Uuid::now_v7(),
        source: descriptor.address.clone(),
    };
    let identity = Arc::new(PendingLaunchIdentity {
        principal: principal.clone(),
        binding: binding.clone(),
        descriptor: descriptor.clone(),
        input,
        authorize: Arc::new(|_, _| Ok(())),
        access: Arc::downgrade(&peers.source_session.access),
        state: worker.subscribe_state(),
    });
    peers
        .source_host
        .invocation_registry()
        .pending_launches
        .lock()
        .unwrap()
        .insert(principal.nonce, Arc::downgrade(&identity));
    let guard =
        super::super::client::read_guard(&peers.source_host, descriptor.address.session_id.0)
            .await
            .unwrap();
    validate(
        &peers.source_host,
        principal.clone(),
        binding.clone(),
        guard,
    )
    .await
    .unwrap();
    worker.stop_execution().await.unwrap();
    let replacement = peers
        .source_session
        .open_flow(descriptor.address.instance_id)
        .await
        .unwrap();
    assert!(!replacement.subscribe_state().same_channel(&identity.state));
    let guard =
        super::super::client::read_guard(&peers.source_host, descriptor.address.session_id.0)
            .await
            .unwrap();
    assert_eq!(
        validate(&peers.source_host, principal, binding, guard)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    peers.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejected_pending_launch_does_not_open_or_recover_a_cold_result_recipient() {
    let peers = super::super::tests::Peers::new().await;
    let (descriptor, input, _) = launch(&peers);
    let recipient = peers.target.address().clone();
    let session = peers.target_host.session(recipient.session_id.0).unwrap();
    peers
        .target
        .transition(
            peers.target.state().await.unwrap().revision(),
            FlowTransition::TurnStarted {
                turn: crate::event::TurnId::now(),
                run: crate::event::FlowRunId::now(),
            },
        )
        .await
        .unwrap();
    drop(peers.target.clone().shutdown().await.unwrap());
    let journal = peers
        .data
        .path()
        .join("runtime-v1/sessions")
        .join(recipient.session_id.to_string())
        .join("flows")
        .join(recipient.instance_id.to_string())
        .join("events.jsonl");
    let before = std::fs::read(&journal).unwrap();
    assert!(session.loaded_flow(&recipient.instance_id).is_none());
    let denied = peers
        .source_host
        .launch_entry(
            descriptor.clone(),
            input.clone(),
            execution(&peers).await,
            |_, _| Ok(()),
        )
        .await
        .err()
        .unwrap();
    assert_eq!(denied.kind(), io::ErrorKind::PermissionDenied);
    assert!(session.loaded_flow(&recipient.instance_id).is_none());
    assert_eq!(std::fs::read(&journal).unwrap(), before);

    grant(&peers, &descriptor.address).await;
    let worker = peers.source_host.flow(&descriptor.address).await.unwrap();
    let anchor = worker
        .request(|store| Ok(store.observation_anchor()))
        .await
        .unwrap();
    let operation = PeerOperation::RegisterLaunchResult {
        request: PendingLaunchRequest {
            source: descriptor.address.clone(),
            creation_request: descriptor.creation_request,
            creator: None,
            route: input.result_route.as_ref().unwrap().clone(),
            anchor: anchor.into(),
            launch_digest: blake3::hash(&serde_json::to_vec(&(&descriptor, &input)).unwrap())
                .to_hex()
                .to_string(),
        },
    };
    assert_eq!(
        handle_peer_call(
            peers.target_host.clone(),
            peers.source_host.peer_identity().unwrap(),
            PeerCall::Execute {
                principal: PeerPrincipal::PendingLaunch(PendingLaunchPrincipal {
                    owner: peers.source_host.generation(),
                    nonce: Uuid::now_v7(),
                    source: descriptor.address.clone(),
                }),
                binding: operation.binding(peers.target_host.generation()).unwrap(),
                operation: Box::new(operation),
            },
        )
        .await
        .unwrap_err()
        .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(session.loaded_flow(&recipient.instance_id).is_none());
    assert_eq!(std::fs::read(&journal).unwrap(), before);
    peers
        .source_host
        .launch_entry(descriptor, input, execution(&peers).await, |_, _| Ok(()))
        .await
        .unwrap();
    assert!(session.loaded_flow(&recipient.instance_id).is_some());
    peers.shutdown().await;
}
