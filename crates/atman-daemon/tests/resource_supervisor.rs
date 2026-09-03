use std::sync::Arc;

use atman_daemon::{DaemonState, dispatch_as};
use atman_proto::{
    InspectResourceRequest, JsonRpcRequest, ListResourcesRequest, ResourceId, ResourceState,
    ResourceTerminationStatus, TerminateResourceRequest,
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
