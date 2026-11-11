use super::*;
use crate::{
    event_log::journal::{Journal, JournalReader},
    flow_store::{
        FlowContent, FlowFact, FlowInstanceId, SessionInspection, SessionProjection, SessionStore,
        conversation::ConversationReadKind, replay::FlowReplay,
    },
};
use std::path::Path;

impl SessionInspection {
    /// Inspect branch metadata without opening an owner, interpreting a model window or repairing files.
    pub fn inspect_history_branches(&self, id: &FlowInstanceId) -> io::Result<ConversationHistory> {
        inspect(&self.dir, &self.projection, id, false).map(|(history, _, _)| history)
    }

    /// Read a fixed branch conversation on a storage worker without selecting it for execution.
    ///
    /// This complete read is not a bounded UI page or an independently retained Fork snapshot. Images retain their original managed ownership and checkpoint selection does not certify an executable boundary.
    pub fn read_branch_conversation(
        &self,
        source: &SourceSelection,
        kind: ConversationReadKind,
    ) -> io::Result<BranchConversation> {
        read_conversation(&self.dir, &self.projection, source, kind)
    }
}

impl SessionStore {
    /// Inspect recorded branch metadata independently of execution attachment.
    pub fn inspect_history_branches(&self, id: &FlowInstanceId) -> io::Result<ConversationHistory> {
        inspect(&self.dir, &self.projection, id, false).map(|(history, _, _)| history)
    }
}

/// Original messages or a model window selected from one immutable journal observation.
pub struct BranchConversation {
    pub source: SourceSelection,
    pub observed_head: HistoryHead,
    pub messages: Vec<crate::message::Message>,
}

fn read_conversation(
    directory: &Path,
    catalog: &SessionProjection,
    source: &SourceSelection,
    kind: ConversationReadKind,
) -> io::Result<BranchConversation> {
    if source.flow.session_id.0 != catalog.descriptor.id {
        return Err(invalid("conversation source belongs to another Session"));
    }
    let (history, mut reader, replay) =
        inspect(directory, catalog, &source.flow.instance_id, true)?;
    reader.validate_receipt(&source.cursor)?;
    history.branch(source.branch)?;
    if !history.contains(source.branch, source.cursor.sequence)? {
        return Err(invalid(
            "conversation cutoff is outside the selected branch",
        ));
    }
    let plan = replay
        .ok_or_else(|| invalid("conversation replay is unavailable"))?
        .conversation_plan(&history, source.branch, source.cursor.sequence, kind)?;
    let flow_directory = directory
        .join("flows")
        .join(source.flow.instance_id.to_string());
    let messages = plan.read(|receipt| {
        let batch = reader.read_batch::<FlowFact>(receipt.require_durable()?)?;
        super::super::images::resolve(&flow_directory, &batch.facts)
    })?;
    Ok(BranchConversation {
        source: source.clone(),
        observed_head: history.head,
        messages,
    })
}

fn inspect(
    directory: &Path,
    catalog: &SessionProjection,
    id: &FlowInstanceId,
    capture_conversation: bool,
) -> io::Result<(ConversationHistory, JournalReader, Option<FlowReplay>)> {
    let registration = catalog
        .flows
        .iter()
        .find(|record| &record.id == id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "flow is not registered"))?;
    let address = FlowRef {
        session_id: catalog.descriptor.id.into(),
        instance_id: id.clone(),
    };
    let mut projection = None;
    let mut replay = capture_conversation.then(FlowReplay::default);
    let flow_directory = directory.join("flows").join(id.to_string());
    let (inspection, reader) = Journal::inspect_with_prefix_reader::<FlowFact>(
        &directory
            .join("flows")
            .join(id.to_string())
            .join("events.jsonl"),
        id.0.0,
        |receipt, facts, prefix| {
            FlowFact::validate_batch(&facts)?;
            if receipt.sequence == 1 {
                let [FlowFact::Created(descriptor)] = facts.as_slice() else {
                    return Err(invalid(
                        "conversation history must begin with its Flow creation",
                    ));
                };
                if descriptor.address != address
                    || descriptor.creation_request != registration.creation_request
                    || !matches!(descriptor.content, FlowContent::Entry(_))
                {
                    return Err(invalid(
                        "conversation history requires the registered native Flow",
                    ));
                }
                projection = Some(Projection::new(address.clone(), receipt.clone())?);
                if let Some(replay) = &mut replay {
                    replay.apply(&flow_directory, &address, receipt.into(), facts)?;
                }
                return Ok(());
            }
            let mut head_changed = false;
            for fact in &facts {
                match fact {
                    FlowFact::Created(_) => return Err(invalid("duplicate Flow creation")),
                    FlowFact::HistoryHeadChanged(change) => {
                        projection
                            .as_mut()
                            .ok_or_else(|| invalid("conversation history has no Flow creation"))?
                            .apply((**change).clone(), receipt.clone(), |cursor| {
                                if cursor == &receipt {
                                    Ok(())
                                } else {
                                    prefix.validate_receipt(cursor)
                                }
                            })?;
                        head_changed = true;
                    }
                    _ => {}
                }
            }
            if let Some(replay) = &mut replay {
                if head_changed {
                    replay.select_conversation(
                        &flow_directory,
                        &projection.as_ref().expect("validated creation").history,
                        &prefix,
                    )?;
                } else {
                    replay.apply(&flow_directory, &address, receipt.into(), facts)?;
                }
            }
            Ok(())
        },
    )?;
    let mut projection =
        projection.ok_or_else(|| invalid("conversation history has no Flow creation"))?;
    projection.history.observed = inspection
        .cursor
        .ok_or_else(|| invalid("conversation history has no committed cursor"))?;
    projection.history.trailing_bytes = inspection.trailing_bytes;
    Ok((projection.history, reader, replay))
}
