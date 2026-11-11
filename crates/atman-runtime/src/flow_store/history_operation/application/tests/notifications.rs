use super::*;
use crate::event_log::journal::Journal;
use crate::flow_store::{
    inbox::InboxPayload,
    subscription::{
        FlowEventFilter, FlowSubscription, SubscriptionPurpose, SubscriptionRecord,
        SubscriptionStatus,
    },
    view::FlowUpdate,
};
use crate::message::MessageOrigin;

#[test]
fn flow_notifications_preserve_complete_origin_groups_expiry_and_replay() {
    for legacy in [false, true] {
        let (directory, mut flow, operation) = unchanged_branch();
        let (_source_directory, mut source, lease) = fixture();
        let anchor = source.observation_anchor();
        let expiry = Utc::now() + chrono::Duration::hours(1);
        let watch = |once| FlowSubscription {
            id: Uuid::new_v4(),
            source: source.state().address().clone(),
            filter: FlowEventFilter::FinalAnswers,
            once,
            purpose: SubscriptionPurpose::Watcher,
            expires_at: Some(expiry),
            reply: None,
        };
        let mut watches = Vec::new();
        for once in [false, true] {
            let watch = watch(once);
            if legacy {
                let record = SubscriptionRecord {
                    history_head: None,
                    subscription: watch.clone(),
                    registered_at: Utc::now(),
                    started_after: anchor.cursor().clone(),
                    cursor: anchor.cursor().sequence(),
                    status: SubscriptionStatus::Active,
                };
                flow.cursor = flow
                    .journal
                    .commit(&[FlowFact::FlowSubscribed(Box::new(record.clone()))])
                    .unwrap();
                flow.subscriptions.register(record, &flow.inbox).unwrap();
            } else {
                flow.subscribe_events(watch.clone(), &anchor).unwrap();
            }
            watches.push(watch);
        }
        let HistoryOperationTarget::Revert { cutoff, .. } = operation.target.clone() else {
            unreachable!()
        };
        let reviewed = request(&flow, *cutoff);
        apply(&mut flow, &reviewed);
        let head = flow.history_head();
        for once in [false, true] {
            let watch = watch(once);
            flow.subscribe_events(watch.clone(), &anchor).unwrap();
            watches.push(watch);
        }
        for text in ["first answer", "second answer"] {
            let mut message = Message::assistant_text(lease.turn.clone(), text);
            message.origin = MessageOrigin::FinalAnswer;
            publish(&mut source, &lease, message);
        }
        let events = source
            .events_after(
                anchor.cursor().sequence(),
                std::num::NonZeroUsize::new(8).unwrap(),
            )
            .unwrap();
        let path = flow.dir().unwrap().join("events.jsonl");
        let registered = std::fs::read(&path).unwrap();
        let before = flow.cursor().clone();
        let receipt = flow.observe_events(&events).unwrap();
        assert_eq!(flow.observe_events(&events).unwrap(), receipt);
        assert_eq!(flow.pending_inputs().count(), 4);
        assert_eq!(flow.pending_inputs_at_head().count(), 2);
        let page = flow
            .read_updates(Some(&before), std::num::NonZeroUsize::new(1).unwrap())
            .unwrap();
        let FlowUpdate::EventsObserved(observation) = &page.batches[0].facts[0] else {
            unreachable!()
        };
        let observation = observation.clone();
        assert_eq!(observation.envelopes.len(), 4);
        for (index, envelope) in observation.envelopes.iter().enumerate() {
            let entry = flow.inbox_entry(envelope.id).unwrap().unwrap();
            assert_eq!(&entry.envelope, envelope);
            assert_eq!(
                entry.history_head,
                if index % 2 == 0 {
                    operation.expected_head.clone()
                } else {
                    head.clone()
                }
            );
            let InboxPayload::FlowEvent { content, .. } = &envelope.request.payload else {
                unreachable!()
            };
            assert_eq!(
                content["subscriptions"].as_array().unwrap().len(),
                if index < 2 { 2 } else { 1 }
            );
        }
        let inspection =
            RuntimeStore::inspect_session(directory.path(), operation.flow.session_id.0)
                .unwrap()
                .inspect_flow(&operation.flow.instance_id)
                .unwrap();
        for (index, watch) in watches.iter().enumerate() {
            let progress = &inspection
                .watchers
                .iter()
                .find(|row| row.registration.id() == watch.id)
                .unwrap()
                .progress;
            assert_eq!(progress.matched, if watch.once { 1 } else { 2 });
            let expected = usize::from(index >= 2) + if watch.once { 0 } else { 2 };
            assert_eq!(
                progress.last_delivery.as_ref().unwrap().message_id,
                observation.envelopes[expected].id
            );
        }
        let expired = flow.expire_flow_watches(expiry).unwrap();
        assert_eq!(flow.expire_flow_watches(expiry).unwrap(), expired);
        assert_eq!(flow.pending_inputs().count(), 6);
        assert_eq!(flow.pending_inputs_at_head().count(), 3);
        assert!(flow.flow_subscriptions().all(|record| record.status
            == if record.subscription.once {
                SubscriptionStatus::Matched
            } else {
                SubscriptionStatus::Expired
            }));
        drop(flow);
        let runtime = RuntimeStore::open(directory.path()).unwrap();
        let session = runtime.open_session(operation.flow.session_id.0).unwrap();
        let mut flow = session.open_flow(&operation.flow.instance_id).unwrap();
        assert_eq!(flow.pending_inputs_at_head().count(), 3);
        assert_eq!(flow.observe_events(&events).unwrap(), expired);
        flow.subscribe_events(watches[0].clone(), &anchor).unwrap();
        assert_eq!(
            flow.flow_subscriptions()
                .find(|record| record.subscription.id == watches[0].id)
                .unwrap()
                .history_head,
            if legacy {
                None
            } else {
                Some(operation.expected_head.clone())
            }
        );
        drop(flow);
        for mutation in 0..6 {
            std::fs::write(&path, &registered).unwrap();
            let mut invalid = observation.clone();
            match mutation {
                0 => {
                    invalid.envelopes.remove(1);
                }
                1 => {
                    invalid.envelopes[1].id = invalid.envelopes[0].id;
                }
                2 => {
                    invalid.envelopes[0].request.expected_head = Some(head.clone());
                }
                3 => {
                    invalid.envelopes.swap(0, 1);
                }
                4 => {
                    invalid.envelopes.insert(1, invalid.envelopes[0].clone());
                }
                _ => {
                    let InboxPayload::FlowEvent { content, .. } =
                        &mut invalid.envelopes[1].request.payload
                    else {
                        unreachable!()
                    };
                    content["subscriptions"] = serde_json::json!([]);
                }
            }
            let (mut journal, _) = Journal::open::<serde_json::Value>(
                &path,
                operation.flow.instance_id.0.0,
                |_, _| Ok(()),
            )
            .unwrap();
            journal
                .commit(&[serde_json::to_value(FlowFact::EventsObserved(invalid)).unwrap()])
                .unwrap();
            drop(journal);
            let bytes = std::fs::read(&path).unwrap();
            assert!(session.inspect_flow(&operation.flow.instance_id).is_err());
            assert!(session.open_flow(&operation.flow.instance_id).is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }
}
