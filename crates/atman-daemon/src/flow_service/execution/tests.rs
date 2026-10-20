use std::{sync::Arc, time::Duration};

use atman_proto::{JsonRpcRequest, methods};
use atman_runtime::{config_hub::ConfigHub, storage::ProjectContext};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{DaemonState, dispatch, flow_service::service, run::RunLauncher};

async fn rpc(state: &Arc<DaemonState>, method: &str, params: Value) -> Value {
    let response = dispatch(state.clone(), JsonRpcRequest::new(1, method, params)).await;
    assert!(response.error.is_none(), "{method}: {:?}", response.error);
    response.result.unwrap()
}

async fn completed(state: &Arc<DaemonState>, flow: &Value, previous: &Value) -> Value {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let view = rpc(state, methods::FLOW_GET, json!({"flow": flow})).await;
            let completion = &view["last_completion"];
            if !completion.is_null() && &completion["lease"]["turn"] != previous {
                assert_eq!(completion["result"]["kind"], "returned", "{view}");
                return completion.clone();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("flow should complete")
}

#[tokio::test]
async fn reassigned_sessions_launch_and_restore_each_project_scope_through_rpc() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    let alpha = dir.path().join("alpha");
    let beta = dir.path().join("beta");
    for path in [&config, &alpha, &beta] {
        std::fs::create_dir_all(path).unwrap();
    }
    std::fs::write(config.join("config.toml"), "").unwrap();
    for (root, name) in [(&alpha, "alpha"), (&beta, "beta")] {
        std::fs::create_dir_all(root.join(".atman/lib")).unwrap();
        std::fs::write(root.join("AGENTS.md"), format!("# project-rule\n{name}")).unwrap();
        std::fs::write(
            root.join(".atman/lib/helper.at"),
            format!(r#"pub flow tag() -> string {{ return "{name}" }}"#),
        )
        .unwrap();
    }
    let path = config.join("entry.at");
    std::fs::write(
        &path,
        r#"use "project:helper.at"::tag
        flow main() -> string {
            label = tag().await
            rule = rule.fetch(name: "project-rule")
            return label + "|" + rule
        }"#,
    )
    .unwrap();
    let data = dir.path().join("data");
    let state = Arc::new(DaemonState::new(data.clone()));
    state.set_launcher(Arc::new(
        RunLauncher::new(alpha.clone(), Some(config.clone()), None).unwrap(),
    ));
    let id = Uuid::now_v7();
    rpc(
        &state,
        methods::SESSION_CREATE,
        json!({
            "session_id": id, "title": "Scoped execution", "project_root": alpha
        }),
    )
    .await;
    let hub = ConfigHub::from_config_dir(&config);
    let session = service(&state).session(id).unwrap();
    let initial = session.snapshot().await.unwrap();
    let prepared = json!({"session_id": id, "instance_id": Uuid::now_v7()});
    let prepared_request = Uuid::now_v7();
    let program = atman_runtime::source_program::load_program(
        &path,
        &initial.catalog.descriptor.source_roots(),
    )
    .unwrap();
    drop(
        session
            .create_flow(atman_runtime::flow_store::FlowDescriptor {
                address: crate::flow_service::core_address(
                    serde_json::from_value(prepared.clone()).unwrap(),
                ),
                creation_request: prepared_request,
                created_by: None,
                content: atman_runtime::flow_store::EntrySnapshot {
                    name: "main".into(),
                    path: path.clone(),
                    bundle: program.revision_bundle(),
                }
                .into(),
            })
            .await
            .unwrap(),
    );
    let mut flows = Vec::new();
    for (index, root) in [&alpha, &beta, &beta].into_iter().enumerate() {
        if index == 2 {
            std::fs::write(config.join("config.toml"), "[storage]\nscope = 'local'\n").unwrap();
        }
        let project = ProjectContext::resolve(&hub, root, &data).unwrap();
        let cursor = session.snapshot().await.unwrap().cursor;
        session
            .set_project(cursor, Some(project.clone()))
            .await
            .unwrap();
        let catalog = rpc(&state, methods::SESSION_OPEN, json!({"session_id": id})).await;
        assert_eq!(catalog["project_root"], json!(project.root()));
        let flow = json!({"session_id": id, "instance_id": Uuid::now_v7()});
        rpc(
            &state,
            methods::FLOW_LAUNCH,
            json!({
                "flow": flow, "request_id": Uuid::now_v7(),
                "expected_catalog_sequence": catalog["catalog"]["sequence"],
                "path": path, "entry": "main"
            }),
        )
        .await;
        let result = completed(&state, &flow, &Value::Null).await;
        let label = if index == 0 { "alpha" } else { "beta" };
        let expected = format!("{label}|# project-rule\n{label}");
        assert_eq!(result["result"]["value"]["value"], expected);
        flows.push((flow, project, result["lease"]["turn"].clone(), expected));
    }
    // A prepared instance also retains its old binding and source bundle across reassignment.
    std::fs::remove_file(&path).unwrap();
    std::fs::remove_file(alpha.join(".atman/lib/helper.at")).unwrap();
    std::fs::remove_file(beta.join(".atman/lib/helper.at")).unwrap();
    rpc(
        &state,
        methods::FLOW_LAUNCH,
        json!({
            "flow": prepared, "request_id": prepared_request,
            "expected_catalog_sequence": initial.cursor.sequence,
            "path": path, "entry": "main"
        }),
    )
    .await;
    let result = completed(&state, &prepared, &Value::Null).await;
    assert_eq!(
        result["result"]["value"]["value"],
        "alpha|# project-rule\nalpha"
    );
    flows.push((
        prepared,
        initial.catalog.project.unwrap(),
        result["lease"]["turn"].clone(),
        "alpha|# project-rule\nalpha".into(),
    ));
    for (flow, _, previous, expected) in &mut flows {
        let rebound = dispatch(
            state.clone(),
            JsonRpcRequest::new(1, methods::FLOW_BIND, json!({"flow": flow})),
        )
        .await;
        assert!(
            rebound
                .error
                .unwrap()
                .message
                .contains("already has a host scheduler")
        );
        rpc(
            &state,
            methods::FLOW_INVOKE,
            json!({
                "flow": flow, "request_id": Uuid::now_v7()
            }),
        )
        .await;
        let result = completed(&state, flow, previous).await;
        assert_eq!(result["result"]["value"]["value"], *expected);
        *previous = result["lease"]["turn"].clone();
    }
    assert_eq!(service(&state).executors.lock().unwrap().len(), 3);
    state.shutdown().await.unwrap();
    drop((session, state));
    // Rebinding uses retained bundles and registered scopes, not current files or config scope.
    let state = Arc::new(DaemonState::new(data));
    rpc(&state, methods::SESSION_OPEN, json!({"session_id": id})).await;
    for (flow, project, previous, expected) in flows {
        rpc(&state, methods::FLOW_BIND, json!({"flow": flow})).await;
        rpc(
            &state,
            methods::FLOW_INVOKE,
            json!({
                "flow": flow, "request_id": Uuid::now_v7()
            }),
        )
        .await;
        let result = completed(&state, &flow, &previous).await;
        assert_eq!(result["result"]["value"]["value"], expected);
        let service = service(&state);
        let slot = service
            .executors
            .lock()
            .unwrap()
            .get(&super::ExecutorScope {
                session: id,
                project: Some(project.clone()),
            })
            .unwrap()
            .clone();
        assert_eq!(slot.get().unwrap().tool_ctx.project(), Some(&project));
    }
    assert_eq!(service(&state).executors.lock().unwrap().len(), 3);
    state.shutdown().await.unwrap();
}
