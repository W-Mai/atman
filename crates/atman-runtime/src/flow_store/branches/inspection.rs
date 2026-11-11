use super::*;
use crate::{
    event_log::journal::Journal,
    flow_store::{
        FlowContent, FlowFact, FlowInstanceId, SessionInspection, SessionProjection, SessionStore,
    },
};
use std::path::Path;

impl SessionInspection {
    /// Inspect branch metadata without opening an owner, interpreting a model window or repairing files.
    pub fn inspect_history_branches(&self, id: &FlowInstanceId) -> io::Result<ConversationHistory> {
        inspect(&self.dir, &self.projection, id)
    }
}

impl SessionStore {
    /// Inspect recorded branch metadata independently of execution attachment.
    pub fn inspect_history_branches(&self, id: &FlowInstanceId) -> io::Result<ConversationHistory> {
        inspect(&self.dir, &self.projection, id)
    }
}

fn inspect(
    directory: &Path,
    catalog: &SessionProjection,
    id: &FlowInstanceId,
) -> io::Result<ConversationHistory> {
    let registration = catalog
        .flows
        .iter()
        .find(|record| &record.id == id)
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "flow is not registered"))?;
    let address = FlowRef {
        session_id: catalog.descriptor.id.into(),
        instance_id: id.clone(),
    };
    let mut created = None;
    let mut changes = Vec::new();
    let (inspection, reader) = Journal::inspect_with_reader::<FlowFact>(
        &directory
            .join("flows")
            .join(id.to_string())
            .join("events.jsonl"),
        id.0.0,
        |receipt, facts| {
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
                created = Some(receipt);
                return Ok(());
            }
            for fact in facts {
                match fact {
                    FlowFact::Created(_) => return Err(invalid("duplicate Flow creation")),
                    FlowFact::HistoryHeadChanged(change) => {
                        changes.push((receipt.clone(), *change))
                    }
                    _ => {}
                }
            }
            Ok(())
        },
    )?;
    let mut projection = Projection::new(
        address,
        created.ok_or_else(|| invalid("conversation history has no Flow creation"))?,
    )?;
    for (receipt, change) in changes {
        projection.apply(change, receipt, |cursor| reader.validate_receipt(cursor))?;
    }
    projection.history.observed = inspection
        .cursor
        .ok_or_else(|| invalid("conversation history has no committed cursor"))?;
    projection.history.trailing_bytes = inspection.trailing_bytes;
    Ok(projection.history)
}
