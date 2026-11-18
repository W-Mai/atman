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
    pub(super) fn positions(&self) -> Vec<FlowPosition> {
        self.base
            .iter()
            .map(|(position, _)| position.clone())
            .chain(self.suffix.iter().cloned())
            .collect()
    }
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

    /// Select a bounded recent subset before reading bodies. Input iterators are in commit order.
    /// Each visited metadata row is charged, including hidden and empty rows.
    pub(super) fn select_recent(
        activation: Option<u64>,
        inputs: impl DoubleEndedIterator<Item = (u64, bool)>,
        publications: impl DoubleEndedIterator<Item = (u64, bool)>,
        visible: impl Fn(u64) -> bool,
        maximum: std::num::NonZeroUsize,
        max_metadata: std::num::NonZeroUsize,
    ) -> Result<RecentConversationPlan, RecentSelectionCapacity> {
        let mut inputs = inputs.rev();
        let mut publications = publications.rev();
        let mut input = inputs.next();
        let mut publication = publications.next();
        let mut inspected = 0;
        let mut selected = Vec::new();
        let mut earlier = false;
        loop {
            let use_input = match (input, publication) {
                (None, None) => break,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (Some(input), Some(publication)) => input.0 >= publication.0,
            };
            if inspected == max_metadata.get() {
                return Err(RecentSelectionCapacity {
                    required_at_least: inspected.saturating_add(1),
                    allowed: max_metadata.get(),
                });
            }
            inspected += 1;
            let (position, shared) = if use_input {
                let marker = input.expect("selected input");
                input = inputs.next();
                marker
            } else {
                let marker = publication.expect("selected publication");
                publication = publications.next();
                marker
            };
            if shared && visible(position) && selected.last() != Some(&position) {
                if selected.len() == maximum.get() {
                    earlier = true;
                    break;
                }
                selected.push(position);
            }
        }
        let activation = activation.filter(|position| visible(*position));
        let base = activation.filter(|_| {
            if earlier || selected.len() == maximum.get() {
                earlier = true;
                false
            } else {
                true
            }
        });
        Ok(RecentConversationPlan {
            base,
            suffix: selected,
            has_earlier_batches: earlier,
            inspected_metadata: inspected,
        })
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

pub(super) struct RecentConversationPlan {
    base: Option<u64>,
    suffix: Vec<u64>,
    pub(super) has_earlier_batches: bool,
    pub(super) inspected_metadata: usize,
}

pub(super) struct RecentSelectionCapacity {
    pub(super) required_at_least: usize,
    pub(super) allowed: usize,
}

impl RecentConversationPlan {
    pub(super) fn resolve(
        self,
        mut receipt: impl FnMut(u64) -> io::Result<FlowPosition>,
    ) -> io::Result<ConversationPlan> {
        let base = self.base.map(&mut receipt).transpose()?;
        let suffix = self
            .suffix
            .into_iter()
            .map(receipt)
            .collect::<io::Result<Vec<_>>>()?;
        Ok(ConversationPlan::select(
            ConversationReadKind::History,
            base.as_ref(),
            std::iter::empty(),
            suffix.iter(),
            std::iter::empty(),
            |_| true,
        ))
    }
}
