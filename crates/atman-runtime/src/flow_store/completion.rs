//! Entry results committed with terminal state, independently of public answers.

use std::{collections::HashMap, io, path::PathBuf};

use atman_rt::{FlowInstance, Interruption, TurnLease, TurnOutcome};
use serde::{Deserialize, Serialize};

use super::{FlowFact, FlowStore, invalid};
use crate::{
    RuntimeError, Value, event::AtmanUuid, flow_store::position::FlowPosition, value::AtmanPayload,
};

/// Typed data retains list/field order, host payloads, and the exact floating-point bits.
/// Executable values are identified without serializing their process-local closures.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum StoredValue {
    Unit,
    Bool(bool),
    Int(i64),
    /// Big-endian IEEE 754 bytes survive JSON adapters with signed-integer numeric values.
    Float([u8; 8]),
    String(String),
    List(Vec<Self>),
    Struct(Vec<(String, Self)>),
    Path(PathBuf),
    Message(Box<crate::message::Message>),
    EditProposal(Box<crate::hunk::EditProposal>),
    Error(RuntimeError),
    Unavailable(String),
}

impl StoredValue {
    pub fn capture(value: &Value) -> Self {
        match value {
            Value::Unit => Self::Unit,
            Value::Bool(value) => Self::Bool(*value),
            Value::Int(value) => Self::Int(*value),
            Value::Float(value) => Self::Float(value.to_be_bytes()),
            Value::Str(value) => Self::String(value.clone()),
            Value::List(values) => Self::List(values.iter().map(Self::capture).collect()),
            Value::Struct(fields) => Self::Struct(
                fields
                    .iter()
                    .map(|(name, value)| (name.clone(), Self::capture(value)))
                    .collect(),
            ),
            Value::Host(AtmanPayload::Path(value)) => {
                serializable_payload(value, || Self::Path(value.clone()))
            }
            Value::Host(AtmanPayload::Message(value)) => {
                serializable_payload(value, || Self::Message(Box::new(value.clone())))
            }
            Value::Host(AtmanPayload::EditProposal(value)) => {
                serializable_payload(value, || Self::EditProposal(value.clone()))
            }
            Value::Err(error) => Self::Error(error.clone()),
            Value::Lambda { .. } | Value::FlowFuture(_) | Value::ToolFuture(_) => {
                Self::Unavailable(format!("process-local {}", value.kind_name()))
            }
        }
    }

    /// Restores data only. A closure or pending call is never converted to null or executed.
    pub fn restore(&self) -> Result<Value, RuntimeError> {
        Ok(match self {
            Self::Unit => Value::Unit,
            Self::Bool(value) => Value::Bool(*value),
            Self::Int(value) => Value::Int(*value),
            Self::Float(bytes) => Value::Float(f64::from_be_bytes(*bytes)),
            Self::String(value) => Value::Str(value.clone()),
            Self::List(values) => {
                Value::List(values.iter().map(Self::restore).collect::<Result<_, _>>()?)
            }
            Self::Struct(fields) => Value::Struct(
                fields
                    .iter()
                    .map(|(name, value)| Ok((name.clone(), value.restore()?)))
                    .collect::<Result<_, RuntimeError>>()?,
            ),
            Self::Path(value) => Value::Host(AtmanPayload::Path(value.clone())),
            Self::Message(value) => Value::Host(AtmanPayload::Message(value.as_ref().clone())),
            Self::EditProposal(value) => Value::Host(AtmanPayload::EditProposal(value.clone())),
            Self::Error(error) => Value::Err(error.clone()),
            Self::Unavailable(kind) => {
                return Err(RuntimeError::ToolFailed(format!(
                    "stored result is unavailable: {kind}"
                )));
            }
        })
    }
}

fn serializable_payload(
    value: &impl Serialize,
    capture: impl FnOnce() -> StoredValue,
) -> StoredValue {
    match serde_json::to_writer(io::sink(), value) {
        Ok(_) => capture(),
        Err(error) => {
            StoredValue::Unavailable(format!("host payload serialization failed: {error}"))
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum InvocationResult {
    Returned(StoredValue),
    Failed(RuntimeError),
    /// Explicit lifecycle completion without an observed entry return, including host loss.
    Unobserved(TurnOutcome),
}

impl InvocationResult {
    pub fn capture(result: &Result<Value, RuntimeError>) -> Self {
        match result {
            Ok(value) => Self::Returned(StoredValue::capture(value)),
            Err(error) => Self::Failed(error.clone()),
        }
    }

    fn outcome(&self) -> TurnOutcome {
        match self {
            Self::Returned(_) => TurnOutcome::Completed,
            Self::Failed(RuntimeError::Cancelled(_)) => {
                TurnOutcome::Interrupted(Interruption::Cancelled)
            }
            Self::Failed(_) => TurnOutcome::Interrupted(Interruption::Failed),
            Self::Unobserved(outcome) => *outcome,
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Completion {
    pub lease: TurnLease<AtmanUuid>,
    pub finished_at: chrono::DateTime<chrono::Utc>,
    /// Execution controls can override a successful return with cancellation.
    pub outcome: TurnOutcome,
    pub result: InvocationResult,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct CommittedCompletion {
    #[serde(flatten)]
    pub completion: Completion,
    pub receipt: FlowPosition,
}

#[derive(Default)]
pub(super) struct Completions {
    records: Vec<CommittedCompletion>,
    epochs: HashMap<u64, usize>,
}

impl Completions {
    pub(super) fn get(&self, lease: &TurnLease<AtmanUuid>) -> Option<&CommittedCompletion> {
        self.epochs
            .get(&lease.epoch)
            .map(|&index| &self.records[index])
    }

    pub fn replay(
        &mut self,
        state: &mut FlowInstance<AtmanUuid>,
        completion: Completion,
        receipt: FlowPosition,
    ) -> io::Result<()> {
        if self.get(&completion.lease).is_some() {
            return Err(invalid("turn completion is already recorded"));
        }
        let mut candidate = state.clone();
        let actual = candidate
            .finish_turn(&completion.lease, completion.result.outcome())
            .map_err(invalid)?;
        if actual != completion.outcome {
            return Err(invalid("turn completion does not match execution state"));
        }
        *state = candidate;
        self.apply(completion, receipt);
        Ok(())
    }

    fn apply(&mut self, completion: Completion, receipt: FlowPosition) {
        self.epochs
            .insert(completion.lease.epoch, self.records.len());
        self.records.push(CommittedCompletion {
            completion,
            receipt,
        });
    }
}

impl FlowStore {
    pub fn completions(&self) -> &[CommittedCompletion] {
        &self.completions.records
    }

    pub fn completion(&self, turn: Option<&crate::event::TurnId>) -> Option<&CommittedCompletion> {
        self.completions
            .records
            .iter()
            .rev()
            .find(|record| turn.is_none_or(|turn| &record.completion.lease.turn == turn))
    }

    /// Commit the exact result and terminal state together. Matching retries retain their receipt.
    pub fn finish_turn(
        &mut self,
        lease: TurnLease<AtmanUuid>,
        result: InvocationResult,
    ) -> io::Result<CommittedCompletion> {
        if let Some(existing) = self.completions.get(&lease) {
            return if existing.completion.lease == lease && existing.completion.result == result {
                Ok(existing.clone())
            } else {
                Err(invalid(
                    "turn completion conflicts with its committed result",
                ))
            };
        }
        let mut state = self.state.clone();
        let outcome = state
            .finish_turn(&lease, result.outcome())
            .map_err(invalid)?;
        let completion = Completion {
            lease,
            finished_at: chrono::Utc::now(),
            outcome,
            result,
        };
        let completed_state = state.clone();
        let mut facts = vec![FlowFact::TurnCompleted(Box::new(completion.clone()))];
        if matches!(
            state.state(),
            atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Interrupted { .. })
        ) && self.inbox.user_requested_after_cancel(&completion.lease)
        {
            state.recover_interrupted().map_err(invalid)?;
            facts.push(FlowFact::Transition(
                super::FlowTransition::InterruptionRecovered,
            ));
        }
        let receipt = self.journal.commit(&facts)?;
        self.completions.apply(completion.clone(), receipt.clone());
        self.inbox.turn_completed(&completion, &receipt);
        if state != completed_state {
            self.inbox.state_changed(&completed_state, &receipt);
        }
        self.install_state(state);
        self.inbox.state_changed(&self.state, &receipt);
        self.cursor = receipt.clone();
        self.refresh_projection();
        Ok(CommittedCompletion {
            completion,
            receipt,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_results_round_trip_without_changing_values_or_executing_closures() {
        use crate::ValueJson;
        let value = Value::List(vec![
            Value::Unit,
            Value::Bool(true),
            Value::Int(i64::MAX),
            Value::Float(-0.0),
            Value::Float(-1.1),
            Value::Float(f64::from_bits(0x7ff8_0000_0000_0042)),
            Value::Str("text".into()),
            Value::Struct(vec![
                ("b".into(), Value::Int(1)),
                ("a".into(), Value::Int(2)),
                ("b".into(), Value::Int(3)),
            ]),
            Value::Host(AtmanPayload::Path("example/file.at".into())),
            Value::Host(AtmanPayload::Message(
                crate::message::Message::assistant_text(crate::event::TurnId::now(), "answer"),
            )),
            Value::Host(AtmanPayload::EditProposal(Box::new(
                crate::hunk::EditProposal::compute(
                    "example.txt".into(),
                    "before".into(),
                    "after".into(),
                ),
            ))),
            Value::Err(RuntimeError::TypeMismatch {
                expected: "int".into(),
                actual: "string".into(),
            }),
        ]);
        let snapshot = StoredValue::capture(&value);
        let decoded: StoredValue =
            serde_json::from_slice(&serde_json::to_vec(&snapshot).unwrap()).unwrap();
        assert_eq!(snapshot, decoded);
        assert_eq!(StoredValue::capture(&decoded.restore().unwrap()), snapshot);
        let through_tool = Value::from_json(serde_json::to_value(&snapshot).unwrap());
        let restored: StoredValue =
            serde_json::from_value(through_tool.to_json().unwrap()).unwrap();
        assert_eq!(restored, snapshot);
        let closure = Value::Lambda {
            params: vec![],
            body: std::sync::Arc::new(atman_rt::ast::Expr::Literal(atman_rt::ast::Literal::Int(1))),
            captured_env: Default::default(),
        };
        let saved = StoredValue::capture(&Value::List(vec![closure]));
        assert!(serde_json::to_value(&saved).is_ok());
        assert!(
            saved
                .restore()
                .unwrap_err()
                .to_string()
                .contains("process-local lambda")
        );
        let error = RuntimeError::Cancelled("explicit cancellation".into());
        let failed = InvocationResult::Failed(error.clone());
        assert_eq!(
            serde_json::from_value::<InvocationResult>(serde_json::to_value(&failed).unwrap())
                .unwrap(),
            failed
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_unicode_host_payloads_remain_explicit_without_blocking_completion() {
        use std::{ffi::OsString, os::unix::ffi::OsStringExt};
        let path = PathBuf::from(OsString::from_vec(vec![0xff]));
        let saved = StoredValue::capture(&Value::Host(AtmanPayload::Path(path)));
        assert!(
            matches!(&saved, StoredValue::Unavailable(reason) if reason.contains("serialization failed"))
        );
        assert!(serde_json::to_value(&saved).is_ok());
        assert!(saved.restore().is_err());
    }
}
