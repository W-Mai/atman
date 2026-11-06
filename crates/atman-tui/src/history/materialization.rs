//! Prepared tool artifacts for one captured native transcript boundary.

use std::{collections::HashMap, sync::Arc};

use atman_runtime::{
    event::AtmanUuid,
    event_log::journal::CommitReceipt,
    flow_runtime::FlowView,
    flow_store::FlowRef,
    message::{Message, MessagePart, MessageRole},
    projection::flow::FlowRecord,
    tools::tool_output::OutputStore,
};

type Scope = Option<(u64, atman_rt::RunId<AtmanUuid>)>;
type Names = HashMap<(Scope, String), bool>;
type Logs = HashMap<String, Result<Arc<str>, Arc<str>>>;

/// Immutable artifacts captured with a committed view. Reads never open original output or log files.
pub struct MaterializedTools {
    pub(crate) outputs: OutputStore,
    logs: Logs,
    flow: FlowRef,
    cursor: CommitReceipt,
    records: std::ops::Range<usize>,
}

impl MaterializedTools {
    pub(crate) fn covers(&self, view: &FlowView, applied_records: usize) -> bool {
        self.flow == *view.transcript().flow()
            && self.cursor == view.snapshot().status.cursor
            && self.records.end == view.transcript().entries().len()
            && self.records.start <= applied_records
            && applied_records <= self.records.end
    }
    pub(super) fn log(&self, path: &str) -> Option<Result<&str, &str>> {
        self.logs.get(path).map(|value| match value {
            Ok(output) => Ok(output.as_ref()),
            Err(error) => Err(error.as_ref()),
        })
    }
}

#[derive(Default)]
pub(crate) struct ToolMaterializer {
    captured: usize,
    names: Names,
}

impl ToolMaterializer {
    pub(crate) fn capture(
        &mut self,
        view: &FlowView,
        output: Option<&OutputStore>,
        full: bool,
    ) -> std::io::Result<MaterializedTools> {
        if matches!(
            view.transcript().content(),
            Some(atman_runtime::flow_store::view::FlowContentSummary::History(_))
        ) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "archived views require archive source preparation",
            ));
        }
        if full || self.captured > view.transcript().entries().len() {
            self.captured = 0;
            self.names.clear();
        }
        let start = self.captured;
        let entries = &view.transcript().entries()[start..];
        let contents = entries
            .iter()
            .flat_map(|entry| entry.record.shared_messages())
            .filter(|message| message.role == MessageRole::Tool)
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                MessagePart::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            });
        let outputs = output
            .map(|output| output.snapshot_references(contents))
            .unwrap_or_default();
        let mut logs = HashMap::new();
        for entry in entries {
            let scope = match &entry.record {
                FlowRecord::Input(input) => Some((input.lease.epoch, input.lease.run.clone())),
                FlowRecord::Publication(publication) => {
                    Some((publication.lease.epoch, publication.run.clone()))
                }
                _ => None,
            };
            if matches!(entry.record, FlowRecord::Continuation { .. }) {
                for message in entry.record.shared_messages() {
                    self.capture_calls(message, &scope);
                }
            }
            for message in entry.record.shared_messages() {
                self.capture_message(message, &scope, &outputs, &mut logs);
            }
        }
        self.captured = view.transcript().entries().len();
        Ok(MaterializedTools {
            outputs,
            logs,
            flow: view.transcript().flow().clone(),
            cursor: view.snapshot().status.cursor.clone(),
            records: start..self.captured,
        })
    }

    fn capture_message(
        &mut self,
        message: &Message,
        scope: &Scope,
        outputs: &OutputStore,
        logs: &mut Logs,
    ) {
        self.capture_calls(message, scope);
        for part in &message.parts {
            match (message.role, part) {
                (
                    MessageRole::Tool,
                    MessagePart::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    },
                ) if self.names.get(&(scope.clone(), tool_use_id.clone())) == Some(&true) => {
                    let content = outputs.retained_content(content).unwrap_or(content);
                    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(content) else {
                        continue;
                    };
                    if parsed
                        .get("output")
                        .and_then(serde_json::Value::as_str)
                        .is_some()
                    {
                        continue;
                    }
                    let Some(path) = parsed.get("log_path").and_then(serde_json::Value::as_str)
                    else {
                        continue;
                    };
                    logs.entry(path.to_owned()).or_insert_with(|| {
                        std::fs::read_to_string(path)
                            .map(|output| Arc::from(super::strip_log_prefixes(&output)))
                            .map_err(|error| {
                                Arc::from(if error.kind() == std::io::ErrorKind::NotFound {
                                    format!("log file removed: {path}")
                                } else {
                                    format!("log read failed: {path}: {error}")
                                })
                            })
                    });
                }
                _ => {}
            }
        }
    }

    fn capture_calls(&mut self, message: &Message, scope: &Scope) {
        if message.role != MessageRole::Assistant {
            return;
        }
        for part in &message.parts {
            if let MessagePart::ToolUse { id, name, .. } = part {
                self.names
                    .insert((scope.clone(), id.clone()), name.starts_with("bash."));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_reads_require_assistant_tool_metadata_and_matching_result_scope() {
        let mut materializer = ToolMaterializer::default();
        let scope = Some((1, atman_rt::RunId::now()));
        let other = Some((1, atman_rt::RunId::now()));
        let output = OutputStore::default();
        let mut logs = Logs::new();
        let mut call = Message::assistant_text(atman_rt::TurnId::now(), "");
        call.parts = vec![MessagePart::ToolUse {
            id: "call".into(),
            name: "bash.spawn".into(),
            input: serde_json::json!({}),
            intent: None,
        }];
        call.role = MessageRole::User;
        materializer.capture_message(&call, &scope, &output, &mut logs);
        let mut result = call.clone();
        result.role = MessageRole::Tool;
        result.parts = vec![MessagePart::ToolResult {
            tool_use_id: "call".into(),
            content: "{\"log_path\":\"/missing-test-log\"}".into(),
            is_error: false,
        }];
        materializer.capture_message(&result, &scope, &output, &mut logs);
        assert!(logs.is_empty());
        call.role = MessageRole::Assistant;
        materializer.capture_message(&call, &scope, &output, &mut logs);
        materializer.capture_message(&result, &other, &output, &mut logs);
        assert!(logs.is_empty());
        result.role = MessageRole::User;
        materializer.capture_message(&result, &scope, &output, &mut logs);
        assert!(logs.is_empty());
    }
}
