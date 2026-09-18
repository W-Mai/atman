use std::ops::Range;

use ratatui::layout::Rect;
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SidebarSection {
    Goal,
    Plan,
    Todo,
    Context,
    Mcp,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SidebarCopyFormat {
    Markdown,
    PlainText,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarRowAtom {
    pub rect: Rect,
    pub cols: Range<u16>,
    pub cell_width: u16,
    pub graphemes: Range<usize>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SidebarSurface {
    pub section: SidebarSection,
    pub source: String,
    pub format: SidebarCopyFormat,
    pub atoms: Vec<SidebarRowAtom>,
}

impl SidebarSurface {
    pub fn goal(source: impl Into<String>, atoms: Vec<SidebarRowAtom>) -> Self {
        Self {
            section: SidebarSection::Goal,
            source: source.into(),
            format: SidebarCopyFormat::PlainText,
            atoms,
        }
    }

    pub fn plan<'a>(
        steps: impl IntoIterator<Item = (bool, &'a str)>,
        atoms: Vec<SidebarRowAtom>,
    ) -> Self {
        Self {
            section: SidebarSection::Plan,
            source: checklist_source(steps),
            format: SidebarCopyFormat::Markdown,
            atoms,
        }
    }

    pub fn todo<'a>(
        items: impl IntoIterator<Item = (bool, &'a str)>,
        atoms: Vec<SidebarRowAtom>,
    ) -> Self {
        Self {
            section: SidebarSection::Todo,
            source: checklist_source(items),
            format: SidebarCopyFormat::Markdown,
            atoms,
        }
    }

    pub fn context<'a>(
        rows: impl IntoIterator<Item = (&'a str, &'a str)>,
        atoms: Vec<SidebarRowAtom>,
    ) -> Self {
        Self {
            section: SidebarSection::Context,
            source: rows
                .into_iter()
                .map(|(key, value)| format!("{key}: {value}"))
                .collect::<Vec<_>>()
                .join("\n"),
            format: SidebarCopyFormat::PlainText,
            atoms,
        }
    }

    pub fn mcp<'a>(
        rows: impl IntoIterator<Item = (&'a str, &'a str, &'a str)>,
        atoms: Vec<SidebarRowAtom>,
    ) -> Self {
        Self {
            section: SidebarSection::Mcp,
            source: rows
                .into_iter()
                .map(|(name, transport, state)| format!("{name} — {transport} — {state}"))
                .collect::<Vec<_>>()
                .join("\n"),
            format: SidebarCopyFormat::PlainText,
            atoms,
        }
    }

    pub fn point_at(&self, row: u16, col: u16) -> Option<usize> {
        let atom = self.atoms.iter().find(|atom| {
            row >= atom.rect.y
                && row < atom.rect.bottom()
                && col >= atom.cols.start
                && col < atom.cols.end
                && atom.cell_width > 0
        })?;
        let step = usize::from((col - atom.cols.start) / atom.cell_width);
        Some(atom.graphemes.start.saturating_add(step))
    }

    pub fn serialize(&self, graphemes: Range<usize>) -> Option<String> {
        let count = self.source.graphemes(true).count();
        let start = graphemes.start.min(graphemes.end).min(count);
        let end = graphemes.start.max(graphemes.end).min(count);
        if start >= end {
            return None;
        }
        let start_byte = self
            .source
            .grapheme_indices(true)
            .nth(start)
            .map_or(self.source.len(), |(byte, _)| byte);
        let end_byte = self
            .source
            .grapheme_indices(true)
            .nth(end)
            .map_or(self.source.len(), |(byte, _)| byte);
        let text = self.source[start_byte..end_byte].to_owned();
        (!text.trim().is_empty()).then_some(text)
    }
}

fn checklist_source<'a>(items: impl IntoIterator<Item = (bool, &'a str)>) -> String {
    items
        .into_iter()
        .map(|(done, text)| format!("- [{}] {text}", if done { 'x' } else { ' ' }))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SidebarSelectionProjection {
    pub surfaces: Vec<SidebarSurface>,
}

impl SidebarSelectionProjection {
    pub fn surface_at(&self, row: u16, col: u16) -> Option<&SidebarSurface> {
        self.surfaces
            .iter()
            .find(|surface| surface.point_at(row, col).is_some())
    }

    pub fn point_at(&self, section: SidebarSection, row: u16, col: u16) -> Option<usize> {
        self.surfaces
            .iter()
            .find(|surface| surface.section == section)?
            .point_at(row, col)
    }
}

pub fn row_atoms(
    rect: Rect,
    start_col: u16,
    text: &str,
    grapheme_start: usize,
) -> Vec<SidebarRowAtom> {
    let mut atoms = Vec::new();
    let mut col = start_col;
    let mut grapheme = grapheme_start;
    let mut run: Option<(u16, u16, Range<usize>)> = None;

    for (_, _, cells) in crate::width::grapheme_indices(text) {
        let cells = u16::try_from(cells).unwrap_or(u16::MAX);
        if col.saturating_add(cells) > rect.right() {
            break;
        }
        match run.as_mut() {
            Some((_, width, range)) if *width == cells => range.end += 1,
            _ => {
                if let Some((run_col, width, range)) = run.take() {
                    atoms.push(SidebarRowAtom {
                        rect,
                        cols: run_col..col,
                        cell_width: width,
                        graphemes: range,
                    });
                }
                run = Some((col, cells, grapheme..grapheme.saturating_add(1)));
            }
        }
        col = col.saturating_add(cells);
        grapheme = grapheme.saturating_add(1);
    }
    if let Some((run_col, width, range)) = run {
        atoms.push(SidebarRowAtom {
            rect,
            cols: run_col..col,
            cell_width: width,
            graphemes: range,
        });
    }
    atoms
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_atoms_map_cjk_and_emoji_by_terminal_cells() {
        let atoms = row_atoms(Rect::new(2, 4, 20, 1), 4, "A界🙂", 0);
        let surface = SidebarSurface {
            section: SidebarSection::Goal,
            source: "A界🙂".into(),
            format: SidebarCopyFormat::PlainText,
            atoms,
        };
        assert_eq!(surface.point_at(4, 4), Some(0));
        assert_eq!(surface.point_at(4, 5), Some(1));
        assert_eq!(surface.point_at(4, 6), Some(1));
        assert_eq!(surface.point_at(4, 7), Some(2));
        assert_eq!(surface.point_at(4, 8), Some(2));
    }

    #[test]
    fn projection_never_crosses_sections() {
        let goal = SidebarSurface {
            section: SidebarSection::Goal,
            source: "goal".into(),
            format: SidebarCopyFormat::PlainText,
            atoms: row_atoms(Rect::new(0, 1, 10, 1), 0, "goal", 0),
        };
        let context = SidebarSurface {
            section: SidebarSection::Context,
            source: "model: test".into(),
            format: SidebarCopyFormat::PlainText,
            atoms: row_atoms(Rect::new(0, 3, 12, 1), 0, "model: test", 0),
        };
        let projection = SidebarSelectionProjection {
            surfaces: vec![goal, context],
        };
        assert_eq!(projection.point_at(SidebarSection::Goal, 1, 1), Some(1));
        assert_eq!(projection.point_at(SidebarSection::Goal, 3, 1), None);
    }

    #[test]
    fn row_atoms_clip_at_the_surface_rect() {
        let atoms = row_atoms(Rect::new(3, 2, 4, 1), 3, "abcdef", 0);
        assert_eq!(atoms.last().map(|atom| atom.cols.end), Some(7));
        assert_eq!(atoms.last().map(|atom| atom.graphemes.end), Some(4));
    }

    #[test]
    fn checklist_second_row_maps_to_its_text() {
        let source = "- [x] done\n- [ ] next";
        let mut atoms = row_atoms(Rect::new(0, 1, 20, 1), 0, "done", 6);
        atoms.extend(row_atoms(Rect::new(0, 2, 20, 1), 0, "next", 17));
        let surface = SidebarSurface {
            section: SidebarSection::Plan,
            source: source.into(),
            format: SidebarCopyFormat::Markdown,
            atoms,
        };
        let start = surface.point_at(2, 0).unwrap();
        assert_eq!(surface.serialize(start..start + 4), Some("next".into()));
    }

    #[test]
    fn serializer_preserves_checklist_source() {
        let source = "- [x] done\n- [ ] next";
        let surface = SidebarSurface {
            section: SidebarSection::Plan,
            source: source.into(),
            format: SidebarCopyFormat::Markdown,
            atoms: Vec::new(),
        };
        assert_eq!(
            surface.serialize(0..source.graphemes(true).count()),
            Some(source.into())
        );
    }
}
