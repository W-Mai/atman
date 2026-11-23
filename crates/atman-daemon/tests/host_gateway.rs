use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use atman_daemon::{
    DaemonState, EmbeddedRpcServer, SharedHostServices,
    host_directory::{HostRecord, HostRegistration, HostRole},
    host_gateway::GENERATION_HEADER,
    run::RunLauncher,
};
use atman_proto::{JsonRpcRequest, JsonRpcResponse, methods};
use atman_runtime::{
    flow_runtime::host::RuntimeHost,
    flow_store::{
        EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowTransition, RuntimeStore,
        SessionDescriptor,
    },
    permission::PermissionClients,
};
use serde_json::{Value, json};
use uuid::Uuid;

const TOKEN: &str = "shared-host-secret";

fn tui(data: &std::path::Path, config: &std::path::Path, generation: Uuid) -> Arc<DaemonState> {
    let host = RuntimeHost::default();
    let state = Arc::new(DaemonState::with_shared_host(
        data.to_owned(),
        generation.to_string(),
        SharedHostServices {
            host,
            permissions: Arc::new(PermissionClients::default()),
            operator: Uuid::from_bytes(*b"atman-local-user"),
            prompts: None,
        },
    ));
    std::fs::create_dir_all(config).unwrap();
    state.set_launcher(Arc::new(
        RunLauncher::new(config.to_owned(), Some(config.to_owned()), None).unwrap(),
    ));
    state
}

async fn register(
    data: &std::path::Path,
    state: Arc<DaemonState>,
    role: HostRole,
) -> (EmbeddedRpcServer, HostRegistration) {
    let server = EmbeddedRpcServer::bind(state.clone(), TOKEN.into())
        .await
        .unwrap();
    let registration = HostRegistration::register(
        data,
        HostRecord {
            generation: state.daemon_generation().parse().unwrap(),
            process_id: std::process::id(),
            role,
            endpoint: server.endpoint(),
            sessions: state.runtime_host().sessions(),
            presentation: state.presentation(),
        },
    )
    .unwrap();
    (server, registration)
}

async fn request(
    client: &reqwest::Client,
    endpoint: std::net::SocketAddr,
    method: &str,
    params: Value,
) -> Value {
    let response = client
        .post(format!("http://{endpoint}/rpc"))
        .bearer_auth(TOKEN)
        .json(&JsonRpcRequest::new("gateway-request", method, params))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    response.json().await.unwrap()
}

fn result(response: Value) -> Value {
    assert!(response["error"].is_null(), "{response}");
    assert_eq!(response["id"], "gateway-request");
    response["result"].clone()
}

#[tokio::test]
async fn quick_ask_uses_the_original_session_owner_for_transcript_inputs_and_stop() {
    use atman_runtime::{
        AtmanRuntime, AtmanRuntimeOptions, config_hub::ConfigHub, event::EventSink,
    };
    use tokio::sync::Notify;

    for embedded in [false, true] {
        let directory = tempfile::tempdir().unwrap();
        let data = directory.path().join("data");
        let config = directory.path().join("config");
        let provider = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let provider_address = provider.local_addr().unwrap();
        let provider_calls = Arc::new(Mutex::new(Vec::new()));
        let calls = provider_calls.clone();
        let model_entered = Arc::new(Notify::new());
        let release_model = Arc::new(Notify::new());
        let waiting = model_entered.clone();
        let released = release_model.clone();
        let provider = tokio::spawn(async move {
            axum::serve(provider, axum::Router::new().route("/v1/chat/completions", axum::routing::post(
                move |axum::Json(body): axum::Json<Value>| {
                    let calls = calls.clone();
                    let waiting = waiting.clone();
                    let released = released.clone();
                    async move {
                        calls.lock().unwrap().push(body);
                        waiting.notify_one();
                        released.notified().await;
                        ([(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                            concat!(
                                "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Fixture answer\"}}]}\n\n",
                                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
                                "data: [DONE]\n\n",
                            ))
                    }
                },
            ))).await.unwrap()
        });
        std::fs::create_dir_all(config.join("commands")).unwrap();
        std::fs::write(
            config.join("config.toml"),
            format!(
                r#"
[trust]
mode = "reckless"
[providers.fixture]
kind = "openai-compat"
base_url = "http://{provider_address}/v1"
api_key = "fixture"
[models.test-model]
model = "test-model"
provider = "fixture"
context_budget = 32768
[models.small-question-model]
model = "small-question-model"
provider = "fixture"
context_budget = 512
[alias.smart]
model = "test-model"
"#
            ),
        )
        .unwrap();
        atman_runtime::templates::ensure_managed_agent_at(&config).unwrap();
        std::fs::write(
            config.join("commands/echo.at"),
            r#"flow echo(input: string) -> string {
                contract { invocation { user_message: input } }
                return final.answer(message: input)
            }"#,
        )
        .unwrap();
        let owner = if embedded {
            tui(&data, &config, Uuid::now_v7())
        } else {
            let state = Arc::new(DaemonState::new(data.clone()));
            state.set_launcher(Arc::new(
                RunLauncher::new(config.clone(), Some(config.clone()), None).unwrap(),
            ));
            state
        };
        let host = owner.runtime_host();
        let hub = ConfigHub::from_config_dir(&config);
        let sid = Uuid::now_v7();
        let session = host
            .create_session(
                &data,
                SessionDescriptor {
                    id: sid,
                    project: None,
                    title: "Quick Ask source".into(),
                    configuration: atman_runtime::flow_store::SessionConfiguration::Atman(
                        hub.source(),
                    ),
                },
            )
            .await
            .unwrap();
        let executor = AtmanRuntime::build(AtmanRuntimeOptions {
            events: EventSink::new(),
            mock: false,
            config: hub,
            project: None,
            home_dir: None,
            workspace_generation: owner.daemon_generation().into(),
        })
        .await
        .unwrap()
        .executor;
        let block = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let resolves = Arc::new(AtomicUsize::new(0));
        let services_unavailable = Arc::new(std::sync::atomic::AtomicBool::new(false));
        session
            .configure_executor({
                let (block, entered, release, resolves) = (
                    block.clone(),
                    entered.clone(),
                    release.clone(),
                    resolves.clone(),
                );
                let services_unavailable = services_unavailable.clone();
                move || {
                    Arc::new(move |_| {
                        let (block, entered, release, resolves) = (
                            block.clone(),
                            entered.clone(),
                            release.clone(),
                            resolves.clone(),
                        );
                        let executor = executor.clone();
                        let services_unavailable = services_unavailable.clone();
                        Box::pin(async move {
                            resolves.fetch_add(1, Ordering::SeqCst);
                            if services_unavailable.load(Ordering::SeqCst) {
                                return Err(std::io::Error::other(
                                    "fixture execution services unavailable",
                                ));
                            }
                            if block.swap(false, Ordering::SeqCst) {
                                entered.notify_one();
                                release.notified().await;
                            }
                            Ok(executor)
                        })
                    })
                }
            })
            .unwrap();
        // Open the real owner before installing an independent gateway.
        let (owner_server, owner_registration) = register(
            &data,
            owner.clone(),
            if embedded {
                HostRole::Tui
            } else {
                HostRole::Daemon
            },
        )
        .await;
        let gateway = Arc::new(DaemonState::new(data.clone()));
        gateway.configure_host_routing(TOKEN.into()).unwrap();
        let (gateway_server, gateway_registration) =
            register(&data, gateway.clone(), HostRole::Daemon).await;
        let endpoint = if embedded {
            gateway_server.endpoint()
        } else {
            owner_server.endpoint()
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap();
        let call = |method, params| request(&client, endpoint, method, params);
        let opened = result(call(methods::SESSION_OPEN, json!({"session_id":sid})).await);
        let launch = result(
            call(
                methods::SESSION_BOOTSTRAP,
                json!({"session_id":sid,"expected_catalog":opened["catalog"]}),
            )
            .await,
        );
        let flow = launch["flow"].clone();
        let submit =
            |text: &str| json!({"flow":flow,"request_id":Uuid::now_v7(),"user":{"text":text}});
        result(
            call(
                methods::FLOW_SUBMIT,
                submit("/echo original question context"),
            )
            .await,
        );
        tokio::time::timeout(Duration::from_secs(10), host.wait_quiescent())
            .await
            .unwrap()
            .unwrap();
        let page = result(call(methods::FLOW_CONVERSATION, json!({"flow":flow,"limit":32})).await);
        let identity =
            json!({"source":flow,"question_id":Uuid::now_v7(),"request_id":Uuid::now_v7()});
        let unseen = result(call(methods::QUESTION_STATUS, json!({"identity":identity})).await);
        assert_eq!(unseen["attempt"]["state"], "unseen");
        assert!(unseen["owner_generation"].is_string());
        let status = json!({"identity":identity,"owner_generation":unseen["owner_generation"]});
        let start = json!({
            "identity":identity,"owner_generation":unseen["owner_generation"],"expected_head":page["page"]["head"],
            "observed":page["page"]["source"],"user":{"text":"Summarize the context"},"model":"smart"
        });
        let transient = atman_rt::FlowInstanceId(
            identity["question_id"]
                .as_str()
                .unwrap()
                .parse::<Uuid>()
                .unwrap()
                .into(),
        );
        assert!(session.loaded_flow(&transient).is_none());
        let before = session.snapshot().await.unwrap().cursor;
        resolves.store(0, Ordering::SeqCst);
        block.store(true, Ordering::SeqCst);
        let original = tokio::spawn({
            let client = client.clone();
            let start = start.clone();
            async move { request(&client, endpoint, methods::QUESTION_START, start).await }
        });
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        let pending = result(call(methods::QUESTION_STATUS, status.clone()).await);
        assert_eq!(pending["attempt"]["state"], "preparing");
        assert!(session.loaded_flow(&transient).is_none());
        let mut altered = start.clone();
        altered["user"]["text"] = json!("Different request");
        assert!(call(methods::QUESTION_START, altered).await["error"].is_object());
        let mut other_question = start.clone();
        other_question["identity"]["question_id"] = json!(Uuid::now_v7());
        assert!(call(methods::QUESTION_START, other_question).await["error"].is_object());
        original.abort();
        assert!(original.await.unwrap_err().is_cancelled());
        release.notify_one();
        let observed = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let observed = result(call(methods::QUESTION_STATUS, status.clone()).await);
                if observed["attempt"]["state"] == "accepted" {
                    break observed;
                }
                assert_eq!(
                    observed["attempt"]["state"],
                    "preparing",
                    "{}",
                    call(methods::QUESTION_START, start.clone()).await
                );
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        let replay = result(call(methods::QUESTION_START, start.clone()).await);
        assert_eq!(replay["receipt"], observed["attempt"]["receipt"]);
        assert_eq!(
            replay["receipt"]["source"]["cursor"],
            start["observed"]["cursor"]
        );
        assert_eq!(
            replay["receipt"]["launch"]["flow_id"],
            identity["question_id"]
        );
        assert_eq!(
            replay["receipt"]["launch"]["host_generation"],
            json!(host.generation())
        );
        assert_eq!(resolves.load(Ordering::SeqCst), 1);
        let question = session.loaded_flow(&transient).unwrap();
        assert!(question.is_transient());
        assert_eq!(session.snapshot().await.unwrap().cursor, before);
        let record = question.launch_record().await.unwrap().unwrap();
        let ceiling = record.input.execution.unwrap().authority;
        assert_eq!(ceiling, ceiling.read_only());
        tokio::time::timeout(Duration::from_secs(5), model_entered.notified())
            .await
            .unwrap();
        let mut transcript = json!({
            "identity":identity,
            "owner_generation":unseen["owner_generation"],
            "host_generation":replay["receipt"]["launch"]["host_generation"],
            "after":null,"limit":2
        });
        let mut rows = Vec::new();
        let mut last_sequence = 0;
        let mut collect = |page: &Value| {
            assert_eq!(page["state"], "ready", "{page}");
            for batch in page["batches"].as_array().unwrap() {
                last_sequence += 1;
                assert_eq!(batch["receipt"]["sequence"], last_sequence);
                rows.extend(batch["items"].as_array().unwrap().iter().cloned());
            }
        };
        loop {
            let page = result(call(methods::QUESTION_TRANSCRIPT, transcript.clone()).await);
            collect(&page["transcript"]);
            transcript["after"] = page["transcript"]["next"].clone();
            if page["transcript"]["has_more"] == false {
                break;
            }
        }
        let mut invalid = transcript.clone();
        invalid["after"]["digest"] = json!("0".repeat(64));
        assert!(call(methods::QUESTION_TRANSCRIPT, invalid).await["error"].is_object());
        let mut wrong_owner = transcript.clone();
        wrong_owner["owner_generation"] = json!(Uuid::now_v7());
        assert_eq!(
            result(call(methods::QUESTION_TRANSCRIPT, wrong_owner).await)["transcript"]["state"],
            "retired"
        );
        let mut wrong_runtime = transcript.clone();
        wrong_runtime["host_generation"] = json!(Uuid::now_v7());
        wrong_runtime["after"] = Value::Null;
        assert_eq!(
            result(call(methods::QUESTION_TRANSCRIPT, wrong_runtime).await)["transcript"]["state"],
            "retired"
        );
        let mut wait = transcript.clone();
        wait["wait"] = json!(true);
        let mut observation = tokio::spawn({
            let client = client.clone();
            let wait = wait.clone();
            async move { request(&client, endpoint, methods::QUESTION_TRANSCRIPT, wait).await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut observation)
                .await
                .is_err()
        );
        observation.abort();
        assert!(observation.await.unwrap_err().is_cancelled());
        let mut resumed = tokio::spawn({
            let client = client.clone();
            async move { request(&client, endpoint, methods::QUESTION_TRANSCRIPT, wait).await }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(100), &mut resumed)
                .await
                .is_err()
        );
        let followup_status = json!({
            "question": {
                "identity": identity,
                "owner_generation": unseen["owner_generation"],
                "host_generation": replay["receipt"]["launch"]["host_generation"]
            },
            "followup_request_id": Uuid::now_v7()
        });
        let stop_request_id = Uuid::now_v7();
        let stop_status =
            json!({"question":followup_status["question"],"stop_request_id":stop_request_id});
        let first_execution = result(call(methods::QUESTION_EXECUTION, stop_status.clone()).await);
        let first_execution = &first_execution["execution"]["observation"];
        assert_eq!(
            first_execution["stoppable"]["flow"]["instance_id"],
            identity["question_id"]
        );
        assert!(first_execution["stop"].is_null());
        let refresh_status =
            json!({"question":stop_status["question"], "refresh_request_id":Uuid::now_v7()});
        let mut refresh = refresh_status.clone();
        refresh["expected"] = first_execution["cursor"].clone();
        refresh["source_head"] = page["page"]["head"].clone();
        refresh["observed_source"] = page["page"]["source"].clone();
        refresh["model"] = json!("smart");
        assert!(call(methods::QUESTION_REFRESH, refresh.clone()).await["error"].is_object());
        assert_eq!(resolves.load(Ordering::SeqCst), 1);
        let mut followup = followup_status.clone();
        followup["observed"] = transcript["after"].clone();
        followup["user"] = json!({"text":"Follow-up details"});
        followup["model"] = json!("smart");
        let mut bad = followup.clone();
        bad["observed"]["digest"] = json!("0".repeat(64));
        assert!(call(methods::QUESTION_FOLLOWUP, bad).await["error"].is_object());
        assert_eq!(resolves.load(Ordering::SeqCst), 1);
        let mut foreign_followup = followup_status.clone();
        foreign_followup["question"]["identity"]["request_id"] = json!(Uuid::now_v7());
        assert_eq!(
            result(call(methods::QUESTION_FOLLOWUP_STATUS, foreign_followup).await)["followup"]["state"],
            "unavailable"
        );
        block.store(true, Ordering::SeqCst);
        let interrupted_followup = tokio::spawn({
            let client = client.clone();
            let followup = followup.clone();
            async move { request(&client, endpoint, methods::QUESTION_FOLLOWUP, followup).await }
        });
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        assert_eq!(
            result(call(methods::QUESTION_FOLLOWUP_STATUS, followup_status.clone()).await)["followup"]
                ["state"],
            "unseen"
        );
        interrupted_followup.abort();
        assert!(interrupted_followup.await.unwrap_err().is_cancelled());
        release.notify_one();
        let followup_observed = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let response =
                    result(call(methods::QUESTION_FOLLOWUP_STATUS, followup_status.clone()).await);
                if response["followup"]["state"] == "observed" {
                    break response["followup"]["observation"].clone();
                }
                assert_eq!(response["followup"]["state"], "unseen");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(followup_observed["input"]["state"], "pending");
        services_unavailable.store(true, Ordering::SeqCst);
        let accepted = result(call(methods::QUESTION_FOLLOWUP, followup.clone()).await);
        assert_eq!(accepted["accepted"], followup_observed["accepted"]);
        assert_eq!(accepted["message_id"], followup_observed["message_id"]);
        let mut altered_followup = followup.clone();
        altered_followup["model"] = json!("another-model");
        assert!(call(methods::QUESTION_FOLLOWUP, altered_followup).await["error"].is_object());
        assert_eq!(resolves.load(Ordering::SeqCst), 2);
        services_unavailable.store(false, Ordering::SeqCst);
        let page = result(
            tokio::time::timeout(Duration::from_secs(3), resumed)
                .await
                .unwrap()
                .unwrap(),
        );
        assert!(!page["transcript"]["batches"].as_array().unwrap().is_empty());
        collect(&page["transcript"]);
        transcript["after"] = page["transcript"]["next"].clone();
        release_model.notify_one();
        tokio::time::timeout(Duration::from_secs(8), model_entered.notified())
            .await
            .unwrap();
        let mut stop = stop_status.clone();
        stop["expected_revision"] = first_execution["revision"].clone();
        stop["lease"] = first_execution["stoppable"].clone();
        assert!(call(methods::QUESTION_STOP, stop.clone()).await["error"].is_object());
        let committed =
            result(call(methods::QUESTION_FOLLOWUP_STATUS, followup_status.clone()).await);
        let committed = &committed["followup"]["observation"];
        assert_eq!(committed["accepted"], followup_observed["accepted"]);
        assert_eq!(committed["input"]["state"], "committed");
        assert_eq!(
            committed["input"]["lease"]["flow"]["instance_id"],
            identity["question_id"]
        );
        release_model.notify_one();
        tokio::time::timeout(Duration::from_secs(10), host.wait_quiescent())
            .await
            .unwrap()
            .unwrap();
        assert!(question.completion(None).await.unwrap().is_some());
        loop {
            let page = result(call(methods::QUESTION_TRANSCRIPT, transcript.clone()).await);
            collect(&page["transcript"]);
            transcript["after"] = page["transcript"]["next"].clone();
            if page["transcript"]["has_more"] == false {
                break;
            }
        }
        let materialized = serde_json::to_string(&rows).unwrap();
        assert!(materialized.contains("Summarize the context"));
        assert!(materialized.contains("Fixture answer"));
        assert!(!materialized.contains("original question context"));
        assert_eq!(
            rows.iter()
                .filter(|row| row["work"]["role"] == "final_answer")
                .count(),
            2
        );
        assert_eq!(materialized.matches("Follow-up details").count(), 1);
        let calls = provider_calls.lock().unwrap().clone();
        assert_eq!(calls.len(), 2);
        assert!(
            calls[0]["messages"]
                .to_string()
                .contains("original question context")
        );
        assert!(
            calls[1]["messages"]
                .to_string()
                .contains("Follow-up details")
        );
        assert!(calls[1]["messages"].to_string().contains("Fixture answer"));
        result(
            call(
                methods::FLOW_SUBMIT,
                submit("/echo updated reference evidence"),
            )
            .await,
        );
        tokio::time::timeout(Duration::from_secs(10), host.wait_quiescent())
            .await
            .unwrap()
            .unwrap();
        let latest_source =
            result(call(methods::FLOW_CONVERSATION, json!({"flow":flow,"limit":4})).await);
        let question_tail = transcript["after"].clone();
        let model_check =
            json!({"question":stop_status["question"],"expected":question_tail,"model":"smart"});
        let selected = result(call(methods::QUESTION_MODEL_CHECK, model_check.clone()).await);
        assert_eq!(selected["model"], "test-model");
        assert_eq!(selected["checked"], question_tail);
        assert_eq!(selected["question"], model_check["question"]);
        let mut model_options = json!({"flow":flow,"question":model_check["question"],"model":"smart","catalog_query":"SMALL"});
        let menu = result(call(methods::QUESTION_MODEL_OPTIONS, model_options.clone()).await);
        assert_eq!(menu["question"], model_check["question"]);
        assert_eq!(menu["model"]["model"], "test-model");
        assert_eq!(menu["model"]["reasoning"]["requested"], Value::Null);
        assert_eq!(menu["catalog"]["rows"].as_array().unwrap().len(), 1);
        assert_eq!(menu["catalog"]["rows"][0]["key"], "small-question-model");
        model_options["question"]["owner_generation"] = json!(Uuid::now_v7());
        assert!(call(methods::QUESTION_MODEL_OPTIONS, model_options).await["error"].is_object());
        let mut foreign_model = model_check.clone();
        foreign_model["question"]["owner_generation"] = json!(Uuid::now_v7());
        assert!(call(methods::QUESTION_MODEL_CHECK, foreign_model).await["error"].is_object());
        let mut false_boundary = model_check.clone();
        false_boundary["expected"]["digest"] = json!("0".repeat(64));
        assert!(call(methods::QUESTION_MODEL_CHECK, false_boundary).await["error"].is_object());
        let mut too_small = model_check.clone();
        too_small["model"] = json!("small-question-model");
        let rejected = call(methods::QUESTION_MODEL_CHECK, too_small).await;
        assert!(
            rejected["error"]["message"]
                .as_str()
                .unwrap()
                .contains("cannot fit the retained question context"),
            "{rejected}"
        );
        refresh["expected"] = question_tail.clone();
        let mut wrong_head = refresh.clone();
        wrong_head["source_head"]["version"] = json!(999);
        assert!(call(methods::QUESTION_REFRESH, wrong_head).await["error"].is_object());
        let mut wrong_source = refresh.clone();
        wrong_source["observed_source"]["cursor"]["digest"] = json!("0".repeat(64));
        assert!(call(methods::QUESTION_REFRESH, wrong_source).await["error"].is_object());
        assert_eq!(
            question.snapshot().await.unwrap().cursor.sequence(),
            question_tail["sequence"]
        );
        assert_eq!(provider_calls.lock().unwrap().len(), 2);
        block.store(true, Ordering::SeqCst);
        let lost = tokio::spawn({
            let client = client.clone();
            let refresh = refresh.clone();
            async move { request(&client, endpoint, methods::QUESTION_REFRESH, refresh).await }
        });
        tokio::time::timeout(Duration::from_secs(5), entered.notified())
            .await
            .unwrap();
        let waiting = result(call(methods::QUESTION_REFRESH_STATUS, refresh_status.clone()).await);
        assert_eq!(waiting["refresh"]["state"], "unseen");
        lost.abort();
        assert!(lost.await.unwrap_err().is_cancelled());
        release.notify_one();
        let refreshed = tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let status =
                    result(call(methods::QUESTION_REFRESH_STATUS, refresh_status.clone()).await);
                if status["refresh"]["state"] == "applied" {
                    break status["refresh"]["receipt"].clone();
                }
                assert_eq!(status["refresh"]["state"], "unseen");
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(refreshed["source"], latest_source["page"]["source"]);
        assert_eq!(
            refreshed["applied"]["sequence"].as_u64().unwrap(),
            question_tail["sequence"].as_u64().unwrap() + 1
        );
        services_unavailable.store(true, Ordering::SeqCst);
        assert_eq!(
            result(call(methods::QUESTION_REFRESH, refresh.clone()).await)["receipt"],
            refreshed
        );
        refresh["model"] = json!("another-model");
        assert!(call(methods::QUESTION_REFRESH, refresh).await["error"].is_object());
        services_unavailable.store(false, Ordering::SeqCst);
        assert_eq!(provider_calls.lock().unwrap().len(), 2);
        assert_eq!(session.snapshot().await.unwrap().cursor, before);
        let mut third = followup.clone();
        third["followup_request_id"] = json!(Uuid::now_v7());
        third["observed"] = refreshed["applied"].clone();
        third["user"]["text"] = json!("Hold this question open for cancellation");
        result(call(methods::QUESTION_FOLLOWUP, third).await);
        tokio::time::timeout(Duration::from_secs(8), model_entered.notified())
            .await
            .unwrap();
        assert!(
            provider_calls.lock().unwrap()[2]["messages"]
                .to_string()
                .contains("updated reference evidence")
        );
        let execution = result(call(methods::QUESTION_EXECUTION, stop_status.clone()).await);
        let execution = &execution["execution"]["observation"];
        stop["expected_revision"] = execution["revision"].clone();
        stop["lease"] = execution["stoppable"].clone();
        let mut foreign_stop = stop.clone();
        foreign_stop["lease"]["flow"] = flow.clone();
        assert!(call(methods::QUESTION_STOP, foreign_stop).await["error"].is_object());
        let stopped = result(call(methods::QUESTION_STOP, stop.clone()).await);
        let applied = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let queried = result(call(methods::QUESTION_EXECUTION, stop_status.clone()).await);
                let queried = &queried["execution"]["observation"];
                assert_eq!(queried["stop"]["accepted"], stopped["accepted"]);
                assert!(queried["stoppable"].is_null());
                if queried["stop"]["applied"].is_object() {
                    break queried["stop"]["applied"].clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap();
        assert!(
            applied["sequence"].as_u64().unwrap()
                > stopped["accepted"]["sequence"].as_u64().unwrap()
        );
        assert_eq!(
            question
                .completion(None)
                .await
                .unwrap()
                .unwrap()
                .receipt
                .sequence(),
            applied["sequence"]
        );
        assert_eq!(
            result(call(methods::QUESTION_STOP, stop.clone()).await),
            stopped
        );
        stop["lease"]["run"] = json!(Uuid::now_v7());
        assert!(call(methods::QUESTION_STOP, stop).await["error"].is_object());
        block.store(false, Ordering::SeqCst);
        result(call(methods::FLOW_SUBMIT, submit("/echo source still active")).await);
        tokio::time::timeout(Duration::from_secs(10), host.wait_quiescent())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            result(call(methods::QUESTION_START, start.clone()).await),
            replay
        );
        let mut foreign = status.clone();
        foreign["owner_generation"] = json!(Uuid::now_v7());
        assert_eq!(
            result(call(methods::QUESTION_STATUS, foreign).await)["attempt"]["state"],
            "retired"
        );
        let mut stale = start;
        stale["identity"]["question_id"] = json!(Uuid::now_v7());
        stale["identity"]["request_id"] = json!(Uuid::now_v7());
        stale["expected_head"]["version"] = json!(999);
        assert_eq!(
            call(methods::QUESTION_START, stale).await["error"]["code"],
            atman_proto::history::HISTORY_ADMISSION_ERROR
        );
        let mut fourth = followup.clone();
        fourth["followup_request_id"] = json!(Uuid::now_v7());
        fourth["observed"] = applied.clone();
        fourth["user"]["text"] = json!("Hold this question open for disposal");
        result(call(methods::QUESTION_FOLLOWUP, fourth).await);
        tokio::time::timeout(Duration::from_secs(8), model_entered.notified())
            .await
            .unwrap();
        let disposal =
            json!({"question":stop_status["question"],"discard_request_id":Uuid::now_v7()});
        let mut discard = disposal.clone();
        discard["observed"] = applied;
        let mut fabricated = discard.clone();
        fabricated["observed"]["digest"] = json!("0".repeat(64));
        assert!(call(methods::QUESTION_DISCARD, fabricated).await["error"].is_object());
        assert_eq!(
            result(call(methods::QUESTION_DISCARD_STATUS, disposal.clone()).await)["discard"],
            "unseen"
        );
        assert!(!question.is_stopped());
        let dispose_request =
            JsonRpcRequest::new("dropped-reply", methods::QUESTION_DISCARD, discard.clone());
        // Receive HTTP headers and drop the original command body without consuming its receipt.
        let dropped = client
            .post(format!("http://{endpoint}/rpc"))
            .bearer_auth(TOKEN)
            .json(&dispose_request)
            .send()
            .await
            .unwrap();
        assert!(dropped.status().is_success());
        drop(dropped);
        let disposed = result(call(methods::QUESTION_DISCARD_STATUS, disposal.clone()).await);
        assert_eq!(disposed["discard"], "discarded");
        assert_eq!(
            result(call(methods::QUESTION_DISCARD, discard.clone()).await),
            disposed
        );
        assert!(question.is_stopped());
        assert!(session.loaded_flow(&transient).is_none());
        assert_eq!(session.snapshot().await.unwrap().cursor, before);
        discard["observed"] = replay["receipt"]["accepted"].clone();
        assert!(call(methods::QUESTION_DISCARD, discard).await["error"].is_object());
        let mut foreign_disposal = disposal;
        foreign_disposal["discard_request_id"] = json!(Uuid::now_v7());
        assert_eq!(
            result(call(methods::QUESTION_DISCARD_STATUS, foreign_disposal).await)["discard"],
            "unseen"
        );
        result(
            call(
                methods::FLOW_SUBMIT,
                submit("/echo source still active after discard"),
            )
            .await,
        );
        tokio::time::timeout(Duration::from_secs(10), host.wait_quiescent())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            result(call(methods::QUESTION_STATUS, status).await)["attempt"]["state"],
            "retired"
        );
        assert_eq!(
            result(call(methods::QUESTION_TRANSCRIPT, transcript).await)["transcript"]["state"],
            "retired"
        );
        assert_eq!(
            result(call(methods::QUESTION_FOLLOWUP_STATUS, followup_status).await)["followup"]["state"],
            "retired"
        );
        assert!(call(methods::QUESTION_FOLLOWUP, followup).await["error"].is_object());
        assert!(call(methods::QUESTION_MODEL_CHECK, model_check).await["error"].is_object());
        assert_eq!(
            result(call(methods::QUESTION_EXECUTION, stop_status).await)["execution"]["state"],
            "retired"
        );
        let retained = verify_quick_ask_keep(
            &client,
            endpoint,
            &host,
            &session,
            flow,
            &model_entered,
            &release_model,
        )
        .await;
        if embedded {
            verify_kept_question_submit(
                &client,
                endpoint,
                &host,
                &retained,
                &model_entered,
                &release_model,
            )
            .await;
        }

        gateway_server.shutdown().await.unwrap();
        owner_server.shutdown().await.unwrap();
        drop((gateway_registration, owner_registration));
        gateway.shutdown().await.unwrap();
        owner.shutdown().await.unwrap();
        host.shutdown().await.unwrap();
        if !embedded {
            let restarted = Arc::new(DaemonState::new(data.clone()));
            restarted.set_launcher(Arc::new(
                RunLauncher::new(config.clone(), Some(config), None).unwrap(),
            ));
            let (server, registration) = register(&data, restarted.clone(), HostRole::Daemon).await;
            let endpoint = server.endpoint();
            result(
                request(
                    &client,
                    endpoint,
                    methods::SESSION_OPEN,
                    json!({"session_id":sid}),
                )
                .await,
            );
            let fresh = restarted.runtime_host();
            verify_kept_question_submit(
                &client,
                endpoint,
                &fresh,
                &retained,
                &model_entered,
                &release_model,
            )
            .await;
            server.shutdown().await.unwrap();
            drop(registration);
            restarted.shutdown().await.unwrap();
            fresh.shutdown().await.unwrap();
        }
        provider.abort();
        assert!(provider.await.unwrap_err().is_cancelled());
    }
}

async fn verify_quick_ask_keep(
    client: &reqwest::Client,
    endpoint: std::net::SocketAddr,
    host: &RuntimeHost,
    session: &atman_runtime::flow_runtime::host::SessionHandle,
    flow: Value,
    model_entered: &tokio::sync::Notify,
    release_model: &tokio::sync::Notify,
) -> Value {
    let call = |method, params| request(client, endpoint, method, params);
    let source = result(call(methods::FLOW_CONVERSATION, json!({"flow":flow,"limit":4})).await);
    let question_id = Uuid::now_v7();
    let identity = json!({"source":flow,"question_id":question_id,"request_id":Uuid::now_v7()});
    let status = result(call(methods::QUESTION_STATUS, json!({"identity":identity})).await);
    let start = result(call(methods::QUESTION_START, json!({
        "identity":identity,"owner_generation":status["owner_generation"],"expected_head":source["page"]["head"],
        "observed":source["page"]["source"],"user":{"text":"A question to keep"},"model":"smart"
    })).await);
    tokio::time::timeout(Duration::from_secs(8), model_entered.notified())
        .await
        .unwrap();
    release_model.notify_waiters();
    release_model.notify_one();
    tokio::time::timeout(Duration::from_secs(8), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    let question = json!({
        "identity":identity,"owner_generation":status["owner_generation"],"host_generation":start["receipt"]["launch"]["host_generation"]
    });
    let execution = result(call(methods::QUESTION_EXECUTION, json!({"question":question})).await);
    assert_eq!(execution["execution"]["observation"]["phase"], "idle");
    let lookup = json!({"question":question,"keep_request_id":Uuid::now_v7()});
    let mut intent = lookup.clone();
    intent["expected"] = execution["execution"]["observation"]["cursor"].clone();
    let mut old = intent.clone();
    old["expected"] = start["receipt"]["accepted"].clone();
    assert!(call(methods::QUESTION_KEEP, old).await["error"].is_object());
    assert_eq!(
        result(call(methods::QUESTION_KEEP_STATUS, lookup.clone()).await)["keep"]["state"],
        "unseen"
    );

    let request = JsonRpcRequest::new("dropped-keep", methods::QUESTION_KEEP, intent.clone());
    let dropped = client
        .post(format!("http://{endpoint}/rpc"))
        .bearer_auth(TOKEN)
        .json(&request)
        .send()
        .await
        .unwrap();
    assert!(dropped.status().is_success());
    drop(dropped);
    let published = result(call(methods::QUESTION_KEEP_STATUS, lookup.clone()).await);
    assert_eq!(published["keep"]["state"], "published");
    let receipt = &published["keep"]["receipt"];
    assert_eq!(receipt["source"], intent["expected"]);
    assert_eq!(receipt["flow"]["session_id"], flow["session_id"]);
    assert_eq!(receipt["flow"]["instance_id"], question_id.to_string());
    assert_eq!(
        result(call(methods::QUESTION_KEEP, intent.clone()).await),
        published
    );
    intent["expected"]["digest"] = json!("0".repeat(64));
    assert!(call(methods::QUESTION_KEEP, intent).await["error"].is_object());

    let mut transcript = question.clone();
    transcript["after"] = Value::Null;
    transcript["limit"] = json!(32);
    // The native retirement tick must not release the browser's only original observation.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let original = result(call(methods::QUESTION_TRANSCRIPT, transcript.clone()).await);
    assert_eq!(original["transcript"]["state"], "ready");
    assert!(
        original["transcript"]["batches"]
            .to_string()
            .contains("A question to keep")
    );
    let durable = result(
        call(
            methods::FLOW_CONVERSATION,
            json!({"flow":receipt["flow"],"limit":32}),
        )
        .await,
    );
    assert_eq!(durable["page"]["source"]["cursor"], receipt["destination"]);
    assert!(
        durable["page"]["items"]
            .to_string()
            .contains("A question to keep")
    );
    let mut installed = lookup.clone();
    installed["installed"] = durable["page"]["source"]["cursor"].clone();
    let mut forged = installed.clone();
    forged["installed"]["digest"] = json!("0".repeat(64));
    assert!(call(methods::QUESTION_KEEP_INSTALLED, forged).await["error"].is_object());
    assert_eq!(
        result(call(methods::QUESTION_KEEP_STATUS, lookup.clone()).await),
        published
    );
    let ack = result(call(methods::QUESTION_KEEP_INSTALLED, installed.clone()).await);
    assert_eq!(ack["keep"]["state"], "installed");
    assert_eq!(ack["keep"]["receipt"], *receipt);
    assert_eq!(
        result(call(methods::QUESTION_KEEP_INSTALLED, installed).await),
        ack
    );
    assert_eq!(
        result(call(methods::QUESTION_KEEP_STATUS, lookup).await),
        ack
    );
    assert_eq!(
        result(call(methods::QUESTION_TRANSCRIPT, transcript).await)["transcript"]["state"],
        "retired"
    );
    let id = atman_rt::FlowInstanceId(question_id.into());
    assert!(!session.loaded_flow(&id).unwrap().is_transient());
    let catalog = session.snapshot().await.unwrap().catalog;
    let retained = catalog
        .flows
        .iter()
        .find(|registered| registered.id == id)
        .unwrap();
    assert!(retained.retention.is_some());
    assert!(retained.retention_resume.is_none());
    receipt.clone()
}

async fn verify_kept_question_submit(
    client: &reqwest::Client,
    endpoint: std::net::SocketAddr,
    host: &RuntimeHost,
    retained: &Value,
    model_entered: &tokio::sync::Notify,
    release_model: &tokio::sync::Notify,
) {
    let call = |method, params| request(client, endpoint, method, params);
    let flow = retained["flow"].clone();
    let session = host
        .session(flow["session_id"].as_str().unwrap().parse().unwrap())
        .unwrap();
    let id = atman_rt::FlowInstanceId(
        flow["instance_id"]
            .as_str()
            .unwrap()
            .parse::<Uuid>()
            .unwrap()
            .into(),
    );
    let registration = |catalog: atman_runtime::flow_store::SessionProjection| {
        catalog
            .flows
            .into_iter()
            .find(|flow| flow.id == id)
            .unwrap()
    };
    let page = result(call(methods::FLOW_CONVERSATION, json!({"flow":flow,"limit":32})).await);
    assert_eq!(page["page"]["source"]["cursor"], retained["destination"]);
    assert!(
        registration(session.snapshot().await.unwrap().catalog)
            .retention_resume
            .is_none()
    );
    assert!(
        session
            .scheduling_status(id.clone())
            .await
            .unwrap()
            .is_none()
    );
    let input = json!({"flow":flow,"request_id":Uuid::now_v7(),"expected_head":page["page"]["head"],"user":{"text":"Continue the retained question"}});
    let response = client
        .post(format!("http://{endpoint}/rpc"))
        .bearer_auth(TOKEN)
        .json(&JsonRpcRequest::new(
            "discard-submit-reply",
            methods::FLOW_SUBMIT,
            input.clone(),
        ))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    drop(response);
    let replay = result(call(methods::FLOW_SUBMIT, input.clone()).await);
    tokio::time::timeout(Duration::from_secs(8), model_entered.notified())
        .await
        .unwrap();
    let verified = registration(session.snapshot().await.unwrap().catalog)
        .retention_resume
        .expect("the retained catalog requires a newly accepted input");
    assert_eq!(
        verified.message_id.to_string(),
        replay["acceptance"]["message_id"]
    );
    assert!(verified.receipt.sequence() > retained["destination"]["sequence"].as_u64().unwrap());
    release_model.notify_waiters();
    release_model.notify_one();
    tokio::time::timeout(Duration::from_secs(8), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    let page = result(call(methods::FLOW_CONVERSATION, json!({"flow":flow,"limit":32})).await);
    let body = page["page"]["items"].to_string();
    assert_eq!(body.matches("Continue the retained question").count(), 1);
    assert_eq!(body.matches("Fixture answer").count(), 2);
    let settled = result(call(methods::FLOW_SUBMIT, input).await);
    assert_eq!(
        settled["acceptance"]["disposition"]["state"],
        "input_committed"
    );
    assert_eq!(
        settled["acceptance"]["message_id"],
        replay["acceptance"]["message_id"]
    );
    assert_eq!(
        settled["acceptance"]["receipt"],
        replay["acceptance"]["receipt"]
    );
}

#[tokio::test]
async fn project_list_observes_shared_storage_while_pins_route_to_the_profile_owner() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let config = directory.path().join("config");
    let owner = tui(&data, &config, Uuid::now_v7());
    let host = owner.runtime_host();
    host.open_profile(&data).await.unwrap();
    let project = atman_runtime::project_catalog::ProjectCatalogStore::new(&data)
        .register(&config, chrono::Utc::now())
        .unwrap();
    let (owner_server, owner_registration) = register(&data, owner.clone(), HostRole::Tui).await;
    let gateway = Arc::new(DaemonState::new(data.clone()));
    gateway.configure_host_routing(TOKEN.into()).unwrap();
    let (gateway_server, gateway_registration) =
        register(&data, gateway.clone(), HostRole::Daemon).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let listed = result(
        request(
            &client,
            gateway_server.endpoint(),
            methods::PROJECT_LIST,
            json!({}),
        )
        .await,
    );
    assert_eq!(listed["projects"][0]["fingerprint"], project.fingerprint);
    let pin_request = json!({
        "fingerprint": project.fingerprint,
        "expected_revision": listed["revision"],
        "pinned": true,
    });
    let pin = result(
        request(
            &client,
            gateway_server.endpoint(),
            methods::PROJECT_SET_PINNED,
            pin_request.clone(),
        )
        .await,
    );
    assert_eq!(pin["project"]["pinned"], true);
    assert_ne!(pin["revision"], listed["revision"]);
    assert!(gateway.runtime_host().profile_handle().is_none());
    assert!(host.sessions().is_empty());

    owner_server.shutdown().await.unwrap();
    let unavailable_pin = request(
        &client,
        gateway_server.endpoint(),
        methods::PROJECT_SET_PINNED,
        pin_request,
    )
    .await;
    assert!(unavailable_pin["result"].is_null());
    assert!(
        unavailable_pin["error"]["message"]
            .as_str()
            .unwrap()
            .contains("host identity observation failed")
    );
    let listed_without_owner = result(
        request(
            &client,
            gateway_server.endpoint(),
            methods::PROJECT_LIST,
            json!({}),
        )
        .await,
    );
    assert_eq!(listed_without_owner["projects"][0], pin["project"]);
    assert_eq!(listed_without_owner["revision"], pin["revision"]);
    assert!(gateway.runtime_host().profile_handle().is_none());
    drop((owner_registration, gateway_registration));
    gateway_server.shutdown().await.unwrap();
    gateway.shutdown().await.unwrap();
    host.shutdown().await.unwrap();
}

async fn flow(session: &atman_runtime::flow_runtime::host::SessionHandle) -> FlowRef {
    let address = FlowRef {
        session_id: session.id().into(),
        instance_id: FlowInstanceId::now(),
    };
    session
        .create_flow(FlowDescriptor {
            address: address.clone(),
            creation_request: Uuid::now_v7(),
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: "main.at".into(),
                bundle: atman_runtime::source_program::RevisionBundle {
                    entry: "main.at".into(),
                    sources: [("main.at".into(), "flow main() { }".into())].into(),
                    edges: Default::default(),
                },
            }
            .into(),
        })
        .await
        .unwrap();
    address
}

#[tokio::test]
async fn flow_drafts_routes_read_only_to_the_existing_worker_and_exact_head() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let owner = tui(&data, &directory.path().join("config"), Uuid::now_v7());
    let host = owner.runtime_host();
    let id = Uuid::now_v7();
    let session = host
        .create_session(
            &data,
            SessionDescriptor {
                id,
                title: "Live source".into(),
                project: None,
                configuration: Default::default(),
            },
        )
        .await
        .unwrap();
    let address = flow(&session).await;
    let worker = session.loaded_flow(&address.instance_id).unwrap();
    let original = worker.status().await.unwrap();
    let head = original.history_head.unwrap();
    let wire = json!({"session_id": id, "instance_id": address.instance_id.0.0});
    let (owner_server, owner_registration) = register(&data, owner.clone(), HostRole::Tui).await;
    let gateway = Arc::new(DaemonState::new(data.clone()));
    gateway.configure_host_routing(TOKEN.into()).unwrap();
    let (gateway_server, gateway_registration) =
        register(&data, gateway.clone(), HostRole::Daemon).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let params = json!({"flow": wire, "head": head});
    let gateway_address = gateway_server.endpoint();
    let ready = result(
        request(
            &client,
            gateway_address,
            methods::FLOW_DRAFTS,
            params.clone(),
        )
        .await,
    );
    assert_eq!(ready["state"], "ready");
    assert_eq!(ready["snapshot"]["flow"], params["flow"]);
    assert_eq!(ready["snapshot"]["head"], params["head"]);
    assert_eq!(ready["snapshot"]["after"], json!(original.cursor));
    assert_eq!(
        ready["snapshot"]["host_generation"],
        json!(host.generation())
    );
    assert_ne!(
        ready["snapshot"]["host_generation"],
        owner.daemon_generation()
    );
    assert_eq!(ready["snapshot"]["calls"], json!([]));
    assert_eq!(worker.status().await.unwrap().cursor, original.cursor);

    let mut waiting_params = params.clone();
    let snapshot = &ready["snapshot"];
    waiting_params["known"] = json!({
        "host_generation": snapshot["host_generation"],
        "worker_incarnation": snapshot["worker_incarnation"],
        "revision": snapshot["revision"],
    });
    let read_after = request(
        &client,
        gateway_address,
        methods::FLOW_DRAFTS,
        waiting_params,
    );
    tokio::pin!(read_after);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut read_after)
            .await
            .is_err()
    );
    let state = worker.state().await.unwrap();
    let receipt = worker
        .transition(state.revision(), FlowTransition::Activated)
        .await
        .unwrap();
    let updated = result(
        tokio::time::timeout(Duration::from_secs(2), read_after)
            .await
            .unwrap(),
    );
    assert_eq!(updated["state"], "ready");
    assert_eq!(updated["snapshot"]["after"], json!(receipt));
    assert_eq!(
        updated["snapshot"]["worker_incarnation"],
        snapshot["worker_incarnation"]
    );
    assert_ne!(updated["snapshot"]["revision"], snapshot["revision"]);

    let mut stale = params.clone();
    stale["head"]["version"] = json!(head.version.get() + 1);
    let changed = result(request(&client, gateway_address, methods::FLOW_DRAFTS, stale).await);
    assert_eq!(changed["state"], "history_changed");
    assert!(changed.get("snapshot").is_none());
    let mut absent = params.clone();
    absent["flow"]["instance_id"] = json!(Uuid::now_v7());
    let unavailable = result(request(&client, gateway_address, methods::FLOW_DRAFTS, absent).await);
    assert_eq!(unavailable["state"], "unavailable");
    let denied = atman_daemon::dispatch_as(
        gateway.clone(),
        JsonRpcRequest::new(1, methods::FLOW_DRAFTS, params),
        "unrecognized",
    )
    .await;
    assert!(denied.error.unwrap().message.contains("permission denied"));
    assert!(gateway.runtime_host().sessions().is_empty());

    drop((owner_registration, gateway_registration));
    gateway_server.shutdown().await.unwrap();
    owner_server.shutdown().await.unwrap();
    gateway.shutdown().await.unwrap();
    owner.shutdown().await.unwrap();
}

#[tokio::test]
async fn human_rpc_routes_bootstrap_and_submission_to_the_original_session_owner() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let config = directory.path().join("config");
    let source = tui(&data, &config, Uuid::now_v7());
    std::fs::create_dir_all(config.join("commands")).unwrap();
    std::fs::write(config.join("config.toml"), "").unwrap();
    std::fs::write(config.join("routes.at"), "default_route { flow: echo }\n").unwrap();
    std::fs::write(config.join("commands/echo.at"), "flow echo(input: string) -> string { contract { invocation { user_message: input } } return final.answer(message: input) }").unwrap();
    let host = source.runtime_host();
    let id = Uuid::now_v7();
    let session = host
        .create_session(
            &data,
            SessionDescriptor {
                id,
                title: "Shared human owner".into(),
                project: None,
                configuration: atman_runtime::flow_store::SessionConfiguration::Atman(
                    atman_runtime::config_hub::ConfigHub::from_config_dir(&config).source(),
                ),
            },
        )
        .await
        .unwrap();
    let catalog = session.snapshot().await.unwrap().cursor;
    let (source_server, source_registration) = register(&data, source.clone(), HostRole::Tui).await;
    let gateway = Arc::new(DaemonState::new(data.clone()));
    gateway.configure_host_routing(TOKEN.into()).unwrap();
    let (gateway_server, gateway_registration) =
        register(&data, gateway.clone(), HostRole::Daemon).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let bootstrap = json!({"session_id": id, "expected_catalog": catalog});
    let launched = result(
        request(
            &client,
            gateway_server.endpoint(),
            methods::SESSION_BOOTSTRAP,
            bootstrap.clone(),
        )
        .await,
    );
    assert_eq!(launched["flow"]["instance_id"], json!(id));
    let flow: atman_proto::flows::FlowAddress =
        serde_json::from_value(launched["flow"].clone()).unwrap();
    let authored =
        json!({"flow": flow, "request_id": Uuid::now_v7(), "user": {"text": "x=y\nsecond line"}});
    let accepted = result(
        request(
            &client,
            gateway_server.endpoint(),
            methods::FLOW_SUBMIT,
            authored.clone(),
        )
        .await,
    );
    tokio::time::timeout(Duration::from_secs(10), host.wait_quiescent())
        .await
        .unwrap()
        .unwrap();
    let worker = host
        .flow(&FlowRef {
            session_id: id.into(),
            instance_id: atman_rt::FlowInstanceId(id.into()),
        })
        .await
        .unwrap();
    assert!(
        worker
            .context()
            .messages_full()
            .iter()
            .any(|message| message.text_concat() == "x=y\nsecond line")
    );
    let cursor = worker.status().await.unwrap().cursor;
    let retried = result(
        request(
            &client,
            gateway_server.endpoint(),
            methods::FLOW_SUBMIT,
            authored,
        )
        .await,
    );
    assert_eq!(
        retried["acceptance"]["message_id"],
        accepted["acceptance"]["message_id"]
    );
    assert_eq!(
        retried["acceptance"]["receipt"],
        accepted["acceptance"]["receipt"]
    );
    assert_eq!(
        result(
            request(
                &client,
                gateway_server.endpoint(),
                methods::SESSION_BOOTSTRAP,
                bootstrap
            )
            .await
        ),
        launched
    );
    assert_eq!(worker.status().await.unwrap().cursor, cursor);
    assert_eq!(session.snapshot().await.unwrap().catalog.flows.len(), 1);
    assert!(!gateway.runtime_host().sessions().contains(&id));
    assert!(RuntimeStore::open(&data).unwrap().open_session(id).is_err());
    gateway_server.shutdown().await.unwrap();
    source_server.shutdown().await.unwrap();
    drop((source_registration, gateway_registration));
    gateway.shutdown().await.unwrap();
    source.shutdown().await.unwrap();
}

#[tokio::test]
async fn routing_uses_the_original_session_profile_and_resumable_event_source() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let source = tui(
        &data,
        &directory.path().join("source-config"),
        Uuid::now_v7(),
    );
    let host = source.runtime_host();
    let id = Uuid::now_v7();
    let session = host
        .create_session(
            &data,
            SessionDescriptor {
                id,
                title: "Original owner".into(),
                project: None,
                configuration: Default::default(),
            },
        )
        .await
        .unwrap();
    host.open_profile(&data).await.unwrap();
    let first = flow(&session).await;
    let second = flow(&session).await;
    let (source_server, source_registration) = register(&data, source.clone(), HostRole::Tui).await;
    let gateway = Arc::new(DaemonState::new(data.clone()));
    gateway.configure_host_routing(TOKEN.into()).unwrap();
    let (gateway_server, gateway_registration) =
        register(&data, gateway.clone(), HostRole::Daemon).await;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    result(
        request(
            &client,
            gateway_server.endpoint(),
            methods::SESSION_OPEN,
            json!({"session_id":id}),
        )
        .await,
    );
    let selected = result(
        request(
            &client,
            gateway_server.endpoint(),
            methods::SESSION_SELECT_FLOW,
            json!({"flow":{"session_id":id,"instance_id":second.instance_id.0.0}}),
        )
        .await,
    );
    assert_eq!(selected["default_flow"], json!(second.instance_id.0.0));
    assert_eq!(
        session.snapshot().await.unwrap().catalog.default_flow,
        Some(second.instance_id.clone())
    );
    assert!(!gateway.runtime_host().sessions().contains(&id));
    assert!(RuntimeStore::open(&data).unwrap().open_session(id).is_err());
    let profile = result(
        request(
            &client,
            gateway_server.endpoint(),
            methods::MANAGEMENT_GET,
            json!({}),
        )
        .await,
    );
    assert!(profile["profile"].is_object());
    assert!(
        gateway.runtime_host().profile_handle().is_none(),
        "gateway must not open another profile owner"
    );

    let status = host.flow(&first).await.unwrap().status().await.unwrap();
    let cursor = status.cursor.require_durable().unwrap();
    let cursor = atman_proto::flows::FlowCursor {
        journal_id: cursor.journal_id,
        sequence: cursor.sequence,
        digest: cursor.digest.clone(),
    };
    let mut stream = client
        .get(format!("http://{}/events", gateway_server.endpoint()))
        .query(&[
            ("session_id", id.to_string()),
            ("instance_id", first.instance_id.0.0.to_string()),
            ("after", "invalid-old-url".into()),
        ])
        .header("Last-Event-ID", cursor.to_string())
        .bearer_auth(TOKEN)
        .send()
        .await
        .unwrap();
    assert!(
        stream.status().is_success(),
        "Last-Event-ID must remain authoritative across the gateway"
    );
    let chunk = stream.chunk().await.unwrap().unwrap();
    assert!(String::from_utf8_lossy(&chunk).contains("retry:"));
    tokio::time::timeout(Duration::from_secs(5), gateway_server.shutdown())
        .await
        .unwrap()
        .unwrap();
    drop(stream);
    assert!(host.flow(&first).await.unwrap().status().await.is_ok());
    assert!(!source.is_stopping());
    source_server.shutdown().await.unwrap();
    drop((source_registration, gateway_registration));
    gateway.shutdown().await.unwrap();
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn daemon_creation_prefers_the_latest_authenticated_tui_and_retries_the_original_owner() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let older = tui(&data, &directory.path().join("older"), Uuid::from_u128(100));
    let newer = tui(&data, &directory.path().join("newer"), Uuid::from_u128(200));
    let (older_server, older_registration) = register(&data, older.clone(), HostRole::Tui).await;
    let (newer_server, newer_registration) = register(&data, newer.clone(), HostRole::Tui).await;
    let gateway = Arc::new(DaemonState::new(data.clone()));
    gateway.configure_host_routing(TOKEN.into()).unwrap();
    let server = EmbeddedRpcServer::bind(gateway.clone(), TOKEN.into())
        .await
        .unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let id = Uuid::now_v7();
    let params = json!({"session_id":id,"title":"Created at TUI","project_root":null});
    let created = result(
        request(
            &client,
            server.endpoint(),
            methods::SESSION_CREATE,
            params.clone(),
        )
        .await,
    );
    assert!(newer.runtime_host().sessions().contains(&id));
    assert!(!older.runtime_host().sessions().contains(&id));
    assert!(!gateway.runtime_host().sessions().contains(&id));
    newer_registration
        .publish_sessions(newer.runtime_host().sessions())
        .unwrap();
    let latest = tui(
        &data,
        &directory.path().join("latest"),
        Uuid::from_u128(300),
    );
    let (latest_server, latest_registration) = register(&data, latest.clone(), HostRole::Tui).await;
    let retried =
        result(request(&client, server.endpoint(), methods::SESSION_CREATE, params).await);
    assert_eq!(
        retried, created,
        "creation retry must retain the original catalog and owner"
    );
    assert!(!latest.runtime_host().sessions().contains(&id));
    server.shutdown().await.unwrap();
    older_server.shutdown().await.unwrap();
    newer_server.shutdown().await.unwrap();
    latest_server.shutdown().await.unwrap();
    drop((older_registration, newer_registration, latest_registration));
    gateway.shutdown().await.unwrap();
    older.runtime_host().shutdown().await.unwrap();
    newer.runtime_host().shutdown().await.unwrap();
    latest.runtime_host().shutdown().await.unwrap();
}

#[tokio::test]
async fn wrong_credentials_and_replaced_endpoint_generations_never_open_another_owner() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let source = tui(&data, &directory.path().join("config"), Uuid::now_v7());
    let id = Uuid::now_v7();
    source
        .runtime_host()
        .create_session(
            &data,
            SessionDescriptor {
                id,
                title: "Held".into(),
                project: None,
                configuration: Default::default(),
            },
        )
        .await
        .unwrap();
    let (source_server, registration) = register(&data, source.clone(), HostRole::Tui).await;
    let wrong = Arc::new(DaemonState::new(data.clone()));
    wrong
        .configure_host_routing("wrong-private-credential".into())
        .unwrap();
    let wrong_server = EmbeddedRpcServer::bind(wrong.clone(), TOKEN.into())
        .await
        .unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let rejected = request(
        &client,
        wrong_server.endpoint(),
        methods::SESSION_OPEN,
        json!({"session_id":id}),
    )
    .await;
    assert!(rejected["error"].is_object());
    assert!(!rejected.to_string().contains("wrong-private-credential"));
    assert!(wrong.runtime_host().sessions().is_empty());
    wrong_server.shutdown().await.unwrap();
    wrong.shutdown().await.unwrap();
    drop(registration);

    let stale = HostRegistration::register(
        &data,
        HostRecord {
            generation: Uuid::now_v7(),
            process_id: std::process::id(),
            role: HostRole::Tui,
            endpoint: source_server.endpoint(),
            sessions: vec![id],
            presentation: atman_proto::hosts::HostPresentation::Attached,
        },
    )
    .unwrap();
    let gateway = Arc::new(DaemonState::new(data.clone()));
    gateway.configure_host_routing(TOKEN.into()).unwrap();
    let server = EmbeddedRpcServer::bind(gateway.clone(), TOKEN.into())
        .await
        .unwrap();
    let rejected = request(
        &client,
        server.endpoint(),
        methods::SESSION_OPEN,
        json!({"session_id":id}),
    )
    .await;
    assert!(rejected["error"].is_object());
    assert!(gateway.runtime_host().sessions().is_empty());
    assert!(source.runtime_host().sessions().contains(&id));
    server.shutdown().await.unwrap();
    source_server.shutdown().await.unwrap();
    drop(stale);
    gateway.shutdown().await.unwrap();
    source.runtime_host().shutdown().await.unwrap();
}

#[tokio::test]
async fn an_unknown_forwarded_write_is_sent_once_with_its_original_request_identity() {
    #[derive(Clone)]
    struct Remote {
        generation: Uuid,
        session: Uuid,
        attempts: Arc<AtomicUsize>,
        requests: Arc<Mutex<Vec<Value>>>,
    }
    async fn handle(
        axum::extract::State(remote): axum::extract::State<Remote>,
        headers: axum::http::HeaderMap,
        axum::Json(request): axum::Json<JsonRpcRequest>,
    ) -> axum::response::Response {
        use axum::response::IntoResponse;
        assert_eq!(
            headers.get("authorization").unwrap(),
            format!("Bearer {TOKEN}").as_str()
        );
        assert_eq!(
            headers.get(GENERATION_HEADER).unwrap(),
            remote.generation.to_string().as_str()
        );
        if request.method == methods::HOST_DESCRIBE {
            let mut response = axum::Json(JsonRpcResponse::ok(request.id, json!({
                "generation":remote.generation.to_string(),"runtime_generation":remote.generation,"role":"tui","sessions":[remote.session],"profile_owner":false,"presentation":"attached",
            }))).into_response();
            response.headers_mut().insert(
                GENERATION_HEADER,
                remote.generation.to_string().parse().unwrap(),
            );
            return response;
        }
        remote.attempts.fetch_add(1, Ordering::SeqCst);
        remote.requests.lock().unwrap().push(json!(request));
        axum::http::StatusCode::SERVICE_UNAVAILABLE.into_response()
    }
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let remote = Remote {
        generation: Uuid::now_v7(),
        session: Uuid::now_v7(),
        attempts: Default::default(),
        requests: Default::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let registration = HostRegistration::register(
        &data,
        HostRecord {
            generation: remote.generation,
            process_id: std::process::id(),
            role: HostRole::Tui,
            endpoint: listener.local_addr().unwrap(),
            sessions: vec![remote.session],
            presentation: atman_proto::hosts::HostPresentation::Attached,
        },
    )
    .unwrap();
    let stopped = tokio_util::sync::CancellationToken::new();
    let shutdown = stopped.clone();
    let app = axum::Router::new()
        .route("/rpc", axum::routing::post(handle))
        .with_state(remote.clone());
    let serving = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { shutdown.cancelled().await })
            .await
            .unwrap()
    });
    let gateway = Arc::new(DaemonState::new(data));
    gateway.configure_host_routing(TOKEN.into()).unwrap();
    let server = EmbeddedRpcServer::bind(gateway.clone(), TOKEN.into())
        .await
        .unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let request_id = Uuid::now_v7();
    let params = json!({"flow":{"session_id":remote.session,"instance_id":Uuid::now_v7()},"request_id":request_id,"arguments":{"text":"retained payload"}});
    let response = request(
        &client,
        server.endpoint(),
        methods::FLOW_INVOKE,
        params.clone(),
    )
    .await;
    assert!(response["error"].is_object());
    assert_eq!(response["id"], "gateway-request");
    assert_eq!(remote.attempts.load(Ordering::SeqCst), 1);
    {
        let requests = remote.requests.lock().unwrap();
        assert_eq!(requests[0]["id"], "gateway-request");
        assert_eq!(requests[0]["params"], params);
    }
    assert!(gateway.runtime_host().sessions().is_empty());
    server.shutdown().await.unwrap();
    gateway.shutdown().await.unwrap();
    stopped.cancel();
    serving.await.unwrap();
    drop(registration);
}
