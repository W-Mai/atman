use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::broadcast::Sender;
use tokio_util::sync::CancellationToken;

use crate::context_plan::ContextCallPurpose;
use crate::error::RuntimeError;
use crate::event::{EventSink, FlowRunId, NodeEvent, TurnId};
use crate::provider::{AssistantMessage, CallTiming, LlmRequest, Provider};
use crate::session::Session;
use crate::stream::StreamFrame;
use crate::tools::agent_ctrl::{FlowEntry, FlowEvent};

#[derive(Debug)]
pub(crate) enum StreamFailure {
    Error(RuntimeError),
    Correction {
        claim: Box<crate::injection::InjectionClaim>,
        partial_output: String,
        partial_tokens: u64,
    },
}

impl From<RuntimeError> for StreamFailure {
    fn from(error: RuntimeError) -> Self {
        Self::Error(error)
    }
}

pub(crate) struct LlmStream<'a> {
    pub(crate) provider: &'a dyn Provider,
    pub(crate) req: LlmRequest,
    call_purpose: ContextCallPurpose,
    pub(crate) event_sink: Option<&'a EventSink>,
    pub(crate) turn_id: Option<TurnId>,
    pub(crate) flow_run_id: Option<FlowRunId>,
    pub(crate) first_token_at: Option<Instant>,
    pub(crate) request_start: Instant,
}

/// Streaming-enabled LlmStream. Created by `LlmStream::with_stream_tx()`.
pub(crate) struct StreamingLlmStream<'a> {
    base: LlmStream<'a>,
    stream_tx: Option<Sender<StreamFrame>>,
    frame_tx: Option<Sender<StreamFrame>>,
    session: Option<&'a Session>,
    entry: Option<&'a Arc<FlowEntry>>,
    flow_cancel: Option<CancellationToken>,
    watch_rules: Option<WatchRules>,
}

impl<'a> LlmStream<'a> {
    pub(crate) fn new(
        provider: &'a dyn Provider,
        req: LlmRequest,
        call_purpose: ContextCallPurpose,
    ) -> Self {
        Self {
            provider,
            req,
            call_purpose,
            event_sink: None,
            turn_id: None,
            flow_run_id: None,
            first_token_at: None,
            request_start: Instant::now(),
        }
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

    pub(crate) fn with_stream_tx(self, tx: Option<Sender<StreamFrame>>) -> StreamingLlmStream<'a> {
        StreamingLlmStream {
            base: self,
            stream_tx: tx,
            frame_tx: None,
            session: None,
            entry: None,
            flow_cancel: None,
            watch_rules: None,
        }
    }

    pub(crate) async fn run(&mut self) -> Result<AssistantMessage, RuntimeError> {
        self.provider
            .call(self.req.clone())
            .await
            .map(|am| self.finalize_timing(am))
    }

    pub(crate) fn finalize_timing(&self, mut am: AssistantMessage) -> AssistantMessage {
        let total_ms = self.request_start.elapsed().as_millis() as u64;
        let ttft_ms = self
            .first_token_at
            .map(|t| t.duration_since(self.request_start).as_millis() as u64);
        am.timing = CallTiming { total_ms, ttft_ms };
        if let Some(turn_id) = &self.turn_id {
            am.message.turn_id = turn_id.clone();
        }
        am
    }

    pub(crate) fn mark_first_token(&mut self) {
        if self.first_token_at.is_none() {
            self.first_token_at = Some(Instant::now());
        }
    }

    pub(crate) fn emit_partial_call(&self, partial_tokens: u64) {
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

impl<'a> StreamingLlmStream<'a> {
    pub(crate) fn with_session(
        mut self,
        session: &'a Session,
        flow_cancel: CancellationToken,
    ) -> Self {
        self.flow_cancel = Some(flow_cancel);
        self.session = Some(session);
        self
    }

    pub(crate) fn with_entry(mut self, entry: &'a Arc<FlowEntry>) -> Self {
        self.frame_tx = self.stream_tx.as_ref().map(|_| entry.frame_tx.clone());
        self.entry = Some(entry);
        self.flow_cancel = Some(entry.cancel.clone());
        self
    }

    #[cfg(test)]
    pub(crate) fn with_frame_tx(mut self, tx: Option<Sender<StreamFrame>>) -> Self {
        self.frame_tx = tx;
        self
    }

    pub(crate) fn with_watch_rules(mut self, rules: WatchRules) -> Self {
        self.watch_rules = Some(rules);
        self
    }

    #[allow(dead_code)]
    pub(crate) fn with_event_sink(mut self, sink: Option<&'a EventSink>) -> Self {
        self.base.event_sink = sink;
        self
    }
    #[allow(dead_code)]
    pub(crate) fn with_turn_id(mut self, turn_id: Option<TurnId>) -> Self {
        self.base.turn_id = turn_id;
        self
    }
    #[allow(dead_code)]
    pub(crate) fn with_flow_run_id(mut self, flow_run_id: Option<FlowRunId>) -> Self {
        self.base.flow_run_id = flow_run_id;
        self
    }

    pub(crate) async fn run(&mut self) -> Result<AssistantMessage, StreamFailure> {
        self.base.first_token_at = None;
        self.base.request_start = Instant::now();
        self.single_attempt()
            .await
            .map(|am| self.base.finalize_timing(am))
    }

    async fn single_attempt(&mut self) -> Result<AssistantMessage, StreamFailure> {
        let req = self.base.req.clone();
        let model_name = req.model.clone();
        let run_id = self.base.flow_run_id.as_ref().map(|r| r.0.to_string());
        let stall_secs = req.stall_timeout_secs;
        let flow_cancel = self.flow_cancel.clone().unwrap_or_default();
        let mut injection_changes = self.entry.map(|entry| entry.injections.watch());
        if let Some(entry) = self.entry {
            handle_pending_injections(entry, self.base.call_purpose)?;
        }
        let obs = self.base.provider.call_streaming(req);
        let cancel = obs.cancel.clone();
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

        let mut state = StreamMonitor::new(&self.base);
        let mut events_closed = false;
        let final_result = loop {
            tokio::select! {
                biased;
                _ = flow_cancel.cancelled(), if self.flow_cancel.is_some() => {
                    cancel.cancel();
                    break Err(RuntimeError::Cancelled("flow cancelled by user".into()).into());
                }
                _ = async {
                    if let Some(changes) = injection_changes.as_mut() {
                        let _ = changes.changed().await;
                    } else {
                        std::future::pending::<()>().await;
                    }
                }, if self.entry.is_some() => {
                    if let Some(entry) = self.entry
                        && let Err(err) = handle_pending_injections(entry, self.base.call_purpose)
                    {
                        cancel.cancel();
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
                            state.on_chunk(&text, cumulative_tokens, self.base.request_start, self.watch_rules.as_ref(), &cancel);
                            if stall_active {
                                stall_sleep.as_mut().reset(tokio::time::Instant::now() + stall_dur);
                            }
                        }
                        Ok(NodeEvent::ThinkingChunk { text }) => {
                            self.on_thinking(&model_name, run_id.as_deref(), text);
                        }
                        Ok(NodeEvent::ToolCallDraft {
                            index,
                            call_id,
                            name,
                            arguments_delta,
                        }) => emit_stream_event(
                            NodeEvent::ToolCallDraft {
                                index,
                                call_id,
                                name,
                                arguments_delta,
                            },
                            &model_name,
                            run_id.as_deref(),
                            self.stream_tx.as_ref(),
                            self.frame_tx.as_ref(),
                        ),
                        Ok(NodeEvent::LlmDone { total_tokens }) => {
                            self.on_done(&model_name, run_id.as_deref(), total_tokens);
                            state.on_done(total_tokens, self.base.request_start, self.watch_rules.as_ref());
                        }
                        Ok(_) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => events_closed = true,
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    }
                }
                _ = &mut elapsed_sleep, if elapsed_active && state.abort_reason.is_none() => {
                    state.abort_reason = Some(format!("elapsed > {elapsed_deadline_ms}ms"));
                    cancel.cancel();
                    break Err(RuntimeError::Cancelled("elapsed".into()).into());
                }
                _ = &mut stall_sleep, if stall_active => {
                    cancel.cancel();
                    break Err(RuntimeError::ToolFailed(format!("llm stall timeout after {}s", stall_secs)).into());
                }
                result = &mut output => break result.map_err(Into::into),
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
                        self.base.request_start,
                        self.watch_rules.as_ref(),
                        &cancel,
                    );
                }
                NodeEvent::ThinkingChunk { text } => {
                    self.on_thinking(&model_name, run_id.as_deref(), text)
                }
                NodeEvent::ToolCallDraft {
                    index,
                    call_id,
                    name,
                    arguments_delta,
                } => emit_stream_event(
                    NodeEvent::ToolCallDraft {
                        index,
                        call_id,
                        name,
                        arguments_delta,
                    },
                    &model_name,
                    run_id.as_deref(),
                    self.stream_tx.as_ref(),
                    self.frame_tx.as_ref(),
                ),
                NodeEvent::LlmDone { total_tokens } => {
                    self.on_done(&model_name, run_id.as_deref(), total_tokens);
                    state.on_done(
                        total_tokens,
                        self.base.request_start,
                        self.watch_rules.as_ref(),
                    );
                }
                _ => {}
            }
        }

        if let Some(reason) = state.abort_reason {
            return Err(RuntimeError::Aborted(reason).into());
        }
        final_result.map_err(|error| match error {
            StreamFailure::Correction { claim, .. } => {
                self.base.emit_partial_call(state.tokens_seen);
                StreamFailure::Correction {
                    claim,
                    partial_output: state.text_captured,
                    partial_tokens: state.tokens_seen,
                }
            }
            error => error,
        })
    }

    fn on_chunk(
        &mut self,
        model_name: &str,
        run_id: Option<&str>,
        text: &str,
        cumulative_tokens: u64,
    ) {
        self.base.mark_first_token();
        if self.stream_tx.is_none() && self.frame_tx.is_none() {
            return;
        }
        if let Some(session) = self.session
            && let Some(turn_id) = &self.base.turn_id
        {
            session.mark_streamed(turn_id);
        }
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
        self.base.mark_first_token();
        emit_stream_event(
            NodeEvent::ThinkingChunk { text },
            model_name,
            run_id,
            self.stream_tx.as_ref(),
            self.frame_tx.as_ref(),
        );
    }

    fn on_done(&mut self, model_name: &str, run_id: Option<&str>, total_tokens: u64) {
        if self.stream_tx.is_none() && self.frame_tx.is_none() {
            return;
        }
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
}

#[derive(Clone, Default)]
pub struct WatchRules {
    pub(crate) token_matches: Vec<(String, String)>,
    pub(crate) tokens_gt: Option<u64>,
    pub(crate) elapsed_ms_gt: Option<u64>,
    pub(crate) warn_token: Vec<WarnRule>,
    pub(crate) warn_tokens_gt: Vec<(u64, WarnRule)>,
    pub(crate) warn_elapsed_ms_gt: Vec<(u64, WarnRule)>,
}

impl WatchRules {
    pub(crate) fn is_active(&self) -> bool {
        !self.token_matches.is_empty()
            || self.tokens_gt.is_some()
            || self.elapsed_ms_gt.is_some()
            || !self.warn_token.is_empty()
            || !self.warn_tokens_gt.is_empty()
            || !self.warn_elapsed_ms_gt.is_empty()
    }
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
    call_purpose: ContextCallPurpose,
) -> Result<(), StreamFailure> {
    if let Some(claim) = entry.injections.claim_interruption(|injection| {
        entry.owns_injection(injection)
            && (call_purpose.accepts_steering_messages() || injection.control_error().is_some())
    }) {
        if claim.injection.level == crate::injection::InjectionLevel::L2CourseCorrect {
            return Err(StreamFailure::Correction {
                claim: Box::new(claim),
                partial_output: String::new(),
                partial_tokens: 0,
            });
        }
        if let Some(injection) = claim.commit(None, || {}) {
            return Err(injection.control_error().expect("selected control").into());
        }
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
        NodeEvent::ToolCallDraft {
            index,
            call_id,
            name,
            arguments_delta,
        } => {
            emit_frame(
                primary,
                fallback,
                StreamFrame::ToolCallDraft {
                    index,
                    call_id,
                    name,
                    arguments_delta,
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
    use crate::injection::InjectionLevel;
    use crate::message::{Message, MessageOrigin, MessagePart, MessageRole};
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
                    origin: MessageOrigin::User,
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
            prompt_cache_key: None,
            tools: Vec::new(),
            reasoning: crate::provider::ReasoningSelection::ProviderDefault,
            stall_timeout_secs,
        }
    }

    fn entry() -> Arc<FlowEntry> {
        let registry = crate::tools::agent_ctrl::FlowRegistry::new();
        let run_id = crate::event::FlowRunId::now();
        registry
            .register_root(
                "session".into(),
                run_id.clone(),
                crate::flow_authority::EffectiveAuthority::root(&Default::default(), false, None),
            )
            .unwrap();
        registry
            .create_entry(
                "h".into(),
                "g".into(),
                "m".into(),
                run_id,
                Default::default(),
            )
            .unwrap()
    }

    fn push_injection(
        entry: &FlowEntry,
        level: InjectionLevel,
        text: &str,
        redirect: Option<&str>,
    ) {
        entry
            .interject(text, level, redirect.map(str::to_string))
            .unwrap();
    }

    #[tokio::test]
    async fn authoritative_turn_id_overrides_provider_turn_for_both_call_paths() {
        let provider = ScriptProvider::new(vec![vec![Step::Done(0)]]);
        let old_turn = crate::event::TurnId::now();
        let current_turn = crate::event::TurnId::now();
        let mut request = req(1);
        request.messages = vec![
            Message::user_text(old_turn, "inherited history"),
            Message::user_text(current_turn.clone(), "current request"),
        ];

        let mut call = LlmStream::new(&provider, request.clone(), ContextCallPurpose::General)
            .with_turn_id(Some(current_turn.clone()));
        let call_message = call.run().await.unwrap();
        assert_eq!(call_message.message.turn_id, current_turn);

        let (stream_tx, _stream_rx) = broadcast::channel(16);
        let mut stream = LlmStream::new(&provider, request, ContextCallPurpose::General)
            .with_turn_id(Some(current_turn.clone()))
            .with_stream_tx(Some(stream_tx));
        let stream_message = stream.run().await.unwrap();
        assert_eq!(stream_message.message.turn_id, current_turn);
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
        let mut stream = LlmStream::new(&provider, req(1), ContextCallPurpose::General)
            .with_stream_tx(Some(stream_tx));
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
        let mut stream = LlmStream::new(&provider, req(0), ContextCallPurpose::General)
            .with_stream_tx(Some(stream_tx));
        stream.base.req.stall_timeout_secs = 1;
        let out = tokio::time::timeout(Duration::from_secs(2), stream.run())
            .await
            .unwrap();
        assert!(
            matches!(out, Err(StreamFailure::Error(RuntimeError::ToolFailed(msg))) if msg.contains("llm stall timeout"))
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
        let mut stream = LlmStream::new(&provider, req(1), ContextCallPurpose::General)
            .with_stream_tx(Some(stream_tx));
        assert_eq!(stream.run().await.unwrap().text_concat(), "ab");
    }

    #[tokio::test]
    async fn correction_returns_a_claim_and_partial_without_rebuilding_the_request() {
        let provider = ScriptProvider::new(vec![vec![Step::Chunk("partial", 1), Step::WaitCancel]]);
        let entry = entry();
        let (stream_tx, mut frames) = broadcast::channel(16);
        let mut stream = LlmStream::new(&provider, req(1), ContextCallPurpose::General)
            .with_stream_tx(Some(stream_tx))
            .with_entry(&entry);
        let inject = async {
            frames.recv().await.unwrap();
            push_injection(&entry, InjectionLevel::L2CourseCorrect, "fix", None);
        };
        let (out, ()) = tokio::join!(stream.run(), inject);
        let Err(StreamFailure::Correction {
            claim,
            partial_output,
            partial_tokens,
        }) = out
        else {
            panic!("expected correction");
        };
        assert_eq!(partial_output, "partial");
        assert_eq!(partial_tokens, 1);
        assert_eq!(provider.stream_hits.load(Ordering::SeqCst), 1);
        assert_eq!(entry.pending_injections().len(), 1);
        assert!(entry.context.messages.lock().unwrap().is_empty());
        drop(claim);
        assert!(matches!(
            handle_pending_injections(&entry, ContextCallPurpose::General),
            Err(StreamFailure::Correction { .. })
        ));
    }

    #[tokio::test]
    async fn pending_correction_does_not_start_provider_io() {
        let provider = ScriptProvider::new(vec![]);
        let entry = entry();
        push_injection(&entry, InjectionLevel::L2CourseCorrect, "fix", None);
        let (stream_tx, _) = broadcast::channel(16);
        let mut stream = LlmStream::new(&provider, req(1), ContextCallPurpose::General)
            .with_stream_tx(Some(stream_tx))
            .with_entry(&entry);
        assert!(matches!(
            stream.run().await,
            Err(StreamFailure::Correction { .. })
        ));
        assert_eq!(provider.stream_hits.load(Ordering::SeqCst), 0);
        assert_eq!(entry.pending_injections().len(), 1);
    }

    const AUXILIARY_PURPOSES: [ContextCallPurpose; 5] = [
        ContextCallPurpose::Classification,
        ContextCallPurpose::Extraction,
        ContextCallPurpose::BranchGeneration,
        ContextCallPurpose::Compaction,
        ContextCallPurpose::InterjectionClassification,
    ];

    #[test]
    fn stream_interruptions_preserve_unselected_messages_and_same_level_order() {
        for purpose in std::iter::once(ContextCallPurpose::General).chain(AUXILIARY_PURPOSES) {
            let entry = entry();
            for (level, text, target) in [
                (InjectionLevel::L1Nudge, "note", None),
                (InjectionLevel::L2CourseCorrect, "first correction", None),
                (InjectionLevel::L2CourseCorrect, "second correction", None),
                (InjectionLevel::L3Redirect, "redirect", Some("first")),
                (InjectionLevel::L3Redirect, "redirect", Some("second")),
                (InjectionLevel::L4HardStop, "stop", None),
            ] {
                push_injection(&entry, level, text, target);
            }
            let original = entry.pending_injections();
            assert!(matches!(handle_pending_injections(&entry, purpose),
                Err(StreamFailure::Error(RuntimeError::Cancelled(text))) if text == "hard stop: stop"));
            assert!(entry.cancel.is_cancelled());
            assert_eq!(entry.pending_injections(), original[..5]);
            assert!(entry.context.messages.lock().unwrap().is_empty());
            for target in ["first", "second"] {
                assert!(matches!(handle_pending_injections(&entry, purpose),
                    Err(StreamFailure::Error(RuntimeError::Redirect(actual))) if actual == target));
            }
            assert_eq!(entry.pending_injections(), original[..3]);
            if purpose.accepts_steering_messages() {
                for text in ["first correction", "second correction"] {
                    let Err(StreamFailure::Correction { claim, .. }) =
                        handle_pending_injections(&entry, purpose)
                    else {
                        panic!("expected correction");
                    };
                    assert_eq!(claim.injection.text, text);
                    claim.commit(Some(&entry.context.messages), || {}).unwrap();
                }
                assert_eq!(entry.pending_injections(), original[..1]);
                handle_pending_injections(&entry, purpose).unwrap();
                assert_eq!(entry.pending_injections(), original[..1]);
                let messages = entry.context.messages.lock().unwrap();
                assert_eq!(messages.len(), 2);
                assert_eq!(messages[0].turn_id, original[1].turn_id);
                assert_eq!(messages[1].turn_id, original[2].turn_id);
            } else {
                handle_pending_injections(&entry, purpose).unwrap();
                assert_eq!(entry.pending_injections(), original[..3]);
                assert!(entry.context.messages.lock().unwrap().is_empty());
            }
        }
    }

    #[tokio::test]
    async fn auxiliary_streams_preserve_steering_without_disabling_output() {
        for purpose in AUXILIARY_PURPOSES {
            let provider = ScriptProvider::new(vec![vec![
                Step::Chunk("ok", 1),
                Step::Sleep(Duration::from_millis(1)),
                Step::Done(1),
            ]]);
            let entry = entry();
            push_injection(&entry, InjectionLevel::L1Nudge, "note", None);
            push_injection(&entry, InjectionLevel::L2CourseCorrect, "fix", None);
            let pending = entry.pending_injections();
            let (stream_tx, mut frames) = broadcast::channel(16);
            let mut stream = LlmStream::new(&provider, req(1), purpose)
                .with_stream_tx(Some(stream_tx))
                .with_entry(&entry);
            assert_eq!(stream.run().await.unwrap().text_concat(), "ok");
            assert_eq!(provider.stream_hits.load(Ordering::SeqCst), 1);
            assert_eq!(entry.pending_injections(), pending);
            assert!(entry.context.messages.lock().unwrap().is_empty());
            assert_eq!(*entry.output.lock().unwrap(), "ok");
            assert!(
                std::iter::from_fn(|| frames.try_recv().ok()).any(
                    |frame| matches!(frame, StreamFrame::LlmChunk { text, .. } if text == "ok")
                )
            );
        }
    }

    #[tokio::test]
    async fn l3_redirect_and_l4_hard_stop_return_errors_for_every_purpose() {
        for purpose in std::iter::once(ContextCallPurpose::General).chain(AUXILIARY_PURPOSES) {
            for level in [InjectionLevel::L3Redirect, InjectionLevel::L4HardStop] {
                let provider = ScriptProvider::new(vec![vec![Step::WaitCancel]]);
                let entry = entry();
                if !purpose.accepts_steering_messages() {
                    push_injection(&entry, InjectionLevel::L1Nudge, "note", None);
                    push_injection(&entry, InjectionLevel::L2CourseCorrect, "fix", None);
                }
                let pending = entry.pending_injections();
                push_injection(&entry, level, "control", Some("review"));
                let (stream_tx, _) = broadcast::channel(16);
                let mut stream = LlmStream::new(&provider, req(1), purpose)
                    .with_stream_tx(Some(stream_tx))
                    .with_entry(&entry);
                let result = stream.run().await;
                match level {
                    InjectionLevel::L3Redirect => assert!(
                        matches!(result, Err(StreamFailure::Error(RuntimeError::Redirect(target))) if target == "review")
                    ),
                    InjectionLevel::L4HardStop => assert!(
                        matches!(result, Err(StreamFailure::Error(RuntimeError::Cancelled(msg))) if msg.contains("hard stop"))
                    ),
                    _ => unreachable!(),
                }
                assert_eq!(entry.pending_injections(), pending);
            }
        }
    }

    #[tokio::test]
    async fn l1_nudge_stays_pending_until_another_agent_call() {
        let provider = ScriptProvider::new(vec![vec![Step::Chunk("ok", 1), Step::Done(1)]]);
        let entry = entry();
        push_injection(&entry, InjectionLevel::L1Nudge, "note", None);
        let (stream_tx, _) = broadcast::channel(16);
        let mut stream = LlmStream::new(&provider, req(1), ContextCallPurpose::General)
            .with_stream_tx(Some(stream_tx))
            .with_entry(&entry);
        assert_eq!(stream.run().await.unwrap().text_concat(), "ok");
        assert!(entry.context.messages.lock().unwrap().is_empty());
        assert_eq!(entry.pending_injections().len(), 1);
        entry.drain_injections().await;
        assert!(entry.pending_injections().is_empty());
        assert!(
            entry.context.messages.lock().unwrap()[0]
                .text_concat()
                .contains("note")
        );
    }

    #[tokio::test]
    async fn flow_cancel_returns_cancelled() {
        let provider = ScriptProvider::new(vec![vec![Step::WaitCancel]]);
        let temp = tempfile::TempDir::new().unwrap();
        let session = Session::open(temp.path()).unwrap();
        let turn_id = session.begin_turn(user_text_message("hi"));
        let (stream_tx, _) = broadcast::channel(16);
        let mut stream = LlmStream::new(&provider, req(1), ContextCallPurpose::General)
            .with_stream_tx(Some(stream_tx))
            .with_session(&session, session.flow_cancel_token(&turn_id).unwrap());
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
        let mut stream = LlmStream::new(&provider, req(1), ContextCallPurpose::General)
            .with_stream_tx(Some(stream_tx))
            .with_entry(&entry);
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
        let mut stream = LlmStream::new(&provider, req(1), ContextCallPurpose::General)
            .with_stream_tx(Some(stream_tx))
            .with_watch_rules(rules)
            .with_event_sink(Some(&sink));
        assert!(
            matches!(stream.run().await, Err(StreamFailure::Error(RuntimeError::Aborted(msg))) if msg.contains("danger"))
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
        let mut stream = LlmStream::new(&provider, req(1), ContextCallPurpose::General)
            .with_stream_tx(Some(stream_tx))
            .with_frame_tx(Some(frame_tx));
        stream.run().await.unwrap();
        assert!(matches!(
            stream_rx.recv().await.unwrap(),
            StreamFrame::LlmChunk { .. }
        ));
        assert!(
            frame_rx.try_recv().is_err(),
            "fallback must not receive when primary is set"
        );
    }

    #[tokio::test]
    async fn no_stream_tx_uses_non_streaming_call() {
        let provider = ScriptProvider::new(vec![vec![Step::Chunk("stream", 1)]]);
        let mut stream = LlmStream::new(&provider, req(1), ContextCallPurpose::General);
        assert_eq!(stream.run().await.unwrap().text_concat(), "call-path");
        assert_eq!(provider.call_hits.load(Ordering::SeqCst), 1);
        assert_eq!(provider.stream_hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn hidden_monitors_do_not_mark_visible_output_or_publish_entry_frames() {
        for purpose in std::iter::once(ContextCallPurpose::General).chain(AUXILIARY_PURPOSES) {
            let provider = ScriptProvider::new(vec![vec![
                Step::Thinking("hidden"),
                Step::Chunk("result", 1),
                Step::Done(1),
            ]]);
            let entry = entry();
            let session = Session::open_ephemeral();
            let turn = session.begin_turn(user_text_message("task"));
            let mut frames = entry.frame_tx.subscribe();
            let mut stream = LlmStream::new(&provider, req(1), purpose)
                .with_turn_id(Some(turn.clone()))
                .with_stream_tx(None)
                .with_session(&session, session.flow_cancel_token(&turn).unwrap())
                .with_entry(&entry);
            let response = stream.run().await.unwrap();
            assert_eq!(response.text_concat(), "result");
            assert!(response.timing.ttft_ms.is_some());
            assert!(!session.take_streamed_flag(&turn));
            assert!(entry.output.lock().unwrap().is_empty());
            assert_eq!(entry.iteration.load(Ordering::Relaxed), 0);
            assert!(frames.try_recv().is_err());
        }
    }
}
