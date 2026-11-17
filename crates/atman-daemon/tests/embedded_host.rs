use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use atman_daemon::{
    DaemonState, EmbeddedRpcServer, SharedHostServices, SharedPrompts, dispatch_as,
};
use atman_proto::{JsonRpcRequest, methods};
use atman_runtime::{
    AtmanRuntime, AtmanRuntimeOptions,
    config_hub::ConfigHub,
    event::EventSink,
    flow_runtime::{
        catalog::SessionCatalog,
        host::{HostServices, RuntimeHost},
    },
    flow_store::{RuntimeStore, SessionConfiguration, SessionDescriptor, inbox::SenderIdentity},
    permission::PermissionClients,
    rendezvous::{PromptId, PromptResolver},
};
use serde_json::{Value, json};
use tokio::sync::oneshot;
use uuid::Uuid;

async fn rpc(
    client: &reqwest::Client,
    endpoint: std::net::SocketAddr,
    method: &str,
    params: Value,
) -> Value {
    let response = client
        .post(format!("http://{endpoint}/rpc"))
        .bearer_auth("secret")
        .json(&JsonRpcRequest::new(1, method, params))
        .send()
        .await
        .unwrap();
    assert!(response.status().is_success());
    let response: Value = response.json().await.unwrap();
    assert!(response["error"].is_null(), "{method}: {response}");
    response["result"].clone()
}

#[tokio::test]
async fn native_and_http_share_ownership_and_listener_shutdown_leaves_execution_alive() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    let config = directory.path().join("config");
    std::fs::create_dir(&config).unwrap();
    std::fs::write(config.join("config.toml"), "[trust]\nmode = \"reckless\"\n").unwrap();
    let hub = ConfigHub::from_config_dir(&config);
    let permissions = Arc::new(PermissionClients::default());
    let host = RuntimeHost::with_services(HostServices {
        permission_clients: Arc::downgrade(&permissions),
        ..Default::default()
    });
    let id = Uuid::now_v7();
    let session = host
        .create_session(
            &data,
            SessionDescriptor {
                id,
                title: "Native owner".into(),
                project: None,
                configuration: SessionConfiguration::Atman(hub.source()),
            },
        )
        .await
        .unwrap();
    let profile = host.open_profile(&data).await.unwrap();
    let original_executor = AtmanRuntime::build(AtmanRuntimeOptions {
        events: EventSink::new(),
        mock: true,
        config: hub,
        project: None,
        home_dir: None,
        workspace_generation: "shared-host-test".into(),
    })
    .await
    .unwrap()
    .executor;
    let native_resolutions = Arc::new(AtomicUsize::new(0));
    let resolutions = native_resolutions.clone();
    session
        .configure_executor(move || {
            Arc::new(move |_| {
                resolutions.fetch_add(1, Ordering::SeqCst);
                let executor = original_executor.clone();
                Box::pin(async move { Ok(executor) })
            })
        })
        .unwrap();
    let operator = Uuid::now_v7();
    let state = Arc::new(DaemonState::with_shared_host(
        data.clone(),
        "native-generation".into(),
        SharedHostServices {
            host: host.clone(),
            permissions,
            operator,
            prompts: None,
        },
    ));
    let server = EmbeddedRpcServer::bind(state.clone(), "secret".into())
        .await
        .unwrap();
    assert!(server.endpoint().ip().is_loopback());
    let endpoint = server.endpoint();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let rejected = client
        .post(format!("http://{endpoint}/rpc"))
        .bearer_auth("wrong")
        .json(&JsonRpcRequest::new(
            1,
            methods::SESSION_OPEN,
            json!({"session_id":id}),
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), reqwest::StatusCode::UNAUTHORIZED);
    let opened = rpc(
        &client,
        endpoint,
        methods::SESSION_OPEN,
        json!({"session_id":id}),
    )
    .await;
    assert_eq!(
        opened["catalog"],
        json!(session.snapshot().await.unwrap().cursor)
    );
    let observed_profile = rpc(&client, endpoint, methods::MANAGEMENT_GET, json!({})).await;
    assert_eq!(
        observed_profile["profile"],
        json!(profile.snapshot().await.unwrap().profile)
    );
    assert!(
        RuntimeStore::open(&data).unwrap().open_session(id).is_err(),
        "another writer must remain excluded"
    );

    let path = directory.path().join("active.at");
    std::fs::write(&path, "flow main(label: string) -> any { loop { yield } }").unwrap();
    let flow = json!({"session_id":id,"instance_id":Uuid::now_v7()});
    rpc(&client, endpoint, methods::FLOW_LAUNCH, json!({
        "flow":flow,"request_id":Uuid::now_v7(),"expected_catalog_sequence":opened["catalog"]["sequence"],
        "path":path,"entry":"main","arguments":{"label":"running"},
    })).await;
    let address = atman_runtime::flow_store::FlowRef {
        session_id: id.into(),
        instance_id: atman_rt::FlowInstanceId(
            flow["instance_id"]
                .as_str()
                .unwrap()
                .parse::<Uuid>()
                .unwrap()
                .into(),
        ),
    };
    let worker = host.flow(&address).await.unwrap();
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if matches!(
                worker.state().await.unwrap().state(),
                atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(_))
            ) {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(
        native_resolutions.load(Ordering::SeqCst) > 0,
        "RPC must use the native executor resolver"
    );
    let request_id = Uuid::now_v7();
    let accepted = rpc(
        &client,
        endpoint,
        methods::FLOW_INVOKE,
        json!({"flow":flow,"request_id":request_id,"arguments":{"label":"queued"}}),
    )
    .await;
    let status = rpc(
        &client,
        endpoint,
        methods::FLOW_INPUT_STATUS,
        json!({"flow":flow,"request_id":request_id}),
    )
    .await;
    assert_eq!(
        status["observation"]["message_id"],
        accepted["acceptance"]["message_id"]
    );
    let observation = SessionCatalog::new(Some(host.clone()), Some(data.clone()))
        .observe_input_request(&address, SenderIdentity::User(operator), request_id, |_| {
            Ok(())
        })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        json!(observation.message_id),
        accepted["acceptance"]["message_id"]
    );

    let stream = client
        .get(format!(
            "http://{endpoint}/events?session_id={id}&instance_id={}",
            flow["instance_id"].as_str().unwrap()
        ))
        .bearer_auth("secret")
        .send()
        .await
        .unwrap();
    assert!(stream.status().is_success());
    tokio::time::timeout(Duration::from_secs(5), server.shutdown())
        .await
        .unwrap()
        .unwrap();
    drop(stream);
    assert!(
        !state.is_stopping(),
        "stopping one listener must not stop other service adapters"
    );
    state.shutdown().await.unwrap();
    assert!(
        matches!(
            worker.state().await.unwrap().state(),
            atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(_))
        ),
        "borrowed RPC shutdown must not cancel native execution"
    );
    tokio::time::timeout(Duration::from_secs(10), host.shutdown())
        .await
        .unwrap()
        .unwrap();
}

struct NativePrompts {
    owner: Uuid,
    pending: Mutex<HashMap<Uuid, oneshot::Sender<Value>>>,
}

impl PromptResolver for NativePrompts {
    fn register(&self, id: PromptId) -> oneshot::Receiver<Value> {
        let (sender, receiver) = oneshot::channel();
        self.pending.lock().unwrap().insert(id.0, sender);
        receiver
    }
    fn drop_pending(&self, id: &PromptId) {
        self.pending.lock().unwrap().remove(&id.0);
    }
}

#[tokio::test]
async fn shared_prompt_replies_keep_the_native_owner_and_original_waiter() {
    let directory = tempfile::tempdir().unwrap();
    let owner = Uuid::now_v7();
    let native = Arc::new(NativePrompts {
        owner,
        pending: Default::default(),
    });
    let original = native.clone();
    let prompts = SharedPrompts::new(native.clone(), move |id, value, session| {
        let original = original.clone();
        Box::pin(async move {
            if session.is_some_and(|id| id != original.owner) {
                return Ok(false);
            }
            Ok(original
                .pending
                .lock()
                .unwrap()
                .remove(&id)
                .is_some_and(|sender| sender.send(value).is_ok()))
        })
    });
    let id = PromptId::now();
    let answer = native.register(id);
    let host = RuntimeHost::default();
    let state = Arc::new(DaemonState::with_shared_host(
        directory.path().to_owned(),
        "prompt-generation".into(),
        SharedHostServices {
            host: host.clone(),
            permissions: Arc::new(PermissionClients::default()),
            operator: Uuid::now_v7(),
            prompts: Some(prompts),
        },
    ));
    let forged = dispatch_as(
        state.clone(),
        JsonRpcRequest::new(
            1,
            methods::RESOLVE_PROMPT,
            json!({"prompt_id":id.0,"answer":"forged","session_id":owner}),
        ),
        "untrusted",
    )
    .await;
    assert!(forged.error.is_some());
    let server = EmbeddedRpcServer::bind(state.clone(), "secret".into())
        .await
        .unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let refused = rpc(
        &client,
        server.endpoint(),
        methods::RESOLVE_PROMPT,
        json!({"prompt_id":id.0,"answer":"wrong source","session_id":Uuid::now_v7()}),
    )
    .await;
    assert_eq!(refused["resolved"], false);
    let resolved = rpc(
        &client,
        server.endpoint(),
        methods::RESOLVE_PROMPT,
        json!({"prompt_id":id.0,"answer":"original answer","session_id":owner}),
    )
    .await;
    assert_eq!(resolved["resolved"], true);
    assert_eq!(answer.await.unwrap(), "original answer");
    let duplicate = rpc(
        &client,
        server.endpoint(),
        methods::RESOLVE_PROMPT,
        json!({"prompt_id":id.0,"answer":"duplicate","session_id":owner}),
    )
    .await;
    assert_eq!(duplicate["resolved"], false);
    server.shutdown().await.unwrap();
    state.shutdown().await.unwrap();
    host.shutdown().await.unwrap();
}
