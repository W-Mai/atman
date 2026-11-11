//! Shared journal selection for complete conversation and checkpointed model windows.

use std::io;

use super::{FlowFact, inbox::InputContent, invalid, position::FlowPosition};
use crate::message::Message;

/// The model window may contain compacted context; history retains original messages.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationReadKind {
    History,
    ModelContext,
}

pub(super) struct ConversationPlan {
    base: Option<(FlowPosition, bool)>,
    suffix: Vec<FlowPosition>,
    kind: ConversationReadKind,
}

impl ConversationPlan {
    /// Select metadata before loading bodies. Visibility includes both ancestry and cutoff.
    pub(super) fn select<'a>(
        kind: ConversationReadKind,
        activation: Option<&FlowPosition>,
        checkpoints: impl DoubleEndedIterator<Item = &'a FlowPosition>,
        inputs: impl Iterator<Item = &'a FlowPosition>,
        publications: impl Iterator<Item = &'a FlowPosition>,
        visible: impl Fn(&FlowPosition) -> bool,
    ) -> Self {
        let checkpoint = (kind == ConversationReadKind::ModelContext)
            .then(|| checkpoints.rev().find(|receipt| visible(receipt)))
            .flatten();
        let after = checkpoint.map_or(0, FlowPosition::sequence);
        let base = checkpoint
            .map(|receipt| (receipt.clone(), true))
            .or_else(|| {
                activation
                    .filter(|receipt| visible(receipt))
                    .map(|receipt| (receipt.clone(), false))
            });
        let mut suffix: Vec<_> = inputs
            .chain(publications)
            .filter(|receipt| receipt.sequence() > after && visible(receipt))
            .cloned()
            .collect();
        suffix.sort_by_key(FlowPosition::sequence);
        suffix.dedup();
        Self { base, suffix, kind }
    }

    pub(super) fn read(
        self,
        mut read: impl FnMut(&FlowPosition) -> io::Result<Vec<FlowFact>>,
    ) -> io::Result<Vec<Message>> {
        let mut messages = if let Some((receipt, checkpoint)) = self.base {
            read(&receipt)?
                .into_iter()
                .find_map(|fact| match (checkpoint, fact) {
                    (true, FlowFact::ContextCheckpoint(record)) => Some(record.messages),
                    (false, FlowFact::Activated(record)) => Some(record.input.inherited_context),
                    _ => None,
                })
                .ok_or_else(|| invalid("conversation base is absent from its recorded batch"))?
        } else {
            Vec::new()
        };
        for receipt in self.suffix {
            for fact in read(&receipt)? {
                match fact {
                    FlowFact::InputCommitted(record) => match record.content {
                        InputContent::Context { messages: content }
                        | InputContent::InvocationInput {
                            messages: content, ..
                        } => messages.extend(content),
                        InputContent::WaitingContinuation { .. } => {}
                    },
                    FlowFact::MessagePublished(record) if record.context_run.is_none() => {
                        messages.push(record.message);
                    }
                    _ => {}
                }
            }
        }
        if self.kind == ConversationReadKind::ModelContext {
            super::publication::trim_context(&mut messages);
        }
        Ok(messages)
    }
}
