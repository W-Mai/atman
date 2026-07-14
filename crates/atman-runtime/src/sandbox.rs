use std::path::{Path, PathBuf};

use crate::error::RuntimeError;
use crate::tool::BoxFut;

pub struct PtySpawnResult {
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    pub reader: Box<dyn std::io::Read + Send>,
    pub writer: Box<dyn std::io::Write + Send>,
    pub master: Box<dyn portable_pty::MasterPty + Send>,
}

pub trait Sandbox: Send + Sync {
    fn spawn<'a>(
        &'a self,
        cmd: &'a [&'a str],
        env: &'a [(String, String)],
        cwd: &'a Path,
    ) -> BoxFut<'a, Result<std::process::Output, RuntimeError>>;

    fn spawn_relaxed<'a>(
        &'a self,
        cmd: &'a [&'a str],
        env: &'a [(String, String)],
        cwd: &'a Path,
    ) -> BoxFut<'a, Result<std::process::Output, RuntimeError>> {
        self.spawn(cmd, env, cwd)
    }

    fn spawn_pty<'a>(
        &'a self,
        cmd: &'a [&'a str],
        env: &'a [(String, String)],
        cwd: &'a Path,
        pty_size: portable_pty::PtySize,
    ) -> BoxFut<'a, Result<PtySpawnResult, RuntimeError>>;

    fn spawn_pty_relaxed<'a>(
        &'a self,
        cmd: &'a [&'a str],
        env: &'a [(String, String)],
        cwd: &'a Path,
        pty_size: portable_pty::PtySize,
    ) -> BoxFut<'a, Result<PtySpawnResult, RuntimeError>> {
        self.spawn_pty(cmd, env, cwd, pty_size)
    }

    fn is_available(&self) -> bool;

    fn kind(&self) -> &'static str;
}

pub struct SandboxExec {
    project_root: PathBuf,
    extra_read: Vec<PathBuf>,
    extra_write: Vec<PathBuf>,
    profile_template: String,
    allow_network: bool,
    relaxed_template: String,
}

impl SandboxExec {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
            extra_read: Vec::new(),
            extra_write: Vec::new(),
            profile_template: DEFAULT_PROFILE.to_string(),
            allow_network: false,
            relaxed_template: RELAXED_PROFILE.to_string(),
        }
    }

    pub fn with_extra_read(mut self, roots: Vec<PathBuf>) -> Self {
        self.extra_read = roots;
        self
    }

    pub fn with_extra_write(mut self, roots: Vec<PathBuf>) -> Self {
        self.extra_write = roots;
        self
    }

    pub fn with_template(mut self, template: impl Into<String>) -> Self {
        self.profile_template = template.into();
        self
    }

    pub fn with_allow_network(mut self, allow: bool) -> Self {
        self.allow_network = allow;
        self
    }

    pub fn render_profile(&self, cwd: &Path) -> String {
        render_template(
            &self.profile_template,
            &self.project_root,
            cwd,
            &self.extra_read,
            &self.extra_write,
            self.allow_network,
        )
    }
}

fn render_template(
    template: &str,
    project_root: &Path,
    cwd: &Path,
    extra_read: &[PathBuf],
    extra_write: &[PathBuf],
    allow_network: bool,
) -> String {
    let mut out = template
        .replace("{PROJECT_ROOT}", &project_root.display().to_string())
        .replace("{CWD}", &cwd.display().to_string());
    if allow_network && !out.contains("(allow network") {
        out.push_str("\n(allow network*)\n");
    }
    if !extra_read.is_empty() {
        let mut extra = String::from("\n;; extra_read\n");
        for r in extra_read {
            extra.push_str(&format!(
                "(allow file-read* (subpath \"{}\"))\n",
                r.display()
            ));
        }
        out.push_str(&extra);
    }
    if !extra_write.is_empty() {
        let mut extra = String::from("\n;; extra_write\n");
        for r in extra_write {
            extra.push_str(&format!(
                "(allow file-write* (subpath \"{}\"))\n",
                r.display()
            ));
        }
        out.push_str(&extra);
    }
    out
}

impl Sandbox for SandboxExec {
    fn spawn<'a>(
        &'a self,
        cmd: &'a [&'a str],
        env: &'a [(String, String)],
        cwd: &'a Path,
    ) -> BoxFut<'a, Result<std::process::Output, RuntimeError>> {
        Box::pin(async move {
            if !self.is_available() {
                return Err(RuntimeError::ToolFailed(
                    "sandbox-exec not available on this host".into(),
                ));
            }
            let profile = self.render_profile(cwd);
            let dir = std::env::temp_dir();
            let profile_path = dir.join(format!("atman-sandbox-{}.sb", uuid::Uuid::new_v4()));
            tokio::fs::write(&profile_path, profile)
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("write .sb: {e}")))?;
            let mut command = tokio::process::Command::new("/usr/bin/sandbox-exec");
            command
                .arg("-f")
                .arg(&profile_path)
                .args(cmd)
                .current_dir(cwd);
            for (k, v) in env {
                command.env(k, v);
            }
            let output = command
                .output()
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("sandbox-exec spawn: {e}")));
            let _ = tokio::fs::remove_file(&profile_path).await;
            output
        })
    }

    fn is_available(&self) -> bool {
        cfg!(target_os = "macos") && std::path::Path::new("/usr/bin/sandbox-exec").exists()
    }

    fn kind(&self) -> &'static str {
        "sandbox-exec"
    }

    fn spawn_relaxed<'a>(
        &'a self,
        cmd: &'a [&'a str],
        env: &'a [(String, String)],
        cwd: &'a Path,
    ) -> BoxFut<'a, Result<std::process::Output, RuntimeError>> {
        Box::pin(async move {
            if !self.is_available() {
                return Err(RuntimeError::ToolFailed(
                    "sandbox-exec not available on this host".into(),
                ));
            }
            let template = self.relaxed_template.clone();
            let profile = render_template(
                &template,
                &self.project_root,
                cwd,
                &self.extra_read,
                &self.extra_write,
                self.allow_network,
            );
            let dir = std::env::temp_dir();
            let profile_path = dir.join(format!("atman-sandbox-{}.sb", uuid::Uuid::new_v4()));
            tokio::fs::write(&profile_path, profile)
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("write .sb: {e}")))?;
            let mut command = tokio::process::Command::new("/usr/bin/sandbox-exec");
            command
                .arg("-f")
                .arg(&profile_path)
                .args(cmd)
                .current_dir(cwd);
            for (k, v) in env {
                command.env(k, v);
            }
            let output = command
                .output()
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("sandbox-exec spawn: {e}")));
            let _ = tokio::fs::remove_file(&profile_path).await;
            output
        })
    }

    fn spawn_pty<'a>(
        &'a self,
        cmd: &'a [&'a str],
        env: &'a [(String, String)],
        cwd: &'a Path,
        pty_size: portable_pty::PtySize,
    ) -> BoxFut<'a, Result<PtySpawnResult, RuntimeError>> {
        Box::pin(async move {
            if !self.is_available() {
                return Err(RuntimeError::ToolFailed(
                    "sandbox-exec not available on this host".into(),
                ));
            }
            let profile = self.render_profile(cwd);
            spawn_pty_with_profile("/usr/bin/sandbox-exec", &profile, cmd, env, cwd, pty_size)
        })
    }

    fn spawn_pty_relaxed<'a>(
        &'a self,
        cmd: &'a [&'a str],
        env: &'a [(String, String)],
        cwd: &'a Path,
        pty_size: portable_pty::PtySize,
    ) -> BoxFut<'a, Result<PtySpawnResult, RuntimeError>> {
        Box::pin(async move {
            if !self.is_available() {
                return Err(RuntimeError::ToolFailed(
                    "sandbox-exec not available on this host".into(),
                ));
            }
            let profile = render_template(
                &self.relaxed_template,
                &self.project_root,
                cwd,
                &self.extra_read,
                &self.extra_write,
                self.allow_network,
            );
            spawn_pty_with_profile("/usr/bin/sandbox-exec", &profile, cmd, env, cwd, pty_size)
        })
    }
}

fn spawn_pty_with_profile(
    sandbox_exec: &str,
    profile: &str,
    cmd: &[&str],
    env: &[(String, String)],
    cwd: &Path,
    pty_size: portable_pty::PtySize,
) -> Result<PtySpawnResult, RuntimeError> {
    let dir = std::env::temp_dir();
    let profile_path = dir.join(format!("atman-sandbox-{}.sb", uuid::Uuid::new_v4()));
    std::fs::write(&profile_path, profile)
        .map_err(|e| RuntimeError::ToolFailed(format!("write .sb: {e}")))?;

    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(pty_size)
        .map_err(|e| RuntimeError::ToolFailed(format!("openpty: {e}")))?;

    let mut builder = portable_pty::CommandBuilder::new(sandbox_exec);
    builder.arg("-f");
    builder.arg(&profile_path);
    for arg in cmd {
        builder.arg(arg);
    }
    builder.cwd(cwd);
    for (k, v) in env {
        builder.env(k, v);
    }

    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(|e| RuntimeError::ToolFailed(format!("pty spawn: {e}")))?;
    let reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| RuntimeError::ToolFailed(format!("pty reader: {e}")))?;
    let writer = pair
        .master
        .take_writer()
        .map_err(|e| RuntimeError::ToolFailed(format!("pty writer: {e}")))?;

    Ok(PtySpawnResult {
        child,
        reader,
        writer,
        master: pair.master,
    })
}

pub const DEFAULT_PROFILE: &str = r#"(version 1)
(deny default)
(allow process-exec (regex #"^/bin/"))
(allow process-exec (regex #"^/usr/bin/"))
(allow process-exec (regex #"^/opt/homebrew/"))
(allow process-fork)
(allow file-read* (subpath "/"))
(allow file-write* (subpath "/tmp"))
(allow file-write* (subpath "/private/tmp"))
(allow file-write* (subpath "/private/var/folders"))
(allow file-write* (regex #"^/dev/(null|zero|tty|dtracehelper|urandom|random|stdout|stderr|fd/|pts/)"))
(allow file-write* (regex #"^/dev/ptmx"))
(allow sysctl*)
(allow mach*)
(allow signal)
(allow process-info* (target self))
"#;

pub const RELAXED_PROFILE: &str = r#"(version 1)
(deny default)
(allow process-exec (regex #"^/bin/"))
(allow process-exec (regex #"^/usr/bin/"))
(allow process-exec (regex #"^/opt/homebrew/"))
(allow process-fork)
(allow file-read* (subpath "/"))
(allow file-write* (subpath "{PROJECT_ROOT}"))
(allow file-write* (subpath "{CWD}"))
(allow file-write* (subpath "/tmp"))
(allow file-write* (subpath "/private/tmp"))
(allow file-write* (subpath "/private/var/folders"))
(allow file-write* (regex #"^/dev/(null|zero|tty|dtracehelper|urandom|random|stdout|stderr|fd/|pts/)"))
(allow file-write* (regex #"^/dev/ptmx"))
(allow sysctl*)
(allow mach*)
(allow signal)
(allow process-info* (target self))
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_profile_substitutes_project_root() {
        let sb = SandboxExec::new("/tmp/proj");
        let rendered = sb.render_profile(Path::new("/tmp/proj/sub"));
        assert!(
            !rendered.contains("{PROJECT_ROOT}"),
            "no residue: {rendered}"
        );
        assert!(!rendered.contains("{CWD}"), "no residue: {rendered}");
    }

    #[test]
    fn render_profile_appends_extra_read_and_write() {
        let sb = SandboxExec::new("/tmp/proj")
            .with_extra_read(vec![PathBuf::from("/opt/homebrew/etc/gitconfig")])
            .with_extra_write(vec![PathBuf::from("/tmp/scratch")]);
        let rendered = sb.render_profile(Path::new("/tmp/proj"));
        assert!(
            rendered.contains("(allow file-read* (subpath \"/opt/homebrew/etc/gitconfig\"))"),
            "profile: {rendered}"
        );
        assert!(
            rendered.contains("(allow file-write* (subpath \"/tmp/scratch\"))"),
            "profile: {rendered}"
        );
    }

    #[test]
    fn is_available_returns_true_only_on_macos_with_binary() {
        let sb = SandboxExec::new("/tmp/proj");
        let expected =
            cfg!(target_os = "macos") && std::path::Path::new("/usr/bin/sandbox-exec").exists();
        assert_eq!(sb.is_available(), expected);
    }

    #[test]
    fn custom_template_is_used_when_set() {
        let sb = SandboxExec::new("/tmp/proj").with_template("(version 1)\n(deny default)\n");
        assert_eq!(
            sb.render_profile(Path::new("/tmp/proj")),
            "(version 1)\n(deny default)\n"
        );
    }
}
