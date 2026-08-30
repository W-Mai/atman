use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use atman_runtime::error::RuntimeError;
use atman_runtime::event::{Event, EventSink, FlowRunId};
use atman_runtime::flow_authority::EffectiveAuthority;
use atman_runtime::flow_workspace::FlowWorkspaceService;
use atman_runtime::git_workspace::{WorkspaceManager, WorkspaceState};
use atman_runtime::provider::ProviderRegistry;
use atman_runtime::task_registry::{TaskFilter, TaskRegistry, TaskStatus};
use atman_runtime::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolRegistry, ToolResult};
use atman_runtime::tools::agent_ctrl::{
    AgentKill, AgentSpawn, AgentStatus, FlowRegistry, FlowRunStatus,
};
use atman_runtime::value::Value;

struct LifecycleProbe;

impl Tool for LifecycleProbe {
    fn name(&self) -> &str {
        "test.lifecycle_probe"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let action = match args.named("action") {
                Some(Value::Str(action)) => action.as_str(),
                _ => return Err(RuntimeError::ToolFailed("missing action".into())),
            };
            match action {
                "cwd" => Ok(Value::Str(ctx.resolve_cwd(None)?.display().to_string())),
                "dirty" => {
                    let path = ctx.resolve_path(Path::new("dirty.txt"))?;
                    std::fs::write(path, "inspect me\n")
                        .map_err(|error| RuntimeError::ToolFailed(error.to_string()))?;
                    Ok(Value::Str("dirty".into()))
                }
                "error" => Err(RuntimeError::ToolFailed("probe failed".into())),
                "wait" => {
                    ctx.cancel.cancelled().await;
                    Err(RuntimeError::Cancelled("probe cancelled".into()))
                }
                "break_cleanup" => {
                    let binding = ctx.workspace.as_ref().expect("managed workspace binding");
                    let registry_path = binding.repository_root.join(".atman/workspaces.json");
                    let registry_bytes = std::fs::read(&registry_path)
                        .map_err(|error| RuntimeError::ToolFailed(error.to_string()))?;
                    let mut registry: serde_json::Value =
                        serde_json::from_slice(&registry_bytes)
                            .map_err(|error| RuntimeError::ToolFailed(error.to_string()))?;
                    let workspaces = registry["workspaces"]
                        .as_array_mut()
                        .expect("workspace registry array");
                    let record = workspaces
                        .iter_mut()
                        .find(|record| record["id"] == binding.workspace_id)
                        .expect("workspace record");
                    record["owner_session"] = serde_json::Value::String("other-session".into());
                    let registry_bytes = serde_json::to_vec_pretty(&registry)
                        .map_err(|error| RuntimeError::ToolFailed(error.to_string()))?;
                    std::fs::write(registry_path, registry_bytes)
                        .map_err(|error| RuntimeError::ToolFailed(error.to_string()))?;
                    Ok(Value::Str("cleanup will fail".into()))
                }
                other => Err(RuntimeError::ToolFailed(format!(
                    "unknown probe action {other}"
                ))),
            }
        })
    }
}

struct Setup {
    _repo: tempfile::TempDir,
    ctx: ToolCtx,
    flow_ref: String,
    flows: Arc<FlowRegistry>,
    tasks: TaskRegistry,
    events: EventSink,
}

impl Setup {
    fn new(with_service: bool, with_session: bool) -> Self {
        let repo = git_repo();
        let flow_path = repo.path().join("workspace_flow.at");
        std::fs::write(
            &flow_path,
            r#"flow lifecycle(action: string) -> string {
    result = test.lifecycle_probe(action: action)
    return result
}
"#,
        )
        .unwrap();

        let flows = Arc::new(FlowRegistry::new());
        let tasks = TaskRegistry::new();
        let events = EventSink::new();
        let tools = ToolRegistry::new();
        tools.register(Arc::new(AgentSpawn));
        tools.register(Arc::new(AgentStatus));
        tools.register(Arc::new(AgentKill));
        tools.register(Arc::new(LifecycleProbe));

        let root_run_id = FlowRunId::now();
        let root_identity = flows
            .register_root(
                "session-one".into(),
                root_run_id.clone(),
                EffectiveAuthority::root(&Default::default(), false, None),
            )
            .unwrap();
        let broker = atman_runtime::permission::PermissionBroker::shared(Arc::clone(&flows));
        let mut ctx = ToolCtx::new()
            .with_registry(Arc::new(tools))
            .with_providers(Arc::new(ProviderRegistry::new()))
            .with_flow_registry(Arc::clone(&flows))
            .with_permission_broker(broker)
            .with_approval(Arc::new(atman_runtime::session::ApprovalRegistry::new()))
            .with_trust(atman_runtime::trust::TrustConfig::default())
            .with_task_registry(tasks.clone())
            .with_events(events.clone());
        ctx.flow_run_id = Some(root_run_id);
        ctx.flow_identity = Some(root_identity);
        if with_session {
            ctx = ctx.with_session_id("session-one");
        }
        if with_service {
            let service = FlowWorkspaceService::new(repo.path(), None, "generation-one").unwrap();
            ctx = ctx.with_flow_workspace_service(Arc::new(service));
        }

        Self {
            flow_ref: format!("{}@lifecycle", flow_path.display()),
            _repo: repo,
            ctx,
            flows,
            tasks,
            events,
        }
    }

    async fn spawn(&self, workspace: Option<&str>, action: &str) -> Value {
        self.spawn_with_extra(workspace, action, Vec::new()).await
    }

    async fn spawn_with_extra(
        &self,
        workspace: Option<&str>,
        action: &str,
        extra: Vec<(String, Value)>,
    ) -> Value {
        let mut named = vec![
            ("flow".into(), Value::Str(self.flow_ref.clone())),
            (
                "arguments".into(),
                Value::Struct(vec![("action".into(), Value::Str(action.into()))]),
            ),
        ];
        if let Some(workspace) = workspace {
            named.push(("workspace".into(), Value::Str(workspace.into())));
        }
        named.extend(extra);
        AgentSpawn
            .call(
                ToolArgs {
                    positional: vec![],
                    named,
                },
                &self.ctx,
            )
            .await
            .unwrap()
    }
}

fn git(cwd: &Path, args: &[&str]) {
    git_output(cwd, args);
}

fn git_output(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn git_repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.name", "Atman Test"]);
    git(
        repo.path(),
        &["config", "user.email", "atman@example.invalid"],
    );
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    git(repo.path(), &["config", "tag.gpgsign", "false"]);
    std::fs::write(repo.path().join("README.md"), "committed\n").unwrap();
    git(repo.path(), &["add", "README.md"]);
    git(repo.path(), &["commit", "-q", "-m", "initial"]);
    repo
}

fn field<'a>(value: &'a Value, name: &str) -> Option<&'a Value> {
    match value {
        Value::Struct(fields) => fields
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value),
        _ => None,
    }
}

fn string_field(value: &Value, name: &str) -> String {
    match field(value, name) {
        Some(Value::Str(value)) => value.clone(),
        other => panic!("expected string field {name}, got {other:?}"),
    }
}

async fn wait_for_terminal(setup: &Setup, handle: &str) -> FlowRunStatus {
    for _ in 0..200 {
        let status = setup
            .flows
            .lookup(handle)
            .unwrap()
            .status
            .lock()
            .unwrap()
            .clone();
        if !status.is_running() {
            return status;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("flow {handle} did not finish");
}

async fn status(setup: &Setup, handle: &str) -> Value {
    AgentStatus
        .call(
            ToolArgs {
                positional: vec![],
                named: vec![("handle".into(), Value::Str(handle.into()))],
            },
            &setup.ctx,
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn none_policy_preserves_existing_spawn_behavior() {
    let setup = Setup::new(false, false);
    let spawned = setup.spawn(None, "cwd").await;
    let handle = string_field(&spawned, "handle");
    assert!(field(&spawned, "workspace_id").is_none());

    assert!(matches!(
        wait_for_terminal(&setup, &handle).await,
        FlowRunStatus::Ok { .. }
    ));
    assert!(setup.flows.lookup(&handle).unwrap().workspace.is_none());
    assert_eq!(
        setup.tasks.lookup_by_handle(&handle).unwrap().workspace_id,
        None
    );
}

#[tokio::test]
async fn managed_allocation_failures_do_not_register_flow_or_task() {
    for setup in [Setup::new(false, true), Setup::new(true, false)] {
        let error = AgentSpawn
            .call(
                ToolArgs {
                    positional: vec![],
                    named: vec![
                        ("flow".into(), Value::Str(setup.flow_ref.clone())),
                        ("workspace".into(), Value::Str("auto".into())),
                    ],
                },
                &setup.ctx,
            )
            .await
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("workspace service is unavailable")
                || error
                    .to_string()
                    .contains("requires a non-empty session id"),
            "unexpected allocation error: {error}"
        );
        assert!(setup.flows.is_empty());
        assert!(setup.tasks.list(&TaskFilter::all()).is_empty());
    }
}

#[tokio::test]
async fn flow_source_failure_happens_before_workspace_allocation() {
    let setup = Setup::new(true, true);
    let missing_flow = setup._repo.path().join("missing.at");

    let error = AgentSpawn
        .call(
            ToolArgs {
                positional: vec![],
                named: vec![
                    (
                        "flow".into(),
                        Value::Str(format!("{}@missing", missing_flow.display())),
                    ),
                    ("workspace".into(), Value::Str("auto".into())),
                ],
            },
            &setup.ctx,
        )
        .await
        .unwrap_err();

    assert!(error.to_string().contains("missing.at"));
    assert!(setup.flows.is_empty());
    assert!(setup.tasks.list(&TaskFilter::all()).is_empty());
    let records = WorkspaceManager::at(setup._repo.path(), None)
        .unwrap()
        .list()
        .unwrap();
    assert!(records.is_empty());
}

#[tokio::test]
async fn auto_child_uses_managed_cwd_and_releases_clean_workspace() {
    let setup = Setup::new(true, true);
    let process_cwd = std::env::current_dir().unwrap();
    let spawned = setup.spawn(Some("auto"), "cwd").await;
    let handle = string_field(&spawned, "handle");
    let workspace_id = string_field(&spawned, "workspace_id");
    let workspace_path = PathBuf::from(string_field(&spawned, "workspace_path"));
    assert_eq!(string_field(&spawned, "workspace_state"), "active");

    let final_status = wait_for_terminal(&setup, &handle).await;
    match final_status {
        FlowRunStatus::Ok { final_text, .. } => {
            assert_eq!(PathBuf::from(final_text), workspace_path)
        }
        other => panic!("expected ok flow, got {other:?}"),
    }
    assert_eq!(std::env::current_dir().unwrap(), process_cwd);
    assert!(!workspace_path.exists());

    let projected = status(&setup, &handle).await;
    assert_eq!(string_field(&projected, "workspace_id"), workspace_id);
    assert_eq!(string_field(&projected, "workspace_state"), "released");
    assert!(field(&projected, "cleanup_error").is_none());

    let task = setup.tasks.lookup_by_handle(&handle).unwrap();
    assert_eq!(task.workspace_id.as_deref(), Some(workspace_id.as_str()));
    assert_eq!(task.status, TaskStatus::Ok);
}

#[tokio::test]
async fn nested_async_spawn_branches_from_parent_workspace_head() {
    let mut setup = Setup::new(true, true);
    let manager = WorkspaceManager::at(setup._repo.path(), None).unwrap();
    let main_head = git_output(setup._repo.path(), &["rev-parse", "HEAD"]);
    let parent_record = manager
        .create_managed(
            "parent",
            "session-one",
            "parent-flow",
            "generation-one",
            true,
            &main_head,
        )
        .unwrap();
    std::fs::write(
        parent_record.worktree_path.join("parent-marker.txt"),
        "from parent\n",
    )
    .unwrap();
    git(&parent_record.worktree_path, &["add", "parent-marker.txt"]);
    git(
        &parent_record.worktree_path,
        &["commit", "-q", "-m", "parent marker"],
    );
    let parent_head = git_output(&parent_record.worktree_path, &["rev-parse", "HEAD"]);
    setup.ctx = setup.ctx.clone().with_workspace(parent_record.binding());

    let spawned = setup.spawn(Some("retain"), "cwd").await;
    let handle = string_field(&spawned, "handle");
    let child_id = string_field(&spawned, "workspace_id");
    let child_path = PathBuf::from(string_field(&spawned, "workspace_path"));
    match wait_for_terminal(&setup, &handle).await {
        FlowRunStatus::Ok { final_text, .. } => assert_eq!(PathBuf::from(final_text), child_path),
        other => panic!("expected ok flow, got {other:?}"),
    }

    let child_record = manager.get(&child_id).unwrap();
    assert_eq!(
        child_record.allocation_base.as_deref(),
        Some(parent_head.as_str())
    );
    assert_eq!(git_output(&child_path, &["rev-parse", "HEAD"]), parent_head);
    assert_eq!(
        std::fs::read_to_string(child_path.join("parent-marker.txt")).unwrap(),
        "from parent\n"
    );
    assert_eq!(
        git_output(setup._repo.path(), &["rev-parse", "HEAD"]),
        main_head
    );
}

#[tokio::test]
async fn dirty_and_retain_policies_preserve_workspaces() {
    for (policy, action, expected) in [
        ("auto", "dirty", WorkspaceState::Dirty),
        ("retain", "cwd", WorkspaceState::Retained),
    ] {
        let setup = Setup::new(true, true);
        let spawned = setup.spawn(Some(policy), action).await;
        let handle = string_field(&spawned, "handle");
        let workspace_id = string_field(&spawned, "workspace_id");
        let workspace_path = PathBuf::from(string_field(&spawned, "workspace_path"));

        assert!(matches!(
            wait_for_terminal(&setup, &handle).await,
            FlowRunStatus::Ok { .. }
        ));
        assert!(workspace_path.exists());
        let projected = status(&setup, &handle).await;
        assert_eq!(
            string_field(&projected, "workspace_state"),
            expected.as_str()
        );
        let manager = WorkspaceManager::at(setup._repo.path(), None).unwrap();
        assert_eq!(
            manager.get(&workspace_id).unwrap().lifecycle_state(),
            expected
        );
    }
}

#[tokio::test]
async fn child_error_still_finalizes_clean_workspace() {
    let setup = Setup::new(true, true);
    let spawned = setup.spawn(Some("auto"), "error").await;
    let handle = string_field(&spawned, "handle");
    let workspace_path = PathBuf::from(string_field(&spawned, "workspace_path"));

    match wait_for_terminal(&setup, &handle).await {
        FlowRunStatus::Err { message, .. } => assert!(message.contains("probe failed")),
        other => panic!("expected error flow, got {other:?}"),
    }
    assert!(!workspace_path.exists());
    let projected = status(&setup, &handle).await;
    assert_eq!(string_field(&projected, "workspace_state"), "released");
    assert_eq!(
        setup.tasks.lookup_by_handle(&handle).unwrap().status,
        TaskStatus::Err
    );
}

#[tokio::test]
async fn kill_maps_flow_and_task_status_and_finalizes_workspace() {
    let setup = Setup::new(true, true);
    let spawned = setup.spawn(Some("auto"), "wait").await;
    let handle = string_field(&spawned, "handle");
    let workspace_path = PathBuf::from(string_field(&spawned, "workspace_path"));

    AgentKill
        .call(
            ToolArgs {
                positional: vec![],
                named: vec![("handle".into(), Value::Str(handle.clone()))],
            },
            &setup.ctx,
        )
        .await
        .unwrap();

    assert!(matches!(
        wait_for_terminal(&setup, &handle).await,
        FlowRunStatus::Killed { .. }
    ));
    assert!(!workspace_path.exists());
    let projected = status(&setup, &handle).await;
    assert_eq!(string_field(&projected, "workspace_state"), "released");
    assert_eq!(
        setup.tasks.lookup_by_handle(&handle).unwrap().status,
        TaskStatus::Killed
    );
}

#[tokio::test]
async fn cleanup_failure_is_projected_without_changing_flow_result() {
    let setup = Setup::new(true, true);
    let spawned = setup.spawn(Some("auto"), "break_cleanup").await;
    let handle = string_field(&spawned, "handle");
    let workspace_path = PathBuf::from(string_field(&spawned, "workspace_path"));

    assert!(matches!(
        wait_for_terminal(&setup, &handle).await,
        FlowRunStatus::Ok { .. }
    ));
    assert!(workspace_path.exists());
    let projected = status(&setup, &handle).await;
    assert_eq!(string_field(&projected, "workspace_state"), "active");
    assert!(!string_field(&projected, "cleanup_error").is_empty());

    let workspace_id = string_field(&projected, "workspace_id");
    let manager = WorkspaceManager::at(setup._repo.path(), None).unwrap();
    assert_eq!(
        manager.get(&workspace_id).unwrap().lifecycle_state(),
        WorkspaceState::Active
    );
    let lifecycle = setup
        .events
        .snapshot()
        .into_iter()
        .find(|event| {
            matches!(
                event,
                Event::WorkspaceLifecycle {
                    workspace_id: event_workspace_id,
                    ..
                } if event_workspace_id == &workspace_id
            )
        })
        .expect("workspace lifecycle event");
    let Event::WorkspaceLifecycle {
        state,
        cleanup_error,
        ..
    } = lifecycle
    else {
        unreachable!()
    };
    assert_eq!(state, "active");
    assert!(cleanup_error.is_some_and(|error| !error.is_empty()));
    assert_eq!(
        setup.tasks.lookup_by_handle(&handle).unwrap().status,
        TaskStatus::Ok
    );
}
