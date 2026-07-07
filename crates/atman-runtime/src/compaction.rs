use crate::message::{Message, MessagePart, MessageRole};

pub fn estimate_tokens_for_message(msg: &Message) -> u64 {
    let mut chars = 0usize;
    for part in &msg.parts {
        chars += match part {
            MessagePart::Text { text } => text.len(),
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
        if matches!(msg.role, MessageRole::System) {
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

pub fn maybe_auto_compact(session: &crate::session::Session, model: &str) {
    let info = crate::model_registry::model_info(model);
    let threshold = info.compact_threshold_tokens();
    let msgs = session.messages();
    let current = estimate_tokens_for_messages(&msgs);
    if current <= threshold {
        return;
    }
    if !session.approval_cooldown_ok_for_compact() {
        return;
    }
    let summary = format!(
        "auto-compacted at {} tokens (budget {}, threshold {})",
        current, info.context_budget, threshold
    );
    match session.compact_messages(summary) {
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
}
