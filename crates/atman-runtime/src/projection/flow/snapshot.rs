//! Explicit original record namespaces for data-only conversation snapshots.

use super::*;
use crate::{event_log::journal::CommitReceipt, flow_store::source_capture::ConversationCutoff};

/// Provenance is not a destination lease, input acceptance or permission grant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConversationRecordSource {
    pub flow: FlowRef,
    pub receipt: CommitReceipt,
    pub record: FlowRecordId,
}

/// Historical presentation with its original namespace and optional authored User boundary.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConversationHistoryRecord {
    pub source: ConversationRecordSource,
    pub user_turn: Option<ConversationCutoff>,
    pub record: FlowRecord,
}

impl FlowTranscript {
    /// Consume a completed durable display projection without exporting inbox, control or invocation state.
    pub fn into_history_records(self) -> io::Result<Vec<ConversationHistoryRecord>> {
        let cursor = self
            .cursor()
            .ok_or_else(|| invalid("historical projection has no committed cursor"))?;
        cursor.require_durable()?;
        if self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.through() != cursor)
        {
            return Err(invalid(
                "historical projection has not reached its captured boundary",
            ));
        }
        // Resolve authored boundaries while the original entry and receipt indexes are intact.
        let mut boundaries: HashMap<_, _> = self
            .entries
            .iter()
            .filter_map(|entry| {
                self.user_turn_boundary(entry.id)
                    .map(|boundary| (entry.id, boundary))
            })
            .collect();
        self.entries
            .into_iter()
            .map(|entry| {
                let index = usize::try_from(
                    entry
                        .id
                        .commit
                        .checked_sub(1)
                        .ok_or_else(|| invalid("historical record has no commit"))?,
                )
                .map_err(|_| invalid("historical record position is too large"))?;
                let receipt = self
                    .receipts
                    .get(index)
                    .ok_or_else(|| invalid("historical record has no receipt"))?
                    .require_durable()?
                    .clone();
                Ok(ConversationHistoryRecord {
                    source: ConversationRecordSource {
                        flow: self.flow.clone(),
                        receipt,
                        record: entry.id,
                    },
                    user_turn: boundaries.remove(&entry.id),
                    record: entry.record,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_rt::{RunId, TurnId, TurnLease};
    use uuid::Uuid;

    #[test]
    fn serializable_history_inputs_cannot_carry_invocation_or_continuation_parameters() {
        let lease = TurnLease {
            flow: FlowRef {
                session_id: Uuid::now_v7().into(),
                instance_id: crate::flow_store::FlowInstanceId::now(),
            },
            turn: TurnId::now(),
            run: RunId::now(),
            epoch: 1,
        };
        let message_ids = vec![Uuid::now_v7()];
        let input = CommittedInput {
            lease: lease.clone(),
            message_ids: message_ids.clone(),
            content: InputContent::InvocationInput {
                command: Some(Uuid::now_v7()),
                arguments: BTreeMap::from([(
                    "argument".into(),
                    serde_json::Value::String("argument-value".into()),
                )]),
                invocation: BTreeMap::from([(
                    "setting".into(),
                    serde_json::Value::String("invocation-value".into()),
                )]),
                messages: vec![Message::user_text(lease.turn.clone(), "authored body")],
            },
        };
        let record = FlowRecord::Input(Box::new(input.into()));
        let encoded = serde_json::to_value(&record).unwrap();
        let data = encoded["data"].as_object().unwrap();
        assert_eq!(
            data.keys()
                .map(String::as_str)
                .collect::<std::collections::BTreeSet<_>>(),
            ["lease", "message_ids", "messages"].into()
        );
        let decoded: FlowRecord = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.shared_messages()[0].text_concat(), "authored body");
        let FlowRecord::Input(input) = decoded else {
            panic!("input")
        };
        assert_eq!(input.lease, lease);
        assert_eq!(input.message_ids, message_ids);
        let continuation = ConversationInput::from(CommittedInput {
            lease,
            message_ids,
            content: InputContent::WaitingContinuation {
                correlation: Uuid::now_v7(),
                value: serde_json::Value::String("reply-value".into()),
            },
        });
        assert!(continuation.messages.is_empty());
        assert!(
            !serde_json::to_string(&continuation)
                .unwrap()
                .contains("reply-value")
        );
    }
}
