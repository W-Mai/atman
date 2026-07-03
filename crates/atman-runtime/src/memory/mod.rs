use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

use crate::error::RuntimeError;

pub mod confession;
pub mod todo;

pub use confession::{Confession, ConfessionStore};
pub use todo::{Todo, TodoStatus, TodoStore};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct MemoryId(pub Uuid);

impl MemoryId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for MemoryId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

pub(crate) async fn append_jsonl(path: &Path, value: &impl Serialize) -> Result<(), RuntimeError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| RuntimeError::ToolFailed(format!("mkdir {}: {e}", parent.display())))?;
    }
    let mut file = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await
        .map_err(|e| RuntimeError::ToolFailed(format!("open {}: {e}", path.display())))?;
    let line = serde_json::to_string(value)
        .map_err(|e| RuntimeError::ToolFailed(format!("encode: {e}")))?;
    file.write_all(line.as_bytes())
        .await
        .map_err(|e| RuntimeError::ToolFailed(format!("write: {e}")))?;
    file.write_all(b"\n")
        .await
        .map_err(|e| RuntimeError::ToolFailed(format!("write: {e}")))?;
    file.flush()
        .await
        .map_err(|e| RuntimeError::ToolFailed(format!("flush: {e}")))?;
    Ok(())
}

pub(crate) async fn read_jsonl<T: for<'de> Deserialize<'de>>(
    path: &PathBuf,
) -> Result<Vec<T>, RuntimeError> {
    let contents = match tokio::fs::read_to_string(path).await {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(RuntimeError::ToolFailed(format!(
                "read {}: {e}",
                path.display()
            )));
        }
    };
    let mut out = Vec::new();
    for (i, line) in contents.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<T>(line) {
            Ok(v) => out.push(v),
            Err(e) => eprintln!(
                "[atman] skipping malformed jsonl line {}:{}: {e}",
                path.display(),
                i + 1
            ),
        }
    }
    Ok(out)
}
