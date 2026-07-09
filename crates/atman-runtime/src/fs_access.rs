use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

// FsAccessMode is the file-system tier of the sandbox policy. It's
// separate from atman-daemon's Seatbelt SandboxConfig (which decides
// whether `shell.exec` runs inside sandbox-exec) — that policy asks
// "do we wrap the shell?", this policy asks "which paths can atman's
// own file tools touch?".
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum FsAccessMode {
    ReadOnly,
    #[default]
    WorkspaceWrite,
    DangerFullAccess,
}

impl FsAccessMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }
}

impl std::str::FromStr for FsAccessMode {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "read-only" | "readonly" | "ro" => Ok(Self::ReadOnly),
            "workspace-write" | "workspace" | "ws" => Ok(Self::WorkspaceWrite),
            "danger-full-access" | "full-access" | "danger" => Ok(Self::DangerFullAccess),
            other => Err(format!(
                "unknown fs access mode: {other}. Expected one of: read-only, workspace-write, danger-full-access"
            )),
        }
    }
}

// Bundle mode + workspace so ToolCtx carries one thing, not two. Default
// is workspace-write with no workspace — writes will fall back to
// tempdir-only, which is the safe posture when running without a project.
#[derive(Debug, Clone, Default)]
pub struct FsAccessPolicy {
    pub mode: FsAccessMode,
    pub workspace: Option<PathBuf>,
}

impl FsAccessPolicy {
    pub fn danger_full_access() -> Self {
        Self {
            mode: FsAccessMode::DangerFullAccess,
            workspace: None,
        }
    }

    pub fn workspace_write(workspace: PathBuf) -> Self {
        Self {
            mode: FsAccessMode::WorkspaceWrite,
            workspace: Some(workspace),
        }
    }

    pub fn check_write(&self, target: &Path) -> Result<(), FsAccessError> {
        check_write(target, self.workspace.as_deref(), self.mode)
    }
}

#[derive(Debug, Error)]
pub enum FsAccessError {
    #[error("read-only fs access: refusing to write {}", path.display())]
    ReadOnlyBlocked { path: PathBuf },
    #[error(
        "workspace-write fs access: refusing to write {} outside workspace root {}",
        path.display(),
        workspace.display()
    )]
    OutsideWorkspace { path: PathBuf, workspace: PathBuf },
}

// Decide whether `target` may be written under the given mode. `workspace`
// is the current project root — `None` means we don't have one, in which
// case workspace-write falls back to allowing writes only inside the
// system temp dir so at least tests and scratch work still function.
pub fn check_write(
    target: &Path,
    workspace: Option<&Path>,
    mode: FsAccessMode,
) -> Result<(), FsAccessError> {
    match mode {
        FsAccessMode::DangerFullAccess => Ok(()),
        FsAccessMode::ReadOnly => Err(FsAccessError::ReadOnlyBlocked {
            path: target.to_path_buf(),
        }),
        FsAccessMode::WorkspaceWrite => {
            let canonical = canonicalize_stable(target);
            let temp = canonicalize_stable(&std::env::temp_dir());
            if canonical.starts_with(&temp) {
                return Ok(());
            }
            let ws = match workspace {
                Some(ws) => canonicalize_stable(ws),
                None => {
                    return Err(FsAccessError::OutsideWorkspace {
                        path: canonical,
                        workspace: PathBuf::from("<none>"),
                    });
                }
            };
            if canonical.starts_with(&ws) {
                Ok(())
            } else {
                Err(FsAccessError::OutsideWorkspace {
                    path: canonical,
                    workspace: ws,
                })
            }
        }
    }
}

// canonicalize() only works for existing paths, but we're often asked
// about paths that will be created. Walk up to the nearest existing
// ancestor, canonicalize it (this resolves macOS /var → /private/var
// symlinks), then re-attach the missing tail. That way "workspace" and
// "target" get the same symlink-resolved prefix and starts_with works.
fn canonicalize_stable(p: &Path) -> PathBuf {
    let absolute = if p.is_absolute() {
        p.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(p),
            Err(_) => return p.to_path_buf(),
        }
    };
    let mut ancestor = absolute.as_path();
    let mut suffix: Vec<&std::ffi::OsStr> = Vec::new();
    let root = loop {
        if let Ok(real) = ancestor.canonicalize() {
            break real;
        }
        match (ancestor.parent(), ancestor.file_name()) {
            (Some(parent), Some(name)) => {
                suffix.push(name);
                ancestor = parent;
            }
            _ => return absolute,
        }
    };
    let mut out = root;
    for name in suffix.into_iter().rev() {
        out.push(name);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn parse_canonical_forms() {
        use std::str::FromStr;
        assert_eq!(
            FsAccessMode::from_str("read-only").unwrap(),
            FsAccessMode::ReadOnly
        );
        assert_eq!(
            FsAccessMode::from_str("workspace-write").unwrap(),
            FsAccessMode::WorkspaceWrite
        );
        assert_eq!(
            FsAccessMode::from_str("danger-full-access").unwrap(),
            FsAccessMode::DangerFullAccess
        );
    }

    #[test]
    fn parse_aliases() {
        use std::str::FromStr;
        assert_eq!(
            FsAccessMode::from_str("ro").unwrap(),
            FsAccessMode::ReadOnly
        );
        assert_eq!(
            FsAccessMode::from_str("ws").unwrap(),
            FsAccessMode::WorkspaceWrite
        );
        assert_eq!(
            FsAccessMode::from_str("danger").unwrap(),
            FsAccessMode::DangerFullAccess
        );
    }

    #[test]
    fn parse_rejects_unknown() {
        use std::str::FromStr;
        let err = FsAccessMode::from_str("chaos").unwrap_err();
        assert!(err.contains("chaos"));
    }

    #[test]
    fn default_is_workspace_write() {
        assert_eq!(FsAccessMode::default(), FsAccessMode::WorkspaceWrite);
    }

    #[test]
    fn round_trip_as_str_and_from_str() {
        use std::str::FromStr;
        for mode in [
            FsAccessMode::ReadOnly,
            FsAccessMode::WorkspaceWrite,
            FsAccessMode::DangerFullAccess,
        ] {
            assert_eq!(FsAccessMode::from_str(mode.as_str()).unwrap(), mode);
        }
    }

    #[test]
    fn read_only_blocks_every_write() {
        let ws = TempDir::new().unwrap();
        let target = ws.path().join("inside.txt");
        let err = check_write(&target, Some(ws.path()), FsAccessMode::ReadOnly).unwrap_err();
        assert!(matches!(err, FsAccessError::ReadOnlyBlocked { .. }));
    }

    #[test]
    fn danger_full_access_permits_arbitrary_paths() {
        let target = std::env::temp_dir().join("some/absurd/deep/path.txt");
        assert!(check_write(&target, None, FsAccessMode::DangerFullAccess).is_ok());
        assert!(
            check_write(
                Path::new("/etc/passwd"),
                None,
                FsAccessMode::DangerFullAccess
            )
            .is_ok()
        );
    }

    #[test]
    fn workspace_write_allows_paths_inside_workspace() {
        let ws = TempDir::new().unwrap();
        let nested = ws.path().join("sub/dir");
        std::fs::create_dir_all(&nested).unwrap();
        let target = nested.join("a.txt");
        assert!(check_write(&target, Some(ws.path()), FsAccessMode::WorkspaceWrite).is_ok());
    }

    #[test]
    fn workspace_write_allows_paths_inside_tempdir() {
        let ws = TempDir::new().unwrap();
        let scratch = TempDir::new().unwrap();
        let target = scratch.path().join("scratch.txt");
        assert!(check_write(&target, Some(ws.path()), FsAccessMode::WorkspaceWrite).is_ok());
    }

    #[test]
    fn workspace_write_blocks_paths_outside_workspace_and_tempdir() {
        let ws = TempDir::new().unwrap();
        let outside = TempDir::new().unwrap();
        let outside_target = outside.path().join("../../etc/passwd");
        // canonicalize will resolve the .. so we craft a clearly-outside path
        // by using an absolute path anchored elsewhere.
        let bad = PathBuf::from("/etc/passwd");
        let err = check_write(&bad, Some(ws.path()), FsAccessMode::WorkspaceWrite).unwrap_err();
        assert!(matches!(err, FsAccessError::OutsideWorkspace { .. }));
        drop(outside_target);
    }

    #[test]
    fn workspace_write_without_workspace_only_allows_tempdir() {
        let scratch = TempDir::new().unwrap();
        assert!(
            check_write(
                &scratch.path().join("f.txt"),
                None,
                FsAccessMode::WorkspaceWrite,
            )
            .is_ok()
        );
        let err =
            check_write(Path::new("/etc/passwd"), None, FsAccessMode::WorkspaceWrite).unwrap_err();
        assert!(matches!(err, FsAccessError::OutsideWorkspace { .. }));
    }

    #[test]
    fn nonexistent_target_still_checked_against_workspace() {
        let ws = TempDir::new().unwrap();
        // File doesn't exist yet — canonicalize will fail — but the parent
        // directory chain is inside the workspace so the write must succeed.
        let target = ws.path().join("does/not/exist/yet.txt");
        assert!(check_write(&target, Some(ws.path()), FsAccessMode::WorkspaceWrite).is_ok());
    }

    #[test]
    fn error_messages_reference_the_offending_paths() {
        let ws = TempDir::new().unwrap();
        let err = check_write(
            Path::new("/etc/passwd"),
            Some(ws.path()),
            FsAccessMode::WorkspaceWrite,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("/etc/passwd"));
        assert!(msg.contains(&ws.path().display().to_string()));
    }
}
