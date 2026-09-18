use std::cmp::Ordering;
use std::ops::Range;

use unicode_segmentation::UnicodeSegmentation;

use crate::app::OutputRevision;
use crate::wm::WindowId;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SidebarSection {
    Goal,
    Plan,
    Todo,
    Context,
    Mcp,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SelectionDomain {
    TranscriptProse,
    MarkdownCode {
        item_id: u64,
        block: u32,
    },
    Thinking {
        item_id: u64,
    },
    RawOutput {
        item_id: u64,
        output: String,
    },
    Terminal {
        handle: String,
        item_id: u64,
    },
    Diff {
        item_id: u64,
    },
    Mermaid {
        item_id: u64,
    },
    WorkflowOutput {
        item_id: u64,
        node: String,
    },
    Sidebar {
        section: SidebarSection,
    },
    Window {
        window_id: WindowId,
        surface: String,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Affinity {
    #[default]
    Before,
    After,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SemanticPoint {
    pub domain: SelectionDomain,
    pub ordinal: u64,
    pub grapheme: usize,
    pub affinity: Affinity,
}

impl SemanticPoint {
    fn position_cmp(&self, other: &Self) -> Ordering {
        (self.ordinal, self.grapheme, self.affinity as u8).cmp(&(
            other.ordinal,
            other.grapheme,
            other.affinity as u8,
        ))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionPhase {
    Pending,
    Active,
    Retained,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionState {
    pub phase: SelectionPhase,
    pub anchor: SemanticPoint,
    pub focus: SemanticPoint,
    pub owner_revision: OutputRevision,
    pub structure_revision: u64,
    pub copied: bool,
}

pub fn selection_begin(
    point: SemanticPoint,
    owner_revision: OutputRevision,
    structure_revision: u64,
) -> SelectionState {
    SelectionState {
        phase: SelectionPhase::Pending,
        anchor: point.clone(),
        focus: point,
        owner_revision,
        structure_revision,
        copied: false,
    }
}

pub fn selection_extend(
    state: &SelectionState,
    focus: SemanticPoint,
    owner_revision: OutputRevision,
    structure_revision: u64,
) -> Option<SelectionState> {
    (state.owner_revision == owner_revision
        && state.structure_revision == structure_revision
        && state.anchor.domain == focus.domain)
        .then(|| {
            let moved = state.anchor.position_cmp(&focus) != Ordering::Equal;
            SelectionState {
                phase: if moved {
                    SelectionPhase::Active
                } else {
                    state.phase
                },
                anchor: state.anchor.clone(),
                focus,
                owner_revision,
                structure_revision,
                copied: false,
            }
        })
}

pub fn selection_is_non_empty(state: &SelectionState) -> bool {
    state.anchor.domain == state.focus.domain
        && state.anchor.position_cmp(&state.focus) != Ordering::Equal
}

pub fn selection_contains(state: &SelectionState, point: &SemanticPoint) -> bool {
    if state.phase == SelectionPhase::Pending || !selection_is_non_empty(state) {
        return false;
    }
    let Some((start, end)) = normalize_endpoints(&state.anchor, &state.focus) else {
        return false;
    };
    point.domain == start.domain
        && !point.position_cmp(&start).is_lt()
        && !point.position_cmp(&end).is_gt()
}

pub fn selection_retain_copied(mut state: SelectionState) -> SelectionState {
    state.phase = SelectionPhase::Retained;
    state.copied = true;
    state
}

pub fn selection_clear() -> Option<SelectionState> {
    None
}

pub fn selection_copy_payload(
    projection: &VisibleSelectionProjection,
    state: &SelectionState,
) -> Option<CopyPayload> {
    (state.phase != SelectionPhase::Pending
        && selection_is_non_empty(state)
        && projection.structure_revision == state.structure_revision)
        .then(|| projection.copy_selection(state))
        .flatten()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CopyPayload {
    Markdown(String),
    PlainText(String),
    Preview(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CopyFormat {
    Markdown,
    PlainText,
    Preview,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProseEventSegment {
    pub event: u32,
    pub event_graphemes: Range<usize>,
    pub fragment_grapheme_start: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CopyFragment {
    pub source_range: Option<Range<usize>>,
    pub semantic_text: String,
    pub format: CopyFormat,
    pub exact_source_safe: bool,
    pub event_segments: Vec<ProseEventSegment>,
}

impl CopyFragment {
    pub fn markdown_node(source_range: Range<usize>, semantic_text: String) -> Self {
        Self {
            source_range: Some(source_range),
            semantic_text,
            format: CopyFormat::Markdown,
            exact_source_safe: true,
            event_segments: Vec::new(),
        }
    }

    pub fn markdown_leaf(source: &str, source_range: Range<usize>, semantic_text: String) -> Self {
        let exact_source_safe = source.get(source_range.clone()) == Some(semantic_text.as_str());
        Self {
            source_range: Some(source_range),
            semantic_text,
            format: CopyFormat::Markdown,
            exact_source_safe,
            event_segments: Vec::new(),
        }
    }

    pub fn plain_text(semantic_text: String) -> Self {
        Self {
            source_range: None,
            semantic_text,
            format: CopyFormat::PlainText,
            exact_source_safe: false,
            event_segments: Vec::new(),
        }
    }

    pub fn with_event_segments(mut self, event_segments: Vec<ProseEventSegment>) -> Self {
        self.event_segments = event_segments;
        self
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelativeProseAtom {
    pub row: u16,
    pub cols: Range<u16>,
    pub cell_width: u16,
    pub event: u32,
    pub event_graphemes: Range<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LocalProsePoint {
    pub fragment: usize,
    pub grapheme: usize,
}

pub fn prose_atom_runs(
    row: u16,
    start_col: u16,
    text: &str,
    event: u32,
    event_grapheme_start: usize,
) -> Vec<RelativeProseAtom> {
    let mut atoms = Vec::new();
    let mut col = start_col;
    let mut grapheme = event_grapheme_start;
    let mut run: Option<(u16, u16, Range<usize>)> = None;

    for (_, _, cells) in crate::width::grapheme_indices(text) {
        let cells = u16::try_from(cells).unwrap_or(u16::MAX);
        match run.as_mut() {
            Some((_, run_cells, range)) if *run_cells == cells => range.end += 1,
            _ => {
                if let Some((run_col, run_cells, range)) = run.take() {
                    atoms.push(RelativeProseAtom {
                        row,
                        cols: run_col..col,
                        cell_width: run_cells,
                        event,
                        event_graphemes: range,
                    });
                }
                run = Some((col, cells, grapheme..grapheme.saturating_add(1)));
            }
        }
        col = col.saturating_add(cells);
        grapheme = grapheme.saturating_add(1);
    }
    if let Some((run_col, run_cells, range)) = run {
        atoms.push(RelativeProseAtom {
            row,
            cols: run_col..col,
            cell_width: run_cells,
            event,
            event_graphemes: range,
        });
    }
    atoms
}

pub fn local_prose_point_at(
    atoms: &[RelativeProseAtom],
    fragments: &[CopyFragment],
    row: u16,
    col: u16,
) -> Option<LocalProsePoint> {
    let atom = atoms.iter().find(|atom| {
        atom.row == row && col >= atom.cols.start && col < atom.cols.end && atom.cell_width > 0
    })?;
    let step = usize::from((col - atom.cols.start) / atom.cell_width);
    let event_grapheme = atom.event_graphemes.start.saturating_add(step);
    fragments
        .iter()
        .enumerate()
        .find_map(|(fragment, candidate)| {
            candidate.event_segments.iter().find_map(|segment| {
                (segment.event == atom.event && segment.event_graphemes.contains(&event_grapheme))
                    .then(|| LocalProsePoint {
                        fragment,
                        grapheme: segment
                            .fragment_grapheme_start
                            .saturating_add(event_grapheme - segment.event_graphemes.start),
                    })
            })
        })
}

/// One run of graphemes as actually rendered on a single item-relative row.
///
/// `row` and `cols` are relative to the item's own first row and column, so shifting a projection
/// into a viewport only means adding the item's start row. `cell_width` is the display width shared
/// by every grapheme in the run, which is what keeps `col -> grapheme` exact for CJK and emoji
/// instead of assuming one cell per character.
///
/// `source` addresses the owning item's source text rather than a fragment index. Semantics stay
/// owned by `CopyFragment`/`MarkdownCodeSource`, so this map never becomes a second copy model that
/// can drift from the serializer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelativeSelectionAtom {
    pub row: u16,
    pub cols: Range<u16>,
    pub cell_width: u16,
    pub source: Range<usize>,
}

/// Builds atoms for one rendered run, grouping consecutive graphemes of equal display width.
///
/// Grouping by width is what keeps `col -> grapheme` exact: a run of ASCII advances one cell per
/// grapheme, a run of CJK or emoji advances two, and a boundary between them starts a new atom
/// instead of silently shifting every later offset by one cell.
///
/// Returns the atoms plus the column just past the run.
pub fn atom_runs(
    row: u16,
    start_col: u16,
    text: &str,
    source_start: usize,
) -> (Vec<RelativeSelectionAtom>, u16) {
    let mut atoms = Vec::new();
    let mut col = start_col;
    let mut run: Option<(u16, u16, Range<usize>)> = None;

    for (offset, grapheme, cells) in crate::width::grapheme_indices(text) {
        let cells = u16::try_from(cells).unwrap_or(u16::MAX);
        let source = source_start.saturating_add(offset)
            ..source_start
                .saturating_add(offset)
                .saturating_add(grapheme.len());
        match run.as_mut() {
            Some((_, run_cells, run_source)) if *run_cells == cells => {
                run_source.end = source.end;
            }
            _ => {
                if let Some((run_col, run_cells, run_source)) = run.take() {
                    atoms.push(RelativeSelectionAtom {
                        row,
                        cols: run_col..col,
                        cell_width: run_cells,
                        source: run_source,
                    });
                }
                run = Some((col, cells, source));
            }
        }
        col = col.saturating_add(cells);
    }

    if let Some((run_col, run_cells, run_source)) = run {
        atoms.push(RelativeSelectionAtom {
            row,
            cols: run_col..col,
            cell_width: run_cells,
            source: run_source,
        });
    }
    (atoms, col)
}

impl RelativeSelectionAtom {
    pub fn contains_col(&self, col: u16) -> bool {
        col >= self.cols.start && col < self.cols.end
    }

    /// Source byte offset of the grapheme rendered at `col`.
    pub fn source_at(&self, source: &str, col: u16) -> Option<usize> {
        if !self.contains_col(col) || self.cell_width == 0 {
            return None;
        }
        let step = usize::from((col - self.cols.start) / self.cell_width);
        let text = source.get(self.source.clone())?;
        Some(
            text.grapheme_indices(true)
                .nth(step)
                .map_or(self.source.end, |(offset, _)| {
                    self.source.start.saturating_add(offset)
                }),
        )
    }
}

/// An atom already shifted into the current viewport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleAtom {
    pub screen_row: u32,
    pub cols: Range<u16>,
    pub cell_width: u16,
    pub source: Range<usize>,
}

impl VisibleAtom {
    pub fn contains_col(&self, col: u16) -> bool {
        col >= self.cols.start && col < self.cols.end
    }

    pub fn source_at(&self, source: &str, col: u16) -> Option<usize> {
        RelativeSelectionAtom {
            row: 0,
            cols: self.cols.clone(),
            cell_width: self.cell_width,
            source: self.source.clone(),
        }
        .source_at(source, col)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeBodySegment {
    pub body_range: Range<usize>,
    pub source_range: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownCodeSource {
    pub domain: SelectionDomain,
    pub block: u32,
    pub body: String,
    pub segments: Vec<CodeBodySegment>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkdownSemanticSource {
    pub owner_revision: OutputRevision,
    pub prose: Vec<CopyFragment>,
    pub code_blocks: Vec<MarkdownCodeSource>,
}

pub fn normalize_endpoints(
    first: &SemanticPoint,
    second: &SemanticPoint,
) -> Option<(SemanticPoint, SemanticPoint)> {
    if first.domain != second.domain {
        return None;
    }
    if first.position_cmp(second).is_le() {
        Some((first.clone(), second.clone()))
    } else {
        Some((second.clone(), first.clone()))
    }
}

pub fn clamp_endpoint(
    point: &SemanticPoint,
    start: &SemanticPoint,
    end: &SemanticPoint,
) -> Option<SemanticPoint> {
    let (start, end) = normalize_endpoints(start, end)?;
    if point.domain != start.domain {
        return None;
    }
    if point.position_cmp(&start).is_lt() {
        Some(start)
    } else if point.position_cmp(&end).is_gt() {
        Some(end)
    } else {
        Some(point.clone())
    }
}

pub fn grapheme_slice(text: &str, range: Range<usize>) -> Option<&str> {
    let boundaries = text
        .grapheme_indices(true)
        .map(|(offset, _)| offset)
        .chain(std::iter::once(text.len()))
        .collect::<Vec<_>>();
    let count = boundaries.len().saturating_sub(1);
    let start = range.start.min(range.end).min(count);
    let end = range.start.max(range.end).min(count);
    if start >= end {
        return None;
    }
    text.get(boundaries[start]..boundaries[end])
}

pub fn serialize_code_body(
    code: &MarkdownCodeSource,
    graphemes: Range<usize>,
) -> Option<CopyPayload> {
    let text = grapheme_slice(&code.body, graphemes)?;
    if text.trim().is_empty() {
        return None;
    }
    Some(CopyPayload::PlainText(text.to_owned()))
}

/// One selected slice of a fragment: its Markdown-or-plain payload plus the plain fallback.
///
/// Both are kept because the degrade rule is decided across the whole selection, not per fragment:
/// a single unsafe slice forces every part back to semantic plain text.
struct SelectedPart {
    payload: CopyPayload,
    plain: String,
}

fn select_part(
    source: &str,
    fragment: &CopyFragment,
    graphemes: Range<usize>,
) -> Option<SelectedPart> {
    let payload = serialize_fragment(source, fragment, graphemes.clone())?;
    let plain = grapheme_slice(&fragment.semantic_text, graphemes)?.to_owned();
    Some(SelectedPart { payload, plain })
}

/// Joins selected parts with one blank line, degrading to plain text if any part is unsafe.
fn join_parts(parts: Vec<SelectedPart>) -> Option<CopyPayload> {
    if parts.is_empty() {
        return None;
    }
    let markdown = parts
        .iter()
        .all(|part| matches!(part.payload, CopyPayload::Markdown(_)));
    let texts = parts
        .into_iter()
        .filter_map(|part| {
            let text = if markdown {
                match part.payload {
                    CopyPayload::Markdown(text) => text,
                    CopyPayload::PlainText(_) | CopyPayload::Preview(_) => unreachable!(),
                }
            } else {
                part.plain
            };
            let text = text.trim_end();
            (!text.is_empty()).then(|| text.to_owned())
        })
        .collect::<Vec<_>>();
    if texts.is_empty() {
        return None;
    }
    let text = texts.join("\n\n");
    Some(if markdown {
        CopyPayload::Markdown(text)
    } else {
        CopyPayload::PlainText(text)
    })
}

fn whole_fragment(source: &str, fragment: &CopyFragment) -> Option<SelectedPart> {
    let count = fragment.semantic_text.graphemes(true).count();
    select_part(source, fragment, 0..count)
}

pub fn serialize_prose_fragments(source: &str, fragments: &[CopyFragment]) -> Option<CopyPayload> {
    join_parts(
        fragments
            .iter()
            .filter_map(|fragment| whole_fragment(source, fragment))
            .collect(),
    )
}

/// Serializes fragments addressed by `ordinal`, clipping only the two endpoint fragments.
///
/// `fragments` must be enumerated in the same order that assigns `SemanticPoint::ordinal`, so a
/// selection reads exactly the fragments the user dragged over.
fn serialize_fragment_range<'a>(
    fragments: impl Iterator<Item = (u64, &'a str, &'a CopyFragment)>,
    start: &SemanticPoint,
    end: &SemanticPoint,
) -> Option<CopyPayload> {
    let parts = fragments
        .filter(|(ordinal, _, _)| *ordinal >= start.ordinal && *ordinal <= end.ordinal)
        .filter_map(|(ordinal, source, fragment)| {
            let count = fragment.semantic_text.graphemes(true).count();
            let from = if ordinal == start.ordinal {
                start.grapheme
            } else {
                0
            };
            let to = if ordinal == end.ordinal {
                end.grapheme
            } else {
                count
            };
            select_part(source, fragment, from..to)
        })
        .collect();
    join_parts(parts)
}

pub fn serialize_fragment(
    source: &str,
    fragment: &CopyFragment,
    graphemes: Range<usize>,
) -> Option<CopyPayload> {
    let count = fragment.semantic_text.graphemes(true).count();
    let start = graphemes.start.min(graphemes.end).min(count);
    let end = graphemes.start.max(graphemes.end).min(count);
    if start >= end {
        return None;
    }

    let whole_fragment = start == 0 && end == count;
    let exact = fragment.source_range.as_ref().and_then(|range| {
        let source_fragment = source.get(range.clone())?;
        if whole_fragment {
            fragment.exact_source_safe.then_some(source_fragment)
        } else if fragment.exact_source_safe && source_fragment == fragment.semantic_text {
            grapheme_slice(source_fragment, start..end)
        } else {
            None
        }
    });

    let (text, exact_source) = match exact {
        Some(text) => (text.to_owned(), true),
        None => (
            grapheme_slice(&fragment.semantic_text, start..end)?.to_owned(),
            false,
        ),
    };
    if text.trim().is_empty() {
        return None;
    }

    Some(match fragment.format {
        CopyFormat::Markdown if exact_source => CopyPayload::Markdown(text),
        CopyFormat::Preview => CopyPayload::Preview(text),
        CopyFormat::Markdown | CopyFormat::PlainText => CopyPayload::PlainText(text),
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IsolatedSource {
    pub domain: SelectionDomain,
    pub fragments: Vec<CopyFragment>,
}

/// Per-item semantic copy source derived during the normal render pass.
///
/// `source` is retained because prose fragments address it by byte range; it is always smaller
/// than the rendered lines cached beside it, and it is dropped with the entry on prune.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RelativeCodeAtom {
    pub domain: SelectionDomain,
    pub atom: RelativeSelectionAtom,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleCodeAtom {
    pub domain: SelectionDomain,
    pub atom: VisibleAtom,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RelativeIsolatedAtom {
    Markdown {
        domain: SelectionDomain,
        atom: RelativeProseAtom,
    },
    Raw {
        domain: SelectionDomain,
        atom: RelativeSelectionAtom,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VisibleIsolatedAtom {
    Markdown {
        domain: SelectionDomain,
        atom: RelativeProseAtom,
        screen_row: u32,
    },
    Raw {
        domain: SelectionDomain,
        atom: VisibleAtom,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ItemSemanticSource {
    pub owner_revision: OutputRevision,
    pub source: String,
    pub prose: Vec<CopyFragment>,
    pub code_blocks: Vec<MarkdownCodeSource>,
    pub prose_atoms: Vec<RelativeProseAtom>,
    pub code_atoms: Vec<RelativeCodeAtom>,
    pub isolated: Vec<IsolatedSource>,
    pub isolated_atoms: Vec<RelativeIsolatedAtom>,
}

pub fn code_atoms(
    code_blocks: &[MarkdownCodeSource],
    spans: &[crate::markdown::CodeBlockSpan],
) -> Vec<RelativeCodeAtom> {
    let mut atoms = Vec::new();
    for code in code_blocks {
        let Some(span) = spans.iter().find(|span| span.block == code.block) else {
            continue;
        };
        let mut body_offset = 0usize;
        for (line_index, raw) in code
            .body
            .split_inclusive('\n')
            .enumerate()
            .take(span.line_count)
        {
            let text = raw.strip_suffix('\n').unwrap_or(raw);
            let row = span
                .first_body_row
                .saturating_add(u16::try_from(line_index).unwrap_or(u16::MAX));
            let (line_atoms, _) = atom_runs(row, span.body_start_col, text, body_offset);
            atoms.extend(line_atoms.into_iter().map(|atom| RelativeCodeAtom {
                domain: code.domain.clone(),
                atom,
            }));
            body_offset = body_offset.saturating_add(raw.len());
        }
    }
    atoms
}

/// One selectable transcript item currently inside the viewport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleProseAtom {
    pub atom: RelativeProseAtom,
    pub screen_row: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisibleSurface {
    pub item_index: usize,
    pub revision: OutputRevision,
    pub start_row: u32,
    pub end_row: u32,
    pub source: std::sync::Arc<ItemSemanticSource>,
    pub prose_atoms: Vec<VisibleProseAtom>,
    pub code_atoms: Vec<VisibleCodeAtom>,
    pub isolated_atoms: Vec<VisibleIsolatedAtom>,
}

/// Semantic copy projection for the current viewport.
///
/// `structure_revision` is the owning `OutputStore` structure revision, so a stale projection is
/// detectable without a second identity or revision authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VisualSelectionPoint {
    pub point: SemanticPoint,
    pub row: u32,
    pub col: u16,
    pub surface: usize,
    pub owner_revision: OutputRevision,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VisibleSelectionProjection {
    pub structure_revision: u64,
    pub surfaces: Vec<VisibleSurface>,
}

impl VisibleSelectionProjection {
    pub fn visual_points(&self) -> Vec<VisualSelectionPoint> {
        let mut points = Vec::new();
        for (surface_index, surface) in self.surfaces.iter().enumerate() {
            for visible in &surface.prose_atoms {
                let step = visible.atom.cell_width.max(1) as usize;
                for col in (visible.atom.cols.start..visible.atom.cols.end).step_by(step) {
                    if let Some(point) = self.prose_point_at(visible.screen_row, col) {
                        points.push(VisualSelectionPoint {
                            point,
                            row: visible.screen_row,
                            col,
                            surface: surface_index,
                            owner_revision: surface.revision,
                        });
                    }
                }
            }
            for visible in &surface.code_atoms {
                let step = visible.atom.cell_width.max(1) as usize;
                for col in (visible.atom.cols.start..visible.atom.cols.end).step_by(step) {
                    if let Some(point) = self.code_point_at(visible.atom.screen_row, col) {
                        points.push(VisualSelectionPoint {
                            point,
                            row: visible.atom.screen_row,
                            col,
                            surface: surface_index,
                            owner_revision: surface.revision,
                        });
                    }
                }
            }
            for visible in &surface.isolated_atoms {
                let (row, cols, width) = match visible {
                    VisibleIsolatedAtom::Markdown {
                        atom, screen_row, ..
                    } => (*screen_row, atom.cols.clone(), atom.cell_width),
                    VisibleIsolatedAtom::Raw { atom, .. } => {
                        (atom.screen_row, atom.cols.clone(), atom.cell_width)
                    }
                };
                for col in (cols.start..cols.end).step_by(width.max(1) as usize) {
                    if let Some(point) = self.isolated_point_at(row, col) {
                        points.push(VisualSelectionPoint {
                            point,
                            row,
                            col,
                            surface: surface_index,
                            owner_revision: surface.revision,
                        });
                    }
                }
            }
        }
        points.sort_by_key(|point| (point.row, point.col, point.surface));
        points.dedup_by(|left, right| {
            left.row == right.row && left.col == right.col && left.point == right.point
        });
        points
    }

    pub fn surface_at_row(&self, row: u32) -> Option<&VisibleSurface> {
        self.surfaces
            .iter()
            .find(|surface| row >= surface.start_row && row < surface.end_row)
    }

    pub fn prose_point_at(&self, row: u32, col: u16) -> Option<SemanticPoint> {
        self.surfaces.iter().find_map(|surface| {
            let atom = surface.prose_atoms.iter().find(|atom| {
                atom.screen_row == row && col >= atom.atom.cols.start && col < atom.atom.cols.end
            })?;
            let point = local_prose_point_at(
                std::slice::from_ref(&atom.atom),
                &surface.source.prose,
                atom.atom.row,
                col,
            )?;
            Some(SemanticPoint {
                domain: SelectionDomain::TranscriptProse,
                ordinal: prose_ordinal(surface.source.owner_revision.id, point.fragment),
                grapheme: point.grapheme,
                affinity: Affinity::Before,
            })
        })
    }

    pub fn code_point_at(&self, row: u32, col: u16) -> Option<SemanticPoint> {
        self.surfaces.iter().find_map(|surface| {
            let visible = surface
                .code_atoms
                .iter()
                .find(|atom| atom.atom.screen_row == row && atom.atom.contains_col(col))?;
            let code = surface
                .source
                .code_blocks
                .iter()
                .find(|code| code.domain == visible.domain)?;
            let byte = visible.atom.source_at(&code.body, col)?;
            let grapheme = code.body[..byte].graphemes(true).count();
            Some(SemanticPoint {
                domain: visible.domain.clone(),
                ordinal: u64::from(code.block),
                grapheme,
                affinity: Affinity::Before,
            })
        })
    }

    pub fn isolated_point_at(&self, row: u32, col: u16) -> Option<SemanticPoint> {
        self.surfaces.iter().find_map(|surface| {
            surface
                .isolated_atoms
                .iter()
                .find_map(|visible| match visible {
                    VisibleIsolatedAtom::Markdown {
                        domain,
                        atom,
                        screen_row,
                    } if *screen_row == row && col >= atom.cols.start && col < atom.cols.end => {
                        let isolated = surface
                            .source
                            .isolated
                            .iter()
                            .find(|isolated| &isolated.domain == domain)?;
                        let point = local_prose_point_at(
                            std::slice::from_ref(atom),
                            &isolated.fragments,
                            atom.row,
                            col,
                        )?;
                        Some(SemanticPoint {
                            domain: domain.clone(),
                            ordinal: point.fragment as u64,
                            grapheme: point.grapheme,
                            affinity: Affinity::Before,
                        })
                    }
                    VisibleIsolatedAtom::Raw { domain, atom }
                        if atom.screen_row == row && atom.contains_col(col) =>
                    {
                        let byte = atom.source_at(&surface.source.source, col)?;
                        Some(SemanticPoint {
                            domain: domain.clone(),
                            ordinal: 0,
                            grapheme: surface.source.source[..byte].graphemes(true).count(),
                            affinity: Affinity::Before,
                        })
                    }
                    VisibleIsolatedAtom::Markdown { .. } | VisibleIsolatedAtom::Raw { .. } => None,
                })
        })
    }

    pub fn code_block(&self, domain: &SelectionDomain) -> Option<&MarkdownCodeSource> {
        self.surfaces.iter().find_map(|surface| {
            surface
                .source
                .code_blocks
                .iter()
                .find(|code| &code.domain == domain)
        })
    }

    pub fn isolated(&self, domain: &SelectionDomain) -> Option<(&str, &IsolatedSource)> {
        self.surfaces.iter().find_map(|surface| {
            surface
                .source
                .isolated
                .iter()
                .find(|isolated| &isolated.domain == domain)
                .map(|isolated| (surface.source.source.as_str(), isolated))
        })
    }

    /// Visible transcript prose in document order, addressed by stable ordinal.
    ///
    /// Isolated surfaces (Thinking, tool output) contribute no prose fragments, so dragging body
    /// text across them joins the surrounding prose instead of leaking their content.
    fn prose_fragments(&self) -> impl Iterator<Item = (u64, &str, &CopyFragment)> {
        self.surfaces.iter().flat_map(|surface| {
            let item_id = surface.source.owner_revision.id;
            surface
                .source
                .prose
                .iter()
                .enumerate()
                .map(move |(index, fragment)| {
                    (
                        prose_ordinal(item_id, index),
                        surface.source.source.as_str(),
                        fragment,
                    )
                })
        })
    }

    /// The single serialization entry point: selection endpoints to clipboard payload.
    ///
    /// The domain is taken from the normalized start point, so a selection can never mix body text
    /// with a code block, Thinking or tool output even if the pointer travelled across them.
    pub fn copy_selection(&self, state: &SelectionState) -> Option<CopyPayload> {
        let (start, end) = normalize_endpoints(&state.anchor, &state.focus)?;
        match &start.domain {
            SelectionDomain::TranscriptProse => {
                serialize_fragment_range(self.prose_fragments(), &start, &end)
            }
            SelectionDomain::MarkdownCode { .. } => {
                let code = self.code_block(&start.domain)?;
                serialize_code_body(code, start.grapheme..end.grapheme)
            }
            domain => {
                let (source, isolated) = self.isolated(domain)?;
                serialize_fragment_range(
                    isolated
                        .fragments
                        .iter()
                        .enumerate()
                        .map(|(index, fragment)| (index as u64, source, fragment)),
                    &start,
                    &end,
                )
            }
        }
    }
}

/// Fragments addressable within one item before the ordinal saturates.
const PROSE_FRAGMENTS_PER_ITEM: u64 = 1 << 20;

/// Stable document-order ordinal for one prose fragment.
///
/// `OutputRevision::id` increases with document position and is never renumbered by removal, so
/// endpoints keep comparing correctly across items instead of depending on viewport indices.
/// Saturating arithmetic keeps the order monotonic rather than wrapping past it.
fn prose_ordinal(item_id: u64, fragment_index: usize) -> u64 {
    item_id
        .saturating_mul(PROSE_FRAGMENTS_PER_ITEM)
        .saturating_add((fragment_index as u64).min(PROSE_FRAGMENTS_PER_ITEM - 1))
}

pub fn terminal_capture_text(screen: &atman_runtime::tools::term::TerminalScreen) -> String {
    let cols = usize::from(screen.cols);
    (0..usize::from(screen.rows))
        .map(|row| {
            let mut text = String::new();
            for col in 0..cols {
                let Some(cell) = screen
                    .cells
                    .get(row.saturating_mul(cols).saturating_add(col))
                else {
                    break;
                };
                if !cell.wide_continuation {
                    text.push_str(if cell.chars.is_empty() {
                        " "
                    } else {
                        &cell.chars
                    });
                }
            }
            text.trim_end().to_owned()
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_owned()
}

pub fn terminal_stream_text(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut output = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '\u{1b}' {
            if ch != '\r' {
                output.push(ch);
            }
            continue;
        }
        match chars.peek().copied() {
            Some('[') => {
                chars.next();
                for next in chars.by_ref() {
                    if ('@'..='~').contains(&next) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                let mut escaped = false;
                for next in chars.by_ref() {
                    if next == '\u{7}' || (escaped && next == '\\') {
                        break;
                    }
                    escaped = next == '\u{1b}';
                }
            }
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    output
}

pub fn item_semantic_source(
    item: &crate::app::OutputItem,
    revision: OutputRevision,
) -> ItemSemanticSource {
    use crate::app::OutputItem;

    match item {
        OutputItem::UserTurn { text } => {
            let graphemes = crate::width::graphemes(text).count();
            ItemSemanticSource {
                owner_revision: revision,
                source: text.clone(),
                prose: vec![
                    CopyFragment::plain_text(text.clone()).with_event_segments(vec![
                        ProseEventSegment {
                            event: 0,
                            event_graphemes: 0..graphemes,
                            fragment_grapheme_start: 0,
                        },
                    ]),
                ],
                ..Default::default()
            }
        }
        OutputItem::AssistantMd { md, .. } => {
            let markdown = crate::markdown::semantic_markdown_source(md, revision);
            ItemSemanticSource {
                owner_revision: revision,
                source: md.clone(),
                prose: markdown.prose,
                code_blocks: markdown.code_blocks,
                prose_atoms: Vec::new(),
                code_atoms: Vec::new(),
                isolated: Vec::new(),
                isolated_atoms: Vec::new(),
            }
        }
        OutputItem::Thinking { text, .. } => {
            let markdown = crate::markdown::semantic_markdown_source(text, revision);
            ItemSemanticSource {
                owner_revision: revision,
                source: text.clone(),
                prose: Vec::new(),
                code_blocks: markdown.code_blocks,
                prose_atoms: Vec::new(),
                code_atoms: Vec::new(),
                isolated: vec![IsolatedSource {
                    domain: SelectionDomain::Thinking {
                        item_id: revision.id,
                    },
                    fragments: markdown.prose,
                }],
                isolated_atoms: Vec::new(),
            }
        }
        OutputItem::Bash { handle, output, .. } => ItemSemanticSource {
            owner_revision: revision,
            source: output.clone(),
            isolated: vec![IsolatedSource {
                domain: SelectionDomain::RawOutput {
                    item_id: revision.id,
                    output: handle.clone(),
                },
                fragments: vec![CopyFragment::plain_text(output.clone())],
            }],
            ..Default::default()
        },
        OutputItem::Terminal {
            handle,
            screen,
            accumulated_bytes,
            mode,
            ..
        } => {
            let output = match mode {
                crate::app::TerminalViewMode::Capture => terminal_capture_text(screen),
                crate::app::TerminalViewMode::Stream => terminal_stream_text(accumulated_bytes),
            };
            ItemSemanticSource {
                owner_revision: revision,
                source: output.clone(),
                isolated: vec![IsolatedSource {
                    domain: SelectionDomain::Terminal {
                        handle: handle.clone(),
                        item_id: revision.id,
                    },
                    fragments: vec![CopyFragment::plain_text(output)],
                }],
                ..Default::default()
            }
        }
        OutputItem::DiffPreview {
            old_content,
            new_content,
            unified_diff,
            ..
        } => {
            let source = unified_diff
                .as_deref()
                .or(old_content.as_deref())
                .or(new_content.as_deref())
                .unwrap_or_default()
                .to_owned();
            ItemSemanticSource {
                owner_revision: revision,
                source: source.clone(),
                isolated: vec![IsolatedSource {
                    domain: SelectionDomain::Diff {
                        item_id: revision.id,
                    },
                    fragments: vec![CopyFragment::plain_text(source)],
                }],
                ..Default::default()
            }
        }
        OutputItem::MermaidDiagram { source } => ItemSemanticSource {
            owner_revision: revision,
            source: source.clone(),
            isolated: vec![IsolatedSource {
                domain: SelectionDomain::Mermaid {
                    item_id: revision.id,
                },
                fragments: vec![CopyFragment::plain_text(source.clone())],
            }],
            ..Default::default()
        },
        OutputItem::CompactionSummary { summary, .. } => ItemSemanticSource {
            owner_revision: revision,
            source: summary.clone(),
            isolated: vec![IsolatedSource {
                domain: SelectionDomain::RawOutput {
                    item_id: revision.id,
                    output: "compaction-summary".into(),
                },
                fragments: vec![CopyFragment::plain_text(summary.clone())],
            }],
            ..Default::default()
        },
        OutputItem::SubAgentActivity { handle, output, .. } => ItemSemanticSource {
            owner_revision: revision,
            source: output.clone(),
            isolated: vec![IsolatedSource {
                domain: SelectionDomain::WorkflowOutput {
                    item_id: revision.id,
                    node: handle.clone(),
                },
                fragments: vec![CopyFragment::plain_text(output.clone())],
            }],
            ..Default::default()
        },
        _ => ItemSemanticSource {
            owner_revision: revision,
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(domain: SelectionDomain, ordinal: u64, grapheme: usize) -> SemanticPoint {
        SemanticPoint {
            domain,
            ordinal,
            grapheme,
            affinity: Affinity::Before,
        }
    }

    #[test]
    fn endpoints_reject_cross_domain_and_normalize_reverse_order() {
        let prose = SelectionDomain::TranscriptProse;
        let code = SelectionDomain::MarkdownCode {
            item_id: 7,
            block: 0,
        };
        assert!(normalize_endpoints(&point(prose.clone(), 1, 0), &point(code, 1, 0)).is_none());

        let (start, end) =
            normalize_endpoints(&point(prose.clone(), 4, 3), &point(prose.clone(), 2, 5)).unwrap();
        assert_eq!((start.ordinal, start.grapheme), (2, 5));
        assert_eq!((end.ordinal, end.grapheme), (4, 3));
        assert_eq!(
            clamp_endpoint(
                &point(prose.clone(), 9, 0),
                &point(prose.clone(), 2, 5),
                &point(prose, 4, 3),
            )
            .unwrap(),
            end
        );
    }

    #[test]
    fn pending_selection_activates_only_after_semantic_movement() {
        let revision = OutputRevision {
            id: 7,
            ..OutputRevision::default()
        };
        let anchor = point(SelectionDomain::TranscriptProse, 3, 2);
        let pending = selection_begin(anchor.clone(), revision, 11);
        assert_eq!(pending.phase, SelectionPhase::Pending);
        assert!(!selection_is_non_empty(&pending));
        assert!(!selection_contains(&pending, &anchor));

        let unchanged = selection_extend(&pending, anchor, revision, 11).unwrap();
        assert_eq!(unchanged.phase, SelectionPhase::Pending);
        assert!(!selection_is_non_empty(&unchanged));

        let moved = selection_extend(
            &unchanged,
            point(SelectionDomain::TranscriptProse, 3, 3),
            revision,
            11,
        )
        .unwrap();
        assert_eq!(moved.phase, SelectionPhase::Active);
        assert!(selection_is_non_empty(&moved));
    }

    #[test]
    fn pending_and_empty_selection_never_copy() {
        let revision = OutputRevision {
            id: 1,
            ..OutputRevision::default()
        };
        let endpoint = point(SelectionDomain::TranscriptProse, prose_ordinal(1, 0), 0);
        let projection = VisibleSelectionProjection {
            structure_revision: 3,
            surfaces: vec![surface(
                0,
                1,
                crate::app::OutputItem::UserTurn {
                    text: "hello".into(),
                },
            )],
        };
        let pending = selection_begin(endpoint, revision, projection.structure_revision);
        assert_eq!(selection_copy_payload(&projection, &pending), None);

        let mut active = pending;
        active.phase = SelectionPhase::Active;
        assert_eq!(selection_copy_payload(&projection, &active), None);
    }

    #[test]
    fn grapheme_slice_keeps_wide_emoji_and_combining_sequences_atomic() {
        let text = "A界👨‍👩‍👧‍👦e\u{301}Z";
        assert_eq!(grapheme_slice(text, 1..4), Some("界👨‍👩‍👧‍👦e\u{301}"));
        assert_eq!(grapheme_slice(text, 3..4), Some("e\u{301}"));
        let reverse_start = 4;
        let reverse_end = 2;
        assert_eq!(
            grapheme_slice(text, reverse_start..reverse_end),
            Some("👨‍👩‍👧‍👦e\u{301}")
        );
    }

    #[test]
    fn serializer_prefers_complete_markdown_source_and_slices_safe_leaves() {
        let source = "**bold** and 世界";
        let node = CopyFragment::markdown_node(0..8, "bold".into());
        assert_eq!(
            serialize_fragment(source, &node, 0..4),
            Some(CopyPayload::Markdown("**bold**".into()))
        );

        let leaf = CopyFragment::markdown_leaf(source, 9..source.len(), "and 世界".into());
        assert_eq!(
            serialize_fragment(source, &leaf, 4..7),
            Some(CopyPayload::Markdown("世界".into()))
        );
    }

    #[test]
    fn unsafe_markdown_slice_falls_back_to_plain_text() {
        let source = "&amp;";
        let fragment = CopyFragment::markdown_leaf(source, 0..source.len(), "&".into());
        assert_eq!(
            serialize_fragment(source, &fragment, 0..1),
            Some(CopyPayload::PlainText("&".into()))
        );
    }

    #[test]
    fn markdown_projection_separates_prose_from_fenced_and_indented_code() {
        let source = "Before **bold**.\n\n```rust\nfn main() {}\n```\n\n    let x = 1;\n    let y = 2;\n\nAfter.\n";
        let revision = OutputRevision {
            id: 42,
            semantic: 3,
            source_generation: 5,
            ..Default::default()
        };
        let projection = crate::markdown::semantic_markdown_source(source, revision);

        assert_eq!(projection.owner_revision, revision);
        assert_eq!(projection.code_blocks.len(), 2);
        assert_eq!(projection.code_blocks[0].body, "fn main() {}\n");
        assert_eq!(projection.code_blocks[1].body, "let x = 1;\nlet y = 2;\n");
        assert!(projection.code_blocks.iter().all(|block| {
            !block.body.contains("```")
                && !block.body.contains("rust")
                && !block.body.contains('│')
                && !block.body.lines().any(|line| line.starts_with("1 "))
        }));
        let indented = &projection.code_blocks[1];
        let mapped_body = indented
            .segments
            .iter()
            .map(|segment| &indented.body[segment.body_range.clone()])
            .collect::<String>();
        let mapped_source = indented
            .segments
            .iter()
            .map(|segment| &source[segment.source_range.clone()])
            .collect::<String>();
        assert_eq!(mapped_body, "let x = 1;\nlet y = 2;\n");
        assert_eq!(mapped_source, "let x = 1;\nlet y = 2;\n");
        assert_eq!(
            serialize_code_body(indented, 0..indented.body.graphemes(true).count()),
            Some(CopyPayload::PlainText("let x = 1;\nlet y = 2;\n".into()))
        );
        let copied = projection
            .prose
            .iter()
            .filter_map(|fragment| {
                let count = fragment.semantic_text.graphemes(true).count();
                serialize_fragment(source, fragment, 0..count)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            copied,
            vec![
                CopyPayload::Markdown("Before **bold**.\n\n".into()),
                CopyPayload::Markdown("After.\n".into()),
            ]
        );
    }

    #[test]
    fn reference_definition_is_not_part_of_adjacent_prose() {
        let source = "para\n\n[t]: https://x\n\nafter";
        let projection = crate::markdown::semantic_markdown_source(
            source,
            OutputRevision {
                id: 7,
                ..Default::default()
            },
        );

        assert_eq!(projection.prose.len(), 2);
        assert_eq!(
            serialize_fragment(source, &projection.prose[0], 0..4),
            Some(CopyPayload::Markdown("para\n\n".into()))
        );
        assert_eq!(
            serialize_fragment(source, &projection.prose[1], 0..5),
            Some(CopyPayload::Markdown("after".into()))
        );
    }

    #[test]
    fn prose_join_uses_one_blank_line_between_three_fragments() {
        let source = "one\n\n**two**\n\nthree\n";
        let projection = crate::markdown::semantic_markdown_source(
            source,
            OutputRevision {
                id: 8,
                ..Default::default()
            },
        );

        assert_eq!(
            serialize_prose_fragments(source, &projection.prose),
            Some(CopyPayload::Markdown("one\n\n**two**\n\nthree".into()))
        );
    }

    fn revision(id: u64) -> OutputRevision {
        OutputRevision {
            id,
            ..Default::default()
        }
    }

    fn surface(item_index: usize, id: u64, item: crate::app::OutputItem) -> VisibleSurface {
        let revision = revision(id);
        VisibleSurface {
            item_index,
            revision,
            start_row: item_index as u32,
            end_row: item_index as u32 + 1,
            source: std::sync::Arc::new(item_semantic_source(&item, revision)),
            prose_atoms: Vec::new(),
            code_atoms: Vec::new(),
            isolated_atoms: Vec::new(),
        }
    }

    fn thinking(text: &str) -> crate::app::OutputItem {
        crate::app::OutputItem::Thinking {
            text: text.to_owned(),
            done: true,
            disclosure: crate::app::Disclosure::default(),
            retried: false,
        }
    }

    fn assistant(md: &str) -> crate::app::OutputItem {
        crate::app::OutputItem::AssistantMd {
            md: md.to_owned(),
            streaming: false,
            retried: false,
        }
    }

    fn user(text: &str) -> crate::app::OutputItem {
        crate::app::OutputItem::UserTurn {
            text: text.to_owned(),
        }
    }

    fn selection(anchor: SemanticPoint, focus: SemanticPoint) -> SelectionState {
        SelectionState {
            phase: SelectionPhase::Retained,
            anchor,
            focus,
            owner_revision: OutputRevision::default(),
            structure_revision: 0,
            copied: false,
        }
    }

    fn prose_point(item_id: u64, fragment: usize, grapheme: usize) -> SemanticPoint {
        SemanticPoint {
            domain: SelectionDomain::TranscriptProse,
            ordinal: prose_ordinal(item_id, fragment),
            grapheme,
            affinity: Affinity::Before,
        }
    }

    #[test]
    fn prose_range_spans_items_and_skips_isolated_thinking() {
        let projection = VisibleSelectionProjection {
            structure_revision: 1,
            surfaces: vec![
                surface(0, 1, user("hello 世界🙂")),
                surface(1, 2, thinking("secret reasoning")),
                surface(2, 3, assistant("tail text")),
            ],
        };

        // "hello " is 6 graphemes, so the start point lands on 世.
        let state = selection(prose_point(1, 0, 6), prose_point(3, 0, 4));
        assert_eq!(
            projection.copy_selection(&state),
            Some(CopyPayload::PlainText("世界🙂\n\ntail".into()))
        );
        assert!(
            !matches!(
                projection.copy_selection(&state),
                Some(CopyPayload::PlainText(ref text)) if text.contains("secret")
            ),
            "isolated thinking text must never enter a prose selection"
        );
    }

    #[test]
    fn reversed_prose_endpoints_produce_identical_payload() {
        let projection = VisibleSelectionProjection {
            structure_revision: 1,
            surfaces: vec![surface(0, 1, assistant("alpha beta"))],
        };
        let forward = selection(prose_point(1, 0, 0), prose_point(1, 0, 5));
        let reverse = selection(prose_point(1, 0, 5), prose_point(1, 0, 0));
        assert_eq!(
            projection.copy_selection(&forward),
            projection.copy_selection(&reverse)
        );
        assert!(projection.copy_selection(&forward).is_some());
    }

    #[test]
    fn thinking_selection_stays_inside_its_own_domain() {
        let projection = VisibleSelectionProjection {
            structure_revision: 1,
            surfaces: vec![
                surface(0, 1, thinking("inner thought")),
                surface(1, 2, assistant("outside prose")),
            ],
        };
        let domain = SelectionDomain::Thinking { item_id: 1 };
        let state = selection(
            SemanticPoint {
                domain: domain.clone(),
                ordinal: 0,
                grapheme: 0,
                affinity: Affinity::Before,
            },
            SemanticPoint {
                domain,
                ordinal: 0,
                grapheme: 5,
                affinity: Affinity::Before,
            },
        );

        let payload = projection.copy_selection(&state).expect("thinking payload");
        let text = match &payload {
            CopyPayload::Markdown(text)
            | CopyPayload::PlainText(text)
            | CopyPayload::Preview(text) => text.as_str(),
        };
        assert_eq!(text, "inner");
        assert!(!text.contains("outside"));
    }

    #[test]
    fn code_selection_copies_body_without_gutter() {
        let projection = VisibleSelectionProjection {
            structure_revision: 1,
            surfaces: vec![surface(
                0,
                1,
                assistant("```rust\nfn a() {}\nfn b() {}\n```\n"),
            )],
        };
        let domain = SelectionDomain::MarkdownCode {
            item_id: 1,
            block: 0,
        };
        let body = &projection.code_block(&domain).expect("code block").body;
        let state = selection(
            SemanticPoint {
                domain: domain.clone(),
                ordinal: 0,
                grapheme: 0,
                affinity: Affinity::Before,
            },
            SemanticPoint {
                domain,
                ordinal: 0,
                grapheme: body.graphemes(true).count(),
                affinity: Affinity::Before,
            },
        );

        assert_eq!(
            projection.copy_selection(&state),
            Some(CopyPayload::PlainText("fn a() {}\nfn b() {}\n".into()))
        );
    }

    #[test]
    fn blank_and_empty_prose_ranges_copy_nothing() {
        let projection = VisibleSelectionProjection {
            structure_revision: 1,
            surfaces: vec![surface(0, 1, user("   spaced"))],
        };
        assert_eq!(
            projection.copy_selection(&selection(prose_point(1, 0, 2), prose_point(1, 0, 2))),
            None,
            "an empty range must not touch the clipboard"
        );
        assert_eq!(
            projection.copy_selection(&selection(prose_point(1, 0, 0), prose_point(1, 0, 3))),
            None,
            "a whitespace-only range must not touch the clipboard"
        );
    }

    #[test]
    fn nested_container_with_code_degrades_prose_to_plain_text() {
        let source = "> - before\n>\n>       code\n>\n>   after\n";
        let projection = crate::markdown::semantic_markdown_source(
            source,
            OutputRevision {
                id: 9,
                ..Default::default()
            },
        );

        assert_eq!(projection.code_blocks.len(), 1);
        assert!(!projection.prose.is_empty());
        assert!(
            projection
                .prose
                .iter()
                .all(|fragment| fragment.format == CopyFormat::PlainText)
        );
        assert!(matches!(
            serialize_prose_fragments(source, &projection.prose),
            Some(CopyPayload::PlainText(_))
        ));
    }
}
