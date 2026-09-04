use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crate::event::TurnId;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(transparent)]
pub struct InjectionId(pub uuid::Uuid);

impl InjectionId {
    pub fn now() -> Self {
        Self(uuid::Uuid::now_v7())
    }
}

impl std::fmt::Display for InjectionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InjectionState {
    Pending,
    Injected,
    Cancelled,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InjectionLevel {
    L1Nudge,
    L2CourseCorrect,
    L3Redirect,
    L4HardStop,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum InjectionSource {
    #[default]
    User,
    Watcher {
        watcher_id: String,
        kind: String,
        handle: String,
    },
}

impl InjectionLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            InjectionLevel::L1Nudge => "l1_nudge",
            InjectionLevel::L2CourseCorrect => "l2_course_correct",
            InjectionLevel::L3Redirect => "l3_redirect",
            InjectionLevel::L4HardStop => "l4_hard_stop",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Injection {
    pub id: InjectionId,
    pub text: String,
    pub turn_id: TurnId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow_run_id: Option<crate::event::FlowRunId>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub state: InjectionState,
    #[serde(default = "default_level")]
    pub level: InjectionLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_target: Option<String>,
    #[serde(default)]
    pub source: InjectionSource,
}

fn default_level() -> InjectionLevel {
    InjectionLevel::L1Nudge
}

#[derive(Default)]
struct QueueState {
    pending: Vec<Injection>,
    claimed: HashSet<InjectionId>,
}

/// Pending inputs and their durable state transitions. Context remains owned by the run.
pub struct InjectionQueue {
    state: Mutex<QueueState>,
    events: Option<crate::event::EventSink>,
    updates: tokio::sync::broadcast::Sender<Injection>,
    changed: tokio::sync::watch::Sender<()>,
}

impl InjectionQueue {
    pub fn new(events: Option<crate::event::EventSink>) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(QueueState::default()),
            events,
            updates: tokio::sync::broadcast::channel(32).0,
            changed: tokio::sync::watch::channel(()).0,
        })
    }

    pub(crate) fn enqueue(&self, injection: Injection) -> Option<crate::event::EventEnvelope> {
        let mut state = self.state.lock().unwrap();
        let envelope = self.publish(&injection, None);
        state.pending.push(injection);
        self.changed.send_replace(());
        envelope
    }

    fn publish(
        &self,
        injection: &Injection,
        context_message: Option<crate::message::Message>,
    ) -> Option<crate::event::EventEnvelope> {
        let envelope = self.events.as_ref().map(|events| {
            events.emit_returning_envelope(crate::event::Event::UserInject {
                turn_id: injection.turn_id.clone(),
                injection: injection.clone(),
                context_message,
            })
        });
        let _ = self.updates.send(injection.clone());
        envelope
    }

    pub fn pending(&self) -> Vec<Injection> {
        self.state.lock().unwrap().pending.clone()
    }

    pub(crate) fn subscribe(&self) -> tokio::sync::broadcast::Receiver<Injection> {
        self.updates.subscribe()
    }

    pub(crate) fn watch(&self) -> tokio::sync::watch::Receiver<()> {
        self.changed.subscribe()
    }

    pub(crate) fn bind_turn(&self, turn_id: &TurnId, run_id: &crate::event::FlowRunId) {
        let mut state = self.state.lock().unwrap();
        for injection in &mut state.pending {
            if injection.turn_id == *turn_id && injection.flow_run_id.is_none() {
                injection.flow_run_id = Some(run_id.clone());
                self.publish(injection, None);
            }
        }
        self.changed.send_replace(());
    }

    pub(crate) fn cancel(&self, eligible: impl Fn(&Injection) -> bool) {
        let mut state = self.state.lock().unwrap();
        let mut cancelled = Vec::new();
        state.pending.retain(|injection| {
            if eligible(injection) {
                cancelled.push(injection.clone());
                false
            } else {
                true
            }
        });
        for mut injection in cancelled {
            state.claimed.remove(&injection.id);
            injection.state = InjectionState::Cancelled;
            self.publish(&injection, None);
        }
        self.changed.send_replace(());
    }

    pub(crate) fn claim_interruption(
        self: &Arc<Self>,
        eligible: impl Fn(&Injection) -> bool,
    ) -> Option<InjectionClaim> {
        self.claim(|state| {
            next_interruption(&state.pending, |injection| {
                !state.claimed.contains(&injection.id) && eligible(injection)
            })
        })
    }

    pub(crate) fn claim_steering(
        self: &Arc<Self>,
        eligible: impl Fn(&Injection) -> bool,
    ) -> Option<InjectionClaim> {
        self.claim(|state| {
            state.pending.iter().position(|injection| {
                !state.claimed.contains(&injection.id)
                    && eligible(injection)
                    && matches!(
                        injection.level,
                        InjectionLevel::L1Nudge | InjectionLevel::L2CourseCorrect
                    )
            })
        })
    }

    fn claim(
        self: &Arc<Self>,
        select: impl FnOnce(&QueueState) -> Option<usize>,
    ) -> Option<InjectionClaim> {
        let mut state = self.state.lock().unwrap();
        let injection = state.pending.get(select(&state)?)?.clone();
        state.claimed.insert(injection.id.clone());
        Some(InjectionClaim {
            queue: Arc::clone(self),
            injection,
        })
    }
}

/// A cancellation-safe claim: dropping it leaves the input available unless the run has ended.
pub(crate) struct InjectionClaim {
    queue: Arc<InjectionQueue>,
    pub(crate) injection: Injection,
}

impl std::fmt::Debug for InjectionClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InjectionClaim")
            .field("injection", &self.injection)
            .finish_non_exhaustive()
    }
}

impl InjectionClaim {
    pub(crate) fn commit(
        self,
        context: Option<&Mutex<Vec<crate::message::Message>>>,
        before: impl FnOnce(),
    ) -> Option<Injection> {
        let mut state = self.queue.state.lock().unwrap();
        let index = state
            .pending
            .iter()
            .position(|injection| injection.id == self.injection.id)?;
        before();
        let mut injection = state.pending.remove(index);
        state.claimed.remove(&injection.id);
        injection.state = InjectionState::Injected;
        let mut messages = context.map(|messages| messages.lock().unwrap());
        let message = context.map(|_| injection.context_message());
        self.queue.publish(&injection, message.clone());
        if let (Some(messages), Some(message)) = (&mut messages, message) {
            messages.push(message);
        }
        Some(injection)
    }
}

impl Drop for InjectionClaim {
    fn drop(&mut self) {
        if self
            .queue
            .state
            .lock()
            .unwrap()
            .claimed
            .remove(&self.injection.id)
        {
            self.queue.changed.send_replace(());
        }
    }
}

pub(crate) fn next_interruption(
    queue: &[Injection],
    eligible: impl Fn(&Injection) -> bool,
) -> Option<usize> {
    queue
        .iter()
        .enumerate()
        .filter(|(_, injection)| injection.state == InjectionState::Pending && eligible(injection))
        .filter_map(|(index, injection)| {
            let priority = match injection.level {
                InjectionLevel::L4HardStop => 0,
                InjectionLevel::L3Redirect => 1,
                InjectionLevel::L2CourseCorrect => 2,
                InjectionLevel::L1Nudge => return None,
            };
            Some((index, priority))
        })
        .min_by_key(|(_, priority)| *priority)
        .map(|(index, _)| index)
}

impl Injection {
    pub(crate) fn control_error(&self) -> Option<crate::error::RuntimeError> {
        match self.level {
            InjectionLevel::L3Redirect => Some(match &self.redirect_target {
                Some(target) => crate::error::RuntimeError::Redirect(target.clone()),
                None => crate::error::RuntimeError::Cancelled(format!(
                    "redirect (no target): {}",
                    self.text,
                )),
            }),
            InjectionLevel::L4HardStop => Some(crate::error::RuntimeError::Cancelled(format!(
                "hard stop: {}",
                self.text,
            ))),
            _ => None,
        }
    }

    pub(crate) fn context_message(&self) -> crate::message::Message {
        let (intro, tag, source) = match &self.source {
            InjectionSource::User => (
                "The user sent the following steering message(s) while you were working. Apply them to your next step if still relevant.\n\n",
                if self.level == InjectionLevel::L2CourseCorrect {
                    "user_correction"
                } else {
                    "user_nudge"
                },
                "user".to_string(),
            ),
            InjectionSource::Watcher {
                watcher_id,
                kind,
                handle,
            } => (
                "A background watcher detected the following event(s):\n\n",
                "watcher_event",
                format!("{kind} '{handle}' watcher {watcher_id}"),
            ),
        };
        let mut message = crate::message::Message::user_text(
            self.turn_id.clone(),
            format!(
                "{intro}<{tag} id=\"{}\" ts=\"{}\" source=\"{source}\">\n{}\n</{tag}>\n",
                self.id,
                self.created_at.to_rfc3339(),
                self.text,
            ),
        );
        message.origin = crate::message::MessageOrigin::Interjection;
        message
    }

    pub fn new_pending(turn_id: TurnId, text: impl Into<String>) -> Self {
        Self::with_level(turn_id, text, InjectionLevel::L1Nudge, None)
    }

    pub fn with_level(
        turn_id: TurnId,
        text: impl Into<String>,
        level: InjectionLevel,
        redirect_target: Option<String>,
    ) -> Self {
        Self::with_level_for_run(turn_id, text, level, redirect_target, None)
    }

    pub fn with_level_for_run(
        turn_id: TurnId,
        text: impl Into<String>,
        level: InjectionLevel,
        redirect_target: Option<String>,
        flow_run_id: Option<crate::event::FlowRunId>,
    ) -> Self {
        Self {
            id: InjectionId::now(),
            text: text.into(),
            turn_id,
            flow_run_id,
            created_at: chrono::Utc::now(),
            state: InjectionState::Pending,
            level,
            redirect_target,
            source: InjectionSource::User,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claims_are_exclusive_retryable_and_cannot_resurrect_cancelled_inputs() {
        let sink = crate::event::EventSink::new();
        let queue = InjectionQueue::new(Some(sink.clone()));
        let turn = TurnId::now();
        let first =
            Injection::with_level(turn.clone(), "same", InjectionLevel::L2CourseCorrect, None);
        let second = Injection::with_level(turn, "same", InjectionLevel::L2CourseCorrect, None);
        queue.enqueue(first.clone());
        queue.enqueue(second.clone());
        let claim = queue.claim_interruption(|_| true).unwrap();
        assert_eq!(claim.injection.id, first.id);
        let other = queue.claim_interruption(|_| true).unwrap();
        assert_eq!(other.injection.id, second.id);
        assert!(queue.claim_interruption(|_| true).is_none());
        drop(claim);
        let claim = queue.claim_interruption(|_| true).unwrap();
        assert_eq!(claim.injection.id, first.id);
        let messages = Mutex::new(Vec::new());
        claim.commit(Some(&messages), || {}).unwrap();
        assert_eq!(messages.lock().unwrap().len(), 1);
        queue.cancel(|_| true);
        assert!(
            other
                .commit(Some(&messages), || panic!(
                    "cancelled claim must not publish partial output"
                ))
                .is_none()
        );
        assert!(queue.pending().is_empty());
        assert!(queue.claim_interruption(|_| true).is_none());
        let events = sink.snapshot();
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    crate::event::Event::UserInject {
                        context_message: Some(_),
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(events.iter().filter(|event| matches!(event, crate::event::Event::UserInject { injection, .. } if injection.state == InjectionState::Cancelled)).count(), 1);
    }

    #[tokio::test]
    async fn queue_changes_are_visible_to_every_subscriber_without_lost_wakeups() {
        let queue = InjectionQueue::new(None);
        let mut first = queue.watch();
        let mut second = queue.watch();
        queue.enqueue(Injection::new_pending(TurnId::now(), "note"));
        assert!(first.has_changed().unwrap());
        assert!(second.has_changed().unwrap());
        first.changed().await.unwrap();
        second.changed().await.unwrap();
        let claim = queue.claim_steering(|_| true).unwrap();
        drop(claim);
        assert!(first.has_changed().unwrap());
        assert!(second.has_changed().unwrap());
    }

    #[test]
    fn injection_roundtrips_via_serde_json() {
        let inj = Injection::new_pending(TurnId::now(), "remember to check tests");
        let s = serde_json::to_string(&inj).unwrap();
        let back: Injection = serde_json::from_str(&s).unwrap();
        assert_eq!(inj, back);
    }

    #[test]
    fn injection_ids_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = InjectionId::now();
            assert!(seen.insert(id));
        }
    }

    #[test]
    fn state_serializes_snake_case() {
        assert_eq!(
            serde_json::to_string(&InjectionState::Pending).unwrap(),
            "\"pending\""
        );
        assert_eq!(
            serde_json::to_string(&InjectionState::Injected).unwrap(),
            "\"injected\""
        );
        assert_eq!(
            serde_json::to_string(&InjectionState::Cancelled).unwrap(),
            "\"cancelled\""
        );
    }

    #[test]
    fn new_pending_starts_in_pending_state() {
        let inj = Injection::new_pending(TurnId::now(), "x");
        assert_eq!(inj.state, InjectionState::Pending);
    }

    #[test]
    fn old_event_without_source_deserializes_as_user() {
        let json = serde_json::json!({
            "id": uuid::Uuid::now_v7(),
            "text": "old message",
            "turn_id": uuid::Uuid::now_v7().to_string(),
            "created_at": "2026-01-01T00:00:00Z",
            "state": "pending",
            "level": "l1_nudge"
        });
        let inj: Injection = serde_json::from_value(json).unwrap();
        assert_eq!(inj.source, InjectionSource::User);
    }

    #[test]
    fn watcher_source_roundtrips() {
        let inj = Injection {
            id: InjectionId::now(),
            text: "pattern found".into(),
            turn_id: TurnId::now(),
            flow_run_id: None,
            created_at: chrono::Utc::now(),
            state: InjectionState::Pending,
            level: crate::injection::InjectionLevel::L1Nudge,
            redirect_target: None,
            source: InjectionSource::Watcher {
                watcher_id: "w_abc".into(),
                kind: "terminal".into(),
                handle: "term_x".into(),
            },
        };
        let s = serde_json::to_string(&inj).unwrap();
        let back: Injection = serde_json::from_str(&s).unwrap();
        assert_eq!(inj, back);
    }
}
