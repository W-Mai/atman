use std::path::PathBuf;

use crate::error::RuntimeError;
use crate::stream::StreamFrame;
use crate::tool::{ApprovalLevel, BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
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
             file. Alternatively pass `anchor` (literal substring) to jump to the first line \
             containing it and return `context` lines on each side (default 5). Explicit \
             offset/limit take precedence over anchor.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer", "description": "1-indexed start line."},
                "limit": {"type": "integer", "description": "Maximum lines to return."},
                "anchor": {"type": "string", "description": "Literal substring; when set, defaults offset to the matched line."},
                "context": {"type": "integer", "description": "Lines around the anchor (default 5)."},
                "start_byte_in_line": {"type": "integer", "description": "UTF-8 byte offset within the starting line for continuation."}
            },
            "required": ["path"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let path = extract_path(&args, "path", 0)?;
            let mut offset = extract_optional_int(&args, "offset")?;
            let mut limit = extract_optional_int(&args, "limit")?;
            let start_byte_in_line = extract_optional_int(&args, "start_byte_in_line")?
                .map(|value| {
                    usize::try_from(value).map_err(|_| {
                        RuntimeError::ToolFailed(
                            "fs.read: start_byte_in_line must be non-negative".into(),
                        )
                    })
                })
                .transpose()?;
            let anchor = match args.named("anchor") {
                Some(Value::Str(s)) if !s.is_empty() => Some(s.clone()),
                Some(Value::Unit) | None => None,
                Some(other) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "string anchor".into(),
                        actual: other.kind_name().into(),
                    });
                }
            };
            let context: usize = match args.named("context") {
                Some(Value::Int(n)) if *n >= 0 => (*n as usize).min(200),
                _ => 5,
            };
            let content = tokio::fs::read_to_string(&path).await.map_err(|e| {
                RuntimeError::ToolFailed(format!("fs.read({}): {e}", path.display()))
            })?;
            let canonical = canonicalize_or_owned(&path);
            ctx.note_read(&canonical);
            let anchor_line = if let Some(needle) = anchor.as_deref() {
                let mut hit: Option<usize> = None;
                for (idx, line) in content.split_inclusive('\n').enumerate() {
                    if line.contains(needle) {
                        hit = Some(idx + 1);
                        break;
                    }
                }
                match hit {
                    Some(l) => Some(l),
                    None => {
                        return Err(RuntimeError::ToolFailed(format!(
                            "fs.read({}): anchor `{needle}` not found",
                            path.display()
                        )));
                    }
                }
            } else {
                None
            };
            if let Some(anchor_line) = anchor_line {
                if offset.is_none() {
                    offset = Some((anchor_line.saturating_sub(context).max(1)) as i64);
                }
                if limit.is_none() {
                    limit = Some((context * 2 + 1) as i64);
                }
            }
            if offset.is_none() && limit.is_none() && start_byte_in_line.is_none() {
                let budget = ctx.tool_output_budget;
                let bounded = crate::tools::tool_output::bounded_text_prefix(&content, budget);
                if bounded == content.len() {
                    return Ok(Value::Str(content));
                }
                return Ok(Value::Struct(vec![
                    ("content".into(), Value::Str(content[..bounded].to_string())),
                    ("truncated".into(), Value::Bool(true)),
                    (
                        "continuation".into(),
                        Value::Struct(vec![
                            ("type".into(), Value::Str("TextLine".into())),
                            (
                                "next_line".into(),
                                Value::Int(line_number_at(&content, bounded) as i64),
                            ),
                            (
                                "next_byte_in_line".into(),
                                Value::Int(byte_in_line_at(&content, bounded) as i64),
                            ),
                            ("has_more".into(), Value::Bool(true)),
                        ]),
                    ),
                ]));
            }
            if let Some(start_byte_in_line) = start_byte_in_line {
                let start_line = offset.unwrap_or(1).max(1) as usize;
                let (body, next_line, next_byte, has_more) = slice_from_position(
                    &content,
                    start_line,
                    start_byte_in_line,
                    limit,
                    ctx.tool_output_budget,
                )?;
                return Ok(Value::Struct(vec![
                    ("content".into(), Value::Str(body)),
                    ("truncated".into(), Value::Bool(has_more)),
                    (
                        "continuation".into(),
                        Value::Struct(vec![
                            ("type".into(), Value::Str("TextLine".into())),
                            ("next_line".into(), Value::Int(next_line as i64)),
                            ("next_byte_in_line".into(), Value::Int(next_byte as i64)),
                            ("has_more".into(), Value::Bool(has_more)),
                        ]),
                    ),
                ]));
            }
            let start_line = offset.unwrap_or(1).max(1) as usize;
            let total_lines = content.split_inclusive('\n').count();
            let out = slice_lines(&content, offset, limit, &path);
            if start_line.saturating_sub(1) >= total_lines {
                return Ok(Value::Str(out));
            }
            let bounded =
                crate::tools::tool_output::bounded_text_prefix(&out, ctx.tool_output_budget);
            if bounded == out.len() {
                return Ok(Value::Str(out));
            }
            let body_start = out.find('\n').map_or(0, |index| index + 1);
            let body_bytes = bounded.saturating_sub(body_start);
            let absolute_start = content
                .split_inclusive('\n')
                .take(start_line.saturating_sub(1))
                .map(str::len)
                .sum::<usize>();
            let absolute_end = absolute_start + body_bytes;
            Ok(Value::Struct(vec![
                ("content".into(), Value::Str(out[..bounded].to_string())),
                ("truncated".into(), Value::Bool(true)),
                (
                    "continuation".into(),
                    Value::Struct(vec![
                        ("type".into(), Value::Str("TextLine".into())),
                        (
                            "next_line".into(),
                            Value::Int(line_number_at(&content, absolute_end) as i64),
                        ),
                        (
                            "next_byte_in_line".into(),
                            Value::Int(byte_in_line_at(&content, absolute_end) as i64),
                        ),
                        ("has_more".into(), Value::Bool(true)),
                    ]),
                ),
            ]))
        })
    }
}

fn slice_from_position(
    content: &str,
    start_line: usize,
    start_byte: usize,
    limit: Option<i64>,
    mut budget: crate::tools::tool_output::ToolOutputBudget,
) -> Result<(String, usize, usize, bool), RuntimeError> {
    let lines: Vec<&str> = content.split_inclusive('\n').collect();
    let index = start_line.saturating_sub(1).min(lines.len());
    if index >= lines.len() {
        return Ok((String::new(), start_line, start_byte, false));
    }
    let line = lines[index].trim_end_matches('\n');
    if start_byte > line.len() || !line.is_char_boundary(start_byte) {
        return Err(RuntimeError::ToolFailed(format!(
            "fs.read: start_byte_in_line={start_byte} is not a valid UTF-8 boundary in line {start_line}"
        )));
    }
    let absolute = lines[..index].iter().map(|line| line.len()).sum::<usize>() + start_byte;
    if let Some(limit) = limit.filter(|limit| *limit > 0) {
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        budget.max_lines = budget.max_lines.min(limit);
    }
    let next = crate::tools::tool_output::bounded_text_prefix(&content[absolute..], budget);
    let body = content[absolute..absolute + next].to_string();
    let end = absolute + next;
    let has_more = end < content.len();
    Ok((
        body,
        line_number_at(content, end),
        byte_in_line_at(content, end),
        has_more,
    ))
}

fn line_number_at(content: &str, byte: usize) -> usize {
    content[..byte].bytes().filter(|b| *b == b'\n').count() + 1
}

fn byte_in_line_at(content: &str, byte: usize) -> usize {
    content[..byte]
        .rsplit_once('\n')
        .map(|(_, line)| line.len())
        .unwrap_or(byte)
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

    fn approval_level(&self, args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        workspace_approval_level(args, ApprovalLevel::from_tier(self.tier()))
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
            let mut approved = false;
            if let Err(e) = ctx.fs_access.check_write(&path) {
                match request_fs_write_approval(ctx, &path, &e.to_string()).await {
                    Some(true) => approved = true,
                    Some(false) => {
                        return Err(RuntimeError::ToolFailed(format!(
                            "fs.write({}): user denied the write operation",
                            path.display()
                        )));
                    }
                    None => {
                        return Err(RuntimeError::ToolFailed(format!(
                            "fs.write({}): {e}",
                            path.display()
                        )));
                    }
                }
            }
            let old_content = tokio::fs::read_to_string(&path).await.ok();
            tokio::fs::write(&path, content.as_bytes())
                .await
                .map_err(|e| {
                    RuntimeError::ToolFailed(format!("fs.write({}): {e}", path.display()))
                })?;
            let canonical = canonicalize_or_owned(&path);
            ctx.note_read(&canonical);
            let path_str = path.display().to_string();
            let diff_patch = match &old_content {
                Some(old) => unified_diff_preview(&path_str, old, &content),
                None => format!("+++ {path_str}\n{content}"),
            };
            if let Some(tx) = &ctx.stream_tx {
                let _ = tx.send(StreamFrame::DiffPreview {
                    title: path_str,
                    old_content: None,
                    new_content: None,
                    unified_diff: Some(diff_patch.clone()),
                    run_id: ctx.flow_run_id.as_ref().map(|r| r.0.to_string()),
                });
            }
            Ok(Value::Struct(vec![
                ("path".into(), Value::Path(path)),
                (
                    "approval".into(),
                    Value::Str(if approved { "approved" } else { "auto" }.into()),
                ),
                ("diff".into(), Value::Str(diff_patch)),
            ]))
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

    fn approval_level(&self, args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        workspace_approval_level(args, ApprovalLevel::from_tier(self.tier()))
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
            let mut approved = false;
            if let Err(e) = ctx.fs_access.check_write(&path) {
                match request_fs_write_approval(ctx, &path, &e.to_string()).await {
                    Some(true) => approved = true,
                    Some(false) => {
                        return Err(RuntimeError::ToolFailed(format!(
                            "fs.edit({}): user denied the write operation",
                            path.display()
                        )));
                    }
                    None => {
                        return Err(RuntimeError::ToolFailed(format!(
                            "fs.edit({}): {e}",
                            path.display()
                        )));
                    }
                }
            }
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
            let path_str = path.display().to_string();
            let diff_patch = unified_diff_preview(&path_str, &content, &updated);
            if let Some(tx) = &ctx.stream_tx {
                let _ = tx.send(StreamFrame::DiffPreview {
                    title: path_str,
                    old_content: None,
                    new_content: None,
                    unified_diff: Some(diff_patch.clone()),
                    run_id: ctx.flow_run_id.as_ref().map(|r| r.0.to_string()),
                });
            }
            let replaced = if replace_all { match_lines.len() } else { 1 };
            let first_line = match_lines[0];
            Ok(Value::Struct(vec![
                (
                    "summary".into(),
                    Value::Str(format!(
                        "[fs.edit({}): replaced {replaced} occurrence(s), first at line {first_line}]",
                        path.display()
                    )),
                ),
                (
                    "approval".into(),
                    Value::Str(if approved { "approved" } else { "auto" }.into()),
                ),
                ("path".into(), Value::Str(path.display().to_string())),
                ("diff".into(), Value::Str(diff_patch)),
            ]))
        })
    }
}

pub(crate) fn unified_diff_preview(path: &str, before: &str, after: &str) -> String {
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

fn path_in_workspace(path: &std::path::Path) -> bool {
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => return false,
        }
    };
    let Ok(cwd) = std::env::current_dir() else {
        return false;
    };
    abs.starts_with(&cwd)
}

fn workspace_approval_level(
    args: &ToolArgs,
    fallback: crate::tool::ApprovalLevel,
) -> crate::tool::ApprovalLevel {
    match extract_path(args, "path", 0) {
        Ok(p) if path_in_workspace(&p) => crate::tool::ApprovalLevel::Auto,
        _ => fallback,
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

    fn approval_level(&self, args: &ToolArgs, _ctx: &ToolCtx) -> ApprovalLevel {
        workspace_approval_level(args, ApprovalLevel::from_tier(self.tier()))
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

async fn request_fs_write_approval(
    ctx: &ToolCtx,
    path: &std::path::Path,
    reason: &str,
) -> Option<bool> {
    use crate::tool::ApprovalLevel;
    let Some(approval) = &ctx.approval else {
        return None;
    };
    let run_id = ctx.flow_run_id.clone()?;
    let id = format!("fs_write_{}", uuid::Uuid::now_v7());
    let pending = crate::session::PendingApproval {
        tool_use_id: id.clone(),
        tool_name: "fs.write (sandboxed)".to_string(),
        args_preview: format!("path={}", path.display()),
        preview: Some(reason.to_string()),
        level: ApprovalLevel::Dangerous,
        run_id,
        emitted_at: chrono::Utc::now(),
        bypass_auto_ceiling: false,
    };
    let rx = approval.request(pending);
    let run_id_for_emit = ctx.flow_run_id.clone();
    if let (Some(sink), Some(rid)) = (ctx.events.as_ref(), run_id_for_emit) {
        sink.emit(crate::event::Event::ToolPendingApproval {
            run_id: rid,
            tool_use_id: id,
            tool_name: "fs.write".into(),
            args_preview: path.display().to_string(),
            level: "dangerous".into(),
            preview: Some(reason.to_string()),
        });
    }
    match rx.await {
        Ok(crate::session::ApprovalDecision::Approve) => Some(true),
        _ => Some(false),
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
    async fn fs_read_continuation_reassembles_long_utf8_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("long.txt");
        let original = "你好世界🚀".repeat(100);
        tokio::fs::write(&path, &original).await.unwrap();
        let mut ctx = ToolCtx::new();
        ctx.tool_output_budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 4,
            max_bytes: 40,
            max_line_bytes: 20,
        };
        let first = FsRead
            .call(
                ToolArgs {
                    positional: vec![Value::Path(path.clone())],
                    named: vec![],
                },
                &ctx,
            )
            .await
            .unwrap();
        let Value::Struct(first_fields) = first else {
            panic!("expected bounded fs.read struct");
        };
        let first_text = first_fields
            .iter()
            .find(|(name, _)| name == "content")
            .and_then(|(_, value)| match value {
                Value::Str(text) => Some(text.as_str()),
                _ => None,
            })
            .unwrap()
            .to_string();
        let Value::Struct(cursor) = first_fields
            .iter()
            .find(|(name, _)| name == "continuation")
            .map(|(_, value)| value)
            .unwrap()
        else {
            panic!("expected continuation");
        };
        let next_line = cursor
            .iter()
            .find(|(name, _)| name == "next_line")
            .and_then(|(_, value)| match value {
                Value::Int(number) => Some(*number),
                _ => None,
            })
            .unwrap();
        let next_byte = cursor
            .iter()
            .find(|(name, _)| name == "next_byte_in_line")
            .and_then(|(_, value)| match value {
                Value::Int(number) => Some(*number),
                _ => None,
            })
            .unwrap();
        let second = FsRead
            .call(
                ToolArgs {
                    positional: vec![Value::Path(path)],
                    named: vec![
                        ("offset".into(), Value::Int(next_line)),
                        ("start_byte_in_line".into(), Value::Int(next_byte)),
                    ],
                },
                &ctx,
            )
            .await
            .unwrap();
        let Value::Struct(second_fields) = second else {
            panic!("expected second bounded fs.read struct");
        };
        let second_text = second_fields
            .iter()
            .find(|(name, _)| name == "content")
            .and_then(|(_, value)| match value {
                Value::Str(text) => Some(text.as_str()),
                _ => None,
            })
            .unwrap();
        let combined = first_text + second_text;
        assert!(combined.len() >= 38);
        assert!(original.starts_with(&combined));
        assert!(next_byte > 0);
    }

    #[tokio::test]
    async fn fs_read_continuation_reassembles_complete_utf8_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("complete.txt");
        let original = format!("first\n{}\nlast", "你好🚀".repeat(40));
        tokio::fs::write(&path, &original).await.unwrap();
        let mut ctx = ToolCtx::new();
        ctx.tool_output_budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 2,
            max_bytes: 31,
            max_line_bytes: 17,
        };

        let mut result = FsRead
            .call(
                ToolArgs {
                    positional: vec![Value::Path(path.clone())],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap();
        let mut assembled = String::new();
        for _ in 0..1000 {
            let Value::Struct(fields) = result else {
                panic!("expected paged fs.read result");
            };
            let text = fields
                .iter()
                .find_map(|(name, value)| (name == "content").then_some(value))
                .and_then(|value| match value {
                    Value::Str(text) => Some(text),
                    _ => None,
                })
                .unwrap();
            assembled.push_str(text);
            let Value::Struct(cursor) = fields
                .iter()
                .find_map(|(name, value)| (name == "continuation").then_some(value))
                .unwrap()
            else {
                panic!("expected continuation");
            };
            let has_more = cursor
                .iter()
                .find_map(|(name, value)| (name == "has_more").then_some(value))
                .and_then(|value| match value {
                    Value::Bool(value) => Some(*value),
                    _ => None,
                })
                .unwrap();
            if !has_more {
                assert_eq!(assembled, original);
                return;
            }
            let next_line = cursor
                .iter()
                .find_map(|(name, value)| (name == "next_line").then_some(value))
                .and_then(|value| match value {
                    Value::Int(value) => Some(*value),
                    _ => None,
                })
                .unwrap();
            let next_byte = cursor
                .iter()
                .find_map(|(name, value)| (name == "next_byte_in_line").then_some(value))
                .and_then(|value| match value {
                    Value::Int(value) => Some(*value),
                    _ => None,
                })
                .unwrap();
            result = FsRead
                .call(
                    ToolArgs {
                        positional: vec![Value::Path(path.clone())],
                        named: vec![
                            ("offset".into(), Value::Int(next_line)),
                            ("start_byte_in_line".into(), Value::Int(next_byte)),
                        ],
                    },
                    &ctx,
                )
                .await
                .unwrap();
        }
        panic!("fs.read continuation did not terminate");
    }

    #[tokio::test]
    async fn fs_read_continuation_advances_past_line_budget_boundary() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("lines.txt");
        tokio::fs::write(&path, "first\nsecond\n").await.unwrap();
        let mut ctx = ToolCtx::new();
        ctx.tool_output_budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 1,
            max_bytes: 64,
            max_line_bytes: 64,
        };

        let first = FsRead
            .call(
                ToolArgs {
                    positional: vec![Value::Path(path.clone())],
                    named: vec![],
                },
                &ctx,
            )
            .await
            .unwrap();
        let Value::Struct(fields) = first else {
            panic!("expected bounded fs.read struct");
        };
        let Value::Struct(cursor) = fields
            .iter()
            .find(|(name, _)| name == "continuation")
            .map(|(_, value)| value)
            .unwrap()
        else {
            panic!("expected continuation");
        };
        let next_line = cursor
            .iter()
            .find_map(|(name, value)| (name == "next_line").then_some(value))
            .and_then(|value| match value {
                Value::Int(value) => Some(*value),
                _ => None,
            })
            .unwrap();
        let next_byte = cursor
            .iter()
            .find_map(|(name, value)| (name == "next_byte_in_line").then_some(value))
            .and_then(|value| match value {
                Value::Int(value) => Some(*value),
                _ => None,
            })
            .unwrap();
        assert_eq!((next_line, next_byte), (2, 0));

        let second = FsRead
            .call(
                ToolArgs {
                    positional: vec![Value::Path(path)],
                    named: vec![
                        ("offset".into(), Value::Int(next_line)),
                        ("start_byte_in_line".into(), Value::Int(next_byte)),
                    ],
                },
                &ctx,
            )
            .await
            .unwrap();
        let Value::Struct(fields) = second else {
            panic!("expected bounded fs.read struct");
        };
        assert!(
            matches!(fields.iter().find(|(name, _)| name == "content"), Some((_, Value::Str(text))) if text == "second\n")
        );
    }

    #[tokio::test]
    async fn fs_read_continuation_honors_explicit_line_limit() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("continuation-limit.txt");
        tokio::fs::write(&path, "first\nsecond\nthird\n")
            .await
            .unwrap();
        let mut ctx = ToolCtx::new();
        ctx.tool_output_budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 10,
            max_bytes: 4096,
            max_line_bytes: 4096,
        };

        let result = FsRead
            .call(
                ToolArgs {
                    positional: vec![Value::Path(path)],
                    named: vec![
                        ("offset".into(), Value::Int(2)),
                        ("start_byte_in_line".into(), Value::Int(3)),
                        ("limit".into(), Value::Int(1)),
                    ],
                },
                &ctx,
            )
            .await
            .unwrap();
        let Value::Struct(fields) = result else {
            panic!("expected continuation fs.read struct");
        };
        assert!(matches!(
            fields.iter().find(|(name, _)| name == "content"),
            Some((_, Value::Str(text))) if text == "ond\n"
        ));
        let Value::Struct(cursor) = fields
            .iter()
            .find(|(name, _)| name == "continuation")
            .map(|(_, value)| value)
            .unwrap()
        else {
            panic!("expected continuation");
        };
        assert!(matches!(
            cursor.iter().find(|(name, _)| name == "next_line"),
            Some((_, Value::Int(3)))
        ));
        assert!(matches!(
            cursor.iter().find(|(name, _)| name == "has_more"),
            Some((_, Value::Bool(true)))
        ));
    }

    #[tokio::test]
    async fn fs_read_offset_past_end_keeps_diagnostic_under_tiny_budget() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("short.txt");
        tokio::fs::write(&path, "one\n").await.unwrap();
        let mut ctx = ToolCtx::new();
        ctx.tool_output_budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 1,
            max_bytes: 1,
            max_line_bytes: 1,
        };

        let result = FsRead
            .call(
                ToolArgs {
                    positional: vec![Value::Path(path)],
                    named: vec![("offset".into(), Value::Int(20))],
                },
                &ctx,
            )
            .await
            .unwrap();
        assert!(matches!(
            result,
            Value::Str(message) if message.contains("offset=20 exceeds file length 1")
        ));
    }

    #[tokio::test]
    async fn fs_read_explicit_slice_counts_header_against_byte_budget() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("header.txt");
        tokio::fs::write(&path, "one\ntwo\n").await.unwrap();
        let mut ctx = ToolCtx::new();
        ctx.tool_output_budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 10,
            max_bytes: 12,
            max_line_bytes: 12,
        };

        let result = FsRead
            .call(
                ToolArgs {
                    positional: vec![Value::Path(path)],
                    named: vec![
                        ("offset".into(), Value::Int(2)),
                        ("limit".into(), Value::Int(10)),
                    ],
                },
                &ctx,
            )
            .await
            .unwrap();
        let Value::Struct(fields) = result else {
            panic!("expected bounded fs.read struct");
        };
        assert!(matches!(
            fields.iter().find(|(name, _)| name == "content"),
            Some((_, Value::Str(text))) if text.len() <= 12
        ));
        let Value::Struct(cursor) = fields
            .iter()
            .find(|(name, _)| name == "continuation")
            .map(|(_, value)| value)
            .unwrap()
        else {
            panic!("expected continuation");
        };
        assert!(matches!(
            cursor.iter().find(|(name, _)| name == "next_line"),
            Some((_, Value::Int(2)))
        ));
        assert!(matches!(
            cursor.iter().find(|(name, _)| name == "next_byte_in_line"),
            Some((_, Value::Int(0)))
        ));
    }

    #[tokio::test]
    async fn fs_read_explicit_limit_cannot_exceed_output_budget() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("limited.txt");
        tokio::fs::write(&path, "one\ntwo\nthree\n").await.unwrap();
        let mut ctx = ToolCtx::new();
        ctx.tool_output_budget = crate::tools::tool_output::ToolOutputBudget {
            max_lines: 2,
            max_bytes: 4096,
            max_line_bytes: 4096,
        };

        let result = FsRead
            .call(
                ToolArgs {
                    positional: vec![Value::Path(path)],
                    named: vec![
                        ("offset".into(), Value::Int(1)),
                        ("limit".into(), Value::Int(100)),
                    ],
                },
                &ctx,
            )
            .await
            .unwrap();
        let Value::Struct(fields) = result else {
            panic!("expected bounded fs.read struct");
        };
        assert!(matches!(
            fields.iter().find(|(name, _)| name == "content"),
            Some((_, Value::Str(text))) if text.ends_with("one\n") && !text.contains("two\n")
        ));
        let Value::Struct(cursor) = fields
            .iter()
            .find(|(name, _)| name == "continuation")
            .map(|(_, value)| value)
            .unwrap()
        else {
            panic!("expected continuation");
        };
        assert!(matches!(
            cursor.iter().find(|(name, _)| name == "next_line"),
            Some((_, Value::Int(2)))
        ));
        assert!(matches!(
            cursor.iter().find(|(name, _)| name == "next_byte_in_line"),
            Some((_, Value::Int(0)))
        ));
    }

    #[tokio::test]
    async fn fs_read_rejects_invalid_utf8_continuation_offset() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("utf8.txt");
        tokio::fs::write(&path, "你好\n").await.unwrap();
        let err = FsRead
            .call(
                ToolArgs {
                    positional: vec![Value::Path(path)],
                    named: vec![
                        ("offset".into(), Value::Int(1)),
                        ("start_byte_in_line".into(), Value::Int(1)),
                    ],
                },
                &ToolCtx::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(err, RuntimeError::ToolFailed(message) if message.contains("not a valid UTF-8 boundary"))
        );
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
            Value::Struct(fields) => fields
                .into_iter()
                .find(|(k, _)| k == "summary")
                .map(|(_, v)| match v {
                    Value::Str(s) => s,
                    _ => panic!(),
                })
                .unwrap(),
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
    async fn fs_read_anchor_jumps_to_matched_line_with_context() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("story.txt");
        let body = (1..=20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        tokio::fs::write(&path, body.as_bytes()).await.unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Path(path)],
            named: vec![
                ("anchor".into(), Value::Str("line 10".into())),
                ("context".into(), Value::Int(2)),
            ],
        };
        let v = FsRead.call(args, &ctx).await.unwrap();
        let text = match v {
            Value::Str(s) => s,
            other => panic!("expected string, got {other:?}"),
        };
        assert!(text.contains("lines 8-12"), "header was: {text}");
        assert!(text.contains("line 10"));
        assert!(!text.contains("line 7"), "context boundary respected");
    }

    #[tokio::test]
    async fn fs_read_anchor_missing_reports_error() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("body.txt");
        tokio::fs::write(&path, b"only this line\n").await.unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Path(path)],
            named: vec![("anchor".into(), Value::Str("nope".into()))],
        };
        let err = FsRead.call(args, &ctx).await.unwrap_err();
        assert!(format!("{err}").contains("anchor `nope` not found"));
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
    async fn fs_write_blocked_outside_workspace() {
        let workspace = TempDir::new().unwrap();
        let policy = crate::fs_access::FsAccessPolicy::workspace_write(workspace.path().into());
        let ctx = ToolCtx::new().with_fs_access(policy);
        // /etc sits outside every workspace and the tempdir whitelist on
        // all supported platforms. The boundary check must reject before
        // tokio::fs::write is ever invoked, so the file is never touched
        // even if the test happened to run as root.
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                ("path".into(), Value::Path(PathBuf::from("/etc/atman-evil"))),
                ("content".into(), Value::Str("pwned".into())),
            ],
        };
        let err = FsWrite.call(args, &ctx).await.unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("outside workspace"), "got: {msg}");
        assert!(!std::path::Path::new("/etc/atman-evil").exists());
    }

    #[tokio::test]
    async fn fs_write_permitted_inside_workspace() {
        let workspace = TempDir::new().unwrap();
        let policy = crate::fs_access::FsAccessPolicy::workspace_write(workspace.path().into());
        let ctx = ToolCtx::new().with_fs_access(policy);
        let target = workspace.path().join("nested/deeper/note.txt");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                ("path".into(), Value::Path(target.clone())),
                ("content".into(), Value::Str("ok".into())),
            ],
        };
        FsWrite.call(args, &ctx).await.unwrap();
        assert_eq!(tokio::fs::read_to_string(&target).await.unwrap(), "ok");
    }

    #[tokio::test]
    async fn fs_write_permitted_under_danger_full_access() {
        let outside = TempDir::new().unwrap();
        let ctx =
            ToolCtx::new().with_fs_access(crate::fs_access::FsAccessPolicy::danger_full_access());
        let target = outside.path().join("wide.txt");
        let args = ToolArgs {
            positional: vec![],
            named: vec![
                ("path".into(), Value::Path(target.clone())),
                ("content".into(), Value::Str("go".into())),
            ],
        };
        FsWrite.call(args, &ctx).await.unwrap();
        assert!(target.exists());
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
