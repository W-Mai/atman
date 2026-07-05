use std::path::Path;
use std::process::Command;

fn atman_bin() -> &'static str {
    env!("CARGO_BIN_EXE_atman")
}

fn have_git() -> bool {
    Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn seed_identity(repo: &Path) {
    for (k, v) in [
        ("user.email", "sync-test@atman.local"),
        ("user.name", "atman sync test"),
        ("commit.gpgsign", "false"),
    ] {
        let out = Command::new("git")
            .args(["config", k, v])
            .current_dir(repo)
            .output()
            .expect("git config");
        assert!(
            out.status.success(),
            "git config {k}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn run_atman(cwd: &Path, args: &[&str]) -> (String, String, i32) {
    let out = Command::new(atman_bin())
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("spawn atman");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

fn init_bare_remote(dir: &Path) {
    let out = Command::new("git")
        .args(["init", "--bare", "--initial-branch=main"])
        .current_dir(dir)
        .output()
        .expect("git init --bare");
    assert!(
        out.status.success(),
        "bare init failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn sync_init_creates_repo_gitignore_and_remote() {
    if !have_git() {
        eprintln!("skip: git binary not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let remote = tmp.path().join("origin.git");
    std::fs::create_dir_all(&remote).unwrap();
    init_bare_remote(&remote);

    let (out, err, code) = run_atman(
        &project,
        &["sync", "init", remote.to_str().unwrap(), "--branch", "main"],
    );
    assert_eq!(code, 0, "init exit: stderr={err}\nstdout={out}");
    assert!(project.join(".atman/.git").exists(), ".git missing");
    let gitignore = std::fs::read_to_string(project.join(".atman/.gitignore")).unwrap();
    assert!(gitignore.contains("todos.jsonl"));
    assert!(gitignore.contains("events.jsonl"));

    let remote_out = Command::new("git")
        .args(["-C", ".atman", "remote", "get-url", "origin"])
        .current_dir(&project)
        .output()
        .unwrap();
    assert!(remote_out.status.success());
    let url = String::from_utf8_lossy(&remote_out.stdout);
    assert!(
        url.trim() == remote.to_str().unwrap(),
        "want {}, got {}",
        remote.display(),
        url.trim()
    );
}

#[test]
fn sync_push_then_pull_round_trips_two_project_dirs() {
    if !have_git() {
        eprintln!("skip: git binary not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("origin.git");
    std::fs::create_dir_all(&remote).unwrap();
    init_bare_remote(&remote);

    let laptop = tmp.path().join("laptop");
    std::fs::create_dir_all(&laptop).unwrap();
    let (_, err, code) = run_atman(
        &laptop,
        &["sync", "init", remote.to_str().unwrap(), "--branch", "main"],
    );
    assert_eq!(code, 0, "init laptop: {err}");
    seed_identity(&laptop.join(".atman"));
    std::fs::write(
        laptop.join(".atman/confessions.jsonl"),
        "{\"trigger\":\"first-machine\"}\n",
    )
    .unwrap();

    let (_, err, code) = run_atman(&laptop, &["sync", "push", "--message", "seed"]);
    assert_eq!(code, 0, "push laptop: {err}");

    let desktop = tmp.path().join("desktop");
    std::fs::create_dir_all(&desktop).unwrap();
    let (_, err, code) = run_atman(
        &desktop,
        &["sync", "init", remote.to_str().unwrap(), "--branch", "main"],
    );
    assert_eq!(code, 0, "init desktop: {err}");
    seed_identity(&desktop.join(".atman"));
    let (_, err, code) = run_atman(&desktop, &["sync", "pull"]);
    assert_eq!(code, 0, "pull desktop: {err}");
    let restored = std::fs::read_to_string(desktop.join(".atman/confessions.jsonl")).unwrap();
    assert!(
        restored.contains("first-machine"),
        "want laptop's confession on desktop, got: {restored}"
    );
}

#[test]
fn sync_status_reports_uninitialised_before_init() {
    if !have_git() {
        eprintln!("skip: git binary not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (out, err, code) = run_atman(tmp.path(), &["sync", "status"]);
    assert_eq!(code, 0, "status exit: stderr={err}");
    assert!(
        out.contains("not a memory repo"),
        "want uninitialised hint, got: {out}"
    );
}

#[test]
fn sync_push_ignores_todos_and_events_via_gitignore() {
    if !have_git() {
        eprintln!("skip: git binary not on PATH");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let remote = tmp.path().join("origin.git");
    std::fs::create_dir_all(&remote).unwrap();
    init_bare_remote(&remote);
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    let (_, err, code) = run_atman(
        &project,
        &["sync", "init", remote.to_str().unwrap(), "--branch", "main"],
    );
    assert_eq!(code, 0, "init: {err}");
    seed_identity(&project.join(".atman"));

    std::fs::write(
        project.join(".atman/confessions.jsonl"),
        "{\"keep\":true}\n",
    )
    .unwrap();
    std::fs::write(project.join(".atman/todos.jsonl"), "{\"drop\":true}\n").unwrap();
    std::fs::write(project.join(".atman/events.jsonl"), "runtime state\n").unwrap();

    let (_, err, code) = run_atman(&project, &["sync", "push"]);
    assert_eq!(code, 0, "push: {err}");

    let bare_out = Command::new("git")
        .args([
            "--git-dir",
            remote.to_str().unwrap(),
            "ls-tree",
            "-r",
            "main",
            "--name-only",
        ])
        .output()
        .expect("git ls-tree");
    assert!(
        bare_out.status.success(),
        "ls-tree bare: {}",
        String::from_utf8_lossy(&bare_out.stderr)
    );
    let listed = String::from_utf8_lossy(&bare_out.stdout);
    assert!(
        listed.contains("confessions.jsonl"),
        "confessions missing from remote: {listed}"
    );
    assert!(
        !listed.contains("todos.jsonl"),
        "todos should be gitignored, got: {listed}"
    );
    assert!(
        !listed.contains("events.jsonl"),
        "events should be gitignored, got: {listed}"
    );
}
