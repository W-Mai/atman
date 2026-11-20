use std::{collections::BTreeMap, sync::Arc, time::Duration};

use atman_daemon::{DaemonState, SharedHostServices, dispatch, dispatch_as};
use atman_proto::{
    JsonRpcRequest, SessionId,
    flows::{FlowAddress, FlowCursor},
    methods::FLOW_OVERVIEW,
    overview::{
        FlowOverviewResponse, OverviewGoalStatus, OverviewTodoStatus, OverviewWorkspaceKind,
    },
};
use atman_runtime::{
    flow_runtime::host::RuntimeHost,
    flow_store::{
        EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowTransition, RuntimeStore,
        SessionDescriptor,
        goal::GoalRequest,
        inbox::SenderIdentity,
        planning::{PlanningEdit, PlanningRequest},
        workspace::{WorkspaceRequest, WorkspaceSource},
    },
    git_workspace::WorkspaceBinding,
    memory::goal::GoalEdit,
    peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan, PeerAuthority},
    permission::PermissionClients,
};
use serde_json::json;
use uuid::Uuid;

#[tokio::test]
async fn reads_exact_committed_overview_with_or_without_an_owner() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    let runtime = RuntimeStore::open(&root).unwrap();
    let session_id = Uuid::now_v7();
    let mut session = runtime
        .create_session(SessionDescriptor {
            id: session_id,
            title: "Overview fixture".into(),
            project: None,
            configuration: Default::default(),
        })
        .unwrap();
    let flow_id = FlowInstanceId::now();
    let mut flow = session
        .create_flow(FlowDescriptor {
            address: FlowRef {
                session_id: session_id.into(),
                instance_id: flow_id.clone(),
            },
            created_by: None,
            creation_request: Uuid::now_v7(),
            content: EntrySnapshot {
                name: "main".into(),
                path: root.join("main.at"),
                bundle: atman_runtime::source_program::RevisionBundle {
                    entry: "entry:main.at".into(),
                    sources: BTreeMap::from([(
                        "entry:main.at".into(),
                        "flow main() -> int { return 7 }".into(),
                    )]),
                    edges: BTreeMap::new(),
                },
            }
            .into(),
        })
        .unwrap();
    let workspace_path = root.join(".atman/worktrees/overview");
    flow.request_workspace(WorkspaceRequest {
        binding: WorkspaceBinding {
            workspace_id: "overview".into(),
            path: workspace_path.clone(),
            repository_root: root.clone(),
            branch: None,
        },
        source: WorkspaceSource::Allocate {
            base: "a".repeat(40),
            retained: true,
        },
    })
    .unwrap();
    flow.transition(flow.state().revision(), FlowTransition::Activated)
        .unwrap();
    let sender = SenderIdentity::User(Uuid::now_v7());
    flow.edit_goal(
        GoalRequest {
            id: Uuid::now_v7(),
            sender: sender.clone(),
            expected: None,
            expected_head: None,
            edit: GoalEdit::Create {
                text: "界".repeat(200),
            },
            start_turn: false,
        },
        |_| Ok(()),
    )
    .unwrap();
    for edit in [
        PlanningEdit::WritePlan {
            id: "plan".into(),
            title: "Fixture".into(),
            steps: (0..25).map(|i| format!("Step {i}")).collect(),
        },
        PlanningEdit::TickPlan {
            id: None,
            step_index: 24,
        },
        PlanningEdit::AddTodo {
            where_: "Overview".into(),
            why: "Inspect".into(),
            how: "Read".into(),
            expected_result: "No owner opened".into(),
        },
    ] {
        flow.edit_planning(
            PlanningRequest {
                id: Uuid::now_v7(),
                sender: sender.clone(),
                expected_revision: None,
                edit,
            },
            |_| Ok(()),
        )
        .unwrap();
    }
    let cursor = FlowCursor {
        journal_id: flow.cursor().journal_id(),
        sequence: flow.cursor().sequence(),
        digest: flow.cursor().digest().into(),
    };
    let flow_path = flow.dir().unwrap().join("events.jsonl");
    let catalog_path = session.dir().join("events.jsonl");
    let original = [
        std::fs::read(&flow_path).unwrap(),
        std::fs::read(&catalog_path).unwrap(),
    ];
    let address = FlowAddress {
        session_id: SessionId(session_id),
        instance_id: cursor.journal_id,
    };
    let state = Arc::new(DaemonState::new(root.clone()));
    let request = || JsonRpcRequest::new(1, FLOW_OVERVIEW, json!({"flow": &address}));
    assert!(
        dispatch_as(state.clone(), request(), "untrusted")
            .await
            .error
            .is_some()
    );
    // The independent storage writer still owns the Session while this daemon reads the journals.
    let response = dispatch(state, request()).await;
    assert!(response.error.is_none(), "{:?}", response.error);
    let view: FlowOverviewResponse = serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(view.flow, address);
    assert_eq!(view.cursor, cursor);
    let goal = view.goal.as_ref().unwrap();
    assert_eq!(goal.status, OverviewGoalStatus::Active);
    assert!(goal.objective.truncated);
    assert_eq!(goal.objective.text, "界".repeat(170));
    assert_eq!(view.planning.revision, 3);
    let plan = view.planning.plan.as_ref().unwrap();
    assert_eq!((plan.done, plan.total, plan.steps.len()), (1, 25, 24));
    assert!(plan.steps_truncated);
    assert_eq!(view.planning.todo_counts.pending, 1);
    assert_eq!(view.planning.todos[0].status, OverviewTodoStatus::Pending);
    let workspace = view.workspace.as_ref().unwrap();
    assert_eq!(workspace.kind, OverviewWorkspaceKind::Allocate);
    assert_eq!(workspace.path.text, workspace_path.to_string_lossy());
    assert_eq!(
        original,
        [
            std::fs::read(&flow_path).unwrap(),
            std::fs::read(&catalog_path).unwrap()
        ]
    );
    drop((flow, session));

    let host = RuntimeHost::default();
    let authority = PeerAuthority::new(root.clone(), Uuid::now_v7()).unwrap();
    host.bind_peer_authority(authority.clone()).unwrap();
    let session = host.open_session(&root, session_id).await.unwrap();
    session.open_flow(flow_id).await.unwrap();
    let state = Arc::new(DaemonState::with_shared_host(
        root,
        Uuid::now_v7().to_string(),
        SharedHostServices {
            host: host.clone(),
            permissions: Arc::new(PermissionClients::default()),
            operator: Uuid::now_v7(),
            prompts: None,
        },
    ));
    // The live host retains exclusive Session authority. Inspect must use that owner
    // without taking a second filesystem lock.
    let competing_read = authority
        .try_acquire(
            AuthorityPlan::new([(AuthorityKey::Session(session_id), AuthorityMode::Shared)])
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(competing_read.is_none());
    let response = tokio::time::timeout(Duration::from_secs(3), dispatch(state, request()))
        .await
        .expect("overview attempted to acquire writer authority");
    assert!(response.error.is_none(), "{:?}", response.error);
    let attached: FlowOverviewResponse = serde_json::from_value(response.result.unwrap()).unwrap();
    assert_eq!(attached, view);
    assert_eq!(
        original,
        [
            std::fs::read(&flow_path).unwrap(),
            std::fs::read(&catalog_path).unwrap()
        ]
    );
    host.shutdown().await.unwrap();
}
