use crate::error::RuntimeError;
use crate::eval::llm_args::LlmNodeArgs;
use crate::eval::llm_context;
use crate::tool::ToolCtx;
use crate::value::Value;

use super::ContextMode;
use super::{
    StreamCallCtx, is_context_overflow_error, parse_context_mode, rebuild_session_llm_messages,
    render_injections, sanitize_tool_pairs, session_system_context,
};
use super::{append_system_context, call_and_maybe_stream, input_with_cache_for_window};

struct ProviderInheritedSummarizer<'a> {
    provider: &'a dyn crate::provider::Provider,
    api_model: &'a str,
    stall_timeout_secs: u64,
}

impl super::inherited_context::InheritedContextSummarizer for ProviderInheritedSummarizer<'_> {
    fn summarize<'a>(
        &'a self,
        source: &'a [crate::message::Message],
        max_tokens: u64,
    ) -> crate::tool::BoxFut<'a, Option<String>> {
        Box::pin(async move {
            let mut source_text = serde_json::to_string(source).ok()?;
            let input_limit = max_tokens.saturating_mul(8).max(512) as usize;
            if source_text.len() > input_limit {
                source_text.truncate(input_limit);
            }
            let prompt = format!(
                "Summarize this prior turn's assistant/system/tool output for context inheritance. Preserve file paths, symbols, commands, errors, decisions, completed work, and unfinished state. Omit hidden reasoning and repetitive tool output. Stay under {max_tokens} tokens.\n\n{source_text}"
            );
            let request = crate::provider::LlmRequest {
                model: self.api_model.to_string(),
                messages: vec![crate::message::Message::user_text(
                    crate::event::TurnId::now(),
                    prompt,
                )],
                system: None,
                input: Value::Unit,
                schema: None,
                cache_prompt: true,
                tools: Vec::new(),
                thinking_enabled: false,
                stall_timeout_secs: self.stall_timeout_secs,
            };
            self.provider
                .call(request)
                .await
                .ok()
                .map(|am| am.text_concat())
        })
    }
}

/// Core LLM dispatch with all side effects.
/// Used as the single implementation behind `llm.call` and higher-level LLM tools.
pub async fn dispatch_llm(mut args: LlmNodeArgs, ctx: &ToolCtx) -> Value {
    let Some(model) = args.model.clone() else {
        return Value::Err(RuntimeError::MissingArg("llm.model".into()));
    };
    let mut system = args.system.clone();
    // Substitute {pwd} placeholder with session working directory
    if let Some(s) = system.as_mut()
        && let Some(session) = ctx.session_runtime.as_ref()
    {
        if let Some(meta) = session.meta() {
            if let Some(cwd) = meta.start_path.as_deref().or(meta.project_root.as_deref()) {
                *s = s.replace("{pwd}", &cwd.display().to_string());
            }
        }
    }
    let input = args.input.clone();
    let retry_count = args.retry_count;
    let retry_kinds = args.retry_kinds.clone();
    let cache_prompt = args.cache_prompt;
    let context_mode = parse_context_mode(&args.context_mode);
    let stream_tx = if matches!(context_mode, ContextMode::None) {
        None
    } else {
        ctx.stream_tx.clone()
    };
    let tool_specs = args.tool_specs.clone();
    let stall_timeout_secs = args.stall_timeout_secs;
    if args.messages_override.is_some() && args.prompt.is_some() {
        return Value::Err(RuntimeError::ToolFailed(
            "llm: cannot specify both `messages:` and `prompt:` (pick one)".into(),
        ));
    }
    if !matches!(context_mode, ContextMode::None) && args.messages_override.is_some() {
        return Value::Err(RuntimeError::ToolFailed(
            "llm: cannot specify both `messages:` and `context:` (pick one)".into(),
        ));
    }
    let providers_reg = ctx
        .providers
        .as_ref()
        .map(|p| (**p).clone())
        .unwrap_or_default();
    let Some(provider) = providers_reg.resolve(&model) else {
        if let Some(entry) = crate::model_registry::model_entry(&model)
            && let Some(ref pname) = entry.provider
            && !crate::model_registry::is_provider_enabled(pname)
        {
            return Value::Err(RuntimeError::ToolFailed(format!(
                "provider `{pname}` is disabled — enable it in Provider Manager"
            )));
        }
        return Value::Err(RuntimeError::ToolFailed(format!(
            "no provider registered for model `{model}`"
        )));
    };
    let api_model = crate::model_registry::model_entry(&model)
        .and_then(|e| {
            if e.model.is_empty() {
                None
            } else {
                Some(e.model)
            }
        })
        .unwrap_or_else(|| model.clone());
    let has_messages_override = args.messages_override.is_some();
    if !matches!(context_mode, ContextMode::None)
        && !has_messages_override
        && let Some(session) = ctx.session_runtime.as_ref()
    {
        crate::compaction::start_auto_compact(
            session.clone(),
            model.clone(),
            providers_reg.clone(),
        )
        .await;
    }
    if let Some(budget) = args.context_budget {
        if let Some(p) = args.prompt.as_mut() {
            let (truncated, stat) =
                super::truncate_prompt_to_budget_tracked(std::mem::take(p), budget);
            *p = truncated;
            if let (Some(sink), Some(stat)) = (ctx.events.as_ref(), stat) {
                sink.emit(crate::event::Event::ContextTruncated {
                    turn_id: ctx.turn_id.clone(),
                    flow_run_id: ctx.flow_run_id.clone(),
                    original_chars: stat.original_chars as u64,
                    result_chars: stat.result_chars as u64,
                    dropped_chars: stat.dropped_chars as u64,
                    budget_tokens: stat.budget_tokens,
                });
            }
        }
    }
    let mut compact_guard = if !matches!(context_mode, ContextMode::None)
        && !has_messages_override
        && let Some(session) = ctx.session_runtime.as_ref()
    {
        Some(session.acquire_compact_lock().await)
    } else {
        None
    };
    let turn_id = ctx
        .turn_id
        .clone()
        .unwrap_or_else(crate::event::TurnId::now);
    let llm_context = match llm_context::build_llm_context(
        &args,
        context_mode,
        ctx.session_runtime.as_ref(),
        ctx.session_messages_handle.as_ref(),
        &turn_id,
        ctx.events.as_ref(),
        ctx.flow_run_id.as_ref(),
    ) {
        Ok(context) => context,
        Err(v) => return v,
    };
    let mut final_messages = llm_context.messages;
    let prompt_for_budget = llm_context.budget_text;
    let session_messages_len = llm_context.session_messages_len;
    if let Some(session) = ctx.session_runtime.as_ref()
        && let Some(l3_or_l2) = session.peek_pending_l2_or_higher(&turn_id)
        && matches!(l3_or_l2.level, crate::injection::InjectionLevel::L3Redirect)
        && let Some(target) = &l3_or_l2.redirect_target
    {
        session.mark_injection_consumed(&l3_or_l2.id);
        return Value::Err(RuntimeError::Redirect(target.clone()));
    }
    if let Some(session) = ctx.session_runtime.as_ref() {
        let injections = session.drain_injections(&turn_id);
        let renderable: Vec<crate::injection::Injection> = injections
            .into_iter()
            .filter(|i| {
                matches!(
                    i.level,
                    crate::injection::InjectionLevel::L1Nudge
                        | crate::injection::InjectionLevel::L2CourseCorrect
                )
            })
            .collect();
        if !renderable.is_empty() {
            let rendered = render_injections(&renderable);
            final_messages.push(crate::message::Message::user_text(
                turn_id.clone(),
                rendered,
            ));
        }
    }
    let prompt = prompt_for_budget;
    let retry_extra_messages = if session_messages_len <= final_messages.len() {
        final_messages[session_messages_len..].to_vec()
    } else {
        Vec::new()
    };
    let mut rewrite_used = false;
    if let Some(session) = ctx.session_runtime.as_ref() {
        append_system_context(&mut system, session_system_context(session).await);
    }
    let model_info = crate::model_registry::model_info(&model);
    let inherited_summarizer = ProviderInheritedSummarizer {
        provider: provider.as_ref(),
        api_model: &api_model,
        stall_timeout_secs,
    };
    if matches!(context_mode, ContextMode::Session) && !has_messages_override {
        let source_messages = final_messages.clone();
        final_messages = super::inherited_context::project_messages(
            &source_messages,
            system.as_deref(),
            &tool_specs,
            &input,
            model_info.context_budget,
            model_info.max_output_tokens,
            &turn_id,
        );
        super::inherited_context::summarize_projected_messages(
            &mut final_messages,
            &source_messages,
            &api_model,
            &inherited_summarizer,
        )
        .await;
        if !super::inherited_context::fits_request(
            &final_messages,
            system.as_deref(),
            &tool_specs,
            &input,
            model_info.context_budget,
            model_info.max_output_tokens,
        ) {
            return Value::Err(RuntimeError::ToolFailed(
                "llm: current session context cannot fit within the model budget".into(),
            ));
        }
    }
    if let Some(safety) = ctx.safety.as_ref()
        && safety.enabled
    {
        let scan_text = final_messages
            .last()
            .map(|m| m.text_concat())
            .unwrap_or_else(|| prompt.clone());
        let verdict = match safety.classifier.scan(&scan_text).await {
            Ok(v) => v,
            Err(e) => {
                crate::notify!(warn, "safety scan skipped: {e}");
                crate::safety::ScanVerdict::Pass
            }
        };
        if !verdict.is_pass()
            && let Some(sink) = ctx.events.as_ref()
        {
            let action = match (&verdict, safety.mode) {
                (crate::safety::ScanVerdict::Deny(_), crate::safety::SafetyMode::Deny) => "blocked",
                _ => "warned",
            };
            for category in verdict.categories() {
                sink.emit(crate::event::Event::ContentFilterHit {
                    turn_id: Some(turn_id.clone()),
                    flow_run_id: ctx.flow_run_id.clone(),
                    provider: safety.classifier.kind().to_string(),
                    model: model.clone(),
                    category: category.clone(),
                    action: action.to_string(),
                });
            }
        }
        if verdict.is_deny() && safety.mode == crate::safety::SafetyMode::Deny {
            let cats = verdict.categories().join(", ");
            return Value::Err(RuntimeError::ToolFailed(format!(
                "safety: content_filter blocked prompt (categories: {cats})"
            )));
        }
    }
    let can_rebuild_from_session = !matches!(context_mode, ContextMode::None)
        && !has_messages_override
        && ctx.session_runtime.is_some();
    let mut compact_after_overflow_used = false;
    let mut saw_context_overflow = false;
    let mut last_err: Option<RuntimeError> = None;
    let retry_kinds_ref = retry_kinds.as_ref();
    if model_info.context_budget == 0 {
        return Value::Err(RuntimeError::ToolFailed(format!(
            "model `{model}` is not registered in config.toml — add a [models.{model}] section with context_budget before using it"
        )));
    }
    let mut thinking_enabled = model_info.thinking_enabled();
    let mut signature_retries: u32 = 0;
    'llm_attempts: loop {
        for attempt in 0..=retry_count {
            let sanitized_messages = sanitize_tool_pairs(final_messages.clone());
            let req = crate::provider::LlmRequest {
                model: api_model.clone(),
                messages: sanitized_messages,
                system: system.clone(),
                input: input.clone(),
                schema: None,
                cache_prompt,
                tools: tool_specs.clone(),
                thinking_enabled,
                stall_timeout_secs,
            };
            let start = std::time::Instant::now();
            let outcome = call_and_maybe_stream(
                provider.as_ref(),
                req,
                StreamCallCtx {
                    session: ctx.session_runtime.as_deref(),
                    stream_tx: stream_tx.clone(),
                    flow_run_id: ctx.flow_run_id.as_ref(),
                    agent_entry: ctx.agent_entry.as_ref(),
                    event_sink: ctx.events.as_ref(),
                    turn_id: ctx.turn_id.clone(),
                },
                ctx.watch_rules.clone(),
            )
            .await;
            let elapsed_ms = start.elapsed().as_millis() as u64;
            let usage = match &outcome {
                Ok(am) => crate::provider::TokenUsage {
                    input: am
                        .token_usage
                        .input
                        .max(crate::provider::estimate_tokens(&prompt)),
                    cached_input: am.token_usage.cached_input,
                    output: am
                        .token_usage
                        .output
                        .max(crate::provider::estimate_tokens(&am.text_concat())),
                    cache_write: am.token_usage.cache_write,
                    ..Default::default()
                },
                Err(_) => crate::provider::TokenUsage {
                    input: crate::provider::estimate_tokens(&prompt),
                    ..Default::default()
                },
            };
            let status = match &outcome {
                Ok(_) => crate::event::LlmCallStatus::Ok,
                Err(e) => crate::event::LlmCallStatus::Errored {
                    message: e.to_string(),
                },
            };
            let (ttft_ms, tps) = match &outcome {
                Ok(am) => (
                    am.timing.ttft_ms,
                    am.timing.tokens_per_second(am.token_usage.output),
                ),
                Err(_) => (None, None),
            };
            if let Some(sink) = ctx.events.as_ref() {
                sink.emit(crate::event::Event::LlmCall {
                    model: model.clone(),
                    provider: provider.name().to_string(),
                    usage: usage.clone(),
                    wallclock_ms: elapsed_ms,
                    ttft_ms,
                    tokens_per_second: tps,
                    status,
                    run_id: ctx.flow_run_id.clone(),
                    node_id: ctx.current_node_id.clone(),
                });
            }
            let input_with_cache = input_with_cache_for_window(&usage);
            if let Some(tx) = stream_tx.as_ref() {
                let _ = tx.send(crate::stream::StreamFrame::LlmCallStats {
                    model: model.clone(),
                    input_tokens: usage.input,
                    output_tokens: usage.output,
                    cache_read: usage.cached_input,
                    cache_write: usage.cache_write,
                    ttft_ms: ttft_ms.unwrap_or(0),
                    tokens_per_second: tps.unwrap_or(0.0),
                    wallclock_ms: elapsed_ms,
                    run_id: ctx.flow_run_id.as_ref().map(|r| r.0.to_string()),
                    node_id: ctx.current_node_id.clone(),
                });
            }
            if let Some(session) = ctx.session_runtime.as_ref()
                && !matches!(context_mode, ContextMode::None)
            {
                session.record_llm_call(
                    &model,
                    input_with_cache,
                    usage.output,
                    usage.cached_input,
                    usage.cache_write,
                    ttft_ms,
                    tps,
                );
            }
            match outcome {
                Ok(am) => {
                    if let Some(session) = ctx.session_runtime.as_ref()
                        && !matches!(context_mode, ContextMode::None)
                    {
                        if !has_messages_override {
                            drop(compact_guard.take());
                        }
                        let _append_compact_guard = session.acquire_compact_lock().await;
                        session.append_message(am.message.clone(), None);
                        drop(_append_compact_guard);
                        crate::compaction::start_auto_compact(
                            session.clone(),
                            model.clone(),
                            providers_reg.clone(),
                        )
                        .await;
                    }
                    if !matches!(context_mode, ContextMode::None) {
                        return Value::Message(am.message.clone());
                    }
                    return crate::provider::assistant_message_to_value(&am);
                }
                Err(e) => {
                    if matches!(
                        e,
                        RuntimeError::Cancelled(_)
                            | RuntimeError::L2Restart { .. }
                            | RuntimeError::Aborted(_)
                    ) {
                        return Value::Err(e);
                    }
                    if is_context_overflow_error(&e)
                        && can_rebuild_from_session
                        && !compact_after_overflow_used
                    {
                        compact_after_overflow_used = true;
                        saw_context_overflow = true;
                        if let Some(tx) = stream_tx.as_ref() {
                            let _ = tx.send(crate::stream::StreamFrame::Note(
                                "context overflow — compacting and retrying".into(),
                            ));
                        }
                        let session = ctx
                            .session_runtime
                            .as_ref()
                            .expect("checked by can_rebuild_from_session");
                        session.request_manual_compact();
                        drop(compact_guard.take());
                        crate::compaction::maybe_auto_compact(session, &model, &providers_reg)
                            .await;
                        final_messages = rebuild_session_llm_messages(
                            session,
                            context_mode,
                            &turn_id,
                            Some(prompt.as_str()),
                            &retry_extra_messages,
                        );
                        if matches!(context_mode, ContextMode::Session) {
                            let source_messages = final_messages.clone();
                            final_messages = super::inherited_context::project_messages(
                                &source_messages,
                                system.as_deref(),
                                &tool_specs,
                                &input,
                                model_info.context_budget,
                                model_info.max_output_tokens,
                                &turn_id,
                            );
                            super::inherited_context::summarize_projected_messages(
                                &mut final_messages,
                                &source_messages,
                                &api_model,
                                &inherited_summarizer,
                            )
                            .await;
                            if !super::inherited_context::fits_request(
                                &final_messages,
                                system.as_deref(),
                                &tool_specs,
                                &input,
                                model_info.context_budget,
                                model_info.max_output_tokens,
                            ) {
                                return Value::Err(RuntimeError::ToolFailed(
                                    "llm: compacted session context cannot fit within the model budget".into(),
                                ));
                            }
                        }
                        last_err = Some(e);
                        continue 'llm_attempts;
                    }
                    if is_context_overflow_error(&e) {
                        last_err = Some(e);
                        break;
                    }
                    if !rewrite_used
                        && let Some(safety) = ctx.safety.as_ref()
                        && safety.enabled
                        && safety.auto_rewrite
                        && matches!(e.kind(), crate::error::ErrorKind::ContentFilter)
                    {
                        rewrite_used = true;
                        if let Some(last) = final_messages.last_mut()
                            && let Some(part) = last.parts.iter_mut().find_map(|p| match p {
                                crate::message::MessagePart::Text { text } => Some(text),
                                _ => None,
                            })
                        {
                            *part = format!(
                                "Please rewrite the following in a neutral, safety-compliant way and answer it:\n{part}"
                            );
                        }
                        if let Some(sink) = ctx.events.as_ref() {
                            sink.emit(crate::event::Event::ContentFilterHit {
                                turn_id: Some(turn_id.clone()),
                                flow_run_id: ctx.flow_run_id.clone(),
                                provider: provider.name().to_string(),
                                model: model.clone(),
                                category: "auto_rewrite".to_string(),
                                action: "rewritten".to_string(),
                            });
                        }
                        if let Some(tx) = stream_tx.as_ref() {
                            let _ = tx.send(crate::stream::StreamFrame::Note(
                                "safety auto-rewrite triggered".into(),
                            ));
                        }
                        last_err = Some(e);
                        continue;
                    }
                    if matches!(e, RuntimeError::ThinkingSignatureMissing) {
                        signature_retries += 1;
                        if signature_retries < 3 {
                            if let Some(tx) = stream_tx.as_ref() {
                                let _ = tx.send(crate::stream::StreamFrame::LlmRetry);
                            }
                            crate::notify!(
                                info,
                                location = Inline,
                                "thinking signature missing — retry {signature_retries}/3"
                            );
                            last_err = Some(e);
                            continue 'llm_attempts;
                        }
                        thinking_enabled = false;
                        if let Some(tx) = stream_tx.as_ref() {
                            let _ = tx.send(crate::stream::StreamFrame::LlmRetry);
                        }
                        crate::notify!(
                            warn,
                            location = Inline,
                            "thinking signature missing after 3 retries; disabling thinking…"
                        );
                        if let Some(tx) = stream_tx.as_ref() {
                            let _ = tx.send(crate::stream::StreamFrame::Note(
                                "thinking disabled after 3 signature failures".into(),
                            ));
                        }
                        last_err = Some(e);
                        continue 'llm_attempts;
                    }
                    if thinking_enabled
                        && matches!(e.kind(), crate::error::ErrorKind::InvalidRequest)
                    {
                        thinking_enabled = false;
                        if let Some(tx) = stream_tx.as_ref() {
                            let _ = tx.send(crate::stream::StreamFrame::LlmRetry);
                        }
                        crate::notify!(
                            warn,
                            location = Inline,
                            "thinking mode disabled due to API error; retrying…"
                        );
                        if let Some(tx) = stream_tx.as_ref() {
                            let _ = tx.send(crate::stream::StreamFrame::Note(
                                "thinking disabled due to API error".into(),
                            ));
                        }
                        last_err = Some(e);
                        continue 'llm_attempts;
                    }
                    if attempt < retry_count {
                        let kind = e.kind();
                        let should_retry = match &retry_kinds_ref {
                            Some(allowed) => allowed.contains(&kind),
                            None => true,
                        };
                        if !should_retry {
                            last_err = Some(e);
                            break;
                        }
                        if matches!(
                            kind,
                            crate::error::ErrorKind::RateLimit
                                | crate::error::ErrorKind::Timeout
                                | crate::error::ErrorKind::ProviderDown
                                | crate::error::ErrorKind::Transient
                        ) {
                            let delay_ms = 1000u64 << attempt;
                            if let Some(tx) = stream_tx.as_ref() {
                                let _ = tx.send(crate::stream::StreamFrame::Note(format!(
                                    "{} — retry {}/{} in {}s…",
                                    e,
                                    attempt + 1,
                                    retry_count,
                                    delay_ms / 1000
                                )));
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                        }
                        last_err = Some(e);
                    } else {
                        last_err = Some(e);
                    }
                }
            }
        }
        break;
    }
    if let Some(fb) = args.fallback_value.clone() {
        return fb;
    }
    if let Some(session) = ctx.session_runtime.as_ref()
        && !saw_context_overflow
    {
        crate::compaction::start_auto_compact(session.clone(), model.clone(), providers_reg).await;
    }
    if let Some(tx) = stream_tx.as_ref() {
        let _ = tx.send(crate::stream::StreamFrame::Note(format!(
            "LLM call failed: {}",
            last_err
                .as_ref()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "unknown error".into())
        )));
    }
    Value::Err(last_err.unwrap_or(RuntimeError::ToolFailed("llm failed".into())))
}
