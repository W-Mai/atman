use crate::error::RuntimeError;
use crate::message::{Message, MessageRole};
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

pub struct SessionPush;

impl Tool for SessionPush {
    fn name(&self) -> &str {
        "session.push"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Push a Message value into the current session's message history. \
             Use after dispatch_all to persist tool results so the next \
             llm.call(context: \"session\") call can see them. The message role \
             (user/assistant/tool/system) is preserved. Returns unit.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "message": {
                    "type": "object",
                    "description": "The Message value to push (e.g. a tool_result from dispatch_all). Pass the value returned by dispatch_all directly."
                }
            },
            "required": ["message"]
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            let val = match args.named("message").or_else(|| args.positional(0).ok()) {
                Some(v) => v.clone(),
                None => {
                    return Err(RuntimeError::MissingArg("session.push: message".into()));
                }
            };
            let msgs = match val {
                Value::Message(m) => vec![m],
                Value::List(items) => items
                    .into_iter()
                    .filter_map(|v| match v {
                        Value::Message(m) => Some(m),
                        _ => None,
                    })
                    .collect(),
                other => {
                    return Err(RuntimeError::TypeMismatch {
                        expected: "message or list of message".into(),
                        actual: other.kind_name().into(),
                    });
                }
            };
            let Some(context) = ctx.context() else {
                return Err(RuntimeError::ToolFailed(
                    "session.push: no session messages handle available".into(),
                ));
            };
            let _compact_guard = context.compact_lock().lock().await;
            for msg in msgs {
                let msg = crate::tools::tool_output::maybe_truncate_tool_message_with_budget(
                    &msg,
                    ctx.output_store.as_deref(),
                    ctx.tool_output_budget,
                );
                append_message_to_context(ctx, msg)?;
            }
            Ok(Value::Unit)
        })
    }
}

pub(crate) fn append_message_to_context(
    ctx: &ToolCtx,
    mut msg: Message,
) -> Result<(), RuntimeError> {
    let Some(handle) = ctx.context().map(|context| context.messages_handle()) else {
        return Err(RuntimeError::ToolFailed(
            "session message context is unavailable".into(),
        ));
    };
    msg.ensure_part_ids();
    let stream_tx = ctx
        .stream_tx
        .clone()
        .or_else(|| ctx.session_runtime().map(|session| session.stream_tx()));
    let diagrams = if ctx.session_runtime().is_some()
        && msg.role == MessageRole::Assistant
        && msg.origin != crate::message::MessageOrigin::Internal
    {
        crate::session::extract_mermaid_blocks(&msg)
    } else {
        Vec::new()
    };
    let flow_run_id = match msg.role {
        MessageRole::Assistant | MessageRole::Tool => {
            ctx.flow_run_id.as_ref().map(|run_id| run_id.0.to_string())
        }
        MessageRole::User => ctx.message_flow_run_id().map(|run_id| run_id.0.to_string()),
        MessageRole::System => None,
    };
    let frame = if msg.origin != crate::message::MessageOrigin::Internal
        && msg.role != MessageRole::System
        && let Some(tx) = &stream_tx
    {
        let frame = match msg.role {
            MessageRole::Assistant => crate::stream::StreamFrame::AssistantMsg {
                flow_run_id,
                message: msg.clone(),
            },
            MessageRole::User | MessageRole::Tool => crate::stream::StreamFrame::ToolResultMsg {
                flow_run_id,
                message: msg.clone(),
            },
            MessageRole::System => unreachable!(),
        };
        Some((tx, frame))
    } else {
        None
    };
    let mut messages = handle.lock().unwrap();
    emit_message_event(ctx, &msg);
    messages.push(msg);
    drop(messages);
    if let Some((tx, frame)) = frame {
        let _ = tx.send(frame);
    }
    for source in diagrams {
        if let Some(sink) = ctx.context_sink() {
            sink.emit(crate::event::Event::MermaidDiagram {
                source: source.clone(),
            });
        }
        if let Some(tx) = &stream_tx {
            let _ = tx.send(crate::stream::StreamFrame::MermaidDiagram { source });
        }
    }
    Ok(())
}

pub(super) fn emit_message_event(ctx: &ToolCtx, msg: &Message) {
    use crate::event::Event;
    let Some(sink) = ctx.context_sink() else {
        return;
    };
    let turn_id = ctx.turn_id.clone().unwrap_or_else(|| msg.turn_id.clone());
    let flow_run_id = ctx.message_flow_run_id();
    let event = match msg.role {
        MessageRole::User => Event::UserMsg {
            turn_id,
            flow_run_id,
            message: msg.clone(),
        },
        MessageRole::Assistant => Event::AssistantMsg {
            turn_id,
            flow_run_id: ctx.flow_run_id.clone(),
            message: msg.clone(),
        },
        MessageRole::Tool => Event::ToolResultMsg {
            turn_id,
            flow_run_id,
            message: msg.clone(),
        },
        MessageRole::System => Event::SystemMsg {
            turn_id,
            flow_run_id: ctx.message_flow_run_id(),
            message: msg.clone(),
        },
    };
    sink.emit(event);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawned_context_does_not_rewrite_messages_outside_compaction() {
        let owner = std::sync::Arc::new(crate::context_state::ContextState::new(
            (0..100)
                .map(|index| {
                    Message::assistant_text(crate::event::TurnId::now(), format!("old-{index}"))
                })
                .collect(),
        ));
        let messages = owner.messages_handle();
        let ctx = ToolCtx::new()
            .with_history_segment(crate::tool::HistorySegment::Spawned)
            .with_context(std::sync::Arc::clone(&owner));

        append_message_to_context(
            &ctx,
            Message::assistant_text(crate::event::TurnId::now(), "new"),
        )
        .unwrap();

        assert_eq!(messages.lock().unwrap().len(), 101);
        assert_eq!(messages.lock().unwrap()[0].text_concat(), "old-0");
        assert_eq!(ctx.context().unwrap().epoch(), None);
    }

    #[test]
    fn session_push_name_and_tier() {
        let tool = SessionPush;
        assert_eq!(tool.name(), "session.push");
        assert_eq!(tool.tier(), Tier::Zero);
        assert!(tool.description().is_some());
    }

    #[test]
    fn owner_writes_preserve_identity_records_and_root_diagrams() {
        use crate::event::{ContextBase, ContextId, ContextInheritance, Event, FlowRunId, TurnId};
        use std::sync::Arc;
        for owner_kind in ["root", "root-traced", "spawned"] {
            let spawned = owner_kind == "spawned";
            let trace = crate::event::EventSink::new();
            let session = Arc::new(crate::Session::open_ephemeral());
            let run = FlowRunId::now();
            let turn = TurnId::now();
            let id = ContextId::now();
            let (tx, mut rx) = tokio::sync::broadcast::channel(8);
            let ctx = if spawned {
                let sink = session.sink().clone().with_context(id.clone());
                sink.emit(Event::ContextCreated {
                    base: None,
                    inheritance: ContextInheritance::Full,
                });
                ToolCtx::new()
                    .with_context(Arc::new(
                        crate::context_state::ContextState::new(Vec::new()),
                    ))
                    .with_history_segment(crate::tool::HistorySegment::Spawned)
                    .with_events(sink)
            } else {
                ToolCtx::new().with_session_runtime(session.clone())
            }
            .with_stream_tx(tx)
            .with_anchors(Some(turn.clone()), Some(run.clone()), None);
            let ctx = if owner_kind == "root-traced" {
                ctx.with_events(trace.clone())
            } else {
                ctx
            };
            let sink = ctx.context_sink().unwrap();
            sink.emit(Event::FlowStart {
                run_id: run.clone(),
                turn_id: Some(turn.clone()),
                flow_name: "test".into(),
                parent_run_id: None,
                parent_node_id: None,
                spawned,
            });
            let message =
                Message::assistant_text(TurnId::now(), "```mermaid\ngraph TD\n  A-->B\n```");
            append_message_to_context(&ctx, message.clone()).unwrap();
            let spec = crate::context_plan::ContextRecordSpec::new(
                "agent.rule.test",
                crate::context_plan::ContextRecordAuthority::Retrieved,
                crate::context_plan::ContextRecordRetention::Latest,
                crate::context_plan::ContextRecordBody::text("rule"),
            );
            assert_eq!(
                crate::tools::context::append_context_records(&ctx, turn.clone(), [spec.clone()])
                    .unwrap()
                    .len(),
                1
            );
            assert!(
                crate::tools::context::append_context_records(&ctx, turn.clone(), [spec])
                    .unwrap()
                    .is_empty()
            );
            let live = ctx.context().unwrap().messages().to_vec();
            assert_eq!(live[0], message);
            assert_eq!(live.len(), 2);
            let events = sink.snapshot_envelopes();
            assert!(events.iter().any(|event| matches!(&event.event,
                Event::AssistantMsg { turn_id, flow_run_id: Some(owner), message: stored }
                    if turn_id == &turn && owner == &run && stored == &message)));
            let base = if spawned {
                ContextBase::Context {
                    context_id: id,
                    through_seq: sink.published_seq(),
                }
            } else {
                ContextBase::LegacyRoot {
                    through_seq: sink.published_seq(),
                }
            };
            assert_eq!(
                crate::projection::context::replay_context(&events, &base)
                    .unwrap()
                    .window()
                    .iter()
                    .map(|(_, m)| m.clone())
                    .collect::<Vec<_>>(),
                live
            );
            assert!(
                matches!(rx.try_recv().unwrap(), crate::stream::StreamFrame::AssistantMsg { flow_run_id: Some(owner), .. } if owner == run.to_string())
            );
            if !spawned {
                assert!(matches!(
                    rx.try_recv().unwrap(),
                    crate::stream::StreamFrame::MermaidDiagram { .. }
                ));
                assert!(
                    events
                        .iter()
                        .any(|event| matches!(event.event, Event::MermaidDiagram { .. }))
                );
            } else {
                assert!(session.messages().is_empty());
                assert!(
                    !events
                        .iter()
                        .any(|event| matches!(event.event, Event::MermaidDiagram { .. }))
                );
            }
            assert!(rx.try_recv().is_err());
            assert!(trace.snapshot().is_empty());
        }
        let session = Arc::new(crate::Session::open_ephemeral());
        let ctx = ToolCtx::new().with_session_runtime(session.clone());
        let message = Message::assistant_text(TurnId::now(), "direct");
        let mut frames = session.stream_subscribe();
        append_message_to_context(&ctx, message.clone()).unwrap();
        assert!(matches!(
            frames.try_recv().unwrap(),
            crate::stream::StreamFrame::AssistantMsg { .. }
        ));
        assert!(session.sink().snapshot().iter().any(|event| matches!(event,
            Event::AssistantMsg { turn_id, message: stored, .. } if turn_id == &message.turn_id && stored == &message)));
    }

    #[tokio::test]
    async fn session_push_scopes_live_tool_result_without_scoping_session_history() {
        use crate::event::{Event, FlowRunId, TurnId};
        use crate::message::{MessageOrigin, MessagePart};

        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        let run_id = FlowRunId::now();
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(8);
        let ctx = ToolCtx::new()
            .with_anchors(Some(TurnId::now()), Some(run_id.clone()), None)
            .with_events(session.sink().clone())
            .with_session_runtime(session.clone())
            .with_stream_tx(stream_tx);
        let message = Message {
            role: MessageRole::Tool,
            parts: vec![MessagePart::ToolResult {
                tool_use_id: "tu_1".into(),
                content: "done".into(),
                is_error: false,
            }],
            turn_id: TurnId::now(),
            origin: MessageOrigin::User,
        };

        SessionPush
            .call(
                ToolArgs {
                    positional: vec![Value::Message(message)],
                    named: Vec::new(),
                },
                &ctx,
            )
            .await
            .unwrap();

        let crate::stream::StreamFrame::ToolResultMsg { flow_run_id, .. } =
            stream_rx.recv().await.unwrap()
        else {
            panic!("tool result frame");
        };
        assert_eq!(flow_run_id.as_deref(), Some(run_id.0.to_string().as_str()));
        assert!(session.sink().snapshot().iter().any(|event| {
            matches!(
                event,
                Event::ToolResultMsg {
                    flow_run_id: None,
                    ..
                }
            )
        }));
        assert!(
            session
                .messages()
                .iter()
                .any(|message| message.role == MessageRole::Tool)
        );
    }

    #[test]
    fn internal_child_system_message_is_audited_without_becoming_a_live_transcript_frame() {
        use crate::context_plan::{
            ContextRecord, ContextRecordAuthority, ContextRecordBody, ContextRecordRetention,
        };
        use crate::event::{Event, FlowRunId, TurnId};

        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        let run_id = FlowRunId::now();
        let (stream_tx, mut stream_rx) = tokio::sync::broadcast::channel(8);
        let owner = std::sync::Arc::new(crate::context_state::ContextState::new(Vec::new()));
        let ctx = ToolCtx::new()
            .with_anchors(Some(TurnId::now()), Some(run_id.clone()), None)
            .with_history_segment(crate::tool::HistorySegment::Spawned)
            .with_events(session.sink().clone())
            .with_context(owner)
            .with_stream_tx(stream_tx);
        let message = Message::context_record(
            TurnId::now(),
            ContextRecord::new(
                "handoff.parent",
                1,
                ContextRecordAuthority::Runtime,
                ContextRecordRetention::Latest,
                ContextRecordBody::text("delegated"),
            ),
        );

        append_message_to_context(&ctx, message).unwrap();

        assert!(matches!(
            stream_rx.try_recv(),
            Err(tokio::sync::broadcast::error::TryRecvError::Empty)
        ));
        assert!(session.sink().snapshot().iter().any(|event| {
            matches!(
                event,
                Event::SystemMsg {
                    flow_run_id: Some(owner),
                    ..
                } if owner == &run_id
            )
        }));
    }
}
