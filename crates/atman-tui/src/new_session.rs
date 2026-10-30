//! Host-provided location choices for a fresh conversation.

use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Location {
    pub path: Option<PathBuf>,
    pub label: String,
}

#[derive(Clone, Debug)]
pub struct Locations {
    pub id: uuid::Uuid,
    pub projects: Vec<Location>,
    pub current_directory: Option<PathBuf>,
    pub startup_directory: PathBuf,
}

#[derive(Clone, Debug)]
pub struct Selection {
    pub project: usize,
    pub directory: PathBuf,
}

impl Locations {
    pub fn directories(&self, project: usize) -> Vec<Location> {
        let mut choices: Vec<Location> = Vec::new();
        for (path, source) in [
            (self.current_directory.as_ref(), "Session directory"),
            (Some(&self.startup_directory), "Startup directory"),
            (
                self.projects.get(project).and_then(|p| p.path.as_ref()),
                "Project root",
            ),
        ] {
            let Some(path) = path else { continue };
            if let Some(existing) = choices.iter_mut().find(|c| c.path.as_ref() == Some(path)) {
                existing.label = format!("{source} / {}", existing.label);
            } else {
                choices.push(Location {
                    path: Some(path.clone()),
                    label: format!("{source} · {}", path.display()),
                });
            }
        }
        choices
    }
}
