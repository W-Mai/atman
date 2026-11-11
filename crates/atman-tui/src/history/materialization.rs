//! Prepared tool artifacts for one captured native transcript boundary.

use std::{collections::HashMap, sync::Arc};

use atman_runtime::{
    event::AtmanUuid,
    flow_runtime::FlowView,
    flow_store::FlowRef,
    flow_store::position::FlowPosition,
    message::{Message, MessagePart, MessageRole},
    projection::flow::{FlowRecord, FlowRecordId},
    tools::tool_output::OutputStore,
};

type Scope = Option<(u64, atman_rt::RunId<AtmanUuid>)>;
type Names = HashMap<(Scope, String), super::ToolDisplayMeta>;
type Logs = HashMap<String, Result<Arc<str>, Arc<str>>>;

/// Blocking restoration output; installation needs no original message or file access.
pub(crate) struct PreparedMessages {
    pub(crate) revision: crate::app::OutputRevision,
    pub(crate) source: crate::flow_view::MessageVersion,
    pub(crate) items: Vec<crate::app::OutputItem>,
    pub(crate) final_keys: Vec<(atman_rt::TurnId<AtmanUuid>, usize)>,
}

impl PreparedMessages {
    pub(crate) fn capture(
        revision: crate::app::OutputRevision,
        messages: &[Message],
        output: Option<&OutputStore>,
    ) -> Self {
        Self::capture_iter(revision, messages.iter(), output)
    }

    pub(crate) fn capture_iter<'a>(
        revision: crate::app::OutputRevision,
        messages: impl Iterator<Item = &'a Message> + Clone,
        output: Option<&OutputStore>,
    ) -> Self {
        let mut counts = HashMap::new();
        let final_keys = messages
            .clone()
            .filter(|message| {
                message.role == MessageRole::Assistant
                    && message.origin == atman_runtime::message::MessageOrigin::FinalAnswer
            })
            .map(|message| {
                let count = counts.entry(message.turn_id.clone()).or_insert(0usize);
                let key = (message.turn_id.clone(), *count);
                *count += 1;
                key
            })
            .collect();
        Self {
            revision,
            source: crate::flow_view::MessageVersion::Revision(revision.id, revision.semantic),
            items: super::flatten_message_iter(messages, output, None),
            final_keys,
        }
    }
}

/// Immutable artifacts captured with a committed view. Reads never open original output or log files.
pub struct MaterializedTools {
    pub(crate) outputs: OutputStore,
    logs: Logs,
    flow: FlowRef,
    cursor: FlowPosition,
    records: std::ops::Range<usize>,
    details: HashMap<(FlowRecordId, String), Option<Arc<crate::app::OutputItem>>>,
}

impl MaterializedTools {
    pub(crate) fn detail(
        &self,
        record: FlowRecordId,
        id: &str,
    ) -> Option<&Option<Arc<crate::app::OutputItem>>> {
        self.details.get(&(record, id.to_owned()))
    }
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
        let mut tools = MaterializedTools {
            outputs,
            logs: HashMap::new(),
            flow: view.transcript().flow().clone(),
            cursor: view.snapshot().status.cursor.clone(),
            records: start..view.transcript().entries().len(),
            details: HashMap::new(),
        };
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
                self.capture_message(message, &scope, &tools.outputs, &mut tools.logs);
                if message.role == MessageRole::Tool {
                    for part in &message.parts {
                        if let MessagePart::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                        } = part
                        {
                            let meta = self.names.get(&(scope.clone(), tool_use_id.clone()));
                            let detail = super::restore_tool_item_with_sources(
                                meta,
                                content,
                                *is_error,
                                None,
                                Some(&tools),
                            )
                            .map(Arc::new);
                            tools
                                .details
                                .insert((entry.id, tool_use_id.clone()), detail);
                        }
                    }
                }
            }
        }
        self.captured = view.transcript().entries().len();
        Ok(tools)
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
                ) if self
                    .names
                    .get(&(scope.clone(), tool_use_id.clone()))
                    .is_some_and(|meta| meta.name.starts_with("bash.")) =>
                {
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
            if let MessagePart::ToolUse {
                id,
                name,
                input,
                intent,
            } = part
            {
                self.names.insert(
                    (scope.clone(), id.clone()),
                    super::ToolDisplayMeta::from_tool_use(name, input, intent.as_ref()),
                );
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
