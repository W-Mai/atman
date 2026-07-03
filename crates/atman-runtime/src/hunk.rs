use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditProposal {
    pub path: PathBuf,
    pub original: String,
    pub proposed: String,
    pub hunks: Vec<Hunk>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hunk {
    pub id: u32,
    pub old_start: u32,
    pub old_len: u32,
    pub new_start: u32,
    pub new_len: u32,
    pub lines: Vec<HunkLine>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HunkLine {
    Context { text: String },
    Add { text: String },
    Delete { text: String },
}

impl EditProposal {
    pub fn compute(path: PathBuf, original: String, proposed: String) -> Self {
        let hunks = extract_hunks(&original, &proposed);
        Self {
            path,
            original,
            proposed,
            hunks,
        }
    }

    pub fn apply_selected(&self, selected: &[u32]) -> Result<String, ApplyError> {
        let selected_set: std::collections::HashSet<u32> = selected.iter().copied().collect();
        let mut lines: Vec<String> = self
            .original
            .split_inclusive('\n')
            .map(String::from)
            .collect();
        let trailing_newline_original = self.original.ends_with('\n');
        if !trailing_newline_original && let Some(last) = lines.last_mut() {
            let s: &str = last;
            if !s.ends_with('\n') {
                last.push('\n');
            }
        }

        let mut sorted: Vec<&Hunk> = self
            .hunks
            .iter()
            .filter(|h| selected_set.contains(&h.id))
            .collect();
        sorted.sort_by_key(|h| std::cmp::Reverse(h.old_start));
        for hunk in sorted {
            let old_start = hunk.old_start as usize;
            let old_len = hunk.old_len as usize;
            if old_start.saturating_add(old_len) > lines.len() {
                return Err(ApplyError::OutOfRange {
                    hunk_id: hunk.id,
                    old_start: hunk.old_start,
                    old_len: hunk.old_len,
                    file_len: lines.len() as u32,
                });
            }
            let replacement: Vec<String> = hunk
                .lines
                .iter()
                .filter_map(|l| match l {
                    HunkLine::Add { text } | HunkLine::Context { text } => {
                        Some(ensure_newline(text.clone()))
                    }
                    HunkLine::Delete { .. } => None,
                })
                .collect();
            lines.splice(old_start..old_start + old_len, replacement);
        }

        let mut out: String = lines.into_iter().collect();
        if !trailing_newline_original && out.ends_with('\n') {
            out.pop();
        }
        Ok(out)
    }
}

fn ensure_newline(mut s: String) -> String {
    if !s.ends_with('\n') {
        s.push('\n');
    }
    s
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum ApplyError {
    #[error(
        "hunk {hunk_id} out of range: old_start={old_start} old_len={old_len} file_len={file_len}"
    )]
    OutOfRange {
        hunk_id: u32,
        old_start: u32,
        old_len: u32,
        file_len: u32,
    },
}

fn extract_hunks(original: &str, proposed: &str) -> Vec<Hunk> {
    use similar::{ChangeTag, TextDiff};

    let diff = TextDiff::from_lines(original, proposed);
    let mut hunks = Vec::new();
    let mut next_id: u32 = 1;
    for group in diff.grouped_ops(3) {
        if group.is_empty() {
            continue;
        }
        let old_start = group[0].old_range().start as u32;
        let new_start = group[0].new_range().start as u32;
        let mut old_end = old_start;
        let mut new_end = new_start;
        let mut lines: Vec<HunkLine> = Vec::new();
        for op in &group {
            for change in diff.iter_changes(op) {
                let raw: &str = change.value();
                let text = strip_trailing_newline(raw.to_string());
                match change.tag() {
                    ChangeTag::Equal => {
                        lines.push(HunkLine::Context { text });
                    }
                    ChangeTag::Insert => {
                        lines.push(HunkLine::Add { text });
                    }
                    ChangeTag::Delete => {
                        lines.push(HunkLine::Delete { text });
                    }
                }
            }
            old_end = op.old_range().end as u32;
            new_end = op.new_range().end as u32;
        }
        hunks.push(Hunk {
            id: next_id,
            old_start,
            old_len: old_end - old_start,
            new_start,
            new_len: new_end - new_start,
            lines,
        });
        next_id += 1;
    }
    hunks
}

fn strip_trailing_newline(mut s: String) -> String {
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_files_produce_no_hunks() {
        let p = EditProposal::compute("/x".into(), "a\nb\nc\n".into(), "a\nb\nc\n".into());
        assert!(p.hunks.is_empty());
    }

    #[test]
    fn single_line_change_produces_one_hunk() {
        let p = EditProposal::compute("/x".into(), "a\nb\nc\n".into(), "a\nB\nc\n".into());
        assert_eq!(p.hunks.len(), 1);
        let h = &p.hunks[0];
        assert_eq!(h.id, 1);
        let deletes: Vec<&str> = h
            .lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Delete { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(deletes, vec!["b"]);
        let adds: Vec<&str> = h
            .lines
            .iter()
            .filter_map(|l| match l {
                HunkLine::Add { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(adds, vec!["B"]);
    }

    #[test]
    fn far_apart_changes_produce_two_separate_hunks() {
        let original: String = (0..20).map(|i| format!("line{i}\n")).collect();
        let mut proposed = original.clone();
        proposed = proposed.replace("line2\n", "LINE2\n");
        proposed = proposed.replace("line17\n", "LINE17\n");
        let p = EditProposal::compute("/x".into(), original, proposed);
        assert_eq!(p.hunks.len(), 2, "hunks: {:#?}", p.hunks);
        assert_eq!(p.hunks[0].id, 1);
        assert_eq!(p.hunks[1].id, 2);
    }

    #[test]
    fn apply_all_hunks_equals_full_replacement() {
        let orig: String = (0..20).map(|i| format!("l{i}\n")).collect();
        let mut proposed = orig.clone();
        proposed = proposed.replace("l3\n", "L3\n");
        proposed = proposed.replace("l15\n", "L15\n");
        let p = EditProposal::compute("/x".into(), orig, proposed.clone());
        assert_eq!(p.hunks.len(), 2);
        let ids: Vec<u32> = p.hunks.iter().map(|h| h.id).collect();
        let out = p.apply_selected(&ids).unwrap();
        assert_eq!(out, proposed);
    }

    #[test]
    fn apply_no_hunks_returns_original() {
        let orig: String = (0..10).map(|i| format!("l{i}\n")).collect();
        let mut proposed = orig.clone();
        proposed = proposed.replace("l3\n", "L3\n");
        let p = EditProposal::compute("/x".into(), orig.clone(), proposed);
        let out = p.apply_selected(&[]).unwrap();
        assert_eq!(out, orig);
    }

    #[test]
    fn apply_selects_only_marked_hunks() {
        let orig: String = (0..20).map(|i| format!("l{i}\n")).collect();
        let mut proposed = orig.clone();
        proposed = proposed.replace("l3\n", "L3\n");
        proposed = proposed.replace("l15\n", "L15\n");
        let p = EditProposal::compute("/x".into(), orig, proposed);
        assert_eq!(p.hunks.len(), 2);
        let out = p.apply_selected(&[p.hunks[0].id]).unwrap();
        assert!(out.contains("L3\n"), "hunk 1 (l3) must be applied: {out}");
        assert!(
            !out.contains("L15\n"),
            "hunk 2 (l15) must NOT be applied: {out}"
        );
        assert!(
            out.contains("l15\n"),
            "hunk 2 line must retain original: {out}"
        );
    }

    #[test]
    fn preserves_no_trailing_newline_when_original_has_none() {
        let orig = "a\nb\nc".to_string();
        let proposed = "a\nB\nc".to_string();
        let p = EditProposal::compute("/x".into(), orig, proposed.clone());
        let ids: Vec<u32> = p.hunks.iter().map(|h| h.id).collect();
        let out = p.apply_selected(&ids).unwrap();
        assert_eq!(out, proposed);
        assert!(!out.ends_with('\n'));
    }

    #[test]
    fn roundtrip_via_serde_json() {
        let p = EditProposal::compute("/x".into(), "a\nb\n".into(), "a\nB\n".into());
        let s = serde_json::to_string(&p).unwrap();
        let back: EditProposal = serde_json::from_str(&s).unwrap();
        assert_eq!(p, back);
    }
}
