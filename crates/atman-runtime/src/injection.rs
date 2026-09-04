use serde::{Deserialize, Serialize};

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
