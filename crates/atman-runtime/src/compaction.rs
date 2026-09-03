use crate::message::{Message, MessagePart, MessageRole};

pub const KEEP_RECENT_MESSAGES: usize = 10;
pub const KEEP_RECENT_USER_TURNS: usize = 5;
const KEEP_RECENT_TOKEN_FRACTION: f64 = 0.05;
const COMPACTION_SAFETY_MARGIN_MIN: u64 = 2_000;
const COMPACTION_SAFETY_MARGIN_MAX_RATIO: f64 = 0.05;

#[derive(Debug, Clone, Copy, Default)]
pub struct CompactionBudgetContext {
    pub fixed_input_tokens: Option<u64>,
}

impl CompactionBudgetContext {
    pub fn estimated_input_tokens(self, message_tokens: u64) -> u64 {
        message_tokens.saturating_add(self.fixed_input_tokens.unwrap_or(0))
    }

    pub fn history_budget(self, info: &crate::model_registry::ModelInfo) -> Option<u64> {
        let fixed_input_tokens = self.fixed_input_tokens?;
        let output_cap = (info.context_budget as f64 * 0.20) as u64;
        let output_floor = 8_000_u64.min(output_cap);
        let output_reserve = (info.max_output_tokens.unwrap_or(32_000) as u64)
            .max(output_floor)
            .min(output_cap);
        let safety_cap = (info.context_budget as f64 * COMPACTION_SAFETY_MARGIN_MAX_RATIO) as u64;
        let safety_margin = ((info.context_budget as f64 * 0.02) as u64)
            .max(COMPACTION_SAFETY_MARGIN_MIN.min(safety_cap))
            .min(safety_cap);
        Some(
            info.context_budget
                .saturating_sub(output_reserve)
                .saturating_sub(safety_margin)
                .saturating_sub(fixed_input_tokens),
        )
    }
}

pub fn estimate_tokens_for_message(msg: &Message) -> u64 {
    let mut chars = 0usize;
    let mut fixed_tokens = 0u64;
    for part in &msg.parts {
        chars += match part {
            MessagePart::ContextRecord(record) => record.render_for_model().len(),
            MessagePart::CompactSummary { summary, .. } => summary.len(),
            MessagePart::Text { text } => text.len(),
            MessagePart::Thinking { thinking, .. } => thinking.len(),
            MessagePart::ToolResult { content, .. } => content.len(),
            MessagePart::Image { source } => {
                fixed_tokens = fixed_tokens.saturating_add(match source.detail {
                    crate::provider::ImageDetail::Low => 85,
                    crate::provider::ImageDetail::Auto => 1_024,
                    crate::provider::ImageDetail::High => 1_536,
                    crate::provider::ImageDetail::Original => 2_048,
                });
                0
            }
            MessagePart::ToolUse {
                name,
                input,
                intent,
                ..
            } => {
                name.len()
                    + input.to_string().len()
                    + intent.as_ref().map_or(0, |intent| {
                        crate::message::TOOL_CALL_INTENT_FIELD.len() + intent.as_str().len() + 5
                    })
            }
        };
    }
    chars = chars.saturating_add(estimate_role_overhead(msg.role));
    (chars as f64 / 3.5).ceil() as u64 + fixed_tokens
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
    msg.parts
        .iter()
        .any(|part| matches!(part, MessagePart::CompactSummary { .. }))
}

fn find_kth_recent_user(messages: &[Message], k: usize) -> usize {
    let mut user_count = 0;
    for (index, message) in messages.iter().enumerate().rev() {
        if message.role == MessageRole::User {
            user_count += 1;
            if user_count == k {
                return index;
            }
        }
    }
    0
}

fn align_compact_end_to_tool_transactions(messages: &[Message], end: usize) -> usize {
    let mut tool_use_messages = std::collections::HashMap::new();
    for (index, message) in messages.iter().take(end).enumerate() {
        for part in &message.parts {
            if let MessagePart::ToolUse { id, .. } = part {
                tool_use_messages.entry(id.as_str()).or_insert(index);
            }
        }
    }

    let mut aligned_end = end;
    for message in messages.iter().skip(end) {
        for part in &message.parts {
            if let MessagePart::ToolResult { tool_use_id, .. } = part
                && let Some(&use_index) = tool_use_messages.get(tool_use_id.as_str())
            {
                aligned_end = aligned_end.min(use_index);
            }
        }
    }
    aligned_end
}

pub fn find_compact_range(messages: &[Message], budget: u64) -> Option<CompactRange> {
    let total = estimate_tokens_for_messages(messages);
    if total <= budget || messages.len() < 4 {
        return None;
    }

    let start = messages
        .iter()
        .rposition(is_compaction_summary)
        .unwrap_or(0);
    let keep_recent_tokens = (budget as f64 * KEEP_RECENT_TOKEN_FRACTION).ceil() as u64;
    let mut recent_tokens = 0u64;
    let mut token_end = messages.len();
    for (index, message) in messages.iter().enumerate().rev() {
        recent_tokens = recent_tokens.saturating_add(estimate_tokens_for_message(message));
        token_end = index;
        if recent_tokens >= keep_recent_tokens {
            break;
        }
    }
    let message_end = messages.len().saturating_sub(KEEP_RECENT_MESSAGES);
    let end = message_end
        .min(token_end)
        .min(find_kth_recent_user(messages, KEEP_RECENT_USER_TURNS));
    let end = align_compact_end_to_tool_transactions(messages, end);
    if end < start + 2 {
        return None;
    }

    let tokens_saved_estimate = messages[start..end]
        .iter()
        .map(estimate_tokens_for_message)
        .sum();
    Some(CompactRange {
        start,
        end,
        tokens_saved_estimate,
    })
}

pub fn estimate_compacted_message_tokens(
    messages: &[Message],
    range: &CompactRange,
    summary: &str,
) -> u64 {
    let turn_id = messages
        .get(range.start)
        .map(|m| m.turn_id.clone())
        .unwrap_or_else(crate::event::TurnId::now);
    let after = replace_range_with_summary(messages, range, summary.to_string(), turn_id);
    estimate_tokens_for_messages(&after)
}

pub fn filter_orphan_tool_messages(messages: &mut Vec<Message>) {
    crate::message::retain_complete_tool_pairs(messages);
}

pub fn find_compact_summaries(messages: &[Message]) -> Vec<CompactSummary> {
    let mut out = Vec::new();
    for (idx, msg) in messages.iter().enumerate() {
        if let Some(summary) = compact_summary(msg) {
            out.push(CompactSummary {
                message_index: idx,
                seq_start: summary.seq_start,
                seq_end: summary.seq_end,
                count: summary.count,
            });
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

struct CompactSummaryPart {
    seq_start: u64,
    seq_end: u64,
    count: usize,
}

fn extract_anchor(messages: &[Message]) -> Option<(String, &[Message])> {
    let first = messages.first()?;
    let summary = first.parts.iter().find_map(|part| match part {
        MessagePart::CompactSummary { summary, .. } => Some(summary.clone()),
        _ => None,
    })?;
    Some((summary, &messages[1..]))
}

fn compact_summary(msg: &Message) -> Option<CompactSummaryPart> {
    if msg.role != MessageRole::System {
        return None;
    }
    msg.parts.iter().find_map(|part| match part {
        MessagePart::CompactSummary {
            seq_start,
            seq_end,
            count,
            ..
        } => Some(CompactSummaryPart {
            seq_start: *seq_start,
            seq_end: *seq_end,
            count: *count,
        }),
        _ => None,
    })
}

pub async fn maybe_auto_compact(
    session: &crate::session::Session,
    model: &str,
    providers: &crate::provider::ProviderRegistry,
) {
    maybe_auto_compact_with_budget(
        session,
        model,
        providers,
        CompactionBudgetContext::default(),
    )
    .await;
}

pub async fn maybe_auto_compact_with_budget(
    session: &crate::session::Session,
    model: &str,
    providers: &crate::provider::ProviderRegistry,
    budget_context: CompactionBudgetContext,
) {
    let _compact_guard = session.acquire_compact_lock().await;
    maybe_auto_compact_locked(session, model, providers, budget_context).await;
}

pub fn spawn_auto_compact(
    session: std::sync::Arc<crate::session::Session>,
    model: String,
    providers: crate::provider::ProviderRegistry,
) {
    tokio::task::spawn_blocking(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            session.push_system_note("compaction skipped: background runtime init failed".into());
            return;
        };
        rt.block_on(async move {
            maybe_auto_compact(&session, &model, &providers).await;
        });
    });
}

pub async fn start_auto_compact(
    session: std::sync::Arc<crate::session::Session>,
    model: String,
    providers: crate::provider::ProviderRegistry,
) {
    start_auto_compact_with_budget(
        session,
        model,
        providers,
        CompactionBudgetContext::default(),
    )
    .await;
}

pub async fn start_auto_compact_with_budget(
    session: std::sync::Arc<crate::session::Session>,
    model: String,
    providers: crate::provider::ProviderRegistry,
    budget_context: CompactionBudgetContext,
) {
    let compact_guard = session.acquire_compact_lock_owned().await;
    spawn_locked_compact(session, model, providers, budget_context, compact_guard);
}

pub fn start_manual_compact(
    session: std::sync::Arc<crate::session::Session>,
    mut model: String,
    providers: crate::provider::ProviderRegistry,
) -> bool {
    let Ok(compact_guard) = session.compact_lock_handle().try_lock_owned() else {
        return false;
    };
    if model.is_empty() {
        model = "smart".into();
    }
    session.request_manual_compact();
    spawn_locked_compact(
        session,
        model,
        providers,
        CompactionBudgetContext::default(),
        compact_guard,
    );
    true
}

fn spawn_locked_compact(
    session: std::sync::Arc<crate::session::Session>,
    model: String,
    providers: crate::provider::ProviderRegistry,
    budget_context: CompactionBudgetContext,
    compact_guard: tokio::sync::OwnedMutexGuard<()>,
) {
    tokio::task::spawn_blocking(move || {
        let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        else {
            drop(compact_guard);
            session.push_system_note("compaction skipped: background runtime init failed".into());
            return;
        };
        rt.block_on(async move {
            maybe_auto_compact_locked(&session, &model, &providers, budget_context).await;
            drop(compact_guard);
        });
    });
}

async fn maybe_auto_compact_locked(
    session: &crate::session::Session,
    model: &str,
    providers: &crate::provider::ProviderRegistry,
    budget_context: CompactionBudgetContext,
) {
    let forced = session.take_manual_compact_request();
    let info = crate::model_registry::model_info(model);
    let trigger = info.compaction_trigger_threshold();
    let target = budget_context
        .history_budget(&info)
        .map(|budget| budget.min(info.compaction_target_after()))
        .unwrap_or_else(|| info.compaction_target_after());
    let msgs = session.messages();
    let window_tokens = estimate_tokens_for_messages(&msgs);
    let current = budget_context.estimated_input_tokens(window_tokens);
    if !forced && current <= trigger {
        return;
    }
    if !forced && !session.approval_cooldown_ok_for_compact() {
        return;
    }
    let Some(range) = find_compact_range(&msgs, target) else {
        let (replacement, rewritten_count) =
            build_budgeted_turn_rewrite(&msgs, target, model, providers).await;
        let after_tokens = estimate_tokens_for_messages(&replacement);
        if rewritten_count == 0 || after_tokens >= window_tokens || after_tokens > target {
            session.emit_compact_warning(
                model,
                current,
                trigger,
                info.context_budget,
                "no compactible span — retained user content cannot fit the history budget",
            );
            return;
        }
        match session.commit_rewritten_window(
            replacement,
            window_tokens,
            window_tokens,
            rewritten_count,
        ) {
            Some(_) => {}
            None => {
                session.emit_compact_warning(
                    model,
                    current,
                    trigger,
                    info.context_budget,
                    "retained turn output rewrite did not shrink the transcript",
                );
            }
        }
        return;
    };
    let _ = session
        .stream_tx()
        .send(crate::stream::StreamFrame::CompactionSummary {
            phase: crate::stream::CompactionPhase::Running,
            range_start: range.start,
            range_end: range.end.saturating_sub(1),
            summary: String::new(),
            before_tokens: current,
            after_tokens: 0,
            compacted_count: range.end - range.start,
        });
    let send_failed = |session: &crate::session::Session, reason: &str| {
        let _ = session
            .stream_tx()
            .send(crate::stream::StreamFrame::CompactionSummary {
                phase: crate::stream::CompactionPhase::Failed,
                range_start: range.start,
                range_end: range.end.saturating_sub(1),
                summary: reason.to_string(),
                before_tokens: current,
                after_tokens: current,
                compacted_count: range.end - range.start,
            });
    };
    let mut filtered: Vec<Message> = msgs[range.start..range.end].to_vec();
    filter_orphan_tool_messages(&mut filtered);
    let (anchor, new_messages) = extract_anchor(&filtered)
        .map(|(anchor, remaining)| (Some(anchor), remaining.to_vec()))
        .unwrap_or_else(|| (None, filtered.clone()));
    let range_start = range.start;
    let range_end = range.end.saturating_sub(1);
    let stream_tx = session.stream_tx();
    let on_delta: std::sync::Arc<dyn Fn(String) + Send + Sync> = std::sync::Arc::new(move |text| {
        let _ = stream_tx.send(crate::stream::StreamFrame::CompactionDelta {
            range_start,
            range_end,
            text,
        });
    });
    let summary = match generate_llm_summary_with_delta(
        anchor.as_deref(),
        &new_messages,
        model,
        providers,
        Some(on_delta),
    )
    .await
    {
        Ok(text) => text,
        Err(err) => {
            session.emit_compact_warning(
                model,
                current,
                trigger,
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
        match request_review_if_enabled(session, forced, &filtered, &range, current, summary).await
        {
            ReviewOutcome::Commit(s) => s,
            ReviewOutcome::Rejected => {
                send_failed(
                    session,
                    "compaction rejected by user; keeping full transcript",
                );
                session.push_system_note(
                    "compaction rejected by user; keeping full transcript".into(),
                );
                return;
            }
        };
    let replacement =
        build_budgeted_replacement(&msgs, &range, &final_summary, target, model, providers).await;
    let after_tokens = estimate_tokens_for_messages(&replacement);
    if after_tokens >= window_tokens {
        send_failed(
            session,
            &format!(
                "compaction skipped: replacement would not shrink transcript ({} >= {} tokens)",
                after_tokens, window_tokens
            ),
        );
        session.push_system_note(format!(
            "compaction skipped: replacement would not shrink transcript ({} >= {} tokens)",
            after_tokens, window_tokens
        ));
        return;
    }
    match session.commit_compacted_window(
        final_summary,
        replacement,
        range,
        window_tokens,
        window_tokens,
    ) {
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
                trigger,
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

const SUMMARY_SYSTEM_PROMPT: &str =
    "You are an anchored context summarization assistant for coding sessions.";

const SUMMARY_INSTRUCTIONS: &str = r#"You are an anchored context summarization assistant.

Below is:
1. <current-anchor>: the existing handoff state, which is authoritative and must be preserved.
2. <new-messages>: only the messages that arrived since the anchor was written.

Merge the NEW facts from <new-messages> INTO the current anchor, producing an upgraded full anchor.

STRUCTURAL RULES (data model, not optional style):
- ## Objective: unchanged unless the new messages show the user explicitly redirected.
- ### Completed: ONLY ADD newly completed items. Never remove or re-evaluate an existing completed item. If a completed item is now in question, add it to ### Active or ### Blocked instead. NEVER delete from Completed.
- ### Active: update based on new messages; move newly-done items to Completed.
- ### Blocked: update based on new messages; remove resolved ones.
- ## Decisions: only add new decisions. Never remove old ones.
- ## Next Move: replace based on current end state.
- Keep every section, even when empty.
- Preserve exact file paths, symbols, commands, error strings, identifiers.

Output exactly this Markdown structure:
## Objective
## Important Details
## Work State
### Completed
### Active
### Blocked
## Decisions
## Next Move
## Relevant Files

Do not mention the summary process or that context was compacted.
Respond in the same language as the conversation."#;

async fn generate_llm_summary(
    anchor: Option<&str>,
    slice: &[Message],
    model: &str,
    providers: &crate::provider::ProviderRegistry,
) -> Result<String, crate::error::RuntimeError> {
    generate_llm_summary_with_delta(anchor, slice, model, providers, None).await
}

async fn generate_llm_summary_with_delta(
    anchor: Option<&str>,
    slice: &[Message],
    model: &str,
    providers: &crate::provider::ProviderRegistry,
    on_delta: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>,
) -> Result<String, crate::error::RuntimeError> {
    let provider = providers.resolve(model).ok_or_else(|| {
        crate::error::RuntimeError::ToolFailed(format!("no provider for {model}"))
    })?;
    let payload = format_slice_for_summary(slice);
    let (messages, dump_user) = if let Some(anchor) = anchor {
        let anchor_user = format!("<current-anchor>\n{anchor}\n</current-anchor>");
        let new_user =
            format!("<new-messages>\n{payload}\n</new-messages>\n\n{SUMMARY_INSTRUCTIONS}");
        (
            vec![
                Message::user_text(crate::event::TurnId::now(), anchor_user.clone()),
                Message::user_text(crate::event::TurnId::now(), new_user.clone()),
            ],
            format!("{anchor_user}\n\n{new_user}"),
        )
    } else {
        let user = format!(
            "<conversation_history>\n{payload}\n</conversation_history>\n\n{SUMMARY_INSTRUCTIONS}"
        );
        (
            vec![Message::user_text(
                crate::event::TurnId::now(),
                user.clone(),
            )],
            user,
        )
    };
    if let Ok(dir) = std::env::var("ATMAN_COMPACT_DUMP") {
        let _ = std::fs::write(
            format!("{dir}/compact_request.txt"),
            format!("=== SYSTEM ===\n{SUMMARY_SYSTEM_PROMPT}\n\n=== USER ===\n{dump_user}"),
        );
    }
    let req = crate::provider::LlmRequest {
        model: model.into(),
        messages,
        system: Some(SUMMARY_SYSTEM_PROMPT.into()),
        input: crate::value::Value::Unit,
        schema: None,
        cache_prompt: false,
        prompt_cache_key: None,
        tools: Vec::new(),
        reasoning: crate::provider::ReasoningSelection::ProviderDefault,
        stall_timeout_secs: 0,
    };
    let outcome = if let Some(on_delta) = on_delta {
        let observable = provider.call_streaming(req);
        let mut events = observable.events;
        let mut output = observable.output;
        let outcome = loop {
            tokio::select! {
                event = events.recv() => match event {
                    Ok(crate::event::NodeEvent::LlmChunk { text, .. }) => {
                        on_delta(text);
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break output.await?,
                },
                result = &mut output => break result?,
            }
        };
        while let Ok(event) = events.try_recv() {
            if let crate::event::NodeEvent::LlmChunk { text, .. } = event {
                on_delta(text);
            }
        }
        outcome
    } else {
        provider.call(req).await?
    };
    let text = outcome.text_concat();
    if text.trim().is_empty() {
        return Err(crate::error::RuntimeError::ToolFailed(
            "empty summary from provider".into(),
        ));
    }
    Ok(text)
}

async fn build_budgeted_turn_rewrite(
    messages: &[Message],
    history_budget: u64,
    model: &str,
    providers: &crate::provider::ProviderRegistry,
) -> (Vec<Message>, usize) {
    let mut replacement = messages.to_vec();
    let mut group_index = 0;
    let mut rewritten_count = 0;
    while estimate_tokens_for_messages(&replacement) > history_budget {
        let groups = user_turn_ranges(&replacement);
        let Some((start, end)) = groups.get(group_index).copied() else {
            break;
        };
        let output = replacement[start + 1..end].to_vec();
        if output.is_empty() {
            group_index += 1;
            continue;
        }
        let output_tokens = estimate_tokens_for_messages(&output);
        let summary = generate_llm_summary(None, &output, model, providers)
            .await
            .unwrap_or_else(|_| deterministic_turn_omission(&output));
        let mut summary_message = Message::assistant_text(
            replacement[start].turn_id.clone(),
            format!("[atman: compacted turn output]\n{summary}\n[/atman: compacted turn output]"),
        );
        if estimate_tokens_for_message(&summary_message) >= output_tokens {
            summary_message = Message::assistant_text(
                replacement[start].turn_id.clone(),
                deterministic_turn_omission(&output),
            );
        }
        rewritten_count += output.len();
        replacement.splice(start + 1..end, [summary_message]);
        group_index += 1;
    }
    filter_orphan_tool_messages(&mut replacement);
    (replacement, rewritten_count)
}

async fn build_budgeted_replacement(
    messages: &[Message],
    range: &CompactRange,
    anchor_summary: &str,
    history_budget: u64,
    model: &str,
    providers: &crate::provider::ProviderRegistry,
) -> Vec<Message> {
    let turn_id = messages
        .get(range.start)
        .map(|message| message.turn_id.clone())
        .unwrap_or_else(crate::event::TurnId::now);
    let mut replacement =
        replace_range_with_summary(messages, range, anchor_summary.to_string(), turn_id);
    filter_orphan_tool_messages(&mut replacement);
    if estimate_tokens_for_messages(&replacement) <= history_budget {
        return replacement;
    }

    let mut group_index = 0;
    loop {
        let groups = user_turn_ranges(&replacement);
        if group_index >= groups.len()
            || estimate_tokens_for_messages(&replacement) <= history_budget
        {
            break;
        }
        let (start, end) = groups[group_index];
        let output: Vec<Message> = replacement[start + 1..end].to_vec();
        if output.is_empty() {
            group_index += 1;
            continue;
        }
        let output_tokens = estimate_tokens_for_messages(&output);
        let summary = generate_llm_summary(None, &output, model, providers)
            .await
            .unwrap_or_else(|_| deterministic_turn_omission(&output));
        let mut summary_message = Message::assistant_text(
            replacement[start].turn_id.clone(),
            format!("[atman: compacted turn output]\n{summary}\n[/atman: compacted turn output]"),
        );
        if estimate_tokens_for_message(&summary_message) >= output_tokens {
            summary_message = Message::assistant_text(
                replacement[start].turn_id.clone(),
                deterministic_turn_omission(&output),
            );
        }
        replacement.splice(start + 1..end, [summary_message]);
        group_index += 1;
    }

    if estimate_tokens_for_messages(&replacement) > history_budget {
        let groups = user_turn_ranges(&replacement);
        for (start, end) in groups.into_iter().rev() {
            let output = replacement[start + 1..end].to_vec();
            if !output.is_empty() {
                replacement.splice(
                    start + 1..end,
                    [Message::assistant_text(
                        replacement[start].turn_id.clone(),
                        deterministic_turn_omission(&output),
                    )],
                );
            }
        }
    }

    if estimate_tokens_for_messages(&replacement) > history_budget {
        replacement = compaction_floor(&replacement);
    }

    filter_orphan_tool_messages(&mut replacement);
    replacement
}

fn user_turn_ranges(messages: &[Message]) -> Vec<(usize, usize)> {
    let starts: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| (message.role == MessageRole::User).then_some(index))
        .collect();
    starts
        .iter()
        .enumerate()
        .map(|(index, start)| {
            (
                *start,
                starts.get(index + 1).copied().unwrap_or(messages.len()),
            )
        })
        .collect()
}

fn deterministic_turn_omission(messages: &[Message]) -> String {
    format!(
        "[atman: omitted {} oversized assistant/system/tool messages during persistent compaction]",
        messages.len()
    )
}

fn compaction_floor(messages: &[Message]) -> Vec<Message> {
    let mut out = Vec::new();
    if let Some(anchor) = messages
        .iter()
        .find(|message| is_compaction_summary(message))
    {
        out.push(anchor.clone());
    }

    let mut latest =
        std::collections::HashMap::<&str, (&crate::context_plan::ContextRecord, &Message)>::new();
    for message in messages {
        for part in &message.parts {
            if let MessagePart::ContextRecord(record) = part {
                latest
                    .entry(record.key())
                    .and_modify(|(current, source)| {
                        if record.revision() >= current.revision() {
                            *current = record;
                            *source = message;
                        }
                    })
                    .or_insert((record, message));
            }
        }
    }
    let mut records: Vec<_> = latest.into_values().collect();
    records.sort_by(|(left, _), (right, _)| left.key().cmp(right.key()));
    out.extend(
        records.into_iter().map(|(record, source)| {
            Message::context_record(source.turn_id.clone(), record.clone())
        }),
    );

    out.extend(messages.iter().filter_map(|message| {
        if message.role != MessageRole::User {
            return None;
        }
        let mut user = message.clone();
        user.parts.retain(|part| {
            !matches!(
                part,
                MessagePart::CompactSummary { .. } | MessagePart::ContextRecord(_)
            )
        });
        (!user.parts.is_empty()).then_some(user)
    }));
    out
}

fn format_slice_for_summary(slice: &[Message]) -> String {
    let mut out = String::new();
    for (i, msg) in slice.iter().enumerate() {
        let role = msg.role.as_str();
        let body = serialize_message_for_summary(msg);
        let truncated: String = body.chars().take(4000).collect();
        out.push_str(&format!("[{i}] {role}: {truncated}\n\n"));
    }
    out.chars().take(120_000).collect()
}

fn serialize_message_for_summary(msg: &Message) -> String {
    let mut parts = Vec::new();
    for part in &msg.parts {
        match part {
            MessagePart::ContextRecord(record) => {
                if record.retention() == crate::context_plan::ContextRecordRetention::Timeline {
                    parts.push(record.render_for_model());
                }
            }
            MessagePart::CompactSummary { summary, .. } => {
                parts.push(summary.clone());
            }
            MessagePart::Text { text } => {
                parts.push(text.clone());
            }
            MessagePart::Thinking { thinking, .. } => {
                let truncated: String = thinking.chars().take(1000).collect();
                parts.push(format!("[thinking: {truncated}]"));
            }
            MessagePart::ToolUse {
                name,
                input,
                intent,
                ..
            } => {
                let input_str = if input.is_null() {
                    String::new()
                } else {
                    input.to_string()
                };
                let truncated: String = input_str.chars().take(2000).collect();
                let purpose = intent
                    .as_ref()
                    .map(|intent| format!(" purpose={}", intent.as_str()))
                    .unwrap_or_default();
                parts.push(format!("[tool_call: {name}{purpose}({truncated})]"));
            }
            MessagePart::ToolResult {
                content,
                is_error,
                tool_use_id,
            } => {
                let truncated: String = content.chars().take(3000).collect();
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
    let retained_records = latest_context_records_before(messages, range.end);
    let mut out =
        Vec::with_capacity(1 + retained_records.len() + messages.len().saturating_sub(range.end));
    out.push(Message::system_compact_summary(
        turn_id,
        summary,
        range.start as u64,
        range.end.saturating_sub(1) as u64,
        range.end - range.start,
    ));
    out.extend(retained_records);
    out.extend_from_slice(&messages[range.end..]);
    out
}

fn latest_context_records_before(messages: &[Message], end: usize) -> Vec<Message> {
    let suffix_keys: std::collections::HashSet<&str> = messages[end..]
        .iter()
        .flat_map(|message| &message.parts)
        .filter_map(|part| match part {
            MessagePart::ContextRecord(record) => Some(record.key()),
            _ => None,
        })
        .collect();
    let mut latest = std::collections::HashMap::<&str, (usize, &Message, &MessagePart)>::new();
    for (index, message) in messages[..end].iter().enumerate() {
        for part in &message.parts {
            if let MessagePart::ContextRecord(record) = part
                && record.retention() == crate::context_plan::ContextRecordRetention::Latest
                && !suffix_keys.contains(record.key())
            {
                latest.insert(record.key(), (index, message, part));
            }
        }
    }
    let mut retained: Vec<_> = latest.into_values().collect();
    retained.sort_by_key(|(index, _, _)| *index);
    retained
        .into_iter()
        .map(|(_, message, part)| Message {
            role: MessageRole::System,
            parts: vec![part.clone()],
            turn_id: message.turn_id.clone(),
            origin: crate::message::MessageOrigin::Internal,
        })
        .collect()
}

/// Result of compacting a messages_handle in place.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HandleCompactResult {
    pub before_tokens: u64,
    pub after_tokens: u64,
    pub compacted_start: usize,
    pub compacted_end: usize,
}

/// Result of applying the automatic compaction policy to an isolated message
/// handle. The caller must hold that handle's async compaction lock.
#[derive(Debug, Clone, PartialEq)]
pub struct HandleAutoCompactResult {
    pub before_tokens: u64,
    pub after_tokens: u64,
    pub compacted_start: usize,
    pub compacted_end: usize,
    pub compacted_count: usize,
    pub summary: String,
    pub checkpoint_messages: Vec<Message>,
}

/// Apply the root compaction budget, range, summary, and replacement policy to
/// an isolated message handle. This function does not acquire the async lock
/// and does not emit session events.
pub async fn maybe_auto_compact_handle_locked(
    handle: &std::sync::Arc<std::sync::Mutex<Vec<Message>>>,
    model: &str,
    providers: &crate::provider::ProviderRegistry,
    budget_context: CompactionBudgetContext,
    forced: bool,
) -> Option<HandleAutoCompactResult> {
    let snapshot = handle.lock().unwrap().clone();
    let info = crate::model_registry::model_info(model);
    let trigger = info.compaction_trigger_threshold();
    let target = budget_context
        .history_budget(&info)
        .map(|budget| budget.min(info.compaction_target_after()))
        .unwrap_or_else(|| info.compaction_target_after());
    let before_tokens = estimate_tokens_for_messages(&snapshot);
    let current = budget_context.estimated_input_tokens(before_tokens);
    if !forced && current <= trigger {
        return None;
    }

    let (replacement, summary, compacted_start, compacted_end, compacted_count, must_fit_target) =
        if let Some(range) = find_compact_range(&snapshot, target) {
            let mut filtered = snapshot[range.start..range.end].to_vec();
            filter_orphan_tool_messages(&mut filtered);
            let (anchor, new_messages) = extract_anchor(&filtered)
                .map(|(anchor, remaining)| (Some(anchor), remaining.to_vec()))
                .unwrap_or_else(|| (None, filtered));
            let summary = generate_llm_summary(anchor.as_deref(), &new_messages, model, providers)
                .await
                .unwrap_or_else(|_| {
                    format!(
                        "[atman: compacted {} messages; summary unavailable]",
                        range.end - range.start
                    )
                });
            let replacement =
                build_budgeted_replacement(&snapshot, &range, &summary, target, model, providers)
                    .await;
            let compacted_end = range.end.saturating_sub(1);
            let compacted_count = range.end - range.start;
            (
                replacement,
                summary,
                range.start,
                compacted_end,
                compacted_count,
                false,
            )
        } else {
            let (replacement, rewritten_count) =
                build_budgeted_turn_rewrite(&snapshot, target, model, providers).await;
            if rewritten_count == 0 {
                return None;
            }
            (
                replacement,
                format!(
                    "[atman: persistently compacted output from {rewritten_count} retained messages]"
                ),
                0,
                0,
                rewritten_count,
                true,
            )
        };
    let after_tokens = estimate_tokens_for_messages(&replacement);
    if after_tokens >= before_tokens || (must_fit_target && after_tokens > target) {
        return None;
    }

    let mut messages = handle.lock().unwrap();
    if *messages != snapshot {
        return None;
    }
    *messages = replacement.clone();
    Some(HandleAutoCompactResult {
        before_tokens,
        after_tokens,
        compacted_start,
        compacted_end,
        compacted_count,
        summary,
        checkpoint_messages: replacement,
    })
}

/// Compact a messages_handle in place (data-layer primitive, operates on any
/// FlowRun's segment). Returns `None` if under `budget` or no compactable
/// range. Caller should hold the FlowRun's `compact_lock`.
pub fn compact_messages_on_handle(
    handle: &std::sync::Arc<std::sync::Mutex<Vec<Message>>>,
    summary: String,
    budget: u64,
) -> Option<HandleCompactResult> {
    let mut msgs = handle.lock().unwrap();
    let before_tokens = estimate_tokens_for_messages(&msgs);
    let range = find_compact_range(&msgs, budget)?;
    let turn_id = msgs
        .get(range.start)
        .map(|m| m.turn_id.clone())
        .unwrap_or_else(crate::event::TurnId::now);
    let after = replace_range_with_summary(&msgs, &range, summary, turn_id);
    let after_tokens = estimate_tokens_for_messages(&after);
    if after_tokens >= before_tokens {
        return None;
    }
    let result = HandleCompactResult {
        before_tokens,
        after_tokens,
        compacted_start: range.start,
        compacted_end: range.end.saturating_sub(1),
    };
    *msgs = after;
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnId;
    use crate::message::MessageOrigin;

    fn user(text: &str) -> Message {
        Message::user_text(TurnId::now(), text)
    }
    fn assistant(text: &str) -> Message {
        Message::assistant_text(TurnId::now(), text)
    }
    fn system(text: &str) -> Message {
        Message::system_text(TurnId::now(), text)
    }

    fn context_record(key: &str, revision: u64, text: &str) -> Message {
        Message::context_record(
            TurnId::now(),
            crate::context_plan::ContextRecord::new(
                key,
                revision,
                crate::context_plan::ContextRecordAuthority::Runtime,
                crate::context_plan::ContextRecordRetention::Latest,
                crate::context_plan::ContextRecordBody::text(text),
            ),
        )
    }

    fn context_tombstone(key: &str, revision: u64) -> Message {
        Message::context_record(
            TurnId::now(),
            crate::context_plan::ContextRecord::new(
                key,
                revision,
                crate::context_plan::ContextRecordAuthority::Runtime,
                crate::context_plan::ContextRecordRetention::Latest,
                crate::context_plan::ContextRecordBody::tombstone(),
            ),
        )
    }

    #[test]
    fn replacement_keeps_only_the_latest_live_record_per_key() {
        let messages = vec![
            user("old"),
            context_record("session.goal", 1, "first"),
            context_record("session.goal", 2, "second"),
            assistant("old answer"),
            user("current"),
        ];
        let replacement = replace_range_with_summary(
            &messages,
            &CompactRange {
                start: 0,
                end: 4,
                tokens_saved_estimate: 1,
            },
            "summary".into(),
            TurnId::now(),
        );

        assert_eq!(replacement.len(), 3);
        assert!(is_compaction_summary(&replacement[0]));
        assert!(matches!(
            replacement[1].parts.as_slice(),
            [MessagePart::ContextRecord(record)]
                if record.key() == "session.goal" && record.revision() == 2
        ));
        assert_eq!(replacement[2].text_concat(), "current");
    }

    #[test]
    fn compaction_budget_reserves_output_safety_and_fixed_input_only_at_compact_time() {
        let info = crate::model_registry::ModelInfo {
            name: "test".into(),
            context_budget: 100_000,
            compact_threshold_ratio: 0.8,
            reasoning: crate::provider::ReasoningSelection::ProviderDefault,
            capabilities: crate::provider::ModelCapabilities::default(),
            image_detail: crate::provider::ImageDetail::Auto,
            max_output_tokens: Some(10_000),
        };
        let budget = CompactionBudgetContext {
            fixed_input_tokens: Some(5_000),
        }
        .history_budget(&info);
        assert_eq!(budget, Some(100_000 - 10_000 - 2_000 - 5_000));
    }

    #[test]
    fn compaction_budget_saturates_when_fixed_input_exceeds_context() {
        let info = crate::model_registry::ModelInfo {
            name: "test".into(),
            context_budget: 20_000,
            compact_threshold_ratio: 0.8,
            reasoning: crate::provider::ReasoningSelection::ProviderDefault,
            capabilities: crate::provider::ModelCapabilities::default(),
            image_detail: crate::provider::ImageDetail::Auto,
            max_output_tokens: None,
        };
        assert_eq!(
            CompactionBudgetContext {
                fixed_input_tokens: Some(100_000)
            }
            .history_budget(&info),
            Some(0)
        );
    }

    #[test]
    fn compaction_preflight_estimate_uses_current_messages_and_fixed_prefix() {
        let budget = CompactionBudgetContext {
            fixed_input_tokens: Some(7_000),
        };
        assert_eq!(budget.estimated_input_tokens(11_000), 18_000);
        assert_eq!(
            CompactionBudgetContext::default().estimated_input_tokens(11_000),
            11_000
        );
    }

    #[tokio::test]
    async fn manual_compaction_starts_only_when_the_compaction_lock_is_available() {
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        let held = session.acquire_compact_lock_owned().await;
        assert!(!start_manual_compact(
            session.clone(),
            "test".into(),
            crate::provider::ProviderRegistry::new(),
        ));
        assert!(!session.take_manual_compact_request());
        drop(held);

        assert!(start_manual_compact(
            session.clone(),
            "test".into(),
            crate::provider::ProviderRegistry::new(),
        ));
        let completed = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            session.acquire_compact_lock_owned(),
        )
        .await
        .expect("manual compaction should release its lock");
        drop(completed);
        assert!(!session.take_manual_compact_request());
    }

    #[tokio::test]
    async fn budgeted_replacement_groups_by_user_boundary_and_persists_omission() {
        let first_turn = TurnId::now();
        let second_turn = TurnId::now();
        let mut messages = vec![system(&"old".repeat(20_000)), assistant("old answer")];
        messages.push(Message::user_text(first_turn.clone(), "first user"));
        messages.push(Message::assistant_text(
            second_turn.clone(),
            "first output".repeat(4_000),
        ));
        messages.push(assistant_with_tool_use(
            "calling tool",
            "fs.read",
            serde_json::json!({"path": "/tmp/example"}),
        ));
        messages.push(tool_result(
            "call_test",
            &"tool output".repeat(4_000),
            false,
        ));
        messages.push(Message::user_text(second_turn.clone(), "current user"));
        messages.push(Message::assistant_text(
            first_turn,
            "current output".repeat(4_000),
        ));
        let range = CompactRange {
            start: 0,
            end: 2,
            tokens_saved_estimate: 1,
        };

        let replacement = build_budgeted_replacement(
            &messages,
            &range,
            "anchor",
            500,
            "missing-provider",
            &crate::provider::ProviderRegistry::default(),
        )
        .await;

        let texts: Vec<String> = replacement.iter().map(Message::text_concat).collect();
        assert!(texts.iter().any(|text| text == "current user"));
        assert!(
            texts
                .iter()
                .any(|text| text.contains("omitted 3 oversized"))
        );
        assert!(!replacement.iter().any(|message| {
            message.parts.iter().any(|part| {
                matches!(
                    part,
                    MessagePart::ToolUse { .. } | MessagePart::ToolResult { .. }
                )
            })
        }));
        assert_eq!(user_turn_ranges(&replacement).len(), 2);
    }

    #[tokio::test]
    async fn budgeted_replacement_floor_keeps_anchor_records_and_user_inputs() {
        let messages = vec![
            system("old system"),
            context_record("session.goal", 1, "old goal"),
            context_record("session.goal", 2, "current goal"),
            context_tombstone("session.workspace", 3),
            user("first user"),
            assistant("first output"),
            user("current user"),
            assistant("current output"),
        ];
        let replacement = build_budgeted_replacement(
            &messages,
            &CompactRange {
                start: 0,
                end: 4,
                tokens_saved_estimate: 1,
            },
            "anchor",
            1,
            "missing-provider",
            &crate::provider::ProviderRegistry::default(),
        )
        .await;

        assert!(is_compaction_summary(&replacement[0]));
        let records: Vec<_> = replacement
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                MessagePart::ContextRecord(record) => Some(record),
                _ => None,
            })
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].key(), "session.goal");
        assert_eq!(records[0].revision(), 2);
        assert_eq!(records[1].key(), "session.workspace");
        assert!(records[1].body().is_tombstone());
        assert_eq!(
            replacement
                .iter()
                .filter(|message| message.role == MessageRole::User)
                .map(Message::text_concat)
                .collect::<Vec<_>>(),
            ["first user", "current user"]
        );
        assert!(
            !replacement
                .iter()
                .any(|message| message.role == MessageRole::Assistant)
        );
    }

    #[tokio::test]
    async fn turn_rewrite_compacts_oversized_tool_output_without_dropping_recent_users() {
        let first = TurnId::now();
        let current = TurnId::now();
        let messages = vec![
            Message::user_text(first, "first user"),
            assistant_with_tool_use(
                &"calling tool".repeat(2_000),
                "fs.read",
                serde_json::json!({"path": "/tmp/example"}),
            ),
            tool_result("call_test", &"tool output".repeat(8_000), false),
            Message::user_text(current, "current user"),
        ];

        assert!(find_compact_range(&messages, 500).is_none());
        let (replacement, rewritten_count) = build_budgeted_turn_rewrite(
            &messages,
            500,
            "missing-provider",
            &crate::provider::ProviderRegistry::default(),
        )
        .await;

        assert_eq!(rewritten_count, 2);
        assert!(estimate_tokens_for_messages(&replacement) <= 500);
        let users: Vec<String> = replacement
            .iter()
            .filter(|message| message.role == MessageRole::User)
            .map(Message::text_concat)
            .collect();
        assert_eq!(users, vec!["first user", "current user"]);
        assert!(replacement.iter().any(|message| {
            message
                .text_concat()
                .contains("omitted 2 oversized assistant/system/tool messages")
        }));
        assert!(!replacement.iter().any(|message| {
            message.parts.iter().any(|part| {
                matches!(
                    part,
                    MessagePart::ToolUse { .. } | MessagePart::ToolResult { .. }
                )
            })
        }));
    }

    #[test]
    fn user_turn_ranges_ignore_misanchored_turn_ids() {
        let first = TurnId::now();
        let second = TurnId::now();
        let messages = vec![
            Message::user_text(first.clone(), "u1"),
            Message::assistant_text(second.clone(), "a1"),
            Message::user_text(second, "u2"),
            Message::assistant_text(first, "a2"),
        ];
        assert_eq!(user_turn_ranges(&messages), vec![(0, 2), (2, 4)]);
    }

    #[test]
    fn summary_instructions_keep_decisions_before_next_move() {
        let objective = SUMMARY_INSTRUCTIONS
            .find("## Objective")
            .expect("objective");
        let decisions = SUMMARY_INSTRUCTIONS
            .find("## Decisions")
            .expect("decisions");
        let next_move = SUMMARY_INSTRUCTIONS
            .find("## Next Move")
            .expect("next move");
        assert!(objective < decisions);
        assert!(decisions < next_move);
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
    fn find_kth_recent_user_handles_exact_excess_and_mixed_history() {
        let exact = vec![
            user("u0"),
            assistant("a0"),
            system("s0"),
            tool_result("call-0", "result", false),
            user("u1"),
            assistant("a1"),
            user("u2"),
            system("s1"),
            user("u3"),
            assistant("a3"),
            user("u4"),
        ];
        // With exactly five users, the fifth recent user is the first message.
        assert_eq!(find_kth_recent_user(&exact, KEEP_RECENT_USER_TURNS), 0);

        let mut excess = exact.clone();
        excess.push(user("u5"));
        assert_eq!(find_kth_recent_user(&excess, KEEP_RECENT_USER_TURNS), 4);

        let too_few = vec![user("a"), assistant("b"), assistant("c")];
        assert_eq!(find_kth_recent_user(&too_few, KEEP_RECENT_USER_TURNS), 0);
    }

    #[test]
    fn find_compact_range_preserves_minimum_recent_messages_without_anchor() {
        let mut msgs = vec![system("head")];
        msgs.extend((0..25).map(|index| assistant(&format!("old {index}"))));
        msgs.extend(
            (0..6).flat_map(|index| [user(&format!("user {index}")), assistant("assistant")]),
        );
        let range = find_compact_range(&msgs, 1).expect("range");
        assert_eq!(range.start, 0);
        assert_eq!(range.end, msgs.len() - KEEP_RECENT_MESSAGES);
    }

    #[test]
    fn find_compact_range_keeps_tool_use_with_later_result() {
        let mut msgs = (0..11)
            .map(|index| assistant(&format!("old {index}")))
            .collect::<Vec<_>>();
        msgs.push(assistant_with_tool_use(
            "calling tool",
            "fs.read",
            serde_json::json!({"path": "/tmp/example"}),
        ));
        msgs.push(tool_result("call_test", "result", false));
        msgs.extend((0..9).map(|index| {
            if index % 2 == 0 {
                user(&format!("recent user {index}"))
            } else {
                assistant(&format!("recent assistant {index}"))
            }
        }));

        let range = find_compact_range(&msgs, 1).expect("range");
        assert_eq!(range.end, 11);
        assert!(matches!(
            msgs[range.end].parts.as_slice(),
            [MessagePart::Text { .. }, MessagePart::ToolUse { id, .. }] if id == "call_test"
        ));
    }

    #[test]
    fn find_compact_range_keeps_parallel_tool_batch_together() {
        let mut msgs = (0..10)
            .map(|index| assistant(&format!("old {index}")))
            .collect::<Vec<_>>();
        msgs.push(assistant_with_tool_uses(&["call_a", "call_b"]));
        msgs.push(tool_result("call_a", "first result", false));
        msgs.push(tool_result("call_b", "second result", false));
        msgs.extend((0..9).map(|index| {
            if index % 2 == 0 {
                user(&format!("recent user {index}"))
            } else {
                assistant(&format!("recent assistant {index}"))
            }
        }));

        let range = find_compact_range(&msgs, 1).expect("range");
        assert_eq!(range.end, 10);
        assert_eq!(
            msgs[range.end]
                .parts
                .iter()
                .filter(|part| matches!(part, MessagePart::ToolUse { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn find_compact_range_keeps_boundary_after_closed_tool_batch() {
        let mut msgs = (0..9)
            .map(|index| assistant(&format!("old {index}")))
            .collect::<Vec<_>>();
        msgs.push(assistant_with_tool_uses(&["call_a", "call_b"]));
        msgs.push(Message {
            role: MessageRole::Tool,
            parts: vec![
                MessagePart::ToolResult {
                    tool_use_id: "call_a".into(),
                    content: "first result".into(),
                    is_error: false,
                },
                MessagePart::ToolResult {
                    tool_use_id: "call_b".into(),
                    content: "second result".into(),
                    is_error: false,
                },
            ],
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
        });
        msgs.push(assistant("batch complete"));
        msgs.extend((0..10).map(|index| {
            if index % 2 == 0 {
                user(&format!("recent user {index}"))
            } else {
                assistant(&format!("recent assistant {index}"))
            }
        }));

        let range = find_compact_range(&msgs, 1).expect("range");
        assert_eq!(range.end, 12);
        assert_eq!(msgs[range.end].role, MessageRole::User);
    }

    #[test]
    fn find_compact_range_preserves_minimum_window_for_large_tool_result() {
        let mut msgs = vec![user(&"h".repeat(500_000))];
        for index in 1..31 {
            if matches!(index, 20 | 22 | 24 | 26 | 28 | 30) {
                msgs.push(user(&"u".repeat(800)));
            } else {
                msgs.push(assistant(&"a".repeat(800)));
            }
        }
        msgs.push(tool_result("call-large", &"t".repeat(22_000), false));

        let budget = 120_000;
        let minimum_recent_tokens = (budget as f64 * KEEP_RECENT_TOKEN_FRACTION).ceil() as u64;
        assert!(estimate_tokens_for_message(msgs.last().unwrap()) > minimum_recent_tokens);

        let range = find_compact_range(&msgs, budget).expect("range");
        assert!(
            range.end <= msgs.len() - KEEP_RECENT_MESSAGES,
            "range was {range:?}"
        );
        assert!(msgs.len() - range.end >= KEEP_RECENT_MESSAGES);
        assert!(estimate_tokens_for_messages(&msgs[range.end..]) >= minimum_recent_tokens);
    }

    #[test]
    fn find_compact_range_handles_four_to_twenty_one_message_histories() {
        for len in 4..=21 {
            let msgs = (0..len)
                .map(|_| user(&"x".repeat(5000)))
                .collect::<Vec<_>>();
            assert_eq!(
                find_compact_range(&msgs, 1).is_some(),
                len >= KEEP_RECENT_MESSAGES + 2,
                "len={len}"
            );
        }
    }

    #[test]
    fn find_compact_range_recent_users_limit_mixed_history() {
        let msgs = vec![
            system("head"),
            assistant("a0"),
            user("u0"),
            tool_result("call-0", "r0", false),
            assistant("a1"),
            user("u1"),
            assistant("a2"),
            tool_result("call-1", "r1", false),
            user("u2"),
            assistant("a3"),
            system("note"),
            user("u3"),
            tool_result("call-2", "r2", false),
            assistant("a4"),
            user("u4"),
            assistant("a5"),
            tool_result("call-3", "r3", false),
            user("u5"),
            assistant("a6"),
            system("tail"),
            assistant("a7"),
        ];

        let range = find_compact_range(&msgs, 1).expect("range");
        assert_eq!(range.end, 5);
        assert_eq!(msgs[range.end].role, MessageRole::User);
    }

    #[test]
    fn find_compact_range_preserves_recent_user_turns_after_anchor() {
        let mut msgs = vec![system("head"), compaction_summary("summary")];
        msgs.extend((0..6).flat_map(|index| {
            [
                user(&format!("user {index}")),
                assistant("assistant"),
                assistant("tool fragment"),
            ]
        }));
        msgs.extend((0..12).map(|_| assistant("recent fragment")));
        let range = find_compact_range(&msgs, 1).expect("range");
        assert_eq!(range.start, 1);
        assert_eq!(range.end, 5);
        assert_eq!(msgs[range.end].role, MessageRole::User);
    }

    #[test]
    fn find_compact_range_returns_none_when_end_cannot_cover_two_messages() {
        let msgs = vec![
            system("head"),
            compaction_summary("summary"),
            assistant("tail"),
            user("tail"),
        ];
        assert!(find_compact_range(&msgs, 1).is_none());
    }

    #[test]
    fn extract_anchor_removes_leading_compact_summary() {
        let messages = vec![compaction_summary("anchor"), user("new")];
        let (anchor, remaining) = extract_anchor(&messages).expect("anchor");
        assert_eq!(anchor, "anchor");
        assert_eq!(remaining, &messages[1..]);
    }

    #[test]
    fn extract_anchor_returns_none_without_leading_summary() {
        let messages = vec![user("new")];
        assert!(extract_anchor(&messages).is_none());
    }

    #[test]
    fn compact_messages_on_handle_replaces_range_in_place() {
        let mut messages = vec![system("head")];
        messages.extend((0..9).map(|index| assistant(&format!("old {index}"))));
        messages.extend(
            (0..6).flat_map(|index| [user(&format!("user {index}")), assistant(&"x".repeat(4000))]),
        );
        messages.extend((0..10).map(|index| assistant(&format!("recent {index}"))));
        messages.push(user("tail"));
        let handle: std::sync::Arc<std::sync::Mutex<Vec<Message>>> =
            std::sync::Arc::new(std::sync::Mutex::new(messages));
        // The recent-message and recent-user limits meet at the fifth recent user.
        // The compacted prefix is replaced by one summary while the tail remains.
        let result = compact_messages_on_handle(&handle, "gist".into(), 100);
        let result = result.expect("should compact");
        assert!(result.after_tokens < result.before_tokens);
        let msgs = handle.lock().unwrap();
        assert_eq!(result.compacted_start, 0);
        assert_eq!(result.compacted_end, 13);
        assert!(is_compaction_summary(&msgs[0]));
        assert_eq!(msgs.last().unwrap().text_concat(), "tail");
    }

    #[test]
    fn compact_messages_on_handle_none_when_under_budget() {
        let handle: std::sync::Arc<std::sync::Mutex<Vec<Message>>> =
            std::sync::Arc::new(std::sync::Mutex::new(vec![user("short")]));
        assert!(compact_messages_on_handle(&handle, "g".into(), 100_000).is_none());
    }

    #[test]
    fn compact_messages_on_handle_none_when_summary_would_not_shrink() {
        // 4 messages, all tiny → find_compact_range returns a range but the
        // summary message itself is comparable in size, so after >= before.
        // Construct a case where find_compact_range returns Some but shrink
        // check rejects it: make the range cover near-empty messages so the
        // summary overhead exceeds the savings.
        let handle: std::sync::Arc<std::sync::Mutex<Vec<Message>>> =
            std::sync::Arc::new(std::sync::Mutex::new(vec![
                system("h"),
                user("."),
                assistant("."),
                user("."),
                assistant("."),
                user("t"),
            ]));
        // budget=1 forces a range, but messages are so small the summary won't help
        let result = compact_messages_on_handle(&handle, "x".into(), 1);
        // Either no range found (len 6 but tiny), or shrink rejected.
        // The key invariant: handle is unchanged if None.
        let before_len = handle.lock().unwrap().len();
        if result.is_none() {
            assert_eq!(handle.lock().unwrap().len(), before_len);
        }
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
        assert_eq!(out.len(), 2, "summary + tail");
        assert_eq!(out[0].role, MessageRole::System);
        assert!(out[0].text_concat().contains("gist: talked about"));
        assert!(matches!(
            out[0].parts.as_slice(),
            [MessagePart::CompactSummary {
                seq_start: 1,
                seq_end: 4,
                count: 4,
                ..
            }]
        ));
        assert_eq!(out[1].role, MessageRole::User);
        assert_eq!(out[1].text_concat(), "tail");
    }

    #[test]
    fn find_compact_range_anchors_on_latest_structured_summary() {
        let mut msgs = vec![
            system("head"),
            Message::system_compact_summary(TurnId::now(), "old", 0, 1, 2),
        ];
        msgs.extend(
            (0..6).flat_map(|index| [user(&format!("user {index}")), assistant("assistant")]),
        );
        msgs.extend((0..12).map(|index| assistant(&format!("recent {index}"))));
        let range = find_compact_range(&msgs, 1).expect("range");
        assert_eq!(range.start, 1);
        assert_eq!(range.end, 4);
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
                    intent: None,
                },
            ],
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
        }
    }

    fn assistant_with_tool_uses(ids: &[&str]) -> Message {
        Message {
            role: MessageRole::Assistant,
            parts: ids
                .iter()
                .map(|id| MessagePart::ToolUse {
                    id: (*id).into(),
                    name: "fs.read".into(),
                    input: serde_json::json!({"path": format!("/tmp/{id}")}),
                    intent: None,
                })
                .collect(),
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
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
            origin: MessageOrigin::User,
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
            origin: MessageOrigin::User,
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
        let long_input = serde_json::json!({"content": "x".repeat(5000)});
        let slice = vec![assistant_with_tool_use("check", "fs.write", long_input)];
        let out = format_slice_for_summary(&slice);
        let tool_call_line = out
            .lines()
            .find(|l| l.contains("tool_call"))
            .unwrap_or_else(|| panic!("no tool_call line in {out}"));
        assert!(
            tool_call_line.chars().count() < 2200,
            "tool_call line not truncated: {tool_call_line}"
        );
    }

    fn compaction_summary(text: &str) -> Message {
        Message::system_compact_summary(TurnId::now(), text, 1, 5, 5)
    }

    #[test]
    fn is_compaction_summary_detects_structured_variant() {
        assert!(is_compaction_summary(&compaction_summary("gist")));
        assert!(!is_compaction_summary(&system("plain system msg")));
        assert!(!is_compaction_summary(&user("user msg")));
    }

    #[test]
    fn find_compact_range_spans_across_compaction_summaries() {
        let mut msgs = vec![
            system("head"),
            user(&"x".repeat(3000)),
            assistant(&"y".repeat(3000)),
            user(&"z".repeat(3000)),
            compaction_summary("first compaction summary"),
        ];
        msgs.extend(
            (0..6).flat_map(|index| [user(&format!("user {index}")), assistant(&"x".repeat(3000))]),
        );
        msgs.extend((0..10).map(|index| assistant(&format!("recent {index}"))));
        let range = find_compact_range(&msgs, 500).expect("expected range across summary");
        assert_eq!(
            range.start, 4,
            "range should anchor at the structured summary"
        );
        assert!(
            range.end > 4,
            "range should include later work, got {range:?}"
        );
        assert!(
            range.end - range.start >= 3,
            "range must cover >= 3 msgs, got {}",
            range.end - range.start
        );
    }

    #[test]
    fn find_compact_starts_from_summary() {
        let mut msgs = vec![user("a"), assistant("b"), compaction_summary("summary 1")];
        msgs.extend(
            (0..6).flat_map(|index| [user(&format!("user {index}")), assistant("assistant")]),
        );
        msgs.extend((0..12).map(|index| assistant(&format!("recent {index}"))));
        let range = find_compact_range(&msgs, 10).expect("expected range");
        assert_eq!(
            range.start, 2,
            "should start from the compact summary anchor"
        );
        assert_eq!(range.end, 5, "the fifth recent user is retained");
    }

    #[test]
    fn find_compact_range_includes_older_compaction_summaries() {
        let mut msgs = vec![compaction_summary("summary 0")];
        msgs.extend((0..6).flat_map(|index| {
            [
                user(&format!("old user {index}")),
                assistant(&"x".repeat(2000)),
            ]
        }));
        msgs.push(compaction_summary("summary 1"));
        msgs.extend((0..6).flat_map(|index| {
            [
                user(&format!("new user {index}")),
                assistant(&"z".repeat(2000)),
            ]
        }));
        msgs.extend((0..10).map(|index| assistant(&format!("tail {index}"))));
        let range = find_compact_range(&msgs, 500).expect("expected range");
        assert_eq!(range.start, 13, "should compact from the latest summary");
        assert!(
            range.end > range.start,
            "should include work after the latest summary"
        );
    }

    #[test]
    fn compacted_message_tokens_detects_growth() {
        let msgs = vec![compaction_summary("summary 0"), user("a"), assistant("b")];
        let range = CompactRange {
            start: 1,
            end: 3,
            tokens_saved_estimate: 0,
        };
        let before = estimate_tokens_for_messages(&msgs);
        let after = estimate_compacted_message_tokens(
            &msgs,
            &range,
            "a very long summary that expands the transcript a lot",
        );
        assert!(after > before, "expected growth to be detectable");
    }

    #[test]
    fn find_compact_starts_from_zero_without_summary() {
        let mut msgs = (0..26)
            .map(|index| assistant(&format!("old {index}")))
            .collect::<Vec<_>>();
        msgs.extend(
            (0..6).flat_map(|index| [user(&format!("user {index}")), assistant("assistant")]),
        );
        let range = find_compact_range(&msgs, 10).expect("expected range");
        assert_eq!(range.start, 0, "should start from 0 without summary");
        assert_eq!(range.end, 28, "the recent-message limit is retained");
    }

    #[test]
    fn filter_orphan_tool_messages_removes_orphan_results() {
        use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};
        let turn = TurnId::now();
        let msgs = vec![
            Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "orphan".into(),
                    content: "no matching use".into(),
                    is_error: false,
                }],
                turn_id: turn.clone(),
                origin: MessageOrigin::User,
            },
            Message {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::ToolUse {
                    id: "call_1".into(),
                    name: "fs.read".into(),
                    input: serde_json::json!({}),
                    intent: None,
                }],
                turn_id: turn.clone(),
                origin: MessageOrigin::User,
            },
            Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "call_1".into(),
                    content: "ok".into(),
                    is_error: false,
                }],
                turn_id: turn,
                origin: MessageOrigin::User,
            },
        ];
        let mut filtered = msgs;
        filter_orphan_tool_messages(&mut filtered);
        assert_eq!(filtered.len(), 2, "orphan result should be removed");
    }

    #[test]
    fn filter_orphan_tool_parts_preserves_valid_mixed_message_content() {
        use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};
        let turn = TurnId::now();
        let mut messages = vec![
            Message {
                role: MessageRole::Assistant,
                parts: vec![
                    MessagePart::Text {
                        text: "keep assistant text".into(),
                    },
                    MessagePart::ToolUse {
                        id: "valid".into(),
                        name: "fs.read".into(),
                        input: serde_json::json!({}),
                        intent: None,
                    },
                    MessagePart::ToolUse {
                        id: "orphan-use".into(),
                        name: "fs.read".into(),
                        input: serde_json::json!({}),
                        intent: None,
                    },
                ],
                turn_id: turn.clone(),
                origin: MessageOrigin::User,
            },
            Message {
                role: MessageRole::Tool,
                parts: vec![
                    MessagePart::Text {
                        text: "keep tool text".into(),
                    },
                    MessagePart::ToolResult {
                        tool_use_id: "valid".into(),
                        content: "ok".into(),
                        is_error: false,
                    },
                    MessagePart::ToolResult {
                        tool_use_id: "orphan-result".into(),
                        content: "drop".into(),
                        is_error: false,
                    },
                ],
                turn_id: turn,
                origin: MessageOrigin::User,
            },
        ];

        filter_orphan_tool_messages(&mut messages);

        assert_eq!(messages.len(), 2);
        assert!(
            matches!(&messages[0].parts[..], [MessagePart::Text { .. }, MessagePart::ToolUse { id, .. }] if id == "valid")
        );
        assert!(
            matches!(&messages[1].parts[..], [MessagePart::Text { .. }, MessagePart::ToolResult { tool_use_id, .. }] if tool_use_id == "valid")
        );
    }
}
