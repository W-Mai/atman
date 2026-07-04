use std::path::PathBuf;

use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct FsRead;

impl Tool for FsRead {
    fn name(&self) -> &str {
        "fs.read"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some("Read a UTF-8 text file from disk and return the whole contents as a string.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute or relative file path."}
            },
            "required": ["path"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let path = extract_path(&args, "path", 0)?;
            let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
                RuntimeError::ToolFailed(format!("fs.read({}): {e}", path.display()))
            })?;
            Ok(Value::Str(content))
        })
    }
}

pub struct FsWrite;

impl Tool for FsWrite {
    fn name(&self) -> &str {
        "fs.write"
    }

    fn tier(&self) -> Tier {
        Tier::Two
    }

    fn description(&self) -> Option<&str> {
        Some("Write text content to a file, replacing anything already there.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Target file path."},
                "content": {"type": "string", "description": "UTF-8 text to write."}
            },
            "required": ["path", "content"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let path = extract_path(&args, "path", 0)?;
            let content = extract_string(&args, "content", 1)?;
            tokio::fs::write(&path, content.as_bytes())
                .await
                .map_err(|e| {
                    RuntimeError::ToolFailed(format!("fs.write({}): {e}", path.display()))
                })?;
            Ok(Value::Path(path))
        })
    }
}

fn extract_string(args: &ToolArgs, name: &str, pos: usize) -> Result<String, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::Str(s) => Ok(s.clone()),
        other => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

pub struct FsList;

impl Tool for FsList {
    fn name(&self) -> &str {
        "fs.list"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some("List the entries of a directory. Returns a list of {name, kind} structs.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory path to list."}
            },
            "required": ["path"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let path = extract_path(&args, "path", 0)?;
            let mut rd = tokio::fs::read_dir(&path).await.map_err(|e| {
                RuntimeError::ToolFailed(format!("fs.list({}): {e}", path.display()))
            })?;
            let mut entries = Vec::new();
            while let Some(entry) = rd
                .next_entry()
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("fs.list next_entry: {e}")))?
            {
                entries.push(Value::Path(entry.path()));
            }
            entries.sort_by(|a, b| match (a, b) {
                (Value::Path(a), Value::Path(b)) => a.cmp(b),
                _ => std::cmp::Ordering::Equal,
            });
            Ok(Value::List(entries))
        })
    }
}

fn extract_path(args: &ToolArgs, name: &str, pos: usize) -> Result<PathBuf, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::Path(p) => Ok(p.clone()),
        Value::Str(s) => Ok(PathBuf::from(s)),
        other => Err(RuntimeError::TypeMismatch {
            expected: "path or string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn fs_read_returns_file_content() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hello.txt");
        tokio::fs::write(&path, b"hi from atman").await.unwrap();

        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Path(path)],
            named: vec![],
        };
        let v = FsRead.call(args, &ctx).await.unwrap();
        assert!(matches!(v, Value::Str(s) if s == "hi from atman"));
    }

    #[tokio::test]
    async fn fs_read_accepts_string_path() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("x.txt");
        tokio::fs::write(&path, b"ok").await.unwrap();

        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Str(path.to_string_lossy().into())],
            named: vec![],
        };
        let v = FsRead.call(args, &ctx).await.unwrap();
        assert!(matches!(v, Value::Str(s) if s == "ok"));
    }

    #[tokio::test]
    async fn fs_read_missing_file_returns_tool_failed() {
        let dir = TempDir::new().unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Path(dir.path().join("nope"))],
            named: vec![],
        };
        let err = FsRead.call(args, &ctx).await.unwrap_err();
        assert!(matches!(err, RuntimeError::ToolFailed(_)));
    }

    #[tokio::test]
    async fn fs_list_returns_sorted_paths() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("b.txt"), b"")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("a.txt"), b"")
            .await
            .unwrap();

        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Path(dir.path().to_path_buf())],
            named: vec![],
        };
        let v = FsList.call(args, &ctx).await.unwrap();
        if let Value::List(items) = v {
            assert_eq!(items.len(), 2);
            if let Value::Path(p) = &items[0] {
                assert!(p.ends_with("a.txt"));
            } else {
                panic!("expected path");
            }
        } else {
            panic!("expected list");
        }
    }

    #[tokio::test]
    async fn missing_positional_is_missing_arg_error() {
        let ctx = ToolCtx::new();
        let args = ToolArgs::default();
        let err = FsRead.call(args, &ctx).await.unwrap_err();
        assert!(matches!(err, RuntimeError::MissingArg(_)));
    }
}
