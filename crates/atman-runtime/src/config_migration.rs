use std::path::{Path, PathBuf};

use anyhow::Result;

pub const MIGRATION_MARKER: &str = ".migrated-to-xdg-config";

// Files historically written straight into data_dir but conceptually belong
// beside the user's other tool configs. Anything not in this list stays in
// data_dir (sessions/, indexes/, projects/, tools/, ...).
const CONFIG_FILES: &[&str] = &[
    "config.toml",
    "daemon.toml",
    "routes.at",
    "on_session_start.at",
    "on_session_end.at",
    "atman.toml",
];

const CONFIG_DIRS: &[&str] = &["commands"];

pub const MIGRATION_STATE: &str = ".config-migration-state.json";
const MANIFEST_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactOutcomeKind {
    NoSource,
    Moved,
    Conflict,
    CommittedSourceRetained,
    RejectedFileType,
    FailedBeforePublish,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ArtifactOutcome {
    pub path: String,
    pub sensitive: bool,
    pub kind: ArtifactOutcomeKind,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub moved: Vec<String>,
    pub skipped_conflicts: Vec<String>,
    pub from: PathBuf,
    pub to: PathBuf,
    pub artifacts: Vec<ArtifactOutcome>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct MigrationState {
    version: u32,
    artifacts: Vec<ArtifactOutcome>,
}

pub fn migrate_legacy_config_if_needed(
    config_dir: &Path,
    data_dir: &Path,
) -> Result<Option<MigrationReport>> {
    crate::config_hub::ConfigHub::from_config_dir(config_dir)
        .migrate_legacy_layout(data_dir)
        .map_err(anyhow::Error::from)
}

pub(crate) fn relocate_legacy_layout(
    config_dir: &Path,
    daemon_config_path: Option<&Path>,
    data_dir: &Path,
) -> Result<Option<MigrationReport>> {
    if data_dir == config_dir || !data_dir.exists() {
        return Ok(None);
    }
    let previous = load_state(data_dir);
    let mut artifacts = Vec::new();
    for name in CONFIG_FILES {
        let destination = if *name == "daemon.toml" {
            daemon_config_path
                .map(Path::to_path_buf)
                .unwrap_or_else(|| config_dir.join(name))
        } else {
            config_dir.join(name)
        };
        artifacts.push(relocate_file(
            &data_dir.join(name),
            &destination,
            name,
            *name == "daemon.toml",
        ));
    }
    for dir in CONFIG_DIRS {
        relocate_dir(
            &data_dir.join(dir),
            &config_dir.join(dir),
            dir,
            &mut artifacts,
        )?;
    }

    let state = MigrationState {
        version: MANIFEST_VERSION,
        artifacts: artifacts.clone(),
    };
    write_state(data_dir, &state)?;

    let moved = artifacts
        .iter()
        .filter(|item| item.kind == ArtifactOutcomeKind::Moved)
        .map(|item| item.path.clone())
        .collect::<Vec<_>>();
    let skipped_conflicts = artifacts
        .iter()
        .filter(|item| item.kind == ArtifactOutcomeKind::Conflict)
        .filter(|item| {
            previous.as_ref().is_none_or(|state| {
                !state
                    .artifacts
                    .iter()
                    .any(|old| old.path == item.path && old.kind == ArtifactOutcomeKind::Conflict)
            })
        })
        .map(|item| item.path.clone())
        .collect::<Vec<_>>();
    let reportable = artifacts.iter().any(|item| {
        matches!(
            item.kind,
            ArtifactOutcomeKind::Moved
                | ArtifactOutcomeKind::CommittedSourceRetained
                | ArtifactOutcomeKind::RejectedFileType
                | ArtifactOutcomeKind::FailedBeforePublish
        )
    }) || !skipped_conflicts.is_empty();
    if !reportable {
        return Ok(None);
    }
    Ok(Some(MigrationReport {
        moved,
        skipped_conflicts,
        from: data_dir.to_path_buf(),
        to: config_dir.to_path_buf(),
        artifacts,
    }))
}

fn relocate_dir(
    src: &Path,
    dst: &Path,
    relative: &str,
    outcomes: &mut Vec<ArtifactOutcome>,
) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(src) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.file_type().is_dir() {
        outcomes.push(outcome(
            relative,
            false,
            ArtifactOutcomeKind::RejectedFileType,
            None,
        ));
        return Ok(());
    }
    match std::fs::symlink_metadata(dst) {
        Ok(metadata) if metadata.file_type().is_dir() => {}
        Ok(_) => {
            outcomes.push(outcome(
                relative,
                false,
                ArtifactOutcomeKind::RejectedFileType,
                None,
            ));
            return Ok(());
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            std::fs::create_dir(dst)?;
        }
        Err(error) => return Err(error.into()),
    }
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let name = entry.file_name();
        let child_relative = format!("{relative}/{}", name.to_string_lossy());
        let child_src = entry.path();
        let child_dst = dst.join(&name);
        let child_type = std::fs::symlink_metadata(&child_src)?.file_type();
        if child_type.is_dir() {
            relocate_dir(&child_src, &child_dst, &child_relative, outcomes)?;
        } else if child_type.is_file() {
            outcomes.push(relocate_file(
                &child_src,
                &child_dst,
                &child_relative,
                false,
            ));
        } else {
            outcomes.push(outcome(
                &child_relative,
                false,
                ArtifactOutcomeKind::RejectedFileType,
                None,
            ));
        }
    }
    let _ = std::fs::remove_dir(src);
    Ok(())
}

fn relocate_file(src: &Path, dst: &Path, relative: &str, sensitive: bool) -> ArtifactOutcome {
    let metadata = match std::fs::symlink_metadata(src) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return outcome(relative, sensitive, ArtifactOutcomeKind::NoSource, None);
        }
        Err(error) => {
            return outcome(
                relative,
                sensitive,
                ArtifactOutcomeKind::FailedBeforePublish,
                Some(error.to_string()),
            );
        }
    };
    if !metadata.file_type().is_file() {
        return outcome(
            relative,
            sensitive,
            ArtifactOutcomeKind::RejectedFileType,
            None,
        );
    }
    match std::fs::symlink_metadata(dst) {
        Ok(metadata) if metadata.file_type().is_file() => {
            return outcome(relative, sensitive, ArtifactOutcomeKind::Conflict, None);
        }
        Ok(_) => {
            return outcome(
                relative,
                sensitive,
                ArtifactOutcomeKind::RejectedFileType,
                None,
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return outcome(
                relative,
                sensitive,
                ArtifactOutcomeKind::FailedBeforePublish,
                Some(error.to_string()),
            );
        }
    }
    match copy_publish_no_clobber(src, dst, sensitive, &metadata) {
        Ok(()) => match std::fs::remove_file(src) {
            Ok(()) => outcome(relative, sensitive, ArtifactOutcomeKind::Moved, None),
            Err(error) => outcome(
                relative,
                sensitive,
                ArtifactOutcomeKind::CommittedSourceRetained,
                Some(error.to_string()),
            ),
        },
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            outcome(relative, sensitive, ArtifactOutcomeKind::Conflict, None)
        }
        Err(error) => outcome(
            relative,
            sensitive,
            ArtifactOutcomeKind::FailedBeforePublish,
            Some(error.to_string()),
        ),
    }
}

fn copy_publish_no_clobber(
    src: &Path,
    dst: &Path,
    sensitive: bool,
    metadata: &std::fs::Metadata,
) -> std::io::Result<()> {
    use std::io::{Read, Write};
    let parent = dst.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let filename = dst
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let tmp = parent.join(format!(".{filename}.{}.tmp", uuid::Uuid::new_v4().simple()));
    let result = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            use std::os::unix::fs::PermissionsExt;
            options.mode(if sensitive {
                0o600
            } else {
                metadata.permissions().mode() & 0o777
            });
        }
        let mut output = options.open(&tmp)?;
        let mut input = std::fs::File::open(src)?;
        let mut bytes = Vec::new();
        input.read_to_end(&mut bytes)?;
        output.write_all(&bytes)?;
        output.sync_all()?;
        drop(output);
        std::fs::hard_link(&tmp, dst)?;
        let published = std::fs::read(dst);
        if !published.is_ok_and(|published| published == bytes) {
            return Err(std::io::Error::other(
                "published config verification failed",
            ));
        }
        let _ = std::fs::File::open(parent).and_then(|dir| dir.sync_all());
        Ok(())
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

fn outcome(
    path: &str,
    sensitive: bool,
    kind: ArtifactOutcomeKind,
    error: Option<String>,
) -> ArtifactOutcome {
    ArtifactOutcome {
        path: path.to_string(),
        sensitive,
        kind,
        error,
    }
}

fn load_state(data_dir: &Path) -> Option<MigrationState> {
    let bytes = std::fs::read(data_dir.join(MIGRATION_STATE)).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn write_state(data_dir: &Path, state: &MigrationState) -> Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let path = data_dir.join(MIGRATION_STATE);
    let tmp = data_dir.join(format!(
        ".config-migration-state.{}.tmp",
        uuid::Uuid::new_v4().simple()
    ));
    let result = (|| -> Result<()> {
        let bytes = serde_json::to_vec_pretty(state)?;
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write(p: &Path, body: &str) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn no_data_dir_returns_none_without_creating_marker() {
        let cfg = TempDir::new().unwrap();
        let data = cfg.path().join("does-not-exist");
        let out = migrate_legacy_config_if_needed(cfg.path(), &data).unwrap();
        assert!(out.is_none());
        assert!(!data.exists());
    }

    #[test]
    fn same_dir_is_noop() {
        let dir = TempDir::new().unwrap();
        write(&dir.path().join("config.toml"), "x");
        let out = migrate_legacy_config_if_needed(dir.path(), dir.path()).unwrap();
        assert!(out.is_none());
        // config.toml stayed, marker never written.
        assert!(dir.path().join("config.toml").exists());
        assert!(!dir.path().join(MIGRATION_MARKER).exists());
    }

    #[test]
    fn moves_config_files_and_writes_marker() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        write(&data.path().join("config.toml"), "cfg");
        write(&data.path().join("daemon.toml"), "d");
        write(&data.path().join("routes.at"), "r");
        write(&data.path().join("routes.toml"), "legacy");
        // Non-config file must stay put.
        write(&data.path().join("sessions").join("keep"), "s");

        let rep = migrate_legacy_config_if_needed(cfg.path(), data.path())
            .unwrap()
            .expect("expected report");
        assert_eq!(rep.moved.len(), 3);
        assert!(rep.skipped_conflicts.is_empty());
        assert!(cfg.path().join("config.toml").exists());
        assert!(cfg.path().join("daemon.toml").exists());
        assert!(cfg.path().join("routes.at").exists());
        assert!(!cfg.path().join("routes.toml").exists());
        assert!(data.path().join("routes.toml").exists());
        assert!(!data.path().join("config.toml").exists());
        // sessions/ never touched.
        assert!(data.path().join("sessions").join("keep").exists());
        assert!(data.path().join(MIGRATION_STATE).exists());
    }

    #[test]
    fn moves_commands_directory() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        write(&data.path().join("commands").join("hello.at"), "greet");

        let rep = migrate_legacy_config_if_needed(cfg.path(), data.path())
            .unwrap()
            .unwrap();
        assert!(rep.moved.iter().any(|path| path == "commands/hello.at"));
        assert!(cfg.path().join("commands").join("hello.at").exists());
        assert!(!data.path().join("commands").join("hello.at").exists());
    }

    #[test]
    fn existing_commands_directory_merges_without_overwriting() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        write(&cfg.path().join("commands/keep.at"), "new");
        write(&data.path().join("commands/keep.at"), "old");
        write(&data.path().join("commands/move.at"), "move");

        let report = migrate_legacy_config_if_needed(cfg.path(), data.path())
            .unwrap()
            .unwrap();

        assert!(report.moved.iter().any(|path| path == "commands/move.at"));
        assert!(
            report
                .skipped_conflicts
                .iter()
                .any(|path| path == "commands/keep.at")
        );
        assert_eq!(
            std::fs::read_to_string(cfg.path().join("commands/keep.at")).unwrap(),
            "new"
        );
        assert!(data.path().join("commands/keep.at").exists());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_is_rejected_and_sensitive_file_becomes_owner_only() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        write(&data.path().join("outside"), "outside");
        symlink(data.path().join("outside"), data.path().join("config.toml")).unwrap();
        write(
            &data.path().join("daemon.toml"),
            "auth_token = \"secret\"\n",
        );
        std::fs::set_permissions(
            data.path().join("daemon.toml"),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();

        let report = migrate_legacy_config_if_needed(cfg.path(), data.path())
            .unwrap()
            .unwrap();

        assert!(report.artifacts.iter().any(|item| {
            item.path == "config.toml" && item.kind == ArtifactOutcomeKind::RejectedFileType
        }));
        assert!(data.path().join("config.toml").exists());
        let mode = std::fs::metadata(cfg.path().join("daemon.toml"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[test]
    fn conflict_leaves_config_dir_version_untouched() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        // User already customized config.toml at the new location.
        write(&cfg.path().join("config.toml"), "new");
        // Old copy from legacy dir must not clobber it.
        write(&data.path().join("config.toml"), "old");

        let rep = migrate_legacy_config_if_needed(cfg.path(), data.path())
            .unwrap()
            .unwrap();
        assert!(rep.moved.is_empty());
        assert_eq!(rep.skipped_conflicts, vec!["config.toml".to_string()]);
        assert_eq!(
            std::fs::read_to_string(cfg.path().join("config.toml")).unwrap(),
            "new"
        );
        // Legacy copy left in place so the user can inspect it manually.
        assert!(data.path().join("config.toml").exists());
    }

    #[test]
    fn versioned_state_does_not_hide_new_legacy_artifacts() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        write(&data.path().join("config.toml"), "first");

        let first = migrate_legacy_config_if_needed(cfg.path(), data.path())
            .unwrap()
            .unwrap();
        assert_eq!(first.moved, vec!["config.toml".to_string()]);

        write(&data.path().join("daemon.toml"), "later");
        let second = migrate_legacy_config_if_needed(cfg.path(), data.path())
            .unwrap()
            .unwrap();
        assert_eq!(second.moved, vec!["daemon.toml".to_string()]);
        assert!(!data.path().join("daemon.toml").exists());
        assert!(cfg.path().join("daemon.toml").exists());
    }

    #[test]
    fn noop_sweep_writes_versioned_state_but_returns_none() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        // data_dir exists but contains only non-config artifacts.
        write(&data.path().join("index.db"), "sqlite");

        let out = migrate_legacy_config_if_needed(cfg.path(), data.path()).unwrap();
        assert!(out.is_none());
        assert!(data.path().join(MIGRATION_STATE).exists());
        assert!(data.path().join("index.db").exists());
    }
}
