use std::collections::HashMap;
use std::path::Path;

use crate::event;
use crate::event_log::reader::{parse_json_lines, read_event_envelopes};
use crate::message::{Message, MessagePart};
use crate::nodegraph;
use crate::provider;
use crate::session::SessionOpenError;
use serde_json;

#[derive(Debug, Clone)]
pub enum TranscriptEntry {
    Message {
        message: Message,
        flow_run_id: Option<String>,
    },
    CompactionSummary {
        range_start: usize,
        range_end: usize,
        compacted_count: usize,
        before_tokens: u64,
        after_tokens: u64,
        summary: String,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    DiffPreview {
        title: String,
        old_content: Option<String>,
        new_content: Option<String>,
        unified_diff: Option<String>,
    },
    FlowGraph {
        run_id: String,
        flow_name: String,
        graph: nodegraph::FlowGraph,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    FlowStart {
        run_id: String,
        flow_name: String,
        parent_run_id: Option<String>,
        parent_node_id: Option<String>,
        spawned: bool,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    FlowNodeStart {
        run_id: String,
        node_id: String,
        kind: nodegraph::NodeKind,
        label: String,
        parent_node_id: Option<String>,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    FlowNodeEnd {
        run_id: String,
        node_id: String,
        status: event::FlowNodeStatus,
        output_preview: Option<String>,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    ToolNode {
        run_id: String,
        parent_node_id: String,
        tool_use_id: String,
        tool_name: String,
        args_preview: String,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    FlowDone {
        run_id: String,
        ok: bool,
        cancelled: bool,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    LlmCall {
        model: String,
        usage: provider::TokenUsage,
        wallclock_ms: u64,
        ttft_ms: Option<u64>,
        tokens_per_second: Option<f64>,
        run_id: Option<event::FlowRunId>,
        node_id: Option<String>,
        ts: Option<chrono::DateTime<chrono::Utc>>,
    },
    TerminalFinalState {
        handle: String,
        screen: crate::tools::term::TerminalScreen,
    },
    MermaidDiagram {
        source: String,
    },
}

pub fn replay_messages_from(path: &Path) -> Result<Vec<Message>, SessionOpenError> {
    Ok(replay_messages_with_seq(path)?
        .into_iter()
        .map(|(_, msg)| msg)
        .collect())
}

pub fn replay_messages_with_seq(path: &Path) -> Result<Vec<(u64, Message)>, SessionOpenError> {
    let envelopes = read_event_envelopes(path)?;
    Ok(envelopes.as_slice().to_messages_with_seq())
}

pub fn replay_all_messages_with_seq(path: &Path) -> Result<Vec<(u64, Message)>, SessionOpenError> {
    let envelopes = read_event_envelopes(path)?;
    Ok(envelopes
        .iter()
        .filter_map(|env| match &env.event {
            crate::event::Event::UserMsg { message, .. }
            | crate::event::Event::AssistantMsg { message, .. }
            | crate::event::Event::ToolResultMsg { message, .. }
            | crate::event::Event::SystemMsg { message, .. } => Some((env.seq, message.clone())),
            _ => None,
        })
        .collect())
}

#[derive(Debug, Clone)]
pub struct AttachmentPatch {
    part_index: usize,
    file_basename: String,
    reason: String,
}

pub fn parse_ts(v: &serde_json::Value) -> Option<chrono::DateTime<chrono::Utc>> {
    v.get("ts")?
        .as_str()
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&chrono::Utc))
}

pub fn parse_context_compact_event(v: &serde_json::Value) -> Option<CompactReplayEvent> {
    if v["type"].as_str() != Some("context_compact") {
        return None;
    }
    Some(CompactReplayEvent {
        range_start: v["compacted_range_start"].as_u64().unwrap_or(0) as usize,
        range_end: v["compacted_range_end"].as_u64().unwrap_or(0) as usize,
        replacement_msg_seq: v["replacement_msg_seq"].as_u64(),
    })
}

#[derive(Debug, Clone)]
pub struct CompactReplayEvent {
    range_start: usize,
    range_end: usize,
    replacement_msg_seq: Option<u64>,
}

pub fn collect_attachment_patches(
    values: &[serde_json::Value],
) -> HashMap<u64, Vec<AttachmentPatch>> {
    let mut map: HashMap<u64, Vec<AttachmentPatch>> = HashMap::new();
    for v in values {
        if v["type"].as_str() == Some("attachment_degraded") {
            let Some(msg_seq) = v["message_seq"].as_u64() else {
                continue;
            };
            let Some(part_index) = v["part_index"].as_u64() else {
                continue;
            };
            let file_basename = v["file_basename"].as_str().unwrap_or("").to_string();
            let reason = v["reason"].as_str().unwrap_or("degraded").to_string();
            map.entry(msg_seq).or_default().push(AttachmentPatch {
                part_index: part_index as usize,
                file_basename,
                reason,
            });
        }
    }
    map
}

pub fn apply_attachment_patches(msg: &mut Message, patches: &[AttachmentPatch]) {
    for p in patches {
        if let Some(part) = msg.parts.get_mut(p.part_index) {
            *part = MessagePart::Text {
                text: format!(
                    "[attachment unavailable: {} — {}]",
                    p.file_basename, p.reason
                ),
            };
        }
    }
}

pub fn replay_transcript_from(path: &Path) -> Result<Vec<TranscriptEntry>, SessionOpenError> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(SessionOpenError::Replay {
                path: path.to_path_buf(),
                source: e,
            });
        }
    };
    let values = parse_json_lines(&text);
    let patches = collect_attachment_patches(&values);
    let mut out = Vec::new();
    let mut msg_indices: Vec<usize> = Vec::new();
    let mut msg_seqs: Vec<u64> = Vec::new();
    for v in &values {
        let ty = v["type"].as_str().unwrap_or("");
        match ty {
            "user_msg" | "assistant_msg" | "tool_result_msg" | "system_msg" => {
                if let Some(m) = v.get("message")
                    && let Ok(mut msg) = serde_json::from_value::<Message>(m.clone())
                {
                    let seq = v["seq"].as_u64().unwrap_or(0);
                    if let Some(ps) = patches.get(&seq) {
                        apply_attachment_patches(&mut msg, ps);
                    }
                    let flow_run_id = v["flow_run_id"].as_str().map(String::from);
                    msg_indices.push(out.len());
                    msg_seqs.push(seq);
                    out.push(TranscriptEntry::Message {
                        message: msg,
                        flow_run_id,
                    });
                }
            }
            "context_compact" => {
                let Some(event) = parse_context_compact_event(v) else {
                    continue;
                };
                if event.range_start > event.range_end || event.range_end >= msg_indices.len() {
                    continue;
                }
                let Some(replacement_seq) = event.replacement_msg_seq else {
                    continue;
                };
                let Some(replacement_pos) = msg_seqs.iter().position(|seq| *seq == replacement_seq)
                else {
                    continue;
                };
                let replacement_out_idx = msg_indices[replacement_pos];
                let replacement_entry = out.remove(replacement_out_idx);
                let removed_out_start = msg_indices[event.range_start];
                let removed_count = event.range_end - event.range_start + 1;
                for _ in 0..removed_count {
                    out.remove(removed_out_start);
                }
                msg_indices.drain(event.range_start..=event.range_end);
                msg_seqs.drain(event.range_start..=event.range_end);
                out.insert(removed_out_start, replacement_entry);
                msg_indices.insert(event.range_start, removed_out_start);
                msg_seqs.insert(event.range_start, replacement_seq);
                for (i, ordinal_out_idx) in msg_indices.iter_mut().enumerate() {
                    if i > event.range_start {
                        *ordinal_out_idx =
                            ordinal_out_idx.saturating_sub(removed_count.saturating_sub(1));
                    }
                }
            }
            "compaction_summary" => {
                out.push(TranscriptEntry::CompactionSummary {
                    range_start: v["range_start"].as_u64().unwrap_or(0) as usize,
                    range_end: v["range_end"].as_u64().unwrap_or(0) as usize,
                    compacted_count: v["compacted_count"].as_u64().unwrap_or(0) as usize,
                    before_tokens: v["before_tokens"].as_u64().unwrap_or(0),
                    after_tokens: v["after_tokens"].as_u64().unwrap_or(0),
                    summary: v["summary"].as_str().unwrap_or("").to_string(),
                    ts: parse_ts(v),
                });
            }
            "diff_preview" => {
                out.push(TranscriptEntry::DiffPreview {
                    title: v["title"].as_str().unwrap_or("").to_string(),
                    old_content: v["old_content"].as_str().map(String::from),
                    new_content: v["new_content"].as_str().map(String::from),
                    unified_diff: v["unified_diff"].as_str().map(String::from),
                });
            }
            "flow_graph" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let flow_name = v
                    .get("graph")
                    .and_then(|g| g["flow_name"].as_str())
                    .unwrap_or("")
                    .to_string();
                let ts = parse_ts(v);
                if let Some(g) = v.get("graph")
                    && let Ok(graph) = serde_json::from_value::<nodegraph::FlowGraph>(g.clone())
                {
                    out.push(TranscriptEntry::FlowGraph {
                        run_id,
                        flow_name,
                        graph,
                        ts,
                    });
                }
            }
            "flow_start" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let flow_name = v["flow_name"].as_str().unwrap_or("").to_string();
                let parent_run_id = v["parent_run_id"].as_str().map(String::from);
                let parent_node_id = v["parent_node_id"].as_str().map(String::from);
                let spawned = v.get("spawned").and_then(|s| s.as_bool()).unwrap_or(false);
                let ts = parse_ts(v);
                out.push(TranscriptEntry::FlowStart {
                    run_id,
                    flow_name,
                    parent_run_id,
                    parent_node_id,
                    spawned,
                    ts,
                });
            }
            "flow_node_start" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let node_id = v["node_id"].as_str().unwrap_or("").to_string();
                let label = v["label"].as_str().unwrap_or(&node_id).to_string();
                let parent_node_id = v["parent_node_id"].as_str().map(String::from);
                let kind = v
                    .get("kind")
                    .and_then(|k| serde_json::from_value(k.clone()).ok())
                    .unwrap_or(nodegraph::NodeKind::UserConfirm);
                let ts = parse_ts(v);
                out.push(TranscriptEntry::FlowNodeStart {
                    run_id,
                    node_id,
                    kind,
                    label,
                    parent_node_id,
                    ts,
                });
            }
            "flow_node_end" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let node_id = v["node_id"].as_str().unwrap_or("").to_string();
                let status: event::FlowNodeStatus = v
                    .get("status")
                    .and_then(|s| serde_json::from_value(s.clone()).ok())
                    .unwrap_or(event::FlowNodeStatus::Ok);
                let output_preview = v["output_preview"].as_str().map(String::from);
                let ts = parse_ts(v);
                out.push(TranscriptEntry::FlowNodeEnd {
                    run_id,
                    node_id,
                    status,
                    output_preview,
                    ts,
                });
            }
            "tool_node" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let parent_node_id = v["parent_node_id"].as_str().unwrap_or("").to_string();
                let tool_use_id = v["tool_use_id"].as_str().unwrap_or("").to_string();
                let tool_name = v["tool_name"].as_str().unwrap_or("").to_string();
                let args_preview = v["args_preview"].as_str().unwrap_or("").to_string();
                let ts = parse_ts(v);
                out.push(TranscriptEntry::ToolNode {
                    run_id,
                    parent_node_id,
                    tool_use_id,
                    tool_name,
                    args_preview,
                    ts,
                });
            }
            "flow_end" => {
                let run_id = v["run_id"].as_str().unwrap_or("").to_string();
                let ok = v["status"]["kind"].as_str() == Some("ok");
                let cancelled = v["status"]["kind"].as_str() == Some("cancelled");
                let ts = parse_ts(v);
                out.push(TranscriptEntry::FlowDone {
                    run_id,
                    ok,
                    cancelled,
                    ts,
                });
            }
            "llm_call" => {
                let model = v["model"].as_str().unwrap_or("").to_string();
                let usage: provider::TokenUsage = v
                    .get("usage")
                    .and_then(|u| serde_json::from_value(u.clone()).ok())
                    .unwrap_or_default();
                let wallclock_ms = v["wallclock_ms"].as_u64().unwrap_or(0);
                let ttft_ms = v["ttft_ms"].as_u64();
                let tokens_per_second = v["tokens_per_second"].as_f64();
                let run_id = v["run_id"]
                    .as_str()
                    .and_then(|s| uuid::Uuid::parse_str(s).ok())
                    .map(event::FlowRunId);
                let node_id = v["node_id"].as_str().map(String::from);
                let ts = parse_ts(v);
                out.push(TranscriptEntry::LlmCall {
                    model,
                    usage,
                    wallclock_ms,
                    ttft_ms,
                    tokens_per_second,
                    run_id,
                    node_id,
                    ts,
                });
            }
            "terminal_final_state" => {
                let handle = v["handle"].as_str().unwrap_or("").to_string();
                if let Some(screen) = v.get("screen")
                    && let Ok(screen) =
                        serde_json::from_value::<crate::tools::term::TerminalScreen>(screen.clone())
                {
                    out.push(TranscriptEntry::TerminalFinalState { handle, screen });
                }
            }
            "mermaid_diagram" => {
                if let Some(source) = v.get("source").and_then(|s| s.as_str()) {
                    out.push(TranscriptEntry::MermaidDiagram {
                        source: source.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

pub trait MessageProjection {
    fn to_messages(&self) -> Vec<Message>;
    fn to_messages_with_seq(&self) -> Vec<(u64, Message)>;
}

impl MessageProjection for [crate::event::EventEnvelope] {
    fn to_messages(&self) -> Vec<Message> {
        self.to_messages_with_seq()
            .into_iter()
            .map(|(_, msg)| msg)
            .collect()
    }

    fn to_messages_with_seq(&self) -> Vec<(u64, Message)> {
        let mut acc: Vec<(u64, Message)> = Vec::new();
        for env in self {
            apply_envelope_to_messages(env, &mut acc);
        }
        acc
    }
}

pub(crate) fn apply_envelope_to_messages(
    env: &crate::event::EventEnvelope,
    acc: &mut Vec<(u64, Message)>,
) {
    match &env.event {
        crate::event::Event::UserMsg { message, .. }
        | crate::event::Event::AssistantMsg { message, .. }
        | crate::event::Event::ToolResultMsg { message, .. }
        | crate::event::Event::SystemMsg { message, .. } => {
            acc.push((env.seq, message.clone()));
        }
        crate::event::Event::ContextCompact {
            compacted_range_start,
            compacted_range_end,
            replacement_msg_seq,
            summary_text,
            after_tokens,
            before_tokens,
            ..
        } => {
            let range_start = *compacted_range_start as usize;
            let range_end = *compacted_range_end as usize;
            if range_start > range_end || range_end >= acc.len() {
                return;
            }
            let Some(rep_seq) = replacement_msg_seq else {
                return;
            };
            let Some(rep_idx) = acc.iter().position(|(s, _)| *s == *rep_seq) else {
                return;
            };
            if *after_tokens >= *before_tokens {
                return;
            }
            let replacement = acc.remove(rep_idx);
            let removed_count = range_end - range_start + 1;
            for _ in 0..removed_count {
                acc.remove(range_start);
            }
            let insertion_idx = range_start.min(acc.len());
            if let Some(summary) = summary_text {
                acc.insert(
                    insertion_idx,
                    (
                        *rep_seq,
                        Message::system_compact_summary(
                            crate::event::TurnId::now(),
                            summary.clone(),
                            range_start as u64,
                            range_end as u64,
                            removed_count,
                        ),
                    ),
                );
            } else {
                acc.insert(insertion_idx, replacement);
            }
        }
        crate::event::Event::AttachmentDegraded {
            message_seq,
            part_index,
            file_basename,
            reason,
            ..
        } => {
            if let Some((_, msg)) = acc.iter_mut().find(|(s, _)| *s == *message_seq) {
                if let Some(part) = msg.parts.get_mut(*part_index) {
                    *part = MessagePart::Text {
                        text: format!("[attachment unavailable: {} — {}]", file_basename, reason),
                    };
                }
            }
        }
        _ => {}
    }
}
