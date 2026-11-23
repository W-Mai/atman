//! Work diffs are scoped to the original execution event and its committed batch.

use atman_rt::TurnLease;

use crate::{
    event::Event,
    flow_store::trace::{ExecutionEvent, ExecutionScope},
};

use super::*;

const MAX_INDEXED_DIFFS: usize = 1024;
const MAX_PAGE_DIFFS: usize = 64;
const MAX_TITLE_BYTES: usize = 160;
const MAX_LINE_BYTES: usize = 4096;
pub(crate) const MAX_DIFF_LINES: usize = 64;

#[derive(Clone, Debug)]
pub struct ConversationDiffSource {
    pub receipt: CommitReceipt,
    pub record: FlowRecordId,
    pub lease: TurnLease<AtmanUuid>,
    pub title: String,
}

pub struct ConversationDiffPage {
    pub offset: usize,
    pub next_offset: usize,
    pub total_lines: usize,
    pub lines: Vec<ConversationDiffLine>,
}

pub struct ConversationDiffLine {
    pub kind: ConversationDiffLineKind,
    pub before: Option<u32>,
    pub after: Option<u32>,
    pub text: String,
    pub truncated: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConversationDiffLineKind {
    Header,
    Hunk,
    Added,
    Removed,
    Context,
    Meta,
}

pub(super) fn record_sources(
    entries: &mut VecDeque<ConversationDiffSource>,
    omitted: &mut bool,
    receipt: &CommitReceipt,
    fact: usize,
    events: &[ExecutionEvent],
) {
    for (item, event) in events.iter().enumerate() {
        let (
            ExecutionScope::Invocation(lease),
            Event::DiffPreview {
                title,
                unified_diff: Some(diff),
                ..
            },
        ) = (&event.scope, &event.event)
        else {
            continue;
        };
        if diff.is_empty() {
            continue;
        }
        if entries.len() == MAX_INDEXED_DIFFS {
            *omitted = true;
            entries.pop_front();
        }
        entries.push_back(ConversationDiffSource {
            receipt: receipt.clone(),
            record: FlowRecordId {
                commit: receipt.sequence,
                fact,
                item,
            },
            lease: lease.clone(),
            title: prefix(title, MAX_TITLE_BYTES).to_owned(),
        });
    }
}

impl PreparedConversationSource {
    pub(super) fn work_diffs(
        &self,
        items: &[ConversationItem],
    ) -> (Vec<ConversationDiffSource>, bool) {
        let leases = items
            .iter()
            .filter_map(|item| item.work.as_ref().map(|work| &work.lease))
            .collect::<Vec<_>>();
        let mut matches = self.markers.diffs.iter().rev().filter(|entry| {
            self.visible(entry.record.commit)
                && entry.lease.flow == self.boundary.source.flow
                && leases.contains(&&entry.lease)
        });
        let mut diffs = matches
            .by_ref()
            .take(MAX_PAGE_DIFFS)
            .cloned()
            .collect::<Vec<_>>();
        let truncated =
            matches.next().is_some() || (self.markers.dropped_diffs && !leases.is_empty());
        diffs.reverse();
        (diffs, truncated)
    }

    pub(crate) fn read_diff_record(
        &mut self,
        record: FlowRecordId,
        receipt: &CommitReceipt,
        offset: usize,
        limit: usize,
    ) -> io::Result<Result<ConversationDiffPage, ConversationCapacity>> {
        if record.commit != receipt.sequence || self.reader.receipt_at(record.commit)? != *receipt {
            return Err(missing_record());
        }
        let facts = match self.read_visible_batch(record.commit)? {
            Ok(facts) => facts,
            Err(capacity) => return Ok(Err(capacity)),
        };
        let Some(FlowFact::ExecutionEvents(events)) = facts.get(record.fact) else {
            return Err(missing_record());
        };
        let Some(event) = events.get(record.item) else {
            return Err(missing_record());
        };
        let (
            ExecutionScope::Invocation(lease),
            Event::DiffPreview {
                unified_diff: Some(diff),
                ..
            },
        ) = (&event.scope, &event.event)
        else {
            return Err(missing_record());
        };
        if lease.flow != self.boundary.source.flow || diff.is_empty() {
            return Err(missing_record());
        }
        Ok(Ok(diff_page(diff, offset, limit)?))
    }
}

fn diff_page(patch: &str, offset: usize, limit: usize) -> io::Result<ConversationDiffPage> {
    let total_lines = patch.lines().count();
    if offset > total_lines || !(1..=MAX_DIFF_LINES).contains(&limit) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid diff page",
        ));
    }
    let mut hunk = None;
    let mut lines = Vec::new();
    for (index, text) in patch.lines().take(offset + limit).enumerate() {
        let (kind, before, after) = classify(text, &mut hunk);
        if index >= offset {
            let clipped = prefix(text, MAX_LINE_BYTES);
            lines.push(ConversationDiffLine {
                kind,
                before,
                after,
                text: clipped.to_owned(),
                truncated: clipped.len() < text.len(),
            });
        }
    }
    Ok(ConversationDiffPage {
        offset,
        next_offset: offset + lines.len(),
        total_lines,
        lines,
    })
}

#[derive(Clone, Copy)]
struct HunkSide {
    next: u32,
    remaining: u32,
}

impl HunkSide {
    fn take(&mut self) -> Option<u32> {
        self.remaining = self.remaining.checked_sub(1)?;
        let next = self.next;
        self.next += 1;
        Some(next)
    }
}

fn range(field: &str, prefix: char) -> Option<HunkSide> {
    let field = field.strip_prefix(prefix)?;
    let (start, count) = field.split_once(',').unwrap_or((field, "1"));
    let next = start.parse::<u32>().ok()?;
    let remaining = count.parse::<u32>().ok()?;
    next.checked_add(remaining)?;
    Some(HunkSide { next, remaining })
}

fn classify(
    text: &str,
    hunk: &mut Option<(HunkSide, HunkSide)>,
) -> (ConversationDiffLineKind, Option<u32>, Option<u32>) {
    use ConversationDiffLineKind::*;

    if let Some(header) = text.strip_prefix("@@ ") {
        let mut fields = header.split_ascii_whitespace();
        *hunk = fields
            .next()
            .and_then(|before| range(before, '-'))
            .zip(fields.next().and_then(|after| range(after, '+')));
        return (Hunk, None, None);
    }
    if let Some((before, after)) = hunk {
        match text.as_bytes().first() {
            Some(b'+') if after.remaining > 0 => return (Added, None, after.take()),
            Some(b'-') if before.remaining > 0 => return (Removed, before.take(), None),
            Some(b' ') if before.remaining > 0 && after.remaining > 0 => {
                return (Context, before.take(), after.take());
            }
            Some(b'\\') => return (Meta, None, None),
            _ => *hunk = None,
        }
    }
    let kind = if ["diff --git ", "--- ", "+++ "]
        .iter()
        .any(|prefix| text.starts_with(prefix))
    {
        Header
    } else {
        Meta
    };
    (kind, None, None)
}

fn prefix(text: &str, allowed: usize) -> &str {
    let mut end = allowed.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
