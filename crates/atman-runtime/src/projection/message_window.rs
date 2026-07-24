use std::collections::HashMap;
use std::path::Path;

use crate::event;
use crate::event::TurnId;
use crate::event_log::reader::{load_last_checkpoint, parse_json_lines, read_jsonl_values};
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
}

pub fn replay_messages_from(path: &Path) -> Result<Vec<Message>, SessionOpenError> {
    if let Some((checkpoint_seq, messages)) = load_last_checkpoint(path)? {
        let values = read_jsonl_values(path)?;
        let patches = collect_attachment_patches(&values);
        let mut message_seqs: Vec<(u64, Message)> =
            messages.into_iter().map(|message| (0, message)).collect();
        for v in &values {
            let ty = v["type"].as_str().unwrap_or("");
            let seq = v["seq"].as_u64().unwrap_or(0);
            if seq <= checkpoint_seq {
                continue;
            }
            if let "user_msg" | "assistant_msg" | "tool_result_msg" | "system_msg" = ty {
                if let Some(m) = v.get("message")
                    && let Ok(mut msg) = serde_json::from_value::<Message>(m.clone())
                {
                    if let Some(seq) = v["seq"].as_u64()
                        && let Some(ps) = patches.get(&seq)
                    {
                        apply_attachment_patches(&mut msg, ps);
                    }
                    message_seqs.push((seq, msg));
                }
            } else if ty == "context_compact" {
                let Some(event) = parse_context_compact_event(v) else {
                    continue;
                };
                if event.range_start > event.range_end || event.range_end >= message_seqs.len() {
                    continue;
                }
                let Some(replacement_seq) = event.replacement_msg_seq else {
                    continue;
                };
                let Some(replacement_idx) = message_seqs
                    .iter()
                    .position(|(msg_seq, _)| *msg_seq == replacement_seq)
                else {
                    continue;
                };
                if event.after_tokens >= event.before_tokens {
                    continue;
                }
                let replacement = message_seqs.remove(replacement_idx);
                let removed_count = event.range_end - event.range_start + 1;
                for _ in 0..removed_count {
                    message_seqs.remove(event.range_start);
                }
                let insertion_idx = event.range_start.min(message_seqs.len());
                if let Some(summary) = event.summary_text {
                    message_seqs.insert(
                        insertion_idx,
                        (
                            replacement_seq,
                            Message::system_compact_summary(
                                TurnId::now(),
                                summary,
                                event.range_start as u64,
                                event.range_end as u64,
                                removed_count,
                            ),
                        ),
                    );
                } else {
                    message_seqs.insert(insertion_idx, replacement);
                }
            }
        }
        return Ok(message_seqs.into_iter().map(|(_, msg)| msg).collect());
    }
    let entries = replay_transcript_from(path)?;
    let mut out = Vec::new();
    for entry in entries {
        if let TranscriptEntry::Message { message, .. } = entry {
            out.push(message);
        }
    }
    Ok(out)
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
        before_tokens: v["before_tokens"].as_u64().unwrap_or(0),
        after_tokens: v["after_tokens"].as_u64().unwrap_or(0),
        summary_text: v
            .get("summary_text")
            .and_then(|s| s.as_str())
            .map(String::from),
        replacement_msg_seq: v["replacement_msg_seq"].as_u64(),
    })
}

#[derive(Debug, Clone)]
pub struct CompactReplayEvent {
    range_start: usize,
    range_end: usize,
    before_tokens: u64,
    after_tokens: u64,
    summary_text: Option<String>,
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
                let ts = parse_ts(v);
                out.push(TranscriptEntry::FlowStart {
                    run_id,
                    flow_name,
                    parent_run_id,
                    parent_node_id,
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
            _ => {}
        }
    }
    Ok(out)
}

use crate::event_log::reader::RawEventRow;

pub trait RawEventRowExt {
    fn find_checkpoint(&self) -> Option<(u64, Vec<Message>)>;
    fn replay_messages(&self, checkpoint: Option<(u64, Vec<Message>)>) -> Vec<Message>;
    fn collect_attachment_patches(&self) -> std::collections::HashMap<u64, Vec<AttachmentPatch>>;
}

impl RawEventRowExt for [RawEventRow] {
    fn find_checkpoint(&self) -> Option<(u64, Vec<Message>)> {
        self.iter().rev().find(|r| r.kind == "checkpoint").map(|r| {
            let seq = r.value["seq"].as_u64().unwrap_or(0);
            let messages = r.field("messages").unwrap_or_default();
            (seq, messages)
        })
    }

    fn replay_messages(&self, checkpoint: Option<(u64, Vec<Message>)>) -> Vec<Message> {
        let patches = self.collect_attachment_patches();
        let message_seqs: Vec<(u64, Message)> = if let Some((checkpoint_seq, messages)) = checkpoint
        {
            let base: Vec<(u64, Message)> = messages.into_iter().map(|m| (0, m)).collect();
            let mut acc = base;
            for row in self.iter() {
                if row.seq <= checkpoint_seq {
                    continue;
                }
                apply_row_to_acc(row, &mut acc, &patches);
            }
            acc
        } else {
            let mut acc = Vec::new();
            for row in self.iter() {
                apply_row_to_acc(row, &mut acc, &patches);
            }
            acc
        };
        message_seqs.into_iter().map(|(_, msg)| msg).collect()
    }

    fn collect_attachment_patches(&self) -> std::collections::HashMap<u64, Vec<AttachmentPatch>> {
        let mut map: std::collections::HashMap<u64, Vec<AttachmentPatch>> =
            std::collections::HashMap::new();
        for row in self.iter() {
            if row.kind != "attachment_degraded" {
                continue;
            }
            let Some(msg_seq) = row.field("message_seq") else {
                continue;
            };
            let Some(part_index) = row.field("part_index") else {
                continue;
            };
            map.entry(msg_seq).or_default().push(AttachmentPatch {
                part_index,
                file_basename: row.field("file_basename").unwrap_or_default(),
                reason: row.field("reason").unwrap_or_default(),
            });
        }
        map
    }
}

fn apply_row_to_acc(
    row: &RawEventRow,
    acc: &mut Vec<(u64, Message)>,
    patches: &std::collections::HashMap<u64, Vec<AttachmentPatch>>,
) {
    match row.kind.as_str() {
        "user_msg" | "assistant_msg" | "tool_result_msg" | "system_msg" => {
            if let Some(mut msg) = row.field::<Message>("message") {
                if let Some(ps) = patches.get(&row.seq) {
                    apply_attachment_patches(&mut msg, ps);
                }
                acc.push((row.seq, msg));
            }
        }
        "context_compact" => {
            let Some(event) = parse_context_compact_event(&row.value) else {
                return;
            };
            if event.range_start > event.range_end || event.range_end >= acc.len() {
                return;
            }
            let Some(replacement_seq) = event.replacement_msg_seq else {
                return;
            };
            let Some(replacement_idx) = acc.iter().position(|(s, _)| *s == replacement_seq) else {
                return;
            };
            if event.after_tokens >= event.before_tokens {
                return;
            }
            let replacement = acc.remove(replacement_idx);
            let removed_count = event.range_end - event.range_start + 1;
            for _ in 0..removed_count {
                acc.remove(event.range_start);
            }
            let insertion_idx = event.range_start.min(acc.len());
            if let Some(summary) = event.summary_text {
                acc.insert(
                    insertion_idx,
                    (
                        replacement_seq,
                        Message::system_compact_summary(
                            crate::event::TurnId::now(),
                            summary,
                            event.range_start as u64,
                            event.range_end as u64,
                            removed_count,
                        ),
                    ),
                );
            } else {
                acc.insert(insertion_idx, replacement);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnId;
    use crate::event_log::reader::RawEventRow;
    use crate::message::{MessagePart, MessageRole};
    use serde_json::json;

    fn row(kind: &str, seq: u64, value: serde_json::Value) -> RawEventRow {
        RawEventRow {
            seq,
            kind: kind.to_string(),
            value,
        }
    }

    fn user_msg_row(seq: u64, text: &str) -> RawEventRow {
        let turn = TurnId::now();
        row(
            "user_msg",
            seq,
            json!({"type":"user_msg","seq":seq,"turn_id":turn.to_string(),"message":{"role":"user","parts":[{"type":"text","text":text}],"turn_id":turn.to_string()}}),
        )
    }

    fn assistant_msg_row(seq: u64, text: &str) -> RawEventRow {
        let turn = TurnId::now();
        row(
            "assistant_msg",
            seq,
            json!({"type":"assistant_msg","seq":seq,"turn_id":turn.to_string(),"message":{"role":"assistant","parts":[{"type":"text","text":text}],"turn_id":turn.to_string()}}),
        )
    }

    #[test]
    fn replay_messages_composition() {
        let rows = vec![
            user_msg_row(1, "hello"),
            assistant_msg_row(2, "hi there"),
            user_msg_row(3, "how are you"),
        ];
        let msgs = rows.replay_messages(None);
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0].text_concat(), "hello");
        assert_eq!(msgs[2].text_concat(), "how are you");
    }

    #[test]
    fn replay_with_checkpoint_skips_old() {
        let summary = Message::system_compact_summary(TurnId::now(), "summary", 0, 3, 4);
        let rows = vec![
            user_msg_row(1, "old1"),
            user_msg_row(2, "old2"),
            user_msg_row(5, "after checkpoint"),
        ];
        let msgs = rows.replay_messages(Some((3, vec![summary.clone()])));
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0].parts[0], MessagePart::CompactSummary { .. }));
        assert_eq!(msgs[1].text_concat(), "after checkpoint");
    }

    #[test]
    fn replay_with_attachment_degraded() {
        let turn = TurnId::now();
        let rows = vec![
            row(
                "user_msg",
                1,
                json!({"type":"user_msg","seq":1,"turn_id":turn.to_string(),"message":{"role":"user","parts":[{"type":"image","source":{"media_type":"image/png","data":{"kind":"path","path":"/tmp/x.png"}}},{"type":"text","text":"describe"}],"turn_id":turn.to_string()}}),
            ),
            row(
                "attachment_degraded",
                2,
                json!({"type":"attachment_degraded","seq":2,"message_seq":1,"part_index":0,"file_basename":"x.png","reason":"image_too_large"}),
            ),
        ];
        let msgs = rows.replay_messages(None);
        assert_eq!(msgs.len(), 1);
        assert!(msgs[0].text_concat().contains("attachment unavailable"));
    }

    #[test]
    fn replay_with_context_compact() {
        let rows = vec![
            user_msg_row(1, "old user 1"),
            assistant_msg_row(2, "old assistant 1"),
            user_msg_row(3, "old user 2"),
            row(
                "system_msg",
                4,
                json!({"type":"system_msg","seq":4,"turn_id":TurnId::now().to_string(),"message":{"role":"system","parts":[{"type":"compact_summary","summary":"compacted","seq_start":0,"seq_end":2,"count":3}],"turn_id":TurnId::now().to_string()}}),
            ),
            row(
                "context_compact",
                5,
                json!({"type":"context_compact","seq":5,"session_id":"x","before_tokens":100,"after_tokens":50,"compacted_range_start":0,"compacted_range_end":2,"summary_text":"compaction summary text","replacement_msg_seq":4}),
            ),
            user_msg_row(6, "after compact"),
        ];
        let msgs = rows.replay_messages(None);
        assert_eq!(msgs.len(), 2);
        assert!(matches!(msgs[0].parts[0], MessagePart::CompactSummary { .. }));
        assert_eq!(msgs[1].text_concat(), "after compact");
    }

    #[test]
    fn find_checkpoint_finds_last() {
        let rows = vec![
            user_msg_row(1, "a"),
            row(
                "checkpoint",
                2,
                json!({"type":"checkpoint","seq":2,"session_id":"x","messages":[{"role":"user","parts":[{"type":"text","text":"a"}],"turn_id":TurnId::now().to_string()}],"window_tokens":10}),
            ),
            user_msg_row(3, "b"),
            row(
                "checkpoint",
                4,
                json!({"type":"checkpoint","seq":4,"session_id":"x","messages":[{"role":"user","parts":[{"type":"text","text":"b"}],"turn_id":TurnId::now().to_string()}],"window_tokens":5}),
            ),
        ];
        let (seq, msgs) = rows.find_checkpoint().unwrap();
        assert_eq!(seq, 4);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].text_concat(), "b");
    }
}
