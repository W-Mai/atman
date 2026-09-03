use std::sync::Arc;

use atman_daemon::{DaemonState, dispatch_as};
use atman_proto::{
    InspectResourceRequest, JsonRpcRequest, ListResourcesRequest, ReleaseResourceRequest,
    ResourceId, ResourceState, ResourceTerminationStatus, RetainResourceRequest,
    TerminateResourceRequest,
};
use atman_runtime::{TaskDisplay, TaskKind, TaskOwner, TaskStatus, task_registry::TaskTermination};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn resource_commands_are_session_scoped_and_terminate_live_tasks() {
    let tmp = tempfile::tempdir().unwrap();
    let state = Arc::new(DaemonState::new(tmp.path().to_path_buf()));
    let session = Arc::new(atman_runtime::Session::open_ephemeral());
    let session_id = atman_proto::SessionId(session.id().0);
    state
        .register_session(session_id.clone(), session, "alice")
        .await
        .unwrap();

    let cancel = CancellationToken::new();
    let task_id = state.task_registry().register(
        TaskKind::Bash,
        TaskDisplay {
            label: "Inspect dependencies".into(),
            command: Some("cargo metadata".into()),
        },
        "bg-test".into(),
        TaskOwner::new(
            session_id.to_string(),
            Some(atman_runtime::event::FlowRunId::now()),
        ),
        cancel.clone(),
    );
    let resource_id = ResourceId(format!("task:{task_id}"));

    let listed = dispatch_as(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::ListResources>(
            1,
            &ListResourcesRequest {
                session_id: session_id.clone(),
            },
        )
        .unwrap(),
        "alice",
    )
    .await
    .into_method_output::<atman_proto::rpc::ListResources>()
    .unwrap();
    assert_eq!(listed.resources.len(), 1);
    assert_eq!(listed.resources[0].id, resource_id);
    assert_eq!(listed.resources[0].state, ResourceState::Running);
    assert_eq!(listed.resources[0].details["command"], "cargo metadata");

    let inspected = dispatch_as(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::InspectResource>(
            2,
            &InspectResourceRequest {
                session_id: session_id.clone(),
                resource_id: resource_id.clone(),
            },
        )
        .unwrap(),
        "alice",
    )
    .await
    .into_method_output::<atman_proto::rpc::InspectResource>()
    .unwrap();
    assert_eq!(inspected.resource.label, "Inspect dependencies");

    let denied = dispatch_as(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::ListResources>(
            3,
            &ListResourcesRequest {
                session_id: session_id.clone(),
            },
        )
        .unwrap(),
        "bob",
    )
    .await;
    assert!(denied.error.is_some());

    let command = TerminateResourceRequest {
        request_id: Some(atman_proto::RequestId::now()),
        session_id: session_id.clone(),
        resource_id: resource_id.clone(),
    };
    let terminated = dispatch_as(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::TerminateResource>(4, &command).unwrap(),
        "alice",
    )
    .await
    .into_method_output::<atman_proto::rpc::TerminateResource>()
    .unwrap();
    assert_eq!(terminated.status, ResourceTerminationStatus::Terminating);
    assert!(cancel.is_cancelled());
    assert!(terminated.cursor > listed.cursor);
    assert!(terminated.revision > listed.revision);

    let retry = dispatch_as(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::TerminateResource>(5, &command).unwrap(),
        "alice",
    )
    .await
    .into_method_output::<atman_proto::rpc::TerminateResource>()
    .unwrap();
    assert_eq!(retry.status, ResourceTerminationStatus::Terminating);
    assert_eq!(retry.cursor, terminated.cursor);

    state.task_registry().finish(&task_id, TaskStatus::Killed);
    let terminal = dispatch_as(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::TerminateResource>(
            6,
            &TerminateResourceRequest {
                request_id: Some(atman_proto::RequestId::now()),
                session_id,
                resource_id,
            },
        )
        .unwrap(),
        "alice",
    )
    .await
    .into_method_output::<atman_proto::rpc::TerminateResource>()
    .unwrap();
    assert_eq!(terminal.status, ResourceTerminationStatus::AlreadyTerminal);
    assert_eq!(
        state.task_registry().lookup(&task_id).unwrap().termination,
        Some(TaskTermination::Killed)
    );
}

#[tokio::test]
async fn workspace_resource_actions_are_owned_durable_and_dirty_safe() {
    let tmp = tempfile::tempdir().unwrap();
    let project_root = tmp.path().join("project");
    std::fs::create_dir_all(&project_root).unwrap();
    init_repo(&project_root);
    let data_dir = tmp.path().join("data");
    let state = Arc::new(DaemonState::new_with_generation(
        data_dir.clone(),
        "workspace-resource-generation".into(),
    ));
    let session = Arc::new(atman_runtime::Session::open(&data_dir).unwrap());
    atman_runtime::session_meta::SessionMeta {
        project_root: Some(project_root.clone()),
        ..Default::default()
    }
    .save(session.dir())
    .unwrap();
    let session_id = atman_proto::SessionId(session.id().0);
    state
        .register_session(session_id.clone(), session.clone(), "alice")
        .await
        .unwrap();
    let service = atman_runtime::flow_workspace::FlowWorkspaceService::new(
        &project_root,
        None,
        state.daemon_generation(),
    )
    .unwrap();
    let run_id = atman_proto::FlowRunId(uuid::Uuid::now_v7());
    let binding = service
        .allocate(
            atman_runtime::git_workspace::WorkspacePolicy::Auto,
            &session_id.to_string(),
            &run_id.to_string(),
            None,
        )
        .unwrap()
        .unwrap();
    emit_workspace(&session, &run_id, &binding, "active");
    let resource_id = ResourceId(format!("workspace:{}", binding.workspace_id));

    let denied = dispatch_as(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::RetainResource>(
            10,
            &RetainResourceRequest {
                request_id: Some(atman_proto::RequestId::now()),
                session_id: session_id.clone(),
                resource_id: resource_id.clone(),
            },
        )
        .unwrap(),
        "bob",
    )
    .await;
    assert!(denied.error.is_some());

    let retain_command = RetainResourceRequest {
        request_id: Some(atman_proto::RequestId::now()),
        session_id: session_id.clone(),
        resource_id: resource_id.clone(),
    };
    let retained = dispatch_as(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::RetainResource>(11, &retain_command)
            .unwrap(),
        "alice",
    )
    .await
    .into_method_output::<atman_proto::rpc::RetainResource>()
    .unwrap();
    assert_eq!(retained.resource.state, ResourceState::Retained);
    assert!(binding.path.exists());

    let retained_retry = dispatch_as(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::RetainResource>(12, &retain_command)
            .unwrap(),
        "alice",
    )
    .await
    .into_method_output::<atman_proto::rpc::RetainResource>()
    .unwrap();
    assert_eq!(retained_retry.cursor, retained.cursor);

    let release_command = ReleaseResourceRequest {
        request_id: Some(atman_proto::RequestId::now()),
        session_id: session_id.clone(),
        resource_id: resource_id.clone(),
    };
    let released = dispatch_as(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::ReleaseResource>(13, &release_command)
            .unwrap(),
        "alice",
    )
    .await
    .into_method_output::<atman_proto::rpc::ReleaseResource>()
    .unwrap();
    assert_eq!(released.resource.state, ResourceState::Released);
    assert_eq!(released.resource.started_at, retained.resource.started_at);
    assert!(!binding.path.exists());

    let released_retry = dispatch_as(
        state.clone(),
        JsonRpcRequest::for_method::<atman_proto::rpc::ReleaseResource>(14, &release_command)
            .unwrap(),
        "alice",
    )
    .await
    .into_method_output::<atman_proto::rpc::ReleaseResource>()
    .unwrap();
    assert_eq!(released_retry.cursor, released.cursor);

    let dirty_run_id = atman_proto::FlowRunId(uuid::Uuid::now_v7());
    let dirty = service
        .allocate(
            atman_runtime::git_workspace::WorkspacePolicy::Auto,
            &session_id.to_string(),
            &dirty_run_id.to_string(),
            None,
        )
        .unwrap()
        .unwrap();
    std::fs::write(dirty.path.join("dirty.txt"), "preserve me\n").unwrap();
    let dirty_record = match service
        .finalize(&dirty, &session_id.to_string(), &dirty_run_id.to_string())
        .unwrap()
    {
        atman_runtime::git_workspace::WorkspaceFinalizeOutcome::Dirty(record) => record,
        outcome => panic!("expected dirty workspace, got {outcome:?}"),
    };
    emit_workspace(&session, &dirty_run_id, &dirty, "dirty");
    let dirty_release = dispatch_as(
        state,
        JsonRpcRequest::for_method::<atman_proto::rpc::ReleaseResource>(
            15,
            &ReleaseResourceRequest {
                request_id: Some(atman_proto::RequestId::now()),
                session_id,
                resource_id: ResourceId(format!("workspace:{}", dirty.workspace_id)),
            },
        )
        .unwrap(),
        "alice",
    )
    .await;
    assert!(dirty_release.error.is_some());
    assert!(dirty_record.worktree_path.exists());
}

fn emit_workspace(
    session: &atman_runtime::Session,
    run_id: &atman_proto::FlowRunId,
    binding: &atman_runtime::git_workspace::WorkspaceBinding,
    state: &str,
) {
    session
        .sink()
        .emit(atman_runtime::event::Event::WorkspaceLifecycle {
            run_id: atman_runtime::event::FlowRunId(run_id.0),
            workspace_id: binding.workspace_id.clone(),
            path: binding.path.display().to_string(),
            state: state.into(),
            cleanup_error: None,
        });
}

fn init_repo(path: &std::path::Path) {
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.name", "Atman Test"],
        vec!["config", "user.email", "atman@example.invalid"],
        vec!["config", "commit.gpgsign", "false"],
    ] {
        assert!(
            std::process::Command::new("git")
                .args(args)
                .current_dir(path)
                .status()
                .unwrap()
                .success()
        );
    }
    std::fs::write(path.join("README.md"), "workspace\n").unwrap();
    assert!(
        std::process::Command::new("git")
            .args(["add", "README.md"])
            .current_dir(path)
            .status()
            .unwrap()
            .success()
    );
    assert!(
        std::process::Command::new("git")
            .args(["commit", "-q", "-m", "initial"])
            .current_dir(path)
            .status()
            .unwrap()
            .success()
    );
}
