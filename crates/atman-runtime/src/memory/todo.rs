use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::RuntimeError;
use crate::memory::{MemoryId, append_jsonl, read_jsonl};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Todo {
    pub id: MemoryId,
    #[serde(rename = "where")]
    pub where_: String,
    pub why: String,
    pub how: String,
    pub expected_result: String,
    pub status: TodoStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum TodoEntry {
    Add(Todo),
    Update { id: MemoryId, status: TodoStatus },
    Delete { id: MemoryId },
}

pub struct TodoStore {
    path: PathBuf,
    notify: Option<tokio::sync::watch::Sender<Vec<Todo>>>,
}

impl TodoStore {
    pub fn at(session_dir: impl AsRef<Path>) -> Self {
        Self {
            path: session_dir.as_ref().join("todos.jsonl"),
            notify: None,
        }
    }

    pub fn with_notify(mut self, tx: tokio::sync::watch::Sender<Vec<Todo>>) -> Self {
        self.notify = Some(tx);
        self
    }

    async fn notify_if_needed(&self) {
        if let Some(tx) = &self.notify {
            if let Ok(list) = self.list().await {
                let _ = tx.send(list);
            }
        }
    }

    pub async fn add(&self, todo: Todo) -> Result<MemoryId, RuntimeError> {
        let id = todo.id.clone();
        append_jsonl(&self.path, &TodoEntry::Add(todo)).await?;
        self.notify_if_needed().await;
        Ok(id)
    }

    pub async fn set_status(&self, id: &MemoryId, status: TodoStatus) -> Result<(), RuntimeError> {
        append_jsonl(
            &self.path,
            &TodoEntry::Update {
                id: id.clone(),
                status,
            },
        )
        .await?;
        self.notify_if_needed().await;
        Ok(())
    }

    pub async fn delete(&self, id: &MemoryId) -> Result<(), RuntimeError> {
        append_jsonl(&self.path, &TodoEntry::Delete { id: id.clone() }).await?;
        self.notify_if_needed().await;
        Ok(())
    }

    pub async fn list(&self) -> Result<Vec<Todo>, RuntimeError> {
        let entries: Vec<TodoEntry> = read_jsonl(&self.path).await?;
        let mut items: Vec<Todo> = Vec::new();
        for entry in entries {
            match entry {
                TodoEntry::Add(todo) => items.push(todo),
                TodoEntry::Update { id, status } => {
                    if let Some(existing) = items.iter_mut().find(|t| t.id == id) {
                        existing.status = status;
                    }
                }
                TodoEntry::Delete { id } => {
                    items.retain(|t| t.id != id);
                }
            }
        }
        Ok(items)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn sample_todo(name: &str) -> Todo {
        Todo {
            id: MemoryId::now(),
            where_: format!("src/{name}.rs"),
            why: "for W5 spec".into(),
            how: "add helper".into(),
            expected_result: "test passes".into(),
            status: TodoStatus::Pending,
        }
    }

    #[tokio::test]
    async fn add_then_list_returns_todo() {
        let dir = TempDir::new().unwrap();
        let store = TodoStore::at(dir.path());
        let id = store.add(sample_todo("foo")).await.unwrap();
        let items = store.list().await.unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, id);
        assert!(matches!(items[0].status, TodoStatus::Pending));
    }

    #[tokio::test]
    async fn set_status_replays_over_add() {
        let dir = TempDir::new().unwrap();
        let store = TodoStore::at(dir.path());
        let id = store.add(sample_todo("bar")).await.unwrap();
        store.set_status(&id, TodoStatus::Done).await.unwrap();
        let items = store.list().await.unwrap();
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0].status, TodoStatus::Done));
    }

    #[tokio::test]
    async fn empty_dir_lists_empty_vec() {
        let dir = TempDir::new().unwrap();
        let store = TodoStore::at(dir.path());
        assert!(store.list().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn multiple_updates_replay_in_order() {
        let dir = TempDir::new().unwrap();
        let store = TodoStore::at(dir.path());
        let id = store.add(sample_todo("baz")).await.unwrap();
        store.set_status(&id, TodoStatus::InProgress).await.unwrap();
        store.set_status(&id, TodoStatus::Done).await.unwrap();
        let items = store.list().await.unwrap();
        assert!(matches!(items[0].status, TodoStatus::Done));
    }
}
