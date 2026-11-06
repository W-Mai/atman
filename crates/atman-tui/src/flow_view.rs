//! Shared document storage and interaction revisions for main and floating flow views.

use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

use crate::app::{OutputItem, OutputRevision};

mod fold;
mod instance;
mod items;
mod live;
pub use instance::FlowViewModel;
pub(crate) use items::DocumentIdentity;
pub use items::{OutputItemIter, OutputItems, OutputSnapshot};

#[cfg(test)]
mod instance_tests;

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
///
/// Source changes are indexed by each item's latest revision. Cache readers observe them independently of pending geometry or paint, without consuming another reader's changes or retaining a log of every streamed token.
#[derive(Default)]
pub struct FlowDocument {
    identity: DocumentIdentity,
    values: Vec<Arc<OutputItem>>,
    revisions: Vec<OutputRevision>,
    animated_ids: BTreeSet<u64>,
    pub(crate) next_id: u64,
    source_revision: Option<(u64, u64)>,
    revision_clock: u64,
    structure_revision: u64,
    changes: BTreeMap<u64, usize>,
}

impl std::ops::Index<usize> for FlowDocument {
    type Output = OutputItem;
    fn index(&self, index: usize) -> &Self::Output {
        &self.values[index]
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
    pub(crate) fn identity(&self) -> DocumentIdentity {
        self.identity.clone()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }
    pub fn is_empty(&self) -> bool {
        self.values.is_empty()
    }
    pub fn get(&self, index: usize) -> Option<&OutputItem> {
        self.values.get(index).map(Arc::as_ref)
    }
    pub fn first(&self) -> Option<&OutputItem> {
        self.values.first().map(Arc::as_ref)
    }
    pub fn last(&self) -> Option<&OutputItem> {
        self.values.last().map(Arc::as_ref)
    }
    pub fn iter(&self) -> OutputItemIter<'_> {
        OutputItems::from(self).iter()
    }

    /// Capture one immutable payload and its revision without copying its contents.
    pub fn snapshot(&self, index: usize) -> Option<OutputSnapshot> {
        Some(OutputSnapshot {
            source: self.identity.clone(),
            item: self.values.get(index)?.clone(),
            revision: self.revisions[index],
        })
    }

    #[cfg(test)]
    pub(crate) fn storage_ptr(&self) -> *const Arc<OutputItem> {
        self.values.as_ptr()
    }

    fn take_tail(&mut self, start: usize) -> Vec<(Arc<OutputItem>, OutputRevision)> {
        let revisions = self.revisions.split_off(start);
        for revision in &revisions {
            self.animated_ids.remove(&revision.id);
        }
        if !revisions.is_empty() {
            self.changes.clear();
            self.structure_revision = self.structure_revision.wrapping_add(1);
            self.revision_clock = self.revision_clock.wrapping_add(1);
        }
        self.values
            .split_off(start)
            .into_iter()
            .zip(revisions)
            .collect()
    }

    fn restore_item(&mut self, item: OutputItem, revision: OutputRevision) {
        self.changes.clear();
        if item.has_dynamic_paint() {
            self.animated_ids.insert(revision.id);
        }
        self.values.push(Arc::new(item));
        self.revisions.push(revision);
        self.structure_revision = self.structure_revision.wrapping_add(1);
    }

    /// Restore only when the source changes; view interactions do not rebuild messages.
    pub(crate) fn restore_items(
        &mut self,
        revision: OutputRevision,
        mut items: Vec<OutputItem>,
    ) -> (Vec<OutputItem>, Vec<Arc<OutputItem>>) {
        let key = (revision.id, revision.semantic);
        if self.source_revision == Some(key) {
            return (items, Vec::new());
        }
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
        let retired = std::mem::take(&mut self.values);
        self.replace(items);
        self.source_revision = Some(key);
        (Vec::new(), retired)
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
        self.changes.clear();
        self.source_revision = None;
        self.values = values.into_iter().map(Arc::new).collect();
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
        self.changes.clear();
        self.next_id = self.next_id.wrapping_add(1);
        self.revision_clock = self.revision_clock.wrapping_add(1);
        self.values.push(Arc::new(value));
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
            .is_some_and(|value| value.has_dynamic_paint())
        {
            self.animated_ids.insert(self.next_id);
        }
        self.structure_revision = self.structure_revision.wrapping_add(1);
    }

    pub(crate) fn remove(&mut self, index: usize) -> Option<Arc<OutputItem>> {
        if index >= self.values.len() {
            return None;
        }
        let revision = self.revisions.remove(index);
        self.changes.clear();
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
        if !mutation(Arc::make_mut(value)) {
            return false;
        }
        self.finish_mutation(index, impact, was_animated)
    }

    pub(crate) fn replace_item(
        &mut self,
        index: usize,
        impact: OutputMutation,
        item: OutputItem,
    ) -> Option<Arc<OutputItem>> {
        let value = self.values.get_mut(index)?;
        let was_animated = value.has_dynamic_paint();
        let previous = std::mem::replace(value, Arc::new(item));
        self.finish_mutation(index, impact, was_animated);
        Some(previous)
    }

    fn finish_mutation(
        &mut self,
        index: usize,
        impact: OutputMutation,
        was_animated: bool,
    ) -> bool {
        let is_animated = self.values[index].has_dynamic_paint();
        if is_animated != was_animated {
            let id = self.revisions[index].id;
            if is_animated {
                self.animated_ids.insert(id);
            } else {
                self.animated_ids.remove(&id);
            }
        }
        self.touch(index, impact)
    }

    pub(crate) fn touch(&mut self, index: usize, impact: OutputMutation) -> bool {
        let Some(revision) = self.revisions.get_mut(index) else {
            return false;
        };
        self.revision_clock = self.revision_clock.wrapping_add(1);
        self.changes.remove(&revision.layout.max(revision.paint));
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
        self.changes.insert(self.revision_clock, index);
        true
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

    pub(crate) fn changed_indices_since(&self, revision: u64) -> impl Iterator<Item = usize> + '_ {
        self.changes
            .range((
                std::ops::Bound::Excluded(revision),
                std::ops::Bound::Unbounded,
            ))
            .map(|(_, index)| *index)
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

#[cfg(test)]
mod change_tests {
    use super::*;

    #[test]
    fn snapshots_share_payloads_and_isolate_document_identity_and_mutations() {
        let item = || OutputItem::AssistantMd {
            md: "immutable 汉\n".repeat(24_000),
            streaming: true,
            retried: false,
        };
        let mut document = FlowDocument::from(vec![item(), OutputItem::Divider]);
        let captured = document.snapshot(0).unwrap();
        assert!(std::ptr::eq(captured.item(), &document[0]));
        assert!(captured.same_version(&captured.clone()));
        let other = FlowDocument::from(vec![item()]);
        assert_eq!(captured.revision(), other.snapshot(0).unwrap().revision());
        assert!(!captured.same_version(&other.snapshot(0).unwrap()));
        let unrelated = document.snapshot(1).unwrap();
        document.mutate(0, OutputMutation::SemanticPreserveSource, |item| {
            let OutputItem::AssistantMd { md, .. } = item else {
                unreachable!()
            };
            md.push_str("new tail");
            true
        });
        let current = document.snapshot(0).unwrap();
        assert!(!captured.same_version(&current));
        assert!(!std::ptr::eq(captured.item(), current.item()));
        assert!(unrelated.same_version(&document.snapshot(1).unwrap()));
        assert_eq!(
            captured.revision().source_generation,
            current.revision().source_generation
        );
        assert!(current.revision().semantic > captured.revision().semantic);
        assert!(
            matches!(captured.item(), OutputItem::AssistantMd { md, .. } if !md.ends_with("new tail"))
        );
        assert!(
            matches!(current.item(), OutputItem::AssistantMd { md, .. } if md.ends_with("new tail"))
        );
        document.touch(0, OutputMutation::Paint);
        assert!(!current.same_version(&document.snapshot(0).unwrap()));
        let removed = document.remove(0).unwrap();
        assert!(std::ptr::eq(current.item(), removed.as_ref()));
        assert!(unrelated.same_version(&document.snapshot(0).unwrap()));
        assert!(!document.has_active_animation());
        let replacement = item();
        let OutputItem::AssistantMd { md, .. } = &replacement else {
            unreachable!()
        };
        let replacement_bytes = md.as_ptr();
        let retired = document
            .replace_item(0, OutputMutation::Semantic, replacement)
            .unwrap();
        assert!(std::ptr::eq(unrelated.item(), retired.as_ref()));
        assert!(
            matches!(document.get(0), Some(OutputItem::AssistantMd { md, .. }) if md.as_ptr() == replacement_bytes)
        );
        assert!(document.has_active_animation());
        drop(document);
        assert!(
            matches!(captured.item(), OutputItem::AssistantMd { md, .. } if !md.ends_with("new tail"))
        );
    }

    #[test]
    fn latest_changes_are_bounded_and_do_not_consume_another_caches_observation() {
        let item = || OutputItem::AssistantMd {
            md: String::new(),
            streaming: true,
            retried: false,
        };
        let mut document = FlowDocument::from(vec![item(), item()]);
        let before = document.revision_clock();
        document.touch(0, OutputMutation::Paint);
        let first = document.revision_clock();
        for _ in 0..1_000 {
            document.mutate(1, OutputMutation::SemanticPreserveSource, |item| {
                let OutputItem::AssistantMd { md, .. } = item else {
                    unreachable!()
                };
                md.push('x');
                true
            });
        }
        document.touch(1, OutputMutation::Paint);
        assert_eq!(document.changes.len(), 2);
        assert_eq!(
            document.changed_indices_since(before).collect::<Vec<_>>(),
            [0, 1]
        );
        assert_eq!(
            document.changed_indices_since(first).collect::<Vec<_>>(),
            [1]
        );
        assert_eq!(
            document.changed_indices_since(before).collect::<Vec<_>>(),
            [0, 1]
        );
        document.remove(0);
        assert!(
            document.changes.is_empty(),
            "structural identity comparison replaces positional hints"
        );
        let before = document.revision_clock();
        document.touch(0, OutputMutation::Semantic);
        assert_eq!(
            document.changed_indices_since(before).collect::<Vec<_>>(),
            [0]
        );
    }
}
