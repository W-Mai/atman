use std::collections::{HashMap, HashSet};
use std::time::Instant;

use atman_runtime::TranscriptEntry;
use atman_runtime::message::{Message, MessagePart, MessageRole};
use atman_runtime::stream::StreamFrame;
use atman_runtime::workflow::WorkflowGraph;

use crate::app::{NoteLevel, OutputItem};

pub fn flatten_transcript(entries: &[TranscriptEntry]) -> Vec<OutputItem> {
    let mut tool_map: HashMap<String, String> = HashMap::new();
    for entry in entries {
        if let TranscriptEntry::Message { message, .. } = entry {
            for part in &message.parts {
                if let MessagePart::ToolUse { id, name, .. } = part {
                    tool_map.insert(id.clone(), name.clone());
                }
            }
        }
    }

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
            cancelled: false,
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
            graph,
            ended_at,
            cancelled,
            ..
        }) = out.get_mut(idx)
        {
            graph.apply_stream_frame_at(frame, ts);
            if let StreamFrame::FlowDone {
                cancelled: flow_cancelled,
                ..
            } = frame
            {
                *ended_at = Some(Instant::now());
                *cancelled = *flow_cancelled;
            }
        }
    };
    for entry in entries {
        match entry {
            TranscriptEntry::Message {
                message: msg,
                flow_run_id,
            } => {
                if matches!(msg.role, MessageRole::System)
                    && matches!(msg.parts.as_slice(), [MessagePart::CompactSummary { .. }])
                {
                    if let Some(summary) = parse_compaction_summary(msg) {
                        out.push(summary);
                    }
                    continue;
                }
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
                flatten_message(msg, &mut out, &tool_map);
            }
            TranscriptEntry::DiffPreview {
                title,
                old_content,
                new_content,
                unified_diff,
            } => {
                out.push(OutputItem::DiffPreview {
                    title: title.clone(),
                    old_content: old_content.clone(),
                    new_content: new_content.clone(),
                    unified_diff: unified_diff.clone(),
                    expanded: false,
                });
            }
            TranscriptEntry::CompactionSummary {
                range_start,
                range_end,
                compacted_count,
                before_tokens,
                after_tokens,
                summary,
                ..
            } => {
                if matches!(
                    out.last(),
                    Some(OutputItem::CompactionSummary {
                        phase: atman_runtime::stream::CompactionPhase::Finished,
                        range_start: last_start,
                        range_end: last_end,
                        summary: last_summary,
                        ..
                    }) if *last_start == *range_start
                        && *last_end == *range_end
                        && last_summary == summary
                ) {
                    continue;
                }
                out.push(OutputItem::CompactionSummary {
                    phase: atman_runtime::stream::CompactionPhase::Finished,
                    range_start: *range_start,
                    range_end: *range_end,
                    summary: summary.clone(),
                    before_tokens: *before_tokens,
                    after_tokens: *after_tokens,
                    compacted_count: *compacted_count,
                    expanded: false,
                });
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
            TranscriptEntry::FlowDone {
                run_id,
                ok,
                cancelled,
                ts,
            } => {
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                apply_workflow(
                    &mut out,
                    panel_idx,
                    &StreamFrame::FlowDone {
                        run_id: run_id.clone(),
                        flow_name: String::new(),
                        ok: *ok,
                        cancelled: *cancelled,
                    },
                    *ts,
                );
            }
            TranscriptEntry::LlmCall {
                model,
                usage,
                wallclock_ms,
                ttft_ms,
                tokens_per_second,
                run_id,
                node_id,
                ts,
            } => {
                let panel_idx = ensure_panel(&mut out, &mut current_workflow_idx);
                apply_workflow(
                    &mut out,
                    panel_idx,
                    &StreamFrame::LlmCallStats {
                        model: model.clone(),
                        input_tokens: usage.input,
                        output_tokens: usage.output,
                        cache_read: usage.cached_input,
                        cache_write: usage.cache_write,
                        ttft_ms: ttft_ms.unwrap_or(0),
                        tokens_per_second: tokens_per_second.unwrap_or(0.0),
                        wallclock_ms: *wallclock_ms,
                        run_id: run_id.as_ref().map(|r| r.0.to_string()),
                        node_id: node_id.clone(),
                    },
                    *ts,
                );
            }
            TranscriptEntry::TerminalFinalState { handle, screen } => {
                // Overwrite the Terminal item's screen with the final state.
                for item in out.iter_mut() {
                    if let OutputItem::Terminal {
                        handle: h,
                        screen: s,
                        ..
                    } = item
                    {
                        if h == handle {
                            *s = screen.clone();
                        }
                    }
                }
            }
        }
    }
    // A restored session cannot have a flow still running.
    for item in out.iter_mut() {
        if let OutputItem::WorkflowPanel { ended_at: e, .. } = item {
            if e.is_none() {
                *e = Some(Instant::now());
            }
        }
    }
    dedup_by_handle(&mut out);
    out
}

/// Remove earlier Terminal/Bash items that share a handle with a later one.
/// When the later item has empty output but the earlier one has content
/// (last call was bash.spawn with no output, but a prior bash.output had
/// the real result), keep the one with content.
fn dedup_by_handle(out: &mut Vec<OutputItem>) {
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut i = 0;
    while i < out.len() {
        if let Some(h) = out[i].handle().map(|s| s.to_string()) {
            if let Some(&prev) = seen.get(&h) {
                let prev_has_content = bash_has_content(&out[prev]);
                let cur_has_content = bash_has_content(&out[i]);
                if prev_has_content && !cur_has_content {
                    // Keep the earlier item with content, skip current.
                    i += 1;
                    continue;
                }
                out.remove(prev);
                seen.iter_mut().for_each(|(_, idx)| {
                    if *idx > prev {
                        *idx -= 1;
                    }
                });
                seen.insert(h, i);
            } else {
                seen.insert(h, i);
                i += 1;
            }
        } else {
            i += 1;
        }
    }
}

fn bash_has_content(item: &OutputItem) -> bool {
    match item {
        OutputItem::Bash { output, .. } => !output.is_empty(),
        OutputItem::Terminal { screen, .. } => !screen.cells.is_empty(),
        _ => true,
    }
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

fn flatten_message(msg: &Message, out: &mut Vec<OutputItem>, tool_map: &HashMap<String, String>) {
    match msg.role {
        MessageRole::User => {
            let text = msg.text_concat();
            if !text.trim().is_empty() {
                out.push(OutputItem::UserTurn { text });
            }
        }
        MessageRole::Assistant => {
            for part in &msg.parts {
                match part {
                    MessagePart::Thinking { thinking, .. } => {
                        if !thinking.is_empty() {
                            out.push(OutputItem::Thinking {
                                text: thinking.clone(),
                                done: true,
                                expanded: false,
                            });
                        }
                    }
                    MessagePart::Text { text } => {
                        out.push(OutputItem::AssistantMd {
                            md: text.clone(),
                            streaming: false,
                        });
                    }
                    _ => {}
                }
            }
        }
        MessageRole::Tool => {
            for part in &msg.parts {
                if let MessagePart::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } = part
                {
                    let tool_name = tool_map.get(tool_use_id).map(|s| s.as_str()).unwrap_or("");
                    if let Some(item) = restore_tool_item(tool_name, content, *is_error) {
                        out.push(item);
                    }
                }
            }
        }
        MessageRole::System => {
            if let Some(summary) = parse_compaction_summary(msg) {
                out.push(summary);
            }
        }
    }
}

fn strip_log_prefixes(raw: &str) -> String {
    raw.lines()
        .map(|line| {
            if let Some(rest) = line.strip_prefix("[out] ") {
                rest
            } else if let Some(rest) = line.strip_prefix("[err] ") {
                rest
            } else {
                line
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn restore_tool_item(tool_name: &str, content: &str, is_error: bool) -> Option<OutputItem> {
    let parsed: serde_json::Value = serde_json::from_str(content).ok()?;
    if tool_name.starts_with("bash.") {
        let handle = parsed
            .get("handle")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let output = parsed
            .get("output")
            .and_then(|v| v.as_str())
            .map(strip_log_prefixes)
            .or_else(|| {
                parsed.get("log_path").and_then(|v| v.as_str()).map(|p| {
                    if !std::path::Path::new(p).exists() {
                        return format!("[log file removed: {p}]\n\n{content}");
                    }
                    let raw = std::fs::read_to_string(p).unwrap_or_default();
                    strip_log_prefixes(&raw)
                })
            })
            .unwrap_or_default();
        Some(OutputItem::Bash {
            handle,
            output,
            done: true,
            expanded: false,
        })
    } else if tool_name.starts_with("term.") || tool_name == "terminal" {
        use atman_runtime::tools::term::{
            DEFAULT_COLS, DEFAULT_ROWS, TerminalCell, TerminalScreen,
        };
        let handle = parsed
            .get("handle")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let rows = parsed
            .get("rows")
            .and_then(|v| v.as_i64())
            .map(|v| v as u16)
            .unwrap_or(DEFAULT_ROWS);
        let cols = parsed
            .get("cols")
            .and_then(|v| v.as_i64())
            .map(|v| v as u16)
            .unwrap_or(DEFAULT_COLS);
        let text = parsed.get("text").and_then(|v| v.as_str()).unwrap_or("");

        // TerminalFinalState events will overwrite this with the full cell grid.
        let mut cells = vec![TerminalCell::default(); (rows as usize) * (cols as usize)];
        for (row, line) in text.lines().enumerate() {
            if row >= rows as usize {
                break;
            }
            let row_start = row * cols as usize;
            for (col, ch) in line.chars().enumerate() {
                if col >= cols as usize {
                    break;
                }
                cells[row_start + col] = TerminalCell {
                    chars: ch.to_string(),
                    ..Default::default()
                };
            }
        }

        Some(OutputItem::Terminal {
            handle,
            screen: TerminalScreen {
                rows,
                cols,
                cells,
                cursor: None,
                alt_screen: false,
            },
            accumulated_bytes: text.as_bytes().to_vec(),
            mode: crate::app::TerminalViewMode::Capture,
            done: true,
            expanded: false,
            scroll_offset: None,
        })
    } else if tool_name.starts_with("fs.edit")
        || tool_name.starts_with("fs.write")
        || tool_name.starts_with("hunk.")
    {
        let diff = parsed.get("diff").and_then(|v| v.as_str())?;
        if diff.is_empty() {
            return None;
        }
        Some(OutputItem::DiffPreview {
            title: parsed
                .get("path")
                .and_then(|v| v.as_str())
                .unwrap_or(tool_name)
                .to_string(),
            old_content: None,
            new_content: None,
            unified_diff: Some(diff.to_string()),
            expanded: false,
        })
    } else if is_error {
        Some(OutputItem::SystemNote {
            text: format!("{tool_name} error: {content}"),
            level: NoteLevel::Warn,
        })
    } else {
        None
    }
}

fn parse_compaction_summary(msg: &Message) -> Option<OutputItem> {
    let footer = atman_runtime::compaction::find_compact_summaries(std::slice::from_ref(msg))
        .into_iter()
        .next()?;
    let body = msg.text_concat();
    Some(OutputItem::CompactionSummary {
        phase: atman_runtime::stream::CompactionPhase::Finished,
        range_start: footer.seq_start as usize,
        range_end: footer.seq_end as usize,
        summary: body,
        before_tokens: 0,
        after_tokens: 0,
        compacted_count: footer.count,
        expanded: false,
    })
}

pub fn flatten_messages(messages: &[Message]) -> Vec<OutputItem> {
    let tool_map: HashMap<String, String> = HashMap::new();
    let mut out: Vec<OutputItem> = Vec::new();
    for msg in messages {
        flatten_message(msg, &mut out, &tool_map);
    }
    dedup_by_handle(&mut out);
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
    fn user_message_becomes_turn() {
        let out = flatten_messages(&[user("hi")]);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], OutputItem::UserTurn { .. }));
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
    fn flatten_transcript_dedup_same_handle_terminal_with_final_state() {
        let handle = "term_s_0";
        let mk_tool_pair = |id: &str, text: &str| -> Vec<TranscriptEntry> {
            vec![
                TranscriptEntry::Message {
                    message: Message {
                        role: MessageRole::Assistant,
                        parts: vec![MessagePart::ToolUse {
                            id: id.into(),
                            name: "term.capture".into(),
                            input: serde_json::json!({}),
                        }],
                        turn_id: TurnId::now(),
                    },
                    flow_run_id: None,
                },
                TranscriptEntry::Message {
                    message: Message {
                        role: MessageRole::Tool,
                        parts: vec![MessagePart::ToolResult {
                            tool_use_id: id.into(),
                            content: format!(
                                r#"{{"handle":"{handle}","state":{{"kind":"running"}},"rows":2,"cols":3,"text":"{text}"}}"#
                            ),
                            is_error: false,
                        }],
                        turn_id: TurnId::now(),
                    },
                    flow_run_id: None,
                },
            ]
        };
        let mut entries = Vec::new();
        entries.extend(mk_tool_pair("c1", "aaa"));
        entries.extend(mk_tool_pair("c2", "bbb"));
        entries.extend(mk_tool_pair("c3", "ccc"));
        entries.push(TranscriptEntry::TerminalFinalState {
            handle: handle.into(),
            screen: atman_runtime::tools::term::TerminalScreen {
                rows: 2,
                cols: 3,
                cells: vec![atman_runtime::tools::term::TerminalCell::default(); 6],
                cursor: None,
                alt_screen: false,
            },
        });
        let out = flatten_transcript(&entries);
        let terminals: Vec<_> = out
            .iter()
            .filter(|it| matches!(it, OutputItem::Terminal { .. }))
            .collect();
        assert_eq!(terminals.len(), 1, "should dedup to 1 Terminal item");
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
                cancelled: false,
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

    #[test]
    fn flatten_transcript_closes_orphan_panels_without_flow_done() {
        use atman_runtime::nodegraph::FlowGraph as StaticFlowGraph;
        let entries = vec![TranscriptEntry::FlowGraph {
            run_id: "r1".into(),
            flow_name: "agent".into(),
            graph: StaticFlowGraph {
                flow_name: "agent".into(),
                root: Vec::new(),
            },
            ts: None,
        }];
        let out = flatten_transcript(&entries);
        let panel = out
            .iter()
            .find_map(|it| match it {
                OutputItem::WorkflowPanel { ended_at, .. } => Some(*ended_at),
                _ => None,
            })
            .expect("workflow panel");
        assert!(
            panel.is_some(),
            "orphan panel without FlowDone must be closed on restore"
        );
    }

    #[test]
    fn flatten_transcript_restores_bash_panel_from_tool_result() {
        let tool_use_id = "call_00_bash1";
        let entries = vec![
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::ToolUse {
                        id: tool_use_id.into(),
                        name: "bash.spawn".into(),
                        input: serde_json::json!({}),
                    }],
                    turn_id: TurnId::now(),
                },
                flow_run_id: None,
            },
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Tool,
                    parts: vec![MessagePart::ToolResult {
                        tool_use_id: tool_use_id.into(),
                        content: r#"{"handle":"bg_1","output":"hello\nworld","status":"exited","exit_code":0}"#.into(),
                        is_error: false,
                    }],
                    turn_id: TurnId::now(),
                },
                flow_run_id: None,
            },
        ];
        let out = flatten_transcript(&entries);
        let bash = out.iter().find_map(|it| match it {
            OutputItem::Bash { output, .. } => Some(output.clone()),
            _ => None,
        });
        assert_eq!(bash.as_deref(), Some("hello\nworld"));
    }

    #[test]
    fn flatten_transcript_restores_bash_from_log_path() {
        let dir = std::env::temp_dir().join(format!("atman_test_bash_log_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log_path = dir.join("bg_test1.log");
        std::fs::write(&log_path, "[out] line1\n[out] line2\n").unwrap();

        let tool_use_id = "call_00_bash2";
        let content = format!(
            r#"{{"handle":"bg_test1","status":"running","pid":123,"log_path":"{}"}}"#,
            log_path.display()
        );
        let entries = vec![
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::ToolUse {
                        id: tool_use_id.into(),
                        name: "bash.spawn".into(),
                        input: serde_json::json!({}),
                    }],
                    turn_id: TurnId::now(),
                },
                flow_run_id: None,
            },
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Tool,
                    parts: vec![MessagePart::ToolResult {
                        tool_use_id: tool_use_id.into(),
                        content,
                        is_error: false,
                    }],
                    turn_id: TurnId::now(),
                },
                flow_run_id: None,
            },
        ];
        let out = flatten_transcript(&entries);
        let bash = out.iter().find_map(|it| match it {
            OutputItem::Bash { output, .. } => Some(output.clone()),
            _ => None,
        });
        assert_eq!(bash.as_deref(), Some("line1\nline2"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn flatten_transcript_bash_log_missing_falls_back_to_json() {
        let tool_use_id = "call_00_bash3";
        let fake_path = "/tmp/atman_nonexistent_bg_test.log";
        let content = format!(
            r#"{{"handle":"bg_test2","status":"running","pid":123,"log_path":"{}"}}"#,
            fake_path
        );
        let entries = vec![
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::ToolUse {
                        id: tool_use_id.into(),
                        name: "bash.spawn".into(),
                        input: serde_json::json!({}),
                    }],
                    turn_id: TurnId::now(),
                },
                flow_run_id: None,
            },
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Tool,
                    parts: vec![MessagePart::ToolResult {
                        tool_use_id: tool_use_id.into(),
                        content,
                        is_error: false,
                    }],
                    turn_id: TurnId::now(),
                },
                flow_run_id: None,
            },
        ];
        let out = flatten_transcript(&entries);
        let bash = out.iter().find_map(|it| match it {
            OutputItem::Bash { output, .. } => Some(output.clone()),
            _ => None,
        });
        let bash = bash.expect("bash item should exist");
        assert!(
            bash.contains("[log file removed:"),
            "should show log removed notice: {bash}"
        );
        assert!(
            bash.contains("\"handle\":\"bg_test2\""),
            "should include original JSON: {bash}"
        );
    }

    #[test]
    fn flatten_transcript_restores_diff_preview_from_fs_edit() {
        let tool_use_id = "call_00_edit1";
        let diff = "--- a.rs\n+++ b.rs\n@@ -1,1 +1,2 @@\n-old\n+new\n";
        let entries = vec![
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::ToolUse {
                        id: tool_use_id.into(),
                        name: "fs.edit".into(),
                        input: serde_json::json!({}),
                    }],
                    turn_id: TurnId::now(),
                },
                flow_run_id: None,
            },
            TranscriptEntry::Message {
                message: Message {
                    role: MessageRole::Tool,
                    parts: vec![MessagePart::ToolResult {
                        tool_use_id: tool_use_id.into(),
                        content: format!(r#"{{"path":"a.rs","diff":{}}}"#, serde_json::json!(diff)),
                        is_error: false,
                    }],
                    turn_id: TurnId::now(),
                },
                flow_run_id: None,
            },
        ];
        let out = flatten_transcript(&entries);
        let diff_item = out.iter().find_map(|it| match it {
            OutputItem::DiffPreview { unified_diff, .. } => unified_diff.clone(),
            _ => None,
        });
        assert_eq!(diff_item.as_deref(), Some(diff));
    }

    #[test]
    fn flatten_transcript_skips_unknown_tool_results() {
        let tool_use_id = "call_00_unknown";
        let entries = vec![TranscriptEntry::Message {
            message: Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: tool_use_id.into(),
                    content: r#"{"result":"ok"}"#.into(),
                    is_error: false,
                }],
                turn_id: TurnId::now(),
            },
            flow_run_id: None,
        }];
        let out = flatten_transcript(&entries);
        assert!(
            out.is_empty(),
            "unknown tool without error should not produce an item"
        );
    }
}
