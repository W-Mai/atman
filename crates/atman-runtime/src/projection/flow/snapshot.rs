//! Explicit original record namespaces for data-only conversation snapshots.

use super::*;
use crate::{event_log::journal::CommitReceipt, flow_store::source_capture::ConversationCutoff};
use std::collections::HashSet;

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

/// A destination-local position assigned to one imported display record.
///
/// The receipt belongs to the destination import publication. The ordinal is the
/// record's position within that publication and never reuses the source
/// journal's commit/fact/item coordinates.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ImportedRecordPosition {
    pub receipt: CommitReceipt,
    pub ordinal: usize,
}

/// One immutable display record after destination import.
///
/// `source` remains the authoritative historical provenance. `position` is the
/// only local identity; source leases and source receipts are not destination
/// execution state.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportedHistoryRecord {
    pub position: ImportedRecordPosition,
    pub source: ConversationRecordSource,
    pub user_turn: Option<ConversationCutoff>,
    pub record: FlowRecord,
}

/// A validated, destination-owned prefix of a native Fork.
///
/// This type is a display/history seed only. It intentionally has no
/// conversion to `LaunchInput`, no source control state and no executable lease
/// rebasing. Destination acceptance must supply the real durable import receipt
/// before constructing it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ImportedHistory {
    pub receipt: CommitReceipt,
    pub records: Vec<ImportedHistoryRecord>,
}

impl ImportedHistory {
    /// Bind source records to the receipt returned by the destination import
    /// publication. The source namespace is checked before any record is
    /// accepted, and native imports reject legacy/archive records.
    pub fn bind(
        receipt: CommitReceipt,
        records: impl IntoIterator<Item = ConversationHistoryRecord>,
    ) -> io::Result<Self> {
        if receipt.sequence == 0 {
            return Err(invalid("import receipt has no destination position"));
        }

        let records: Vec<_> = records.into_iter().collect();
        let source_flow = records.first().map(|record| record.source.flow.clone());
        let mut seen = HashSet::with_capacity(records.len());
        let mut previous = None;
        let mut imported = Vec::with_capacity(records.len());

        for (ordinal, record) in records.into_iter().enumerate() {
            if source_flow
                .as_ref()
                .is_some_and(|flow| flow != &record.source.flow)
            {
                return Err(invalid("import prefix contains multiple source Flows"));
            }
            if record.source.receipt.sequence != record.source.record.commit {
                return Err(invalid("source record position disagrees with its receipt"));
            }
            if record.source.receipt.journal_id != record.source.flow.instance_id.0.0 {
                return Err(invalid("source record receipt belongs to another Flow"));
            }
            if matches!(
                &record.record,
                FlowRecord::Legacy(_) | FlowRecord::Continuation { .. }
            ) {
                return Err(invalid(
                    "native import cannot carry legacy or continuation records",
                ));
            }
            let key = (
                record.source.flow.clone(),
                record.source.receipt.journal_id,
                record.source.receipt.sequence,
                record.source.receipt.digest.clone(),
                record.source.record,
            );
            if !seen.insert(key) {
                return Err(invalid("import prefix contains duplicate source records"));
            }
            let order = (
                record.source.receipt.sequence,
                record.source.record.commit,
                record.source.record.fact,
                record.source.record.item,
            );
            if previous.is_some_and(|previous| order <= previous) {
                return Err(invalid("import prefix source records are out of order"));
            }
            previous = Some(order);
            imported.push(ImportedHistoryRecord {
                position: ImportedRecordPosition {
                    receipt: receipt.clone(),
                    ordinal,
                },
                source: record.source,
                user_turn: record.user_turn,
                record: record.record,
            });
        }

        Ok(Self {
            receipt,
            records: imported,
        })
    }

    /// Bind an immutable history prefix to the destination journal that published it.
    ///
    /// The imported positions are display identities only. Source receipts and leases stay in
    /// their original namespace and are never accepted as destination execution state.
    pub fn bind_for_destination(
        destination: &FlowRef,
        receipt: CommitReceipt,
        records: impl IntoIterator<Item = ConversationHistoryRecord>,
    ) -> io::Result<Self> {
        if receipt.journal_id != destination.instance_id.0.0 || receipt.sequence == 0 {
            return Err(invalid(
                "import receipt does not belong to the destination Flow",
            ));
        }
        let history = Self::bind(receipt, records)?;
        history.validate_for_destination(destination)?;
        Ok(history)
    }

    /// Recheck destination ownership after deserialization or before a replay decision.
    pub fn validate_for_destination(&self, destination: &FlowRef) -> io::Result<()> {
        if destination.session_id.0.is_nil()
            || destination.instance_id.0.0.is_nil()
            || self.receipt.journal_id != destination.instance_id.0.0
            || self.receipt.sequence == 0
        {
            return Err(invalid(
                "import history receipt does not belong to the destination Flow",
            ));
        }
        for (ordinal, imported) in self.records.iter().enumerate() {
            if imported.position.receipt != self.receipt
                || imported.position.ordinal != ordinal
                || imported.source.flow == *destination
                || imported.source.receipt.sequence == 0
                || imported.source.receipt.journal_id != imported.source.flow.instance_id.0.0
                || imported.source.receipt.sequence != imported.source.record.commit
            {
                return Err(invalid(
                    "import history contains an invalid source boundary",
                ));
            }
            if matches!(
                &imported.record,
                FlowRecord::Legacy(_) | FlowRecord::Continuation { .. }
            ) {
                return Err(invalid(
                    "native import cannot carry legacy or continuation records",
                ));
            }
            let owner = match &imported.record {
                FlowRecord::Input(input) => Some(&input.lease.flow),
                FlowRecord::Publication(publication) => Some(&publication.lease.flow),
                FlowRecord::Completion(completion) => Some(&completion.lease.flow),
                FlowRecord::Execution(event) => Some(event.scope.flow()),
                FlowRecord::Checkpoint(_)
                | FlowRecord::Legacy(_)
                | FlowRecord::Continuation { .. } => None,
            };
            if owner.is_some_and(|owner| owner != &imported.source.flow) {
                return Err(invalid(
                    "import history record carries a foreign source lease",
                ));
            }
        }
        Ok(())
    }

    pub fn source_flow(&self) -> Option<&FlowRef> {
        self.records.first().map(|record| &record.source.flow)
    }

    pub fn record(&self, position: usize) -> Option<&ImportedHistoryRecord> {
        self.records.get(position)
    }
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

    fn imported_record(flow: &FlowRef, sequence: u64, item: usize) -> ConversationHistoryRecord {
        let receipt = CommitReceipt {
            journal_id: flow.instance_id.0.0,
            sequence,
            digest: format!("source-{sequence}"),
        };
        let lease = atman_rt::TurnLease {
            flow: flow.clone(),
            turn: TurnId::now(),
            run: RunId::now(),
            epoch: sequence,
        };
        ConversationHistoryRecord {
            source: ConversationRecordSource {
                flow: flow.clone(),
                receipt,
                record: FlowRecordId {
                    commit: sequence,
                    fact: 0,
                    item,
                },
            },
            user_turn: None,
            record: FlowRecord::Input(Box::new(ConversationInput {
                lease,
                message_ids: vec![Uuid::now_v7()],
                messages: vec![Message::user_text(
                    TurnId::now(),
                    format!("record-{sequence}"),
                )],
            })),
        }
    }

    #[test]
    fn imported_history_assigns_destination_positions_without_rebasing_source() {
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: crate::flow_store::FlowInstanceId::now(),
        };
        let records = vec![imported_record(&flow, 1, 0), imported_record(&flow, 2, 1)];
        let destination = CommitReceipt {
            journal_id: Uuid::now_v7(),
            sequence: 7,
            digest: "destination-import".into(),
        };
        let imported = ImportedHistory::bind(destination.clone(), records.clone()).unwrap();
        assert_eq!(imported.source_flow(), Some(&flow));
        assert_eq!(imported.records.len(), 2);
        assert_eq!(imported.records[0].position.ordinal, 0);
        assert_eq!(imported.records[1].position.ordinal, 1);
        assert_eq!(imported.records[0].position.receipt, destination);
        assert_eq!(imported.records[1].source, records[1].source);
        assert_eq!(imported.records[1].source.record.commit, 2);

        let decoded: ImportedHistory =
            serde_json::from_value(serde_json::to_value(&imported).unwrap()).unwrap();
        assert_eq!(decoded.records[0].position, imported.records[0].position);
        assert_eq!(decoded.records[1].source, imported.records[1].source);
    }

    #[test]
    fn imported_history_rejects_mixed_or_reordered_source_records() {
        let first_flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: crate::flow_store::FlowInstanceId::now(),
        };
        let second_flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: crate::flow_store::FlowInstanceId::now(),
        };
        let destination = CommitReceipt {
            journal_id: Uuid::now_v7(),
            sequence: 1,
            digest: "destination-import".into(),
        };
        assert!(
            ImportedHistory::bind(
                destination.clone(),
                [
                    imported_record(&first_flow, 1, 0),
                    imported_record(&second_flow, 2, 0)
                ]
            )
            .is_err()
        );

        let first = imported_record(&first_flow, 1, 0);
        let mut second = imported_record(&first_flow, 2, 1);
        second.source.record.commit = 1;
        assert!(ImportedHistory::bind(destination, [first, second]).is_err());
    }

    #[test]
    fn imported_history_binds_destination_positions_without_rebasing_source_leases() {
        let source = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: crate::flow_store::FlowInstanceId::now(),
        };
        let destination = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: crate::flow_store::FlowInstanceId::now(),
        };
        let receipt = CommitReceipt {
            journal_id: destination.instance_id.0.0,
            sequence: 2,
            digest: "destination-import".into(),
        };
        let history = ImportedHistory::bind_for_destination(
            &destination,
            receipt.clone(),
            [imported_record(&source, 1, 0)],
        )
        .unwrap();
        assert_eq!(history.receipt, receipt);
        assert_eq!(history.records[0].source.flow, source);

        let mut foreign = imported_record(&source, 1, 0);
        let FlowRecord::Input(input) = &mut foreign.record else {
            panic!("input fixture")
        };
        input.lease.flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: crate::flow_store::FlowInstanceId::now(),
        };
        assert!(ImportedHistory::bind_for_destination(&destination, receipt, [foreign]).is_err());
    }
}
