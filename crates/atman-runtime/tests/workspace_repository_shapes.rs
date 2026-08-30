use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use atman_runtime::event::FlowRunId;
use atman_runtime::flow_authority::EffectiveAuthority;
use atman_runtime::flow_workspace::FlowWorkspaceService;
use atman_runtime::git_workspace::{WorkspaceManager, WorkspacePolicy, WorkspaceState};
use atman_runtime::provider::ProviderRegistry;
use atman_runtime::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolRegistry, ToolResult};
use atman_runtime::tools::agent_ctrl::{AgentSpawn, FlowRegistry};
use atman_runtime::tools::git_workspace::GitWorkspacePrune;
use atman_runtime::value::Value;

struct Noop;

impl Tool for Noop {
    fn name(&self) -> &str {
        "test.noop"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async { Ok(Value::Str("ok".into())) })
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

fn repo() -> tempfile::TempDir {
    let repo = tempfile::tempdir().unwrap();
    git(repo.path(), &["init", "-q"]);
    git(repo.path(), &["config", "user.name", "Atman Test"]);
    git(
        repo.path(),
        &["config", "user.email", "atman@example.invalid"],
    );
    git(repo.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.path().join("README.md"), "committed\n").unwrap();
    git(repo.path(), &["add", "README.md"]);
    git(repo.path(), &["commit", "-q", "-m", "initial"]);
    repo
}

#[test]
fn linked_worktree_allocation_anchors_registry_and_storage_at_common_root() {
    let repo = repo();
    let linked_parent = tempfile::tempdir().unwrap();
    let linked = linked_parent.path().join("linked");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            linked.to_str().unwrap(),
        ],
    );

    let service = FlowWorkspaceService::new(&linked, None, "linked-generation").unwrap();
    let binding = service
        .allocate(
            WorkspacePolicy::Retain,
            "linked-session",
            "linked-flow",
            None,
        )
        .unwrap()
        .unwrap();
    let manager = WorkspaceManager::at(&linked, None).unwrap();
    let repository_root = repo.path().canonicalize().unwrap();
    assert_eq!(manager.repository_root(), repository_root);
    assert!(binding.path.starts_with(manager.managed_root()));
    assert!(repository_root.join(".atman/workspaces.json").exists());
    assert!(!linked.join(".atman/workspaces.json").exists());
}

#[test]
fn bare_repository_requires_configured_external_root_and_stays_under_it() {
    let source = repo();
    let bare_parent = tempfile::tempdir().unwrap();
    let bare = bare_parent.path().join("repo.git");
    git(
        source.path(),
        &["clone", "-q", "--bare", ".", bare.to_str().unwrap()],
    );
    let service = FlowWorkspaceService::new(&bare, None, "bare-generation").unwrap();
    let error = service
        .allocate(WorkspacePolicy::Auto, "bare-session", "bare-flow", None)
        .unwrap_err();
    assert!(error.to_string().contains("require external_root"));

    let external = tempfile::tempdir().unwrap();
    let service = FlowWorkspaceService::new(
        &bare,
        Some(external.path().to_path_buf()),
        "bare-generation",
    )
    .unwrap();
    let binding = service
        .allocate(WorkspacePolicy::Retain, "bare-session", "bare-flow", None)
        .unwrap()
        .unwrap();
    assert!(
        binding
            .path
            .starts_with(external.path().canonicalize().unwrap())
    );
    assert!(external.path().join(".atman/workspaces.json").exists());
    assert!(!bare.join(".atman/workspaces.json").exists());
}

#[tokio::test]
async fn automatic_ownership_rejects_user_supplied_owner_fields_before_allocation() {
    let repo = repo();
    let flow_path = repo.path().join("noop.at");
    std::fs::write(
        &flow_path,
        "flow noop() -> string {\n    return test.noop()\n}\n",
    )
    .unwrap();
    let tools = ToolRegistry::new();
    tools.register(Arc::new(AgentSpawn));
    tools.register(Arc::new(Noop));
    let service = FlowWorkspaceService::new(repo.path(), None, "generation").unwrap();
    let registry = Arc::new(FlowRegistry::new());
    let root_run_id = FlowRunId::now();
    let root_identity = registry
        .register_root(
            "trusted-session".into(),
            root_run_id.clone(),
            EffectiveAuthority::root(&Default::default(), false, None),
        )
        .unwrap();
    let broker = atman_runtime::permission::PermissionBroker::shared(Arc::clone(&registry));
    let mut ctx = ToolCtx::new()
        .with_registry(Arc::new(tools))
        .with_providers(Arc::new(ProviderRegistry::new()))
        .with_flow_registry(registry)
        .with_permission_broker(broker)
        .with_approval(Arc::new(atman_runtime::session::ApprovalRegistry::new()))
        .with_trust(atman_runtime::trust::TrustConfig::default())
        .with_session_id("trusted-session")
        .with_flow_workspace_service(Arc::new(service));
    ctx.flow_run_id = Some(root_run_id);
    ctx.flow_identity = Some(root_identity);
    let error = AgentSpawn
        .call(
            ToolArgs {
                positional: Vec::new(),
                named: vec![
                    (
                        "flow".into(),
                        Value::Str(format!("{}@noop", flow_path.display())),
                    ),
                    ("async".into(), Value::Bool(false)),
                    ("workspace".into(), Value::Str("retain".into())),
                    ("owner_session".into(), Value::Str("spoof-session".into())),
                    ("owner_flow".into(), Value::Str("spoof-flow".into())),
                ],
            },
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("owner_flow"));
    assert!(error.to_string().contains("owner_session"));

    let records = WorkspaceManager::at(repo.path(), None)
        .unwrap()
        .list()
        .unwrap();
    assert!(records.is_empty());
}

#[tokio::test]
async fn orphan_prune_is_dry_run_first_and_never_forces_dirty_or_retained_work() {
    let repo = repo();
    let manager = WorkspaceManager::at(repo.path(), None).unwrap();
    let base_oid = git_output(repo.path(), &["rev-parse", "HEAD"]);
    let orphan = manager
        .create_managed("orphan", "session", "orphan", "old", false, &base_oid)
        .unwrap();
    std::fs::write(orphan.worktree_path.join("dirty.txt"), "preserve me\n").unwrap();
    let retained = manager
        .create_managed("retained", "session", "retained", "old", true, &base_oid)
        .unwrap();
    manager
        .finalize_managed("retained", "session", "retained")
        .unwrap();
    manager.reconcile_generation("new").unwrap();

    let ctx = ToolCtx::new();
    let cwd = Value::Str(repo.path().display().to_string());
    let dry_run = GitWorkspacePrune
        .call(
            ToolArgs {
                positional: Vec::new(),
                named: vec![("cwd".into(), cwd.clone())],
            },
            &ctx,
        )
        .await
        .unwrap();
    assert!(matches!(dry_run, Value::List(ref records) if records.len() == 1));
    assert!(orphan.worktree_path.exists());
    assert!(retained.worktree_path.exists());

    let error = GitWorkspacePrune
        .call(
            ToolArgs {
                positional: Vec::new(),
                named: vec![("cwd".into(), cwd), ("dry_run".into(), Value::Bool(false))],
            },
            &ctx,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("changes") || error.to_string().contains("dirty"));
    assert_eq!(
        std::fs::read_to_string(orphan.worktree_path.join("dirty.txt")).unwrap(),
        "preserve me\n"
    );
    assert!(retained.worktree_path.exists());
    assert_eq!(
        manager.get("orphan").unwrap().lifecycle_state(),
        WorkspaceState::Orphaned
    );
    assert_eq!(
        manager.get("retained").unwrap().lifecycle_state(),
        WorkspaceState::Retained
    );
}
