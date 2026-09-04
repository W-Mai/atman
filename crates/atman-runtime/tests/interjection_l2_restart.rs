mod common;

use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use atman_dsl::parse::parse_file;
use atman_runtime::event::{Event, FlowRunId, FlowStatus, NodeEvent, Observable, TurnId};
use atman_runtime::flow_authority::EffectiveAuthority;
use atman_runtime::injection::{InjectionLevel, InjectionState};
use atman_runtime::message::Message;
use atman_runtime::provider::{AssistantMessage, LlmRequest, Provider};
use atman_runtime::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolRegistry, ToolResult};
use atman_runtime::tools::agent_ctrl::{AgentSpawn, FlowEntry, FlowEvent, FlowRunStatus};
use atman_runtime::{Executor, RuntimeError, Session, Value};
use tokio::sync::{broadcast, watch};
use tokio_util::sync::CancellationToken;

struct CorrectingProvider {
    session: Weak<Session>,
    calls: Arc<Mutex<Vec<LlmRequest>>>,
    target: Option<watch::Receiver<Option<Arc<FlowEntry>>>>,
}

impl Provider for CorrectingProvider {
    fn name(&self) -> &str {
        "correcting"
    }

    fn capabilities(&self) -> atman_runtime::provider::ProviderCapabilities {
        atman_runtime::provider::ProviderCapabilities {
            prompt_cache_key: true,
            ..Default::default()
        }
    }

    fn call<'a>(&'a self, _: LlmRequest) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
        panic!("run controls must also be monitored without a UI stream subscriber")
    }

    fn call_streaming(&self, request: LlmRequest) -> Observable<AssistantMessage> {
        let index = {
            let mut calls = self.calls.lock().unwrap();
            let index = calls.len();
            calls.push(request);
            index
        };
        let session = self.session.upgrade().unwrap();
        let target = self.target.as_ref().map(|target| {
            target
                .borrow()
                .clone()
                .expect("the child must bind its entry before calling the provider")
        });
        let (tx, events) = broadcast::channel(16);
        let cancel = CancellationToken::new();
        let request_cancel = cancel.clone();
        let output = Box::pin(async move {
            let text = if index < 4 {
                format!("partial-{index}")
            } else {
                "complete".into()
            };
            tx.send(NodeEvent::LlmChunk {
                text: text.clone(),
                cumulative_tokens: 1,
            })
            .unwrap();
            if index < 4 {
                if let Some(entry) = &target {
                    session
                        .flow_registry
                        .interject(
                            &entry.handle,
                            "same correction",
                            InjectionLevel::L2CourseCorrect,
                            None,
                        )
                        .unwrap();
                } else if index % 2 == 0 {
                    session
                        .enqueue_injection_with_level(
                            "same correction",
                            InjectionLevel::L2CourseCorrect,
                            None,
                        )
                        .unwrap();
                } else {
                    session
                        .flow_registry
                        .interject(
                            "root",
                            "same correction",
                            InjectionLevel::L2CourseCorrect,
                            None,
                        )
                        .unwrap();
                }
                request_cancel.cancelled().await;
                return Err(RuntimeError::Cancelled("interrupted".into()));
            }
            tx.send(NodeEvent::LlmDone { total_tokens: 1 }).unwrap();
            Ok(AssistantMessage::text_only(Message::assistant_text(
                TurnId::now(),
                text,
            )))
        });
        Observable {
            output,
            events,
            cancel,
        }
    }
}

#[tokio::test]
async fn corrections_rebuild_canonical_context_without_a_restart_limit_or_duplicate_prompt() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "model",
            "correcting",
            100_000,
            None,
        )]))
        .await;
    for (inline, watched) in [(false, false), (false, true), (true, false), (true, true)] {
        for selection in [
            "context: \"session\"",
            "context: \"session_recent(1)\"",
            "prompt: \"explicit\"",
            "messages: [message.user(\"explicit\")]",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let session = Arc::new(Session::open(dir.path()).unwrap());
            let turn = session.begin_turn(Message::user_text(TurnId::now(), "task"));
            let calls = Arc::new(Mutex::new(Vec::new()));
            let executor = Executor::with_events(session.sink().clone());
            executor.providers.register(Arc::new(CorrectingProvider {
                session: Arc::downgrade(&session),
                calls: calls.clone(),
                target: None,
            }));
            let watch = if watched {
                "watch reply { on token(match: \"forbidden-marker\") { abort(\"unexpected token\") } }"
            } else {
                ""
            };
            let body =
                format!("reply = llm.call(model: \"model\", {selection})\n{watch}\nreturn reply");
            let source = if inline {
                format!(
                    "flow main() -> string {{ return subflow(agent) }}\nflow agent() -> string {{ {body} }}"
                )
            } else {
                format!("flow main() -> string {{ {body} }}")
            };
            let file = parse_file(&source).unwrap();
            let result = tokio::time::timeout(
                Duration::from_secs(5),
                executor.run_in_turn(
                    &file,
                    "main",
                    vec![],
                    Some(turn.clone()),
                    Some(session.clone()),
                ),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(
                !matches!(result, atman_runtime::Value::Err(_)),
                "{result:?}"
            );
            session.end_turn(&turn);
            let requests = calls.lock().unwrap().clone();
            assert_eq!(requests.len(), 5, "{selection}, inline={inline}");
            for (index, request) in requests.iter().enumerate() {
                let texts: Vec<_> = request.messages.iter().map(Message::text_concat).collect();
                assert_eq!(
                    texts
                        .iter()
                        .filter(|text| text.contains("same correction"))
                        .count(),
                    index
                );
                for partial in 0..index {
                    assert_eq!(
                        texts
                            .iter()
                            .filter(|text| **text == format!("partial-{partial}"))
                            .count(),
                        1
                    );
                }
                if selection.starts_with("prompt:") || selection.starts_with("messages:") {
                    assert_eq!(
                        texts
                            .iter()
                            .filter(|text| text.as_str() == "explicit")
                            .count(),
                        1
                    );
                }
                if index > 0 && selection == "context: \"session\"" {
                    let previous = &requests[index - 1].messages;
                    assert_eq!(&request.messages[..previous.len()], previous);
                }
            }
            let entry = session.flow_registry.lookup("root").unwrap();
            assert!(Arc::ptr_eq(&entry.context, &session.context()));
            assert!(Arc::ptr_eq(
                entry.context.compact_lock(),
                &session.compact_lock_handle()
            ));
            assert!(entry.pending_injections().is_empty());
            let events = session.sink().snapshot();
            let consumed: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    Event::UserInject {
                        injection,
                        context_message: Some(message),
                        ..
                    } if injection.state == InjectionState::Injected => Some((injection, message)),
                    _ => None,
                })
                .collect();
            assert_eq!(consumed.len(), 4);
            for (injection, message) in consumed {
                assert_eq!(injection.flow_run_id.as_ref(), Some(&entry.child_run_id));
                assert_eq!(message.turn_id, turn);
            }
            let expected = session.messages().to_vec();
            assert_eq!(
                expected
                    .iter()
                    .filter(|message| message.text_concat().starts_with("partial-"))
                    .count(),
                4
            );
            let published = session.sink().published_seq();
            let durable = session.flush_writer().await.unwrap();
            assert!(
                durable.seq >= published,
                "event writer did not persist the complete history: {durable:?}, published={published}"
            );
            let restored = Session::open_existing(dir.path(), &session.id().to_string()).unwrap();
            assert_eq!(restored.messages().to_vec(), expected);
            session.shutdown().await;
            restored.shutdown().await;
        }
    }
}

struct CaptureOwner(watch::Sender<Option<Arc<FlowEntry>>>);

impl Tool for CaptureOwner {
    fn name(&self) -> &str {
        "capture_owner"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, _: ToolArgs, ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            assert!(ctx.session_runtime().is_none());
            let entry = ctx.agent_entry.as_ref().unwrap();
            assert!(Arc::ptr_eq(ctx.context().unwrap(), &entry.context));
            assert_eq!(ctx.flow_run_id.as_ref(), Some(&entry.child_run_id));
            assert_eq!(ctx.turn_id.as_ref(), Some(&entry.turn_id));
            assert!(Arc::ptr_eq(
                ctx.context().unwrap().messages_handle(),
                entry.context.messages_handle()
            ));
            assert!(Arc::ptr_eq(
                ctx.context().unwrap().compact_lock(),
                entry.context.compact_lock()
            ));
            self.0.send_replace(Some(entry.clone()));
            Ok(Value::Unit)
        })
    }
}

#[tokio::test]
async fn spawned_corrections_preserve_child_history_without_parent_or_output_leaks() {
    let _registry =
        common::ModelRegistryGuard::acquire(common::config([common::model_for_provider(
            "model",
            "correcting",
            100_000,
            None,
        )]))
        .await;
    for (is_async, watched, inline) in [
        (false, false, false),
        (false, true, false),
        (true, false, false),
        (true, true, false),
        (false, false, true),
        (false, true, true),
        (true, false, true),
        (true, true, true),
    ] {
        for selection in ["context: \"session\"", "prompt: \"explicit\""] {
            if watched && selection.starts_with("prompt:") {
                continue;
            }
            let watch = if watched {
                "watch reply { on token(match: \"forbidden-marker\") { abort(\"unexpected token\") } }"
            } else {
                ""
            };
            let dir = tempfile::tempdir().unwrap();
            let source_path = dir.path().join("child.at");
            let call = format!(
                "reply = llm.call(model: \"model\", {selection}, cache: true)\n {watch}\n return text_concat(reply)"
            );
            let body = if inline {
                format!("return subflow(helper)\n }}\n flow helper() -> string {{ {call}")
            } else {
                call
            };
            std::fs::write(
                &source_path,
                format!(
                    "flow child() -> string {{\n capture_owner()\n session.push(message.user(\"child task\"))\n context.record(key: \"agent.rule.test\", content: \"child rule\")\n context.record(key: \"agent.rule.test\", content: \"child rule\")\n {body}\n }}"
                ),
            )
            .unwrap();
            let session = Arc::new(Session::open(dir.path()).unwrap());
            let turn = session.begin_turn(Message::user_text(TurnId::now(), "parent task"));
            let parent_messages = session.messages().to_vec();
            let parent_run = FlowRunId::now();
            let parent_identity = session
                .flow_registry
                .register_root(
                    session.id().to_string(),
                    parent_run.clone(),
                    EffectiveAuthority::root(&Default::default(), false, None),
                )
                .unwrap();
            let (owner_tx, mut owner_rx) = watch::channel(None);
            let tools = Arc::new(ToolRegistry::new());
            atman_runtime::tools::register_tier_zero(&tools);
            tools.register(Arc::new(CaptureOwner(owner_tx)));
            let calls = Arc::new(Mutex::new(Vec::new()));
            let providers = Arc::new(atman_runtime::provider::ProviderRegistry::new());
            providers.register(Arc::new(CorrectingProvider {
                session: Arc::downgrade(&session),
                calls: calls.clone(),
                target: Some(owner_rx.clone()),
            }));
            let mut ctx = ToolCtx::new()
                .with_registry(tools)
                .with_providers(providers)
                .with_flow_registry(session.flow_registry.clone())
                .with_permission_broker(atman_runtime::permission::PermissionBroker::shared(
                    session.flow_registry.clone(),
                ))
                .with_approval(Arc::new(atman_runtime::session::ApprovalRegistry::new()))
                .with_trust(Default::default())
                .with_session_id(session.id().to_string())
                .with_session_runtime(session.clone())
                .with_events(session.sink().clone())
                .with_anchors(Some(turn.clone()), Some(parent_run.clone()), None);
            ctx.flow_identity = Some(parent_identity);
            let args = ToolArgs {
                positional: vec![],
                named: vec![
                    ("flow".into(), Value::Str(source_path.display().to_string())),
                    ("async".into(), Value::Bool(is_async)),
                ],
            };
            let entry = tokio::time::timeout(Duration::from_secs(5), async {
                let result = AgentSpawn.call(args, &ctx).await.unwrap();
                if !is_async {
                    assert!(matches!(result, Value::Str(ref text) if text == "complete"));
                }
                let entry = owner_rx
                    .wait_for(Option::is_some)
                    .await
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .clone();
                let mut completed = entry.stream_tx.subscribe();
                while entry.status.lock().unwrap().is_running() {
                    if matches!(completed.recv().await.unwrap(), FlowEvent::Exited { .. }) {
                        break;
                    }
                }
                entry
            })
            .await
            .unwrap();
            assert!(matches!(
                &*entry.status.lock().unwrap(),
                FlowRunStatus::Ok { final_text, .. } if final_text == "complete"
            ));
            assert_eq!(entry.turn_id, turn);
            assert_ne!(entry.child_run_id, parent_run);
            assert!(!Arc::ptr_eq(&entry.context, &session.context()));
            assert!(!Arc::ptr_eq(
                entry.context.compact_lock(),
                &session.compact_lock_handle()
            ));
            assert_eq!(session.messages().to_vec(), parent_messages);
            assert!(entry.pending_injections().is_empty());
            let managed = selection.starts_with("context:");
            let usage_key = atman_runtime::context_plan::ContextUsageKey {
                provider: "correcting".into(),
                model: "model".into(),
                call_purpose: atman_runtime::context_plan::ContextCallPurpose::General,
                call_identity: atman_runtime::context_plan::ContextCallIdentity {
                    scope: atman_runtime::context_plan::ContextCallScope::Child,
                    session_id: Some(session.id().to_string()),
                    flow_run_id: Some(entry.child_run_id.clone()),
                },
            };
            assert_eq!(entry.context.last_usage(&usage_key).is_some(), managed);
            assert!(session.last_context_usage(&usage_key).is_none());
            assert_eq!(
                entry.output.lock().unwrap().as_str(),
                if managed {
                    "partial-0partial-1partial-2partial-3complete"
                } else {
                    ""
                }
            );
            assert_eq!(
                entry.iteration.load(std::sync::atomic::Ordering::Relaxed),
                u64::from(managed)
            );
            assert!(!session.take_streamed_flag(&turn));

            let requests = calls.lock().unwrap().clone();
            assert_eq!(requests.len(), 5);
            assert!(requests[0].prompt_cache_key.is_some());
            for (index, request) in requests.iter().enumerate() {
                assert_eq!(request.prompt_cache_key, requests[0].prompt_cache_key);
                let texts: Vec<_> = request.messages.iter().map(Message::text_concat).collect();
                assert!(!texts.iter().any(|text| text == "parent task"));
                assert_eq!(
                    texts
                        .iter()
                        .filter(|text| text.contains("same correction"))
                        .count(),
                    index
                );
                for partial in 0..index {
                    assert_eq!(
                        texts
                            .iter()
                            .filter(|text| **text == format!("partial-{partial}"))
                            .count(),
                        1
                    );
                }
                if selection.starts_with("prompt:") {
                    assert_eq!(
                        texts
                            .iter()
                            .filter(|text| text.as_str() == "explicit")
                            .count(),
                        1
                    );
                    assert!(!texts.iter().any(|text| text == "child task"));
                } else if index > 0 {
                    let previous = &requests[index - 1].messages;
                    assert_eq!(&request.messages[..previous.len()], previous);
                }
            }
            let events = session.sink().snapshot();
            let llm_calls: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    Event::LlmCall {
                        run_id,
                        context_call_identity: Some(identity),
                        ..
                    } => Some((run_id, identity)),
                    _ => None,
                })
                .collect();
            assert!(!llm_calls.is_empty());
            for (run_id, identity) in llm_calls {
                assert_eq!(identity, &usage_key.call_identity);
                assert_eq!(run_id.as_ref() != Some(&entry.child_run_id), inline);
            }
            let captured: Vec<_> = events
                .iter()
                .filter_map(|event| match event {
                    Event::UserInject {
                        injection,
                        context_message: Some(_),
                        ..
                    } if injection.state == InjectionState::Injected => Some(injection),
                    _ => None,
                })
                .collect();
            assert_eq!(captured.len(), 4);
            assert!(captured.iter().all(|injection| injection.turn_id == turn
                && injection.flow_run_id.as_ref() == Some(&entry.child_run_id)));
            let child_messages_from = |events: &[Event]| -> Vec<Message> {
                let owners: std::collections::HashSet<_> = events
                    .iter()
                    .filter_map(|event| match event {
                        Event::FlowStart {
                            run_id,
                            parent_run_id: Some(parent),
                            spawned: false,
                            ..
                        } if parent == &entry.child_run_id => Some(run_id.clone()),
                        _ => None,
                    })
                    .chain(std::iter::once(entry.child_run_id.clone()))
                    .collect();
                events
                    .iter()
                    .filter_map(|event| match event {
                        Event::UserMsg {
                            message,
                            flow_run_id,
                            ..
                        }
                        | Event::AssistantMsg {
                            message,
                            flow_run_id,
                            ..
                        }
                        | Event::ToolResultMsg {
                            message,
                            flow_run_id,
                            ..
                        }
                        | Event::SystemMsg {
                            message,
                            flow_run_id,
                            ..
                        } if flow_run_id.as_ref().is_some_and(|id| owners.contains(id)) => {
                            Some(message.clone())
                        }
                        Event::UserInject {
                            injection,
                            context_message: Some(message),
                            ..
                        } if injection.state == InjectionState::Injected
                            && injection.flow_run_id.as_ref() == Some(&entry.child_run_id) =>
                        {
                            Some(message.clone())
                        }
                        _ => None,
                    })
                    .collect()
            };
            let child_messages = child_messages_from(&events);
            assert_eq!(
                *entry.context.messages_handle().lock().unwrap(),
                child_messages
            );
            assert_eq!(
                child_messages
                    .iter()
                    .filter(|message| message.text_concat().starts_with("partial-"))
                    .count(),
                4
            );
            assert_eq!(
                child_messages
                    .iter()
                    .filter(|message| message.text_concat() == "complete")
                    .count(),
                usize::from(managed)
            );
            for key in ["handoff.parent", "agent.rule.test", "session.workspace"] {
                assert_eq!(child_messages.iter().flat_map(|message| &message.parts)
                    .filter(|part| matches!(part, atman_runtime::message::MessagePart::ContextRecord(record) if record.key() == key)).count(), 1);
            }
            assert!(events.iter().any(|event| matches!(event,
                Event::FlowEnd { run_id, status: FlowStatus::Ok, .. } if run_id == &entry.child_run_id)));
            session.flow_registry.mark_terminal(&parent_run);
            session.end_turn(&turn);
            let published = session.sink().published_seq();
            let durable = session.flush_writer().await.unwrap();
            assert!(
                durable.seq >= published,
                "event writer did not persist the complete history: {durable:?}, published={published}"
            );
            let restored = Session::restore_existing_with_context_and_trust(
                dir.path(),
                &session.id().to_string(),
                None,
                None,
                Default::default(),
            )
            .unwrap();
            assert_eq!(restored.session.messages().to_vec(), parent_messages);
            let restored_child = child_messages_from(
                &restored
                    .events
                    .iter()
                    .map(|envelope| envelope.event.clone())
                    .collect::<Vec<_>>(),
            );
            assert_eq!(restored_child, child_messages);
            session.shutdown().await;
            restored.session.shutdown().await;
        }
    }
}
