use std::{path::Path, sync::Arc, time::Duration};

use atman_proto::{JsonRpcRequest, flows::FlowAddress, methods, responses::LaunchFlowResponse};
use atman_runtime::{
    flow_store::{
        EntrySnapshot, FlowDescriptor,
        history_operation::{HistoryOperationTarget, HistoryReviewRequest},
        source_capture::{ConversationBranchId, ConversationCutoff, SourceSelection},
        view::{AcceptedInput, FlowUpdate},
    },
    source_program::load_program,
};
use serde_json::{Value, json};
use uuid::Uuid;

use super::*;
use crate::{dispatch, flow_service::service, run::RunLauncher};

async fn rpc(state: &Arc<DaemonState>, method: &str, params: Value) -> Value {
    let response = dispatch(state.clone(), JsonRpcRequest::new(1, method, params)).await;
    assert!(response.error.is_none(), "{method}: {:?}", response.error);
    response.result.unwrap()
}

fn daemon(data: &Path, config: &Path) -> Arc<DaemonState> {
    let state = Arc::new(DaemonState::new(data.to_owned()));
    state.set_launcher(Arc::new(
        RunLauncher::new(config.to_owned(), Some(config.to_owned()), None).unwrap(),
    ));
    state
}

async fn quiescent(state: &DaemonState) {
    tokio::time::timeout(
        Duration::from_secs(10),
        service(state).host.wait_quiescent(),
    )
    .await
    .unwrap()
    .unwrap();
}

fn assert_launch_identity(original: &LaunchFlowResponse, current: Value) -> LaunchFlowResponse {
    let current: LaunchFlowResponse = serde_json::from_value(current).unwrap();
    assert_eq!(current.flow, original.flow);
    assert_eq!(current.receipt, original.receipt);
    assert_eq!(
        current
            .acceptance
            .as_ref()
            .map(|value| (&value.message_id, &value.receipt)),
        original
            .acceptance
            .as_ref()
            .map(|value| (&value.message_id, &value.receipt)),
    );
    current
}

#[tokio::test]
async fn composer_defaults_reach_human_input_without_changing_its_retry_identity() {
    use atman_runtime::user_input::preferences::{ComposerReasoning, PreferenceEdit};

    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config");
    let commands = config.join("commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(
        config.join("config.toml"),
        r#"
[providers.vendor]
kind = "openai-compat"
api_key = "test-key"
base_url = "https://gateway.example/v1"
reasoning_format = "reasoning-effort"

[models.model_a]
model = "fixture/alpha"
provider = "vendor"
context_budget = 8192
reasoning_efforts = ["low", "high"]

[models.model_b]
model = "fixture/beta"
provider = "vendor"
context_budget = 8192
reasoning_efforts = ["low", "high"]

[alias.smart]
model = "model_a"
"#,
    )
    .unwrap();
    std::fs::write(
        commands.join("echo.at"),
        r#"flow echo(input: string) -> string {
            contract { invocation { user_message: input } }
            when env("effort") == "off" { return final.answer(message: "off|" + input) }
            when env("effort") == "high" { return final.answer(message: "high|" + input) }
            when env("effort") == "low" { return final.answer(message: "low|" + input) }
            return final.answer(message: "missing invocation effort")
        }"#,
    )
    .unwrap();
    let state = daemon(&directory.path().join("data"), &config);
    let first = Uuid::now_v7();
    let second = Uuid::now_v7();
    let created = rpc(
        &state,
        methods::SESSION_CREATE,
        json!({"session_id": first, "title": "First"}),
    )
    .await;
    rpc(
        &state,
        methods::SESSION_CREATE,
        json!({"session_id": second, "title": "Second"}),
    )
    .await;
    let launch = rpc(
        &state,
        methods::SESSION_BOOTSTRAP,
        json!({"session_id": first, "expected_catalog": created["catalog"]}),
    )
    .await;
    let flow = launch["flow"].clone();
    let first_session = service(&state).session(first).unwrap();
    let second_session = service(&state).session(second).unwrap();
    first_session
        .set_input_preference(PreferenceEdit::Model("model_a".into()))
        .await
        .unwrap();
    second_session
        .set_input_preference(PreferenceEdit::Model("model_b".into()))
        .await
        .unwrap();
    let effort = |selection: &str| {
        PreferenceEdit::Reasoning(ComposerReasoning::Selection(selection.parse().unwrap()))
    };
    second_session
        .set_input_preference(effort("high"))
        .await
        .unwrap();
    rpc(&state, methods::SESSION_OPEN, json!({"session_id": first})).await;
    let composer = rpc(&state, methods::FLOW_COMPOSER, json!({"flow": flow})).await;
    assert_eq!(composer["model"]["model"], "model_a");
    assert_eq!(
        composer["model"]["reasoning"]["effective"]["effort"],
        "high"
    );

    let inherited = json!({
        "flow": flow, "request_id": Uuid::now_v7(),
        "user": {"text": "/echo inherited"},
    });
    let accepted = rpc(&state, methods::FLOW_SUBMIT, inherited.clone()).await;
    quiescent(&state).await;
    second_session
        .set_input_preference(effort("low"))
        .await
        .unwrap();
    rpc(&state, methods::SESSION_OPEN, json!({"session_id": first})).await;
    let retried = rpc(&state, methods::FLOW_SUBMIT, inherited).await;
    assert_eq!(
        retried["acceptance"]["message_id"],
        accepted["acceptance"]["message_id"]
    );
    assert_eq!(
        retried["acceptance"]["receipt"],
        accepted["acceptance"]["receipt"]
    );
    for (text, invocation) in [
        ("/echo later", json!({})),
        ("/echo explicit", json!({"effort": "off"})),
    ] {
        rpc(
            &state,
            methods::FLOW_SUBMIT,
            json!({"flow": flow, "request_id": Uuid::now_v7(), "user": {"text": text}, "invocation": invocation}),
        )
        .await;
    }
    quiescent(&state).await;
    let worker = first_session
        .loaded_flow(&atman_rt::FlowInstanceId(first.into()))
        .unwrap();
    let messages = worker.context().messages_full();
    for expected in ["high|inherited", "low|later", "off|explicit"] {
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.text_concat() == expected)
                .count(),
            1
        );
    }
    state.shutdown().await.unwrap();
}

#[tokio::test]
async fn authored_rpc_retains_original_receipts_across_source_head_and_host_changes() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config");
    let commands = config.join("commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(config.join("config.toml"), "").unwrap();
    std::fs::write(config.join("routes.at"), "default_route { flow: echo }\n").unwrap();
    let echo = r#"flow echo(input: string) -> string {
        contract { invocation { user_message: input } }
        when env("effort") == "high" { return final.answer(message: "high|" + input) }
        when env("effort") == "low" { return final.answer(message: "low|" + input) }
        return final.answer(message: "missing invocation effort")
    }"#;
    std::fs::write(commands.join("echo.at"), echo).unwrap();
    std::fs::write(
        config.join("on_session_start.at"),
        "flow startup() -> string { return final.answer(message: \"ready\") }",
    )
    .unwrap();
    let image = directory.path().join("authored.png");
    std::fs::write(&image, b"\x89PNG\r\n\x1a\n").unwrap();
    let data = directory.path().join("data");
    let state = daemon(&data, &config);
    let id = Uuid::now_v7();
    let created = rpc(
        &state,
        methods::SESSION_CREATE,
        json!({"session_id": id, "title": "Human input", "project_root": directory.path()}),
    )
    .await;
    assert_eq!(created["flow_count"], 0);
    assert!(created["default_flow"].is_null());
    let bootstrap = json!({"session_id": id, "expected_catalog": created["catalog"]});
    let (left, right) = tokio::join!(
        rpc(&state, methods::SESSION_BOOTSTRAP, bootstrap.clone()),
        rpc(&state, methods::SESSION_BOOTSTRAP, bootstrap.clone()),
    );
    let launch: LaunchFlowResponse = serde_json::from_value(left.clone()).unwrap();
    assert_launch_identity(&launch, right);
    assert_eq!(launch.flow.instance_id, id);
    assert!(launch.acceptance.is_some());
    quiescent(&state).await;
    let flow = serde_json::to_value(&launch.flow).unwrap();
    let session = service(&state).session(id).unwrap();
    let worker = service(&state)
        .host
        .flow(&core_address(launch.flow.clone()))
        .await
        .unwrap();
    let startup = worker
        .context()
        .messages_full()
        .iter()
        .filter(|message| message.text_concat() == "ready")
        .count();
    assert_eq!(startup, 1);
    let routed = "x=y \"quoted\" https://code.example/?a=b\nsecond=line";
    let prompt = format!("{routed}\n @./authored.png");
    let quote = "/not-a-command @/not-an-image.png input=quoted";
    let authored = json!({
        "flow": flow, "request_id": Uuid::now_v7(),
        "user": {"text": prompt, "quote": quote},
        "invocation": {"effort": "high"},
    });
    let other = json!({
        "flow": flow, "request_id": Uuid::now_v7(),
        "user": {"text": "/echo second"},
        "invocation": {"effort": "low"},
    });
    // A reopened scheduler is cold; two different requests share its original bind gate.
    session
        .stop_scheduling(worker.address().instance_id.clone())
        .await
        .unwrap();
    let (accepted, second) = tokio::join!(
        rpc(&state, methods::FLOW_SUBMIT, authored.clone()),
        rpc(&state, methods::FLOW_SUBMIT, other),
    );
    assert_ne!(
        accepted["acceptance"]["message_id"],
        second["acceptance"]["message_id"]
    );
    let typed: SubmitFlowResponse = serde_json::from_value(accepted.clone()).unwrap();
    assert_eq!(serde_json::to_value(&typed).unwrap(), accepted);
    assert_eq!(typed.flow, launch.flow);
    quiescent(&state).await;
    let events = rpc(
        &state,
        methods::FLOW_EVENTS,
        json!({"flow": flow, "limit": 128}),
    )
    .await;
    let input = events["batches"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|batch| batch["facts"].as_array().unwrap())
        .find_map(
            |fact| match serde_json::from_value::<FlowUpdate>(fact.clone()).unwrap() {
                FlowUpdate::InputAccepted(input)
                    if matches!(input.as_ref(), AcceptedInput::Command { id, .. }
                        if *id == typed.acceptance.message_id) =>
                {
                    Some(*input)
                }
                _ => None,
            },
        )
        .expect("accepted input must remain in the public event projection");
    let published = serde_json::to_value(&input).unwrap();
    for private in ["arguments", "invocation", "entry", "bundle"] {
        assert!(published["data"].get(private).is_none());
    }
    let AcceptedInput::Command {
        id: message_id,
        request_id,
        recipient,
        name,
        user: Some(user),
        ..
    } = input
    else {
        panic!("authored route must publish sanitized command input metadata");
    };
    assert_eq!(message_id, typed.acceptance.message_id);
    assert_eq!(request_id, typed.request_id);
    assert_eq!(&recipient, worker.address());
    assert_eq!(name, "echo");
    assert_eq!(user.images.len(), 1);
    assert_eq!(user.presentation.unwrap().quote.unwrap().text, quote);
    let messages = worker.context().messages_full();
    for output in [format!("high|{routed}"), "low|second".into()] {
        assert_eq!(
            messages
                .iter()
                .filter(|message| message.origin
                    == atman_runtime::message::MessageOrigin::FinalAnswer
                    && message.text_concat() == output)
                .count(),
            1,
            "routed arguments and per-submission invocation must execute intact"
        );
    }
    quiescent(&state).await;
    let status = worker.status().await.unwrap();
    let before_head = status.history_head.unwrap();
    let completed = status.completion.unwrap();
    let operator = LocalOperator::new(service(&state).operator);
    let review = worker
        .review_operator_history_operation(
            operator,
            HistoryReviewRequest {
                id: Uuid::now_v7(),
                sender: operator.sender(),
                flow: worker.address().clone(),
                expected_head: before_head.clone(),
                target: HistoryOperationTarget::Revert {
                    observed: SourceSelection {
                        flow: worker.address().clone(),
                        branch: before_head.branch,
                        cursor: status.cursor.require_durable().unwrap().clone(),
                    },
                    cutoff: Box::new(ConversationCutoff::AfterCompletedTurn {
                        lease: completed.completion.lease,
                        receipt: completed.receipt.require_durable().unwrap().clone(),
                    }),
                    branch: ConversationBranchId::random(),
                },
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    let operation = review.request.id;
    worker
        .begin_operator_history_operation(operator, review.request, |_| Ok(()))
        .await
        .unwrap();
    worker
        .stop_operator_history_operation(operator, operation, |_| Ok(()))
        .await
        .unwrap();
    worker
        .apply_operator_history_operation(operator, operation, |_| Ok(()))
        .await
        .unwrap();
    let head = worker.status().await.unwrap().history_head.unwrap();
    assert_ne!(head, before_head);
    quiescent(&state).await;
    let cursor = worker.status().await.unwrap().cursor;
    std::fs::write(config.join("routes.at"), "invalid route").unwrap();
    std::fs::write(commands.join("echo.at"), "invalid command").unwrap();
    std::fs::write(config.join("config.toml"), "invalid [[[").unwrap();
    std::fs::remove_file(&image).unwrap();
    let retried = rpc(&state, methods::FLOW_SUBMIT, authored.clone()).await;
    assert_eq!(
        retried["acceptance"]["message_id"],
        accepted["acceptance"]["message_id"]
    );
    assert_eq!(
        retried["acceptance"]["receipt"],
        accepted["acceptance"]["receipt"]
    );
    for field in ["text", "quote", "invocation"] {
        let mut conflict = authored.clone();
        match field {
            "text" => conflict["user"]["text"] = json!(format!("{prompt}!")),
            "quote" => conflict["user"]["quote"] = json!("changed context"),
            "invocation" => conflict["invocation"]["effort"] = json!("low"),
            _ => unreachable!(),
        }
        let rejected = dispatch(
            state.clone(),
            JsonRpcRequest::new(2, methods::FLOW_SUBMIT, conflict),
        )
        .await;
        assert!(rejected.error.is_some());
    }
    assert_eq!(worker.status().await.unwrap().cursor, cursor);
    let startup_retry = assert_launch_identity(
        &launch,
        rpc(&state, methods::SESSION_BOOTSTRAP, bootstrap.clone()).await,
    );
    let startup_acceptance = startup_retry.acceptance.unwrap();
    let atman_proto::responses::InputDisposition::InputCommitted { by } =
        startup_acceptance.disposition
    else {
        panic!("startup retry must report the original input consumption");
    };
    assert_eq!(by.journal_id, startup_acceptance.receipt.journal_id);
    assert!(by.sequence > startup_acceptance.receipt.sequence);
    assert_eq!(
        worker
            .context()
            .messages_full()
            .iter()
            .filter(|message| message.text_concat() == "ready")
            .count(),
        startup
    );
    // Fresh submissions still require the displayed selected head.
    std::fs::write(config.join("config.toml"), "").unwrap();
    std::fs::write(config.join("routes.at"), "default_route { flow: echo }\n").unwrap();
    std::fs::write(commands.join("echo.at"), echo).unwrap();
    for expected in [None, Some(wire::head(&before_head))] {
        let mut fresh =
            json!({"flow": flow, "request_id": Uuid::now_v7(), "user": {"text": "fresh"}});
        if let Some(expected) = expected {
            fresh["expected_head"] = serde_json::to_value(expected).unwrap();
        }
        let rejected = dispatch(
            state.clone(),
            JsonRpcRequest::new(3, methods::FLOW_SUBMIT, fresh),
        )
        .await;
        assert_eq!(
            rejected.error.unwrap().code,
            atman_proto::history::HISTORY_ADMISSION_ERROR
        );
    }
    assert_eq!(worker.status().await.unwrap().cursor, cursor);
    std::fs::write(config.join("config.toml"), "invalid [[[").unwrap();
    state.shutdown().await.unwrap();
    drop((worker, session, state));
    let state = daemon(&data, &config);
    rpc(&state, methods::SESSION_OPEN, json!({"session_id": id})).await;
    let reopened = rpc(&state, methods::FLOW_SUBMIT, authored).await;
    assert_eq!(
        reopened["acceptance"]["receipt"],
        accepted["acceptance"]["receipt"]
    );
    assert_eq!(
        reopened["acceptance"]["message_id"],
        accepted["acceptance"]["message_id"]
    );
    let reopened_startup = assert_launch_identity(
        &launch,
        rpc(&state, methods::SESSION_BOOTSTRAP, bootstrap).await,
    );
    assert!(matches!(reopened_startup.acceptance.unwrap().disposition,
        atman_proto::responses::InputDisposition::InputCommitted { by: reopened } if reopened == by));
    assert!(service(&state).executors.lock().unwrap().is_empty());
    let session = service(&state).session(id).unwrap();
    assert!(
        session
            .scheduling_status(atman_rt::FlowInstanceId(id.into()))
            .await
            .unwrap()
            .is_none()
    );
    let worker = service(&state)
        .host
        .flow(&core_address(launch.flow))
        .await
        .unwrap();
    assert_eq!(worker.status().await.unwrap().cursor, cursor);
    state.shutdown().await.unwrap();
}

#[tokio::test]
async fn bootstrap_preserves_retained_source_and_rejects_conflicting_catalogs() {
    let directory = tempfile::tempdir().unwrap();
    let config = directory.path().join("config");
    std::fs::create_dir_all(config.join("commands")).unwrap();
    std::fs::write(config.join("config.toml"), "").unwrap();
    std::fs::write(config.join("commands/agent.at"), "broken source").unwrap();
    let path = config.join("retained.at");
    std::fs::write(&path, "flow main(input: string) -> string { contract { invocation { user_message: input } } return final.answer(message: input) }").unwrap();
    let state = daemon(&directory.path().join("data"), &config);
    let id = Uuid::now_v7();
    let created = rpc(
        &state,
        methods::SESSION_CREATE,
        json!({"session_id": id, "title": "Retained source", "project_root": null}),
    )
    .await;
    let mut stale = created["catalog"].clone();
    stale["digest"] = json!("0".repeat(64));
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(
                1,
                methods::SESSION_BOOTSTRAP,
                json!({"session_id": id, "expected_catalog": stale})
            )
        )
        .await
        .error
        .is_some()
    );
    let session = service(&state).session(id).unwrap();
    let snapshot = session.snapshot().await.unwrap();
    assert_eq!(
        super::super::events::cursor(&snapshot.cursor),
        serde_json::from_value::<atman_proto::flows::FlowCursor>(created["catalog"].clone())
            .unwrap()
    );
    assert!(snapshot.catalog.flows.is_empty());
    let program = load_program(&path, &snapshot.catalog.descriptor.source_roots()).unwrap();
    let descriptor = FlowDescriptor {
        address: FlowRef {
            session_id: id.into(),
            instance_id: atman_rt::FlowInstanceId(id.into()),
        },
        creation_request: id,
        created_by: None,
        content: EntrySnapshot {
            name: "main".into(),
            path: path.clone(),
            bundle: program.revision_bundle(),
        }
        .into(),
    };
    session.create_flow(descriptor.clone()).await.unwrap();
    std::fs::remove_file(&path).unwrap();
    let bootstrap = json!({"session_id": id, "expected_catalog": created["catalog"]});
    let launched: LaunchFlowResponse =
        serde_json::from_value(rpc(&state, methods::SESSION_BOOTSTRAP, bootstrap.clone()).await)
            .unwrap();
    assert!(launched.acceptance.is_none());
    let root = service(&state)
        .host
        .flow(&descriptor.address)
        .await
        .unwrap();
    assert_eq!(root.descriptor().await.unwrap(), descriptor);
    assert_eq!(
        std::fs::read_to_string(config.join("commands/agent.at")).unwrap(),
        "broken source"
    );
    let mut other = descriptor.clone();
    other.address.instance_id = atman_runtime::flow_store::FlowInstanceId::now();
    other.creation_request = Uuid::now_v7();
    session.create_flow(other.clone()).await.unwrap();
    let catalog = session.snapshot().await.unwrap();
    session
        .select_default(catalog.cursor, other.address.instance_id.clone())
        .await
        .unwrap();
    let before = session.snapshot().await.unwrap();
    assert_eq!(
        serde_json::from_value::<LaunchFlowResponse>(
            rpc(&state, methods::SESSION_BOOTSTRAP, bootstrap).await
        )
        .unwrap(),
        launched
    );
    assert_eq!(session.snapshot().await.unwrap().cursor, before.cursor);
    assert_eq!(
        session.snapshot().await.unwrap().catalog.default_flow,
        Some(other.address.instance_id)
    );
    let id = Uuid::now_v7();
    let created = rpc(
        &state,
        methods::SESSION_CREATE,
        json!({"session_id": id, "title": "Other default", "project_root": null}),
    )
    .await;
    let session = service(&state).session(id).unwrap();
    let mut other = descriptor;
    other.address.session_id = id.into();
    other.address.instance_id = atman_runtime::flow_store::FlowInstanceId::now();
    other.creation_request = Uuid::now_v7();
    session.create_flow(other).await.unwrap();
    let before = session.snapshot().await.unwrap();
    let rejected = dispatch(state.clone(), JsonRpcRequest::new(2, methods::SESSION_BOOTSTRAP,
        json!({"session_id": id, "expected_catalog": super::super::events::cursor(&before.cursor)}))).await;
    assert!(rejected.error.is_some());
    assert_eq!(session.snapshot().await.unwrap().cursor, before.cursor);
    let forged =
        json!({"session_id": id, "expected_catalog": created["catalog"], "sender": "forged"});
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(3, methods::SESSION_BOOTSTRAP, forged)
        )
        .await
        .error
        .is_some()
    );
    let submitted = json!({"flow": FlowAddress { session_id: atman_proto::SessionId(id), instance_id: id },
        "request_id": Uuid::now_v7(), "user": {"text": "message", "role": "host"}});
    assert!(
        dispatch(
            state.clone(),
            JsonRpcRequest::new(4, methods::FLOW_SUBMIT, submitted.clone())
        )
        .await
        .error
        .is_some()
    );
    assert!(
        crate::dispatch_as(
            state.clone(),
            JsonRpcRequest::new(5, methods::FLOW_SUBMIT, submitted),
            "other-operator"
        )
        .await
        .error
        .is_some()
    );
    assert_eq!(session.snapshot().await.unwrap().cursor, before.cursor);
    state.shutdown().await.unwrap();
}
