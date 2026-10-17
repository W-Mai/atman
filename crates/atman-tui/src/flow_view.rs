//! Shared document storage and interaction revisions for main and floating flow views.

use std::collections::{BTreeSet, HashMap};

use crate::app::{OutputItem, OutputRevision};

#[derive(Clone, Copy)]
pub(crate) enum ToolDisclosureAction {
    Content,
    Detail { width: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OutputMutation {
    Semantic,
    SemanticPreserveSource,
    Interaction,
    Paint,
}

/// A projected transcript with tracked item identities and content, layout, and paint revisions.
///
/// Main and floating views use the same document mutations and tool disclosure operations. Restoring a message snapshot preserves disclosure by tool-call identity; layout-only changes reuse the restored content. Each view owns its document and interaction state.
#[derive(Default)]
pub struct FlowDocument {
    values: Vec<OutputItem>,
    revisions: Vec<OutputRevision>,
    animated_ids: BTreeSet<u64>,
    pub(crate) next_id: u64,
    source_revision: Option<(u64, u64)>,
    revision_clock: u64,
    structure_revision: u64,
}

impl std::ops::Deref for FlowDocument {
    type Target = [OutputItem];

    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

impl From<Vec<OutputItem>> for FlowDocument {
    fn from(values: Vec<OutputItem>) -> Self {
        let mut store = Self::default();
        store.replace(values);
        store
    }
}

impl FlowDocument {
    /// Restore only when the source changes; view interactions do not rebuild messages.
    pub(crate) fn restore_messages(
        &mut self,
        revision: OutputRevision,
        messages: &[atman_runtime::message::Message],
        output_store: Option<&atman_runtime::tools::tool_output::OutputStore>,
    ) {
        let key = (revision.id, revision.semantic);
        if self.source_revision == Some(key) {
            return;
        }
        let mut items = crate::history::flatten_messages_with_output_store(messages, output_store);
        let disclosures = self
            .iter()
            .filter_map(|item| match item {
                OutputItem::ToolDispatch { calls } => Some(calls),
                _ => None,
            })
            .flatten()
            .map(|call| (call.id.as_str(), call.disclosure))
            .collect::<HashMap<_, _>>();
        for item in &mut items {
            if let OutputItem::ToolDispatch { calls } = item {
                for call in calls {
                    if let Some(disclosure) = disclosures.get(call.id.as_str()) {
                        call.disclosure = *disclosure;
                    }
                }
            }
        }
        self.replace(items);
        self.source_revision = Some(key);
    }

    pub(crate) fn toggle_tool_disclosure(
        &mut self,
        index: usize,
        tool_use_id: &str,
        action: ToolDisclosureAction,
    ) -> bool {
        self.mutate(index, OutputMutation::Interaction, |item| {
            let OutputItem::ToolDispatch { calls } = item else {
                return false;
            };
            let Some(call) = calls.iter_mut().find(|call| call.id == tool_use_id) else {
                return false;
            };
            call.disclosure = match action {
                ToolDisclosureAction::Content => {
                    crate::output::toggle_tool_call_content_disclosure(call)
                }
                ToolDisclosureAction::Detail { width } => {
                    crate::output::toggle_tool_call_detail_disclosure(call, width)
                }
            };
            true
        })
    }

    pub(crate) fn tool_detail(&self, index: usize, tool_use_id: &str) -> Option<&OutputItem> {
        let OutputItem::ToolDispatch { calls } = self.get(index)? else {
            return None;
        };
        calls
            .iter()
            .find(|call| call.id == tool_use_id)?
            .detail
            .as_deref()
    }

    pub(crate) fn replace(&mut self, values: Vec<OutputItem>) {
        self.source_revision = None;
        self.values = values;
        self.revisions.clear();
        self.animated_ids.clear();
        self.revisions.reserve(self.values.len());
        for value in &self.values {
            self.next_id = self.next_id.wrapping_add(1);
            self.revision_clock = self.revision_clock.wrapping_add(1);
            if value.has_dynamic_paint() {
                self.animated_ids.insert(self.next_id);
            }
            self.revisions.push(OutputRevision {
                id: self.next_id,
                semantic: self.revision_clock,
                layout: self.revision_clock,
                source_generation: self.revision_clock,
                ..OutputRevision::default()
            });
        }
        self.structure_revision = self.structure_revision.wrapping_add(1);
    }

    pub(crate) fn push(&mut self, value: OutputItem) {
        self.next_id = self.next_id.wrapping_add(1);
        self.revision_clock = self.revision_clock.wrapping_add(1);
        self.values.push(value);
        self.revisions.push(OutputRevision {
            id: self.next_id,
            semantic: self.revision_clock,
            layout: self.revision_clock,
            source_generation: self.revision_clock,
            ..OutputRevision::default()
        });
        if self
            .values
            .last()
            .is_some_and(OutputItem::has_dynamic_paint)
        {
            self.animated_ids.insert(self.next_id);
        }
        self.structure_revision = self.structure_revision.wrapping_add(1);
    }

    pub(crate) fn remove(&mut self, index: usize) -> Option<OutputItem> {
        if index >= self.values.len() {
            return None;
        }
        let revision = self.revisions.remove(index);
        self.animated_ids.remove(&revision.id);
        self.structure_revision = self.structure_revision.wrapping_add(1);
        Some(self.values.remove(index))
    }

    pub(crate) fn mutate(
        &mut self,
        index: usize,
        impact: OutputMutation,
        mutation: impl FnOnce(&mut OutputItem) -> bool,
    ) -> bool {
        let Some(value) = self.values.get_mut(index) else {
            return false;
        };
        let was_animated = value.has_dynamic_paint();
        if !mutation(value) {
            return false;
        }
        let is_animated = value.has_dynamic_paint();
        self.revision_clock = self.revision_clock.wrapping_add(1);
        let revision = &mut self.revisions[index];
        if is_animated != was_animated {
            if is_animated {
                self.animated_ids.insert(revision.id);
            } else {
                self.animated_ids.remove(&revision.id);
            }
        }
        match impact {
            OutputMutation::Semantic => {
                revision.semantic = self.revision_clock;
                revision.layout = self.revision_clock;
                revision.source_generation = self.revision_clock;
            }
            OutputMutation::SemanticPreserveSource => {
                revision.semantic = self.revision_clock;
                revision.layout = self.revision_clock;
            }
            OutputMutation::Interaction => {
                revision.interaction = self.revision_clock;
                revision.layout = self.revision_clock;
            }
            OutputMutation::Paint => {
                revision.interaction = self.revision_clock;
                revision.paint = self.revision_clock;
            }
        }
        true
    }

    pub(crate) fn touch(&mut self, index: usize, impact: OutputMutation) -> bool {
        self.mutate(index, impact, |_| true)
    }

    pub(crate) fn revisions(&self) -> &[OutputRevision] {
        &self.revisions
    }

    pub(crate) fn structure_revision(&self) -> u64 {
        self.structure_revision
    }

    pub(crate) fn revision_clock(&self) -> u64 {
        self.revision_clock
    }

    pub(crate) fn has_active_animation(&self) -> bool {
        !self.animated_ids.is_empty()
    }

    pub(crate) fn index_by_id(&self, id: u64) -> Option<usize> {
        self.revisions.iter().position(|revision| revision.id == id)
    }

    #[cfg(test)]
    pub(crate) fn animated_ids(&self) -> &BTreeSet<u64> {
        &self.animated_ids
    }
}
