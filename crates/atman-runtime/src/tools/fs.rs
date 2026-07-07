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
        Some(
            "Read a UTF-8 text file. Use `offset` (1-indexed) + `limit` to fetch a slice of a large \
             file (recommended over reading the whole file when you only need part of it).",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Absolute or relative file path."},
                "offset": {"type": "integer", "description": "1-indexed start line. Omit to read from the beginning."},
                "limit": {"type": "integer", "description": "Maximum number of lines to return. Omit to read to end."}
            },
            "required": ["path"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let path = extract_path(&args, "path", 0)?;
            let offset = extract_optional_int(&args, "offset")?;
            let limit = extract_optional_int(&args, "limit")?;
            let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
                RuntimeError::ToolFailed(format!("fs.read({}): {e}", path.display()))
            })?;
            let canonical = canonicalize_or_owned(&path);
            ctx.note_read(&canonical);
            if offset.is_none() && limit.is_none() {
                return Ok(Value::Str(content));
            }
            let out = slice_lines(&content, offset, limit, &path);
            Ok(Value::Str(out))
        })
    }
}

fn canonicalize_or_owned(path: &std::path::Path) -> std::path::PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn slice_lines(
    content: &str,
    offset: Option<i64>,
    limit: Option<i64>,
    path: &std::path::Path,
) -> String {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let total = lines.len();
    let start_line = offset.unwrap_or(1).max(1) as usize;
    let start_idx = start_line.saturating_sub(1);
    if start_idx >= total {
        return format!(
            "[fs.read({}): offset={start_line} exceeds file length {total}. File has {total} line(s).]",
            path.display()
        );
    }
    let take = match limit {
        Some(n) if n > 0 => n as usize,
        _ => total.saturating_sub(start_idx),
    };
    let end_idx = (start_idx + take).min(total);
    let body: String = lines[start_idx..end_idx].concat();
    let end_line = end_idx;
    format!(
        "[fs.read({}): lines {start_line}-{end_line} of {total}]\n{body}",
        path.display()
    )
}

fn extract_optional_int(args: &ToolArgs, name: &str) -> Result<Option<i64>, RuntimeError> {
    match args.named(name) {
        None => Ok(None),
        Some(Value::Int(n)) => Ok(Some(*n)),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "integer".into(),
            actual: other.kind_name().into(),
        }),
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
        Some(
            "Create a new file or rewrite an existing one from scratch. \
             Provide BOTH `path` and `content` — never emit an empty {} input. \
             \
             PREFER fs.edit INSTEAD when: (a) the file already exists and you \
             only want to change part of it, (b) the file is longer than ~200 \
             lines, or (c) the intended content would exceed 4KB. Repeatedly \
             regenerating a large file via fs.write tends to fail — use \
             fs.read + fs.edit to apply targeted str_replace edits. \
             \
             Example (new file): \
             {\"path\":\"index.html\",\"content\":\"<html>...</html>\"}",
        )
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

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let path = extract_path(&args, "path", 0)?;
            let content = extract_string(&args, "content", 1)?;
            tokio::fs::write(&path, content.as_bytes())
                .await
                .map_err(|e| {
                    RuntimeError::ToolFailed(format!("fs.write({}): {e}", path.display()))
                })?;
            let canonical = canonicalize_or_owned(&path);
            ctx.note_read(&canonical);
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

pub struct FsEdit;

impl Tool for FsEdit {
    fn name(&self) -> &str {
        "fs.edit"
    }

    fn tier(&self) -> Tier {
        Tier::Two
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Replace an exact text snippet in a file. Preferred over fs.write for changing part of \
             an existing file. `old_string` must match VERBATIM (whitespace + newlines) and, by \
             default, appear exactly once — if it matches multiple times the error tells you how \
             to disambiguate. Use `replace_all: true` to change every occurrence. \
             Example: {\"path\":\"a.rs\",\"old_string\":\"fn foo() {}\",\"new_string\":\"fn foo() { println!(\\\"hi\\\"); }\"}",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Target file path."},
                "old_string": {"type": "string", "description": "Exact text to find. Match is literal, not regex."},
                "new_string": {"type": "string", "description": "Replacement text. May be empty to delete."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence. Default false (unique match required)."}
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    fn preview_call<'a>(
        &'a self,
        args: &'a ToolArgs,
        _ctx: &'a ToolCtx,
    ) -> BoxFut<'a, Option<String>> {
        Box::pin(async move {
            let path = extract_path(args, "path", 0).ok()?;
            let old_string = extract_string(args, "old_string", 1).ok()?;
            let new_string = extract_string(args, "new_string", 2).ok()?;
            let replace_all = matches!(args.named("replace_all"), Some(Value::Bool(true)));
            let content = tokio::fs::read_to_string(&path).await.ok()?;
            let updated = if replace_all {
                content.replace(&old_string, &new_string)
            } else {
                content.replacen(&old_string, &new_string, 1)
            };
            if updated == content {
                return None;
            }
            Some(unified_diff_preview(
                &path.display().to_string(),
                &content,
                &updated,
            ))
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let path = extract_path(&args, "path", 0)?;
            let old_string = extract_string(&args, "old_string", 1)?;
            let new_string = extract_string(&args, "new_string", 2)?;
            let replace_all = matches!(args.named("replace_all"), Some(Value::Bool(true)));
            let canonical = canonicalize_or_owned(&path);
            if ctx.read_files.is_some() && !ctx.has_read(&canonical) {
                return Err(RuntimeError::ToolFailed(format!(
                    "fs.edit({}): file has not been read in this session. Call fs.read({}) first so the model works on current content.",
                    path.display(),
                    path.display()
                )));
            }
            if old_string == new_string {
                return Err(RuntimeError::ToolFailed(format!(
                    "fs.edit({}): old_string equals new_string — edit would be a no-op",
                    path.display()
                )));
            }
            if old_string.is_empty() {
                return Err(RuntimeError::ToolFailed(format!(
                    "fs.edit({}): old_string is empty — refusing to insert at every byte boundary",
                    path.display()
                )));
            }
            let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
                RuntimeError::ToolFailed(format!("fs.edit({}): {e}", path.display()))
            })?;
            let match_lines = find_match_lines(&content, &old_string);
            if match_lines.is_empty() {
                let similar = similar_line_hint(&content, &old_string);
                let snippet: String = old_string.chars().take(60).collect();
                return Err(RuntimeError::ToolFailed(format!(
                    "fs.edit({}): old_string not found. First 60 chars searched: {snippet:?}. {similar}",
                    path.display()
                )));
            }
            if !replace_all && match_lines.len() > 1 {
                let sample: Vec<String> = match_lines
                    .iter()
                    .take(3)
                    .map(|n| format!("line {n}"))
                    .collect();
                return Err(RuntimeError::ToolFailed(format!(
                    "fs.edit({}): old_string matches {} times ({}). Add surrounding context so it is unique, or pass replace_all=true.",
                    path.display(),
                    match_lines.len(),
                    sample.join(", ")
                )));
            }
            let updated = if replace_all {
                content.replace(&old_string, &new_string)
            } else {
                content.replacen(&old_string, &new_string, 1)
            };
            tokio::fs::write(&path, updated.as_bytes())
                .await
                .map_err(|e| {
                    RuntimeError::ToolFailed(format!(
                        "fs.edit({}): write failed: {e}",
                        path.display()
                    ))
                })?;
            let replaced = if replace_all { match_lines.len() } else { 1 };
            let first_line = match_lines[0];
            Ok(Value::Str(format!(
                "[fs.edit({}): replaced {replaced} occurrence(s), first at line {first_line}]",
                path.display()
            )))
        })
    }
}

fn unified_diff_preview(path: &str, before: &str, after: &str) -> String {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(before, after);
    let mut out = format!("--- {path}\n+++ {path}\n");
    for (shown, hunk) in diff
        .unified_diff()
        .context_radius(3)
        .iter_hunks()
        .enumerate()
    {
        if shown >= 4 {
            out.push_str("... (truncated) ...\n");
            break;
        }
        out.push_str(&hunk.header().to_string());
        out.push('\n');
        for change in hunk.iter_changes() {
            let sign = match change.tag() {
                ChangeTag::Delete => '-',
                ChangeTag::Insert => '+',
                ChangeTag::Equal => ' ',
            };
            let line = change.value();
            let trimmed = line.strip_suffix('\n').unwrap_or(line);
            out.push(sign);
            out.push_str(trimmed);
            out.push('\n');
        }
    }
    if out.chars().count() > 4000 {
        let head: String = out.chars().take(4000).collect();
        format!("{head}\n... (preview truncated at 4000 chars) ...")
    } else {
        out
    }
}

fn find_match_lines(content: &str, needle: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut cursor = 0usize;
    while let Some(pos) = content[cursor..].find(needle) {
        let abs = cursor + pos;
        let line = content[..abs].bytes().filter(|b| *b == b'\n').count() + 1;
        out.push(line);
        cursor = abs + needle.len().max(1);
        if needle.is_empty() {
            break;
        }
    }
    out
}

fn similar_line_hint(content: &str, needle: &str) -> String {
    let first_needle_line = needle.lines().next().unwrap_or("").trim();
    if first_needle_line.is_empty() {
        return "No similar lines to suggest.".into();
    }
    let needle_tokens: Vec<&str> = first_needle_line
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| !t.is_empty())
        .collect();
    if needle_tokens.is_empty() {
        return "No similar lines to suggest.".into();
    }
    let mut scored: Vec<(usize, usize, &str)> = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let mut hits = 0usize;
        for tok in &needle_tokens {
            if line.contains(tok) {
                hits += 1;
            }
        }
        if hits > 0 {
            scored.push((hits, i + 1, line));
        }
    }
    scored.sort_by_key(|x| std::cmp::Reverse(x.0));
    scored.truncate(3);
    if scored.is_empty() {
        "No similar lines found — perhaps whitespace differs or the file was already edited.".into()
    } else {
        let joined = scored
            .iter()
            .map(|(_, n, l)| format!("  line {n}: {}", l.trim_end()))
            .collect::<Vec<_>>()
            .join("\n");
        format!("Similar lines in file:\n{joined}")
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

pub struct FsGrep;

impl Tool for FsGrep {
    fn name(&self) -> &str {
        "fs.grep"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Search files under `path` for a regex `pattern` (like ripgrep). Returns matches \
             grouped by file with `context_lines` before + after each match. Honors .gitignore \
             and hidden-file rules by default. Params: pattern (regex, required), path (dir or \
             file, default cwd), context_lines (int 0..=10, default 3), case_sensitive (bool, \
             default false), limit (int, default 50 matches, max 200). Use this INSTEAD of \
             bash.exec + rg — it's faster and returns structured results.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "path": {"type": "string"},
                "context_lines": {"type": "integer", "default": 3},
                "case_sensitive": {"type": "boolean", "default": false},
                "limit": {"type": "integer", "default": 50}
            },
            "required": ["pattern"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move { fs_grep_impl(args).await })
    }
}

async fn fs_grep_impl(args: ToolArgs) -> ToolResult {
    let pattern = extract_string(&args, "pattern", 0)?;
    if pattern.is_empty() {
        return Err(RuntimeError::ToolFailed("fs.grep: empty pattern".into()));
    }
    let base_path: std::path::PathBuf = match args.named("path") {
        Some(Value::Str(s)) => std::path::PathBuf::from(s),
        Some(Value::Path(p)) => p.clone(),
        Some(other) => {
            return Err(RuntimeError::TypeMismatch {
                expected: "path or string".into(),
                actual: other.kind_name().into(),
            });
        }
        None => std::env::current_dir()
            .map_err(|e| RuntimeError::ToolFailed(format!("fs.grep: cwd: {e}")))?,
    };
    let context_lines: usize = match args.named("context_lines") {
        Some(Value::Int(n)) if *n >= 0 => (*n as usize).min(10),
        _ => 3,
    };
    let case_sensitive = matches!(args.named("case_sensitive"), Some(Value::Bool(true)));
    let limit: usize = match args.named("limit") {
        Some(Value::Int(n)) if *n > 0 => (*n as usize).min(200),
        _ => 50,
    };
    let re = regex::RegexBuilder::new(&pattern)
        .case_insensitive(!case_sensitive)
        .build()
        .map_err(|e| RuntimeError::ToolFailed(format!("fs.grep: invalid regex: {e}")))?;
    let walker = ignore::WalkBuilder::new(&base_path).build();
    let mut hits: Vec<Value> = Vec::new();
    for entry in walker {
        if hits.len() >= limit {
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };
        if entry.file_type().is_none_or(|ft| !ft.is_file()) {
            continue;
        }
        let contents = match tokio::fs::read_to_string(entry.path()).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        let lines: Vec<&str> = contents.lines().collect();
        for (idx, line) in lines.iter().enumerate() {
            if !re.is_match(line) {
                continue;
            }
            let before_start = idx.saturating_sub(context_lines);
            let after_end = (idx + context_lines + 1).min(lines.len());
            let before: Vec<Value> = lines[before_start..idx]
                .iter()
                .map(|s| Value::Str((*s).to_string()))
                .collect();
            let after: Vec<Value> = lines[idx + 1..after_end]
                .iter()
                .map(|s| Value::Str((*s).to_string()))
                .collect();
            hits.push(Value::Struct(vec![
                (
                    "file".into(),
                    Value::Str(entry.path().display().to_string()),
                ),
                ("line".into(), Value::Int((idx + 1) as i64)),
                ("before".into(), Value::List(before)),
                ("match".into(), Value::Str((*line).to_string())),
                ("after".into(), Value::List(after)),
            ]));
            if hits.len() >= limit {
                break;
            }
        }
    }
    Ok(Value::List(hits))
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

    #[tokio::test]
    async fn fs_read_offset_limit_returns_slice_with_header() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("multi.txt");
        tokio::fs::write(&path, b"line1\nline2\nline3\nline4\nline5\n")
            .await
            .unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Path(path.clone())],
            named: vec![
                ("offset".into(), Value::Int(2)),
                ("limit".into(), Value::Int(2)),
            ],
        };
        let v = FsRead.call(args, &ctx).await.unwrap();
        let s = match v {
            Value::Str(s) => s,
            _ => panic!(),
        };
        assert!(s.contains("lines 2-3 of 5"), "header missing: {s}");
        assert!(s.contains("line2\nline3"), "body wrong: {s}");
        assert!(!s.contains("line1"));
        assert!(!s.contains("line4"));
    }

    #[tokio::test]
    async fn fs_read_offset_past_end_reports_bounds() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("short.txt");
        tokio::fs::write(&path, b"only\n").await.unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Path(path)],
            named: vec![("offset".into(), Value::Int(99))],
        };
        let v = FsRead.call(args, &ctx).await.unwrap();
        let s = match v {
            Value::Str(s) => s,
            _ => panic!(),
        };
        assert!(s.contains("offset=99 exceeds"), "expected bounds msg: {s}");
    }

    #[tokio::test]
    async fn fs_edit_unique_match_replaces_and_returns_diff_summary() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("code.rs");
        tokio::fs::write(&path, b"fn foo() {}\nfn bar() {}\n")
            .await
            .unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                ("path".into(), Value::Path(path.clone())),
                ("old_string".into(), Value::Str("fn foo() {}".into())),
                (
                    "new_string".into(),
                    Value::Str("fn foo() { println!(\"hi\"); }".into()),
                ),
            ],
        };
        let v = FsEdit.call(args, &ctx).await.unwrap();
        let s = match v {
            Value::Str(s) => s,
            _ => panic!(),
        };
        assert!(s.contains("replaced 1 occurrence"), "summary: {s}");
        assert!(s.contains("line 1"));
        let updated = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(updated.starts_with("fn foo() { println!(\"hi\"); }\n"));
        assert!(updated.contains("fn bar() {}"));
    }

    #[tokio::test]
    async fn fs_edit_missing_match_returns_similar_lines_hint() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("code.rs");
        tokio::fs::write(&path, b"fn foo() {}\nfn baz() {}\n")
            .await
            .unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                ("path".into(), Value::Path(path)),
                ("old_string".into(), Value::Str("fn bar() {}".into())),
                ("new_string".into(), Value::Str("changed".into())),
            ],
        };
        let err = FsEdit.call(args, &ctx).await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("not found"), "msg: {msg}");
        assert!(
            msg.contains("line 1") || msg.contains("line 2"),
            "msg: {msg}"
        );
    }

    #[tokio::test]
    async fn fs_edit_ambiguous_match_reports_locations() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("code.rs");
        tokio::fs::write(&path, b"TODO\nline\nTODO\n")
            .await
            .unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                ("path".into(), Value::Path(path)),
                ("old_string".into(), Value::Str("TODO".into())),
                ("new_string".into(), Value::Str("DONE".into())),
            ],
        };
        let err = FsEdit.call(args, &ctx).await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("matches 2 times"), "msg: {msg}");
        assert!(msg.contains("line 1"));
        assert!(msg.contains("line 3"));
        assert!(msg.contains("replace_all=true"));
    }

    #[tokio::test]
    async fn fs_edit_replace_all_replaces_every_occurrence() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("code.rs");
        tokio::fs::write(&path, b"TODO\nTODO\nTODO\n")
            .await
            .unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                ("path".into(), Value::Path(path.clone())),
                ("old_string".into(), Value::Str("TODO".into())),
                ("new_string".into(), Value::Str("DONE".into())),
                ("replace_all".into(), Value::Bool(true)),
            ],
        };
        FsEdit.call(args, &ctx).await.unwrap();
        let after = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(after, "DONE\nDONE\nDONE\n");
    }

    #[tokio::test]
    async fn fs_edit_noop_rejected() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("code.rs");
        tokio::fs::write(&path, b"same\n").await.unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                ("path".into(), Value::Path(path)),
                ("old_string".into(), Value::Str("same".into())),
                ("new_string".into(), Value::Str("same".into())),
            ],
        };
        let err = FsEdit.call(args, &ctx).await.unwrap_err();
        assert!(format!("{err}").contains("no-op"));
    }

    #[tokio::test]
    async fn fs_edit_requires_prior_read_when_tracker_present() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("code.rs");
        tokio::fs::write(&path, b"foo\n").await.unwrap();
        let ctx = ToolCtx::new().with_read_files(std::sync::Arc::new(std::sync::Mutex::new(
            std::collections::HashSet::new(),
        )));
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                ("path".into(), Value::Path(path)),
                ("old_string".into(), Value::Str("foo".into())),
                ("new_string".into(), Value::Str("bar".into())),
            ],
        };
        let err = FsEdit.call(args, &ctx).await.unwrap_err();
        assert!(format!("{err}").contains("has not been read"));
    }

    #[tokio::test]
    async fn fs_edit_allowed_after_fs_read() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("code.rs");
        tokio::fs::write(&path, b"foo\n").await.unwrap();
        let tracker = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
        let ctx = ToolCtx::new().with_read_files(tracker);
        let read_args = ToolArgs {
            positional: vec![Value::Path(path.clone())],
            named: vec![],
        };
        FsRead.call(read_args, &ctx).await.unwrap();
        let edit_args = ToolArgs {
            positional: vec![],
            named: vec![
                ("path".into(), Value::Path(path.clone())),
                ("old_string".into(), Value::Str("foo".into())),
                ("new_string".into(), Value::Str("bar".into())),
            ],
        };
        FsEdit.call(edit_args, &ctx).await.unwrap();
    }

    #[tokio::test]
    async fn fs_edit_new_string_containing_old_string_does_not_loop() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("code.rs");
        tokio::fs::write(&path, b"foo bar\n").await.unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                ("path".into(), Value::Path(path.clone())),
                ("old_string".into(), Value::Str("foo".into())),
                ("new_string".into(), Value::Str("foo foo".into())),
            ],
        };
        FsEdit.call(args, &ctx).await.unwrap();
        let after = tokio::fs::read_to_string(&path).await.unwrap();
        assert_eq!(after, "foo foo bar\n");
    }

    #[tokio::test]
    async fn fs_read_without_offset_limit_is_backward_compatible() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("plain.txt");
        tokio::fs::write(&path, b"one\ntwo\n").await.unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Path(path)],
            named: vec![],
        };
        let v = FsRead.call(args, &ctx).await.unwrap();
        assert!(matches!(v, Value::Str(s) if s == "one\ntwo\n"));
    }

    #[tokio::test]
    async fn fs_grep_finds_matches_with_context() {
        let dir = TempDir::new().unwrap();
        let file_a = dir.path().join("a.txt");
        tokio::fs::write(&file_a, b"foo\nhello world\nbar\nbaz\n")
            .await
            .unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: Vec::new(),
            named: vec![
                ("pattern".into(), Value::Str("world".into())),
                (
                    "path".into(),
                    Value::Str(dir.path().to_string_lossy().to_string()),
                ),
                ("context_lines".into(), Value::Int(1)),
            ],
        };
        let out = FsGrep.call(args, &ctx).await.unwrap();
        let items = match out {
            Value::List(v) => v,
            other => panic!("expected list, got {other:?}"),
        };
        assert_eq!(items.len(), 1);
        let fields = match &items[0] {
            Value::Struct(f) => f.clone(),
            other => panic!("expected struct, got {other:?}"),
        };
        let matched = fields.iter().find(|(k, _)| k == "match").unwrap();
        assert!(matches!(&matched.1, Value::Str(s) if s == "hello world"));
        let line = fields.iter().find(|(k, _)| k == "line").unwrap();
        assert!(matches!(line.1, Value::Int(2)));
    }

    #[tokio::test]
    async fn fs_grep_case_insensitive_by_default() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join("b.txt"), b"HELLO World")
            .await
            .unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: Vec::new(),
            named: vec![
                ("pattern".into(), Value::Str("hello".into())),
                (
                    "path".into(),
                    Value::Str(dir.path().to_string_lossy().to_string()),
                ),
            ],
        };
        let out = FsGrep.call(args, &ctx).await.unwrap();
        let n = match out {
            Value::List(v) => v.len(),
            _ => 0,
        };
        assert_eq!(n, 1);
    }

    #[tokio::test]
    async fn fs_grep_respects_gitignore() {
        let dir = TempDir::new().unwrap();
        tokio::fs::write(dir.path().join(".ignore"), b"skip.txt\n")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("skip.txt"), b"needle")
            .await
            .unwrap();
        tokio::fs::write(dir.path().join("keep.txt"), b"needle")
            .await
            .unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: Vec::new(),
            named: vec![
                ("pattern".into(), Value::Str("needle".into())),
                (
                    "path".into(),
                    Value::Str(dir.path().to_string_lossy().to_string()),
                ),
            ],
        };
        let out = FsGrep.call(args, &ctx).await.unwrap();
        let items = match out {
            Value::List(v) => v,
            _ => panic!("list"),
        };
        assert_eq!(items.len(), 1, "gitignored file should be skipped");
    }
}
