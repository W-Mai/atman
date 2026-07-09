use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

// Sentinel in data_dir. Presence means we already ran (or attempted) the
// legacy sweep so we don't repeat rename syscalls on every startup.
pub const MIGRATION_MARKER: &str = ".migrated-to-xdg-config";

// Files historically written straight into data_dir but conceptually belong
// beside the user's other tool configs. Anything not in this list stays in
// data_dir (sessions/, indexes/, projects/, tools/, ...).
const CONFIG_FILES: &[&str] = &[
    "config.toml",
    "daemon.toml",
    "routes.at",
    "routes.toml",
    "on_session_start.at",
    "on_session_end.at",
    "atman.toml",
];

const CONFIG_DIRS: &[&str] = &["commands"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub moved: Vec<String>,
    pub skipped_conflicts: Vec<String>,
    pub from: PathBuf,
    pub to: PathBuf,
}

pub fn migrate_legacy_config_if_needed(
    config_dir: &Path,
    data_dir: &Path,
) -> Result<Option<MigrationReport>> {
    // User pointed both dirs at the same place — nothing to relocate.
    if data_dir == config_dir {
        return Ok(None);
    }
    if !data_dir.exists() {
        return Ok(None);
    }
    let marker = data_dir.join(MIGRATION_MARKER);
    if marker.exists() {
        return Ok(None);
    }

    let mut moved = Vec::new();
    let mut skipped = Vec::new();

    for name in CONFIG_FILES {
        let src = data_dir.join(name);
        if !src.is_file() {
            continue;
        }
        let dst = config_dir.join(name);
        if dst.exists() {
            skipped.push((*name).to_string());
            continue;
        }
        std::fs::create_dir_all(config_dir)
            .with_context(|| format!("mkdir {}", config_dir.display()))?;
        std::fs::rename(&src, &dst)
            .with_context(|| format!("move {} → {}", src.display(), dst.display()))?;
        moved.push((*name).to_string());
    }

    for dir in CONFIG_DIRS {
        let src = data_dir.join(dir);
        if !src.is_dir() {
            continue;
        }
        let dst = config_dir.join(dir);
        if dst.exists() {
            skipped.push((*dir).to_string());
            continue;
        }
        std::fs::create_dir_all(config_dir)
            .with_context(|| format!("mkdir {}", config_dir.display()))?;
        std::fs::rename(&src, &dst)
            .with_context(|| format!("move {} → {}", src.display(), dst.display()))?;
        moved.push((*dir).to_string());
    }

    // Marker always written after a successful sweep — even a pure no-op
    // sweep counts as "done" so subsequent runs skip immediately.
    std::fs::create_dir_all(data_dir).with_context(|| format!("mkdir {}", data_dir.display()))?;
    std::fs::write(&marker, marker_contents(&moved, &skipped))
        .with_context(|| format!("write {}", marker.display()))?;

    if moved.is_empty() && skipped.is_empty() {
        return Ok(None);
    }
    Ok(Some(MigrationReport {
        moved,
        skipped_conflicts: skipped,
        from: data_dir.to_path_buf(),
        to: config_dir.to_path_buf(),
    }))
}

fn marker_contents(moved: &[String], skipped: &[String]) -> String {
    if moved.is_empty() && skipped.is_empty() {
        return "no-op\n".into();
    }
    let mut s = String::new();
    if !moved.is_empty() {
        s.push_str("moved:\n");
        for m in moved {
            s.push_str(&format!("  {m}\n"));
        }
    }
    if !skipped.is_empty() {
        s.push_str("skipped (destination already existed):\n");
        for k in skipped {
            s.push_str(&format!("  {k}\n"));
        }
    }
    s
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
        assert!(!data.path().join("config.toml").exists());
        // sessions/ never touched.
        assert!(data.path().join("sessions").join("keep").exists());
        // Marker written.
        assert!(data.path().join(MIGRATION_MARKER).exists());
    }

    #[test]
    fn moves_commands_directory() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        write(&data.path().join("commands").join("hello.at"), "greet");

        let rep = migrate_legacy_config_if_needed(cfg.path(), data.path())
            .unwrap()
            .unwrap();
        assert!(rep.moved.contains(&"commands".to_string()));
        assert!(cfg.path().join("commands").join("hello.at").exists());
        assert!(!data.path().join("commands").exists());
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
    fn marker_short_circuits_second_run() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        write(&data.path().join("config.toml"), "first");

        let first = migrate_legacy_config_if_needed(cfg.path(), data.path())
            .unwrap()
            .unwrap();
        assert_eq!(first.moved, vec!["config.toml".to_string()]);

        // Drop a fresh legacy file — marker should still block a rerun.
        write(&data.path().join("daemon.toml"), "later");
        let second = migrate_legacy_config_if_needed(cfg.path(), data.path()).unwrap();
        assert!(second.is_none());
        assert!(data.path().join("daemon.toml").exists());
        assert!(!cfg.path().join("daemon.toml").exists());
    }

    #[test]
    fn noop_sweep_still_writes_marker_but_returns_none() {
        let cfg = TempDir::new().unwrap();
        let data = TempDir::new().unwrap();
        // data_dir exists but contains only non-config artifacts.
        write(&data.path().join("index.db"), "sqlite");

        let out = migrate_legacy_config_if_needed(cfg.path(), data.path()).unwrap();
        assert!(out.is_none());
        assert!(data.path().join(MIGRATION_MARKER).exists());
        assert!(data.path().join("index.db").exists());
    }
}
