use crate::error::RuntimeError;
use crate::eval::ContextMode;
use crate::eval::llm_args::LlmNodeArgs;
use crate::value::Value;

pub struct LlmContext {
    pub messages: Vec<crate::message::Message>,
    pub budget_text: String,
    pub session_messages_len: usize,
}

pub fn build_llm_context(
    args: &LlmNodeArgs,
    context_mode: ContextMode,
    session: Option<&std::sync::Arc<crate::session::Session>>,
    turn_id: &crate::event::TurnId,
    events: Option<&crate::event::EventSink>,
    flow_run_id: Option<&crate::event::FlowRunId>,
) -> Result<LlmContext, Value> {
    let (final_messages, prompt_for_budget) = if let Some(msgs) = args.messages_override.clone() {
        let budget_text = msgs.last().map(|m| m.text_concat()).unwrap_or_default();
        (msgs, budget_text)
    } else if !matches!(context_mode, ContextMode::None)
        && let Some(session) = session
    {
        let mut history = match context_mode {
            ContextMode::Session => session.messages().to_vec(),
            ContextMode::SessionRecent(n) => {
                let all = session.messages();
                let start = all.len().saturating_sub(n);
                all[start..].to_vec()
            }
            ContextMode::None => Vec::new(),
        };
        let budget_text = args.prompt.clone().unwrap_or_default();
        if let Some(p) = args.prompt.clone()
            && !p.is_empty()
        {
            history.push(crate::message::Message::user_text(turn_id.clone(), p));
        }
        (history, budget_text)
    } else {
        let Some(mut prompt_text) = args.prompt.clone() else {
            return Err(Value::Err(RuntimeError::MissingArg(
                "llm node: either `prompt:` or `messages:` required".into(),
            )));
        };
        if let Some(budget) = args.context_budget {
            let (truncated, stat) = super::truncate_prompt_to_budget_tracked(prompt_text, budget);
            prompt_text = truncated;
            if let (Some(sink), Some(stat)) = (events, stat) {
                sink.emit(crate::event::Event::ContextTruncated {
                    turn_id: Some(turn_id.clone()),
                    flow_run_id: flow_run_id.cloned(),
                    original_chars: stat.original_chars as u64,
                    result_chars: stat.result_chars as u64,
                    dropped_chars: stat.dropped_chars as u64,
                    budget_tokens: stat.budget_tokens,
                });
            }
        }
        let user_msg = crate::message::Message::user_text(turn_id.clone(), prompt_text.clone());
        (vec![user_msg], prompt_text)
    };
    let session_messages_len = final_messages.len();

    Ok(LlmContext {
        messages: final_messages,
        budget_text: prompt_for_budget,
        session_messages_len,
    })
}
