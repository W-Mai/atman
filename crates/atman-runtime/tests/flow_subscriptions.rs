use std::{io::Write, num::NonZeroUsize};

use atman_rt::{RunId, TurnId, TurnOutcome};
use atman_runtime::flow_store::inbox::{InboxDisposition, InboxPayload, InputContent};
use atman_runtime::flow_store::publication::MessagePublication;
use atman_runtime::flow_store::subscription::{
    FlowEventFilter, FlowSubscription, SubscriptionPurpose, SubscriptionStatus,
};
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowStore, FlowTransition,
    RuntimeStore, SessionDescriptor,
};
use atman_runtime::message::{Message, MessageOrigin};
use atman_runtime::source_program::RevisionBundle;
use uuid::Uuid;

fn descriptor(session: Uuid) -> FlowDescriptor {
    FlowDescriptor {
        address: FlowRef {
            session_id: session.into(),
            instance_id: FlowInstanceId::now(),
        },
        creation_request: Uuid::new_v4(),
        created_by: None,
        content: EntrySnapshot {
            name: "main".into(),
            path: "main.at".into(),
            bundle: RevisionBundle {
                entry: "entry:main.at".into(),
                sources: [(
                    "entry:main.at".into(),
                    "flow main() -> int { return 1 }".into(),
                )]
                .into(),
                edges: Default::default(),
            },
        }
        .into(),
    }
}

fn transition(store: &mut FlowStore, transition: FlowTransition) {
    store
        .transition(store.state().revision(), transition)
        .unwrap();
}

fn answer(store: &mut FlowStore, text: &str, isolated: bool) {
    let lease = store
        .state()
        .clone()
        .begin_turn(TurnId::now(), RunId::now())
        .unwrap();
    transition(
        store,
        FlowTransition::TurnStarted {
            turn: lease.turn.clone(),
            run: lease.run.clone(),
        },
    );
    let mut message = Message::assistant_text(lease.turn.clone(), text);
    message.origin = MessageOrigin::FinalAnswer;
    store
        .publish_message(MessagePublication {
            id: Uuid::new_v4(),
            run: lease.run.clone(),
            context_run: isolated.then(|| lease.run.clone()),
            message,
            lease: lease.clone(),
        })
        .unwrap();
    transition(
        store,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    );
}

fn subscription(source: &FlowStore, purpose: SubscriptionPurpose, once: bool) -> FlowSubscription {
    FlowSubscription {
        id: Uuid::new_v4(),
        source: source.descriptor().address.clone(),
        filter: FlowEventFilter::FinalAnswers,
        once,
        purpose,
        expires_at: None,
        reply: None,
    }
}

#[tokio::test]
async fn relation_inspection_preserves_direction_revocation_and_read_only_ownership() {
    use atman_runtime::flow_runtime::catalog::SessionCatalog;
    use atman_runtime::flow_store::{
        access::{AccessTarget, FlowAction, FlowGrant},
        relation::RelationKind,
    };

    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            id,
            project: None,
            configuration: Default::default(),
            title: "Relations".into(),
        })
        .unwrap();
    let mut source = session.create_flow(descriptor(id)).unwrap();
    let source_address = source.descriptor().address.clone();
    let mut recipient_descriptor = descriptor(id);
    recipient_descriptor.created_by = Some(source_address.clone());
    let creation = recipient_descriptor.creation_request;
    let recipient_address = recipient_descriptor.address.clone();
    let mut recipient = session.create_flow(recipient_descriptor).unwrap();
    transition(&mut source, FlowTransition::Activated);
    transition(&mut recipient, FlowTransition::Activated);
    let watch = subscription(&source, SubscriptionPurpose::Watcher, false);
    recipient
        .subscribe_events(watch.clone(), &source.observation_anchor())
        .unwrap();
    let result = subscription(&source, SubscriptionPurpose::SpawnResult, false);
    recipient
        .subscribe_events(result.clone(), &source.observation_anchor())
        .unwrap();
    let grant = FlowGrant {
        id: Uuid::new_v4(),
        grantee: recipient_address.clone(),
        target: AccessTarget::Flow(source_address.clone()),
        actions: [
            FlowAction::ReadOutput,
            FlowAction::ReadContext,
            FlowAction::Send,
        ]
        .into(),
    };
    session
        .grant_access(&session.cursor().clone(), grant.clone())
        .unwrap();
    let catalog = SessionCatalog::new(None, Some(dir.path().to_owned()));
    let relations = catalog.relations(id).await.unwrap();
    assert_eq!(relations.len(), 4);
    let created = relations.iter().find(|edge| edge.id == creation).unwrap();
    assert_eq!(created.kind, RelationKind::Created);
    assert_eq!(created.source, source_address);
    assert_eq!(
        created.target,
        AccessTarget::Flow(recipient_address.clone())
    );
    let observed = relations.iter().find(|edge| edge.id == watch.id).unwrap();
    assert_eq!(observed.kind, RelationKind::Watch);
    assert_eq!(observed.source, recipient_address);
    assert_eq!(observed.target, AccessTarget::Flow(source_address));
    assert!(observed.active);
    assert_eq!(
        relations
            .iter()
            .filter(|edge| edge.kind == RelationKind::Read)
            .count(),
        1
    );
    assert!(!relations.iter().any(|edge| edge.id == result.id));

    recipient.unsubscribe_events(watch.id).unwrap();
    session
        .revoke_access(&session.cursor().clone(), grant.id)
        .unwrap();
    let paths = [
        session.dir().join("events.jsonl"),
        source.dir().join("events.jsonl"),
        recipient.dir().join("events.jsonl"),
    ];
    let bytes: Vec<_> = paths
        .iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    let relations = catalog.relations(id).await.unwrap();
    assert_eq!(relations.len(), 2);
    assert!(
        !relations
            .iter()
            .find(|edge| edge.id == watch.id)
            .unwrap()
            .active
    );
    // Inspection succeeds while the original writers still hold their exclusive leases.
    drop(recipient);
    drop(source);
    drop(session);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&paths[2])
        .unwrap()
        .write_all(b"{partial")
        .unwrap();
    assert_eq!(catalog.relations(id).await.unwrap(), relations);
    for (index, path) in paths.iter().enumerate() {
        let mut expected = bytes[index].clone();
        if index == 2 {
            expected.extend_from_slice(b"{partial");
        }
        assert_eq!(std::fs::read(path).unwrap(), expected);
    }
}

#[test]
fn delayed_closure_observation_cannot_suppress_an_elapsed_watcher_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            configuration: Default::default(),
            id,
            title: "Deadline".into(),
            project: None,
        })
        .unwrap();
    let mut source = session.create_flow(descriptor(id)).unwrap();
    let mut recipient = session.create_flow(descriptor(id)).unwrap();
    transition(&mut recipient, FlowTransition::Activated);
    let anchor = source.observation_anchor();
    let deadline = chrono::Utc::now() + chrono::Duration::seconds(1);
    let watch = FlowSubscription {
        filter: FlowEventFilter::InstanceClosed,
        expires_at: Some(deadline),
        ..subscription(&source, SubscriptionPurpose::Watcher, true)
    };
    recipient.subscribe_events(watch, &anchor).unwrap();
    transition(&mut source, FlowTransition::CloseRequested);
    transition(&mut source, FlowTransition::Closed);
    std::thread::sleep((deadline - chrono::Utc::now()).to_std().unwrap_or_default());
    let page = source
        .events_after(anchor.cursor().sequence, NonZeroUsize::new(1).unwrap())
        .unwrap();
    recipient.observe_events(&page).unwrap();
    assert!(recipient.pending_inputs().next().is_none());
    assert_eq!(
        recipient.flow_subscriptions().next().unwrap().status,
        SubscriptionStatus::Active
    );
    recipient.expire_flow_watches(chrono::Utc::now()).unwrap();
    assert_eq!(
        recipient.flow_subscriptions().next().unwrap().status,
        SubscriptionStatus::Expired
    );
    let pending: Vec<_> = recipient.pending_inputs().cloned().collect();
    assert_eq!(pending.len(), 1);
    assert!(
        matches!(&pending[0].request.payload, InboxPayload::Timer { content, .. } if content["kind"] == "flow_watch_expired")
    );
    let cursor = recipient.cursor().clone();
    let address = recipient.descriptor().address.clone();
    drop(recipient);
    let mut recipient = session.open_flow(&address.instance_id).unwrap();
    assert_eq!(recipient.observe_events(&page).unwrap(), cursor);
    assert_eq!(
        recipient.expire_flow_watches(chrono::Utc::now()).unwrap(),
        cursor
    );
    assert_eq!(
        recipient.pending_inputs().cloned().collect::<Vec<_>>(),
        pending
    );
}

#[test]
fn closure_retires_routes_without_losing_accepted_replies_or_replaying_events() {
    use atman_runtime::flow_store::{
        inbox::SenderIdentity,
        waiter::{ReplyStatus, ReplyWaiter},
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            configuration: Default::default(),
            id,
            title: "Closure".into(),
            project: None,
        })
        .unwrap();
    let mut source = session.create_flow(descriptor(id)).unwrap();
    let mut recipient = session.create_flow(descriptor(id)).unwrap();
    transition(&mut source, FlowTransition::Activated);
    transition(&mut recipient, FlowTransition::Activated);
    let lease = recipient
        .state()
        .clone()
        .begin_turn(TurnId::now(), RunId::now())
        .unwrap();
    transition(
        &mut recipient,
        FlowTransition::TurnStarted {
            turn: lease.turn.clone(),
            run: lease.run.clone(),
        },
    );
    let anchor = source.observation_anchor();
    answer(&mut source, "ready", false);
    let completed = source
        .completion(None)
        .unwrap()
        .completion
        .lease
        .turn
        .clone();
    // Closure can race with registration after the source owner has supplied its anchor.
    transition(&mut source, FlowTransition::CloseRequested);
    transition(&mut source, FlowTransition::Closed);
    let closure = source.cursor().clone();
    let waiter = ReplyWaiter {
        correlation: Uuid::new_v4(),
        lease: lease.clone(),
        sender: SenderIdentity::Flow(source.descriptor().address.clone()),
    };
    let abandoned = ReplyWaiter {
        correlation: Uuid::new_v4(),
        ..waiter.clone()
    };
    let mut routes = vec![
        subscription(&source, SubscriptionPurpose::Watcher, false),
        FlowSubscription {
            filter: FlowEventFilter::InstanceClosed,
            ..subscription(&source, SubscriptionPurpose::Watcher, true)
        },
        FlowSubscription {
            filter: FlowEventFilter::InstanceClosed,
            ..subscription(&source, SubscriptionPurpose::Watcher, false)
        },
    ];
    for (turn, reply) in [
        (completed, waiter.clone()),
        (TurnId::now(), waiter.clone()),
        (TurnId::now(), abandoned.clone()),
    ] {
        routes.push(FlowSubscription {
            filter: FlowEventFilter::TurnCompleted { turn: Some(turn) },
            reply: Some(reply),
            ..subscription(&source, SubscriptionPurpose::SpawnResult, true)
        });
    }
    for route in &routes {
        recipient.subscribe_events(route.clone(), &anchor).unwrap();
    }
    let before = recipient.cursor().clone();
    assert!(
        recipient
            .subscribe_events(
                subscription(&source, SubscriptionPurpose::Watcher, true),
                &source.observation_anchor()
            )
            .is_err()
    );
    assert_eq!(recipient.cursor(), &before);
    let source_address = source.descriptor().address.clone();
    drop(source);
    let source = session.open_flow(&source_address.instance_id).unwrap();
    let mut position = anchor.cursor().sequence;
    for (index, event_name) in ["final_answer", "turn_completed", "instance_closed"]
        .into_iter()
        .enumerate()
    {
        let page = source
            .events_after(position, NonZeroUsize::new(1).unwrap())
            .unwrap();
        assert!(page.through() > position);
        position = page.through();
        let receipt = recipient.observe_events(&page).unwrap();
        assert_eq!(recipient.observe_events(&page).unwrap(), receipt);
        let inputs: Vec<_> = recipient.pending_inputs().collect();
        assert_eq!(inputs.len(), index + 1);
        let InboxPayload::FlowEvent { event, content, .. } = &inputs[index].request.payload else {
            panic!("flow event")
        };
        assert_eq!(event, event_name);
        if event_name == "instance_closed" {
            assert_eq!(content["subscriptions"].as_array().unwrap().len(), 2);
            assert_eq!(content["receipt"], serde_json::to_value(&closure).unwrap());
        }
    }
    assert_eq!(position, closure.sequence);
    let empty = source
        .events_after(position, NonZeroUsize::new(1).unwrap())
        .unwrap();
    assert_eq!(empty.through(), position);
    let cursor = recipient.cursor().clone();
    assert_eq!(recipient.observe_events(&empty).unwrap(), cursor);
    for (index, route) in routes.iter().enumerate() {
        let record = recipient
            .flow_subscriptions()
            .find(|record| record.subscription.id == route.id)
            .unwrap();
        assert_eq!(
            record.status,
            if (1..=3).contains(&index) {
                SubscriptionStatus::Matched
            } else {
                SubscriptionStatus::SourceClosed
            }
        );
    }
    assert_eq!(
        recipient
            .reply_waiters()
            .iter()
            .find(|record| record.waiter == waiter)
            .unwrap()
            .status,
        ReplyStatus::Pending
    );
    assert!(matches!(
        recipient
            .reply_waiters()
            .iter()
            .find(|record| record.waiter == abandoned)
            .unwrap()
            .status,
        ReplyStatus::Cancelled { .. }
    ));
    let records: Vec<_> = recipient.flow_subscriptions().cloned().collect();
    let pending: Vec<_> = recipient.pending_inputs().cloned().collect();
    let address = recipient.descriptor().address.clone();
    drop(recipient);
    let mut recipient = session.open_flow(&address.instance_id).unwrap();
    // Reopening interrupts the lost receiver turn, but cannot discard its accepted replies.
    let restored = recipient.cursor().clone();
    assert_eq!(restored.sequence, cursor.sequence + 1);
    assert_eq!(
        recipient.flow_subscriptions().cloned().collect::<Vec<_>>(),
        records
    );
    assert_eq!(
        recipient.pending_inputs().cloned().collect::<Vec<_>>(),
        pending
    );
    assert_eq!(
        recipient
            .observe_events(
                &source
                    .events_after(anchor.cursor().sequence, NonZeroUsize::new(10).unwrap())
                    .unwrap()
            )
            .unwrap(),
        restored
    );
    assert!(matches!(
        recipient
            .reply_waiters()
            .iter()
            .find(|record| record.waiter == waiter)
            .unwrap()
            .status,
        ReplyStatus::Cancelled { .. }
    ));
}

#[test]
fn watcher_expiry_preserves_accepted_answers_and_recovers_without_repeating_deadlines() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            configuration: Default::default(),
            id,
            title: "Expiry".into(),
            project: None,
        })
        .unwrap();
    let mut source = session.create_flow(descriptor(id)).unwrap();
    let mut recipient = session.create_flow(descriptor(id)).unwrap();
    transition(&mut source, FlowTransition::Activated);
    transition(&mut recipient, FlowTransition::Activated);
    let anchor = source.observation_anchor();
    let deadline = chrono::Utc::now() + chrono::Duration::days(1);
    let mut bounded = subscription(&source, SubscriptionPurpose::Watcher, false);
    bounded.expires_at = Some(deadline);
    let once = FlowSubscription {
        id: Uuid::new_v4(),
        once: true,
        ..bounded.clone()
    };
    let unbounded = subscription(&source, SubscriptionPurpose::Watcher, false);
    for watch in [&bounded, &once, &unbounded] {
        recipient.subscribe_events(watch.clone(), &anchor).unwrap();
    }
    let pause = Uuid::new_v4().into();
    transition(
        &mut recipient,
        FlowTransition::ExecutionPaused { key: pause },
    );
    answer(&mut source, "ready", false);
    recipient
        .observe_events(
            &source
                .events_after(anchor.cursor().sequence, NonZeroUsize::new(10).unwrap())
                .unwrap(),
        )
        .unwrap();
    assert_eq!(recipient.pending_inputs().count(), 1);
    let expiry_receipt = recipient.expire_flow_watches(deadline).unwrap();
    assert_eq!(
        recipient.expire_flow_watches(deadline).unwrap(),
        expiry_receipt
    );
    assert_eq!(recipient.pending_inputs().count(), 2);
    let last = recipient.pending_inputs().last().unwrap();
    assert!(
        matches!(&last.request.payload, InboxPayload::Timer { timer, due_at, .. } if *timer == bounded.id && *due_at == deadline)
    );
    assert_eq!(
        atman_runtime::flow_runtime::input::context_message(last, &TurnId::now())
            .unwrap()
            .origin,
        MessageOrigin::Watcher
    );
    let prior = source.observation_anchor();
    answer(&mut source, "later", false);
    recipient
        .observe_events(
            &source
                .events_after(prior.cursor().sequence, NonZeroUsize::new(10).unwrap())
                .unwrap(),
        )
        .unwrap();
    let last = recipient.pending_inputs().last().unwrap();
    assert!(
        matches!(&last.request.payload, InboxPayload::FlowEvent { content, .. } if content["subscriptions"].as_array().unwrap().len() == 1)
    );
    recipient
        .subscribe_events(bounded.clone(), &anchor)
        .unwrap();
    let mut changed = bounded.clone();
    changed.expires_at = Some(deadline + chrono::Duration::days(1));
    assert!(recipient.subscribe_events(changed, &anchor).is_err());
    let records: Vec<_> = recipient.flow_subscriptions().cloned().collect();
    let inputs: Vec<_> = recipient.pending_inputs().cloned().collect();
    let address = recipient.descriptor().address.clone();
    drop(recipient);
    let mut recipient = session.open_flow(&address.instance_id).unwrap();
    let before = recipient.cursor().clone();
    assert_eq!(recipient.expire_flow_watches(deadline).unwrap(), before);
    assert_eq!(
        recipient.flow_subscriptions().cloned().collect::<Vec<_>>(),
        records
    );
    assert_eq!(
        recipient.pending_inputs().cloned().collect::<Vec<_>>(),
        inputs
    );
    assert_eq!(
        records
            .iter()
            .find(|r| r.subscription.id == once.id)
            .unwrap()
            .status,
        SubscriptionStatus::Matched
    );
    assert_eq!(
        records
            .iter()
            .find(|r| r.subscription.id == bounded.id)
            .unwrap()
            .status,
        SubscriptionStatus::Expired
    );
    transition(&mut recipient, FlowTransition::CloseRequested);
    assert!(recipient.expire_flow_watches(deadline).is_err());
}

#[test]
fn watcher_ids_cannot_alias_sources_and_result_routes_cannot_expire() {
    use atman_runtime::flow_store::process_watch::{ProcessSource, ProcessWatch};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            configuration: Default::default(),
            id,
            title: "Watcher identities".into(),
            project: None,
        })
        .unwrap();
    let source = session.create_flow(descriptor(id)).unwrap();
    let mut recipient = session.create_flow(descriptor(id)).unwrap();
    transition(&mut recipient, FlowTransition::Activated);
    let anchor = source.observation_anchor();
    let flow = subscription(&source, SubscriptionPurpose::Watcher, false);
    recipient.subscribe_events(flow.clone(), &anchor).unwrap();
    let mut process = ProcessWatch {
        id: flow.id,
        source: ProcessSource::Bash {
            incarnation: Uuid::new_v4(),
            handle: "bg-owned".into(),
        },
        pattern: "ready".into(),
        once: false,
        expires_at: None,
    };
    assert!(
        recipient
            .register_process_watch(process.clone(), 0)
            .is_err()
    );
    process.id = Uuid::new_v4();
    recipient
        .register_process_watch(process.clone(), 0)
        .unwrap();
    assert!(
        recipient
            .subscribe_events(
                FlowSubscription {
                    id: process.id,
                    ..flow
                },
                &anchor
            )
            .is_err()
    );
    let mut result = subscription(&source, SubscriptionPurpose::SpawnResult, false);
    result.expires_at = Some(chrono::Utc::now() + chrono::Duration::days(1));
    assert!(recipient.subscribe_events(result, &anchor).is_err());
    let before = recipient.cursor().clone();
    assert_eq!(
        recipient
            .expire_flow_watches(chrono::Utc::now() + chrono::Duration::days(2))
            .unwrap(),
        before
    );
}

#[test]
fn completion_filters_replay_one_fact_without_fabricating_final_answers() {
    use atman_rt::{Interruption, TurnPhase};
    use atman_runtime::flow_store::completion::{InvocationResult, StoredValue};
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            configuration: Default::default(),
            id,
            title: "Completion subscriptions".into(),
            project: None,
        })
        .unwrap();
    let mut source = session.create_flow(descriptor(id)).unwrap();
    let recipient_info = descriptor(id);
    let mut recipient = session.create_flow(recipient_info.clone()).unwrap();
    transition(&mut source, FlowTransition::Activated);
    transition(&mut recipient, FlowTransition::Activated);
    let lease = source
        .state()
        .clone()
        .begin_turn(TurnId::now(), RunId::now())
        .unwrap();
    let anchor = source.observation_anchor();
    let mut exact = subscription(&source, SubscriptionPurpose::Watcher, true);
    exact.filter = FlowEventFilter::TurnCompleted {
        turn: Some(lease.turn.clone()),
    };
    let mut all = subscription(&source, SubscriptionPurpose::Watcher, false);
    all.filter = FlowEventFilter::TurnCompleted { turn: None };
    for sub in [
        exact.clone(),
        all,
        subscription(&source, SubscriptionPurpose::Watcher, false),
    ] {
        recipient.subscribe_events(sub, &anchor).unwrap();
    }
    transition(
        &mut source,
        FlowTransition::TurnStarted {
            turn: lease.turn.clone(),
            run: lease.run.clone(),
        },
    );
    transition(
        &mut source,
        FlowTransition::PhaseChanged {
            lease: lease.clone(),
            phase: TurnPhase::Cancelling,
        },
    );
    let completion = source
        .finish_turn(lease, InvocationResult::Returned(StoredValue::Int(42)))
        .unwrap();
    assert_eq!(
        completion.completion.outcome,
        TurnOutcome::Interrupted(Interruption::Cancelled)
    );
    assert!(source.published_messages().is_empty());
    let page = source
        .events_after(anchor.cursor().sequence, NonZeroUsize::new(1).unwrap())
        .unwrap();
    let receipt = recipient.observe_events(&page).unwrap();
    let item = recipient.pending_inputs().next().unwrap().clone();
    let InboxPayload::FlowEvent { event, content, .. } = &item.request.payload else {
        panic!("missing completion")
    };
    assert_eq!(event, "turn_completed");
    assert_eq!(content["subscriptions"].as_array().unwrap().len(), 2);
    assert_eq!(
        content["completion"],
        serde_json::to_value(&completion).unwrap()
    );
    assert!(!content.as_object().unwrap().contains_key("message"));
    drop(recipient);
    let mut recipient = session
        .open_flow(&recipient_info.address.instance_id)
        .unwrap();
    assert_eq!(recipient.observe_events(&page).unwrap(), receipt);
    assert_eq!(recipient.pending_inputs().count(), 1);
    assert_eq!(recipient.pending_inputs().next().unwrap(), &item);
    assert_eq!(
        recipient
            .flow_subscriptions()
            .find(|s| s.subscription.id == exact.id)
            .unwrap()
            .status,
        SubscriptionStatus::Matched
    );
    transition(&mut source, FlowTransition::InterruptionRecovered);
    answer(&mut source, "visible answer", false);
    let next = source
        .events_after(page.through(), NonZeroUsize::new(2).unwrap())
        .unwrap();
    recipient.observe_events(&next).unwrap();
    let inputs: Vec<_> = recipient.pending_inputs().collect();
    assert_eq!(inputs.len(), 3);
    let InboxPayload::FlowEvent { content, .. } = &inputs[2].request.payload else {
        panic!("missing completion")
    };
    assert_eq!(content["subscriptions"].as_array().unwrap().len(), 1);
    assert_eq!(content["completion"]["result"]["kind"], "unobserved");
}

#[test]
fn synchronous_results_skip_only_the_initial_turn_and_preserve_other_subscribers_after_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            configuration: Default::default(),
            id,
            title: "Synchronous results".into(),
            project: None,
        })
        .unwrap();
    let mut source = session.create_flow(descriptor(id)).unwrap();
    let recipient_info = descriptor(id);
    let mut recipient = session.create_flow(recipient_info.clone()).unwrap();
    transition(&mut source, FlowTransition::Activated);
    transition(&mut recipient, FlowTransition::Activated);
    let lease = source
        .state()
        .clone()
        .begin_turn(TurnId::now(), RunId::now())
        .unwrap();
    let anchor = source.observation_anchor();
    let mut result = subscription(&source, SubscriptionPurpose::SpawnResult, false);
    result.filter = FlowEventFilter::AnswersExceptTurn(lease.turn.clone());
    let watcher = subscription(&source, SubscriptionPurpose::Watcher, false);
    for sub in [&result, &watcher] {
        recipient.subscribe_events(sub.clone(), &anchor).unwrap();
    }
    recipient
        .result_subscription_proof(result.id, None)
        .unwrap();
    transition(
        &mut source,
        FlowTransition::TurnStarted {
            turn: lease.turn.clone(),
            run: lease.run.clone(),
        },
    );
    for text in ["same answer", "another answer"] {
        let mut message = Message::assistant_text(lease.turn.clone(), text);
        message.origin = MessageOrigin::FinalAnswer;
        source
            .publish_message(MessagePublication {
                id: Uuid::new_v4(),
                run: lease.run.clone(),
                context_run: None,
                message,
                lease: lease.clone(),
            })
            .unwrap();
    }
    transition(
        &mut source,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    );
    let page = source
        .events_after(anchor.cursor().sequence, NonZeroUsize::new(10).unwrap())
        .unwrap();
    recipient.observe_events(&page).unwrap();
    assert_eq!(recipient.pending_inputs().count(), 2);
    for message in recipient.pending_inputs() {
        let InboxPayload::FlowEvent { content, .. } = &message.request.payload else {
            panic!("missing answer")
        };
        assert_eq!(content["subscriptions"].as_array().unwrap().len(), 1);
        assert_eq!(content["subscriptions"][0]["id"], watcher.id.to_string());
    }
    drop(recipient);
    let mut recipient = session
        .open_flow(&recipient_info.address.instance_id)
        .unwrap();
    recipient.observe_events(&page).unwrap();
    assert_eq!(recipient.pending_inputs().count(), 2);
    answer(&mut source, "same answer", false);
    let next = source
        .events_after(page.through(), NonZeroUsize::new(10).unwrap())
        .unwrap();
    recipient.observe_events(&next).unwrap();
    let last = recipient.pending_inputs().last().unwrap();
    let InboxPayload::FlowEvent { content, .. } = &last.request.payload else {
        panic!("missing answer")
    };
    assert_eq!(content["subscriptions"].as_array().unwrap().len(), 2);
    assert_eq!(recipient.pending_inputs().count(), 3);
}

#[test]
fn source_answer_and_watcher_commit_one_inbox_item_with_all_reasons_and_recover_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let session_id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            configuration: Default::default(),
            id: session_id,
            title: "Subscriptions".into(),
            project: None,
        })
        .unwrap();
    let mut source = session.create_flow(descriptor(session_id)).unwrap();
    let recipient_info = descriptor(session_id);
    let mut recipient = session.create_flow(recipient_info.clone()).unwrap();
    transition(&mut recipient, FlowTransition::Activated);
    let anchor = source.observation_anchor();
    let result = subscription(&source, SubscriptionPurpose::SpawnResult, false);
    let mut watcher = subscription(&source, SubscriptionPurpose::Watcher, true);
    watcher.filter = FlowEventFilter::AnswerContains("ready".into());
    for sub in [&result, &watcher] {
        recipient.subscribe_events(sub.clone(), &anchor).unwrap();
    }
    let registered = recipient.cursor().clone();
    assert_eq!(
        recipient.subscribe_events(result.clone(), &anchor).unwrap(),
        registered
    );
    transition(&mut source, FlowTransition::Activated);
    let pause = Uuid::new_v4().into();
    transition(
        &mut recipient,
        FlowTransition::ExecutionPaused { key: pause },
    );
    answer(&mut source, "private ready", true);
    answer(&mut source, "ready", false);
    // The isolated answer is private, but its entry completion is still an observable fact.
    // A page with no matching answer advances once and does not lose the following answer.
    let page = source
        .events_after(anchor.cursor().sequence, NonZeroUsize::new(1).unwrap())
        .unwrap();
    recipient.observe_events(&page).unwrap();
    assert_eq!(recipient.pending_inputs().count(), 0);
    let page = source
        .events_after(page.through(), NonZeroUsize::new(1).unwrap())
        .unwrap();
    let observed = recipient.observe_events(&page).unwrap();
    let pending = recipient.pending_inputs().cloned().collect::<Vec<_>>();
    assert_eq!(pending.len(), 1);
    let InboxPayload::FlowEvent {
        source: event,
        content,
        ..
    } = &pending[0].request.payload
    else {
        panic!("source event")
    };
    assert_eq!(event.flow, source.descriptor().address);
    assert_eq!(content["subscriptions"].as_array().unwrap().len(), 2);
    assert_eq!(content["message"]["origin"], "final_answer");
    assert_eq!(recipient.inbox_entries()[0].acceptance.receipt, observed);
    assert!(
        recipient
            .flow_subscriptions()
            .all(|sub| sub.cursor == page.through())
    );
    assert_eq!(recipient.observe_events(&page).unwrap(), observed);
    let mut changed = result.clone();
    changed.once = true;
    assert!(recipient.subscribe_events(changed, &anchor).is_err());
    assert!(
        recipient
            .subscribe_events(
                subscription(&source, SubscriptionPurpose::Watcher, false),
                &anchor
            )
            .is_err()
    );
    let item = pending[0].id;
    let recorded = session
        .inspect_flow(&recipient_info.address.instance_id)
        .unwrap();
    assert_eq!(
        recorded.watchers.len(),
        1,
        "internal result routes are not user watchers"
    );
    let watched = &recorded.watchers[0];
    assert_eq!(watched.registration.id(), watcher.id);
    assert_eq!(watched.progress.matched, 1);
    assert_eq!(watched.progress.pending, 1);
    assert_eq!(
        watched.progress.last_delivery.as_ref().unwrap().message_id,
        item
    );
    assert!(!recipient.state().can_start_turn());
    drop(recipient);
    let mut recipient = session
        .open_flow(&recipient_info.address.instance_id)
        .unwrap();
    assert_eq!(
        recipient
            .pending_inputs()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        [item]
    );
    assert_eq!(recipient.observe_events(&page).unwrap(), observed);
    transition(
        &mut recipient,
        FlowTransition::ExecutionResumed { key: pause },
    );
    let turn = TurnId::now();
    let reserved = recipient
        .reserve_invocation_input(turn.clone(), RunId::now(), |_| true)
        .unwrap()
        .unwrap();
    let lease = reserved.lease().clone();
    let projected = atman_runtime::flow_runtime::input::context_message(
        recipient.pending_inputs().next().unwrap(),
        &turn,
    )
    .unwrap();
    assert_ne!(projected.origin, MessageOrigin::User);
    recipient
        .commit_input(
            &reserved,
            InputContent::InvocationInput {
                command: None,
                arguments: Default::default(),
                invocation: Default::default(),
                messages: vec![projected],
            },
        )
        .unwrap();
    transition(
        &mut recipient,
        FlowTransition::TurnFinished {
            lease,
            outcome: TurnOutcome::Completed,
        },
    );
    answer(&mut source, "ready again", false);
    let page = source
        .events_after(page.through(), NonZeroUsize::new(5).unwrap())
        .unwrap();
    recipient.observe_events(&page).unwrap();
    let InboxPayload::FlowEvent { content, .. } =
        &recipient.pending_inputs().next().unwrap().request.payload
    else {
        panic!("event")
    };
    let recorded = session
        .inspect_flow(&recipient_info.address.instance_id)
        .unwrap();
    assert_eq!(recorded.watchers.len(), 1);
    let watched = &recorded.watchers[0];
    assert_eq!(watched.progress.matched, 1);
    assert_eq!(watched.progress.pending, 0);
    assert_eq!(watched.progress.input_committed, 1);
    assert_eq!(content["subscriptions"].as_array().unwrap().len(), 1);
    assert!(
        recipient
            .flow_subscriptions()
            .any(|record| record.subscription.id == watcher.id
                && record.status == SubscriptionStatus::Matched)
    );
    drop(recipient);
    let recipient = session
        .open_flow(&recipient_info.address.instance_id)
        .unwrap();
    assert_eq!(recipient.inbox_entries().len(), 2);
    assert!(matches!(
        recipient.inbox_entries()[0].acceptance.disposition,
        InboxDisposition::InputCommitted { .. }
    ));
    assert_eq!(recipient.pending_inputs().count(), 1);
}

#[test]
fn cancellation_close_and_source_pages_cannot_skip_or_reactivate_subscriptions() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            configuration: Default::default(),
            id,
            title: "Watch".into(),
            project: None,
        })
        .unwrap();
    let mut source = session.create_flow(descriptor(id)).unwrap();
    let mut recipient = session.create_flow(descriptor(id)).unwrap();
    transition(&mut recipient, FlowTransition::Activated);
    let anchor = source.observation_anchor();
    let sub = subscription(&source, SubscriptionPurpose::Watcher, false);
    recipient.subscribe_events(sub.clone(), &anchor).unwrap();
    transition(&mut source, FlowTransition::Activated);
    answer(&mut source, "one", false);
    let page = source
        .events_after(anchor.cursor().sequence, NonZeroUsize::new(1).unwrap())
        .unwrap();
    answer(&mut source, "two", false);
    let skipped = source
        .events_after(page.through(), NonZeroUsize::new(1).unwrap())
        .unwrap();
    let before = recipient.cursor().clone();
    assert!(recipient.observe_events(&skipped).is_err());
    assert_eq!(recipient.cursor(), &before);
    let cancelled = recipient.unsubscribe_events(sub.id).unwrap();
    assert_eq!(recipient.observe_events(&page).unwrap(), cancelled);
    assert_eq!(
        recipient.subscribe_events(sub.clone(), &anchor).unwrap(),
        cancelled
    );
    assert_eq!(
        recipient.flow_subscriptions().next().unwrap().status,
        SubscriptionStatus::Cancelled
    );
    let replacement = subscription(&source, SubscriptionPurpose::Watcher, false);
    recipient
        .subscribe_events(replacement, &source.observation_anchor())
        .unwrap();
    answer(&mut source, "three", false);
    let page = source
        .events_after(anchor.cursor().sequence, NonZeroUsize::new(10).unwrap())
        .unwrap();
    recipient.observe_events(&page).unwrap();
    assert_eq!(recipient.pending_inputs().count(), 1);
    transition(&mut recipient, FlowTransition::CloseRequested);
    assert!(recipient.observe_events(&page).is_err());
    assert_eq!(recipient.pending_inputs().count(), 0);
    assert!(
        recipient
            .flow_subscriptions()
            .all(|record| record.status == SubscriptionStatus::Cancelled)
    );
    let address = recipient.descriptor().address.clone();
    drop(recipient);
    let recipient = session.open_flow(&address.instance_id).unwrap();
    assert!(
        recipient
            .flow_subscriptions()
            .all(|record| record.status == SubscriptionStatus::Cancelled)
    );
    assert_eq!(recipient.inbox_entries().len(), 1);
}

#[test]
fn torn_observation_does_not_advance_cursor_without_its_input() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            configuration: Default::default(),
            id,
            title: "Recovery".into(),
            project: None,
        })
        .unwrap();
    let mut source = session.create_flow(descriptor(id)).unwrap();
    let mut recipient = session.create_flow(descriptor(id)).unwrap();
    transition(&mut recipient, FlowTransition::Activated);
    let anchor = source.observation_anchor();
    recipient
        .subscribe_events(
            subscription(&source, SubscriptionPurpose::Watcher, false),
            &anchor,
        )
        .unwrap();
    transition(&mut source, FlowTransition::Activated);
    answer(&mut source, "answer", false);
    let page = source
        .events_after(anchor.cursor().sequence, NonZeroUsize::new(8).unwrap())
        .unwrap();
    let path = recipient.dir().join("events.jsonl");
    let before = std::fs::read(&path).unwrap();
    recipient.observe_events(&page).unwrap();
    let complete = std::fs::read(&path).unwrap();
    let address = recipient.descriptor().address.clone();
    drop(recipient);
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .unwrap();
    file.write_all(&complete[..complete.len() - 7]).unwrap();
    file.sync_all().unwrap();
    drop(file);
    let mut recipient = session.open_flow(&address.instance_id).unwrap();
    assert!(recipient.recovered_tail.is_some());
    assert!(recipient.inbox_entries().is_empty());
    assert_eq!(
        recipient.flow_subscriptions().next().unwrap().cursor,
        anchor.cursor().sequence
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    recipient.observe_events(&page).unwrap();
    assert_eq!(recipient.pending_inputs().count(), 1);
}

#[test]
fn self_subscription_does_not_turn_its_own_cursor_commits_into_new_work() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            configuration: Default::default(),
            id,
            title: "Self watch".into(),
            project: None,
        })
        .unwrap();
    let mut flow = session.create_flow(descriptor(id)).unwrap();
    transition(&mut flow, FlowTransition::Activated);
    let anchor = flow.observation_anchor();
    flow.subscribe_events(
        subscription(&flow, SubscriptionPurpose::Watcher, false),
        &anchor,
    )
    .unwrap();
    answer(&mut flow, "one", false);
    let page = flow
        .events_after(anchor.cursor().sequence, NonZeroUsize::new(8).unwrap())
        .unwrap();
    let committed = flow.observe_events(&page).unwrap();
    let empty = flow
        .events_after(page.through(), NonZeroUsize::new(8).unwrap())
        .unwrap();
    assert_eq!(empty.through(), page.through());
    assert_eq!(flow.observe_events(&empty).unwrap(), committed);
    assert_eq!(flow.pending_inputs().count(), 1);
}

#[test]
fn reply_registration_and_subscription_cancellation_keep_one_durable_owner() {
    use atman_runtime::flow_store::{
        inbox::SenderIdentity,
        waiter::{ReplyStatus, ReplyWaiter},
    };
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            configuration: Default::default(),
            id,
            title: "Replies".into(),
            project: None,
        })
        .unwrap();
    let source = session.create_flow(descriptor(id)).unwrap();
    let mut recipient = session.create_flow(descriptor(id)).unwrap();
    transition(&mut recipient, FlowTransition::Activated);
    let lease = recipient
        .state()
        .clone()
        .begin_turn(TurnId::now(), RunId::now())
        .unwrap();
    transition(
        &mut recipient,
        FlowTransition::TurnStarted {
            turn: lease.turn.clone(),
            run: lease.run.clone(),
        },
    );
    let anchor = source.observation_anchor();
    let mut first = subscription(&source, SubscriptionPurpose::Watcher, true);
    first.filter = FlowEventFilter::TurnCompleted {
        turn: Some(TurnId::now()),
    };
    first.reply = Some(ReplyWaiter {
        correlation: Uuid::new_v4(),
        lease,
        sender: SenderIdentity::Flow(source.descriptor().address.clone()),
    });
    let receipt = recipient.subscribe_events(first.clone(), &anchor).unwrap();
    assert_eq!(recipient.reply_waiters()[0].registered, receipt);
    let mut second = first.clone();
    second.id = Uuid::new_v4();
    recipient.subscribe_events(second.clone(), &anchor).unwrap();
    let mut conflict = first.clone();
    conflict.id = Uuid::new_v4();
    conflict.reply.as_mut().unwrap().correlation = Uuid::new_v4();
    let before = recipient.cursor().clone();
    assert!(recipient.subscribe_events(conflict, &anchor).is_err());
    assert_eq!(recipient.cursor(), &before);
    assert_eq!(recipient.reply_waiters().len(), 1);
    recipient.unsubscribe_events(first.id).unwrap();
    assert_eq!(recipient.reply_waiters()[0].status, ReplyStatus::Pending);
    let cancelled = recipient.unsubscribe_events(second.id).unwrap();
    assert_eq!(
        recipient.reply_waiters()[0].status,
        ReplyStatus::Cancelled {
            by: cancelled.clone()
        }
    );
    assert_eq!(
        recipient.subscribe_events(second, &anchor).unwrap(),
        cancelled
    );
    assert!(
        recipient
            .flow_subscriptions()
            .all(|record| record.status == SubscriptionStatus::Cancelled)
    );

    // A torn compound registration restores neither the subscription nor a ghost waiter.
    let mut torn = first;
    torn.id = Uuid::new_v4();
    torn.reply.as_mut().unwrap().correlation = Uuid::new_v4();
    let path = recipient.dir().join("events.jsonl");
    recipient.subscribe_events(torn.clone(), &anchor).unwrap();
    let complete = std::fs::read(&path).unwrap();
    let address = recipient.descriptor().address.clone();
    drop(recipient);
    std::fs::write(&path, &complete[..complete.len() - 7]).unwrap();
    let recipient = session.open_flow(&address.instance_id).unwrap();
    assert!(recipient.recovered_tail.is_some());
    assert_eq!(recipient.reply_waiters().len(), 1);
    assert_eq!(
        recipient.reply_waiters()[0].status,
        ReplyStatus::Cancelled { by: cancelled }
    );
    assert!(
        recipient
            .flow_subscriptions()
            .all(|record| record.subscription.id != torn.id)
    );
}
