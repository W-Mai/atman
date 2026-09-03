use crate::error::RuntimeError;
use crate::eval::llm_args::LlmNodeArgs;
use crate::eval::llm_context;
use crate::tool::ToolCtx;
use crate::value::Value;

use super::ContextMode;
use super::{
    StreamCallCtx, is_context_overflow_error, parse_context_mode, rebuild_session_llm_messages,
    render_injections, session_context_record_specs, tool_context_working_directory_context,
};
use super::{append_system_context, call_and_maybe_stream};

/// Core LLM dispatch with all side effects.
/// Used as the single implementation behind `llm.call` and higher-level LLM tools.
pub async fn dispatch_llm(mut args: LlmNodeArgs, ctx: &ToolCtx) -> Value {
    let Some(model) = args.model.clone() else {
        return Value::Err(RuntimeError::MissingArg("llm.model".into()));
    };
    let mut system = args.system.clone();
    normalize_working_directory_context(&mut system, request_working_directory(ctx).as_deref());
    let input = args.input.clone();
    let retry_count = args.retry_count;
    let retry_kinds = args.retry_kinds.clone();
    let cache_prompt = args.cache_prompt;
    let context_mode = parse_context_mode(&args.context_mode);
    let watch_active = ctx
        .watch_rules
        .as_ref()
        .is_some_and(crate::streaming::WatchRules::is_active);
    let stream_tx = if matches!(context_mode, ContextMode::None) && !watch_active {
        None
    } else {
        ctx.stream_tx
            .clone()
            .or_else(|| Some(tokio::sync::broadcast::channel(64).0))
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
    let turn_id = ctx
        .turn_id
        .clone()
        .unwrap_or_else(crate::event::TurnId::now);
    if ctx.session_runtime.is_some()
        || (matches!(ctx.history_segment, crate::tool::HistorySegment::Spawned)
            && ctx.session_messages_handle.is_some())
    {
        append_system_context(
            &mut system,
            vec![crate::context_plan::CONTEXT_RECORD_INSTRUCTIONS.to_string()],
        );
        sync_runtime_context_records(ctx, &turn_id).await;
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
    let compaction_budget = crate::compaction::CompactionBudgetContext {
        fixed_input_tokens: Some(crate::context_plan::estimate_fixed_input_tokens(
            &system,
            &tool_specs,
        )),
    };
    if !matches!(context_mode, ContextMode::None)
        && !has_messages_override
        && let Some(session) = ctx.session_runtime.as_ref()
    {
        crate::compaction::start_auto_compact_with_budget(
            session.clone(),
            model.clone(),
            providers_reg.clone(),
            compaction_budget,
        )
        .await;
    }
    let uses_managed_context = !matches!(context_mode, ContextMode::None) && !has_messages_override;
    let uses_spawned_context = uses_managed_context
        && ctx.session_runtime.is_none()
        && matches!(ctx.history_segment, crate::tool::HistorySegment::Spawned)
        && ctx.session_messages_handle.is_some();
    let mut compact_guard = if uses_managed_context {
        if let Some(session) = ctx.session_runtime.as_ref() {
            Some(session.acquire_compact_lock().await)
        } else if uses_spawned_context {
            match ctx.compact_lock_handle.as_ref() {
                Some(lock) => Some(lock.lock().await),
                None => None,
            }
        } else {
            None
        }
    } else {
        None
    };
    if uses_spawned_context
        && compact_guard.is_some()
        && let Some(messages) = ctx.session_messages_handle.as_ref()
        && let Some(result) = crate::compaction::maybe_auto_compact_handle_locked(
            messages,
            &model,
            &providers_reg,
            compaction_budget,
            false,
        )
        .await
    {
        record_spawned_compaction(ctx, &result);
    }
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
    let mut rewrite_used = false;
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
    let retry_base_messages = final_messages.clone();
    let can_rebuild_from_managed_context = uses_managed_context
        && (ctx.session_runtime.is_some() || (uses_spawned_context && compact_guard.is_some()));
    let mut compact_after_overflow_used = false;
    let mut saw_context_overflow = false;
    let mut last_err: Option<RuntimeError> = None;
    let retry_kinds_ref = retry_kinds.as_ref();
    let model_info = crate::model_registry::model_info(&model);
    if model_info.context_budget == 0 {
        return Value::Err(RuntimeError::ToolFailed(format!(
            "model `{model}` is not registered in config.toml — add a [models.{model}] section with context_budget before using it"
        )));
    }
    let has_images = final_messages.iter().any(|message| {
        message
            .parts
            .iter()
            .any(|part| matches!(part, crate::message::MessagePart::Image { .. }))
    });
    if has_images
        && !model_info.capabilities.input_modalities.is_empty()
        && !model_info
            .capabilities
            .input_modalities
            .contains(&crate::provider::InputModality::Image)
    {
        return Value::Err(RuntimeError::AttachmentError {
            reason: format!("model `{model}` does not advertise image input support"),
        });
    }
    let requested_reasoning = args
        .reasoning
        .clone()
        .unwrap_or_else(|| model_info.reasoning.clone());
    let mut reasoning =
        match crate::model_registry::resolve_reasoning_for_model(&model, &requested_reasoning) {
            Ok(reasoning) => reasoning,
            Err(error) => {
                let error =
                    RuntimeError::ToolFailed(format!("model `{model}` reasoning config: {error}"));
                if let Some(sink) = ctx.events.as_ref() {
                    sink.emit(crate::event::Event::LlmCall {
                        model: model.clone(),
                        provider: provider.name().to_string(),
                        context_plan_id: None,
                        context_epoch: None,
                        context_tokens: None,
                        usage_source: None,
                        context_call_purpose: Some(args.call_purpose),
                        context_call_identity: Some(
                            crate::context_plan::ContextCallIdentity::from_tool_context(ctx),
                        ),
                        context_cache: None,
                        assistant_tool_batch_width: None,
                        usage: crate::provider::TokenUsage::default(),
                        wallclock_ms: 0,
                        ttft_ms: None,
                        tokens_per_second: None,
                        status: crate::event::LlmCallStatus::Errored {
                            message: error.to_string(),
                        },
                        run_id: ctx.flow_run_id.clone(),
                        node_id: ctx.current_node_id.clone(),
                    });
                }
                send_llm_diagnostic(
                    ctx,
                    crate::notify::NotifyLevel::Error,
                    format!("LLM call failed: {error}"),
                );
                return Value::Err(error);
            }
        };
    let mut signature_retries: u32 = 0;
    'llm_attempts: loop {
        for attempt in 0..=retry_count {
            let mut sanitized_messages =
                crate::message::normalize_tool_pairs_for_model(&final_messages);
            for message in &mut sanitized_messages {
                for part in &mut message.parts {
                    if let crate::message::MessagePart::Image { source } = part
                        && matches!(source.detail, crate::provider::ImageDetail::Auto)
                    {
                        source.detail = model_info.image_detail;
                    }
                }
            }
            let req = crate::provider::LlmRequest {
                model: api_model.clone(),
                messages: sanitized_messages,
                system: system.clone(),
                input: input.clone(),
                schema: None,
                cache_prompt,
                prompt_cache_key: None,
                tools: tool_specs.clone(),
                reasoning: reasoning.clone(),
                stall_timeout_secs,
            };
            let context_epoch = ctx
                .session_runtime
                .as_ref()
                .and_then(|session| session.context_epoch())
                .or_else(|| ctx.context_epoch_seed());
            let context_plan = crate::context_plan::ModelContextPlan::for_provider_call(
                req,
                args.call_purpose,
                crate::context_plan::ContextCallIdentity::from_tool_context(ctx),
                provider.name(),
                provider.capabilities(),
                context_epoch.as_deref(),
            );
            let context_plan_id = context_plan.id().clone();
            let context_epoch = context_plan.cache_plan().epoch.clone();
            let context_tokens = context_plan.token_lanes().clone();
            let context_call_purpose = context_plan.call_purpose();
            let context_call_identity = context_plan.call_identity().clone();
            let context_prefix = provider
                .context_prefix(context_plan.request())
                .or_else(|_| {
                    crate::context_plan::ContextPrefixSnapshot::provider_neutral(
                        context_plan.request(),
                    )
                })
                .expect("provider-neutral context prefix serialization");
            let context_cache = if let Some(session) = ctx.session_runtime.as_ref() {
                session.observe_context_prefix(
                    provider.name(),
                    &api_model,
                    context_call_purpose,
                    context_call_identity.clone(),
                    context_prefix,
                )
            } else if let Some(tracker) = ctx.context_prefix_tracker.as_ref() {
                tracker
                    .lock()
                    .expect("context prefix lock poisoned")
                    .observe(
                        context_call_purpose,
                        context_call_identity.clone(),
                        provider.name(),
                        &api_model,
                        context_prefix,
                    )
            } else {
                context_prefix.initial_observation()
            };
            let estimated_input = context_plan.estimated_input_tokens();
            let start = std::time::Instant::now();
            let outcome = call_and_maybe_stream(
                provider.as_ref(),
                context_plan.into_request(),
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
            let provider_usage = outcome
                .as_ref()
                .map(|am| am.token_usage.clone())
                .unwrap_or_default();
            let estimated_output = outcome
                .as_ref()
                .map(|am| crate::provider::estimate_tokens(&am.text_concat()))
                .unwrap_or(0);
            let assistant_tool_batch_width = outcome.as_ref().ok().map(|message| {
                message
                    .message
                    .parts
                    .iter()
                    .filter(|part| matches!(part, crate::message::MessagePart::ToolUse { .. }))
                    .count() as u64
            });
            let response_error = outcome.as_ref().ok().and_then(|message| {
                message.message.parts.is_empty().then(|| {
                    RuntimeError::ToolFailed("LLM returned an empty assistant message".into())
                })
            });
            let (usage, usage_source) = crate::context_plan::reconcile_token_usage(
                &provider_usage,
                estimated_input,
                estimated_output,
            );
            let status = match (&outcome, &response_error) {
                (_, Some(error)) => crate::event::LlmCallStatus::Errored {
                    message: error.to_string(),
                },
                (Ok(_), None) => crate::event::LlmCallStatus::Ok,
                (Err(error), None) => crate::event::LlmCallStatus::Errored {
                    message: error.to_string(),
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
                    context_plan_id: Some(context_plan_id.clone()),
                    context_epoch: Some(context_epoch),
                    context_tokens: Some(context_tokens),
                    usage_source: Some(usage_source),
                    context_call_purpose: Some(context_call_purpose),
                    context_call_identity: Some(context_call_identity.clone()),
                    context_cache: Some(context_cache),
                    assistant_tool_batch_width,
                    usage: usage.clone(),
                    wallclock_ms: elapsed_ms,
                    ttft_ms,
                    tokens_per_second: tps,
                    status,
                    run_id: ctx.flow_run_id.clone(),
                    node_id: ctx.current_node_id.clone(),
                });
            }
            if let Some(tx) = stream_tx.as_ref() {
                let _ = tx.send(crate::stream::StreamFrame::LlmCallStats {
                    model: model.clone(),
                    provider: provider.name().to_string(),
                    context_call_purpose,
                    context_call_scope: context_call_identity.scope,
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
                session.record_context_plan_call(
                    provider.name(),
                    &model,
                    context_plan_id,
                    context_call_purpose,
                    context_call_identity,
                    &usage,
                    ttft_ms,
                    tps,
                );
            }
            let outcome = match (outcome, response_error) {
                (Ok(_), Some(error)) => Err(error),
                (outcome, _) => outcome,
            };
            match outcome {
                Ok(am) => {
                    if let Some(exposures) = ctx.model_tool_exposures.as_ref() {
                        exposures.register_response(
                            ctx.flow_run_id.as_ref(),
                            &am.message,
                            tool_specs.iter().map(|tool| tool.name.as_str()),
                        );
                    }
                    if last_err.is_some() {
                        send_llm_diagnostic(
                            ctx,
                            crate::notify::NotifyLevel::Success,
                            "LLM call recovered after retry".into(),
                        );
                    }
                    if let Some(session) = ctx.session_runtime.as_ref()
                        && !matches!(context_mode, ContextMode::None)
                    {
                        if !has_messages_override {
                            drop(compact_guard.take());
                        }
                        let _append_compact_guard = session.acquire_compact_lock().await;
                        session.append_message(am.message.clone(), ctx.flow_run_id.clone());
                        drop(_append_compact_guard);
                        crate::compaction::start_auto_compact_with_budget(
                            session.clone(),
                            model.clone(),
                            providers_reg.clone(),
                            compaction_budget,
                        )
                        .await;
                    } else if uses_spawned_context {
                        if let Err(error) = crate::tools::session::append_message_to_context(
                            ctx,
                            am.message.clone(),
                        ) {
                            return Value::Err(error);
                        }
                        drop(compact_guard.take());
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
                    send_llm_diagnostic(
                        ctx,
                        crate::notify::NotifyLevel::Warn,
                        format!("LLM call failed: {e}"),
                    );
                    if is_context_overflow_error(&e)
                        && can_rebuild_from_managed_context
                        && !compact_after_overflow_used
                    {
                        compact_after_overflow_used = true;
                        saw_context_overflow = true;
                        send_llm_diagnostic(
                            ctx,
                            crate::notify::NotifyLevel::Info,
                            "context overflow — compacting and retrying".into(),
                        );
                        if let Some(session) = ctx.session_runtime.as_ref() {
                            session.request_manual_compact();
                            drop(compact_guard.take());
                            crate::compaction::maybe_auto_compact_with_budget(
                                session,
                                &model,
                                &providers_reg,
                                compaction_budget,
                            )
                            .await;
                            final_messages = rebuild_session_llm_messages(
                                session,
                                context_mode,
                                &turn_id,
                                Some(prompt.as_str()),
                                &retry_base_messages[session_messages_len..],
                            );
                            last_err = Some(e);
                            continue 'llm_attempts;
                        }
                        if let Some(messages) = ctx.session_messages_handle.as_ref()
                            && let Some(result) =
                                crate::compaction::maybe_auto_compact_handle_locked(
                                    messages,
                                    &model,
                                    &providers_reg,
                                    compaction_budget,
                                    true,
                                )
                                .await
                        {
                            record_spawned_compaction(ctx, &result);
                            match llm_context::build_llm_context(
                                &args,
                                context_mode,
                                None,
                                Some(messages),
                                &turn_id,
                                ctx.events.as_ref(),
                                ctx.flow_run_id.as_ref(),
                            ) {
                                Ok(context) => final_messages = context.messages,
                                Err(value) => return value,
                            }
                            last_err = Some(e);
                            continue 'llm_attempts;
                        }
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
                        send_llm_diagnostic(
                            ctx,
                            crate::notify::NotifyLevel::Warn,
                            "safety auto-rewrite triggered".into(),
                        );
                        last_err = Some(e);
                        continue;
                    }
                    if matches!(e, RuntimeError::ThinkingSignatureMissing) {
                        signature_retries += 1;
                        if signature_retries < 3 {
                            if let Some(tx) = stream_tx.as_ref() {
                                let _ = tx.send(crate::stream::StreamFrame::LlmRetry {
                                    run_id: ctx.flow_run_id.as_ref().map(ToString::to_string),
                                });
                            }
                            crate::notify!(
                                info,
                                location = Inline,
                                "thinking signature missing — retry {signature_retries}/3"
                            );
                            last_err = Some(e);
                            continue 'llm_attempts;
                        }
                        if !matches!(reasoning, crate::provider::ReasoningSelection::Auto { .. }) {
                            last_err = Some(e);
                            break 'llm_attempts;
                        }
                        reasoning = crate::provider::ReasoningSelection::Disabled;
                        if let Some(tx) = stream_tx.as_ref() {
                            let _ = tx.send(crate::stream::StreamFrame::LlmRetry {
                                run_id: ctx.flow_run_id.as_ref().map(ToString::to_string),
                            });
                        }
                        crate::notify!(
                            warn,
                            location = Inline,
                            "thinking signature missing after 3 retries; disabling thinking…"
                        );
                        send_llm_diagnostic(
                            ctx,
                            crate::notify::NotifyLevel::Warn,
                            "thinking disabled after 3 signature failures".into(),
                        );
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
                            send_llm_diagnostic(
                                ctx,
                                crate::notify::NotifyLevel::Warn,
                                format!(
                                    "{} — retry {}/{} in {}s…",
                                    e,
                                    attempt + 1,
                                    retry_count,
                                    delay_ms / 1000
                                ),
                            );
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
        send_llm_diagnostic(
            ctx,
            crate::notify::NotifyLevel::Warn,
            "LLM call failed; using configured fallback value".into(),
        );
        return fb;
    }
    if let Some(session) = ctx.session_runtime.as_ref()
        && !saw_context_overflow
    {
        crate::compaction::start_auto_compact_with_budget(
            session.clone(),
            model.clone(),
            providers_reg,
            compaction_budget,
        )
        .await;
    }
    let error = last_err.unwrap_or(RuntimeError::ToolFailed("llm failed".into()));
    send_llm_diagnostic(
        ctx,
        crate::notify::NotifyLevel::Error,
        format!("LLM call failed: {error}"),
    );
    Value::Err(error)
}

fn record_spawned_compaction(ctx: &ToolCtx, result: &crate::compaction::HandleAutoCompactResult) {
    ctx.advance_context_epoch();
    let Some(flow_run_id) = ctx.message_flow_run_id() else {
        return;
    };
    let Some(sink) = ctx.events.as_ref() else {
        return;
    };
    let session_id = ctx
        .session_id
        .clone()
        .or_else(|| ctx.turn_id.as_ref().map(ToString::to_string))
        .unwrap_or_default();
    sink.emit(crate::event::Event::ContextCompact {
        session_id: session_id.clone(),
        flow_run_id: Some(flow_run_id.clone()),
        before_tokens: result.before_tokens,
        after_tokens: result.after_tokens,
        compacted_range_start: result.compacted_start as u64,
        compacted_range_end: result.compacted_end as u64,
        summary_text: Some(result.summary.clone()),
        replacement_msg_seq: None,
    });
    sink.emit(crate::event::Event::CompactionSummary {
        session_id: session_id.clone(),
        flow_run_id: Some(flow_run_id.clone()),
        range_start: result.compacted_start as u64,
        range_end: result.compacted_end as u64,
        compacted_count: result.compacted_count,
        before_tokens: result.before_tokens,
        after_tokens: result.after_tokens,
        summary: result.summary.clone(),
    });
    sink.emit(crate::event::Event::Checkpoint {
        session_id,
        flow_run_id: Some(flow_run_id),
        messages: result.checkpoint_messages.clone(),
        window_tokens: result.after_tokens,
    });
}

fn send_llm_diagnostic(ctx: &ToolCtx, level: crate::notify::NotifyLevel, message: String) {
    let tx = ctx
        .session_runtime
        .as_ref()
        .map(|session| session.stream_tx())
        .or_else(|| ctx.stream_tx.clone());
    let Some(tx) = tx else {
        return;
    };
    let run = ctx
        .flow_run_id
        .as_ref()
        .map(ToString::to_string)
        .or_else(|| ctx.turn_id.as_ref().map(ToString::to_string))
        .unwrap_or_else(|| "session".into());
    let node = ctx.current_node_id.as_deref().unwrap_or("llm");
    let _ = tx.send(crate::stream::StreamFrame::Notification(
        crate::stream::NotificationFrame {
            run_id: ctx.flow_run_id.as_ref().map(ToString::to_string),
            level,
            location: crate::notify::NotifyLocation::Inline,
            lifecycle: crate::notify::NotifyLifecycle::UntilReplaced,
            stack: crate::notify::NotifyStack::Replace {
                key: format!("llm-call:{run}:{node}"),
            },
            message,
        },
    ));
}

fn request_working_directory(ctx: &ToolCtx) -> Option<std::path::PathBuf> {
    ctx.session_runtime
        .as_ref()
        .and_then(|session| session.meta())
        .and_then(|meta| meta.start_path.or(meta.project_root))
        .or_else(|| ctx.resolve_cwd(None).ok())
}

async fn sync_runtime_context_records(ctx: &ToolCtx, turn_id: &crate::event::TurnId) {
    if let Some(session) = ctx.session_runtime.as_ref() {
        session
            .append_context_records(turn_id.clone(), session_context_record_specs(session).await);
        return;
    }
    if !matches!(ctx.history_segment, crate::tool::HistorySegment::Spawned) {
        return;
    }
    let Some(messages) = ctx.session_messages_handle.as_ref() else {
        return;
    };
    let workspace = tool_context_working_directory_context(ctx);
    let spec = crate::context_plan::ContextRecordSpec::new(
        "session.workspace",
        crate::context_plan::ContextRecordAuthority::Runtime,
        crate::context_plan::ContextRecordRetention::Latest,
        workspace.map_or_else(
            crate::context_plan::ContextRecordBody::tombstone,
            crate::context_plan::ContextRecordBody::text,
        ),
    );
    let mut messages = messages.lock().unwrap();
    let records = crate::context_plan::compile_context_records(&messages, [spec]);
    messages.extend(
        records
            .into_iter()
            .map(|record| crate::message::Message::context_record(turn_id.clone(), record)),
    );
}

fn normalize_working_directory_context(system: &mut Option<String>, cwd: Option<&std::path::Path>) {
    let Some(system) = system.as_mut() else {
        return;
    };
    *system = system.replace("[working directory]\n{pwd}\n\n", "");
    if let Some(cwd) = cwd {
        *system = system.replace("{pwd}", &cwd.display().to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixed_wire_prefix_counts_only_system_and_tool_definitions() {
        let system = Some("stable instructions".to_string());
        let tools = vec![crate::tool::ToolSpec {
            name: "read".into(),
            description: Some("Read a file".into()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}}
            }),
        }];

        let expected = crate::provider::estimate_tokens(system.as_deref().unwrap())
            + crate::provider::estimate_tokens(&serde_json::to_string(&tools).unwrap());
        assert_eq!(
            crate::context_plan::estimate_fixed_input_tokens(&system, &tools),
            expected
        );
    }

    #[test]
    fn legacy_working_directory_block_is_removed_before_runtime_context() {
        let cwd = std::path::Path::new("/tmp/atman-context-test");
        let mut system =
            Some("stable\n\n[working directory]\n{pwd}\n\nrule path: {pwd}".to_string());

        normalize_working_directory_context(&mut system, Some(cwd));

        let system = system.unwrap();
        assert!(!system.contains("[working directory]"));
        assert!(!system.contains("{pwd}"));
        assert!(system.contains("rule path: /tmp/atman-context-test"));
    }

    #[tokio::test]
    async fn llm_diagnostic_uses_session_stream_without_content_streaming() {
        let session = std::sync::Arc::new(crate::session::Session::open_ephemeral());
        let mut rx = session.stream_subscribe();
        let expected_run_id = crate::event::FlowRunId::now();
        let ctx = ToolCtx::new()
            .with_session_runtime(session)
            .with_anchors(None, Some(expected_run_id.clone()), None)
            .with_current_node(Some("7.iter[0].0".into()));

        send_llm_diagnostic(
            &ctx,
            crate::notify::NotifyLevel::Error,
            "LLM call failed: invalid request".into(),
        );

        let frame = rx.recv().await.unwrap();
        assert!(matches!(
            frame,
            crate::stream::StreamFrame::Notification(crate::stream::NotificationFrame {
                run_id: Some(run_id),
                level: crate::notify::NotifyLevel::Error,
                location: crate::notify::NotifyLocation::Inline,
                stack: crate::notify::NotifyStack::Replace { key },
                message,
                ..
            }) if run_id == expected_run_id.to_string()
                && key.ends_with(":7.iter[0].0")
                && message.contains("invalid request")
        ));
    }

    #[test]
    fn spawned_compaction_audit_is_owned_and_checkpointed() {
        let sink = crate::event::EventSink::new();
        let expected_run_id = crate::event::FlowRunId::now();
        let checkpoint = vec![crate::message::Message::system_compact_summary(
            crate::event::TurnId::now(),
            "summary",
            0,
            1,
            2,
        )];
        let mut ctx = ToolCtx::new()
            .with_history_segment(crate::tool::HistorySegment::Spawned)
            .with_anchors(None, Some(expected_run_id.clone()), None)
            .with_events(sink.clone());
        ctx.context_epoch_handle = Some(std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)));
        let result = crate::compaction::HandleAutoCompactResult {
            before_tokens: 100,
            after_tokens: 10,
            compacted_start: 0,
            compacted_end: 1,
            compacted_count: 2,
            summary: "summary".into(),
            checkpoint_messages: checkpoint.clone(),
        };

        record_spawned_compaction(&ctx, &result);

        let events = sink.snapshot();
        assert_eq!(events.len(), 3);
        assert!(events.iter().all(|event| match event {
            crate::event::Event::ContextCompact { flow_run_id, .. }
            | crate::event::Event::CompactionSummary { flow_run_id, .. }
            | crate::event::Event::Checkpoint { flow_run_id, .. } => {
                flow_run_id.as_ref() == Some(&expected_run_id)
            }
            _ => false,
        }));
        assert!(matches!(
            &events[2],
            crate::event::Event::Checkpoint { messages, .. } if messages == &checkpoint
        ));
        assert_eq!(ctx.context_epoch_seed().as_deref(), Some("generation:1"));
    }

    #[tokio::test]
    async fn spawned_workspace_context_is_append_only_per_local_history() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        let messages = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let context = |path: &std::path::Path| {
            ToolCtx::new()
                .with_history_segment(crate::tool::HistorySegment::Spawned)
                .with_session_messages_handle(std::sync::Arc::clone(&messages))
                .with_workspace(crate::git_workspace::WorkspaceBinding {
                    workspace_id: path.display().to_string(),
                    path: path.to_path_buf(),
                    repository_root: path.to_path_buf(),
                    branch: None,
                })
        };
        let turn_id = crate::event::TurnId::now();

        sync_runtime_context_records(&context(first.path()), &turn_id).await;
        sync_runtime_context_records(&context(first.path()), &turn_id).await;
        sync_runtime_context_records(&context(second.path()), &turn_id).await;

        let messages = messages.lock().unwrap();
        let records: Vec<_> = messages
            .iter()
            .flat_map(|message| &message.parts)
            .filter_map(|part| match part {
                crate::message::MessagePart::ContextRecord(record) => Some(record),
                _ => None,
            })
            .collect();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0].revision(), 1);
        assert_eq!(records[1].revision(), 2);
        assert!(
            records[1]
                .render_for_model()
                .contains(&second.path().display().to_string())
        );
    }
}
