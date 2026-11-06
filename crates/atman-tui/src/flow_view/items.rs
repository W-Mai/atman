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

/// Shared source ownership for a root item or one captured tool detail.
#[derive(Clone)]
pub(crate) struct OutputCapture {
    root: Arc<OutputItem>,
    tool: Option<String>,
}

impl From<Arc<OutputItem>> for OutputCapture {
    fn from(root: Arc<OutputItem>) -> Self {
        Self { root, tool: None }
    }
}

impl OutputCapture {
    pub(crate) fn detail(mut self, id: String) -> Self {
        self.tool = Some(id);
        self
    }

    pub(crate) fn item(&self) -> Option<&OutputItem> {
        let Some(id) = self.tool.as_ref() else {
            return Some(&self.root);
        };
        let OutputItem::ToolDispatch { calls } = self.root.as_ref() else {
            return None;
        };
        calls.iter().find(|call| &call.id == id)?.detail.as_deref()
    }
}

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
    pub(crate) fn capture(self, index: usize) -> Option<OutputCapture> {
        match self {
            Self::Shared(items) => items.get(index).cloned().map(Into::into),
            Self::Owned(items) => items.get(index).cloned().map(Arc::new).map(Into::into),
        }
    }
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
    fn shared_detail_capture_keeps_original_payload_and_tool_identity_without_cloning() {
        use crate::app::{Disclosure, ToolCallStatus, ToolCallView};
        let now = std::time::Instant::now();
        let source = OutputItem::ToolDispatch {
            calls: vec![ToolCallView {
                id: "call".into(),
                tool: "fs.read".into(),
                intent: String::new(),
                input: serde_json::Value::Null,
                status: ToolCallStatus::Ok,
                disclosure: Disclosure::Summary,
                detail: Some(std::sync::Arc::new(OutputItem::FsDetail {
                    view: crate::app::FsDetail::Read {
                        path: "source.rs".into(),
                        content: "original payload\n".repeat(20_000),
                        start_line: 1,
                        total_lines: None,
                        truncated: false,
                    },
                    expanded: true,
                })),
                draft_index: None,
                draft_preview: Default::default(),
                applied_edit: None,
                started_at: now,
                ended_at: Some(now),
            }],
        };
        let mut document = crate::flow_view::FlowDocument::from(vec![source]);
        let capture = OutputItems::from(&document).capture(0).unwrap();
        assert!(std::ptr::eq(capture.item().unwrap(), &document[0]));
        let nested = capture.clone().detail("call".into());
        let OutputItem::ToolDispatch { calls } = &document[0] else {
            unreachable!()
        };
        assert!(std::ptr::eq(
            nested.item().unwrap(),
            calls[0].detail.as_deref().unwrap()
        ));
        assert!(capture.clone().detail("missing".into()).item().is_none());
        assert!(Arc::ptr_eq(&capture.root, &nested.root));
        document.mutate(0, crate::flow_view::OutputMutation::Semantic, |item| {
            let OutputItem::ToolDispatch { calls } = item else {
                unreachable!()
            };
            calls[0].detail = None;
            true
        });
        drop(document);
        assert!(
            matches!(nested.item(), Some(OutputItem::FsDetail { view: crate::app::FsDetail::Read { content, .. }, .. }) if content.starts_with("original payload"))
        );
    }

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
