use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use atman_runtime::git_workspace::{WorkspaceError, WorkspaceManager};

const HELPER_MODE: &str = "ATMAN_WORKSPACE_PROCESS_HELPER";
const MANAGED_CONTENTION: &str = "ATMAN_WORKSPACE_PROCESS_MANAGED_CONTENTION";
const RESULT_PREFIX: &str = "ATMAN_WORKSPACE_PROCESS_RESULT=";
const REPOSITORY: &str = "ATMAN_WORKSPACE_PROCESS_REPOSITORY";
const WORKSPACE_ID: &str = "ATMAN_WORKSPACE_PROCESS_ID";
const OWNER_SESSION: &str = "ATMAN_WORKSPACE_PROCESS_OWNER_SESSION";
const OWNER_FLOW: &str = "ATMAN_WORKSPACE_PROCESS_OWNER_FLOW";
const BASE_OID: &str = "ATMAN_WORKSPACE_PROCESS_BASE_OID";
const READY_PATH: &str = "ATMAN_WORKSPACE_PROCESS_READY_PATH";
const START_PATH: &str = "ATMAN_WORKSPACE_PROCESS_START_PATH";

#[test]
fn workspace_transaction_process_helper() {
    if std::env::var_os(HELPER_MODE).is_none() {
        return;
    }

    let repository = env_path(REPOSITORY);
    let id = std::env::var(WORKSPACE_ID).unwrap();
    let owner_session = std::env::var(OWNER_SESSION).unwrap();
    let owner_flow = std::env::var(OWNER_FLOW).unwrap();
    let base_oid = std::env::var(BASE_OID).unwrap();
    let ready_path = env_path(READY_PATH);
    let start_path = env_path(START_PATH);

    std::fs::write(&ready_path, b"ready").unwrap();
    wait_for_path(&start_path, Duration::from_secs(20));

    let manager = WorkspaceManager::at(&repository, None).unwrap();
    if std::env::var_os(MANAGED_CONTENTION).is_some() {
        let status = match manager.create_managed(
            &id,
            &owner_session,
            &owner_flow,
            "process-test-generation",
            false,
            &base_oid,
        ) {
            Ok(_) => "winner",
            Err(WorkspaceError::Invalid(message)) if message == "workspace ownership mismatch" => {
                "conflict"
            }
            Err(error) => panic!("unexpected managed allocation error: {error}"),
        };
        println!(
            "{RESULT_PREFIX}{}",
            serde_json::json!({
                "status": status,
                "owner_session": owner_session,
                "owner_flow": owner_flow,
            })
        );
    } else {
        manager
            .create(
                &id,
                None,
                Some(&base_oid),
                false,
                Some(&owner_session),
                Some(&owner_flow),
            )
            .unwrap();
    }
}

#[test]
fn independent_processes_do_not_lose_registry_updates() {
    let fixture = ProcessFixture::new();
    let process_count = 8;
    let mut children = Vec::new();

    for index in 0..process_count {
        children.push(fixture.spawn(
            &format!("process-{index}"),
            &format!("session-{index}"),
            &format!("flow-{index}"),
            index,
        ));
    }

    fixture.release_when_ready(process_count);
    assert_children_succeeded(children);

    let records = fixture.manager.list().unwrap();
    assert_eq!(records.len(), process_count);
    let ids: HashSet<_> = records.iter().map(|record| record.id.as_str()).collect();
    let owners: HashSet<_> = records
        .iter()
        .map(|record| {
            (
                record.owner_session.as_deref().unwrap(),
                record.owner_flow.as_deref().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        ids.len(),
        process_count,
        "duplicate workspace IDs persisted"
    );
    assert_eq!(owners.len(), process_count, "duplicate ownership persisted");

    let registered = registered_worktree_paths(fixture.repository.path());
    for index in 0..process_count {
        let id = format!("process-{index}");
        let record = records.iter().find(|record| record.id == id).unwrap();
        assert_eq!(
            record.owner_session.as_deref(),
            Some(format!("session-{index}").as_str())
        );
        assert_eq!(
            record.owner_flow.as_deref(),
            Some(format!("flow-{index}").as_str())
        );
        assert!(record.worktree_path.exists());
        assert!(registered.contains(&record.worktree_path));
    }

    fixture.assert_registry_is_valid_json();
    fixture.assert_no_atomic_save_temporaries();
    fixture.release_all(&records);
}

#[test]
fn independent_processes_competing_for_one_id_create_one_record() {
    let fixture = ProcessFixture::new();
    let process_count = 8;
    let mut children = Vec::new();

    for index in 0..process_count {
        children.push(fixture.spawn_managed(
            "shared",
            &format!("contending-session-{index}"),
            &format!("contending-flow-{index}"),
            index,
        ));
    }

    fixture.release_when_ready(process_count);
    let outcomes = collect_managed_outcomes(children);
    let winners: Vec<_> = outcomes
        .iter()
        .filter(|outcome| outcome["status"] == "winner")
        .collect();
    let conflicts = outcomes
        .iter()
        .filter(|outcome| outcome["status"] == "conflict")
        .count();
    assert_eq!(winners.len(), 1, "expected exactly one allocation winner");
    assert_eq!(
        conflicts,
        process_count - 1,
        "expected seven ownership conflicts"
    );
    let winner = winners[0];

    let records = fixture.manager.list().unwrap();
    assert_eq!(records.len(), 1);
    let record = &records[0];
    assert_eq!(record.id, "shared");
    assert_eq!(
        record.owner_session.as_deref(),
        winner["owner_session"].as_str()
    );
    assert_eq!(record.owner_flow.as_deref(), winner["owner_flow"].as_str());
    assert!(record.worktree_path.exists());

    let registered = registered_worktree_paths(fixture.repository.path());
    assert!(registered.contains(&record.worktree_path));
    assert_eq!(
        registered
            .iter()
            .filter(|path| path.starts_with(fixture.manager.managed_root()))
            .count(),
        1
    );

    fixture.assert_registry_is_valid_json();
    fixture.assert_no_atomic_save_temporaries();
    fixture.release_all(&records);
}

struct ProcessFixture {
    repository: tempfile::TempDir,
    synchronization: tempfile::TempDir,
    manager: WorkspaceManager,
    base_oid: String,
}

impl ProcessFixture {
    fn new() -> Self {
        let repository = git_repository();
        let synchronization = tempfile::tempdir().unwrap();
        let manager = WorkspaceManager::at(repository.path(), None).unwrap();
        let base_oid = git_output(repository.path(), &["rev-parse", "HEAD"]);
        Self {
            repository,
            synchronization,
            manager,
            base_oid,
        }
    }

    fn spawn(&self, id: &str, owner_session: &str, owner_flow: &str, index: usize) -> Child {
        self.helper_command(id, owner_session, owner_flow, index)
            .spawn()
            .unwrap()
    }

    fn spawn_managed(
        &self,
        id: &str,
        owner_session: &str,
        owner_flow: &str,
        index: usize,
    ) -> Child {
        self.helper_command(id, owner_session, owner_flow, index)
            .env(MANAGED_CONTENTION, "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn helper_command(
        &self,
        id: &str,
        owner_session: &str,
        owner_flow: &str,
        index: usize,
    ) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "workspace_transaction_process_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER_MODE, "1")
            .env(REPOSITORY, self.repository.path())
            .env(WORKSPACE_ID, id)
            .env(OWNER_SESSION, owner_session)
            .env(OWNER_FLOW, owner_flow)
            .env(BASE_OID, &self.base_oid)
            .env(
                READY_PATH,
                self.synchronization.path().join(format!("ready-{index}")),
            )
            .env(START_PATH, self.synchronization.path().join("start"));
        command
    }

    fn release_when_ready(&self, process_count: usize) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let ready = std::fs::read_dir(self.synchronization.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().starts_with("ready-"))
                .count();
            if ready == process_count {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "child processes did not reach the start barrier"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        std::fs::write(self.synchronization.path().join("start"), b"start").unwrap();
    }

    fn assert_registry_is_valid_json(&self) {
        let bytes = std::fs::read(self.repository.path().join(".atman/workspaces.json")).unwrap();
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert!(parsed["workspaces"].is_array());
    }

    fn assert_no_atomic_save_temporaries(&self) {
        let leftovers: Vec<_> = std::fs::read_dir(self.repository.path().join(".atman"))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| {
                let name = name.to_string_lossy();
                name.starts_with(".workspaces.json.") && name.ends_with(".tmp")
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "atomic save temporaries remain: {leftovers:?}"
        );
    }

    fn release_all(&self, records: &[atman_runtime::git_workspace::WorkspaceRecord]) {
        for record in records {
            self.manager
                .release(
                    &record.id,
                    record.owner_session.as_deref(),
                    record.owner_flow.as_deref(),
                    false,
                )
                .unwrap();
        }
    }
}

fn env_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap())
}

fn wait_for_path(path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_children_succeeded(children: Vec<Child>) {
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert_process_succeeded(&output);
    }
}

fn collect_managed_outcomes(children: Vec<Child>) -> Vec<serde_json::Value> {
    children
        .into_iter()
        .map(|child| {
            let output = child.wait_with_output().unwrap();
            assert_process_succeeded(&output);
            let stdout = String::from_utf8(output.stdout).unwrap();
            let outcomes: Vec<_> = stdout
                .lines()
                .filter_map(|line| {
                    line.find(RESULT_PREFIX)
                        .map(|index| &line[index + RESULT_PREFIX.len()..])
                })
                .collect();
            assert_eq!(
                outcomes.len(),
                1,
                "managed helper did not emit exactly one structured outcome:\n{stdout}"
            );
            serde_json::from_str(outcomes[0]).unwrap()
        })
        .collect()
}

fn assert_process_succeeded(output: &Output) {
    assert!(
        output.status.success(),
        "child process failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn registered_worktree_paths(repository: &Path) -> HashSet<PathBuf> {
    git_output(repository, &["worktree", "list", "--porcelain"])
        .lines()
        .filter_map(|line| line.strip_prefix("worktree "))
        .map(PathBuf::from)
        .collect()
}

fn git_repository() -> tempfile::TempDir {
    let repository = tempfile::tempdir().unwrap();
    git(repository.path(), &["init", "-q"]);
    git(repository.path(), &["config", "user.name", "Atman Test"]);
    git(
        repository.path(),
        &["config", "user.email", "atman@example.invalid"],
    );
    git(repository.path(), &["config", "commit.gpgsign", "false"]);
    std::fs::write(repository.path().join("README.md"), "committed\n").unwrap();
    git(repository.path(), &["add", "README.md"]);
    git(repository.path(), &["commit", "-q", "-m", "initial"]);
    repository
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
    assert_process_succeeded(&output);
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
