use std::path::{Path, PathBuf};

pub struct GoalStore {
    path: PathBuf,
}

impl GoalStore {
    pub fn at(session_dir: impl AsRef<Path>) -> Self {
        Self {
            path: session_dir.as_ref().join("goal.txt"),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn get(&self) -> std::io::Result<String> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => Ok(s.trim_end().to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(e) => Err(e),
        }
    }

    pub fn set(&self, text: &str) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.path, text.trim_end())
    }

    pub fn clear(&self) -> std::io::Result<()> {
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_when_never_set() {
        let tmp = tempfile::tempdir().unwrap();
        let g = GoalStore::at(tmp.path());
        assert_eq!(g.get().unwrap(), "");
    }

    #[test]
    fn set_then_get_roundtrips_trimmed() {
        let tmp = tempfile::tempdir().unwrap();
        let g = GoalStore::at(tmp.path());
        g.set("ship an atman agent\n\n").unwrap();
        assert_eq!(g.get().unwrap(), "ship an atman agent");
    }

    #[test]
    fn set_overwrites_previous() {
        let tmp = tempfile::tempdir().unwrap();
        let g = GoalStore::at(tmp.path());
        g.set("v1").unwrap();
        g.set("v2").unwrap();
        assert_eq!(g.get().unwrap(), "v2");
    }

    #[test]
    fn clear_removes_file_and_get_returns_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let g = GoalStore::at(tmp.path());
        g.set("temporary").unwrap();
        g.clear().unwrap();
        assert_eq!(g.get().unwrap(), "");
        assert!(!g.path().exists());
    }

    #[test]
    fn clear_on_missing_file_is_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let g = GoalStore::at(tmp.path());
        g.clear().unwrap();
    }

    #[test]
    fn set_creates_parent_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("nested/deep");
        let g = GoalStore::at(&nested);
        g.set("hi").unwrap();
        assert_eq!(g.get().unwrap(), "hi");
    }
}
