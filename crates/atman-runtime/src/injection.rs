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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Injection {
    pub id: InjectionId,
    pub text: String,
    pub turn_id: TurnId,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub state: InjectionState,
}

impl Injection {
    pub fn new_pending(turn_id: TurnId, text: impl Into<String>) -> Self {
        Self {
            id: InjectionId::now(),
            text: text.into(),
            turn_id,
            created_at: chrono::Utc::now(),
            state: InjectionState::Pending,
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
}
