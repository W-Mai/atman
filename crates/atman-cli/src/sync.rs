use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use atman_runtime::git::{self, GitCli};

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
    GitCli::ensure_available()
        .map_err(|e| anyhow::anyhow!("`git` not found on PATH — install git first ({e})"))
}

pub fn init(env: &SyncEnv, remote_url: &str, branch: Option<&str>) -> Result<InitReport> {
    ensure_git_available()?;
    std::fs::create_dir_all(&env.memory_root)
        .with_context(|| format!("mkdir memory root {}", env.memory_root.display()))?;
    let cli = GitCli::at(&env.memory_root);
    let already = env.memory_root.join(".git").exists();
    let branch = branch.unwrap_or("main");
    if !already {
        cli.init(branch)
            .with_context(|| format!("git init {}", env.memory_root.display()))?;
    } else {
        cli.run(&["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")])
            .with_context(|| "set HEAD to branch")?;
    }
    let has_remote = cli.remote_exists("origin").context("list remotes")?;
    if has_remote {
        cli.remote_set_url("origin", remote_url)
            .context("reconfigure origin")?;
    } else {
        cli.remote_add("origin", remote_url)
            .context("add remote origin")?;
    }
    let fetched_existing = fetch_and_checkout_if_remote_has_branch(&cli, branch)?;
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

fn fetch_and_checkout_if_remote_has_branch(cli: &GitCli, branch: &str) -> Result<bool> {
    if cli.fetch("origin", branch).is_err() {
        return Ok(false);
    }
    if !cli
        .ref_exists(&format!("refs/remotes/origin/{branch}"))
        .context("show-ref origin/branch")?
    {
        return Ok(false);
    }
    cli.reset_hard(&format!("origin/{branch}"))
        .context("checkout origin branch")?;
    Ok(true)
}

pub fn push(env: &SyncEnv, message: Option<&str>) -> Result<PushReport> {
    ensure_git_available()?;
    ensure_repo(&env.memory_root)?;
    let cli = GitCli::at(&env.memory_root);
    let has_changes = git::has_changes(&env.memory_root)
        .with_context(|| format!("read status of {}", env.memory_root.display()))?;
    let committed = if has_changes {
        cli.add_all().context("git add")?;
        let default_msg = format!("atman sync {}", epoch_seconds());
        let msg = message.unwrap_or(default_msg.as_str());
        cli.commit(msg).context("git commit")?;
        true
    } else {
        false
    };
    let branch = git::current_branch(&env.memory_root).with_context(|| "read HEAD branch")?;
    let pushed_stderr = cli.push("origin", &branch).context("git push")?;
    Ok(PushReport {
        committed,
        branch,
        stderr_tail: last_line(&pushed_stderr),
    })
}

pub fn pull(env: &SyncEnv) -> Result<String> {
    ensure_git_available()?;
    ensure_repo(&env.memory_root)?;
    let cli = GitCli::at(&env.memory_root);
    let branch = git::current_branch(&env.memory_root).with_context(|| "read HEAD branch")?;
    cli.pull_rebase("origin", &branch).context("git pull")
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
    let porcelain = git::status_porcelain(&env.memory_root)
        .with_context(|| format!("read status of {}", env.memory_root.display()))?;
    let branch = git::current_branch(&env.memory_root).ok();
    Ok(StatusReport {
        initialised: true,
        porcelain,
        branch,
    })
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
