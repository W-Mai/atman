use super::*;
use crate::{
    flow_runtime::FlowWorker,
    flow_store::{
        FlowStore, RuntimeStore,
        goal::GoalRequest,
        history_operation::tests::active_fixture,
        inbox::{DeliveryMode, DeliveryRequest, InboxPayload},
        source_capture::tests::fixture,
    },
    memory::goal::{GoalActor, GoalEdit, GoalPauseReason},
};

fn input(store: &FlowStore, operator: LocalOperator) -> DeliveryRequest {
    DeliveryRequest {
        expected_head: Some(store.history_head()),
        request_id: Uuid::new_v4(),
        sender: operator.sender(),
        recipient: store.state().address().clone(),
        delivery: DeliveryMode::NextTurn,
        cause: None,
        correlation: None,
        scope: None,
        payload: InboxPayload::Interjection {
            text: "original input".into(),
        },
    }
}

fn goal(operator: LocalOperator) -> GoalRequest {
    GoalRequest {
        id: Uuid::new_v4(),
        sender: operator.sender(),
        expected: None,
        expected_head: None,
        edit: GoalEdit::Create {
            text: "original objective".into(),
        },
        start_turn: false,
    }
}

#[tokio::test]
async fn local_operator_input_is_atomic_across_native_and_rpc_namespaces() {
    let (directory, flow, _) = fixture();
    let original = input(&flow, LocalOperator::NATIVE);
    let mut alternate = original.clone();
    alternate.sender = LocalOperator::LEGACY_RPC.sender();
    let worker = FlowWorker::start(flow).await.unwrap();
    let (native, rpc) = tokio::join!(
        worker.accept_operator_input(LocalOperator::NATIVE, original.clone(), |_| Ok(())),
        worker.accept_operator_input(LocalOperator::LEGACY_RPC, alternate.clone(), |_| Ok(())),
    );
    let accepted = native.unwrap();
    assert_eq!(accepted, rpc.unwrap());
    let before = worker.snapshot().await.unwrap().cursor;
    alternate.payload = InboxPayload::Interjection {
        text: "different input".into(),
    };
    assert!(
        worker
            .accept_operator_input(LocalOperator::LEGACY_RPC, alternate, |_| Ok(()))
            .await
            .is_err()
    );
    assert_eq!(worker.snapshot().await.unwrap().cursor, before);
    for operator in [LocalOperator::NATIVE, LocalOperator::LEGACY_RPC] {
        let observed = worker
            .observe_operator_input(operator, original.request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(observed.message_id, accepted.message_id);
        assert_eq!(observed.accepted, accepted.receipt);
    }
    let flow = worker.shutdown().await.unwrap();
    let address = flow.state().address().clone();
    drop(flow);
    let read = RuntimeStore::inspect_session(directory.path(), address.session_id.0).unwrap();
    assert_eq!(
        read.observe_operator_input(
            &address.instance_id,
            LocalOperator::LEGACY_RPC,
            original.request_id,
        )
        .unwrap()
        .unwrap()
        .accepted,
        accepted.receipt,
    );
}

#[tokio::test]
async fn local_operator_goal_retries_keep_the_original_request_and_receipt() {
    let (_directory, flow, _) = fixture();
    let original = goal(LocalOperator::NATIVE);
    let mut alternate = original.clone();
    alternate.sender = LocalOperator::LEGACY_RPC.sender();
    let worker = FlowWorker::start(flow).await.unwrap();
    let (native, rpc) = tokio::join!(
        worker.edit_operator_goal(LocalOperator::NATIVE, original.clone(), |_| Ok(())),
        worker.edit_operator_goal(LocalOperator::LEGACY_RPC, alternate.clone(), |_| Ok(())),
    );
    let committed = native.unwrap();
    assert_eq!(committed, rpc.unwrap());
    let before = worker.snapshot().await.unwrap().cursor;
    alternate.start_turn = true;
    assert!(
        worker
            .edit_operator_goal(LocalOperator::LEGACY_RPC, alternate, |_| Ok(()))
            .await
            .is_err()
    );
    assert_eq!(worker.snapshot().await.unwrap().cursor, before);
    assert_eq!(
        worker
            .operator_goal_update(LocalOperator::LEGACY_RPC, original.id)
            .await
            .unwrap(),
        Some(committed),
    );
    assert!(
        worker
            .operator_goal_update(LocalOperator::new(Uuid::new_v4()), original.id)
            .await
            .unwrap()
            .is_none()
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn local_operator_history_keeps_the_original_operation_and_cancellation() {
    let (directory, flow, mut original) = active_fixture();
    original.sender = LocalOperator::NATIVE.sender();
    let mut alternate = original.clone();
    alternate.sender = LocalOperator::LEGACY_RPC.sender();
    let worker = FlowWorker::start(flow).await.unwrap();
    let (native, rpc) = tokio::join!(
        worker.begin_operator_history_operation(
            LocalOperator::NATIVE,
            original.clone(),
            |_| Ok(())
        ),
        worker.begin_operator_history_operation(
            LocalOperator::LEGACY_RPC,
            alternate.clone(),
            |_| Ok(())
        ),
    );
    let accepted = native.unwrap();
    assert_eq!(accepted, rpc.unwrap());
    let before = worker.snapshot().await.unwrap().cursor;
    alternate.execution_revision += 1;
    assert!(
        worker
            .begin_operator_history_operation(LocalOperator::LEGACY_RPC, alternate, |_| Ok(()))
            .await
            .is_err()
    );
    assert_eq!(worker.snapshot().await.unwrap().cursor, before);
    let stopping = worker
        .stop_operator_history_operation(LocalOperator::LEGACY_RPC, original.id, |_| Ok(()))
        .await
        .unwrap();
    assert_eq!(
        worker
            .stop_operator_history_operation(LocalOperator::NATIVE, original.id, |_| Ok(()))
            .await
            .unwrap(),
        stopping,
    );
    assert_eq!(stopping.request, accepted.request);
    assert_eq!(stopping.accepted, accepted.accepted);
    assert_eq!(
        worker
            .pending_operator_history_operation(LocalOperator::LEGACY_RPC, |_| Ok(()))
            .await
            .unwrap(),
        Some(stopping.clone()),
    );
    let flow = worker.shutdown().await.unwrap();
    let address = flow.state().address().clone();
    drop(flow);
    let read = RuntimeStore::inspect_session(directory.path(), address.session_id.0).unwrap();
    let recorded = read
        .operator_history_operation(&address.instance_id, LocalOperator::LEGACY_RPC, original.id)
        .unwrap()
        .unwrap();
    assert_eq!(recorded.request, stopping.request);
    assert_eq!(recorded.accepted, stopping.accepted);
    assert_eq!(recorded.cancellation, stopping.cancellation);
}

#[test]
fn local_operator_ambiguous_origins_fail_without_mutation() {
    let (_directory, mut flow, _) = fixture();
    let original = input(&flow, LocalOperator::NATIVE);
    let mut alternate = original.clone();
    alternate.sender = LocalOperator::LEGACY_RPC.sender();
    flow.accept_input(original.clone(), |_| Ok(())).unwrap();
    flow.accept_input(alternate, |_| Ok(())).unwrap();
    let before = flow.cursor().clone();
    assert!(
        flow.observe_operator_input(LocalOperator::NATIVE, original.request_id)
            .is_err()
    );
    assert!(
        flow.accept_operator_input(LocalOperator::NATIVE, original, |_| Ok(()))
            .is_err()
    );
    assert_eq!(flow.cursor(), &before);

    let original = goal(LocalOperator::NATIVE);
    flow.edit_goal(original.clone(), |_| Ok(())).unwrap();
    let mut alternate = original.clone();
    alternate.sender = LocalOperator::LEGACY_RPC.sender();
    alternate.expected = flow.goal().map(Into::into);
    alternate.edit = GoalEdit::Pause {
        actor: GoalActor::User,
        reason: GoalPauseReason::UserRequested,
        summary: String::new(),
    };
    flow.edit_goal(alternate, |_| Ok(())).unwrap();
    let before = flow.cursor().clone();
    assert!(
        flow.operator_goal_update(LocalOperator::NATIVE, original.id)
            .is_err()
    );
    assert!(
        flow.edit_operator_goal(LocalOperator::NATIVE, original, |_| Ok(()))
            .is_err()
    );
    assert_eq!(flow.cursor(), &before);

    let (_directory, mut flow, mut original) = active_fixture();
    original.sender = LocalOperator::NATIVE.sender();
    flow.begin_history_operation(original.clone(), |_| Ok(()))
        .unwrap();
    flow.abort_history_operation(original.sender.clone(), original.id, |_| Ok(()))
        .unwrap();
    let mut alternate = original.clone();
    alternate.sender = LocalOperator::LEGACY_RPC.sender();
    flow.begin_history_operation(alternate, |_| Ok(())).unwrap();
    let before = flow.cursor().clone();
    assert!(
        flow.operator_history_operation(LocalOperator::NATIVE, original.id)
            .is_err()
    );
    assert!(
        flow.pending_operator_history_operation(LocalOperator::NATIVE)
            .is_err()
    );
    assert!(
        flow.begin_operator_history_operation(LocalOperator::NATIVE, original, |_| Ok(()))
            .is_err()
    );
    assert_eq!(flow.cursor(), &before);
}

#[test]
fn local_operator_bindings_do_not_alias_other_users_or_nonuser_principals() {
    let (_directory, mut flow, _) = fixture();
    let mut request = input(&flow, LocalOperator::NATIVE);
    let original = flow
        .accept_operator_input(LocalOperator::NATIVE, request.clone(), |_| Ok(()))
        .unwrap();
    let custom = LocalOperator::new(Uuid::new_v4());
    assert!(
        flow.observe_operator_input(custom, request.request_id)
            .unwrap()
            .is_none()
    );
    let before = flow.cursor().clone();
    for sender in [
        custom.sender(),
        SenderIdentity::Flow(request.recipient.clone()),
        SenderIdentity::Host(LocalOperator::NATIVE.id()),
    ] {
        request.sender = sender;
        assert!(
            flow.accept_operator_input(LocalOperator::NATIVE, request.clone(), |_| Ok(()))
                .is_err()
        );
    }
    assert_eq!(flow.cursor(), &before);
    request.sender = custom.sender();
    let other = flow
        .accept_operator_input(custom, request, |_| Ok(()))
        .unwrap();
    assert_ne!(original.message_id, other.message_id);
}
