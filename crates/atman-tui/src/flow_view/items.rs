//! Borrowed access to owned and immutable shared output items.

use std::{ops::Range, sync::Arc};

use crate::app::{OutputItem, OutputRevision};

#[derive(Clone, Debug, Default)]
pub(crate) struct DocumentIdentity(Arc<()>);

impl PartialEq for DocumentIdentity {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for DocumentIdentity {}

/// An immutable item captured at its document revision.
#[derive(Clone)]
pub struct OutputSnapshot {
    pub(super) source: DocumentIdentity,
    pub(super) item: Arc<OutputItem>,
    pub(super) revision: OutputRevision,
}

impl OutputSnapshot {
    pub fn item(&self) -> &OutputItem {
        &self.item
    }

    pub fn revision(&self) -> OutputRevision {
        self.revision
    }

    pub fn same_version(&self, other: &Self) -> bool {
        self.source == other.source && self.revision == other.revision
    }
}

/// A read-only output sequence without materializing or copying its payloads.
#[derive(Clone, Copy)]
pub enum OutputItems<'a> {
    Owned(&'a [OutputItem]),
    Shared(&'a [Arc<OutputItem>]),
}

impl<'a> OutputItems<'a> {
    pub fn len(self) -> usize {
        match self {
            Self::Owned(items) => items.len(),
            Self::Shared(items) => items.len(),
        }
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }

    pub fn get(self, index: usize) -> Option<&'a OutputItem> {
        match self {
            Self::Owned(items) => items.get(index),
            Self::Shared(items) => items.get(index).map(Arc::as_ref),
        }
    }

    pub fn first(self) -> Option<&'a OutputItem> {
        self.get(0)
    }

    pub fn last(self) -> Option<&'a OutputItem> {
        self.len().checked_sub(1).and_then(|index| self.get(index))
    }

    pub fn slice(self, range: Range<usize>) -> Self {
        match self {
            Self::Owned(items) => Self::Owned(&items[range]),
            Self::Shared(items) => Self::Shared(&items[range]),
        }
    }

    pub fn iter(self) -> OutputItemIter<'a> {
        OutputItemIter {
            items: self,
            indices: 0..self.len(),
        }
    }
}

impl std::ops::Index<usize> for OutputItems<'_> {
    type Output = OutputItem;

    fn index(&self, index: usize) -> &Self::Output {
        self.get(index).expect("output item index out of bounds")
    }
}

impl<'a> From<&'a [OutputItem]> for OutputItems<'a> {
    fn from(items: &'a [OutputItem]) -> Self {
        Self::Owned(items)
    }
}

impl<'a, const N: usize> From<&'a [OutputItem; N]> for OutputItems<'a> {
    fn from(items: &'a [OutputItem; N]) -> Self {
        Self::Owned(items)
    }
}

impl<'a> From<&'a Vec<OutputItem>> for OutputItems<'a> {
    fn from(items: &'a Vec<OutputItem>) -> Self {
        Self::Owned(items)
    }
}

impl<'a> From<&'a super::FlowDocument> for OutputItems<'a> {
    fn from(document: &'a super::FlowDocument) -> Self {
        Self::Shared(&document.values)
    }
}

impl<'a> From<&'a super::FlowViewModel> for OutputItems<'a> {
    fn from(document: &'a super::FlowViewModel) -> Self {
        Self::from(&**document)
    }
}

#[derive(Clone)]
pub struct OutputItemIter<'a> {
    items: OutputItems<'a>,
    indices: Range<usize>,
}

impl<'a> Iterator for OutputItemIter<'a> {
    type Item = &'a OutputItem;

    fn next(&mut self) -> Option<Self::Item> {
        self.indices.next().and_then(|index| self.items.get(index))
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.indices.nth(n).and_then(|index| self.items.get(index))
    }

    fn last(self) -> Option<Self::Item> {
        self.indices.last().and_then(|index| self.items.get(index))
    }

    fn count(self) -> usize {
        self.indices.len()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.indices.size_hint()
    }
}

impl DoubleEndedIterator for OutputItemIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.indices
            .next_back()
            .and_then(|index| self.items.get(index))
    }

    fn nth_back(&mut self, n: usize) -> Option<Self::Item> {
        self.indices
            .nth_back(n)
            .and_then(|index| self.items.get(index))
    }
}

impl ExactSizeIterator for OutputItemIter<'_> {}
impl std::iter::FusedIterator for OutputItemIter<'_> {}

impl<'a> IntoIterator for OutputItems<'a> {
    type Item = &'a OutputItem;
    type IntoIter = OutputItemIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_and_shared_views_borrow_original_items_with_identical_navigation() {
        let owned = vec![
            OutputItem::Divider,
            OutputItem::AssistantMd {
                md: "body 汉".into(),
                streaming: false,
                retried: false,
            },
        ];
        let shared = owned.iter().cloned().map(Arc::new).collect::<Vec<_>>();
        for items in [OutputItems::Owned(&owned), OutputItems::Shared(&shared)] {
            assert_eq!(items.len(), 2);
            assert!(!items.is_empty());
            let mut iter = items.iter();
            assert_eq!(iter.len(), 2);
            assert!(std::ptr::eq(iter.next().unwrap(), items.first().unwrap()));
            assert!(std::ptr::eq(
                iter.next_back().unwrap(),
                items.last().unwrap()
            ));
            assert_eq!(iter.len(), 0);
            assert!(iter.next().is_none() && iter.next_back().is_none());
            let tail = items.slice(1..2);
            assert!(std::ptr::eq(tail.first().unwrap(), items.get(1).unwrap()));
            assert!(items.get(2).is_none());
            assert!(items.slice(2..2).is_empty());
            assert!(std::ptr::eq(
                items.iter().nth(1).unwrap(),
                items.last().unwrap()
            ));
            assert!(std::ptr::eq(
                items.iter().nth_back(1).unwrap(),
                items.first().unwrap()
            ));
        }
        assert!(std::ptr::eq(
            OutputItems::Owned(&owned).get(1).unwrap(),
            &owned[1]
        ));
        assert!(std::ptr::eq(
            OutputItems::Shared(&shared).get(1).unwrap(),
            shared[1].as_ref()
        ));
        assert!(shared.iter().all(|item| Arc::strong_count(item) == 1));
        let context = crate::output::RenderCtx {
            panel_width: 23,
            ..crate::output::RenderCtx::empty()
        };
        assert_eq!(
            crate::output::build_lines(OutputItems::Owned(&owned), &context),
            crate::output::build_lines(OutputItems::Shared(&shared), &context)
        );
        assert_eq!(
            crate::output::build_lines_with_ranges(OutputItems::Owned(&owned), 23, &context),
            crate::output::build_lines_with_ranges(OutputItems::Shared(&shared), 23, &context)
        );
    }
}
