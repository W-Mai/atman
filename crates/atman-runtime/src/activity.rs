use std::path::Path;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct EditMetrics {
    pub hunks: usize,
    pub insertions: usize,
    pub deletions: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActivitySummary {
    pub attempted_calls: usize,
    pub completed_calls: usize,
    pub failed_calls: usize,
    pub applied_edits: usize,
    pub files: usize,
    pub hunks: usize,
    pub insertions: usize,
    pub deletions: usize,
}

pub fn summarize_events(events: &[crate::event::EventEnvelope]) -> ActivitySummary {
    let mut summary = ActivitySummary::default();
    let mut attempted = std::collections::HashSet::new();
    let mut completed = std::collections::HashSet::new();
    let mut files = std::collections::HashSet::new();
    for envelope in events {
        match &envelope.event {
            crate::event::Event::ToolNode {
                run_id,
                tool_use_id,
                ..
            } => {
                if attempted.insert((run_id.to_string(), tool_use_id.clone())) {
                    summary.attempted_calls += 1;
                }
            }
            crate::event::Event::ToolResultMsg {
                flow_run_id,
                message,
                ..
            } => {
                for part in &message.parts {
                    if let crate::message::MessagePart::ToolResult {
                        tool_use_id,
                        is_error,
                        ..
                    } = part
                    {
                        let key = (
                            flow_run_id
                                .as_ref()
                                .map(ToString::to_string)
                                .unwrap_or_default(),
                            tool_use_id.clone(),
                        );
                        if completed.insert(key) {
                            summary.completed_calls += 1;
                            summary.failed_calls += usize::from(*is_error);
                        }
                    }
                }
            }
            crate::event::Event::FileEditApplied { path, metrics, .. } => {
                summary.applied_edits += 1;
                files.insert(path.clone());
                summary.hunks += metrics.hunks;
                summary.insertions += metrics.insertions;
                summary.deletions += metrics.deletions;
            }
            _ => {}
        }
    }
    summary.files = files.len();
    summary
}

pub fn edit_metrics(before: &str, after: &str) -> EditMetrics {
    let diff = similar::TextDiff::from_lines(before, after);
    let mut metrics = EditMetrics::default();
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => metrics.insertions += 1,
            similar::ChangeTag::Delete => metrics.deletions += 1,
            similar::ChangeTag::Equal => {}
        }
    }
    metrics.hunks = diff
        .grouped_ops(3)
        .into_iter()
        .filter(|group| {
            group
                .iter()
                .any(|op| !matches!(op.tag(), similar::DiffTag::Equal))
        })
        .count();
    metrics
}

pub fn emit_file_edit_applied(
    ctx: &crate::tool::ToolCtx,
    tool_name: &str,
    path: &Path,
    before: &str,
    after: &str,
) {
    let metrics = edit_metrics(before, after);
    if metrics.insertions == 0 && metrics.deletions == 0 {
        return;
    }
    let path = path
        .canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned();
    if let Some(sink) = &ctx.events {
        sink.emit(crate::event::Event::FileEditApplied {
            turn_id: ctx.turn_id.clone(),
            flow_run_id: ctx.flow_run_id.clone(),
            tool_use_id: ctx.tool_use_id.clone(),
            tool_name: tool_name.to_string(),
            path: path.clone(),
            metrics,
        });
    }
    if let Some(tx) = &ctx.stream_tx {
        let _ = tx.send(crate::stream::StreamFrame::FileEditApplied {
            turn_id: ctx.turn_id.as_ref().map(ToString::to_string),
            run_id: ctx.flow_run_id.as_ref().map(ToString::to_string),
            tool_use_id: ctx.tool_use_id.clone(),
            tool_name: tool_name.to_string(),
            path,
            metrics,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edit_metrics_count_visual_line_changes_and_hunks() {
        let metrics = edit_metrics("one\ntwo\nthree\n", "one\nchanged\nthree\nadded\n");
        assert_eq!(
            metrics,
            EditMetrics {
                hunks: 1,
                insertions: 2,
                deletions: 1,
            }
        );
    }
}
