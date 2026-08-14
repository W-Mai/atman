use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, OnceLock};

use crate::compaction::estimate_tokens_for_message;
use crate::message::{Message, MessagePart, MessageRole};
use crate::tool::{BoxFut, ToolSpec};

pub(crate) const MAX_INHERITED_TURNS: usize = 5;
const LATEST_TURN_CAP: u64 = 30_000;
const SUMMARY_PREFIX: &str = "[atman: inherited turn summary]\nThis turn's assistant/system/tool output was compressed for context inheritance.\n\nSummary:\n";
const SUMMARY_SUFFIX: &str = "\n[/atman: inherited turn summary]";
const SUMMARY_PROMPT_VERSION: &str = "inherited-turn-v1";

pub(crate) trait InheritedContextSummarizer: Send + Sync {
    fn summarize<'a>(
        &'a self,
        source: &'a [Message],
        max_tokens: u64,
    ) -> BoxFut<'a, Option<String>>;
}

static SUMMARY_CACHE: OnceLock<Mutex<HashMap<u64, String>>> = OnceLock::new();

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ContextBudget {
    pub request: u64,
    pub history: u64,
}

pub(crate) fn project_messages(
    messages: &[Message],
    system: Option<&str>,
    tools: &[ToolSpec],
    input: &crate::value::Value,
    context_budget: u64,
    max_output_tokens: Option<u32>,
    current_turn: &crate::event::TurnId,
) -> Vec<Message> {
    if context_budget == 0 || messages.is_empty() {
        return messages.to_vec();
    }
    let budget = calculate_budget(context_budget, max_output_tokens, system, tools, input);
    let groups = group_turns(messages);
    let mut selected = Vec::new();
    let mut used = 0u64;
    let mut turns = 0usize;
    let current_index = groups
        .iter()
        .rposition(|group| group.turn_id == *current_turn);
    let mut ordered_indices = Vec::with_capacity(groups.len());
    if let Some(index) = current_index {
        ordered_indices.push(index);
    }
    ordered_indices.extend(
        groups
            .iter()
            .enumerate()
            .rev()
            .filter_map(|(index, _)| (Some(index) != current_index).then_some(index)),
    );

    for group_index in ordered_indices {
        if turns >= MAX_INHERITED_TURNS && Some(group_index) != current_index {
            break;
        }
        let group = &groups[group_index];
        let is_current = Some(group_index) == current_index;
        let user_tokens = group
            .messages
            .iter()
            .filter(|m| m.role == MessageRole::User)
            .map(estimate_tokens_for_message)
            .sum::<u64>();
        let output_messages: Vec<&Message> = group
            .messages
            .iter()
            .filter(|m| m.role != MessageRole::User && !is_compact_anchor(m))
            .collect();
        let output_tokens = output_messages
            .iter()
            .map(|m| estimate_tokens_for_message(m))
            .sum::<u64>();
        let output_limit = if is_current {
            LATEST_TURN_CAP.min(budget.history / 2)
        } else {
            u64::MAX
        };

        if user_tokens == 0 {
            continue;
        }
        if !is_current && used.saturating_add(user_tokens) > budget.history {
            continue;
        }

        let mut projected = group.messages.clone();
        let output_fits = output_tokens <= output_limit
            && used
                .saturating_add(user_tokens)
                .saturating_add(output_tokens)
                <= budget.history;
        if !output_fits && !output_messages.is_empty() {
            let summary_budget = budget
                .history
                .saturating_sub(used)
                .saturating_sub(user_tokens)
                .min(output_limit);
            projected.retain(|m| m.role == MessageRole::User || is_compact_anchor(m));
            if summary_budget > 0 {
                projected.push(inherited_summary(
                    group.turn_id.clone(),
                    output_tokens,
                    summary_budget,
                ));
            }
        }
        let projected_tokens = projected
            .iter()
            .map(estimate_tokens_for_message)
            .sum::<u64>();
        if used.saturating_add(projected_tokens) > budget.history && !is_current {
            continue;
        }
        used = used.saturating_add(projected_tokens);
        selected.push((group_index, projected));
        turns += 1;
    }

    selected.sort_by_key(|(index, _)| *index);
    let mut out = selected
        .into_iter()
        .flat_map(|(_, messages)| messages)
        .collect::<Vec<_>>();
    hard_fit(&mut out, budget.history, current_turn);
    out
}

pub(crate) fn fits_request(
    messages: &[Message],
    system: Option<&str>,
    tools: &[ToolSpec],
    input: &crate::value::Value,
    context_budget: u64,
    max_output_tokens: Option<u32>,
) -> bool {
    let budget = calculate_budget(context_budget, max_output_tokens, system, tools, input);
    estimate_messages(messages) <= budget.history
}

pub(crate) async fn summarize_projected_messages(
    projected: &mut [Message],
    source: &[Message],
    model: &str,
    summarizer: &dyn InheritedContextSummarizer,
) {
    let source_groups = group_turns(source);
    for message in projected.iter_mut().filter(|m| is_inherited_summary(m)) {
        let turn_source = source_groups
            .iter()
            .rfind(|group| group.turn_id == message.turn_id)
            .map(|group| {
                group
                    .messages
                    .iter()
                    .filter(|candidate| {
                        candidate.role != MessageRole::User && !is_compact_anchor(candidate)
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if turn_source.is_empty() {
            continue;
        }
        let max_tokens = estimate_tokens_for_message(message).max(64);
        let key = summary_cache_key(&turn_source, model);
        let cached = SUMMARY_CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .get(&key)
            .cloned();
        let summary = if let Some(cached) = cached {
            Some(cached)
        } else {
            summarizer.summarize(&turn_source, max_tokens).await
        };
        let Some(summary) = summary.filter(|summary| !summary.trim().is_empty()) else {
            continue;
        };
        let summary = truncate_summary(summary, max_tokens);
        SUMMARY_CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .insert(key, summary.clone());
        *message = Message::system_text(
            message.turn_id.clone(),
            format!("{SUMMARY_PREFIX}{summary}{SUMMARY_SUFFIX}"),
        );
    }
}

pub(crate) fn calculate_budget(
    context_window: u64,
    max_output_tokens: Option<u32>,
    system: Option<&str>,
    tools: &[ToolSpec],
    input: &crate::value::Value,
) -> ContextBudget {
    let configured = max_output_tokens.unwrap_or(32_000) as u64;
    let output_cap = (context_window as f64 * 0.20) as u64;
    let output_reserve = configured.min(output_cap).max(8_000.min(output_cap));
    let safety_cap = (context_window as f64 * 0.05) as u64;
    let safety = ((context_window as f64 * 0.02) as u64)
        .max(2_000.min(safety_cap))
        .min(safety_cap);
    let request = context_window
        .saturating_sub(output_reserve)
        .saturating_sub(safety);
    let system_tokens = system.map(estimate_text).unwrap_or(0);
    let tool_tokens = estimate_text(&serde_json::to_string(tools).unwrap_or_default());
    let input_tokens = estimate_text(&input.to_json().to_string());
    ContextBudget {
        request,
        history: request
            .saturating_sub(system_tokens)
            .saturating_sub(tool_tokens)
            .saturating_sub(input_tokens),
    }
}

fn estimate_text(text: &str) -> u64 {
    (text.len() as f64 / 3.5).ceil() as u64
}

fn summary_cache_key(source: &[Message], model: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    SUMMARY_PROMPT_VERSION.hash(&mut hasher);
    model.hash(&mut hasher);
    serde_json::to_string(source)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

fn truncate_summary(summary: String, max_tokens: u64) -> String {
    let max_chars = max_tokens.saturating_mul(3) as usize;
    summary.chars().take(max_chars).collect()
}

#[derive(Debug)]
struct TurnGroup {
    turn_id: crate::event::TurnId,
    messages: Vec<Message>,
}

fn group_turns(messages: &[Message]) -> Vec<TurnGroup> {
    let mut groups: Vec<TurnGroup> = Vec::new();
    let mut prefix = Vec::new();
    for message in messages {
        if message.role == MessageRole::User {
            if let Some(group) = groups.last_mut()
                && group.turn_id == message.turn_id
            {
                group.messages.push(message.clone());
                continue;
            }
            let mut turn_messages = std::mem::take(&mut prefix);
            turn_messages.push(message.clone());
            groups.push(TurnGroup {
                turn_id: message.turn_id.clone(),
                messages: turn_messages,
            });
        } else if let Some(group) = groups.last_mut() {
            group.messages.push(message.clone());
        } else {
            prefix.push(message.clone());
        }
    }
    if groups.is_empty() && !prefix.is_empty() {
        groups.push(TurnGroup {
            turn_id: prefix[0].turn_id.clone(),
            messages: prefix,
        });
    }
    groups
}

fn inherited_summary(turn_id: crate::event::TurnId, original_tokens: u64, budget: u64) -> Message {
    let body = format!(
        "{SUMMARY_PREFIX}The agent output for turn {turn_id} was omitted from the raw history projection because it exceeded the inherited context budget. Original estimated size: {original_tokens} tokens.{SUMMARY_SUFFIX}"
    );
    let max_chars = budget.saturating_mul(3) as usize;
    let text = if body.len() > max_chars {
        body.chars().take(max_chars).collect()
    } else {
        body
    };
    Message::system_text(turn_id, text)
}

fn hard_fit(messages: &mut Vec<Message>, budget: u64, current_turn: &crate::event::TurnId) {
    let mut groups = group_turns(messages);
    while groups
        .iter()
        .flat_map(|group| &group.messages)
        .map(estimate_tokens_for_message)
        .sum::<u64>()
        > budget
    {
        if let Some(group) = groups.iter_mut().find(|group| {
            group.turn_id != *current_turn
                && group.messages.iter().any(|m| {
                    m.role != MessageRole::User && !is_compact_anchor(m) && !is_inherited_summary(m)
                })
        }) {
            group.messages.retain(|m| {
                m.role == MessageRole::User || is_compact_anchor(m) || is_inherited_summary(m)
            });
        } else if let Some(group) = groups.iter_mut().find(|group| {
            group.turn_id != *current_turn && group.messages.iter().any(is_inherited_summary)
        }) {
            group.messages.retain(|m| !is_inherited_summary(m));
        } else if let Some(index) = groups.iter().position(|group| {
            group.turn_id != *current_turn
                && group
                    .messages
                    .iter()
                    .any(|message| !is_compact_anchor(message))
        }) {
            let anchors = groups[index]
                .messages
                .iter()
                .filter(|message| is_compact_anchor(message))
                .cloned()
                .collect::<Vec<_>>();
            if anchors.is_empty() {
                groups.remove(index);
            } else {
                groups[index].messages = anchors;
            }
        } else {
            break;
        }
    }
    *messages = groups
        .into_iter()
        .flat_map(|group| group.messages)
        .collect();
}

fn is_compact_anchor(message: &Message) -> bool {
    message
        .parts
        .iter()
        .any(|part| matches!(part, MessagePart::CompactSummary { .. }))
}

fn is_inherited_summary(message: &Message) -> bool {
    message.role == MessageRole::System && message.text_concat().starts_with(SUMMARY_PREFIX)
}

fn estimate_messages(messages: &[Message]) -> u64 {
    messages.iter().map(estimate_tokens_for_message).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::TurnId;

    fn turn(text: &str) -> TurnId {
        let _ = text;
        TurnId::now()
    }
    fn user(id: &TurnId, text: &str) -> Message {
        Message::user_text(id.clone(), text)
    }
    fn assistant(id: &TurnId, text: &str) -> Message {
        Message::assistant_text(id.clone(), text)
    }

    #[test]
    fn keeps_newest_five_turns_and_user_messages() {
        let mut messages = Vec::new();
        for i in 0..7 {
            let id = turn(&i.to_string());
            messages.push(user(&id, &format!("user {i}")));
            messages.push(assistant(&id, &format!("assistant {i}")));
        }
        let out = project_messages(
            &messages,
            None,
            &[],
            &crate::value::Value::Unit,
            20_000,
            Some(1_000),
            &messages[12].turn_id,
        );
        let ids: std::collections::HashSet<_> = out.iter().map(|m| m.turn_id.clone()).collect();
        assert_eq!(ids.len(), 5);
        assert!(out.iter().any(|m| m.text_concat() == "user 2"));
        assert!(!out.iter().any(|m| m.text_concat() == "user 1"));
    }

    #[test]
    fn replaces_oversized_agent_output_with_marked_summary() {
        let id = turn("x");
        let messages = vec![user(&id, "keep"), assistant(&id, &"x".repeat(20_000))];
        let out = project_messages(
            &messages,
            None,
            &[],
            &crate::value::Value::Unit,
            10_000,
            Some(1_000),
            &id,
        );
        assert!(
            out.iter()
                .any(|m| m.text_concat().contains("inherited turn summary"))
        );
        assert!(
            estimate_messages(&out)
                <= calculate_budget(10_000, Some(1_000), None, &[], &crate::value::Value::Unit)
                    .history
        );
    }

    #[test]
    fn projection_does_not_mutate_source() {
        let id = turn("x");
        let messages = vec![user(&id, "keep"), assistant(&id, &"x".repeat(20_000))];
        let before = messages.clone();
        let _ = project_messages(
            &messages,
            None,
            &[],
            &crate::value::Value::Unit,
            10_000,
            Some(1_000),
            &id,
        );
        assert_eq!(messages, before);
    }

    #[test]
    fn repeated_non_contiguous_turn_id_is_not_merged() {
        let repeated = turn("repeated");
        let middle = turn("middle");
        let messages = vec![
            user(&repeated, "first"),
            assistant(&repeated, "first answer"),
            user(&middle, "middle"),
            assistant(&middle, "middle answer"),
            user(&repeated, "second"),
        ];
        let groups = group_turns(&messages);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].turn_id, repeated);
        assert_eq!(groups[2].turn_id, repeated);
    }

    #[test]
    fn user_boundary_keeps_misanchored_agent_output_with_current_turn() {
        let old = turn("old");
        let current = turn("current");
        let messages = vec![
            user(&old, "old request"),
            assistant(&old, "old answer"),
            user(&current, "current request"),
            assistant(&old, "misanchored current answer"),
            Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "call-current".into(),
                    content: "current result".into(),
                    is_error: false,
                }],
                turn_id: current.clone(),
                origin: crate::message::MessageOrigin::Internal,
            },
        ];

        let groups = group_turns(&messages);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[1].turn_id, current);
        assert_eq!(groups[1].messages.len(), 3);

        let out = project_messages(
            &messages,
            None,
            &[],
            &crate::value::Value::Unit,
            20_000,
            Some(1_000),
            &current,
        );
        assert!(
            out.iter()
                .any(|message| message.text_concat() == "misanchored current answer")
        );
        assert!(out.iter().any(|message| {
            message.parts.iter().any(|part| {
                matches!(
                    part,
                    MessagePart::ToolResult { content, .. } if content == "current result"
                )
            })
        }));
    }

    #[test]
    fn tool_use_and_result_are_removed_as_one_agent_output_group() {
        let old = turn("old");
        let current = turn("current");
        let messages = vec![
            user(&old, "run it"),
            Message {
                role: MessageRole::Assistant,
                parts: vec![MessagePart::ToolUse {
                    id: "call-1".into(),
                    name: "large".into(),
                    input: serde_json::json!({"payload": "x".repeat(30_000)}),
                }],
                turn_id: old.clone(),
                origin: crate::message::MessageOrigin::Internal,
            },
            Message {
                role: MessageRole::Tool,
                parts: vec![MessagePart::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "y".repeat(30_000),
                    is_error: false,
                }],
                turn_id: old.clone(),
                origin: crate::message::MessageOrigin::Internal,
            },
            user(&current, "continue"),
        ];
        let out = project_messages(
            &messages,
            None,
            &[],
            &crate::value::Value::Unit,
            12_000,
            Some(1_000),
            &current,
        );
        assert!(!out.iter().any(|m| {
            m.parts.iter().any(|part| {
                matches!(
                    part,
                    MessagePart::ToolUse { .. } | MessagePart::ToolResult { .. }
                )
            })
        }));
        assert!(out.iter().any(is_inherited_summary));
    }

    #[test]
    fn dynamic_system_and_tool_schema_reduce_history_budget() {
        let tool = ToolSpec {
            name: "large".into(),
            description: Some("d".repeat(2_000)),
            input_schema: serde_json::json!({"description": "s".repeat(4_000)}),
        };
        let plain = calculate_budget(20_000, Some(1_000), None, &[], &crate::value::Value::Unit);
        let dynamic = calculate_budget(
            20_000,
            Some(1_000),
            Some(&"x".repeat(4_000)),
            &[tool],
            &crate::value::Value::Unit,
        );
        assert!(dynamic.history < plain.history);
        assert_eq!(dynamic.request, plain.request);
    }

    #[test]
    fn compact_summary_is_preserved_as_cross_turn_anchor() {
        let old = turn("old");
        let current = turn("current");
        let anchor = Message {
            role: MessageRole::System,
            parts: vec![MessagePart::CompactSummary {
                summary: "cross-turn facts".into(),
                seq_start: 1,
                seq_end: 2,
                count: 2,
            }],
            turn_id: old.clone(),
            origin: crate::message::MessageOrigin::Internal,
        };
        let messages = vec![
            user(&old, "old user"),
            anchor.clone(),
            assistant(&old, &"x".repeat(20_000)),
            user(&current, "current user"),
        ];
        let out = project_messages(
            &messages,
            None,
            &[],
            &crate::value::Value::Unit,
            10_000,
            Some(1_000),
            &current,
        );
        assert!(out.contains(&anchor));
    }

    struct MockSummarizer {
        calls: std::sync::atomic::AtomicUsize,
        result: Option<String>,
    }

    impl InheritedContextSummarizer for MockSummarizer {
        fn summarize<'a>(&'a self, _: &'a [Message], _: u64) -> BoxFut<'a, Option<String>> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let result = self.result.clone();
            Box::pin(async move { result })
        }
    }

    #[tokio::test]
    async fn summarizer_is_cached_and_failure_keeps_omission_fallback() {
        SUMMARY_CACHE
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap()
            .clear();
        let id = turn("summary");
        let source = vec![user(&id, "keep"), assistant(&id, &"z".repeat(20_000))];
        let mut projected = project_messages(
            &source,
            None,
            &[],
            &crate::value::Value::Unit,
            10_000,
            Some(1_000),
            &id,
        );
        let ok = MockSummarizer {
            calls: std::sync::atomic::AtomicUsize::new(0),
            result: Some("important facts".into()),
        };
        summarize_projected_messages(&mut projected, &source, "model", &ok).await;
        assert!(
            projected
                .iter()
                .any(|m| m.text_concat().contains("important facts"))
        );
        let mut projected_again = project_messages(
            &source,
            None,
            &[],
            &crate::value::Value::Unit,
            10_000,
            Some(1_000),
            &id,
        );
        summarize_projected_messages(&mut projected_again, &source, "model", &ok).await;
        assert_eq!(ok.calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        let other = turn("failure");
        let failure_source = vec![user(&other, "keep"), assistant(&other, &"q".repeat(20_000))];
        let mut fallback = project_messages(
            &failure_source,
            None,
            &[],
            &crate::value::Value::Unit,
            10_000,
            Some(1_000),
            &other,
        );
        let failing = MockSummarizer {
            calls: std::sync::atomic::AtomicUsize::new(0),
            result: None,
        };
        summarize_projected_messages(&mut fallback, &failure_source, "model", &failing).await;
        assert!(fallback.iter().any(|m| {
            m.text_concat()
                .contains("was omitted from the raw history projection")
        }));
    }
}
