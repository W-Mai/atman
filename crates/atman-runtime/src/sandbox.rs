use std::path::{Path, PathBuf};
use std::process::Stdio;

use command_group::{AsyncCommandGroup, AsyncGroupChild};

use crate::error::RuntimeError;
use crate::permission::{InvocationAuthorization, ResourceProvenance};
use crate::tool::BoxFut;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxOperation {
    BackgroundSpawn,
    PtySpawn,
}

#[derive(Debug, Clone)]
pub struct SandboxDenial {
    pub operation: SandboxOperation,
    pub reason: String,
    pub provenance: ResourceProvenance,
}

#[derive(Debug, Clone)]
pub enum SandboxLaunchError {
    Denied(Box<SandboxDenial>),
    Runtime(RuntimeError),
}

impl From<RuntimeError> for SandboxLaunchError {
    fn from(error: RuntimeError) -> Self {
        Self::Runtime(error)
    }
}

impl SandboxLaunchError {
    pub fn into_runtime(self, tool: &str) -> RuntimeError {
        match self {
            Self::Denied(denial) => RuntimeError::ToolFailed(format!(
                "{tool}: sandbox denied {:?}: {}",
                denial.operation, denial.reason
            )),
            Self::Runtime(error) => error,
        }
    }
}

pub(crate) struct TempProfile {
    path: PathBuf,
}

impl TempProfile {
    pub(crate) fn create(profile: &str) -> Result<Self, RuntimeError> {
        let path = std::env::temp_dir().join(format!("atman-sandbox-{}.sb", uuid::Uuid::new_v4()));
        let guard = Self { path };
        std::fs::write(&guard.path, profile)
            .map_err(|error| RuntimeError::ToolFailed(format!("write .sb: {error}")))?;
        Ok(guard)
    }

    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempProfile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub struct BackgroundSpawnResult {
    pub child: AsyncGroupChild,
    pub(crate) profile: Option<TempProfile>,
}

impl BackgroundSpawnResult {
    pub(crate) fn direct(child: AsyncGroupChild) -> Self {
        Self {
            child,
            profile: None,
        }
    }
}

pub trait BackgroundLauncher: Send {
    fn launch(self: Box<Self>) -> Result<BackgroundSpawnResult, SandboxLaunchError>;
}

pub struct PtySpawnResult {
    pub child: Box<dyn portable_pty::Child + Send + Sync>,
    pub reader: Box<dyn std::io::Read + Send>,
    pub writer: Box<dyn std::io::Write + Send>,
    pub master: Box<dyn portable_pty::MasterPty + Send>,
    pub(crate) profile: Option<TempProfile>,
}

pub(crate) fn terminate_pty_child(child: &mut dyn portable_pty::Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn complete_pty_spawn(
    mut child: Box<dyn portable_pty::Child + Send + Sync>,
    master: Box<dyn portable_pty::MasterPty + Send>,
    profile: Option<TempProfile>,
) -> Result<PtySpawnResult, RuntimeError> {
    let reader = match master.try_clone_reader() {
        Ok(reader) => reader,
        Err(error) => {
            terminate_pty_child(child.as_mut());
            return Err(RuntimeError::ToolFailed(format!("pty reader: {error}")));
        }
    };
    let writer = match master.take_writer() {
        Ok(writer) => writer,
        Err(error) => {
            terminate_pty_child(child.as_mut());
            return Err(RuntimeError::ToolFailed(format!("pty writer: {error}")));
        }
    };
    Ok(PtySpawnResult {
        child,
        reader,
        writer,
        master,
        profile,
    })
}

pub trait Sandbox: Send + Sync {
    fn spawn<'a>(
        &'a self,
        cmd: &'a [&'a str],
        env: &'a [(String, String)],
        cwd: &'a Path,
    ) -> BoxFut<'a, Result<std::process::Output, RuntimeError>>;

    fn prepare_background(
        &self,
        cmd: &[&str],
        env: &[(String, String)],
        cwd: &Path,
        authorization: &InvocationAuthorization,
    ) -> Result<Box<dyn BackgroundLauncher>, SandboxLaunchError>;

    fn spawn_pty<'a>(
        &'a self,
        cmd: &'a [&'a str],
        env: &'a [(String, String)],
        cwd: &'a Path,
        pty_size: portable_pty::PtySize,
        authorization: &'a InvocationAuthorization,
    ) -> BoxFut<'a, Result<PtySpawnResult, SandboxLaunchError>>;

    fn is_available(&self) -> bool;

    fn kind(&self) -> &'static str;
}

pub struct SandboxExec {
    project_root: PathBuf,
    extra_read: Vec<PathBuf>,
    extra_write: Vec<PathBuf>,
    profile_template: String,
    allow_network: bool,
}

impl SandboxExec {
    pub fn new(project_root: impl Into<PathBuf>) -> Self {
        Self {
            project_root: project_root.into(),
            extra_read: Vec::new(),
            extra_write: Vec::new(),
            profile_template: DEFAULT_PROFILE.to_string(),
            allow_network: false,
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

fn sandbox_string(value: &Path) -> String {
    let mut escaped = String::new();
    for ch in value.to_string_lossy().chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            ch if ch.is_control() => {
                use std::fmt::Write as _;
                let _ = write!(escaped, "\\x{:02x}", ch as u32);
            }
            ch => escaped.push(ch),
        }
    }
    escaped
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
        .replace("{PROJECT_ROOT}", &sandbox_string(project_root))
        .replace("{CWD}", &sandbox_string(cwd));
    if allow_network && !out.contains("(allow network") {
        out.push_str("\n(allow network*)\n");
    }
    if !extra_read.is_empty() {
        out.push_str("\n;; extra_read\n");
        for root in extra_read {
            out.push_str(&format!(
                "(allow file-read* (subpath \"{}\"))\n",
                sandbox_string(root)
            ));
        }
    }
    if !extra_write.is_empty() {
        out.push_str("\n;; extra_write\n");
        for root in extra_write {
            out.push_str(&format!(
                "(allow file-write* (subpath \"{}\"))\n",
                sandbox_string(root)
            ));
        }
    }
    out
}

struct SandboxExecBackgroundLauncher {
    profile: String,
    cmd: Vec<String>,
    env: Vec<(String, String)>,
    cwd: PathBuf,
    provenance: ResourceProvenance,
}

impl BackgroundLauncher for SandboxExecBackgroundLauncher {
    fn launch(self: Box<Self>) -> Result<BackgroundSpawnResult, SandboxLaunchError> {
        let profile = TempProfile::create(&self.profile)?;
        let mut command = tokio::process::Command::new("/usr/bin/sandbox-exec");
        command
            .arg("-f")
            .arg(&profile.path)
            .args(&self.cmd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .current_dir(&self.cwd);
        for (key, value) in &self.env {
            command.env(key, value);
        }
        match command.group().kill_on_drop(true).spawn() {
            Ok(child) => Ok(BackgroundSpawnResult {
                child,
                profile: Some(profile),
            }),
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                Err(SandboxLaunchError::Denied(Box::new(SandboxDenial {
                    operation: SandboxOperation::BackgroundSpawn,
                    reason: error.to_string(),
                    provenance: self.provenance,
                })))
            }
            Err(error) => Err(SandboxLaunchError::Runtime(RuntimeError::ToolFailed(
                format!("sandbox-exec spawn: {error}"),
            ))),
        }
    }
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
            let profile = TempProfile::create(&self.render_profile(cwd))?;
            let mut command = tokio::process::Command::new("/usr/bin/sandbox-exec");
            command
                .arg("-f")
                .arg(&profile.path)
                .args(cmd)
                .current_dir(cwd);
            for (key, value) in env {
                command.env(key, value);
            }
            command
                .output()
                .await
                .map_err(|error| RuntimeError::ToolFailed(format!("sandbox-exec spawn: {error}")))
        })
    }

    fn prepare_background(
        &self,
        cmd: &[&str],
        env: &[(String, String)],
        cwd: &Path,
        authorization: &InvocationAuthorization,
    ) -> Result<Box<dyn BackgroundLauncher>, SandboxLaunchError> {
        if !authorization.is_for_call(authorization.tool_use_id(), "bash.spawn") {
            return Err(SandboxLaunchError::Runtime(RuntimeError::ToolFailed(
                "sandbox background execution requires bash.spawn authorization".into(),
            )));
        }
        if !self.is_available() {
            return Err(SandboxLaunchError::Runtime(RuntimeError::ToolFailed(
                "sandbox-exec not available on this host".into(),
            )));
        }
        Ok(Box::new(SandboxExecBackgroundLauncher {
            profile: self.render_profile(cwd),
            cmd: cmd.iter().map(|arg| (*arg).to_owned()).collect(),
            env: env.to_vec(),
            cwd: cwd.to_path_buf(),
            provenance: authorization.provenance().clone(),
        }))
    }

    fn spawn_pty<'a>(
        &'a self,
        cmd: &'a [&'a str],
        env: &'a [(String, String)],
        cwd: &'a Path,
        pty_size: portable_pty::PtySize,
        authorization: &'a InvocationAuthorization,
    ) -> BoxFut<'a, Result<PtySpawnResult, SandboxLaunchError>> {
        Box::pin(async move {
            if !authorization.is_for_call(authorization.tool_use_id(), "term.spawn") {
                return Err(SandboxLaunchError::Runtime(RuntimeError::ToolFailed(
                    "sandbox PTY execution requires term.spawn authorization".into(),
                )));
            }
            if !self.is_available() {
                return Err(SandboxLaunchError::Runtime(RuntimeError::ToolFailed(
                    "sandbox-exec not available on this host".into(),
                )));
            }
            spawn_pty_with_profile(
                "/usr/bin/sandbox-exec",
                &self.render_profile(cwd),
                cmd,
                env,
                cwd,
                pty_size,
                authorization.provenance().clone(),
            )
        })
    }

    fn is_available(&self) -> bool {
        cfg!(target_os = "macos") && Path::new("/usr/bin/sandbox-exec").exists()
    }

    fn kind(&self) -> &'static str {
        "sandbox-exec"
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_pty_with_profile(
    sandbox_exec: &str,
    profile_text: &str,
    cmd: &[&str],
    env: &[(String, String)],
    cwd: &Path,
    pty_size: portable_pty::PtySize,
    provenance: ResourceProvenance,
) -> Result<PtySpawnResult, SandboxLaunchError> {
    let profile = TempProfile::create(profile_text)?;
    let pty_system = portable_pty::native_pty_system();
    let pair = pty_system
        .openpty(pty_size)
        .map_err(|error| RuntimeError::ToolFailed(format!("openpty: {error}")))?;
    let mut builder = portable_pty::CommandBuilder::new(sandbox_exec);
    builder.arg("-f");
    builder.arg(&profile.path);
    for arg in cmd {
        builder.arg(arg);
    }
    builder.cwd(cwd);
    for (key, value) in env {
        builder.env(key, value);
    }

    let child = pair.slave.spawn_command(builder).map_err(|error| {
        if error
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)
        {
            SandboxLaunchError::Denied(Box::new(SandboxDenial {
                operation: SandboxOperation::PtySpawn,
                reason: error.to_string(),
                provenance,
            }))
        } else {
            SandboxLaunchError::Runtime(RuntimeError::ToolFailed(format!("pty spawn: {error}")))
        }
    })?;
    complete_pty_spawn(child, pair.master, Some(profile)).map_err(SandboxLaunchError::Runtime)
}

pub const DEFAULT_PROFILE: &str = r#"(version 1)
(deny default)
(import "system.sb")
(allow process*)
(allow signal (target same-sandbox))
(allow file-read*
  (subpath "/System")
  (subpath "/usr")
  (subpath "/bin")
  (subpath "/sbin")
  (subpath "/Library")
  (subpath "/private/etc")
  (subpath "/private/var/db")
  (subpath "/private/var/select")
  (subpath "/dev")
  (subpath "/tmp")
  (subpath "/private/tmp")
  (subpath "{PROJECT_ROOT}")
  (subpath "{CWD}"))
(allow file-write*
  (subpath "{PROJECT_ROOT}")
  (subpath "{CWD}")
  (subpath "/tmp")
  (subpath "/private/tmp"))
(allow sysctl-read)
(allow mach-lookup)
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_profile_substitutes_paths_and_options() {
        let sandbox = SandboxExec::new("/project")
            .with_extra_read(vec![PathBuf::from("/read")])
            .with_extra_write(vec![PathBuf::from("/write")])
            .with_allow_network(true);
        let profile = sandbox.render_profile(Path::new("/cwd"));
        assert!(profile.contains("/project"));
        assert!(profile.contains("/cwd"));
        assert!(profile.contains("/read"));
        assert!(profile.contains("/write"));
        assert!(profile.contains("/private/var/select"));
        assert!(profile.contains("(allow network*)"));
    }

    #[test]
    fn render_profile_escapes_all_path_literals() {
        let injected = PathBuf::from("/tmp/a\"\\\n) (allow network*) (");
        let sandbox = SandboxExec::new(&injected)
            .with_extra_read(vec![injected.clone()])
            .with_extra_write(vec![injected.clone()]);
        let profile = sandbox.render_profile(&injected);
        let escaped = "/tmp/a\\\"\\\\\\n) (allow network*) (";
        assert_eq!(profile.matches(escaped).count(), 6);
        assert!(!profile.contains("/tmp/a\"\\\n)"));
    }

    #[test]
    fn temp_profile_is_removed_on_drop() {
        let path = {
            let profile = TempProfile::create("(version 1)").unwrap();
            assert!(profile.path.exists());
            profile.path.clone()
        };
        assert!(!path.exists());
    }
}
