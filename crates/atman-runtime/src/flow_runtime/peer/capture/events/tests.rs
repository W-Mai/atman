use super::*;
use crate::{
    flow_runtime::peer::tests::Peers,
    flow_store::{
        FlowTransition,
        completion::InvocationResult,
        publication::MessagePublication,
        subscription::{FlowEventFilter, SubscriptionPurpose},
    },
    message::MessageOrigin,
};

async fn watch(peers: &Peers) -> SubscriptionRecord {
    peers
        .client
        .watch_events(FlowSubscription {
            id: Uuid::now_v7(),
            source: peers.target.address().clone(),
            filter: FlowEventFilter::FinalAnswers,
            once: false,
            expires_at: None,
            purpose: SubscriptionPurpose::Watcher,
            reply: None,
        })
        .await
        .unwrap()
}

async fn publish(peers: &Peers, count: usize, size: usize) -> (Vec<u64>, u64) {
    peers
        .target
        .request(move |store| {
            store.transition(
                store.state().revision(),
                FlowTransition::TurnStarted {
                    turn: atman_rt::TurnId::now(),
                    run: atman_rt::RunId::now(),
                },
            )?;
            let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) =
                store.state().state()
            else {
                panic!("publisher must be active")
            };
            let lease = active.lease.clone();
            let body = "x".repeat(size);
            let mut positions = Vec::new();
            for index in 0..count {
                let message = Message {
                    origin: MessageOrigin::FinalAnswer,
                    ..Message::assistant_text(
                        lease.turn.clone(),
                        format!("event {index:02}\n{body}"),
                    )
                };
                let published = store.publish_message(MessagePublication {
                    tool_artifacts: Vec::new(),
                    id: Uuid::now_v7(),
                    lease: lease.clone(),
                    run: lease.run.clone(),
                    context_run: None,
                    message,
                })?;
                positions.push(published.receipt.sequence());
            }
            store.finish_turn(
                lease,
                InvocationResult::Unobserved(atman_rt::TurnOutcome::Completed),
            )?;
            Ok((positions, store.cursor().sequence()))
        })
        .await
        .unwrap()
}

async fn state(peers: &Peers, id: Uuid) -> (Vec<u64>, u64) {
    peers
        .source
        .request(move |store| {
            let positions = store
                .pending_inputs()
                .filter_map(|entry| entry.request.cause.as_ref().map(|source| source.sequence))
                .collect();
            let cursor = store
                .flow_subscriptions()
                .find(|record| record.subscription.id == id)
                .unwrap()
                .cursor;
            Ok((positions, cursor))
        })
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn original_event_page_larger_than_transport_limit_arrives_whole_and_advances_only_with_delivery()
 {
    let peers = Peers::new().await;
    let registered = watch(&peers).await;
    let (blocked, resume) = peers.block_next_chunk();
    let (expected, through) = publish(&peers, 40, 512 * 1024).await;
    tokio::time::timeout(Duration::from_secs(20), blocked)
        .await
        .unwrap()
        .unwrap();
    let (pending, cursor) = state(&peers, registered.subscription.id).await;
    assert!(pending.is_empty());
    assert_eq!(cursor, registered.cursor);
    let after = registered.cursor;
    let page = peers
        .target
        .request(move |store| store.events_after(after, std::num::NonZeroUsize::new(64).unwrap()))
        .await
        .unwrap();
    assert_eq!(page.through(), through);
    assert!(
        serde_json::to_vec(&PeerReply::Events(page.into()))
            .unwrap()
            .len()
            > 16 * 1024 * 1024
    );
    let mut commits = peers.source.subscribe_commits();
    resume.notify_one();
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            commits.borrow_and_update();
            let (actual, cursor) = state(&peers, registered.subscription.id).await;
            if cursor == through {
                assert_eq!(actual, expected);
                break;
            }
            commits.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while peers.target_host.peer_transfers_pending() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    peers.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn revoked_watch_during_event_transfer_keeps_original_cursor_and_cleans_the_unapplied_page() {
    let peers = Peers::new().await;
    let registered = watch(&peers).await;
    let (blocked, resume) = peers.block_next_chunk();
    publish(&peers, 2, 128 * 1024).await;
    tokio::time::timeout(Duration::from_secs(10), blocked)
        .await
        .unwrap()
        .unwrap();
    assert!(peers.target_host.peer_transfers_pending());
    peers.revoke().await;
    resume.notify_one();
    tokio::time::timeout(Duration::from_secs(5), async {
        while peers.target_host.peer_transfers_pending() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (pending, cursor) = state(&peers, registered.subscription.id).await;
    assert!(pending.is_empty());
    assert_eq!(cursor, registered.cursor);
    peers.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cancelling_the_original_observer_releases_its_page_without_waiting_for_an_inflight_chunk()
{
    use crate::flow_runtime::host::SessionHandoffOutcome;
    let peers = Peers::new().await;
    let registered = watch(&peers).await;
    let (blocked, resume) = peers.block_next_chunk();
    publish(&peers, 2, 128 * 1024).await;
    tokio::time::timeout(Duration::from_secs(10), blocked)
        .await
        .unwrap()
        .unwrap();
    peers
        .client
        .unwatch_events(registered.subscription.id)
        .await
        .unwrap();
    let lease = peers.client.lease.clone();
    peers
        .source
        .request(move |store| {
            store.finish_turn(
                lease,
                InvocationResult::Unobserved(atman_rt::TurnOutcome::Completed),
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let operation = Uuid::now_v7();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                peers
                    .source_host
                    .handoff_session(peers.source.address().session_id.0, operation)
                    .await
                    .unwrap(),
                SessionHandoffOutcome::Released { .. }
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        while peers.target_host.peer_transfers_pending() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    resume.notify_one();
    let inspected = crate::flow_store::RuntimeStore::inspect_session(
        peers.data.path(),
        peers.source.address().session_id.0,
    )
    .unwrap()
    .inspect_flow(&peers.source.address().instance_id)
    .unwrap();
    assert_eq!(inspected.pending_inputs, 0);
    peers.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dropping_an_event_reader_off_runtime_releases_only_its_page_before_its_chunk_returns() {
    let peers = Peers::new().await;
    let registered = watch(&peers).await;
    let (blocked, resume) = peers.block_next_chunk();
    publish(&peers, 2, 128 * 1024).await;
    tokio::time::timeout(Duration::from_secs(10), blocked)
        .await
        .unwrap()
        .unwrap();
    let original = {
        let captures = peers
            .target_host
            .invocation_registry()
            .captures
            .lock()
            .unwrap();
        assert_eq!(captures.len(), 1);
        *captures.keys().next().unwrap()
    };

    // Keep the host observer blocked while independently reading its real subscription page.
    let (blocked, release_reader) = peers.block_next_chunk();
    let host = peers.source_host.clone();
    let recipient = peers.source.clone();
    let publisher = peers.target.address().clone();
    let after = registered.cursor;
    let mut reader = Box::pin(async move {
        crate::flow_runtime::peer::durable::subscription_events(
            &host, &recipient, &publisher, after,
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::select! {
            _ = &mut reader => panic!("reader completed before its blocked chunk"),
            ready = blocked => ready.unwrap(),
        }
    })
    .await
    .unwrap();
    assert_eq!(
        peers
            .target_host
            .invocation_registry()
            .captures
            .lock()
            .unwrap()
            .len(),
        2
    );
    std::thread::spawn(move || {
        assert!(tokio::runtime::Handle::try_current().is_err());
        drop(reader);
    })
    .join()
    .unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let released = {
                let captures = peers
                    .target_host
                    .invocation_registry()
                    .captures
                    .lock()
                    .unwrap();
                captures.len() == 1 && captures.contains_key(&original)
            };
            if released {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (pending, cursor) = state(&peers, registered.subscription.id).await;
    assert!(pending.is_empty());
    assert_eq!(cursor, registered.cursor);
    release_reader.notify_one();
    resume.notify_one();
    peers.shutdown().await;
}
