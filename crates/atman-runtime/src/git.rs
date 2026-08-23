use std::path::{Path, PathBuf};
use std::process::Output;

pub type Result<T> = std::result::Result<T, GitError>;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git binary not available: {0}")]
    NotAvailable(String),
    #[error("git spawn failed: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("git {args} exit {code}: {stderr}")]
    ExitNonZero {
        args: String,
        code: i32,
        stderr: String,
    },
    #[error("libgit2: {0}")]
    Libgit2(#[from] git2::Error),
    #[error("not a git repository at {0}")]
    NotARepo(PathBuf),
    #[error("invalid worktree operation: {0}")]
    InvalidWorktree(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: Option<String>,
    pub prunable: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepositoryInfo {
    pub path: PathBuf,
    pub workdir: Option<PathBuf>,
    pub git_dir: PathBuf,
    pub bare: bool,
}

impl RepositoryInfo {
    fn from_repository(repo: &git2::Repository, requested: &Path) -> Self {
        Self {
            path: canonical_path(requested),
            workdir: repo.workdir().map(canonical_path),
            git_dir: canonical_path(repo.path()),
            bare: repo.is_bare(),
        }
    }
}

fn canonical_path(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

pub fn init_repository(
    path: &Path,
    bare: bool,
    initial_branch: Option<&str>,
) -> Result<RepositoryInfo> {
    let mut options = git2::RepositoryInitOptions::new();
    options.bare(bare);
    if let Some(branch) = initial_branch {
        options.initial_head(branch);
    }
    let repo = git2::Repository::init_opts(path, &options)?;
    Ok(RepositoryInfo::from_repository(&repo, path))
}

pub fn discover_repository(start: &Path) -> Result<RepositoryInfo> {
    let repo =
        git2::Repository::discover(start).map_err(|_| GitError::NotARepo(start.to_path_buf()))?;
    Ok(RepositoryInfo::from_repository(&repo, start))
}

pub fn discover_toplevel(start: &Path) -> Result<PathBuf> {
    discover_repository(start)?
        .workdir
        .ok_or_else(|| GitError::NotARepo(start.to_path_buf()))
}

pub fn diff_range(cwd: &Path, range: &str, paths: &[String]) -> Result<DiffResult> {
    let repo = git2::Repository::open(cwd).map_err(|_| GitError::NotARepo(cwd.to_path_buf()))?;
    let revspec = repo.revparse(range)?;
    let from = revspec
        .from()
        .ok_or_else(|| GitError::Libgit2(git2::Error::from_str("revspec missing 'from'")))?
        .peel_to_commit()?
        .tree()?;
    let to = revspec
        .to()
        .map(|t| t.peel_to_commit().and_then(|c| c.tree()))
        .transpose()?;

    let mut opts = git2::DiffOptions::new();
    for p in paths {
        opts.pathspec(p);
    }
    let diff = match to {
        Some(to_tree) => repo.diff_tree_to_tree(Some(&from), Some(&to_tree), Some(&mut opts))?,
        None => repo.diff_tree_to_workdir_with_index(Some(&from), Some(&mut opts))?,
    };

    let mut files = Vec::new();
    diff.foreach(
        &mut |delta, _| {
            let path = delta
                .new_file()
                .path()
                .or_else(|| delta.old_file().path())
                .map(|p| p.to_string_lossy().into_owned());
            if let Some(p) = path {
                if !files.contains(&p) {
                    files.push(p);
                }
            }
            true
        },
        None,
        None,
        None,
    )?;

    let mut body = String::new();
    diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
        match line.origin() {
            'F' | 'H' => body.push_str(&String::from_utf8_lossy(line.content())),
            '+' | '-' | ' ' => {
                body.push(line.origin());
                body.push_str(&String::from_utf8_lossy(line.content()));
            }
            _ => body.push_str(&String::from_utf8_lossy(line.content())),
        }
        true
    })?;

    Ok(DiffResult { body, files })
}

pub struct DiffResult {
    pub body: String,
    pub files: Vec<String>,
}

pub fn status_porcelain(cwd: &Path) -> Result<String> {
    let repo = git2::Repository::open(cwd).map_err(|_| GitError::NotARepo(cwd.to_path_buf()))?;
    let mut opts = git2::StatusOptions::new();
    opts.include_untracked(true).include_ignored(false);
    let statuses = repo.statuses(Some(&mut opts))?;
    let mut out = String::new();
    for s in statuses.iter() {
        let bits = s.status();
        let (index_c, wt_c) = if bits.contains(git2::Status::WT_NEW)
            && !bits.intersects(
                git2::Status::INDEX_NEW
                    | git2::Status::INDEX_MODIFIED
                    | git2::Status::INDEX_DELETED
                    | git2::Status::INDEX_RENAMED
                    | git2::Status::INDEX_TYPECHANGE,
            ) {
            ('?', '?')
        } else {
            (index_flag(bits), worktree_flag(bits))
        };
        let path = s.path().unwrap_or("").to_string();
        out.push(index_c);
        out.push(wt_c);
        out.push(' ');
        out.push_str(&path);
        out.push('\n');
    }
    Ok(out)
}

pub fn has_changes(cwd: &Path) -> Result<bool> {
    Ok(!status_porcelain(cwd)?.trim().is_empty())
}

pub fn current_branch(cwd: &Path) -> Result<String> {
    let repo = git2::Repository::open(cwd).map_err(|_| GitError::NotARepo(cwd.to_path_buf()))?;
    let head_ref = repo.find_reference("HEAD")?;
    let sym = head_ref
        .symbolic_target()
        .ok_or_else(|| GitError::Libgit2(git2::Error::from_str("HEAD is not symbolic")))?;
    Ok(sym.strip_prefix("refs/heads/").unwrap_or(sym).to_string())
}

fn index_flag(s: git2::Status) -> char {
    if s.contains(git2::Status::INDEX_NEW) {
        'A'
    } else if s.contains(git2::Status::INDEX_MODIFIED) {
        'M'
    } else if s.contains(git2::Status::INDEX_DELETED) {
        'D'
    } else if s.contains(git2::Status::INDEX_RENAMED) {
        'R'
    } else if s.contains(git2::Status::INDEX_TYPECHANGE) {
        'T'
    } else {
        ' '
    }
}

fn worktree_flag(s: git2::Status) -> char {
    if s.contains(git2::Status::WT_NEW) {
        '?'
    } else if s.contains(git2::Status::WT_MODIFIED) {
        'M'
    } else if s.contains(git2::Status::WT_DELETED) {
        'D'
    } else if s.contains(git2::Status::WT_RENAMED) {
        'R'
    } else if s.contains(git2::Status::WT_TYPECHANGE) {
        'T'
    } else {
        ' '
    }
}

pub struct GitCli {
    cwd: PathBuf,
}

impl GitCli {
    pub fn at(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn ensure_available() -> Result<()> {
        let out = std::process::Command::new("git").arg("--version").output();
        match out {
            Ok(o) if o.status.success() => Ok(()),
            Ok(o) => Err(GitError::NotAvailable(format!(
                "git --version exit {}",
                o.status
            ))),
            Err(e) => Err(GitError::NotAvailable(format!("spawn: {e}"))),
        }
    }

    pub fn worktree_list(&self) -> Result<Vec<WorktreeInfo>> {
        parse_worktree_porcelain(&self.run(&["worktree", "list", "--porcelain"])?)
    }

    pub fn worktree_add(
        &self,
        path: &Path,
        branch: Option<&str>,
        base: Option<&str>,
        create_branch: bool,
        detach: bool,
    ) -> Result<WorktreeInfo> {
        if create_branch && branch.is_none() {
            return Err(GitError::InvalidWorktree(
                "create_branch requires branch".into(),
            ));
        }
        if detach && (create_branch || branch.is_some()) {
            return Err(GitError::InvalidWorktree(
                "detach cannot be combined with branch or create_branch".into(),
            ));
        }
        let target = canonicalize_new_path(path)?;
        if target.exists() {
            return Err(GitError::InvalidWorktree(format!(
                "target path already exists: {}",
                target.display()
            )));
        }
        let worktrees = self.worktree_list()?;
        if worktrees.iter().any(|entry| entry.path == target) {
            return Err(GitError::InvalidWorktree(format!(
                "worktree path already registered: {}",
                target.display()
            )));
        }
        if let Some(branch) = branch {
            let full_branch = format!("refs/heads/{branch}");
            if worktrees
                .iter()
                .any(|entry| entry.branch.as_deref() == Some(full_branch.as_str()))
            {
                return Err(GitError::InvalidWorktree(format!(
                    "branch is already checked out: {branch}"
                )));
            }
            let repo = git2::Repository::discover(&self.cwd)
                .map_err(|_| GitError::NotARepo(self.cwd.clone()))?;
            let exists = repo.find_branch(branch, git2::BranchType::Local).is_ok();
            if create_branch == exists {
                let state = if exists {
                    "already exists"
                } else {
                    "does not exist"
                };
                return Err(GitError::InvalidWorktree(format!(
                    "branch {branch} {state}"
                )));
            }
        }

        let path_arg = target.to_string_lossy().into_owned();
        let mut owned = vec!["worktree".to_string(), "add".to_string()];
        if detach {
            owned.push("--detach".into());
        } else if create_branch {
            owned.push("-b".into());
            owned.push(branch.expect("validated branch").into());
        }
        owned.push("--".into());
        owned.push(path_arg);
        if !create_branch {
            if let Some(branch) = branch {
                owned.push(branch.into());
            } else if let Some(base) = base {
                owned.push(base.into());
            }
        } else if let Some(base) = base {
            owned.push(base.into());
        }
        let args: Vec<&str> = owned.iter().map(String::as_str).collect();
        self.run(&args)?;
        self.worktree_list()?
            .into_iter()
            .find(|entry| entry.path == target)
            .ok_or_else(|| GitError::InvalidWorktree("created worktree was not listed".into()))
    }

    pub fn worktree_remove(&self, path: &Path, force: bool) -> Result<()> {
        let target = self.known_worktree(path, true)?;
        let repo = discover_repository(&self.cwd)?;
        if repo.workdir.as_deref() == Some(target.path.as_path()) || target.bare {
            return Err(GitError::InvalidWorktree(
                "refusing to remove the main worktree or repository root".into(),
            ));
        }
        if !force
            && !GitCli::at(&target.path)
                .run(&["status", "--porcelain"])?
                .is_empty()
        {
            return Err(GitError::InvalidWorktree(
                "worktree has uncommitted changes; set force=true to remove it".into(),
            ));
        }
        let target_arg = target.path.to_string_lossy().into_owned();
        let mut args = vec!["worktree", "remove"];
        if force {
            args.push("--force");
        }
        args.extend(["--", target_arg.as_str()]);
        self.run(&args).map(|_| ())
    }

    pub fn worktree_prune(&self, dry_run: bool) -> Result<String> {
        let mut args = vec!["worktree", "prune", "--verbose"];
        if dry_run {
            args.push("--dry-run");
        }
        self.run(&args)
    }

    pub fn worktree_lock(&self, path: &Path, reason: Option<&str>) -> Result<()> {
        let target = self.known_worktree(path, false)?;
        let target_arg = target.path.to_string_lossy().into_owned();
        let mut args = vec!["worktree", "lock"];
        if let Some(reason) = reason {
            args.extend(["--reason", reason]);
        }
        args.extend(["--", target_arg.as_str()]);
        self.run(&args).map(|_| ())
    }

    pub fn worktree_unlock(&self, path: &Path) -> Result<()> {
        let target = self.known_worktree(path, false)?;
        let target_arg = target.path.to_string_lossy().into_owned();
        self.run(&["worktree", "unlock", "--", &target_arg])
            .map(|_| ())
    }

    fn known_worktree(&self, path: &Path, allow_missing: bool) -> Result<WorktreeInfo> {
        let target = if allow_missing && !path.exists() {
            canonicalize_new_path(path)?
        } else {
            path.canonicalize().map_err(|error| {
                GitError::InvalidWorktree(format!(
                    "cannot resolve worktree path {}: {error}",
                    path.display()
                ))
            })?
        };
        self.worktree_list()?
            .into_iter()
            .find(|entry| entry.path == target)
            .ok_or_else(|| {
                GitError::InvalidWorktree(format!(
                    "path is not a registered worktree: {}",
                    target.display()
                ))
            })
    }

    pub fn run(&self, args: &[&str]) -> Result<String> {
        let out = self.spawn(args)?;
        if !out.status.success() {
            return Err(GitError::ExitNonZero {
                args: args.join(" "),
                code: out.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            });
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    fn spawn(&self, args: &[&str]) -> Result<Output> {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(&self.cwd)
            .output()?;
        Ok(out)
    }

    pub fn init(&self, branch: &str) -> Result<()> {
        std::fs::create_dir_all(&self.cwd)?;
        self.run(&["init"])?;
        self.run(&["symbolic-ref", "HEAD", &format!("refs/heads/{branch}")])?;
        Ok(())
    }

    pub fn add_all(&self) -> Result<()> {
        self.run(&["add", "."]).map(|_| ())
    }

    pub fn commit(&self, message: &str) -> Result<()> {
        self.commit_with_options(message, false).map(|_| ())
    }

    pub fn commit_with_options(&self, message: &str, amend: bool) -> Result<String> {
        let mut args = vec!["commit", "-m", message];
        if amend {
            args.insert(1, "--amend");
        }
        self.run(&args)
    }

    pub fn head_oid(&self) -> Result<String> {
        self.run(&["rev-parse", "HEAD"])
            .map(|output| output.trim().to_string())
    }

    pub fn push(&self, remote: &str, branch: &str) -> Result<String> {
        self.run(&["push", "-u", remote, branch])
    }

    pub fn pull_rebase(&self, remote: &str, branch: &str) -> Result<String> {
        self.run(&["pull", "--rebase", remote, branch])
    }

    pub fn fetch(&self, remote: &str, branch: &str) -> Result<()> {
        self.run(&["fetch", remote, branch]).map(|_| ())
    }

    pub fn reset_hard(&self, target: &str) -> Result<()> {
        self.run(&["reset", "--hard", target]).map(|_| ())
    }

    pub fn ref_exists(&self, refname: &str) -> Result<bool> {
        match self.spawn(&["show-ref", "--verify", refname])? {
            o if o.status.success() => Ok(true),
            _ => Ok(false),
        }
    }

    pub fn remote_exists(&self, name: &str) -> Result<bool> {
        let text = self.run(&["remote"])?;
        Ok(text.lines().any(|l| l.trim() == name))
    }

    pub fn remote_add(&self, name: &str, url: &str) -> Result<()> {
        self.run(&["remote", "add", name, url]).map(|_| ())
    }

    pub fn remote_set_url(&self, name: &str, url: &str) -> Result<()> {
        self.run(&["remote", "set-url", name, url]).map(|_| ())
    }
}

pub fn parse_worktree_porcelain(input: &str) -> Result<Vec<WorktreeInfo>> {
    let mut entries = Vec::new();
    let mut current: Option<WorktreeInfo> = None;
    for line in input.lines().chain(std::iter::once("")) {
        if line.is_empty() {
            if let Some(entry) = current.take() {
                entries.push(entry);
            }
            continue;
        }
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if key == "worktree" {
            if current.is_some() {
                return Err(GitError::InvalidWorktree(
                    "malformed porcelain: missing record separator".into(),
                ));
            }
            current = Some(WorktreeInfo {
                path: PathBuf::from(value),
                head: None,
                branch: None,
                detached: false,
                bare: false,
                locked: None,
                prunable: None,
            });
            continue;
        }
        let entry = current.as_mut().ok_or_else(|| {
            GitError::InvalidWorktree(format!("malformed porcelain: {key} before worktree"))
        })?;
        match key {
            "HEAD" => entry.head = Some(value.into()),
            "branch" => entry.branch = Some(value.into()),
            "detached" => entry.detached = true,
            "bare" => entry.bare = true,
            "locked" => entry.locked = Some(value.into()),
            "prunable" => entry.prunable = Some(value.into()),
            _ => {}
        }
    }
    Ok(entries)
}

fn canonicalize_new_path(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(GitError::InvalidWorktree("worktree path is empty".into()));
    }
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let name = absolute.file_name().ok_or_else(|| {
        GitError::InvalidWorktree(format!("invalid worktree path: {}", path.display()))
    })?;
    let parent = absolute.parent().ok_or_else(|| {
        GitError::InvalidWorktree(format!("invalid worktree path: {}", path.display()))
    })?;
    let parent = parent.canonicalize().map_err(|error| {
        GitError::InvalidWorktree(format!(
            "cannot resolve worktree parent {}: {error}",
            parent.display()
        ))
    })?;
    Ok(parent.join(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn have_git() -> bool {
        GitCli::ensure_available().is_ok()
    }

    #[test]
    fn parses_worktree_porcelain_states() {
        let parsed = parse_worktree_porcelain(
            "worktree /repo\nHEAD abc123\nbranch refs/heads/main\n\nworktree /tmp/detached\nHEAD def456\ndetached\nlocked maintenance window\nprunable gitdir file points to non-existent location\n\n",
        )
        .unwrap();

        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].branch.as_deref(), Some("refs/heads/main"));
        assert!(!parsed[0].detached);
        assert!(parsed[1].detached);
        assert_eq!(parsed[1].locked.as_deref(), Some("maintenance window"));
        assert_eq!(
            parsed[1].prunable.as_deref(),
            Some("gitdir file points to non-existent location")
        );
    }

    #[test]
    fn rejects_malformed_worktree_porcelain() {
        let error = parse_worktree_porcelain("HEAD abc123\n").unwrap_err();
        assert!(error.to_string().contains("before worktree"));
    }

    fn seed_two_commits(dir: &Path) {
        let cli = GitCli::at(dir);
        cli.init("main").unwrap();
        for (k, v) in [
            ("user.email", "t@atman.local"),
            ("user.name", "atman test"),
            ("commit.gpgsign", "false"),
        ] {
            cli.run(&["config", k, v]).unwrap();
        }
        std::fs::write(dir.join("a.txt"), "line one\n").unwrap();
        std::fs::write(dir.join("b.txt"), "b\n").unwrap();
        cli.add_all().unwrap();
        cli.commit("initial").unwrap();
        std::fs::write(dir.join("a.txt"), "line one\nline two\n").unwrap();
        std::fs::write(dir.join("c.txt"), "new file\n").unwrap();
        cli.add_all().unwrap();
        cli.commit("second").unwrap();
    }

    #[test]
    fn init_repository_supports_empty_and_non_empty_directories() {
        let empty = tempfile::tempdir().unwrap();
        let info = init_repository(empty.path(), false, None).unwrap();
        assert!(!info.bare);
        assert_eq!(info.workdir, Some(empty.path().canonicalize().unwrap()));
        assert!(info.git_dir.is_dir());

        let non_empty = tempfile::tempdir().unwrap();
        std::fs::write(non_empty.path().join("README.md"), "content\n").unwrap();
        let info = init_repository(non_empty.path(), false, None).unwrap();
        assert!(!info.bare);
        assert_eq!(info.workdir, Some(non_empty.path().canonicalize().unwrap()));
    }

    #[test]
    fn init_repository_sets_initial_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let info = init_repository(tmp.path(), false, Some("trunk")).unwrap();
        let repo = git2::Repository::open(info.path).unwrap();
        assert_eq!(
            repo.find_reference("HEAD").unwrap().symbolic_target(),
            Some("refs/heads/trunk")
        );
    }

    #[test]
    fn init_repository_supports_bare_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let info = init_repository(tmp.path(), true, None).unwrap();
        assert!(info.bare);
        assert_eq!(info.workdir, None);
        assert_eq!(info.git_dir, tmp.path().canonicalize().unwrap());
        assert!(tmp.path().join("HEAD").is_file());
    }

    #[test]
    fn git_init_discover_reports_worktree_and_bare_repositories() {
        let worktree = tempfile::tempdir().unwrap();
        init_repository(worktree.path(), false, None).unwrap();
        let nested = worktree.path().join("nested");
        std::fs::create_dir(&nested).unwrap();
        let info = discover_repository(&nested).unwrap();
        assert!(!info.bare);
        assert_eq!(
            std::fs::canonicalize(info.workdir.as_ref().unwrap()).unwrap(),
            std::fs::canonicalize(worktree.path()).unwrap()
        );

        let bare = tempfile::tempdir().unwrap();
        init_repository(bare.path(), true, None).unwrap();
        let info = discover_repository(bare.path()).unwrap();
        assert!(info.bare);
        assert_eq!(info.workdir, None);
    }

    #[test]
    fn discover_toplevel_finds_repo_root_from_subdir() {
        if !have_git() {
            eprintln!("skip: git not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        seed_two_commits(tmp.path());
        let sub = tmp.path().join("nested/deep");
        std::fs::create_dir_all(&sub).unwrap();
        let root = discover_toplevel(&sub).unwrap();
        assert_eq!(
            std::fs::canonicalize(&root).unwrap(),
            std::fs::canonicalize(tmp.path()).unwrap()
        );
    }

    #[test]
    fn discover_toplevel_outside_repo_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let err = discover_toplevel(tmp.path()).unwrap_err();
        assert!(matches!(err, GitError::NotARepo(_)), "got {err:?}");
    }

    #[test]
    fn diff_range_reports_files_and_body() {
        if !have_git() {
            eprintln!("skip: git not on PATH");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        seed_two_commits(tmp.path());
        let out = diff_range(tmp.path(), "HEAD~1..HEAD", &[]).unwrap();
        assert!(
            out.body.contains("+line two"),
            "want addition, got:\n{}",
            out.body
        );
        assert!(
            out.body.contains("+new file"),
            "want new file body:\n{}",
            out.body
        );
        assert!(
            out.files.contains(&"a.txt".to_string()),
            "files={:?}",
            out.files
        );
        assert!(
            out.files.contains(&"c.txt".to_string()),
            "files={:?}",
            out.files
        );
    }

    #[test]
    fn diff_range_paths_filter_narrows() {
        if !have_git() {
            eprintln!("skip");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        seed_two_commits(tmp.path());
        let out = diff_range(tmp.path(), "HEAD~1..HEAD", &["a.txt".to_string()]).unwrap();
        assert_eq!(
            out.files,
            vec!["a.txt".to_string()],
            "files={:?}",
            out.files
        );
    }

    #[test]
    fn status_porcelain_reflects_worktree_changes() {
        if !have_git() {
            eprintln!("skip");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        seed_two_commits(tmp.path());
        assert!(!has_changes(tmp.path()).unwrap(), "clean tree");
        std::fs::write(tmp.path().join("a.txt"), "changed\n").unwrap();
        std::fs::write(tmp.path().join("d.txt"), "new\n").unwrap();
        let text = status_porcelain(tmp.path()).unwrap();
        assert!(text.contains(" M a.txt"), "want dirty a.txt: {text}");
        assert!(text.contains("?? d.txt"), "want untracked d.txt: {text}");
        assert!(has_changes(tmp.path()).unwrap());
    }

    #[test]
    fn worktree_lifecycle_enforces_safety_checks() {
        if !have_git() {
            eprintln!("skip");
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let repo_path = root.path().join("repo");
        std::fs::create_dir(&repo_path).unwrap();
        seed_two_commits(&repo_path);
        let worktree_path = root.path().join("feature-worktree");
        let cli = GitCli::at(&repo_path);

        let added = cli
            .worktree_add(&worktree_path, Some("feature"), Some("HEAD"), true, false)
            .unwrap();
        assert_eq!(added.path, worktree_path.canonicalize().unwrap());
        assert_eq!(added.branch.as_deref(), Some("refs/heads/feature"));
        assert_eq!(cli.worktree_list().unwrap().len(), 2);

        cli.worktree_lock(&worktree_path, Some("test lock"))
            .unwrap();
        assert_eq!(
            cli.worktree_list().unwrap()[1].locked.as_deref(),
            Some("test lock")
        );
        cli.worktree_unlock(&worktree_path).unwrap();

        std::fs::write(worktree_path.join("dirty.txt"), "dirty\n").unwrap();
        let error = cli.worktree_remove(&worktree_path, false).unwrap_err();
        assert!(error.to_string().contains("uncommitted changes"));
        assert!(worktree_path.exists());

        cli.worktree_remove(&worktree_path, true).unwrap();
        assert!(!worktree_path.exists());
        assert_eq!(cli.worktree_list().unwrap().len(), 1);
        cli.worktree_prune(true).unwrap();
    }

    #[test]
    fn current_branch_after_first_commit_is_main() {
        if !have_git() {
            eprintln!("skip");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        seed_two_commits(tmp.path());
        assert_eq!(current_branch(tmp.path()).unwrap(), "main");
    }

    #[test]
    fn git_cli_remote_add_and_lookup() {
        if !have_git() {
            eprintln!("skip");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let cli = GitCli::at(tmp.path());
        cli.init("main").unwrap();
        assert!(!cli.remote_exists("origin").unwrap());
        cli.remote_add("origin", "https://example.invalid/repo.git")
            .unwrap();
        assert!(cli.remote_exists("origin").unwrap());
        cli.remote_set_url("origin", "https://example.invalid/other.git")
            .unwrap();
        let list = cli.run(&["remote", "get-url", "origin"]).unwrap();
        assert!(list.contains("other.git"), "want reset url: {list}");
    }

    #[test]
    fn git_cli_commit_honors_amend_and_hooks() {
        if !have_git() {
            eprintln!("skip");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let cli = GitCli::at(tmp.path());
        cli.init("main").unwrap();
        cli.run(&["config", "user.name", "atman test"]).unwrap();
        cli.run(&["config", "user.email", "test@atman.local"])
            .unwrap();
        std::fs::write(tmp.path().join("file.txt"), "one\n").unwrap();
        cli.add_all().unwrap();
        cli.commit_with_options("first\n\nbody", false).unwrap();
        let first = cli.head_oid().unwrap();
        std::fs::write(tmp.path().join("file.txt"), "two\n").unwrap();
        cli.add_all().unwrap();
        cli.commit_with_options("amended", true).unwrap();
        assert_ne!(first, cli.head_oid().unwrap());
        assert_eq!(
            cli.run(&["log", "-1", "--format=%B"]).unwrap().trim(),
            "amended"
        );

        let hook = tmp.path().join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 17\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(tmp.path().join("file.txt"), "three\n").unwrap();
        cli.add_all().unwrap();
        let err = cli.commit_with_options("blocked", false).unwrap_err();
        assert!(matches!(err, GitError::ExitNonZero { .. }));
    }

    #[test]
    fn git_cli_run_maps_exit_code_to_error() {
        if !have_git() {
            eprintln!("skip");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let cli = GitCli::at(tmp.path());
        let err = cli.run(&["diff", "HEAD"]).unwrap_err();
        match err {
            GitError::ExitNonZero { code, .. } => assert_ne!(code, 0),
            other => panic!("want ExitNonZero, got {other:?}"),
        }
    }
}
