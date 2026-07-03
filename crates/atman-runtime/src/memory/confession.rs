use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::RuntimeError;
use crate::memory::{MemoryId, append_jsonl, read_jsonl};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Confession {
    pub id: MemoryId,
    pub trigger: String,
    pub rule_violated: String,
    pub what_i_did: String,
    pub why: String,
    pub mitigation: String,
    #[serde(default)]
    pub anchors: Vec<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

impl Confession {
    fn md_slug(&self) -> String {
        let date = self.created_at.format("%Y-%m-%d");
        let slug: String = self
            .trigger
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-')
            .take(48)
            .collect::<String>()
            .to_lowercase();
        let slug = if slug.is_empty() {
            "trigger".into()
        } else {
            slug
        };
        format!("{date}-{slug}-{}.md", &self.id.to_string()[..8])
    }

    fn render_md(&self) -> String {
        format!(
            "# {trigger}\n\n\
             - **id**: `{id}`\n\
             - **rule_violated**: {rule}\n\
             - **created_at**: {ts}\n\n\
             ## What I did\n\n{what}\n\n\
             ## Why\n\n{why}\n\n\
             ## Mitigation\n\n{mit}\n",
            trigger = self.trigger,
            id = self.id,
            rule = self.rule_violated,
            ts = self.created_at.to_rfc3339(),
            what = self.what_i_did,
            why = self.why,
            mit = self.mitigation,
        )
    }
}

pub struct ConfessionStore {
    dir: PathBuf,
    index_path: PathBuf,
}

impl ConfessionStore {
    pub fn at(scope_dir: impl AsRef<Path>) -> Self {
        let dir = scope_dir.as_ref().to_path_buf();
        let index_path = dir.join("confessions.jsonl");
        Self { dir, index_path }
    }

    pub async fn append(&self, confession: Confession) -> Result<MemoryId, RuntimeError> {
        let id = confession.id.clone();
        tokio::fs::create_dir_all(&self.dir)
            .await
            .map_err(|e| RuntimeError::ToolFailed(format!("mkdir {}: {e}", self.dir.display())))?;
        let md_path = self.dir.join(confession.md_slug());
        tokio::fs::write(&md_path, confession.render_md())
            .await
            .map_err(|e| RuntimeError::ToolFailed(format!("write {}: {e}", md_path.display())))?;
        append_jsonl(&self.index_path, &confession).await?;
        Ok(id)
    }

    pub async fn list(&self) -> Result<Vec<Confession>, RuntimeError> {
        read_jsonl(&self.index_path).await
    }

    pub async fn find_by_trigger(&self, needle: &str) -> Result<Vec<Confession>, RuntimeError> {
        let all = self.list().await?;
        Ok(all
            .into_iter()
            .filter(|c| c.trigger.contains(needle))
            .collect())
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn index_path(&self) -> &Path {
        &self.index_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample(trigger: &str, rule: &str) -> Confession {
        Confession {
            id: MemoryId::now(),
            trigger: trigger.into(),
            rule_violated: rule.into(),
            what_i_did: "wrote `as any`".into(),
            why: "was in a hurry".into(),
            mitigation: "run cargo check on every edit".into(),
            anchors: vec![],
            created_at: chrono::Utc::now(),
        }
    }

    #[tokio::test]
    async fn append_then_list_returns_confession() {
        let dir = TempDir::new().unwrap();
        let store = ConfessionStore::at(dir.path());
        let id = store
            .append(sample("you keep doing X", "no-as-any"))
            .await
            .unwrap();
        let items = store.list().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, id);
    }

    #[tokio::test]
    async fn find_by_trigger_filters() {
        let dir = TempDir::new().unwrap();
        let store = ConfessionStore::at(dir.path());
        store
            .append(sample("comment discipline", "no-narrative-comments"))
            .await
            .unwrap();
        store
            .append(sample("type safety", "no-as-any"))
            .await
            .unwrap();
        let hits = store.find_by_trigger("comment").await.unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rule_violated, "no-narrative-comments");
    }

    #[tokio::test]
    async fn empty_returns_empty() {
        let dir = TempDir::new().unwrap();
        let store = ConfessionStore::at(dir.path());
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn append_writes_md_body_alongside_index() {
        let dir = TempDir::new().unwrap();
        let store = ConfessionStore::at(dir.path());
        store
            .append(sample("comment discipline again", "no-narrative-comments"))
            .await
            .unwrap();
        let mut md_files = tokio::fs::read_dir(dir.path()).await.unwrap();
        let mut found_md = false;
        while let Some(entry) = md_files.next_entry().await.unwrap() {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".md") {
                found_md = true;
                let body = tokio::fs::read_to_string(entry.path()).await.unwrap();
                assert!(body.starts_with("# comment discipline again"));
                assert!(body.contains("no-narrative-comments"));
            }
        }
        assert!(found_md, "expected a `.md` body file next to the index");
    }
}
