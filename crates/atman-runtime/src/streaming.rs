use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::broadcast::Sender;
use tokio_util::sync::CancellationToken;

use crate::error::RuntimeError;
use crate::event::{EventSink, FlowRunId, NodeEvent, TurnId};
use crate::provider::{AssistantMessage, CallTiming, LlmRequest, Provider};
use crate::session::Session;
use crate::stream::StreamFrame;
use crate::tools::agent_ctrl::{FlowEntry, FlowEvent};

pub(crate) struct LlmStream<'a> {
    provider: &'a dyn Provider,
    req: LlmRequest,
    session: Option<&'a Session>,
    entry: Option<&'a Arc<FlowEntry>>,
    stream_tx: Option<Sender<StreamFrame>>,
    frame_tx: Option<Sender<StreamFrame>>,
    flow_cancel: Option<CancellationToken>,
    watch_rules: Option<WatchRules>,
    event_sink: Option<&'a EventSink>,
    turn_id: Option<TurnId>,
    flow_run_id: Option<FlowRunId>,
    correction: Option<String>,
    prior_partial: Option<String>,
    restart_count: u32,
    first_token_at: Option<Instant>,
    request_start: Instant,
}

impl<'a> LlmStream<'a> {
    pub(crate) fn new(provider: &'a dyn Provider, req: LlmRequest) -> Self {
        Self {
            provider,
            req,
            session: None,
            entry: None,
            stream_tx: None,
            frame_tx: None,
            flow_cancel: None,
            watch_rules: None,
            event_sink: None,
            turn_id: None,
            flow_run_id: None,
            correction: None,
            prior_partial: None,
            restart_count: 0,
            first_token_at: None,
            request_start: Instant::now(),
        }
    }

    pub(crate) fn with_session(mut self, session: &'a Session) -> Self {
        self.flow_cancel = Some(session.flow_cancel_token());
        self.session = Some(session);
        self
    }

    pub(crate) fn with_entry(mut self, entry: &'a Arc<FlowEntry>) -> Self {
        self.frame_tx = Some(entry.frame_tx.clone());
        self.entry = Some(entry);
        self
    }

    pub(crate) fn with_stream_tx(mut self, tx: Option<Sender<StreamFrame>>) -> Self {
        self.stream_tx = tx;
        self
    }

    pub(crate) fn with_frame_tx(mut self, tx: Option<Sender<StreamFrame>>) -> Self {
        self.frame_tx = tx;
        self
    }

    pub(crate) fn with_watch_rules(mut self, rules: WatchRules) -> Self {
        self.watch_rules = Some(rules);
        self
    }

    pub(crate) fn with_event_sink(mut self, sink: Option<&'a EventSink>) -> Self {
        self.event_sink = sink;
        self
    }

    pub(crate) fn with_turn_id(mut self, turn_id: Option<TurnId>) -> Self {
        self.turn_id = turn_id;
        self
    }

    pub(crate) fn with_flow_run_id(mut self, flow_run_id: Option<FlowRunId>) -> Self {
        self.flow_run_id = flow_run_id;
        self
    }

    pub(crate) async fn run(&mut self) -> Result<AssistantMessage, RuntimeError> {
        if self.stream_tx.is_none() && self.frame_tx.is_none() {
            return self
                .provider
                .call(self.rebuild_req())
                .await
                .map(|am| self.finalize_timing(am));
        }
        loop {
            self.first_token_at = None;
            self.request_start = Instant::now();
            match self.single_attempt().await {
                Ok(am) => return Ok(self.finalize_timing(am)),
                Err(RuntimeError::L2Restart {
                    correction_text,
                    partial_output,
                    partial_tokens,
                }) => {
                    if self.restart_count < 3 {
                        self.emit_partial_call(partial_tokens);
                        self.restart_count += 1;
                        self.correction = Some(correction_text);
                        self.prior_partial = Some(partial_output);
                        continue;
                    }
                    return Err(RuntimeError::Cancelled("l2 restart exhausted".into()));
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn single_attempt(&mut self) -> Result<AssistantMessage, RuntimeError> {
        let req = self.rebuild_req();
        let model_name = req.model.clone();
        let run_id = self.flow_run_id.as_ref().map(|r| r.0.to_string());
        let stall_secs = req.stall_timeout_secs;
        let obs = self.provider.call_streaming(req);
        let cancel = obs.cancel.clone();
        let flow_cancel = self.flow_cancel.clone().unwrap_or_default();
        if let Some(entry) = self.entry {
            handle_pending_injections(entry, &cancel)?;
        }
        let mut events = obs.events;
        let output = obs.output;
        tokio::pin!(output);

        let stall_active = stall_secs > 0;
        let stall_dur = std::time::Duration::from_secs(stall_secs);
        let stall_sleep = tokio::time::sleep(stall_dur);
        tokio::pin!(stall_sleep);

        let elapsed_active = self
            .watch_rules
            .as_ref()
            .and_then(|r| r.elapsed_ms_gt)
            .is_some();
        let elapsed_deadline_ms = self
            .watch_rules
            .as_ref()
            .and_then(|r| r.elapsed_ms_gt)
            .unwrap_or(u64::MAX / 2);
        let elapsed_sleep = tokio::time::sleep(tokio::time::Duration::from_millis(
            elapsed_deadline_ms.saturating_add(1),
        ));
        tokio::pin!(elapsed_sleep);

        let mut state = StreamMonitor::new(self);
        let mut events_closed = false;
        let final_result = loop {
            tokio::select! {
                biased;
                _ = flow_cancel.cancelled(), if self.flow_cancel.is_some() => {
                    cancel.cancel();
                    break Err(RuntimeError::Cancelled("flow cancelled by user".into()));
                }
                _ = async {
                    if let Some(entry) = self.entry {
                        entry.injection_notify.notified().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                }, if self.entry.is_some() => {
                    if let Some(entry) = self.entry
                        && let Err(err) = handle_pending_injections(entry, &cancel)
                    {
                        break Err(err);
                    }
                }
                ev = async {
                    if events_closed {
                        std::future::pending().await
                    } else {
                        events.recv().await
                    }
                }, if !events_closed => {
                    match ev {
                        Ok(NodeEvent::LlmChunk { text, cumulative_tokens }) => {
                            self.on_chunk(&model_name, run_id.as_deref(), &text, cumulative_tokens);
                            state.on_chunk(&text, cumulative_tokens, self.request_start, self.watch_rules.as_ref(), &cancel);
                            if stall_active {
                                stall_sleep.as_mut().reset(tokio::time::Instant::now() + stall_dur);
                            }
                        }
                        Ok(NodeEvent::ThinkingChunk { text }) => {
                            self.on_thinking(&model_name, run_id.as_deref(), text);
                        }
                        Ok(NodeEvent::LlmDone { total_tokens }) => {
                            self.on_done(&model_name, run_id.as_deref(), total_tokens);
                            state.on_done(total_tokens, self.request_start, self.watch_rules.as_ref());
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => events_closed = true,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    }
                }
                _ = &mut elapsed_sleep, if elapsed_active && state.abort_reason.is_none() => {
                    state.abort_reason = Some(format!("elapsed > {elapsed_deadline_ms}ms"));
                    cancel.cancel();
                    break Err(RuntimeError::Cancelled("elapsed".into()));
                }
                _ = &mut stall_sleep, if stall_active => {
                    cancel.cancel();
                    break Err(RuntimeError::ToolFailed(format!("llm stall timeout after {}s", stall_secs)));
                }
                result = &mut output => break result,
            }
        };

        while let Ok(ev) = events.try_recv() {
            match ev {
                NodeEvent::LlmChunk {
                    text,
                    cumulative_tokens,
                } => {
                    self.on_chunk(&model_name, run_id.as_deref(), &text, cumulative_tokens);
                    state.on_chunk(
                        &text,
                        cumulative_tokens,
                        self.request_start,
                        self.watch_rules.as_ref(),
                        &cancel,
                    );
                }
                NodeEvent::ThinkingChunk { text } => {
                    self.on_thinking(&model_name, run_id.as_deref(), text)
                }
                NodeEvent::LlmDone { total_tokens } => {
                    self.on_done(&model_name, run_id.as_deref(), total_tokens);
                    state.on_done(total_tokens, self.request_start, self.watch_rules.as_ref());
                }
                _ => {}
            }
        }

        if let Some(reason) = state.abort_reason {
            return Err(RuntimeError::Aborted(reason));
        }
        final_result.map_err(|e| merge_restart_error(e, state.text_captured, state.tokens_seen))
    }

    fn rebuild_req(&self) -> LlmRequest {
        let mut req = self.req.clone();
        if let Some(partial) = &self.prior_partial {
            req.messages.push(crate::message::Message::assistant_text(
                self.turn_id.clone().unwrap_or_else(TurnId::now),
                format!("[partial output before user correction]\n{partial}"),
            ));
        }
        if let Some(correction) = &self.correction {
            let last_prompt = req
                .messages
                .iter()
                .rev()
                .find(|m| matches!(m.role, crate::message::MessageRole::User))
                .map(|m| m.text_concat())
                .unwrap_or_else(|| req.input.to_json().to_string());
            req.messages.push(crate::message::Message::user_text(
                self.turn_id.clone().unwrap_or_else(TurnId::now),
                format!("<user_correction>{correction}</user_correction>\n\n{last_prompt}"),
            ));
        }
        req
    }

    fn on_chunk(
        &mut self,
        model_name: &str,
        run_id: Option<&str>,
        text: &str,
        cumulative_tokens: u64,
    ) {
        if let Some(session) = self.session {
            session.mark_streamed();
        }
        self.mark_first_token();
        emit_stream_event(
            NodeEvent::LlmChunk {
                text: text.to_string(),
                cumulative_tokens,
            },
            model_name,
            run_id,
            self.stream_tx.as_ref(),
            self.frame_tx.as_ref(),
        );
        if let Some(entry) = self.entry {
            entry.output.lock().unwrap().push_str(text);
        }
    }

    fn on_thinking(&mut self, model_name: &str, run_id: Option<&str>, text: String) {
        self.mark_first_token();
        emit_stream_event(
            NodeEvent::ThinkingChunk { text },
            model_name,
            run_id,
            self.stream_tx.as_ref(),
            self.frame_tx.as_ref(),
        );
    }

    fn on_done(&mut self, model_name: &str, run_id: Option<&str>, total_tokens: u64) {
        emit_stream_event(
            NodeEvent::LlmDone { total_tokens },
            model_name,
            run_id,
            self.stream_tx.as_ref(),
            self.frame_tx.as_ref(),
        );
        if let Some(entry) = self.entry {
            entry
                .iteration
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let out = entry.output.lock().unwrap().clone();
            let _ = entry.stream_tx.send(FlowEvent::AssistantDone { text: out });
        }
    }

    fn mark_first_token(&mut self) {
        if self.first_token_at.is_none() {
            self.first_token_at = Some(Instant::now());
        }
    }

    fn finalize_timing(&self, mut am: AssistantMessage) -> AssistantMessage {
        let total_ms = self.request_start.elapsed().as_millis() as u64;
        let ttft_ms = self
            .first_token_at
            .map(|t| t.duration_since(self.request_start).as_millis() as u64);
        am.timing = CallTiming { total_ms, ttft_ms };
        am
    }

    fn emit_partial_call(&self, partial_tokens: u64) {
        if let Some(sink) = self.event_sink {
            sink.emit(crate::event::Event::LlmPartialCall {
                turn_id: self.turn_id.clone(),
                flow_run_id: self.flow_run_id.clone(),
                model: self.req.model.clone(),
                provider: self.provider.name().to_string(),
                tokens_before_abort: partial_tokens,
                restart_reason: "l2_course_correct".to_string(),
            });
        }
    }
}

fn merge_restart_error(
    err: RuntimeError,
    partial_output: String,
    partial_tokens: u64,
) -> RuntimeError {
    match err {
        RuntimeError::L2Restart {
            correction_text,
            partial_output: fallback_output,
            partial_tokens: fallback_tokens,
        } => RuntimeError::L2Restart {
            correction_text,
            partial_output: if partial_output.is_empty() {
                fallback_output
            } else {
                partial_output
            },
            partial_tokens: partial_tokens.max(fallback_tokens),
        },
        other => other,
    }
}

#[derive(Clone, Default)]
pub(crate) struct WatchRules {
    pub(crate) token_matches: Vec<(String, String)>,
    pub(crate) tokens_gt: Option<u64>,
    pub(crate) elapsed_ms_gt: Option<u64>,
    pub(crate) warn_token: Vec<WarnRule>,
    pub(crate) warn_tokens_gt: Vec<(u64, WarnRule)>,
    pub(crate) warn_elapsed_ms_gt: Vec<(u64, WarnRule)>,
}

#[derive(Clone)]
pub(crate) struct WarnRule {
    pub(crate) target: String,
    pub(crate) message: String,
    pub(crate) pattern: String,
}

struct StreamMonitor<'a> {
    window: String,
    text_captured: String,
    tokens_seen: u64,
    abort_reason: Option<String>,
    fired_warn_token: HashSet<String>,
    fired_warn_tokens: HashSet<u64>,
    fired_warn_elapsed: HashSet<u64>,
    event_sink: Option<&'a EventSink>,
    turn_id: Option<TurnId>,
    flow_run_id: Option<FlowRunId>,
}

impl<'a> StreamMonitor<'a> {
    fn new(stream: &LlmStream<'a>) -> Self {
        Self {
            window: String::new(),
            text_captured: String::new(),
            tokens_seen: 0,
            abort_reason: None,
            fired_warn_token: Default::default(),
            fired_warn_tokens: Default::default(),
            fired_warn_elapsed: Default::default(),
            event_sink: stream.event_sink,
            turn_id: stream.turn_id.clone(),
            flow_run_id: stream.flow_run_id.clone(),
        }
    }

    fn push_window(&mut self, text: &str) {
        self.window.push_str(text);
        while self.window.len() > 512 {
            let mut drop = self.window.len() - 512;
            while drop < self.window.len() && !self.window.is_char_boundary(drop) {
                drop += 1;
            }
            self.window.drain(..drop);
        }
        self.text_captured.push_str(text);
    }

    fn emit_warn(&self, rule: &WarnRule, trigger: &str) {
        if let Some(sink) = self.event_sink {
            sink.emit(crate::event::Event::WatchWarn {
                turn_id: self.turn_id.clone(),
                flow_run_id: self.flow_run_id.clone(),
                target: rule.target.clone(),
                trigger: trigger.to_string(),
                message: rule.message.clone(),
            });
        }
    }

    fn check_token_warns(&mut self, rules: &WatchRules) {
        for rule in &rules.warn_token {
            if !self.fired_warn_token.contains(&rule.pattern)
                && self.window.contains(rule.pattern.as_str())
            {
                self.fired_warn_token.insert(rule.pattern.clone());
                self.emit_warn(rule, &format!("token({})", rule.pattern));
            }
        }
    }

    fn check_tokens_consumed_warns(&mut self, rules: &WatchRules) {
        for (threshold, rule) in &rules.warn_tokens_gt {
            if !self.fired_warn_tokens.contains(threshold) && self.tokens_seen > *threshold {
                self.fired_warn_tokens.insert(*threshold);
                self.emit_warn(rule, &format!("tokens_consumed>{threshold}"));
            }
        }
    }

    fn check_elapsed_warns(&mut self, rules: &WatchRules, started: Instant) {
        let elapsed = started.elapsed().as_millis() as u64;
        for (threshold, rule) in &rules.warn_elapsed_ms_gt {
            if !self.fired_warn_elapsed.contains(threshold) && elapsed > *threshold {
                self.fired_warn_elapsed.insert(*threshold);
                self.emit_warn(rule, &format!("elapsed>{threshold}ms"));
            }
        }
    }

    fn on_chunk(
        &mut self,
        text: &str,
        cumulative_tokens: u64,
        started: Instant,
        rules: Option<&WatchRules>,
        cancel: &CancellationToken,
    ) {
        self.tokens_seen = cumulative_tokens.max(self.tokens_seen);
        self.push_window(text);
        let Some(rules) = rules else {
            return;
        };
        if self.abort_reason.is_none() {
            for (pat, reason) in &rules.token_matches {
                if self.window.contains(pat.as_str()) {
                    self.abort_reason = Some(reason.clone());
                    cancel.cancel();
                    break;
                }
            }
        }
        if self.abort_reason.is_none()
            && let Some(limit) = rules.tokens_gt
            && self.tokens_seen > limit
        {
            self.abort_reason = Some(format!("tokens_consumed > {limit}"));
            cancel.cancel();
        }
        self.check_token_warns(rules);
        self.check_tokens_consumed_warns(rules);
        self.check_elapsed_warns(rules, started);
    }

    fn on_done(&mut self, total_tokens: u64, started: Instant, rules: Option<&WatchRules>) {
        self.tokens_seen = total_tokens.max(self.tokens_seen);
        let Some(rules) = rules else {
            return;
        };
        if self.abort_reason.is_none()
            && let Some(limit) = rules.tokens_gt
            && self.tokens_seen > limit
        {
            self.abort_reason = Some(format!("tokens_consumed > {limit}"));
        }
        self.check_tokens_consumed_warns(rules);
        self.check_elapsed_warns(rules, started);
    }
}

pub(crate) fn handle_pending_injections(
    entry: &Arc<FlowEntry>,
    cancel: &CancellationToken,
) -> Result<(), RuntimeError> {
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
                RuntimeError::Cancelled(format!("hard stop: {}", inj.text))
            }
            crate::injection::InjectionLevel::L3Redirect => {
                if let Some(target) = &inj.redirect_target {
                    RuntimeError::Redirect(target.clone())
                } else {
                    RuntimeError::Cancelled(format!("redirect (no target): {}", inj.text))
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
                RuntimeError::L2Restart {
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
    event: NodeEvent,
    model_name: &str,
    run_id: Option<&str>,
    primary: Option<&Sender<StreamFrame>>,
    fallback: Option<&Sender<StreamFrame>>,
) {
    match event {
        NodeEvent::LlmChunk {
            text,
            cumulative_tokens: _,
        } => {
            emit_frame(
                primary,
                fallback,
                StreamFrame::LlmChunk {
                    text: text.clone(),
                    model: model_name.to_string(),
                    run_id: run_id.map(std::borrow::ToOwned::to_owned),
                },
            );
        }
        NodeEvent::ThinkingChunk { text } => {
            emit_frame(
                primary,
                fallback,
                StreamFrame::ThinkingChunk {
                    text: text.clone(),
                    run_id: run_id.map(std::borrow::ToOwned::to_owned),
                },
            );
        }
        NodeEvent::LlmDone { total_tokens } => {
            emit_frame(
                primary,
                fallback,
                StreamFrame::LlmDone {
                    total_tokens,
                    run_id: run_id.map(std::borrow::ToOwned::to_owned),
                },
            );
        }
        _ => {}
    }
}

fn emit_frame(
    primary: Option<&Sender<StreamFrame>>,
    fallback: Option<&Sender<StreamFrame>>,
    frame: StreamFrame,
) {
    if let Some(tx) = primary {
        let _ = tx.send(frame);
    } else if let Some(tx) = fallback {
        let _ = tx.send(frame);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::sync::broadcast;

    use super::*;
    use crate::event::Observable;
    use crate::injection::{Injection, InjectionLevel};
    use crate::message::{Message, MessagePart, MessageRole};
    use crate::provider::{StopReason, TokenUsage, estimate_tokens, user_text_message};
    use crate::tool::BoxFut;
    use crate::value::Value;

    #[derive(Clone)]
    enum Step {
        Chunk(&'static str, u64),
        Thinking(&'static str),
        Done(u64),
        Sleep(Duration),
        WaitCancel,
    }

    struct ScriptProvider {
        scripts: Mutex<Vec<Vec<Step>>>,
        call_hits: AtomicUsize,
        stream_hits: AtomicUsize,
        seen_prompts: Mutex<Vec<String>>,
    }

    impl ScriptProvider {
        fn new(scripts: Vec<Vec<Step>>) -> Self {
            Self {
                scripts: Mutex::new(scripts),
                call_hits: AtomicUsize::new(0),
                stream_hits: AtomicUsize::new(0),
                seen_prompts: Mutex::new(Vec::new()),
            }
        }

        fn assistant(text: String) -> AssistantMessage {
            AssistantMessage {
                message: Message {
                    role: MessageRole::Assistant,
                    parts: vec![MessagePart::Text { text: text.clone() }],
                    turn_id: crate::event::TurnId::now(),
                },
                stop_reason: StopReason::End,
                token_usage: TokenUsage {
                    output: estimate_tokens(&text),
                    ..Default::default()
                },
                timing: Default::default(),
                model: "test".into(),
                response_id: None,
            }
        }
    }

    impl Provider for ScriptProvider {
        fn name(&self) -> &str {
            "script"
        }

        fn call<'a>(
            &'a self,
            req: LlmRequest,
        ) -> BoxFut<'a, Result<AssistantMessage, RuntimeError>> {
            self.call_hits.fetch_add(1, Ordering::SeqCst);
            self.seen_prompts
                .lock()
                .unwrap()
                .push(req.messages.last().unwrap().text_concat());
            Box::pin(async { Ok(Self::assistant("call-path".into())) })
        }

        fn call_streaming(&self, req: LlmRequest) -> Observable<AssistantMessage> {
            self.stream_hits.fetch_add(1, Ordering::SeqCst);
            self.seen_prompts
                .lock()
                .unwrap()
                .push(req.messages.last().unwrap().text_concat());
            let steps = self.scripts.lock().unwrap().remove(0);
            let (tx, events) = broadcast::channel(64);
            let cancel = CancellationToken::new();
            let cancel_for_task = cancel.clone();
            let output: BoxFut<'static, Result<AssistantMessage, RuntimeError>> =
                Box::pin(async move {
                    let mut text = String::new();
                    let mut tokens = 0;
                    for step in steps {
                        match step {
                            Step::Chunk(s, t) => {
                                tokens = t;
                                text.push_str(s);
                                let _ = tx.send(NodeEvent::LlmChunk {
                                    text: s.into(),
                                    cumulative_tokens: t,
                                });
                            }
                            Step::Thinking(s) => {
                                let _ = tx.send(NodeEvent::ThinkingChunk { text: s.into() });
                            }
                            Step::Done(t) => {
                                tokens = t;
                                let _ = tx.send(NodeEvent::LlmDone { total_tokens: t });
                            }
                            Step::Sleep(d) => tokio::time::sleep(d).await,
                            Step::WaitCancel => {
                                cancel_for_task.cancelled().await;
                                return Err(RuntimeError::Cancelled("script cancelled".into()));
                            }
                        }
                        if cancel_for_task.is_cancelled() {
                            return Err(RuntimeError::Cancelled("script cancelled".into()));
                        }
                    }
                    if tokens == 0 {
                        tokens = estimate_tokens(&text);
                    }
                    let mut am = Self::assistant(text);
                    am.token_usage.output = tokens;
                    Ok(am)
                });
            Observable {
                output,
                events,
                cancel,
            }
        }
    }

    fn req(stall_timeout_secs: u64) -> LlmRequest {
        LlmRequest {
            model: "m".into(),
            messages: vec![user_text_message("prompt")],
            system: None,
            input: Value::Unit,
            schema: None,
            cache_prompt: false,
            tools: Vec::new(),
            thinking_enabled: false,
            stall_timeout_secs,
        }
    }

    fn entry() -> Arc<FlowEntry> {
        crate::tools::agent_ctrl::FlowRegistry::new().create_entry(
            "h".into(),
            "g".into(),
            "m".into(),
            crate::event::FlowRunId::now(),
        )
    }

    fn push_injection(
        entry: &FlowEntry,
        level: InjectionLevel,
        text: &str,
        redirect: Option<&str>,
    ) {
        entry
            .pending_injections
            .lock()
            .unwrap()
            .push(Injection::with_level(
                crate::event::TurnId::now(),
                text,
                level,
                redirect.map(str::to_string),
            ));
        entry.injection_notify.notify_one();
    }

    #[tokio::test]
    async fn basic_streaming_returns_message_and_frames() {
        let provider = ScriptProvider::new(vec![vec![
            Step::Thinking("think"),
            Step::Chunk("hel", 1),
            Step::Chunk("lo", 2),
            Step::Done(2),
        ]]);
        let (stream_tx, mut stream_rx) = broadcast::channel(16);
        let mut stream = LlmStream::new(&provider, req(1)).with_stream_tx(Some(stream_tx));
        let am = stream.run().await.unwrap();
        assert_eq!(am.text_concat(), "hello");
        assert!(am.timing.total_ms > 0 || am.timing.ttft_ms == Some(0));
        assert!(matches!(
            stream_rx.recv().await.unwrap(),
            StreamFrame::ThinkingChunk { .. }
        ));
        assert!(
            matches!(stream_rx.recv().await.unwrap(), StreamFrame::LlmChunk { text, .. } if text == "hel")
        );
        assert!(
            matches!(stream_rx.recv().await.unwrap(), StreamFrame::LlmChunk { text, .. } if text == "lo")
        );
        assert!(matches!(
            stream_rx.recv().await.unwrap(),
            StreamFrame::LlmDone {
                total_tokens: 2,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn stall_timeout_fires_without_token() {
        let provider = ScriptProvider::new(vec![vec![
            Step::Sleep(Duration::from_millis(1100)),
            Step::Chunk("late", 1),
        ]]);
        let (stream_tx, _) = broadcast::channel(16);
        let mut stream = LlmStream::new(&provider, req(0)).with_stream_tx(Some(stream_tx));
        stream.req.stall_timeout_secs = 1;
        let out = tokio::time::timeout(Duration::from_secs(2), stream.run())
            .await
            .unwrap();
        assert!(
            matches!(out, Err(RuntimeError::ToolFailed(msg)) if msg.contains("llm stall timeout"))
        );
    }

    #[tokio::test]
    async fn stall_timer_resets_after_chunk() {
        let provider = ScriptProvider::new(vec![vec![
            Step::Chunk("a", 1),
            Step::Sleep(Duration::from_millis(10)),
            Step::Chunk("b", 2),
            Step::Done(2),
        ]]);
        let (stream_tx, _) = broadcast::channel(16);
        let mut stream = LlmStream::new(&provider, req(1)).with_stream_tx(Some(stream_tx));
        assert_eq!(stream.run().await.unwrap().text_concat(), "ab");
    }

    #[tokio::test]
    async fn l2_restart_rebuilds_request_and_succeeds() {
        let provider = ScriptProvider::new(vec![
            vec![Step::Chunk("bad", 1), Step::WaitCancel],
            vec![Step::Chunk("good", 1), Step::Done(1)],
        ]);
        let entry = entry();
        let (stream_tx, _) = broadcast::channel(16);
        let sink = EventSink::new();
        let mut stream = LlmStream::new(&provider, req(1))
            .with_entry(&entry)
            .with_stream_tx(Some(stream_tx))
            .with_event_sink(Some(&sink));
        let entry_for_task = entry.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            push_injection(
                &entry_for_task,
                InjectionLevel::L2CourseCorrect,
                "fix",
                None,
            );
        });
        assert_eq!(stream.run().await.unwrap().text_concat(), "good");
        assert_eq!(provider.stream_hits.load(Ordering::SeqCst), 2);
        let prompts = provider.seen_prompts.lock().unwrap().clone();
        assert!(prompts[1].contains("<user_correction>fix</user_correction>"));
        assert!(
            sink.snapshot()
                .iter()
                .any(|e| matches!(e, crate::event::Event::LlmPartialCall { .. }))
        );
    }

    #[tokio::test]
    async fn l2_restart_exhaustion_cancels() {
        let provider = ScriptProvider::new(vec![
            vec![Step::WaitCancel],
            vec![Step::WaitCancel],
            vec![Step::WaitCancel],
            vec![Step::WaitCancel],
        ]);
        let entry = entry();
        let (stream_tx, _) = broadcast::channel(16);
        let mut stream = LlmStream::new(&provider, req(1))
            .with_entry(&entry)
            .with_stream_tx(Some(stream_tx));
        let entry_for_task = entry.clone();
        tokio::spawn(async move {
            for i in 0..4 {
                tokio::task::yield_now().await;
                push_injection(
                    &entry_for_task,
                    InjectionLevel::L2CourseCorrect,
                    &format!("fix{i}"),
                    None,
                );
            }
        });
        let out = stream.run().await;
        assert!(
            matches!(out, Err(RuntimeError::Cancelled(msg)) if msg.contains("l2 restart exhausted"))
        );
    }

    #[tokio::test]
    async fn l3_redirect_and_l4_hard_stop_return_errors() {
        let provider = ScriptProvider::new(vec![vec![Step::WaitCancel], vec![Step::WaitCancel]]);
        let first_entry = entry();
        push_injection(
            &first_entry,
            InjectionLevel::L3Redirect,
            "go",
            Some("review"),
        );
        let (stream_tx, _) = broadcast::channel(16);
        let mut stream = LlmStream::new(&provider, req(1))
            .with_entry(&first_entry)
            .with_stream_tx(Some(stream_tx.clone()));
        assert!(
            matches!(stream.run().await, Err(RuntimeError::Redirect(target)) if target == "review")
        );

        let second_entry = entry();
        push_injection(&second_entry, InjectionLevel::L4HardStop, "stop", None);
        let mut stream = LlmStream::new(&provider, req(1))
            .with_entry(&second_entry)
            .with_stream_tx(Some(stream_tx));
        assert!(
            matches!(stream.run().await, Err(RuntimeError::Cancelled(msg)) if msg.contains("hard stop"))
        );
    }

    #[tokio::test]
    async fn l1_nudge_adds_message_and_continues() {
        let provider = ScriptProvider::new(vec![vec![Step::Chunk("ok", 1), Step::Done(1)]]);
        let entry = entry();
        push_injection(&entry, InjectionLevel::L1Nudge, "note", None);
        let (stream_tx, _) = broadcast::channel(16);
        let mut stream = LlmStream::new(&provider, req(1))
            .with_entry(&entry)
            .with_stream_tx(Some(stream_tx));
        assert_eq!(stream.run().await.unwrap().text_concat(), "ok");
        assert!(
            entry
                .messages
                .lock()
                .unwrap()
                .iter()
                .any(|m| m.text_concat().contains("[interjection] note"))
        );
    }

    #[tokio::test]
    async fn flow_cancel_returns_cancelled() {
        let provider = ScriptProvider::new(vec![vec![Step::WaitCancel]]);
        let temp = tempfile::TempDir::new().unwrap();
        let session = Session::open(temp.path()).unwrap();
        session.begin_turn(user_text_message("hi"));
        let (stream_tx, _) = broadcast::channel(16);
        let mut stream = LlmStream::new(&provider, req(1))
            .with_session(&session)
            .with_stream_tx(Some(stream_tx));
        let fut = async {
            tokio::task::yield_now().await;
            session.cancel_flow();
        };
        tokio::join!(stream.run(), fut).0.unwrap_err();
    }

    #[tokio::test]
    async fn entry_side_effects_update_output_iteration_and_assistant_done() {
        let provider = ScriptProvider::new(vec![vec![Step::Chunk("hi", 1), Step::Done(1)]]);
        let entry = entry();
        let mut flow_rx = entry.stream_tx.subscribe();
        let (stream_tx, _) = broadcast::channel(16);
        let mut stream = LlmStream::new(&provider, req(1))
            .with_entry(&entry)
            .with_stream_tx(Some(stream_tx));
        stream.run().await.unwrap();
        assert_eq!(*entry.output.lock().unwrap(), "hi");
        assert_eq!(entry.iteration.load(Ordering::SeqCst), 1);
        assert!(
            matches!(flow_rx.recv().await.unwrap(), FlowEvent::AssistantDone { text } if text == "hi")
        );
    }

    #[tokio::test]
    async fn watch_rules_abort_and_warn() {
        let provider = ScriptProvider::new(vec![vec![Step::Chunk("danger", 3), Step::Done(3)]]);
        let (stream_tx, _) = broadcast::channel(16);
        let sink = EventSink::new();
        let rules = WatchRules {
            token_matches: vec![("danger".into(), "token match: danger".into())],
            warn_token: vec![WarnRule {
                target: "x".into(),
                message: "warn".into(),
                pattern: "danger".into(),
            }],
            ..Default::default()
        };
        let mut stream = LlmStream::new(&provider, req(1))
            .with_stream_tx(Some(stream_tx))
            .with_watch_rules(rules)
            .with_event_sink(Some(&sink));
        assert!(
            matches!(stream.run().await, Err(RuntimeError::Aborted(msg)) if msg.contains("danger"))
        );
        assert!(sink.snapshot().iter().any(
            |e| matches!(e, crate::event::Event::WatchWarn { message, .. } if message == "warn")
        ));
    }

    #[tokio::test]
    async fn stream_and_frame_tx_both_receive_frames_via_fallback_rules() {
        let provider = ScriptProvider::new(vec![vec![Step::Chunk("x", 1), Step::Done(1)]]);
        let (stream_tx, mut stream_rx) = broadcast::channel(16);
        let (frame_tx, mut frame_rx) = broadcast::channel(16);
        let mut stream = LlmStream::new(&provider, req(1))
            .with_stream_tx(Some(stream_tx))
            .with_frame_tx(Some(frame_tx));
        stream.run().await.unwrap();
        assert!(matches!(
            stream_rx.recv().await.unwrap(),
            StreamFrame::LlmChunk { .. }
        ));
        assert!(frame_rx.try_recv().is_err(), "fallback must not receive when primary is set");

        let provider = ScriptProvider::new(vec![vec![Step::Chunk("x", 1), Step::Done(1)]]);
        let (frame_tx, mut frame_rx) = broadcast::channel(16);
        let mut stream = LlmStream::new(&provider, req(1)).with_frame_tx(Some(frame_tx));
        stream.run().await.unwrap();
        assert!(matches!(
            frame_rx.recv().await.unwrap(),
            StreamFrame::LlmChunk { .. }
        ));
    }

    #[tokio::test]
    async fn no_stream_tx_uses_non_streaming_call() {
        let provider = ScriptProvider::new(vec![vec![Step::Chunk("stream", 1)]]);
        let mut stream = LlmStream::new(&provider, req(1));
        assert_eq!(stream.run().await.unwrap().text_concat(), "call-path");
        assert_eq!(provider.call_hits.load(Ordering::SeqCst), 1);
        assert_eq!(provider.stream_hits.load(Ordering::SeqCst), 0);
    }
}
