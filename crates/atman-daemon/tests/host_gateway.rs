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
