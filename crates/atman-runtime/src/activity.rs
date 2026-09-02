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

#[derive(Debug, Clone, Default)]
pub(crate) struct ActivityAccumulator {
    summary: ActivitySummary,
    attempted: std::collections::HashSet<(String, String)>,
    completed: std::collections::HashSet<(String, String)>,
    files: std::collections::BTreeSet<String>,
}

impl ActivityAccumulator {
    pub(crate) fn observe(&mut self, event: &crate::event::Event) {
        match event {
            crate::event::Event::ToolNode {
                run_id,
                tool_use_id,
                ..
            } => {
                if self
                    .attempted
                    .insert((run_id.to_string(), tool_use_id.clone()))
                {
                    self.summary.attempted_calls += 1;
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
                        if self.completed.insert(key) {
                            self.summary.completed_calls += 1;
                            self.summary.failed_calls += usize::from(*is_error);
                        }
                    }
                }
            }
            crate::event::Event::FileEditApplied { path, metrics, .. } => {
                self.summary.applied_edits += 1;
                self.files.insert(path.clone());
                self.summary.hunks += metrics.hunks;
                self.summary.insertions += metrics.insertions;
                self.summary.deletions += metrics.deletions;
            }
            _ => {}
        }
        self.summary.files = self.files.len();
    }

    pub(crate) fn summary(&self) -> ActivitySummary {
        self.summary.clone()
    }

    pub(crate) fn file_paths(&self) -> Vec<String> {
        self.files.iter().cloned().collect()
    }
}

pub fn summarize_events(events: &[crate::event::EventEnvelope]) -> ActivitySummary {
    let mut activity = ActivityAccumulator::default();
    for envelope in events {
        activity.observe(&envelope.event);
    }
    activity.summary()
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
