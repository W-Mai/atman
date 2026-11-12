use super::*;
use crate::flow_store::{
    branches::HistoryHead, completion::InvocationResult, continuation::goal_request,
    source_capture::tests::fixture,
};
use crate::memory::goal::{GoalActor, GoalPauseReason, GoalStatus};
use atman_rt::TurnOutcome;

fn edit(store: &FlowStore, edit: GoalEdit, start_turn: bool) -> GoalRequest {
    GoalRequest {
        id: Uuid::new_v4(),
        sender: SenderIdentity::User(Uuid::new_v4()),
        expected: store.goal().map(GoalVersion::from),
        expected_head: None,
        edit,
        start_turn,
    }
}

fn select_head(store: &mut FlowStore) -> HistoryHead {
    let mut value = serde_json::to_value(store.history_head()).unwrap();
    value["version"] = (store.history_head().version.get() + 1).into();
    let head: HistoryHead = serde_json::from_value(value).unwrap();
    store.inbox.select_head(head.clone());
    head
}

fn create(store: &mut FlowStore) {
    store
        .edit_goal(
            edit(
                store,
                GoalEdit::Create {
                    text: "finish the task".into(),
                },
                false,
            ),
            |_| Ok(()),
        )
        .unwrap();
}

#[test]
fn goal_resume_admits_only_the_observed_head_and_preserves_accepted_retry_identity() {
    let (_directory, mut store, lease) = fixture();
    store
        .finish_turn(lease, InvocationResult::Unobserved(TurnOutcome::Completed))
        .unwrap();
    create(&mut store);
    store
        .edit_goal(
            edit(
                &store,
                GoalEdit::Pause {
                    actor: GoalActor::User,
                    reason: GoalPauseReason::UserRequested,
                    summary: String::new(),
                },
                false,
            ),
            |_| Ok(()),
        )
        .unwrap();
    let old = store.history_head();
    let head = select_head(&mut store);
    let mut resume = edit(
        &store,
        GoalEdit::Resume {
            actor: GoalActor::User,
        },
        true,
    );
    let cursor = store.cursor().clone();
    assert!(store.edit_goal(resume.clone(), |_| Ok(())).is_err());
    resume.expected_head = Some(old);
    assert!(store.edit_goal(resume.clone(), |_| Ok(())).is_err());
    assert_eq!(store.cursor(), &cursor);
    assert_eq!(store.goal().unwrap().status, GoalStatus::Paused);
    resume.expected_head = Some(head.clone());
    let accepted = store.edit_goal(resume.clone(), |_| Ok(())).unwrap();
    let input = accepted.update.input.as_ref().unwrap();
    assert_eq!(input.request.expected_head, Some(head.clone()));
    assert_eq!(
        store.inbox_entry(input.id).unwrap().unwrap().history_head,
        head
    );
    select_head(&mut store);
    assert!(store.pending_inputs_at_head().next().is_none());
    assert_eq!(
        store
            .edit_goal(resume.clone(), |_| panic!(
                "an accepted retry is not a new goal edit"
            ))
            .unwrap(),
        accepted
    );
    resume.expected_head = Some(store.history_head());
    assert!(store.edit_goal(resume, |_| Ok(())).is_err());
}

#[test]
fn goal_continuation_retries_keep_original_optional_heads_and_payload_fingerprints() {
    for legacy in [false, true] {
        let (_directory, mut store, lease) = fixture();
        create(&mut store);
        let goal = GoalVersion::from(store.goal().unwrap());
        let text = "continue the task".to_owned();
        let accepted = if legacy {
            store
                .accept_input(
                    goal_request(lease.clone(), goal.clone(), text.clone()).unwrap(),
                    |_| Ok(()),
                )
                .unwrap()
        } else {
            store
                .schedule_goal(lease.clone(), goal.clone(), text.clone(), |request| {
                    assert!(request.expected_head.is_some());
                    Ok(())
                })
                .unwrap()
                .unwrap()
        };
        let input = store.inbox_entry(accepted.message_id).unwrap().unwrap();
        assert_eq!(input.envelope.request.expected_head.is_none(), legacy);
        store
            .finish_turn(
                lease.clone(),
                InvocationResult::Unobserved(TurnOutcome::Completed),
            )
            .unwrap();
        select_head(&mut store);
        assert_eq!(
            store
                .schedule_goal(lease.clone(), goal.clone(), text, |_| panic!(
                    "accepted continuation must retain its receipt"
                ))
                .unwrap(),
            Some(accepted)
        );
        assert!(
            store
                .schedule_goal(lease, goal, "changed text".into(), |_| Ok(()))
                .is_err()
        );
        assert!(store.pending_inputs_at_head().next().is_none());
    }
}

#[test]
fn goal_continuation_payload_changes_get_a_new_delivery_identity() {
    let (_directory, _store, lease) = fixture();
    let goal = GoalVersion {
        id: "goal".into(),
        revision: 1,
    };
    let first = goal_request(lease.clone(), goal.clone(), "first payload".into()).unwrap();
    let second = goal_request(lease, goal, "second payload".into()).unwrap();
    assert_ne!(first.request_id, second.request_id);
}

#[test]
fn absent_goal_heads_preserve_wire_shape_and_resume_envelopes_validate_exactly() {
    let (_directory, mut store, _lease) = fixture();
    create(&mut store);
    store
        .edit_goal(
            edit(
                &store,
                GoalEdit::Pause {
                    actor: GoalActor::User,
                    reason: GoalPauseReason::UserRequested,
                    summary: String::new(),
                },
                false,
            ),
            |_| Ok(()),
        )
        .unwrap();
    let request = edit(
        &store,
        GoalEdit::Resume {
            actor: GoalActor::User,
        },
        true,
    );
    let wire = serde_json::to_value(&request).unwrap();
    assert!(wire.get("expected_head").is_none());
    assert_eq!(
        serde_json::from_value::<GoalRequest>(wire).unwrap(),
        request
    );
    let mut update = GoalUpdate {
        request,
        new_goal_id: Uuid::new_v4(),
        at: chrono::Utc::now(),
        input: None,
    };
    let goal = store.goals.prepare_record(&store.state, &update).unwrap();
    update.input = update.resume_input(&store.state, goal.as_ref()).unwrap();
    assert!(
        serde_json::to_value(&update.input).unwrap()["request"]
            .get("expected_head")
            .is_none()
    );
    assert!(store.goals.prepare(&store.state, &update).is_ok());
    update.input.as_mut().unwrap().request.expected_head = Some(store.history_head());
    assert!(store.goals.prepare(&store.state, &update).is_err());
}
