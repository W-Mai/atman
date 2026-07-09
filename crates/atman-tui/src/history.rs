use std::collections::HashSet;
use std::time::Instant;

use atman_runtime::TranscriptEntry;
use atman_runtime::message::{Message, MessagePart, MessageRole};
use atman_runtime::stream::StreamFrame;
use atman_runtime::workflow::WorkflowGraph;

use crate::app::{NoteLevel, OutputItem};

pub fn flatten_transcript(entries: &[TranscriptEntry]) -> Vec<OutputItem> {
    let mut out: Vec<OutputItem> = Vec::new();
    let mut current_workflow_idx: Option<usize> = None;
    let ensure_panel = |out: &mut Vec<OutputItem>, current: &mut Option<usize>| -> usize {
        if let Some(i) = *current {
            return i;
        }
        let turn_index = out
            .iter()
            .filter(|it| matches!(it, OutputItem::WorkflowPanel { .. }))
            .count();
        out.push(OutputItem::WorkflowPanel {
            turn_index,
            graph: WorkflowGraph::new(atman_runtime::event::TurnId::now()),
            expanded_nodes: HashSet::new(),
            panel_expanded: false,
            started_at: Instant::now(),
            ended_at: None,
        });
        let idx = out.len() - 1;
        *current = Some(idx);
        idx
    };
    let apply_workflow = |out: &mut Vec<OutputItem>,
                          idx: usize,
                          frame: &StreamFrame,
                          ts: Option<chrono::DateTime<chrono::Utc>>| {
        if let Some(OutputItem::WorkflowPanel {
            graph, ended_at, ..
        }) = out.get_mut(idx)
        {
            graph.apply_stream_frame_at(frame, ts);
            if let StreamFrame::FlowDone { .. } = frame {
                *ended_at = Some(Instant::now());
            }
        }
    };
    for entry in entries {
        match entry {
            TranscriptEntry::Message {
                message: msg,
                flow_run_id,
            } => {
                if matches!(msg.role, MessageRole::User)
                    && let Some(i) = current_workflow_idx.take()
                    && let Some(OutputItem::WorkflowPanel { ended_at, .. }) = out.get_mut(i)
                    && ended_at.is_none()
                {
                    *ended_at = Some(Instant::now());
                }
                if matches!(msg.role, MessageRole::Assistant | MessageRole::Tool)
                    && flow_run_id.is_some()
                    && let Some(idx) = current_workflow_idx
                    && let Some(OutputItem::WorkflowPanel { graph, .. }) = out.get_mut(idx)
                {
                    apply_message_to_workflow(graph, msg, flow_run_id.as_deref());
                }
                flatten_message(msg, &mut out);
            }
            TranscriptEntry::FlowGraph {
                run_id, graph, ts, ..
            } => {
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                apply_workflow(
                    &mut out,
                    panel_idx,
                    &StreamFrame::FlowGraph {
                        run_id: run_id.clone(),
                        graph: graph.clone(),
                    },
                    *ts,
                );
            }
            TranscriptEntry::FlowStart {
                run_id,
                flow_name,
                parent_run_id,
                parent_node_id,
                ts,
            } => {
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                apply_workflow(
                    &mut out,
                    panel_idx,
                    &StreamFrame::FlowStart {
                        run_id: run_id.clone(),
                        flow_name: flow_name.clone(),
                        parent_run_id: parent_run_id.clone(),
                        parent_node_id: parent_node_id.clone(),
                    },
                    *ts,
                );
            }
            TranscriptEntry::FlowNodeStart {
                run_id,
                node_id,
                kind,
                label,
                parent_node_id,
                ts,
            } => {
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                apply_workflow(
                    &mut out,
                    panel_idx,
                    &StreamFrame::FlowNodeStart {
                        run_id: run_id.clone(),
                        node_id: node_id.clone(),
                        kind: kind.clone(),
                        label: label.clone(),
                        parent_node_id: parent_node_id.clone(),
                    },
                    *ts,
                );
            }
            TranscriptEntry::FlowNodeEnd {
                run_id,
                node_id,
                status,
                output_preview,
                ts,
            } => {
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                apply_workflow(
                    &mut out,
                    panel_idx,
                    &StreamFrame::FlowNodeEnd {
                        run_id: run_id.clone(),
                        node_id: node_id.clone(),
                        status: status.clone(),
                        output_preview: output_preview.clone(),
                        parent_node_id: None,
                    },
                    *ts,
                );
            }
            TranscriptEntry::ToolNode {
                run_id,
                parent_node_id,
                tool_use_id,
                tool_name,
                args_preview,
                ts,
            } => {
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                apply_workflow(
                    &mut out,
                    panel_idx,
                    &StreamFrame::ToolNode {
                        run_id: run_id.clone(),
                        parent_node_id: parent_node_id.clone(),
                        tool_use_id: tool_use_id.clone(),
                        tool: tool_name.clone(),
                        args_preview: args_preview.clone(),
                    },
                    *ts,
                );
            }
            TranscriptEntry::FlowDone { run_id, ok, ts } => {
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                apply_workflow(
                    &mut out,
                    panel_idx,
                    &StreamFrame::FlowDone {
                        run_id: run_id.clone(),
                        flow_name: String::new(),
                        ok: *ok,
                    },
                    *ts,
                );
            }
        }
    }
    out
}

fn apply_message_to_workflow(graph: &mut WorkflowGraph, msg: &Message, flow_run_id: Option<&str>) {
    match msg.role {
        MessageRole::Assistant => {
            graph.apply_stream_frame(&StreamFrame::AssistantMsg {
                flow_run_id: flow_run_id.map(String::from),
                message: msg.clone(),
            });
        }
        MessageRole::Tool => {
            graph.apply_stream_frame(&StreamFrame::ToolResultMsg {
                flow_run_id: flow_run_id.map(String::from),
                message: msg.clone(),
            });
        }
        _ => {}
    }
}

fn flatten_message(msg: &Message, out: &mut Vec<OutputItem>) {
    match msg.role {
        MessageRole::User => {
            let text = msg.text_concat();
            if !text.trim().is_empty() {
                out.push(OutputItem::UserTurn { text });
            }
            out.push(OutputItem::Divider);
        }
        MessageRole::Assistant => {
            for part in &msg.parts {
                if let MessagePart::Text { text } = part {
                    out.push(OutputItem::AssistantMd {
                        md: text.clone(),
                        streaming: false,
                    });
                }
            }
        }
        MessageRole::Tool | MessageRole::System => {}
    }
}

pub fn flatten_messages(messages: &[Message]) -> Vec<OutputItem> {
    let mut out: Vec<OutputItem> = Vec::new();
    for msg in messages {
        flatten_message(msg, &mut out);
    }
    out
}

pub fn history_note(item_count: usize, message_count: usize) -> Option<OutputItem> {
    if item_count == 0 {
        return None;
    }
    Some(OutputItem::SystemNote {
        text: format!(
            "resumed with {message_count} prior message(s), {item_count} item(s) restored"
        ),
        level: NoteLevel::Info,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::event::{FlowNodeStatus, TurnId};

    fn assistant(parts: Vec<MessagePart>) -> Message {
        Message {
            role: MessageRole::Assistant,
            parts,
            turn_id: TurnId::now(),
        }
    }

    fn user(text: &str) -> Message {
        Message::user_text(TurnId::now(), text)
    }

    #[test]
    fn user_message_becomes_turn_plus_divider() {
        let out = flatten_messages(&[user("hi")]);
        assert_eq!(out.len(), 2);
        matches!(out[0], OutputItem::UserTurn { .. });
        matches!(out[1], OutputItem::Divider);
    }

    #[test]
    fn assistant_text_becomes_markdown_item() {
        let msgs = vec![assistant(vec![MessagePart::Text {
            text: "hello".into(),
        }])];
        let out = flatten_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], OutputItem::AssistantMd { .. }));
    }

    #[test]
    fn tool_use_and_tool_result_parts_do_not_produce_items() {
        use serde_json::json;
        let msgs = vec![
            assistant(vec![MessagePart::ToolUse {
                id: "toolu_1".into(),
                name: "fs.read".into(),
                input: json!({}),
            }]),
            Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    content: "12 bytes".into(),
                    is_error: false,
                }],
                turn_id: TurnId::now(),
            },
        ];
        let out = flatten_messages(&msgs);
        assert!(
            out.is_empty(),
            "tool traffic now flows through workflow panel, not messages: {out:?}"
        );
    }

    #[test]
    fn image_part_is_skipped_silently() {
        use atman_runtime::message::{ImageData, ImageSource};
        use std::path::PathBuf;
        let msgs = vec![assistant(vec![
            MessagePart::Text {
                text: "here".into(),
            },
            MessagePart::Image {
                source: ImageSource {
                    media_type: "image/png".into(),
                    data: ImageData::Path {
                        path: PathBuf::from("/tmp/x.png"),
                    },
                },
            },
        ])];
        let out = flatten_messages(&msgs);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], OutputItem::AssistantMd { .. }));
    }

    #[test]
    fn flatten_transcript_builds_workflow_panel_from_events() {
        use atman_runtime::nodegraph::FlowGraph as StaticFlowGraph;
        let entries = vec![
            TranscriptEntry::FlowGraph {
                run_id: "r1".into(),
                flow_name: "look_into".into(),
                graph: StaticFlowGraph {
                    flow_name: "look_into".into(),
                    root: Vec::new(),
                },
                ts: None,
            },
            TranscriptEntry::FlowNodeEnd {
                run_id: "r1".into(),
                node_id: "stmt_0".into(),
                status: FlowNodeStatus::Ok,
                output_preview: None,
                ts: None,
            },
            TranscriptEntry::FlowDone {
                run_id: "r1".into(),
                ok: true,
                ts: None,
            },
        ];
        let out = flatten_transcript(&entries);
        let panel = out
            .iter()
            .find_map(|it| match it {
                OutputItem::WorkflowPanel {
                    graph, ended_at, ..
                } => Some((graph, *ended_at)),
                _ => None,
            })
            .expect("workflow panel");
        assert_eq!(panel.0.root.len(), 1);
        assert_eq!(panel.0.root[0].label, "look_into");
        assert!(panel.1.is_some(), "FlowDone should close panel");
    }
}
