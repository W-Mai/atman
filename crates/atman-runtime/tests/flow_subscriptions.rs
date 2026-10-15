use std::{io::Write, num::NonZeroUsize};

use atman_rt::{RunId, TurnId, TurnOutcome};
use atman_runtime::flow_store::inbox::{InboxDisposition, InboxPayload, InputContent};
use atman_runtime::flow_store::publication::MessagePublication;
use atman_runtime::flow_store::subscription::{
    AnswerFilter, AnswerSubscription, SubscriptionPurpose, SubscriptionStatus,
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
        entry: EntrySnapshot {
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
        },
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

fn subscription(
    source: &FlowStore,
    purpose: SubscriptionPurpose,
    once: bool,
) -> AnswerSubscription {
    AnswerSubscription {
        id: Uuid::new_v4(),
        source: source.descriptor().address.clone(),
        filter: AnswerFilter::All,
        once,
        purpose,
    }
}

#[test]
fn source_answer_and_watcher_commit_one_inbox_item_with_all_reasons_and_recover_once() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let session_id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            id: session_id,
            title: "Subscriptions".into(),
            project_root: None,
        })
        .unwrap();
    let mut source = session.create_flow(descriptor(session_id)).unwrap();
    let recipient_info = descriptor(session_id);
    let mut recipient = session.create_flow(recipient_info.clone()).unwrap();
    transition(&mut recipient, FlowTransition::Activated);
    let anchor = source.publication_anchor();
    let result = subscription(&source, SubscriptionPurpose::SpawnResult, false);
    let mut watcher = subscription(&source, SubscriptionPurpose::Watcher, true);
    watcher.filter = AnswerFilter::Contains("ready".into());
    for sub in [&result, &watcher] {
        recipient.subscribe_answers(sub.clone(), &anchor).unwrap();
    }
    let registered = recipient.cursor().clone();
    assert_eq!(
        recipient
            .subscribe_answers(result.clone(), &anchor)
            .unwrap(),
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
    let page = source
        .publications_after(anchor.cursor().sequence, NonZeroUsize::new(1).unwrap())
        .unwrap();
    let observed = recipient.observe_answers(&page).unwrap();
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
            .answer_subscriptions()
            .all(|sub| sub.cursor == page.through())
    );
    assert_eq!(recipient.observe_answers(&page).unwrap(), observed);
    let mut changed = result.clone();
    changed.once = true;
    assert!(recipient.subscribe_answers(changed, &anchor).is_err());
    assert!(
        recipient
            .subscribe_answers(
                subscription(&source, SubscriptionPurpose::Watcher, false),
                &anchor
            )
            .is_err()
    );
    let item = pending[0].id;
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
    assert_eq!(recipient.observe_answers(&page).unwrap(), observed);
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
        .publications_after(page.through(), NonZeroUsize::new(5).unwrap())
        .unwrap();
    recipient.observe_answers(&page).unwrap();
    let InboxPayload::FlowEvent { content, .. } =
        &recipient.pending_inputs().next().unwrap().request.payload
    else {
        panic!("event")
    };
    assert_eq!(content["subscriptions"].as_array().unwrap().len(), 1);
    assert!(
        recipient
            .answer_subscriptions()
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
            id,
            title: "Watch".into(),
            project_root: None,
        })
        .unwrap();
    let mut source = session.create_flow(descriptor(id)).unwrap();
    let mut recipient = session.create_flow(descriptor(id)).unwrap();
    transition(&mut recipient, FlowTransition::Activated);
    let anchor = source.publication_anchor();
    let sub = subscription(&source, SubscriptionPurpose::Watcher, false);
    recipient.subscribe_answers(sub.clone(), &anchor).unwrap();
    transition(&mut source, FlowTransition::Activated);
    answer(&mut source, "one", false);
    let page = source
        .publications_after(anchor.cursor().sequence, NonZeroUsize::new(1).unwrap())
        .unwrap();
    answer(&mut source, "two", false);
    let skipped = source
        .publications_after(page.through(), NonZeroUsize::new(1).unwrap())
        .unwrap();
    let before = recipient.cursor().clone();
    assert!(recipient.observe_answers(&skipped).is_err());
    assert_eq!(recipient.cursor(), &before);
    let cancelled = recipient.unsubscribe_answers(sub.id).unwrap();
    assert_eq!(recipient.observe_answers(&page).unwrap(), cancelled);
    assert_eq!(
        recipient.subscribe_answers(sub.clone(), &anchor).unwrap(),
        cancelled
    );
    assert_eq!(
        recipient.answer_subscriptions().next().unwrap().status,
        SubscriptionStatus::Cancelled
    );
    let replacement = subscription(&source, SubscriptionPurpose::Watcher, false);
    recipient
        .subscribe_answers(replacement, &source.publication_anchor())
        .unwrap();
    answer(&mut source, "three", false);
    let page = source
        .publications_after(anchor.cursor().sequence, NonZeroUsize::new(10).unwrap())
        .unwrap();
    recipient.observe_answers(&page).unwrap();
    assert_eq!(recipient.pending_inputs().count(), 1);
    transition(&mut recipient, FlowTransition::CloseRequested);
    assert!(recipient.observe_answers(&page).is_err());
    assert_eq!(recipient.pending_inputs().count(), 0);
    assert!(
        recipient
            .answer_subscriptions()
            .all(|record| record.status == SubscriptionStatus::Cancelled)
    );
    let address = recipient.descriptor().address.clone();
    drop(recipient);
    let recipient = session.open_flow(&address.instance_id).unwrap();
    assert!(
        recipient
            .answer_subscriptions()
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
            id,
            title: "Recovery".into(),
            project_root: None,
        })
        .unwrap();
    let mut source = session.create_flow(descriptor(id)).unwrap();
    let mut recipient = session.create_flow(descriptor(id)).unwrap();
    transition(&mut recipient, FlowTransition::Activated);
    let anchor = source.publication_anchor();
    recipient
        .subscribe_answers(
            subscription(&source, SubscriptionPurpose::Watcher, false),
            &anchor,
        )
        .unwrap();
    transition(&mut source, FlowTransition::Activated);
    answer(&mut source, "answer", false);
    let page = source
        .publications_after(anchor.cursor().sequence, NonZeroUsize::new(8).unwrap())
        .unwrap();
    let path = recipient.dir().join("events.jsonl");
    let before = std::fs::read(&path).unwrap();
    recipient.observe_answers(&page).unwrap();
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
        recipient.answer_subscriptions().next().unwrap().cursor,
        anchor.cursor().sequence
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    recipient.observe_answers(&page).unwrap();
    assert_eq!(recipient.pending_inputs().count(), 1);
}

#[test]
fn self_subscription_does_not_turn_its_own_cursor_commits_into_new_work() {
    let dir = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(dir.path()).unwrap();
    let id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            id,
            title: "Self watch".into(),
            project_root: None,
        })
        .unwrap();
    let mut flow = session.create_flow(descriptor(id)).unwrap();
    transition(&mut flow, FlowTransition::Activated);
    let anchor = flow.publication_anchor();
    flow.subscribe_answers(
        subscription(&flow, SubscriptionPurpose::Watcher, false),
        &anchor,
    )
    .unwrap();
    answer(&mut flow, "one", false);
    let page = flow
        .publications_after(anchor.cursor().sequence, NonZeroUsize::new(8).unwrap())
        .unwrap();
    let committed = flow.observe_answers(&page).unwrap();
    let empty = flow
        .publications_after(page.through(), NonZeroUsize::new(8).unwrap())
        .unwrap();
    assert_eq!(empty.through(), page.through());
    assert_eq!(flow.observe_answers(&empty).unwrap(), committed);
    assert_eq!(flow.pending_inputs().count(), 1);
}
