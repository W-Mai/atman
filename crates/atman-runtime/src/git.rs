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
}

pub fn discover_toplevel(start: &Path) -> Result<PathBuf> {
    let repo =
        git2::Repository::discover(start).map_err(|_| GitError::NotARepo(start.to_path_buf()))?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| GitError::NotARepo(start.to_path_buf()))?;
    Ok(workdir.to_path_buf())
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
        self.run(&["commit", "-m", message]).map(|_| ())
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

#[cfg(test)]
mod tests {
    use super::*;

    fn have_git() -> bool {
        GitCli::ensure_available().is_ok()
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
