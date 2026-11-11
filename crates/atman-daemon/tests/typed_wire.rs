use std::sync::Arc;

use atman_daemon::{DaemonState, dispatch, run::RunLauncher};
use atman_proto::{JsonRpcRequest, flows::FlowCursor, methods, responses::*};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use uuid::Uuid;

async fn rpc(state: &Arc<DaemonState>, method: &str, params: Value) -> Value {
    let response = dispatch(state.clone(), JsonRpcRequest::new(1, method, params)).await;
    assert!(response.error.is_none(), "{method}: {:?}", response.error);
    response.result.unwrap()
}

fn typed<T: DeserializeOwned + Serialize>(value: Value) -> T {
    let result: T = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        serde_json::to_value(&result).unwrap(),
        value,
        "DTO must preserve every published field"
    );
    result
}

#[tokio::test]
async fn durable_owner_responses_keep_existing_wire_and_acceptance_identity() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(config.join("config.toml"), "[trust]\nmode=\"reckless\"\n").unwrap();
    let state = Arc::new(DaemonState::new(directory.path().join("data")));
    state.set_launcher(Arc::new(
        RunLauncher::new(directory.path().into(), Some(config), None).unwrap(),
    ));
    let ping = rpc(&state, methods::PING, json!({"ignored_legacy_field": true})).await;
    assert_eq!(
        ping,
        json!({"pong": true, "version": env!("CARGO_PKG_VERSION")})
    );
    typed::<PingResponse>(ping);
    let id = Uuid::now_v7();
    let created: SessionView = typed(
        rpc(
            &state,
            methods::SESSION_CREATE,
            json!({"session_id": id, "title": "Wire contract", "project_root": directory.path()}),
        )
        .await,
    );
    assert_eq!(created.session_id.0, id);
    assert_eq!(created.flow_count, 0);
    assert_eq!(created.default_flow, None);
    let reopened: SessionView =
        typed(rpc(&state, methods::SESSION_OPEN, json!({"session_id": id})).await);
    assert_eq!(reopened, created);
    let source = directory.path().join("wire.at");
    std::fs::write(
        &source,
        "flow main(text: string) -> string { return final.answer(message: text) }",
    )
    .unwrap();
    let flow = json!({"session_id": id, "instance_id": Uuid::now_v7()});
    let launch = json!({"flow": flow, "request_id": Uuid::now_v7(), "expected_catalog_sequence": created.catalog.sequence, "path": source, "entry": "main", "start": "waiting"});
    let launched: LaunchFlowResponse =
        typed(rpc(&state, methods::FLOW_LAUNCH, launch.clone()).await);
    assert!(launched.acceptance.is_none());
    assert_eq!(
        typed::<LaunchFlowResponse>(rpc(&state, methods::FLOW_LAUNCH, launch).await),
        launched
    );
    let selected: SessionView =
        typed(rpc(&state, methods::SESSION_SELECT_FLOW, json!({"flow": flow})).await);
    assert_eq!(selected.default_flow, Some(launched.flow.instance_id));
    // Launch already installs scheduling. Only a newly attached host needs bind.
    let duplicate_bind = dispatch(
        state.clone(),
        JsonRpcRequest::new(2, methods::FLOW_BIND, json!({"flow": flow})),
    )
    .await
    .error
    .unwrap();
    assert_eq!(
        duplicate_bind.code,
        atman_proto::JsonRpcError::APPLICATION_ERROR
    );
    assert_eq!(
        duplicate_bind.message,
        "instance already has a host scheduler"
    );
    state.shutdown().await.unwrap();
    let state = Arc::new(DaemonState::new(directory.path().join("data")));
    typed::<SessionView>(rpc(&state, methods::SESSION_OPEN, json!({"session_id": id})).await);
    typed::<BindFlowResponse>(rpc(&state, methods::FLOW_BIND, json!({"flow": flow})).await);
    let view = rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await;
    let request_id = Uuid::now_v7();
    let control = json!({"flow": flow, "request_id": request_id, "expected_revision": view["execution"]["revision"], "action": "pause"});
    let paused: ControlFlowResponse =
        typed(rpc(&state, methods::FLOW_CONTROL, control.clone()).await);
    assert_eq!(paused.request_id, request_id);
    assert_eq!(
        typed::<ControlFlowResponse>(rpc(&state, methods::FLOW_CONTROL, control).await),
        paused
    );
    let input_request = Uuid::now_v7();
    let invocation = json!({"flow": flow, "request_id": input_request, "arguments": {"text": "accepted once"}, "user": {"text": "accepted once"}});
    let accepted: InvokeFlowResponse =
        typed(rpc(&state, methods::FLOW_INVOKE, invocation.clone()).await);
    assert_eq!(accepted.acceptance.disposition, InputDisposition::Pending);
    assert_eq!(
        typed::<InvokeFlowResponse>(rpc(&state, methods::FLOW_INVOKE, invocation).await),
        accepted
    );
    assert_eq!(
        accepted.acceptance.receipt.journal_id,
        launched.receipt.journal_id
    );
    assert!(accepted.acceptance.receipt.sequence > paused.receipt.sequence);
    let query = json!({"flow": flow, "request_id": input_request});
    let pending: InputStatusResponse =
        typed(rpc(&state, methods::FLOW_INPUT_STATUS, query.clone()).await);
    let observed = pending.observation.unwrap();
    assert_eq!(observed.accepted, accepted.acceptance.receipt);
    assert_eq!(observed.message_id, accepted.acceptance.message_id);
    assert_eq!(observed.input, InputConsumption::Pending);
    assert!(
        !serde_json::to_string(&observed)
            .unwrap()
            .contains("accepted once")
    );
    let missing: InputStatusResponse = typed(
        rpc(
            &state,
            methods::FLOW_INPUT_STATUS,
            json!({"flow": flow, "request_id": Uuid::now_v7()}),
        )
        .await,
    );
    assert!(missing.observation.is_none());
    let denied = atman_daemon::dispatch_as(
        state.clone(),
        JsonRpcRequest::new(1, methods::FLOW_INPUT_STATUS, query.clone()),
        "foreign",
    )
    .await;
    assert_eq!(
        denied.error.unwrap().code,
        atman_proto::JsonRpcError::APPLICATION_ERROR
    );
    for params in [
        json!({"flow": flow, "request_id": input_request, "sender": Uuid::now_v7()}),
        json!({"flow": flow, "request_id": Uuid::nil()}),
    ] {
        assert_eq!(
            dispatch(
                state.clone(),
                JsonRpcRequest::new(1, methods::FLOW_INPUT_STATUS, params)
            )
            .await
            .error
            .unwrap()
            .code,
            atman_proto::JsonRpcError::INVALID_PARAMS
        );
    }
    typed::<ListSessionsResponse>(rpc(&state, methods::LIST_SESSIONS, json!({})).await);
    typed::<CancelRunResponse>(
        rpc(
            &state,
            methods::CANCEL_RUN,
            json!({"run_id": Uuid::now_v7()}),
        )
        .await,
    );
    state.shutdown().await.unwrap();

    // Cold lookup keeps the original acceptance and cannot install a Session or execution owner.
    let state = Arc::new(DaemonState::new(directory.path().join("data")));
    let journal = directory
        .path()
        .join("data/runtime-v1/sessions")
        .join(id.to_string())
        .join("flows")
        .join(launched.flow.instance_id.to_string())
        .join("events.jsonl");
    let before = std::fs::read(&journal).unwrap();
    let cold: InputStatusResponse =
        typed(rpc(&state, methods::FLOW_INPUT_STATUS, query.clone()).await);
    assert_eq!(cold.observation.unwrap().input, InputConsumption::Pending);
    assert_eq!(std::fs::read(&journal).unwrap(), before);
    let unopened = dispatch(
        state.clone(),
        JsonRpcRequest::new(1, methods::FLOW_GET, json!({"flow": flow})),
    )
    .await;
    assert!(
        unopened.error.is_some(),
        "status lookup must not attach a Session"
    );
    rpc(&state, methods::SESSION_OPEN, json!({"session_id": id})).await;
    rpc(&state, methods::FLOW_BIND, json!({"flow": flow})).await;
    let view = rpc(&state, methods::FLOW_GET, json!({"flow": flow})).await;
    rpc(
        &state,
        methods::FLOW_CONTROL,
        json!({"flow": flow, "request_id": Uuid::now_v7(),
        "expected_revision": view["execution"]["revision"], "action": "resume"}),
    )
    .await;
    let consumed = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let status: InputStatusResponse =
                typed(rpc(&state, methods::FLOW_INPUT_STATUS, query.clone()).await);
            let observation = status.observation.unwrap();
            if matches!(observation.input, InputConsumption::Committed { .. }) {
                break observation;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(consumed.accepted, accepted.acceptance.receipt);
    let InputConsumption::Committed { lease, receipt } = &consumed.input else {
        unreachable!()
    };
    assert_eq!(lease.flow, launched.flow);
    assert!(receipt.sequence > consumed.accepted.sequence);
    state.shutdown().await.unwrap();
    let state = Arc::new(DaemonState::new(directory.path().join("data")));
    let recovered: InputStatusResponse =
        typed(rpc(&state, methods::FLOW_INPUT_STATUS, query).await);
    let recovered = recovered.observation.unwrap();
    assert_eq!(recovered.accepted, consumed.accepted);
    assert_eq!(recovered.input, consumed.input);
    state.shutdown().await.unwrap();
}

#[test]
fn fixed_durable_receipt_and_disposition_shapes_are_compatible() {
    let receipt =
        json!({"journal_id": Uuid::from_u128(1), "sequence": 7, "digest": "a".repeat(64)});
    let cursor: FlowCursor = typed(receipt.clone());
    assert_eq!(cursor.to_string().parse::<FlowCursor>().unwrap(), cursor);
    for disposition in [
        json!({"state": "pending"}),
        json!({"state": "input_committed", "by": receipt}),
        json!({"state": "cancelled", "by": receipt}),
    ] {
        typed::<FlowAcceptance>(
            json!({"message_id": Uuid::from_u128(2), "receipt": receipt, "disposition": disposition}),
        );
    }
    for scheduler in [
        json!({"status": "running"}),
        json!({"status": "stopped"}),
        json!({"status": "failed", "error": "entry unavailable"}),
    ] {
        typed::<SchedulingView>(scheduler);
    }
}
