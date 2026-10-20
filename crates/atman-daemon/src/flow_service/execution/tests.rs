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
async fn moved_session_peer_spawn_uses_the_same_executor_resolver_as_rpc_launch() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("config");
    let alpha = dir.path().join("alpha");
    let beta = dir.path().join("beta");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.toml"), "[trust]\nmode = 'reckless'\n").unwrap();
    for (root, label) in [(&alpha, "alpha"), (&beta, "beta")] {
        std::fs::create_dir_all(root.join(".atman/commands")).unwrap();
        std::fs::create_dir_all(root.join(".atman/lib")).unwrap();
        std::fs::write(root.join("marker.txt"), label).unwrap();
        std::fs::write(
            root.join(".atman/lib/helper.at"),
            format!("pub flow label() -> string {{ return \"{label}\" }}"),
        )
        .unwrap();
        std::fs::write(root.join(".atman/commands/child.at"),
            "use \"project:helper.at\"::label\nflow child() -> string { return label().await + \"|\" + fs.read(\"marker.txt\") }").unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "add",
                "marker.txt",
                ".atman/commands/child.at",
                ".atman/lib/helper.at",
            ],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "source",
            ],
        ] {
            let output = std::process::Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }
    let path = config.join("main.at");
    std::fs::write(
        &path,
        r#"flow main(workspace: string = "none") -> string {
        contract { capabilities { shell: true } }
        inventory = flow.instances()
        description = flow.describe(ref: "child.at")
        child = flow.spawn(flow: description.ref, version: description.version, spawn_token: inventory.spawn_token, async: false, workspace: workspace)
        return fs.read("marker.txt") + "|" + child
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
        json!({"session_id": id, "title": "Peer scopes", "project_root": alpha}),
    )
    .await;
    let session = service(&state).session(id).unwrap();
    let catalog = session.snapshot().await.unwrap();
    let parent = json!({"session_id": id, "instance_id": Uuid::now_v7()});
    rpc(
        &state,
        methods::FLOW_LAUNCH,
        json!({
            "flow": parent, "request_id": Uuid::now_v7(),
            "expected_catalog_sequence": catalog.cursor.sequence, "path": path, "entry": "main"
        }),
    )
    .await;
    let first = completed(&state, &parent, &Value::Null).await;
    assert_eq!(first["result"]["value"]["value"], "alpha|alpha|alpha");
    let hub = ConfigHub::from_config_dir(&config);
    let project = ProjectContext::resolve(&hub, &beta, &data).unwrap();
    let catalog = session.snapshot().await.unwrap();
    session
        .set_project(catalog.cursor, Some(project.clone()))
        .await
        .unwrap();
    rpc(
        &state,
        methods::FLOW_INVOKE,
        json!({"flow": parent, "request_id": Uuid::now_v7(), "arguments": {"workspace": "auto"}}),
    )
    .await;
    let result = completed(&state, &parent, &first["lease"]["turn"]).await;
    assert_eq!(result["result"]["value"]["value"], "alpha|beta|beta");
    let catalog = session.snapshot().await.unwrap();
    let child = catalog.catalog.flows.last().unwrap();
    assert_eq!(child.project.as_ref(), Some(&project));
    let worker = session.open_flow(child.id.clone()).await.unwrap();
    let workspace = worker
        .launch_record()
        .await
        .unwrap()
        .unwrap()
        .input
        .workspace
        .unwrap();
    assert_eq!(workspace.binding.repository_root, project.root());
    assert!(
        workspace
            .binding
            .path
            .starts_with(project.root().join(".atman"))
    );
    let child = json!({"session_id": id, "instance_id": child.id.to_string()});
    let previous = completed(&state, &child, &Value::Null).await;
    std::fs::create_dir_all(config.join("commands")).unwrap();
    std::fs::write(
        config.join("commands/child.at"),
        "flow child() -> string { return \"global\" }",
    )
    .unwrap();
    let cursor = session.snapshot().await.unwrap().cursor;
    session.set_project(cursor, None).await.unwrap();
    rpc(
        &state,
        methods::FLOW_INVOKE,
        json!({"flow": parent, "request_id": Uuid::now_v7(), "arguments": {"workspace": "none"}}),
    )
    .await;
    let projectless = completed(&state, &parent, &result["lease"]["turn"]).await;
    assert_eq!(projectless["result"]["value"]["value"], "alpha|global");
    assert!(
        session
            .snapshot()
            .await
            .unwrap()
            .catalog
            .flows
            .last()
            .unwrap()
            .project
            .is_none()
    );
    assert_eq!(service(&state).executors.lock().unwrap().len(), 3);
    state.shutdown().await.unwrap();
    drop((session, state));

    std::fs::remove_file(beta.join(".atman/commands/child.at")).unwrap();
    std::fs::remove_file(beta.join(".atman/lib/helper.at")).unwrap();
    let state = Arc::new(DaemonState::new(data));
    rpc(&state, methods::SESSION_OPEN, json!({"session_id": id})).await;
    rpc(&state, methods::FLOW_BIND, json!({"flow": child})).await;
    rpc(
        &state,
        methods::FLOW_INVOKE,
        json!({"flow": child, "request_id": Uuid::now_v7()}),
    )
    .await;
    let restored = completed(&state, &child, &previous["lease"]["turn"]).await;
    assert_eq!(restored["result"]["value"]["value"], "beta|beta");
    state.shutdown().await.unwrap();
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
