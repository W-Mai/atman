use std::sync::Arc;

use crate::error::RuntimeError;
use crate::memory::MemoryId;
use crate::memory::confession::{Confession, ConfessionStore};
use crate::memory::goal::GoalStore;
use crate::memory::goal::{GoalActor, GoalPauseReason};
use crate::memory::spec::SpecStore;
use crate::memory::todo::{Todo, TodoStatus, TodoStore};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::ValueJson;
use crate::value::{AtmanPayload, Value};

pub struct MemoryGoalGet {
    pub store: Arc<GoalStore>,
}

impl Tool for MemoryGoalGet {
    fn name(&self) -> &str {
        "memory.goal.get"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Read the current agent's goal, state and revision. Use target_handle to inspect a descendant's goal.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        goal_schema(false)
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            if let Some(control) = goal_target(&args, ctx)? {
                return goal_value(&control);
            }
            Ok(Value::from_json(
                serde_json::to_value(crate::goal::GoalSnapshot {
                    goal: self.store.load().map_err(goal_error)?,
                    execution: crate::goal::GoalExecution::Idle,
                    paused_by: None,
                    running_operations: 0,
                })
                .map_err(goal_error)?,
            ))
        })
    }
}

pub struct MemoryGoalSet {
    pub store: Arc<GoalStore>,
}

impl Tool for MemoryGoalSet {
    fn name(&self) -> &str {
        "memory.goal.set"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Create a goal for an actionable request, or update its objective. Updates require goal_id and expected_revision from goal.get and preserve pause state. target_handle addresses a descendant. Use goal.pause when complete or waiting for input.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        let mut schema = goal_schema(false);
        schema["properties"]["text"] = serde_json::json!({"type":"string", "description":"Concrete objective and acceptance criteria."});
        schema["required"] = serde_json::json!(["text"]);
        schema
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let text = required_string(&args, "text")?;
            if let Some(control) = goal_target(&args, ctx)? {
                let expected = goal_expected(&args, control.record().is_some())?;
                if ctx.goal_resume_allowed
                    && control.record().is_some_and(|g| {
                        g.pause.as_ref().is_some_and(|p| {
                            p.actor == GoalActor::Agent && p.reason == GoalPauseReason::Completed
                        })
                    })
                {
                    control
                        .replace_completed(&text, expected)
                        .map_err(goal_error)?;
                } else {
                    control.set(&text, expected).map_err(goal_error)?;
                }
                return goal_value(&control);
            }
            self.store
                .set(&text)
                .map_err(|e| RuntimeError::ToolFailed(format!("goal.set: {e}")))?;
            Ok(Value::from_json(
                serde_json::to_value(crate::goal::GoalSnapshot {
                    goal: self.store.load().map_err(goal_error)?,
                    execution: crate::goal::GoalExecution::Idle,
                    paused_by: None,
                    running_operations: 0,
                })
                .map_err(goal_error)?,
            ))
        })
    }
}

fn goal_error(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ToolFailed(format!("goal: {error}"))
}

fn goal_value(control: &crate::goal::GoalController) -> ToolResult {
    Ok(Value::from_json(
        serde_json::to_value(control.snapshot()).map_err(goal_error)?,
    ))
}

fn goal_schema(required_revision: bool) -> serde_json::Value {
    serde_json::json!({
        "type":"object",
        "properties":{
            "target_handle":{"type":"string", "description":"Optional descendant agent handle; defaults to this agent."},
            "goal_id":{"type":"string"},
            "expected_revision":{"type":"integer", "minimum":1}
        },
        "required": if required_revision { vec!["goal_id", "expected_revision"] } else { vec![] }
    })
}

fn goal_expected(args: &ToolArgs, required: bool) -> Result<Option<(&str, u64)>, RuntimeError> {
    match (args.named("goal_id"), args.named("expected_revision")) {
        (Some(Value::Str(id)), Some(Value::Int(revision))) if *revision > 0 => {
            Ok(Some((id, *revision as u64)))
        }
        (None, None) if !required => Ok(None),
        _ => Err(goal_error(
            "goal_id and expected_revision are required; read goal.get first",
        )),
    }
}

fn goal_target(
    args: &ToolArgs,
    ctx: &ToolCtx,
) -> Result<Option<Arc<crate::goal::GoalController>>, RuntimeError> {
    let own = ctx
        .goal_control
        .clone()
        .or_else(|| ctx.session_runtime.as_ref().map(|s| s.goal_control.clone()));
    let Some(target) = args.named("target_handle") else {
        return Ok(own);
    };
    let Value::Str(handle) = target else {
        return Err(goal_error("target_handle must be a string"));
    };
    let own = own.ok_or_else(|| goal_error("no managed agent goal context"))?;
    let entry = ctx
        .flow_registry
        .as_ref()
        .ok_or_else(|| goal_error("no flow registry"))?
        .lookup(handle)?;
    let target = entry
        .goal_control
        .get()
        .ok_or_else(|| goal_error("target has no managed goal"))?
        .clone();
    if !Arc::ptr_eq(&own, &target) && !own.is_ancestor_of(&target) {
        return Err(goal_error("target is not this agent or its descendant"));
    }
    Ok(Some(target))
}

pub struct MemoryGoalControl {
    pub resume: bool,
}

impl Tool for MemoryGoalControl {
    fn name(&self) -> &str {
        if self.resume {
            "memory.goal.resume"
        } else {
            "memory.goal.pause"
        }
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }
    fn description(&self) -> Option<&str> {
        Some(if self.resume {
            "Resume a descendant goal, or your own agent-paused goal after new user input. User-paused goals require user controls. Read goal.get first."
        } else {
            "Pause this goal when completed, blocked, or awaiting input. This stops automatic continuation and allows your final response. With target_handle, pause a descendant at its next execution boundary. Read goal.get first."
        })
    }
    fn input_schema(&self) -> serde_json::Value {
        let mut schema = goal_schema(true);
        if !self.resume {
            schema["properties"]["reason"] = serde_json::json!({"type":"string", "enum":["completed","needs_input","blocked","user_requested"]});
            schema["properties"]["summary"] = serde_json::json!({"type":"string", "minLength":1});
            schema["required"] =
                serde_json::json!(["goal_id", "expected_revision", "reason", "summary"]);
        }
        schema
    }
    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let control = goal_target(&args, ctx)?.ok_or_else(|| goal_error("no goal context"))?;
            let own = ctx
                .goal_control
                .as_ref()
                .or_else(|| ctx.session_runtime.as_ref().map(|s| &s.goal_control))
                .ok_or_else(|| goal_error("no goal context"))?;
            let actor = if Arc::ptr_eq(own, &control) {
                GoalActor::Agent
            } else {
                GoalActor::Parent
            };
            let expected = goal_expected(&args, true)?;
            if self.resume {
                if let Some(Value::Str(handle)) = args.named("target_handle")
                    && let Some(registry) = &ctx.flow_registry
                    && registry.lookup(handle)?.cancel.is_cancelled()
                {
                    return Err(goal_error("cancelled agent cannot be resumed"));
                }
                if actor == GoalActor::Agent && !ctx.goal_resume_allowed {
                    return Err(goal_error("self resume requires new user or parent input"));
                }
                control.resume(actor, expected).map_err(goal_error)?;
            } else {
                let reason = required_string(&args, "reason")?;
                let reason = match reason.as_str() {
                    "completed" => GoalPauseReason::Completed,
                    "needs_input" => GoalPauseReason::NeedsInput,
                    "blocked" => GoalPauseReason::Blocked,
                    "user_requested" => GoalPauseReason::UserRequested,
                    _ => return Err(goal_error("invalid pause reason")),
                };
                let summary = required_string(&args, "summary")?;
                if summary.trim().is_empty() {
                    return Err(goal_error("pause summary is required"));
                }
                control
                    .pause(actor, reason, summary, expected)
                    .map_err(goal_error)?;
            }
            goal_value(&control)
        })
    }
}

pub struct MemoryRecentTurns;

#[cfg(test)]
mod goal_control_tests {
    use super::*;
    use crate::goal::GoalController;

    #[tokio::test]
    async fn goal_tools_use_current_owner_and_reject_sibling_targets() {
        let root = GoalController::ephemeral("root".into());
        root.set("root objective", None).unwrap();
        let child = root
            .child("child".into(), "child objective".into(), None)
            .unwrap();
        let sibling = root
            .child("sibling".into(), "other objective".into(), None)
            .unwrap();
        let registry = Arc::new(crate::tools::agent_ctrl::FlowRegistry::new());
        let child_entry = registry.create_entry(
            "child".into(),
            "child".into(),
            String::new(),
            crate::event::FlowRunId::now(),
        );
        child_entry.goal_control.set(child.clone()).ok().unwrap();
        let peer_entry = registry.create_entry(
            "sibling".into(),
            "other".into(),
            String::new(),
            crate::event::FlowRunId::now(),
        );
        peer_entry.goal_control.set(sibling.clone()).ok().unwrap();
        let mut ctx = ToolCtx::new().with_flow_registry(registry);
        ctx.goal_control = Some(child.clone());
        assert!(Arc::ptr_eq(
            &goal_target(&ToolArgs::default(), &ctx).unwrap().unwrap(),
            &child
        ));
        let mut args = ToolArgs {
            positional: vec![],
            named: vec![("target_handle".into(), Value::Str("sibling".into()))],
        };
        assert!(goal_target(&args, &ctx).is_err());
        ctx.goal_control = Some(root.clone());
        let saved = sibling.record().unwrap();
        args.named.extend([
            ("goal_id".into(), Value::Str(saved.id)),
            (
                "expected_revision".into(),
                Value::Int(saved.revision as i64),
            ),
            ("reason".into(), Value::Str("blocked".into())),
            ("summary".into(), Value::Str("Need a decision".into())),
        ]);
        MemoryGoalControl { resume: false }
            .call(args, &ctx)
            .await
            .unwrap();
        assert!(!sibling.should_continue());
        assert!(root.should_continue());
        assert!(child.should_continue());
        peer_entry.cancel.cancel();
        let saved = sibling.record().unwrap();
        let args = ToolArgs {
            named: vec![
                ("target_handle".into(), Value::Str("sibling".into())),
                ("goal_id".into(), Value::Str(saved.id)),
                (
                    "expected_revision".into(),
                    Value::Int(saved.revision as i64),
                ),
            ],
            ..Default::default()
        };
        assert!(
            MemoryGoalControl { resume: true }
                .call(args, &ctx)
                .await
                .is_err()
        );
        assert!(!sibling.should_continue());
    }
}

const RECENT_EXCERPT_MESSAGE_CHARS: usize = 2_000;

#[derive(Clone, Copy)]
enum RecentExcerpt {
    LegacyRecent(usize),
    HeadTail { head: usize, tail: usize },
}

#[derive(Clone, Copy, Default)]
enum RecentExcerptMode {
    #[default]
    Transcript,
    Conversation,
}

fn conversation_excerpt_messages(
    messages: &[crate::message::Message],
) -> Vec<crate::message::Message> {
    use crate::message::{MessageOrigin, MessagePart, MessageRole};
    messages
        .iter()
        .filter_map(|message| {
            let summary = (message.role == MessageRole::Assistant)
                .then(|| crate::tools::final_answer::summary(message))
                .flatten();
            let parts = if let Some(text) = summary {
                vec![MessagePart::Text { text }]
            } else {
                message
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        MessagePart::CompactSummary { .. } => Some(part.clone()),
                        MessagePart::Text { .. } | MessagePart::Image { .. }
                            if message.origin != MessageOrigin::Internal
                                && message.role != MessageRole::System =>
                        {
                            Some(part.clone())
                        }
                        MessagePart::ToolUse {
                            id, name, intent, ..
                        } => Some(MessagePart::ToolUse {
                            id: id.clone(),
                            name: name.clone(),
                            intent: intent.clone(),
                            input: serde_json::Value::Null,
                        }),
                        MessagePart::ToolResult {
                            tool_use_id,
                            content,
                            is_error: true,
                        } => Some(MessagePart::ToolResult {
                            tool_use_id: tool_use_id.clone(),
                            content: content.chars().take(RECENT_EXCERPT_MESSAGE_CHARS).collect(),
                            is_error: true,
                        }),
                        _ => None,
                    })
                    .collect()
            };
            (!parts.is_empty()).then(|| crate::message::Message {
                parts,
                role: message.role,
                turn_id: message.turn_id.clone(),
                origin: message.origin,
            })
        })
        .collect()
}

fn recent_turns_value(
    message_count: u64,
    turn_count: u64,
    messages: Vec<crate::message::Message>,
    excerpt: Option<RecentExcerpt>,
    excerpt_mode: RecentExcerptMode,
) -> Value {
    let excerpt = excerpt.map(|mode| {
        let projected;
        let source = match excerpt_mode {
            RecentExcerptMode::Transcript => &messages,
            RecentExcerptMode::Conversation => {
                projected = conversation_excerpt_messages(&messages);
                &projected
            }
        };
        match mode {
            RecentExcerpt::LegacyRecent(limit) => bounded_recent_excerpt(source, limit),
            RecentExcerpt::HeadTail { head, tail } => bounded_head_tail_excerpt(source, head, tail),
        }
    });
    let items = messages
        .into_iter()
        .map(|message| Value::Host(AtmanPayload::Message(message)))
        .collect();
    let mut fields = vec![
        (
            "total_message_count".into(),
            Value::Int(message_count as i64),
        ),
        ("total_turn_count".into(), Value::Int(turn_count as i64)),
        ("items".into(), Value::List(items)),
    ];
    if let Some((text, truncated)) = excerpt {
        fields.push(("excerpt".into(), Value::Str(text)));
        fields.push(("excerpt_truncated".into(), Value::Bool(truncated)));
    }
    Value::Struct(fields)
}

#[derive(Default)]
struct HeadTailExcerpt {
    head: String,
    tail: std::collections::VecDeque<char>,
    head_chars: usize,
    head_limit: usize,
    tail_limit: usize,
    total_chars: usize,
}

impl HeadTailExcerpt {
    fn new(head_limit: usize, tail_limit: usize) -> Self {
        Self {
            head_limit,
            tail_limit,
            ..Self::default()
        }
    }

    fn push(&mut self, text: &str) {
        for ch in text.chars() {
            if self.head_chars < self.head_limit {
                self.head.push(ch);
                self.head_chars += 1;
            }
            if self.tail_limit > 0 {
                if self.tail.len() == self.tail_limit {
                    self.tail.pop_front();
                }
                self.tail.push_back(ch);
            }
            self.total_chars += 1;
        }
    }

    fn finish(self) -> (String, bool) {
        let head_chars = self.head_chars;
        let tail_chars = self.tail.len();
        let overlap = head_chars
            .saturating_add(tail_chars)
            .saturating_sub(self.total_chars);
        let tail = self.tail.into_iter().skip(overlap).collect::<String>();
        let truncated = self.total_chars > head_chars.saturating_add(tail_chars);
        if !truncated {
            return (self.head + &tail, false);
        }
        if self.head.is_empty() {
            return (tail, true);
        }
        if tail.is_empty() {
            return (self.head, true);
        }
        let omitted = self
            .total_chars
            .saturating_sub(head_chars.saturating_add(tail_chars));
        (
            format!(
                "{}\n\n[... omitted {omitted} chars ...]\n\n{tail}",
                self.head
            ),
            true,
        )
    }
}

fn visit_message_excerpt_segments(
    message: &crate::message::Message,
    mut visit: impl FnMut(&str) -> bool,
) {
    use crate::message::MessagePart;

    if visit(&format!("[{}]", message.role.as_str())) {
        return;
    }
    for part in &message.parts {
        let stop = match part {
            MessagePart::FinalAnswerSummary { .. } | MessagePart::ResponsesState { .. } => false,
            MessagePart::ContextRecord(record) => {
                let label = format!("\n[context {}@{}]\n", record.key(), record.revision());
                visit(&label) || visit(&record.render_for_model())
            }
            MessagePart::CompactSummary { summary, .. } => visit("\nsummary: ") || visit(summary),
            MessagePart::Text { text } => visit("\ntext: ") || visit(text),
            MessagePart::Thinking { .. } => visit("\n[thinking omitted]"),
            MessagePart::Image { .. } => visit("\n[image]"),
            MessagePart::ToolUse { name, intent, .. } => {
                visit("\ntool_call: ")
                    || visit(name)
                    || intent
                        .as_ref()
                        .is_some_and(|intent| visit(" — ") || visit(intent.as_str()))
            }
            MessagePart::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                let status = if *is_error { "error" } else { "ok" };
                visit(&format!("\ntool_result {tool_use_id} ({status}): ")) || visit(content)
            }
        };
        if stop {
            return;
        }
    }
}

fn bounded_head_tail_excerpt(
    messages: &[crate::message::Message],
    head_chars: usize,
    tail_chars: usize,
) -> (String, bool) {
    let mut excerpt = HeadTailExcerpt::new(head_chars, tail_chars);
    for (index, message) in messages.iter().enumerate() {
        if index > 0 {
            excerpt.push("\n\n");
        }
        visit_message_excerpt_segments(message, |segment| {
            excerpt.push(segment);
            false
        });
    }
    excerpt.finish()
}

fn non_negative_excerpt_field(value: &Value, name: &str) -> Result<usize, RuntimeError> {
    match value.field(name) {
        Some(Value::Int(value)) if *value >= 0 => Ok(*value as usize),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: format!("non-negative int for excerpt.{name}"),
            actual: other.kind_name().into(),
        }),
        None => Ok(0),
    }
}

fn recent_excerpt_arg(args: &ToolArgs) -> Result<Option<RecentExcerpt>, RuntimeError> {
    let legacy = args.named("excerpt_chars");
    let head_tail = args.named("excerpt");
    if legacy.is_some() && head_tail.is_some() {
        return Err(RuntimeError::ToolFailed(
            "memory.recent_turns: choose `excerpt` or `excerpt_chars`, not both".into(),
        ));
    }
    if let Some(value) = head_tail {
        let Value::Struct(_) = value else {
            return Err(RuntimeError::TypeMismatch {
                expected: "struct with non-negative `head` and/or `tail`".into(),
                actual: value.kind_name().into(),
            });
        };
        return Ok(Some(RecentExcerpt::HeadTail {
            head: non_negative_excerpt_field(value, "head")?,
            tail: non_negative_excerpt_field(value, "tail")?,
        }));
    }
    match legacy {
        Some(Value::Int(value)) if *value >= 0 => {
            Ok(Some(RecentExcerpt::LegacyRecent(*value as usize)))
        }
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "non-negative int".into(),
            actual: other.kind_name().into(),
        }),
        None => Ok(None),
    }
}

fn bounded_recent_excerpt(
    messages: &[crate::message::Message],
    max_chars: usize,
) -> (String, bool) {
    let mut remaining = max_chars;
    let mut chunks = Vec::new();
    let mut truncated = false;
    for message in messages.iter().rev() {
        let separator_chars = usize::from(!chunks.is_empty()) * 2;
        if remaining <= separator_chars {
            truncated = true;
            break;
        }
        let message_limit = remaining
            .saturating_sub(separator_chars)
            .min(RECENT_EXCERPT_MESSAGE_CHARS);
        let (chunk, message_truncated) = bounded_message_excerpt(message, message_limit);
        remaining = remaining.saturating_sub(separator_chars + chunk.chars().count());
        chunks.push(chunk);
        truncated |= message_truncated;
    }
    if chunks.len() < messages.len() {
        truncated = true;
    }
    chunks.reverse();
    (chunks.join("\n\n"), truncated)
}

fn bounded_message_excerpt(message: &crate::message::Message, max_chars: usize) -> (String, bool) {
    fn push_bounded(out: &mut String, used: &mut usize, max: usize, text: &str) -> bool {
        for ch in text.chars() {
            if *used == max {
                return true;
            }
            out.push(ch);
            *used += 1;
        }
        false
    }

    let mut out = String::new();
    let mut used = 0;
    let mut truncated = false;
    visit_message_excerpt_segments(message, |segment| {
        let segment_truncated = push_bounded(&mut out, &mut used, max_chars, segment);
        truncated |= segment_truncated;
        segment_truncated
    });
    (out, truncated)
}

impl Tool for MemoryRecentTurns {
    fn name(&self) -> &str {
        "memory.recent_turns"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Return the last N Message values (user + assistant + tool_result) from the \
             current context so a flow can hand the code agent a sliding \
             history window. `items` remain lossless; `excerpt: {head, tail}` returns a \
             bounded text excerpt retaining independently selected transcript edges. \
             `excerpt_chars` selects a recent-first budget. `excerpt_mode: conversation` \
             keeps conversation, available answer summaries, tool intents and bounded errors \
             without full tool outputs, thinking, or context records.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "n": {"type": "integer", "description": "Max complete turns to return (default 10)"},
                "excerpt_mode": {"type": "string", "enum": ["transcript", "conversation"], "description": "Excerpt projection only; items remain lossless. Defaults to transcript."},
                "excerpt": {
                    "type": "object",
                    "description": "Also return an `excerpt` retaining independently bounded transcript edges without changing lossless `items`.",
                    "properties": {
                        "head": {"type": "integer", "minimum": 0, "description": "Characters retained from the start of the selected transcript."},
                        "tail": {"type": "integer", "minimum": 0, "description": "Characters retained from the end of the selected transcript."}
                    },
                    "additionalProperties": false
                },
                "excerpt_chars": {"type": "integer", "minimum": 0, "description": "Legacy recent-first excerpt budget; cannot be combined with `excerpt`."}
            }
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let n = match args.named("n").or_else(|| args.positional(0).ok()) {
                Some(Value::Int(k)) if *k >= 0 => *k as usize,
                Some(other) => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "non-negative int".into(),
                        actual: other.kind_name().into(),
                    });
                }
                None => 10,
            };
            let excerpt = recent_excerpt_arg(&args)?;
            let excerpt_mode = match args.named("excerpt_mode") {
                None => RecentExcerptMode::Transcript,
                Some(Value::Str(mode)) if mode == "transcript" => RecentExcerptMode::Transcript,
                Some(Value::Str(mode)) if mode == "conversation" => RecentExcerptMode::Conversation,
                _ => {
                    return Err(RuntimeError::ToolFailed(
                        "memory.recent_turns: excerpt_mode must be transcript or conversation"
                            .into(),
                    ));
                }
            };
            if n == 0 {
                if let Some(cb) = &ctx.on_memory_recent {
                    cb(0);
                }
                return Ok(recent_turns_value(0, 0, Vec::new(), excerpt, excerpt_mode));
            }
            if matches!(ctx.history_segment, crate::tool::HistorySegment::Spawned)
                && let Some(handle) = ctx.session_messages_handle.as_ref()
            {
                let msgs = handle.lock().unwrap().clone();
                let (total, recent) = crate::history_store::recent_turn_messages(&msgs, n);
                if let Some(cb) = &ctx.on_memory_recent {
                    cb(recent.len() as u16);
                }
                return Ok(recent_turns_value(
                    msgs.len() as u64,
                    total,
                    recent,
                    excerpt,
                    excerpt_mode,
                ));
            }
            // Sub-agent path: session_messages has the child's local message list.
            if let Some(msgs) = ctx.session_messages.as_ref() {
                let (total, recent) = crate::history_store::recent_turn_messages(msgs, n);
                if let Some(cb) = &ctx.on_memory_recent {
                    cb(recent.len() as u16);
                }
                return Ok(recent_turns_value(
                    msgs.len() as u64,
                    total,
                    recent,
                    excerpt,
                    excerpt_mode,
                ));
            }
            // Main-agent path: delegate to HistoryStore.
            let Some(store) = ctx.history_store.clone() else {
                return Err(RuntimeError::ToolFailed(
                    "memory.recent_turns: no history store on context".into(),
                ));
            };
            let (message_count, turn_count, msgs) =
                tokio::task::spawn_blocking(move || store.recent(n))
                    .await
                    .map_err(|e| RuntimeError::ToolFailed(format!("recent_turns: {e}")))??;
            if let Some(cb) = &ctx.on_memory_recent {
                cb(msgs.len() as u16);
            }
            Ok(recent_turns_value(
                message_count,
                turn_count,
                msgs,
                excerpt,
                excerpt_mode,
            ))
        })
    }
}

pub struct MemoryGoalClear {
    pub store: Arc<GoalStore>,
}

impl Tool for MemoryGoalClear {
    fn name(&self) -> &str {
        "memory.goal.clear"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Clear a custom session goal. Managed agents report completion with goal.pause; user controls can clear managed goals.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    fn call<'a>(&'a self, _args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            if ctx.goal_control.is_some() {
                return Err(goal_error(
                    "managed goals must be paused; clearing requires user control",
                ));
            }
            if let Some(session) = &ctx.session_runtime {
                session.goal_control.clear().map_err(goal_error)?;
                return Ok(Value::Unit);
            }
            self.store
                .clear()
                .map_err(|e| RuntimeError::ToolFailed(format!("goal.clear: {e}")))?;
            Ok(Value::Unit)
        })
    }
}

pub struct MemoryTodoSet {
    pub store: Arc<TodoStore>,
}

impl Tool for MemoryTodoSet {
    fn name(&self) -> &str {
        "memory.todo.set"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Create a concrete execution todo. Returns the todo id (UUID string) — \
             save it for memory.todo.done / memory.todo.cancel / memory.todo.delete.\n\n\
             Todos are for short, trackable work items, usually inside the current \
             plan step. Use plan.write/read/tick for the high-level ordered route \
             through a multi-step task. Do not create todos that simply mirror plan \
             steps; do not create a todo when one plan step is enough.\n\n\
             Best practice: create a todo for each discrete execution item that \
             should stay visible while you work. Keep `where` specific (file path \
             or module), `why` one sentence, `how` a brief approach, \
             `expected_result` the verification criteria. Don't create todos for \
             trivial steps — only for things the user would want to track.\n\n\
             To modify an existing todo, cancel the old one then create a new one. \
             There is no update tool.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "where": {"type": "string", "description": "Where to do it (file path, module, etc.)"},
                "why": {"type": "string", "description": "Why this needs doing"},
                "how": {"type": "string", "description": "How to do it (brief approach)"},
                "expected_result": {"type": "string", "description": "What success looks like"}
            },
            "required": ["where", "why", "how", "expected_result"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let where_ = required_string(&args, "where")?;
            let why = required_string(&args, "why")?;
            let how = required_string(&args, "how")?;
            let expected_result = required_string(&args, "expected_result")?;
            let todo = Todo {
                id: MemoryId::now(),
                where_,
                why,
                how,
                expected_result,
                status: TodoStatus::Pending,
            };
            let id = self.store.add(todo).await?;
            Ok(Value::Str(id.to_string()))
        })
    }
}

pub struct MemoryTodoDone {
    pub store: Arc<TodoStore>,
}

impl Tool for MemoryTodoDone {
    fn name(&self) -> &str {
        "memory.todo.done"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Mark a todo as done. Once done, a todo cannot be un-done. \
             The id must be the UUID string returned by memory.todo.set. \
             Returns \"ok\" on success (including if already done).",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "The UUID returned by memory.todo.set (e.g. \"019f5500-9a53-7800-8083-b608fdc4124a\")"}
            },
            "required": ["id"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let id = required_string(&args, "id")?;
            let uuid = uuid::Uuid::parse_str(&id).map_err(|e| {
                RuntimeError::ToolFailed(format!(
                    "bad todo id: {e}. The id must be the UUID returned by memory.todo.set."
                ))
            })?;
            self.store
                .set_status(&MemoryId(uuid), TodoStatus::Done)
                .await?;
            Ok(Value::Str("ok".into()))
        })
    }
}

pub struct MemoryTodoCancel {
    pub store: Arc<TodoStore>,
}

impl Tool for MemoryTodoCancel {
    fn name(&self) -> &str {
        "memory.todo.cancel"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Cancel a todo. Once cancelled, a todo cannot be re-activated. \
             The id must be the UUID string returned by memory.todo.set. \
             Returns \"ok\" on success (including if already cancelled).",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "The UUID returned by memory.todo.set"}
            },
            "required": ["id"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let id = required_string(&args, "id")?;
            let uuid = uuid::Uuid::parse_str(&id).map_err(|e| {
                RuntimeError::ToolFailed(format!(
                    "bad todo id: {e}. The id must be the UUID returned by memory.todo.set."
                ))
            })?;
            self.store
                .set_status(&MemoryId(uuid), TodoStatus::Cancelled)
                .await?;
            Ok(Value::Str("ok".into()))
        })
    }
}

pub struct MemoryTodoDelete {
    pub store: Arc<TodoStore>,
}

impl Tool for MemoryTodoDelete {
    fn name(&self) -> &str {
        "memory.todo.delete"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Permanently delete a todo. Unlike done/cancel, the todo is removed \
             entirely from the list. Use for todos created by mistake. \
             The id must be the UUID string returned by memory.todo.set.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "The UUID returned by memory.todo.set"}
            },
            "required": ["id"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let id = required_string(&args, "id")?;
            let uuid = uuid::Uuid::parse_str(&id).map_err(|e| {
                RuntimeError::ToolFailed(format!(
                    "bad todo id: {e}. The id must be the UUID returned by memory.todo.set."
                ))
            })?;
            self.store.delete(&MemoryId(uuid)).await?;
            Ok(Value::Str("ok".into()))
        })
    }
}

pub struct MemoryTodoList {
    pub store: Arc<TodoStore>,
}

impl Tool for MemoryTodoList {
    fn name(&self) -> &str {
        "memory.todo.list"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "List all todos in the current session. Returns an array of \
             {id, where, why, how, expected_result, status}. \
             status is one of: pending, done, cancelled. \
             Call this to check concrete work items before starting or resuming a \
             plan step.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }

    fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let todos = self.store.list().await?;
            let items: Vec<Value> = todos
                .into_iter()
                .map(|t| {
                    Value::Struct(vec![
                        ("id".into(), Value::Str(t.id.to_string())),
                        ("where".into(), Value::Str(t.where_)),
                        ("why".into(), Value::Str(t.why)),
                        ("how".into(), Value::Str(t.how)),
                        ("expected_result".into(), Value::Str(t.expected_result)),
                        (
                            "status".into(),
                            Value::Str(format!("{:?}", t.status).to_lowercase()),
                        ),
                    ])
                })
                .collect();
            Ok(Value::List(items))
        })
    }
}

pub struct MemoryConfess {
    pub store: Arc<ConfessionStore>,
}

impl Tool for MemoryConfess {
    fn name(&self) -> &str {
        "memory.confess"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Record a confession when the agent broke a rule. Anchors are auto-filled from \
             the current turn / flow_run / event_seq. Returns the new confession id.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "trigger": {"type": "string", "description": "What the user or watcher noticed."},
                "rule_violated": {"type": "string", "description": "Name of the red-line rule."},
                "what_i_did": {"type": "string", "description": "The concrete mistake."},
                "why": {"type": "string", "description": "The reasoning that led there."},
                "mitigation": {"type": "string", "description": "What will prevent recurrence."},
                "anchors": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Optional extra anchor strings (auto-filled ones stay)."
                }
            },
            "required": ["trigger", "rule_violated", "what_i_did", "why", "mitigation"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        let anchors = collect_anchors(&args, ctx);
        Box::pin(async move {
            let trigger = required_string(&args, "trigger")?;
            let rule_violated = required_string(&args, "rule_violated")?;
            let what_i_did = required_string(&args, "what_i_did")?;
            let why = required_string(&args, "why")?;
            let mitigation = required_string(&args, "mitigation")?;
            let confession = Confession {
                id: MemoryId::now(),
                trigger,
                rule_violated,
                what_i_did,
                why,
                mitigation,
                anchors,
                created_at: chrono::Utc::now(),
            };
            let id = self.store.append(confession).await?;
            Ok(Value::Str(id.to_string()))
        })
    }
}

fn collect_anchors(args: &ToolArgs, ctx: &ToolCtx) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(flow_run) = &ctx.flow_run_id {
        out.push(format!("flow_run:{flow_run}"));
    }
    if let Some(turn) = &ctx.turn_id {
        out.push(format!("turn:{turn}"));
    }
    if let Some(seq) = ctx.event_seq {
        out.push(format!("event_seq:{seq}"));
    }
    if let Some(Value::List(items)) = args.named("anchors") {
        for item in items {
            if let Value::Str(s) = item {
                out.push(s.clone());
            }
        }
    }
    out
}

pub struct MemorySpecStatus {
    pub store: Arc<SpecStore>,
}

impl Tool for MemorySpecStatus {
    fn name(&self) -> &str {
        "memory.spec.status"
    }
    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Return progress counters for a named spec feature. Use it to check the current phase, update count, and deviation count before continuing spec-driven work.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "feature": {"type": "string", "description": "Spec feature name to inspect."}
            },
            "required": ["feature"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let feature = required_string(&args, "feature")?;
            let st = self.store.status(&feature).await?;
            let approved = st.approved_design_revision.is_some();
            Ok(Value::Struct(vec![
                ("feature".into(), Value::Str(st.feature)),
                ("phase".into(), Value::Str(st.phase)),
                ("entry_count".into(), Value::Int(st.entry_count as i64)),
                (
                    "deviation_count".into(),
                    Value::Int(st.deviation_count as i64),
                ),
                (
                    "design_revision".into(),
                    st.design_revision.map(Value::Str).unwrap_or(Value::Unit),
                ),
                (
                    "approved_design_revision".into(),
                    st.approved_design_revision
                        .map(Value::Str)
                        .unwrap_or(Value::Unit),
                ),
                ("approved".into(), Value::Bool(approved)),
            ]))
        })
    }
}

pub struct MemorySpecRead {
    pub store: Arc<SpecStore>,
}

impl Tool for MemorySpecRead {
    fn name(&self) -> &str {
        "memory.spec.read"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some("Read stored entries for a project-scoped spec feature and optional phase.")
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "feature": {"type": "string"},
                "phase": {"type": "string"}
            },
            "required": ["feature"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let feature = required_string(&args, "feature")?;
            let phase = args.named("phase").and_then(|value| match value {
                Value::Str(text) => Some(text.as_str()),
                _ => None,
            });
            let entries = self.store.entries(&feature).await?;
            let entries = entries
                .into_iter()
                .filter(|entry| phase.is_none_or(|phase| entry.phase == phase))
                .map(|entry| {
                    Value::Struct(vec![
                        ("id".into(), Value::Str(entry.id.to_string())),
                        ("phase".into(), Value::Str(entry.phase)),
                        ("content".into(), Value::Str(entry.content)),
                    ])
                })
                .collect();
            Ok(Value::Struct(vec![
                ("entries".into(), Value::List(entries)),
                (
                    "markdown".into(),
                    match phase {
                        Some(phase) => {
                            Value::Str(self.store.phase_markdown(&feature, phase).await?)
                        }
                        None => Value::Unit,
                    },
                ),
                (
                    "file_revision".into(),
                    match phase {
                        Some(phase) => {
                            Value::Str(self.store.materialized_revision(&feature, phase).await?)
                        }
                        None => Value::Unit,
                    },
                ),
                (
                    "phase_revision".into(),
                    match phase {
                        Some(phase) => self
                            .store
                            .phase_revision(&feature, phase)
                            .await?
                            .map(Value::Str)
                            .unwrap_or(Value::Unit),
                        None => Value::Unit,
                    },
                ),
                (
                    "design_revision".into(),
                    self.store
                        .design_revision(&feature)
                        .await?
                        .map(Value::Str)
                        .unwrap_or(Value::Unit),
                ),
            ]))
        })
    }
}

pub struct MemorySpecReview {
    pub store: Arc<SpecStore>,
}

impl Tool for MemorySpecReview {
    fn name(&self) -> &str {
        "memory.spec.review"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Consume a single-use confirmation from form.ask and record the user's review of the exact current design revision. First call form.ask with confirmation.action = spec_review, then pass its confirmation_id here. Never infer approval from ordinary text.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "feature": {"type": "string"},
                "design_revision": {"type": "string"},
                "confirmation_id": {"type": "string"}
            },
            "required": ["feature", "design_revision", "confirmation_id"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let feature = required_string(&args, "feature")?;
            let revision = required_string(&args, "design_revision")?;
            let confirmation_id = required_string(&args, "confirmation_id")?;
            let forms = ctx.forms.as_ref().ok_or_else(|| {
                crate::error::RuntimeError::ToolFailed(
                    "spec.review: no session FormRegistry attached".into(),
                )
            })?;
            let receipt = forms
                .consume_confirmation(&confirmation_id)
                .ok_or_else(|| {
                    crate::error::RuntimeError::ToolFailed(
                        "spec.review: confirmation is missing, expired, or already used".into(),
                    )
                })?;
            match receipt.binding {
                crate::form::FormConfirmationBinding::SpecReview {
                    feature: bound_feature,
                    design_revision: bound_revision,
                } if bound_feature == feature && bound_revision == revision => {}
                _ => {
                    return Err(crate::error::RuntimeError::ToolFailed(
                        "spec.review: confirmation does not match feature and design revision"
                            .into(),
                    ));
                }
            }
            let approved = match receipt.submission {
                crate::form::FormSubmission::Submitted { answers } => match answers.as_slice() {
                    [crate::form::FormAnswer::Confirmed { value }] => Some(*value),
                    [crate::form::FormAnswer::Cancelled] => None,
                    _ => {
                        return Err(crate::error::RuntimeError::ToolFailed(
                            "spec.review: confirmation receipt is not a single confirm answer"
                                .into(),
                        ));
                    }
                },
                crate::form::FormSubmission::Rejected => None,
            };
            if let Some(approved) = approved {
                let record = self.store.review(&feature, &revision, approved).await?;
                Ok(Value::Struct(vec![
                    ("feature".into(), Value::Str(record.feature)),
                    ("design_revision".into(), Value::Str(record.design_revision)),
                    (
                        "decision".into(),
                        Value::Str(if record.approved {
                            "approved".into()
                        } else {
                            "rejected".into()
                        }),
                    ),
                    ("approved".into(), Value::Bool(record.approved)),
                ]))
            } else {
                Ok(Value::Struct(vec![
                    ("feature".into(), Value::Str(feature)),
                    ("design_revision".into(), Value::Str(revision)),
                    ("decision".into(), Value::Str("cancelled".into())),
                    ("approved".into(), Value::Unit),
                ]))
            }
        })
    }
}

pub struct MemorySpecMaterialize {
    pub store: Arc<SpecStore>,
}

impl Tool for MemorySpecMaterialize {
    fn name(&self) -> &str {
        "memory.spec.materialize"
    }

    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Materialize project-scoped JSONL spec state to Markdown with revision conflict protection. Set phase for a phase document; omit it for the aggregate IMPLEMENTATION.md.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "feature": {"type": "string"},
                "phase": {"type": "string", "enum": ["research", "design", "implementation", "testing", "retrospective"]},
                "expected_revision": {"type": "string"}
            },
            "required": ["feature"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let feature = required_string(&args, "feature")?;
            let expected = match args.named("expected_revision") {
                Some(Value::Str(value)) => Some(value.as_str()),
                _ => None,
            };
            let phase = match args.named("phase") {
                Some(Value::Str(value)) => Some(value.as_str()),
                _ => None,
            };
            let result = self
                .store
                .materialize_phase(&feature, phase, expected)
                .await?;
            Ok(Value::Struct(vec![
                ("path".into(), Value::Str(result.path.display().to_string())),
                ("revision".into(), Value::Str(result.revision)),
                ("changed".into(), Value::Bool(result.changed)),
            ]))
        })
    }
}

pub struct MemorySpecUpdate {
    pub store: Arc<SpecStore>,
}

impl Tool for MemorySpecUpdate {
    fn name(&self) -> &str {
        "memory.spec.update"
    }
    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Append a progress entry for a spec feature and phase. Use it to persist research, design, implementation, or verification notes as spec work advances.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "feature": {"type": "string", "description": "Spec feature name to update."},
                "phase": {"type": "string", "description": "Spec phase or section name, such as research, design, implementation, or verification."},
                "content": {"type": "string", "description": "Progress entry content to append."}
            },
            "required": ["feature", "phase", "content"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let feature = required_string(&args, "feature")?;
            let phase = required_string(&args, "phase")?;
            let content = required_string(&args, "content")?;
            let entry = self.store.update(&feature, &phase, content).await?;
            Ok(Value::Struct(vec![
                ("id".into(), Value::Str(entry.id.to_string())),
                ("feature".into(), Value::Str(entry.feature)),
                ("phase".into(), Value::Str(entry.phase)),
            ]))
        })
    }
}

pub struct MemorySpecDeviate {
    pub store: Arc<SpecStore>,
}

impl Tool for MemorySpecDeviate {
    fn name(&self) -> &str {
        "memory.spec.deviate"
    }
    fn tier(&self) -> Tier {
        Tier::One
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Record an intentional deviation from a spec section. Use it when implementation differs from the written plan and the delta plus reason must be preserved.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "feature": {"type": "string", "description": "Spec feature name that owns the deviation."},
                "section": {"type": "string", "description": "Spec section or decision being changed."},
                "delta": {"type": "string", "description": "What changed from the spec."},
                "reason": {"type": "string", "description": "Why the deviation is necessary."}
            },
            "required": ["feature", "section", "delta", "reason"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let feature = required_string(&args, "feature")?;
            let section = required_string(&args, "section")?;
            let delta = required_string(&args, "delta")?;
            let reason = required_string(&args, "reason")?;
            let dev = self.store.deviate(&feature, section, delta, reason).await?;
            Ok(Value::Struct(vec![
                ("id".into(), Value::Str(dev.id.to_string())),
                ("feature".into(), Value::Str(dev.feature)),
                ("section".into(), Value::Str(dev.section)),
            ]))
        })
    }
}

pub struct MemoryFetchConfessions {
    pub store: Arc<ConfessionStore>,
}

impl Tool for MemoryFetchConfessions {
    fn name(&self) -> &str {
        "memory.fetch_confessions"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Fetch past confession records about rule violations, optionally filtered by trigger text. Use it to recall prior mistakes and mitigations before repeating risky work.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "trigger": {"type": "string", "description": "Optional trigger substring to search for; omit to list all confession records."}
            }
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let items = match args.named("trigger") {
                Some(Value::Str(needle)) => self.store.find_by_trigger(needle).await?,
                _ => self.store.list().await?,
            };
            let list = items
                .into_iter()
                .map(|c| {
                    Value::Struct(vec![
                        ("id".into(), Value::Str(c.id.to_string())),
                        ("trigger".into(), Value::Str(c.trigger)),
                        ("rule_violated".into(), Value::Str(c.rule_violated)),
                        ("what_i_did".into(), Value::Str(c.what_i_did)),
                        ("why".into(), Value::Str(c.why)),
                        ("mitigation".into(), Value::Str(c.mitigation)),
                    ])
                })
                .collect();
            Ok(Value::List(list))
        })
    }
}

pub struct MemoryHistorySearch;

impl Tool for MemoryHistorySearch {
    fn name(&self) -> &str {
        "memory.history.search"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Full-text search the current session's chat history (or optionally every session \
             in the same project). Use it to recall past turns that fell out of your working \
             context — e.g. `plan we agreed on this morning`, `which files did we read`, \
             `error the user reported earlier`. NOT for searching source code; use fs.grep for \
             that. Params: query (FTS5 syntax, required), scope (\"session\"|\"project\", \
             default \"session\"), limit (int, default 10, max 50).",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"},
                "scope": {"type": "string", "enum": ["session", "project"], "default": "session"},
                "limit": {"type": "integer", "default": 10}
            },
            "required": ["query"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let query = required_string(&args, "query")?;
            let scope = match args.named("scope") {
                Some(Value::Str(s)) if s == "project" => HistoryScope::Project,
                _ => HistoryScope::Session,
            };
            let limit = match args.named("limit") {
                Some(Value::Int(n)) if *n > 0 => (*n as usize).min(50),
                _ => 10,
            };
            let Some(store) = ctx.history_store.clone() else {
                return Err(RuntimeError::ToolFailed(
                    "memory.history.search: no history store on context".into(),
                ));
            };
            let search_scope = match scope {
                HistoryScope::Project => crate::history_store::SearchScope::Project,
                HistoryScope::Session => crate::history_store::SearchScope::Session,
            };
            let result =
                tokio::task::spawn_blocking(move || store.search(&query, search_scope, limit))
                    .await
                    .map_err(|e| RuntimeError::ToolFailed(format!("history.search: {e}")))??;
            let hits: Vec<Value> = result
                .hits
                .into_iter()
                .map(|hit| {
                    Value::Struct(vec![
                        ("session_id".into(), Value::Str(hit.session_id)),
                        ("seq".into(), Value::Int(hit.seq as i64)),
                        ("ts".into(), Value::Str(hit.ts)),
                        ("kind".into(), Value::Str(hit.kind)),
                        ("snippet".into(), Value::Str(hit.snippet)),
                    ])
                })
                .collect();
            Ok(Value::Struct(vec![
                ("total".into(), Value::Int(result.total as i64)),
                ("hits".into(), Value::List(hits)),
            ]))
        })
    }
}

pub struct MemoryHistoryRead;

impl Tool for MemoryHistoryRead {
    fn name(&self) -> &str {
        "memory.history.read"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Paginate through past messages of a session by turn index. Prefer \
             memory.history.search first to find a hit, then call this for surrounding context. \
             Params: session_id (string, default current session's directory name), offset \
             (1-based turn index, default 1), limit (int, default 20, max 100), role_filter \
             (comma-separated: user,assistant,tool,system; default all).",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {"type": "string"},
                "offset": {"type": "integer", "default": 1},
                "limit": {"type": "integer", "default": 20},
                "role_filter": {"type": "string"}
            }
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let Some(current_dir) = ctx.session_dir.as_ref() else {
                return Err(RuntimeError::ToolFailed(
                    "memory.history.read: no session dir on context".into(),
                ));
            };
            let session_id = match args.named("session_id") {
                Some(Value::Str(sid)) if !sid.is_empty() => sid.clone(),
                _ => current_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            };
            let offset = match args.named("offset") {
                Some(Value::Int(n)) if *n >= 1 => *n as usize,
                _ => 1,
            };
            let limit = match args.named("limit") {
                Some(Value::Int(n)) if *n >= 1 => (*n as usize).min(100),
                _ => 20,
            };
            let role_filter: Option<Vec<String>> = match args.named("role_filter") {
                Some(Value::Str(s)) if !s.is_empty() => Some(
                    s.split(',')
                        .map(|t| t.trim().to_lowercase())
                        .filter(|t| !t.is_empty())
                        .collect(),
                ),
                _ => None,
            };
            let Some(store) = ctx.history_store.clone() else {
                return Err(RuntimeError::ToolFailed(
                    "memory.history.read: no history store on context".into(),
                ));
            };
            let query = crate::history_store::HistoryQuery {
                session_id,
                offset,
                limit,
                role_filter,
            };
            let page = tokio::task::spawn_blocking(move || store.read(query))
                .await
                .map_err(|e| RuntimeError::ToolFailed(format!("history.read: {e}")))??;
            let item_count = page.items.len();
            let items: Vec<Value> = page
                .items
                .into_iter()
                .map(|message| Value::Host(AtmanPayload::Message(message)))
                .collect();
            let start = offset;
            let end = if item_count == 0 {
                start
            } else {
                start + item_count - 1
            };
            let header = format!("[history: turns {start}-{end} of {}]", page.total);
            Ok(Value::Struct(vec![
                ("total".into(), Value::Int(page.total as i64)),
                ("offset".into(), Value::Int(page.offset as i64)),
                ("limit".into(), Value::Int(page.limit as i64)),
                ("header".into(), Value::Str(header)),
                ("items".into(), Value::List(items)),
            ]))
        })
    }
}

pub struct MemoryHistoryCount;

impl Tool for MemoryHistoryCount {
    fn name(&self) -> &str {
        "memory.history.count"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Return the total message count for a session. Lightweight — use this to check \
             how many messages exist before paginating with memory.history.read. \
             Params: session_id (string, default current session), role_filter \
             (comma-separated: user,assistant,tool,system; default all).",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "session_id": {"type": "string"},
                "role_filter": {"type": "string"}
            }
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let Some(current_dir) = ctx.session_dir.as_ref() else {
                return Err(RuntimeError::ToolFailed(
                    "memory.history.count: no session dir on context".into(),
                ));
            };
            let session_id = match args.named("session_id") {
                Some(Value::Str(sid)) if !sid.is_empty() => sid.clone(),
                _ => current_dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
            };
            let role_filter: Option<Vec<String>> = match args.named("role_filter") {
                Some(Value::Str(s)) if !s.is_empty() => Some(
                    s.split(',')
                        .map(|t| t.trim().to_lowercase())
                        .filter(|t| !t.is_empty())
                        .collect(),
                ),
                _ => None,
            };
            let Some(store) = ctx.history_store.clone() else {
                return Err(RuntimeError::ToolFailed(
                    "memory.history.count: no history store on context".into(),
                ));
            };
            let total = tokio::task::spawn_blocking(move || {
                let role_refs: Option<Vec<&str>> = role_filter
                    .as_ref()
                    .map(|rs| rs.iter().map(|s| s.as_str()).collect());
                store.count(&session_id, role_refs.as_deref())
            })
            .await
            .map_err(|e| RuntimeError::ToolFailed(format!("history.count: {e}")))??;
            Ok(Value::Int(total as i64))
        })
    }
}

enum HistoryScope {
    Session,
    Project,
}

fn required_string(args: &ToolArgs, name: &str) -> Result<String, RuntimeError> {
    match args.named(name) {
        Some(Value::Str(s)) => Ok(s.clone()),
        Some(other) => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
        None => Err(RuntimeError::MissingArg(name.into())),
    }
}

#[cfg(test)]
mod spec_review_tests {
    use super::*;
    use crate::form::{FormAnswer, FormConfirmationBinding, FormSubmission};

    async fn prepared_store() -> (Arc<SpecStore>, tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(SpecStore::new(dir.path().to_path_buf()));
        store
            .update("copy-selection", "research", "facts".into())
            .await
            .unwrap();
        store
            .update("copy-selection", "design", "design".into())
            .await
            .unwrap();
        let materialized = store
            .materialize_phase("copy-selection", Some("design"), Some(""))
            .await
            .unwrap();
        (store, dir, materialized.revision)
    }

    fn args(revision: &str, confirmation_id: &str) -> ToolArgs {
        ToolArgs {
            positional: Vec::new(),
            named: vec![
                ("feature".into(), Value::Str("copy-selection".into())),
                ("design_revision".into(), Value::Str(revision.to_string())),
                (
                    "confirmation_id".into(),
                    Value::Str(confirmation_id.to_string()),
                ),
            ],
        }
    }

    fn record(
        forms: &crate::session::FormRegistry,
        id: &str,
        revision: &str,
        submission: FormSubmission,
    ) {
        forms.record_confirmation(
            id.into(),
            FormConfirmationBinding::SpecReview {
                feature: "copy-selection".into(),
                design_revision: revision.into(),
            },
            submission,
        );
    }

    #[tokio::test]
    async fn review_consumes_real_confirmation_once() {
        let (store, _dir, revision) = prepared_store().await;
        let forms = Arc::new(crate::session::FormRegistry::new());
        record(
            &forms,
            "approved",
            &revision,
            FormSubmission::Submitted {
                answers: vec![FormAnswer::Confirmed { value: true }],
            },
        );
        let ctx = ToolCtx::default().with_forms(forms);
        let tool = MemorySpecReview {
            store: store.clone(),
        };
        let result = tool.call(args(&revision, "approved"), &ctx).await.unwrap();
        assert!(matches!(result.field("decision"), Some(Value::Str(value)) if value == "approved"));
        assert_eq!(
            store
                .status("copy-selection")
                .await
                .unwrap()
                .approved_design_revision,
            Some(revision.clone())
        );
        let error = tool
            .call(args(&revision, "approved"), &ctx)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("already used"));
    }

    #[tokio::test]
    async fn review_distinguishes_rejection_from_cancellation() {
        let (store, _dir, revision) = prepared_store().await;
        let forms = Arc::new(crate::session::FormRegistry::new());
        record(
            &forms,
            "rejected",
            &revision,
            FormSubmission::Submitted {
                answers: vec![FormAnswer::Confirmed { value: false }],
            },
        );
        record(&forms, "cancelled", &revision, FormSubmission::Rejected);
        let ctx = ToolCtx::default().with_forms(forms);
        let tool = MemorySpecReview {
            store: store.clone(),
        };
        let rejected = tool.call(args(&revision, "rejected"), &ctx).await.unwrap();
        assert!(
            matches!(rejected.field("decision"), Some(Value::Str(value)) if value == "rejected")
        );
        let cancelled = tool.call(args(&revision, "cancelled"), &ctx).await.unwrap();
        assert!(
            matches!(cancelled.field("decision"), Some(Value::Str(value)) if value == "cancelled")
        );
        assert_eq!(
            store
                .status("copy-selection")
                .await
                .unwrap()
                .approved_design_revision,
            None
        );
    }

    #[tokio::test]
    async fn stale_confirmation_is_consumed_and_cannot_be_replayed() {
        let (store, _dir, first) = prepared_store().await;
        let forms = Arc::new(crate::session::FormRegistry::new());
        forms.record_confirmation(
            "mismatch".into(),
            FormConfirmationBinding::SpecReview {
                feature: "different-feature".into(),
                design_revision: first.clone(),
            },
            FormSubmission::Submitted {
                answers: vec![FormAnswer::Confirmed { value: true }],
            },
        );
        record(
            &forms,
            "stale",
            &first,
            FormSubmission::Submitted {
                answers: vec![FormAnswer::Confirmed { value: true }],
            },
        );
        let ctx = ToolCtx::default().with_forms(forms);
        let tool = MemorySpecReview {
            store: store.clone(),
        };
        let mismatch = tool.call(args(&first, "mismatch"), &ctx).await.unwrap_err();
        assert!(mismatch.to_string().contains("does not match"));
        let mismatch_replay = tool.call(args(&first, "mismatch"), &ctx).await.unwrap_err();
        assert!(mismatch_replay.to_string().contains("already used"));
        store
            .update("copy-selection", "design", "changed".into())
            .await
            .unwrap();
        store
            .materialize_phase("copy-selection", Some("design"), Some(&first))
            .await
            .unwrap();
        let error = tool.call(args(&first, "stale"), &ctx).await.unwrap_err();
        assert!(error.to_string().contains("stale"));
        let replay = tool.call(args(&first, "stale"), &ctx).await.unwrap_err();
        assert!(replay.to_string().contains("already used"));
    }
}
