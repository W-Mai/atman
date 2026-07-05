use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};

pub const GITIGNORE_TEMPLATE: &str = "todos.jsonl
events.jsonl
*.tmp
";

pub struct SyncEnv {
    pub memory_root: PathBuf,
}

impl SyncEnv {
    pub fn discover() -> Result<Self> {
        let cwd = std::env::current_dir().context("read current working directory")?;
        Ok(Self::from_project_root(cwd))
    }

    pub fn from_project_root(project_root: PathBuf) -> Self {
        Self {
            memory_root: project_root.join(".atman"),
        }
    }
}

pub fn ensure_git_available() -> Result<()> {
    let out = Command::new("git").arg("--version").output();
    match out {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => bail!(
            "`git --version` exit {}: {}",
            o.status,
            String::from_utf8_lossy(&o.stderr).trim()
        ),
        Err(e) => bail!("`git` not found on PATH — install git first ({e})"),
    }
}

pub fn init(env: &SyncEnv, remote_url: &str, branch: Option<&str>) -> Result<InitReport> {
    ensure_git_available()?;
    std::fs::create_dir_all(&env.memory_root)
        .with_context(|| format!("mkdir memory root {}", env.memory_root.display()))?;
    let dot_git = env.memory_root.join(".git");
    let already = dot_git.exists();
    if !already {
        run_git(&env.memory_root, &["init"], "git init")?;
    }
    let branch = branch.unwrap_or("main");
    run_git(
        &env.memory_root,
        &["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")],
        "set HEAD to branch",
    )?;
    let has_remote = remote_exists(&env.memory_root, "origin")?;
    if has_remote {
        run_git(
            &env.memory_root,
            &["remote", "set-url", "origin", remote_url],
            "reconfigure origin",
        )?;
    } else {
        run_git(
            &env.memory_root,
            &["remote", "add", "origin", remote_url],
            "add remote origin",
        )?;
    }
    let fetched_existing = fetch_and_checkout_if_remote_has_branch(&env.memory_root, branch)?;
    let gitignore_path = env.memory_root.join(".gitignore");
    let wrote_gitignore = if fetched_existing || gitignore_path.exists() {
        false
    } else {
        std::fs::write(&gitignore_path, GITIGNORE_TEMPLATE)
            .with_context(|| format!("write {}", gitignore_path.display()))?;
        true
    };
    Ok(InitReport {
        already_initialised: already,
        wrote_gitignore,
        remote_url: remote_url.to_string(),
        branch: branch.to_string(),
    })
}

fn fetch_and_checkout_if_remote_has_branch(dir: &Path, branch: &str) -> Result<bool> {
    let fetch = Command::new("git")
        .args(["fetch", "origin", branch])
        .current_dir(dir)
        .output()
        .with_context(|| format!("git fetch origin {branch}"))?;
    if !fetch.status.success() {
        return Ok(false);
    }
    let show = Command::new("git")
        .args([
            "show-ref",
            "--verify",
            &format!("refs/remotes/origin/{branch}"),
        ])
        .current_dir(dir)
        .output()
        .with_context(|| "git show-ref origin/branch")?;
    if !show.status.success() {
        return Ok(false);
    }
    run_git(
        dir,
        &["reset", "--hard", &format!("origin/{branch}")],
        "checkout origin branch",
    )?;
    Ok(true)
}

pub fn push(env: &SyncEnv, message: Option<&str>) -> Result<PushReport> {
    ensure_git_available()?;
    ensure_repo(&env.memory_root)?;
    let has_changes = has_changes(&env.memory_root)?;
    let committed = if has_changes {
        run_git(&env.memory_root, &["add", "."], "git add")?;
        let default_msg = format!("atman sync {}", epoch_seconds());
        let msg = message.unwrap_or(default_msg.as_str());
        run_git(&env.memory_root, &["commit", "-m", msg], "git commit")?;
        true
    } else {
        false
    };
    let branch = current_branch(&env.memory_root)?;
    let pushed_stderr = run_git(
        &env.memory_root,
        &["push", "-u", "origin", branch.as_str()],
        "git push",
    )?;
    Ok(PushReport {
        committed,
        branch,
        stderr_tail: last_line(&pushed_stderr),
    })
}

pub fn pull(env: &SyncEnv) -> Result<String> {
    ensure_git_available()?;
    ensure_repo(&env.memory_root)?;
    let branch = current_branch(&env.memory_root)?;
    run_git(
        &env.memory_root,
        &["pull", "--rebase", "origin", branch.as_str()],
        "git pull",
    )
}

pub fn status(env: &SyncEnv) -> Result<StatusReport> {
    ensure_git_available()?;
    if !env.memory_root.join(".git").exists() {
        return Ok(StatusReport {
            initialised: false,
            porcelain: String::new(),
            branch: None,
        });
    }
    let porcelain = run_git(
        &env.memory_root,
        &["status", "--porcelain=v1"],
        "git status",
    )?;
    let branch = current_branch(&env.memory_root).ok();
    Ok(StatusReport {
        initialised: true,
        porcelain,
        branch,
    })
}

fn run_git(cwd: &Path, args: &[&str], label: &str) -> Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .with_context(|| format!("spawn `git {}`", args.join(" ")))?;
    if !out.status.success() {
        bail!(
            "{label} failed ({} in {}): {}",
            out.status,
            cwd.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn ensure_repo(dir: &Path) -> Result<()> {
    if !dir.join(".git").exists() {
        bail!(
            "{} is not initialised as a memory repo — run `atman sync init <url>` first",
            dir.display()
        );
    }
    Ok(())
}

fn remote_exists(dir: &Path, name: &str) -> Result<bool> {
    let out = Command::new("git")
        .args(["remote"])
        .current_dir(dir)
        .output()
        .with_context(|| format!("list remotes in {}", dir.display()))?;
    if !out.status.success() {
        bail!(
            "git remote list failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .any(|line| line.trim() == name))
}

fn has_changes(dir: &Path) -> Result<bool> {
    let out = run_git(dir, &["status", "--porcelain=v1"], "git status")?;
    Ok(!out.trim().is_empty())
}

fn current_branch(dir: &Path) -> Result<String> {
    let out = run_git(
        dir,
        &["symbolic-ref", "--short", "HEAD"],
        "git symbolic-ref",
    )?;
    Ok(out.trim().to_string())
}

fn last_line(s: &str) -> String {
    s.lines().last().unwrap_or("").to_string()
}

fn epoch_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[derive(Debug, Clone)]
pub struct InitReport {
    pub already_initialised: bool,
    pub wrote_gitignore: bool,
    pub remote_url: String,
    pub branch: String,
}

#[derive(Debug, Clone)]
pub struct PushReport {
    pub committed: bool,
    pub branch: String,
    pub stderr_tail: String,
}

#[derive(Debug, Clone)]
pub struct StatusReport {
    pub initialised: bool,
    pub porcelain: String,
    pub branch: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitignore_template_covers_ephemeral_paths() {
        assert!(GITIGNORE_TEMPLATE.contains("todos.jsonl"));
        assert!(GITIGNORE_TEMPLATE.contains("events.jsonl"));
    }

    #[test]
    fn discover_reads_current_working_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let env = SyncEnv::from_project_root(tmp.path().to_path_buf());
        assert_eq!(env.memory_root, tmp.path().join(".atman"));
    }
}
