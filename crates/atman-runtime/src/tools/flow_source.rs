use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::tool::ToolCtx;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowSourceScope {
    Project,
    User,
}

impl FlowSourceScope {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Project => "project",
            Self::User => "user",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledFlowSource {
    pub path: PathBuf,
    pub scope: FlowSourceScope,
}

pub fn current_project_root() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    crate::session_meta::find_project_root(&cwd)
}

pub fn project_root_for_ctx(ctx: &ToolCtx) -> Option<PathBuf> {
    ctx.workspace
        .as_ref()
        .map(|workspace| workspace.path.clone())
        .or_else(current_project_root)
}

pub fn installed_sources(
    config_dir: Option<&Path>,
    project_root: Option<&Path>,
) -> Vec<InstalledFlowSource> {
    let dirs = [
        project_root.map(|root| (FlowSourceScope::Project, root.join(".atman/commands"))),
        config_dir.map(|root| (FlowSourceScope::User, root.join("commands"))),
    ];
    let mut seen = HashSet::new();
    let mut sources = Vec::new();
    for (scope, dir) in dirs.into_iter().flatten() {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut paths = entries
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().and_then(|extension| extension.to_str()) == Some("at"))
            .collect::<Vec<_>>();
        paths.sort();
        for path in paths {
            let Some(name) = path.file_name().map(|name| name.to_os_string()) else {
                continue;
            };
            if seen.insert(name) {
                sources.push(InstalledFlowSource { path, scope });
            }
        }
    }
    sources
}

pub fn resolve_installed_command(
    name: &str,
    config_dir: Option<&Path>,
    project_root: Option<&Path>,
) -> Option<InstalledFlowSource> {
    let file_name = format!("{name}.at");
    installed_sources(config_dir, project_root)
        .into_iter()
        .find(|source| source.path.file_name().and_then(|name| name.to_str()) == Some(&file_name))
}

pub fn candidates(flow_ref: &str, ctx: &ToolCtx) -> Vec<PathBuf> {
    let config_dir = crate::storage::config_dir().ok();
    let project_root = project_root_for_ctx(ctx);
    let cwd = std::env::current_dir().ok();
    candidates_from(
        flow_ref,
        config_dir.as_deref(),
        project_root.as_deref(),
        cwd.as_deref(),
    )
}

pub fn candidates_from(
    flow_ref: &str,
    config_dir: Option<&Path>,
    project_root: Option<&Path>,
    cwd: Option<&Path>,
) -> Vec<PathBuf> {
    let path = PathBuf::from(flow_ref);
    if path.is_absolute() || path.components().count() > 1 || flow_ref.starts_with('.') {
        return vec![match (path.is_absolute(), cwd) {
            (false, Some(cwd)) => cwd.join(path),
            _ => path,
        }];
    }
    let file_name = if flow_ref.ends_with(".at") {
        flow_ref.to_string()
    } else {
        format!("{flow_ref}.at")
    };
    let mut candidates = Vec::new();
    if let Some(project_root) = project_root {
        candidates.push(project_root.join(".atman/commands").join(&file_name));
    }
    if let Some(config_dir) = config_dir {
        candidates.push(config_dir.join("commands").join(&file_name));
    }
    if let Some(cwd) = cwd {
        candidates.push(cwd.join(file_name));
    } else {
        candidates.push(PathBuf::from(file_name));
    }
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_sources_shadow_user_sources_by_file_name() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let config = root.path().join("config");
        std::fs::create_dir_all(project.join(".atman/commands")).unwrap();
        std::fs::create_dir_all(config.join("commands")).unwrap();
        std::fs::write(project.join(".atman/commands/review.at"), "project").unwrap();
        std::fs::write(config.join("commands/review.at"), "user").unwrap();
        std::fs::write(config.join("commands/agent.at"), "user").unwrap();

        let sources = installed_sources(Some(&config), Some(&project));

        assert_eq!(sources.len(), 2);
        assert!(sources.iter().any(|source| {
            source.scope == FlowSourceScope::Project
                && source.path == project.join(".atman/commands/review.at")
        }));
        assert!(
            !sources
                .iter()
                .any(|source| source.path == config.join("commands/review.at"))
        );
    }

    #[test]
    fn explicit_relative_path_is_not_shadowed_by_catalogs() {
        let paths = candidates_from(
            "./flows/review.at",
            Some(Path::new("/config")),
            Some(Path::new("/project")),
            Some(Path::new("/cwd")),
        );
        assert_eq!(paths, [PathBuf::from("/cwd/./flows/review.at")]);
    }

    #[test]
    fn named_flow_uses_project_user_then_cwd_precedence() {
        let paths = candidates_from(
            "review",
            Some(Path::new("/config")),
            Some(Path::new("/project")),
            Some(Path::new("/cwd")),
        );
        assert_eq!(
            paths,
            [
                PathBuf::from("/project/.atman/commands/review.at"),
                PathBuf::from("/config/commands/review.at"),
                PathBuf::from("/cwd/review.at"),
            ]
        );
    }
}
