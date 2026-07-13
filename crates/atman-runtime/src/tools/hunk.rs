use std::path::PathBuf;

use crate::error::RuntimeError;
use crate::hunk::EditProposal;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct FsEdit;

impl Tool for FsEdit {
    fn name(&self) -> &str {
        "hunk.plan_edit"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Compute a hunk-level EditProposal for replacing a file with new content. \
             Nothing is written; feed the proposal into hunk.review or hunk.apply. \
             For straightforward str_replace edits, prefer fs.edit instead.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File to edit."},
                "new_content": {"type": "string", "description": "Proposed replacement content."}
            },
            "required": ["path", "new_content"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let path = extract_path(&args, "path", 0)?;
            let new_content = extract_string(&args, "new_content", 1)?;
            let original = tokio::fs::read_to_string(&path).await.map_err(|e| {
                RuntimeError::ToolFailed(format!("fs.edit({}): {e}", path.display()))
            })?;
            let proposal = EditProposal::compute(path, original, new_content);
            Ok(Value::EditProposal(Box::new(proposal)))
        })
    }
}

pub struct HunkReview;

impl Tool for HunkReview {
    fn name(&self) -> &str {
        "hunk.review"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn approval_level(&self, _args: &ToolArgs, _ctx: &ToolCtx) -> crate::tool::ApprovalLevel {
        crate::tool::ApprovalLevel::Auto
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Present an EditProposal to a human reviewer (or auto-approve if no resolver is \
             configured). Returns a struct with mode = auto|resolved and a hunks id list \
             the caller should pass to hunk.apply.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "proposal": {"description": "EditProposal value from hunk.plan_edit."},
                "timeout_secs": {"type": "integer", "description": "Seconds to wait for a reviewer answer (default 300)."}
            },
            "required": ["proposal"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let proposal = extract_proposal(&args)?;
            let timeout_secs = match args.named("timeout_secs") {
                Some(Value::Int(n)) if *n > 0 => *n as u64,
                _ => 300,
            };
            let default_selection: Vec<u32> = proposal.hunks.iter().map(|h| h.id).collect();
            let Some(resolver) = ctx.prompt_resolver.clone() else {
                return Ok(Value::Struct(vec![
                    ("mode".into(), Value::Str("auto".into())),
                    (
                        "hunks".into(),
                        Value::List(
                            default_selection
                                .into_iter()
                                .map(|id| Value::Int(id as i64))
                                .collect(),
                        ),
                    ),
                ]));
            };
            let id = crate::rendezvous::PromptId::now();
            let payload = hunk_review_payload(&proposal);
            let answer = crate::rendezvous::await_prompt_with_payload(
                &resolver,
                id,
                "hunk_selection",
                payload,
                std::time::Duration::from_secs(timeout_secs),
            )
            .await?;
            let selection = parse_answer_hunk_ids(&answer, &default_selection)?;
            Ok(Value::Struct(vec![
                ("mode".into(), Value::Str("resolved".into())),
                ("prompt_id".into(), Value::Str(id.to_string())),
                (
                    "hunks".into(),
                    Value::List(
                        selection
                            .into_iter()
                            .map(|id| Value::Int(id as i64))
                            .collect(),
                    ),
                ),
            ]))
        })
    }
}

fn hunk_review_payload(proposal: &EditProposal) -> serde_json::Value {
    let hunks: Vec<serde_json::Value> = proposal
        .hunks
        .iter()
        .map(|h| {
            let mut diff = String::new();
            for line in &h.lines {
                match line {
                    crate::hunk::HunkLine::Add { text } => {
                        diff.push('+');
                        diff.push_str(text);
                        if !text.ends_with('\n') {
                            diff.push('\n');
                        }
                    }
                    crate::hunk::HunkLine::Delete { text } => {
                        diff.push('-');
                        diff.push_str(text);
                        if !text.ends_with('\n') {
                            diff.push('\n');
                        }
                    }
                    crate::hunk::HunkLine::Context { text } => {
                        diff.push(' ');
                        diff.push_str(text);
                        if !text.ends_with('\n') {
                            diff.push('\n');
                        }
                    }
                }
            }
            serde_json::json!({
                "id": h.id,
                "old_start": h.old_start,
                "old_len": h.old_len,
                "new_start": h.new_start,
                "new_len": h.new_len,
                "unified_diff": diff,
            })
        })
        .collect();
    serde_json::json!({
        "path": proposal.path.display().to_string(),
        "hunks": hunks,
        "options": ["all", "none", "select"],
    })
}

fn parse_answer_hunk_ids(
    answer: &serde_json::Value,
    default: &[u32],
) -> Result<Vec<u32>, RuntimeError> {
    if answer.is_null() {
        return Ok(default.to_vec());
    }
    if let Some(s) = answer.as_str() {
        match s {
            "all" => return Ok(default.to_vec()),
            "none" => return Ok(Vec::new()),
            other => {
                return Err(RuntimeError::ToolFailed(format!(
                    "hunk.review answer: unknown string `{other}`"
                )));
            }
        }
    }
    if let Some(hunks) = answer.get("hunks").and_then(|v| v.as_array()) {
        let mut ids = Vec::with_capacity(hunks.len());
        for h in hunks {
            let n = h.as_u64().ok_or_else(|| {
                RuntimeError::ToolFailed(format!("hunk.review answer: hunk id not u64: {h:?}"))
            })?;
            ids.push(n as u32);
        }
        return Ok(ids);
    }
    Err(RuntimeError::ToolFailed(format!(
        "hunk.review answer: unrecognized shape: {answer:?}"
    )))
}

pub struct HunkApply;

impl Tool for HunkApply {
    fn name(&self) -> &str {
        "hunk.apply"
    }

    fn tier(&self) -> Tier {
        Tier::Two
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Apply selected hunks from an EditProposal to disk. `hunks` is a list of hunk ids \
             (usually from a hunk.review result). Returns which ids were applied vs skipped.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "proposal": {"description": "EditProposal value from hunk.plan_edit."},
                "hunks": {
                    "type": "array",
                    "items": {"type": "integer"},
                    "description": "Hunk ids to apply. Omit or pass \"all\" to apply everything."
                }
            },
            "required": ["proposal"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let proposal = extract_proposal(&args)?;
            let selection = resolve_selection(&args, &proposal)?;
            let applied = proposal
                .apply_selected(&selection)
                .map_err(|e| RuntimeError::ToolFailed(format!("hunk.apply: {e}")))?;
            tokio::fs::write(&proposal.path, applied.as_bytes())
                .await
                .map_err(|e| {
                    RuntimeError::ToolFailed(format!(
                        "hunk.apply write {}: {e}",
                        proposal.path.display()
                    ))
                })?;
            let all_ids: Vec<u32> = proposal.hunks.iter().map(|h| h.id).collect();
            let skipped: Vec<Value> = all_ids
                .iter()
                .filter(|id| !selection.contains(id))
                .map(|id| Value::Int(*id as i64))
                .collect();
            let applied_ids: Vec<Value> =
                selection.iter().map(|id| Value::Int(*id as i64)).collect();
            Ok(Value::Struct(vec![
                ("status".into(), Value::Str("applied".into())),
                ("path".into(), Value::Path(proposal.path.clone())),
                ("applied_hunks".into(), Value::List(applied_ids)),
                ("skipped_hunks".into(), Value::List(skipped)),
                ("total_hunks".into(), Value::Int(all_ids.len() as i64)),
            ]))
        })
    }
}

fn resolve_selection(args: &ToolArgs, proposal: &EditProposal) -> Result<Vec<u32>, RuntimeError> {
    let value = args
        .named("hunks")
        .cloned()
        .or_else(|| args.positional(1).ok().cloned())
        .unwrap_or(Value::Str("all".into()));
    match value {
        Value::Str(s) => match s.as_str() {
            "all" => Ok(proposal.hunks.iter().map(|h| h.id).collect()),
            "none" => Ok(Vec::new()),
            other => Err(RuntimeError::ToolFailed(format!(
                "hunk.apply: unknown selection string `{other}` (want `all` | `none` | [1,3,...])"
            ))),
        },
        Value::List(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                match item {
                    Value::Int(n) if n > 0 => out.push(n as u32),
                    other => {
                        return Err(RuntimeError::TypeMismatch {
                            expected: "positive int (hunk id)".into(),
                            actual: other.kind_name().into(),
                        });
                    }
                }
            }
            Ok(out)
        }
        other => Err(RuntimeError::TypeMismatch {
            expected: "`all` | `none` | list of int (hunk ids)".into(),
            actual: other.kind_name().into(),
        }),
    }
}

fn extract_proposal(args: &ToolArgs) -> Result<EditProposal, RuntimeError> {
    let value = match args.named("proposal") {
        Some(v) => v,
        None => args.positional(0)?,
    };
    match value {
        Value::EditProposal(p) => Ok((**p).clone()),
        other => Err(RuntimeError::TypeMismatch {
            expected: "edit_proposal".into(),
            actual: other.kind_name().into(),
        }),
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

fn extract_path(args: &ToolArgs, name: &str, pos: usize) -> Result<PathBuf, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::Path(p) => Ok(p.clone()),
        Value::Str(s) => Ok(PathBuf::from(s)),
        other => Err(RuntimeError::TypeMismatch {
            expected: "path".into(),
            actual: other.kind_name().into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fs_edit_returns_edit_proposal_with_hunks() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let ctx = ToolCtx::new();
        let args = ToolArgs {
            positional: vec![Value::Path(path.clone()), Value::Str("a\nB\nc\n".into())],
            named: vec![],
        };
        let v = FsEdit.call(args, &ctx).await.unwrap();
        let Value::EditProposal(p) = v else {
            panic!("expected EditProposal");
        };
        assert_eq!(p.hunks.len(), 1);
        assert_eq!(p.original, "a\nb\nc\n");
        assert_eq!(p.proposed, "a\nB\nc\n");
    }

    #[tokio::test]
    async fn hunk_apply_all_writes_full_proposed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let ctx = ToolCtx::new();
        let proposal = FsEdit
            .call(
                ToolArgs {
                    positional: vec![Value::Path(path.clone()), Value::Str("a\nB\nc\n".into())],
                    named: vec![],
                },
                &ctx,
            )
            .await
            .unwrap();
        let apply_args = ToolArgs {
            positional: vec![proposal, Value::Str("all".into())],
            named: vec![],
        };
        let out = HunkApply.call(apply_args, &ctx).await.unwrap();
        let Value::Struct(fields) = out else {
            panic!("expected struct");
        };
        assert!(matches!(
            fields.iter().find(|(k, _)| k == "status").unwrap().1,
            Value::Str(ref s) if s == "applied"
        ));
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "a\nB\nc\n");
    }

    #[tokio::test]
    async fn hunk_apply_none_leaves_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let ctx = ToolCtx::new();
        let proposal = FsEdit
            .call(
                ToolArgs {
                    positional: vec![Value::Path(path.clone()), Value::Str("a\nB\nc\n".into())],
                    named: vec![],
                },
                &ctx,
            )
            .await
            .unwrap();
        let apply_args = ToolArgs {
            positional: vec![proposal],
            named: vec![("hunks".into(), Value::Str("none".into()))],
        };
        HunkApply.call(apply_args, &ctx).await.unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert_eq!(on_disk, "a\nb\nc\n");
    }

    #[tokio::test]
    async fn hunk_apply_with_id_list_writes_only_selected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        let original: String = (0..20).map(|i| format!("l{i}\n")).collect();
        std::fs::write(&path, &original).unwrap();
        let mut proposed = original.clone();
        proposed = proposed.replace("l3\n", "L3\n");
        proposed = proposed.replace("l15\n", "L15\n");
        let ctx = ToolCtx::new();
        let proposal_v = FsEdit
            .call(
                ToolArgs {
                    positional: vec![Value::Path(path.clone()), Value::Str(proposed.clone())],
                    named: vec![],
                },
                &ctx,
            )
            .await
            .unwrap();
        let apply_args = ToolArgs {
            positional: vec![proposal_v],
            named: vec![("hunks".into(), Value::List(vec![Value::Int(1)]))],
        };
        let out = HunkApply.call(apply_args, &ctx).await.unwrap();
        let Value::Struct(fields) = out else {
            panic!("expected struct");
        };
        let f = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert!(matches!(f("total_hunks"), Some(Value::Int(2))));
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("L3\n"));
        assert!(!on_disk.contains("L15\n"));
        assert!(on_disk.contains("l15\n"));
    }

    #[tokio::test]
    async fn hunk_apply_rejects_unknown_selection_string() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a\n").unwrap();
        let ctx = ToolCtx::new();
        let proposal = FsEdit
            .call(
                ToolArgs {
                    positional: vec![Value::Path(path), Value::Str("b\n".into())],
                    named: vec![],
                },
                &ctx,
            )
            .await
            .unwrap();
        let args = ToolArgs {
            positional: vec![proposal, Value::Str("some".into())],
            named: vec![],
        };
        let err = HunkApply.call(args, &ctx).await.unwrap_err();
        assert!(format!("{err}").contains("unknown selection"));
    }
}
