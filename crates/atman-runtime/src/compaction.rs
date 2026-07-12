use crate::message::{Message, MessagePart, MessageRole};

pub fn estimate_tokens_for_message(msg: &Message) -> u64 {
    let mut chars = 0usize;
    for part in &msg.parts {
        chars += match part {
            MessagePart::Text { text } => text.len(),
            MessagePart::Thinking { thinking, .. } => thinking.len(),
            MessagePart::ToolResult { content, .. } => content.len(),
            MessagePart::Image { .. } => 512,
            MessagePart::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
        };
    }
    chars = chars.saturating_add(estimate_role_overhead(msg.role));
    (chars as f64 / 3.5).ceil() as u64
}

fn estimate_role_overhead(role: MessageRole) -> usize {
    match role {
        MessageRole::System => 12,
        MessageRole::User => 8,
        MessageRole::Assistant => 8,
        MessageRole::Tool => 16,
    }
}

pub fn estimate_tokens_for_messages(messages: &[Message]) -> u64 {
    messages.iter().map(estimate_tokens_for_message).sum()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactRange {
    pub start: usize,
    pub end: usize,
    pub tokens_saved_estimate: u64,
}

pub fn is_plan_related(msg: &Message) -> bool {
    for part in &msg.parts {
        match part {
            MessagePart::ToolUse { name, .. } if name.starts_with("plan.") => return true,
            MessagePart::ToolResult { content, .. } if content.starts_with("# Plan:") => {
                return true;
            }
            _ => {}
        }
    }
    false
}

pub fn is_compaction_summary(msg: &Message) -> bool {
    if !matches!(msg.role, MessageRole::System) {
        return false;
    }
    msg.text_concat().contains("[atman:compact")
}

pub fn find_compact_range(messages: &[Message], budget: u64) -> Option<CompactRange> {
    let total = estimate_tokens_for_messages(messages);
    if total <= budget || messages.len() < 4 {
        return None;
    }
    let head = 0usize;
    let tail = messages.len().saturating_sub(2);
    if tail <= head + 2 {
        return None;
    }
    let mut best: Option<CompactRange> = None;
    let mut cur_start: Option<usize> = None;
    let mut cur_tokens: u64 = 0;
    for (i, msg) in messages.iter().enumerate().take(tail).skip(head) {
        let is_barrier = (matches!(msg.role, MessageRole::System) && !is_compaction_summary(msg))
            || is_plan_related(msg);
        if is_barrier {
            if let Some(start) = cur_start.take()
                && i - start >= 3
            {
                let range = CompactRange {
                    start,
                    end: i,
                    tokens_saved_estimate: cur_tokens,
                };
                if best
                    .as_ref()
                    .is_none_or(|b| range.tokens_saved_estimate > b.tokens_saved_estimate)
                {
                    best = Some(range);
                }
            }
            cur_start = None;
            cur_tokens = 0;
            continue;
        }
        if cur_start.is_none() {
            cur_start = Some(i);
            cur_tokens = 0;
        }
        cur_tokens += estimate_tokens_for_message(msg);
    }
    if let Some(start) = cur_start
        && tail - start >= 3
    {
        let range = CompactRange {
            start,
            end: tail,
            tokens_saved_estimate: cur_tokens,
        };
        if best
            .as_ref()
            .is_none_or(|b| range.tokens_saved_estimate > b.tokens_saved_estimate)
        {
            best = Some(range);
        }
    }
    best
}

pub fn find_compact_summaries(messages: &[Message]) -> Vec<CompactSummary> {
    let mut out = Vec::new();
    for (idx, msg) in messages.iter().enumerate() {
        if msg.role != MessageRole::System {
            continue;
        }
        let text = msg.text_concat();
        if let Some(summary) = parse_compact_footer(&text, idx) {
            out.push(summary);
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactSummary {
    pub message_index: usize,
    pub seq_start: u64,
    pub seq_end: u64,
    pub count: usize,
}

fn parse_compact_footer(text: &str, idx: usize) -> Option<CompactSummary> {
    let start_marker = "[atman:compact ";
    let start = text.rfind(start_marker)?;
    let after = &text[start + start_marker.len()..];
    let end = after.find(']')?;
    let inner = &after[..end];
    let mut seq_start = None;
    let mut seq_end = None;
    let mut count = None;
    for token in inner.split_whitespace() {
        let Some((k, v)) = token.split_once('=') else {
            continue;
        };
        match k {
            "seq_start" => seq_start = v.parse().ok(),
            "seq_end" => seq_end = v.parse().ok(),
            "count" => count = v.parse().ok(),
            _ => {}
        }
    }
    Some(CompactSummary {
        message_index: idx,
        seq_start: seq_start?,
        seq_end: seq_end?,
        count: count?,
    })
}

pub async fn maybe_auto_compact(
    session: &crate::session::Session,
    model: &str,
    providers: &crate::provider::ProviderRegistry,
) {
    let forced = session.take_manual_compact_request();
    let info = crate::model_registry::model_info(model);
    let threshold = info.compact_threshold_tokens();
    let msgs = session.messages();
    let current = estimate_tokens_for_messages(&msgs);
    if !forced && current <= threshold {
        return;
    }
    if !forced && !session.approval_cooldown_ok_for_compact() {
        return;
    }
    let Some(range) = find_compact_range(&msgs, info.context_budget) else {
        session.emit_compact_warning(
            model,
            current,
            threshold,
            info.context_budget,
            "no compactible span — history too short or already fully compacted",
        );
        return;
    };
    let slice = &msgs[range.start..range.end];
    let summary = match generate_llm_summary(slice, model, providers).await {
        Ok(text) => text,
        Err(err) => {
            session.emit_compact_warning(
                model,
                current,
                threshold,
                info.context_budget,
                &format!("LLM summary failed: {err}. Degraded to placeholder."),
            );
            format!(
                "[atman: compacted {} messages, LLM summary unavailable at {}]",
                range.end - range.start,
                chrono::Utc::now().to_rfc3339()
            )
        }
    };
    let final_summary =
        match request_review_if_enabled(session, forced, slice, &range, current, summary).await {
            ReviewOutcome::Commit(s) => s,
            ReviewOutcome::Rejected => {
                session.push_system_note(
                    "compaction rejected by user; keeping full transcript".into(),
                );
                return;
            }
        };
    match session.compact_messages(final_summary) {
        Some(result) => {
            session.push_system_note(format!(
                "auto-compacted {}..{} — {} → {} tokens",
                result.compacted_start,
                result.compacted_end,
                result.before_tokens,
                result.after_tokens
            ));
        }
        None => {
            session.emit_compact_warning(
                model,
                current,
                threshold,
                info.context_budget,
                "no compactible span — history too short or already fully compacted",
            );
        }
    }
}

enum ReviewOutcome {
    Commit(String),
    Rejected,
}

async fn request_review_if_enabled(
    session: &crate::session::Session,
    forced: bool,
    slice: &[Message],
    range: &CompactRange,
    tokens_before: u64,
    summary: String,
) -> ReviewOutcome {
    if !session.compact_review_mode().should_review(forced) {
        return ReviewOutcome::Commit(summary);
    }
    let reviews = session.compact_reviews();
    if reviews.subscriber_count() == 0 {
        return ReviewOutcome::Commit(summary);
    }
    let pending = crate::session::PendingCompactReview {
        review_id: uuid::Uuid::now_v7().to_string(),
        summary: summary.clone(),
        slice_preview: format_slice_for_preview(slice),
        slice_count: slice.len(),
        range_start: range.start,
        range_end: range.end,
        tokens_before,
        emitted_at: chrono::Utc::now(),
    };
    let rx = reviews.request(pending);
    match rx.await {
        Ok(crate::session::CompactReviewDecision::AcceptAsIs) => ReviewOutcome::Commit(summary),
        Ok(crate::session::CompactReviewDecision::AcceptEdited { summary: edited }) => {
            ReviewOutcome::Commit(edited)
        }
        Ok(crate::session::CompactReviewDecision::Reject) | Err(_) => ReviewOutcome::Rejected,
    }
}

fn format_slice_for_preview(slice: &[Message]) -> String {
    let mut out = String::new();
    for (i, msg) in slice.iter().enumerate() {
        let role = msg.role.as_str();
        let body = serialize_message_for_summary(msg);
        let truncated: String = body.chars().take(400).collect();
        out.push_str(&format!("[{i}] {role}: {truncated}\n"));
    }
    out.chars().take(16_000).collect()
}

const SUMMARY_SYSTEM_PROMPT: &str = "You are summarizing a slice of an ongoing conversation so a future model can continue without losing context. Write 200-400 words focused on: key facts and decisions (files touched, tools invoked, verdicts reached); open threads (unfinished tasks, unresolved questions); what the user asked and what the assistant delivered. Rules: no code fences; include file, function, library, package names verbatim; write in past tense first-person from the assistant's perspective (\"I investigated foo.rs, decided to...\"); no speculation, only what actually happened in the transcript.";

async fn generate_llm_summary(
    slice: &[Message],
    model: &str,
    providers: &crate::provider::ProviderRegistry,
) -> Result<String, crate::error::RuntimeError> {
    let provider = providers.resolve(model).ok_or_else(|| {
        crate::error::RuntimeError::ToolFailed(format!("no provider for {model}"))
    })?;
    let payload = format_slice_for_summary(slice);
    let user = format!("Summarize these {} messages:\n\n{}", slice.len(), payload);
    let req = crate::provider::LlmRequest {
        model: model.into(),
        messages: vec![Message::user_text(crate::event::TurnId::now(), user)],
        system: Some(SUMMARY_SYSTEM_PROMPT.into()),
        input: crate::value::Value::Unit,
        schema: None,
        cache_prompt: false,
        tools: Vec::new(),
        thinking_enabled: false,
    };
    let outcome = provider.call(req).await?;
    let text = outcome.text_concat();
    if text.trim().is_empty() {
        return Err(crate::error::RuntimeError::ToolFailed(
            "empty summary from provider".into(),
        ));
    }
    Ok(text)
}

fn format_slice_for_summary(slice: &[Message]) -> String {
    let mut out = String::new();
    for (i, msg) in slice.iter().enumerate() {
        let role = msg.role.as_str();
        let body = serialize_message_for_summary(msg);
        let truncated: String = body.chars().take(2000).collect();
        out.push_str(&format!("[{i}] {role}: {truncated}\n\n"));
    }
    out.chars().take(60_000).collect()
}

fn serialize_message_for_summary(msg: &Message) -> String {
    let mut parts = Vec::new();
    for part in &msg.parts {
        match part {
            MessagePart::Text { text } => {
                parts.push(text.clone());
            }
            MessagePart::Thinking { thinking, .. } => {
                let truncated: String = thinking.chars().take(500).collect();
                parts.push(format!("[thinking: {truncated}]"));
            }
            MessagePart::ToolUse { name, input, .. } => {
                let input_str = if input.is_null() {
                    String::new()
                } else {
                    input.to_string()
                };
                let truncated: String = input_str.chars().take(800).collect();
                parts.push(format!("[tool_call: {name}({truncated})]"));
            }
            MessagePart::ToolResult {
                content,
                is_error,
                tool_use_id,
            } => {
                let truncated: String = content.chars().take(1000).collect();
                let marker = if *is_error { "ERROR" } else { "ok" };
                let id_short: String = tool_use_id.chars().take(12).collect();
                parts.push(format!("[tool_result {id_short}… {marker}: {truncated}]"));
            }
            MessagePart::Image { .. } => {
                parts.push("[image]".into());
            }
        }
    }
    parts.join(" ")
}

pub fn replace_range_with_summary(
    messages: &[Message],
    range: &CompactRange,
    summary: String,
    turn_id: crate::event::TurnId,
) -> Vec<Message> {
    let mut out = Vec::with_capacity(messages.len() - (range.end - range.start) + 1);
    out.extend_from_slice(&messages[..range.start]);
    let compacted_marker = format!(
        "[atman: compacted {} messages]\n{}",
        range.end - range.start,
        summary
    );
    out.push(Message::system_text(turn_id, compacted_marker));
    out.extend_from_slice(&messages[range.end..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnId;

    fn user(text: &str) -> Message {
        Message::user_text(TurnId::now(), text)
    }
    fn assistant(text: &str) -> Message {
        Message::assistant_text(TurnId::now(), text)
    }
    fn system(text: &str) -> Message {
        Message::system_text(TurnId::now(), text)
    }

    #[test]
    fn estimate_scales_with_char_length() {
        let short = user("hi");
        let long = user(&"x".repeat(3500));
        assert!(estimate_tokens_for_message(&long) > estimate_tokens_for_message(&short) * 100);
    }

    #[test]
    fn find_compact_returns_none_when_under_budget() {
        let msgs = vec![user("a"), assistant("b"), user("c"), assistant("d")];
        assert!(find_compact_range(&msgs, 1000).is_none());
    }

    #[test]
    fn find_compact_returns_none_for_short_history() {
        let msgs = vec![user(&"x".repeat(9000))];
        assert!(find_compact_range(&msgs, 100).is_none());
    }

    #[test]
    fn find_compact_targets_middle_run_of_at_least_3() {
        let msgs = vec![
            system("keep-system-head"),
            user(&"x".repeat(5000)),
            assistant(&"y".repeat(5000)),
            user(&"z".repeat(5000)),
            assistant(&"w".repeat(5000)),
            user("tail-user"),
            assistant("tail-assistant"),
        ];
        let range = find_compact_range(&msgs, 500).expect("expected compact range");
        assert!(range.start >= 1, "system head must be preserved");
        assert!(
            range.end <= msgs.len() - 2,
            "last 2 messages must be preserved"
        );
        assert!(range.end - range.start >= 3, "range must cover >= 3 msgs");
    }

    #[test]
    fn find_compact_skips_system_boundary() {
        let msgs = vec![
            user(&"x".repeat(3000)),
            assistant(&"y".repeat(3000)),
            system("mid-system-break"),
            user(&"a".repeat(3000)),
            assistant(&"b".repeat(3000)),
            user(&"c".repeat(3000)),
            assistant(&"d".repeat(3000)),
            user("tail"),
        ];
        let range = find_compact_range(&msgs, 500).expect("expected range");
        assert!(
            range.start >= 3,
            "must start after mid-system, got {range:?}"
        );
    }

    #[test]
    fn replace_range_puts_summary_system_message_in_place() {
        let msgs = vec![
            system("head"),
            user("m1"),
            assistant("m2"),
            user("m3"),
            assistant("m4"),
            user("tail"),
        ];
        let range = CompactRange {
            start: 1,
            end: 5,
            tokens_saved_estimate: 100,
        };
        let out = replace_range_with_summary(
            &msgs,
            &range,
            "gist: talked about m1..m4".into(),
            TurnId::now(),
        );
        assert_eq!(out.len(), 3, "1 head + 1 summary + 1 tail");
        assert_eq!(out[0].role, MessageRole::System);
        assert_eq!(out[0].text_concat(), "head");
        assert_eq!(out[1].role, MessageRole::System);
        assert!(out[1].text_concat().contains("compacted 4 messages"));
        assert!(out[1].text_concat().contains("gist: talked about"));
        assert_eq!(out[2].role, MessageRole::User);
        assert_eq!(out[2].text_concat(), "tail");
    }

    fn assistant_with_tool_use(text: &str, tool_name: &str, input: serde_json::Value) -> Message {
        Message {
            role: MessageRole::Assistant,
            parts: vec![
                MessagePart::Text { text: text.into() },
                MessagePart::ToolUse {
                    id: "call_test".into(),
                    name: tool_name.into(),
                    input,
                },
            ],
            turn_id: TurnId::now(),
        }
    }

    fn tool_result(id: &str, content: &str, is_error: bool) -> Message {
        Message {
            role: MessageRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_use_id: id.into(),
                content: content.into(),
                is_error,
            }],
            turn_id: TurnId::now(),
        }
    }

    fn thinking(text: &str) -> Message {
        Message {
            role: MessageRole::Assistant,
            parts: vec![
                MessagePart::Thinking {
                    thinking: text.into(),
                    signature: None,
                },
                MessagePart::Text {
                    text: "after thinking".into(),
                },
            ],
            turn_id: TurnId::now(),
        }
    }

    #[test]
    fn format_slice_for_summary_includes_tool_use() {
        let slice = vec![
            user("read the file"),
            assistant_with_tool_use(
                "let me check",
                "fs.read",
                serde_json::json!({"path": "/tmp/foo.rs"}),
            ),
            tool_result("call_test", "fn main() {}", false),
        ];
        let out = format_slice_for_summary(&slice);
        assert!(out.contains("fs.read"), "missing tool name: {out}");
        assert!(out.contains("/tmp/foo.rs"), "missing tool input: {out}");
        assert!(
            out.contains("fn main()"),
            "missing tool_result content: {out}"
        );
        assert!(out.contains("tool_call"), "missing tool_call marker: {out}");
        assert!(
            out.contains("tool_result"),
            "missing tool_result marker: {out}"
        );
    }

    #[test]
    fn format_slice_for_summary_includes_thinking() {
        let slice = vec![thinking("I should consider the edge case")];
        let out = format_slice_for_summary(&slice);
        assert!(out.contains("thinking"), "missing thinking marker: {out}");
        assert!(out.contains("edge case"), "missing thinking content: {out}");
    }

    #[test]
    fn format_slice_for_summary_marks_error_tool_results() {
        let slice = vec![tool_result("call_1", "permission denied", true)];
        let out = format_slice_for_summary(&slice);
        assert!(out.contains("ERROR"), "missing ERROR marker: {out}");
    }

    #[test]
    fn format_slice_for_summary_truncates_long_tool_input() {
        let long_input = serde_json::json!({"content": "x".repeat(2000)});
        let slice = vec![assistant_with_tool_use("check", "fs.write", long_input)];
        let out = format_slice_for_summary(&slice);
        let tool_call_line = out
            .lines()
            .find(|l| l.contains("tool_call"))
            .unwrap_or_else(|| panic!("no tool_call line in {out}"));
        assert!(
            tool_call_line.chars().count() < 1200,
            "tool_call line not truncated: {tool_call_line}"
        );
    }

    fn compaction_summary(text: &str) -> Message {
        let body = format!(
            "[atman: compacted 5 messages]\n{text}\n[atman:compact seq_start=1 seq_end=5 count=5]"
        );
        Message::system_text(TurnId::now(), body)
    }

    #[test]
    fn is_compaction_summary_detects_marker() {
        assert!(is_compaction_summary(&compaction_summary("gist")));
        assert!(!is_compaction_summary(&system("plain system msg")));
        assert!(!is_compaction_summary(&user("user msg")));
    }

    #[test]
    fn find_compact_range_spans_across_compaction_summaries() {
        let msgs = vec![
            system("head"),
            user(&"x".repeat(3000)),
            assistant(&"y".repeat(3000)),
            user(&"z".repeat(3000)),
            compaction_summary("first compaction summary"),
            user(&"a".repeat(3000)),
            assistant(&"b".repeat(3000)),
            user(&"c".repeat(3000)),
            assistant(&"d".repeat(3000)),
            user("tail"),
            assistant("tail"),
        ];
        let range = find_compact_range(&msgs, 500).expect("expected range across summary");
        assert!(
            range.start <= 4 && range.end > 4,
            "range should span across the compaction summary at idx 4, got {range:?}"
        );
        assert!(
            range.end - range.start >= 3,
            "range must cover >= 3 msgs, got {}",
            range.end - range.start
        );
    }

    #[test]
    fn find_compact_range_still_treats_plain_system_as_barrier() {
        let msgs = vec![
            user(&"x".repeat(3000)),
            assistant(&"y".repeat(3000)),
            system("plain system break"),
            user(&"a".repeat(3000)),
            assistant(&"b".repeat(3000)),
            user(&"c".repeat(3000)),
            assistant(&"d".repeat(3000)),
            user("tail"),
        ];
        let range = find_compact_range(&msgs, 500).expect("expected range");
        assert!(
            range.start >= 3,
            "must start after the plain system barrier, got {range:?}"
        );
    }
}
