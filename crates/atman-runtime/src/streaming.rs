use std::sync::Arc;

pub(crate) fn handle_pending_injections(
    entry: &Arc<crate::tools::agent_ctrl::FlowEntry>,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(), crate::error::RuntimeError> {
    let pending: Vec<_> = entry.pending_injections.lock().unwrap().drain(..).collect();
    for inj in &pending {
        if matches!(inj.level, crate::injection::InjectionLevel::L1Nudge) {
            entry
                .messages
                .lock()
                .unwrap()
                .push(crate::message::Message::user_text(
                    crate::event::TurnId::now(),
                    format!("[interjection] {}", inj.text),
                ));
        }
    }
    if let Some(inj) = pending
        .iter()
        .find(|i| !matches!(i.level, crate::injection::InjectionLevel::L1Nudge))
    {
        cancel.cancel();
        return Err(match inj.level {
            crate::injection::InjectionLevel::L4HardStop => {
                crate::error::RuntimeError::Cancelled(format!("hard stop: {}", inj.text))
            }
            crate::injection::InjectionLevel::L3Redirect => {
                if let Some(target) = &inj.redirect_target {
                    crate::error::RuntimeError::Redirect(target.clone())
                } else {
                    crate::error::RuntimeError::Cancelled(format!(
                        "redirect (no target): {}",
                        inj.text
                    ))
                }
            }
            crate::injection::InjectionLevel::L2CourseCorrect => {
                entry
                    .messages
                    .lock()
                    .unwrap()
                    .push(crate::message::Message::user_text(
                        crate::event::TurnId::now(),
                        format!("[course correct] {}", inj.text),
                    ));
                crate::error::RuntimeError::L2Restart {
                    correction_text: inj.text.clone(),
                    partial_output: entry.output.lock().unwrap().clone(),
                    partial_tokens: 0,
                }
            }
            crate::injection::InjectionLevel::L1Nudge => unreachable!(),
        });
    }
    Ok(())
}

pub(crate) fn emit_stream_event(
    event: crate::event::NodeEvent,
    model_name: &str,
    run_id: Option<&str>,
    primary: Option<&tokio::sync::broadcast::Sender<crate::stream::StreamFrame>>,
    fallback: Option<&tokio::sync::broadcast::Sender<crate::stream::StreamFrame>>,
) {
    match event {
        crate::event::NodeEvent::LlmChunk {
            text,
            cumulative_tokens: _,
        } => {
            emit_frame(
                primary,
                fallback,
                crate::stream::StreamFrame::LlmChunk {
                    text: text.clone(),
                    model: model_name.to_string(),
                    run_id: run_id.map(std::borrow::ToOwned::to_owned),
                },
            );
        }
        crate::event::NodeEvent::ThinkingChunk { text } => {
            emit_frame(
                primary,
                fallback,
                crate::stream::StreamFrame::ThinkingChunk {
                    text: text.clone(),
                    run_id: run_id.map(std::borrow::ToOwned::to_owned),
                },
            );
        }
        crate::event::NodeEvent::LlmDone { total_tokens } => {
            emit_frame(
                primary,
                fallback,
                crate::stream::StreamFrame::LlmDone {
                    total_tokens,
                    run_id: run_id.map(std::borrow::ToOwned::to_owned),
                },
            );
        }
        _ => {}
    }
}

fn emit_frame(
    primary: Option<&tokio::sync::broadcast::Sender<crate::stream::StreamFrame>>,
    fallback: Option<&tokio::sync::broadcast::Sender<crate::stream::StreamFrame>>,
    frame: crate::stream::StreamFrame,
) {
    if let Some(tx) = primary {
        let _ = tx.send(frame);
    } else if let Some(tx) = fallback {
        let _ = tx.send(frame);
    }
}
