//! Run two persistent peers without a model or external tools.

use std::{collections::BTreeMap, io, num::NonZeroUsize, path::PathBuf, time::Duration};

use atman_rt::{InstanceState, RunId, TurnId};
use atman_runtime::{
    Executor,
    flow_authority::EffectiveAuthority,
    flow_runtime::{FlowSnapshot, FlowWorker, execution::TurnRequest, host::RuntimeHost},
    flow_store::{
        EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowTransition, RuntimeStore,
        SessionDescriptor,
        inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity},
        subscription::{FlowEventFilter, FlowSubscription, SubscriptionPurpose},
    },
    message::MessageOrigin,
    source_program::RevisionBundle,
};
use uuid::Uuid;

fn prepare(snapshot: &FlowSnapshot) -> io::Result<Option<TurnRequest>> {
    let turn = TurnId::now();
    let context = snapshot
        .pending
        .iter()
        .map(|envelope| atman_runtime::flow_runtime::input::context_message(envelope, &turn))
        .collect::<io::Result<Vec<_>>>()?;
    let text = context
        .iter()
        .map(|message| message.text_concat())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(Some(TurnRequest {
        turn,
        run: RunId::now(),
        messages: snapshot
            .pending
            .iter()
            .map(|envelope| envelope.id)
            .collect(),
        arguments: [("input".into(), text.into())].into(),
        invocation: BTreeMap::new(),
        context,
        authority: EffectiveAuthority::root(&Default::default(), false, None),
        yield_interval: NonZeroUsize::new(64).unwrap(),
    }))
}

fn descriptor(session: Uuid) -> FlowDescriptor {
    FlowDescriptor {
        address: FlowRef {
            session_id: session.into(),
            instance_id: FlowInstanceId::now(),
        },
        creation_request: Uuid::new_v4(),
        created_by: None,
        content: EntrySnapshot {
            name: "peer".into(),
            path: PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/examples/peer_flows.at"
            )),
            bundle: RevisionBundle {
                entry: "entry:peer_flows.at".into(),
                sources: [(
                    "entry:peer_flows.at".into(),
                    include_str!("peer_flows.at").into(),
                )]
                .into(),
                edges: BTreeMap::new(),
            },
        }
        .into(),
    }
}

async fn change(peer: &FlowWorker, transition: FlowTransition) -> io::Result<()> {
    peer.transition(peer.state().await?.revision(), transition)
        .await?;
    Ok(())
}

async fn deliver(host: &RuntimeHost, target: &FlowRef, user: Uuid, text: &str) -> io::Result<()> {
    // This standalone executable is the trusted operator. Network hosts must authenticate first.
    host.deliver(
        DeliveryRequest {
            expected_head: None,
            request_id: Uuid::new_v4(),
            sender: SenderIdentity::User(user),
            recipient: target.clone(),
            delivery: DeliveryMode::AtBoundary,
            cause: None,
            correlation: None,
            scope: None,
            payload: InboxPayload::Interjection { text: text.into() },
        },
        |_| Ok(()),
    )
    .await?;
    Ok(())
}

async fn settle(host: &RuntimeHost) -> io::Result<()> {
    tokio::time::timeout(Duration::from_secs(10), host.wait_quiescent())
        .await
        .map_err(io::Error::other)?
}

async fn answers(peer: &FlowWorker) -> io::Result<usize> {
    Ok(peer.read_output(|_| Ok(())).await?.len())
}

async fn wait_for(peer: &FlowWorker, ready: impl Fn(&FlowSnapshot) -> bool) -> io::Result<()> {
    let mut commits = peer.subscribe_commits();
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            commits.borrow_and_update();
            if ready(&peer.snapshot().await?) {
                return Ok(());
            }
            commits.changed().await.map_err(io::Error::other)?;
        }
    })
    .await
    .map_err(io::Error::other)?
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let data = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: peer_flows <data-directory>")?;
    if args.next().is_some() {
        return Err("usage: peer_flows <data-directory>".into());
    }
    let root = RuntimeStore::open(&data)?;
    let host = RuntimeHost::default();
    let session_id = Uuid::new_v4();
    let session = host.attach_session(root.create_session(SessionDescriptor {
        id: session_id,
        project: None,
        configuration: Default::default(),
        title: "Peer demo".into(),
    })?)?;
    let a = session.create_flow(descriptor(session_id)).await?;
    let b = session.create_flow(descriptor(session_id)).await?;
    let addresses = [a.address().clone(), b.address().clone()];
    let pause = Uuid::new_v4().into();
    let user = Uuid::new_v4();
    for peer in [&a, &b] {
        change(peer, FlowTransition::Activated).await?;
        session
            .schedule_flow(peer.address().instance_id.clone(), Executor::new(), prepare)
            .await?;
    }
    // Both peers observe public results. One-shot watches prevent an endless echo cycle.
    for (recipient, source) in [(&a, &b), (&b, &a)] {
        host.watch_events(
            recipient.address().clone(),
            FlowSubscription {
                id: Uuid::new_v4(),
                source: source.address().clone(),
                filter: FlowEventFilter::FinalAnswers,
                once: true,
                purpose: SubscriptionPurpose::Watcher,
                expires_at: None,
                reply: None,
            },
            |_| Ok(()),
        )
        .await?;
    }
    change(&b, FlowTransition::ExecutionPaused { key: pause }).await?;
    deliver(&host, a.address(), user, "A starts the exchange").await?;
    // The still-live watch on B intentionally prevents global quiescence here.
    wait_for(&a, |state| {
        state.state.can_start_turn() && state.pending.is_empty()
    })
    .await?;
    wait_for(&b, |state| state.pending.len() == 1).await?;
    deliver(&host, b.address(), user, "Additional input while paused").await?;
    assert_eq!(answers(&a).await?, 1);
    assert_eq!(answers(&b).await?, 0);
    let queued: Vec<_> = b
        .snapshot()
        .await?
        .pending
        .iter()
        .map(|item| item.id)
        .collect();
    assert_eq!(queued.len(), 2);
    println!("A completed; B is paused with two durable inputs");
    host.shutdown().await?;

    // Reattach the same instances and explicitly restore their scheduling policy.
    let host = RuntimeHost::default();
    let session = host.attach_session(root.open_session(session_id)?)?;
    let a = session.open_flow(addresses[0].instance_id.clone()).await?;
    let b = session.open_flow(addresses[1].instance_id.clone()).await?;
    assert_eq!(
        b.snapshot()
            .await?
            .pending
            .iter()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        queued
    );
    assert_eq!(b.state().await?.pauses(), &[pause]);
    assert_eq!(answers(&a).await?, 1);
    for peer in [&a, &b] {
        session
            .schedule_flow(peer.address().instance_id.clone(), Executor::new(), prepare)
            .await?;
    }
    change(&b, FlowTransition::ExecutionResumed { key: pause }).await?;
    settle(&host).await?;
    assert_eq!(answers(&b).await?, 1);
    assert_eq!(answers(&a).await?, 2);
    assert!(b.snapshot().await?.pending.is_empty());
    assert!(a.snapshot().await?.pending.is_empty());
    println!("Reopened: B consumed its batch; its result woke A for another turn");

    change(&b, FlowTransition::CloseRequested).await?;
    change(&b, FlowTransition::Closed).await?;
    deliver(&host, a.address(), user, "A continues after B closes").await?;
    settle(&host).await?;
    assert_eq!(answers(&a).await?, 3);
    host.shutdown().await?;
    let session = root.open_session(session_id)?;
    let mut restored_a = session.open_flow(&addresses[0].instance_id)?;
    let restored_b = session.open_flow(&addresses[1].instance_id)?;
    assert_eq!(
        restored_a
            .history_messages()?
            .iter()
            .filter(|message| message.origin == MessageOrigin::FinalAnswer)
            .count(),
        3
    );
    assert_eq!(restored_b.state().state(), &InstanceState::Closed);
    println!(
        "Verified retained history and independent closure in {}",
        session.dir().display()
    );
    println!(
        "A: {}\nB: {}",
        addresses[0].instance_id, addresses[1].instance_id
    );
    Ok(())
}
