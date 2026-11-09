//! Viewport projection for the prepared, source-bound right-hand inspector.

use super::*;

#[derive(Clone)]
pub(crate) struct Inspector {
    pub key: (String, Section),
    pub top: u32,
    pub bottom: u32,
    pub rows: u32,
    pub header_rows: u32,
    pub columns: std::ops::Range<u16>,
    pub regions: Arc<[NodeRegion]>,
}

#[derive(Default)]
pub(crate) struct InspectorScroll {
    key: Option<(String, Section)>,
    offset: u32,
}

impl InspectorScroll {
    pub fn scroll(&mut self, view: &InspectorViewport, delta: i32) -> bool {
        if delta == 0 || view.max_scroll == 0 {
            return false;
        }
        self.key = Some(view.key.clone());
        self.offset = view
            .offset
            .saturating_add_signed(delta)
            .min(view.max_scroll);
        true
    }
}

#[derive(Clone)]
pub(crate) struct InspectorViewport {
    key: (String, Section),
    pub mask_rows: std::ops::Range<u32>,
    pub columns: std::ops::Range<u16>,
    pub rows: Vec<(u32, u32)>, // Prepared source row, displayed document row.
    regions: Vec<NodeRegion>,
    offset: u32,
    max_scroll: u32,
}

impl Inspector {
    pub fn shift_rows(&mut self, offset: u32) {
        self.top += offset;
        self.bottom += offset;
        for region in Arc::make_mut(&mut self.regions) {
            region.start_row += offset;
            region.end_row += offset;
        }
    }

    pub fn viewport(
        &self,
        state: &mut InspectorScroll,
        row_offset: u32,
        column_offset: u16,
        scroll: u32,
        height: u32,
    ) -> Option<InspectorViewport> {
        if state.key.as_ref() != Some(&self.key) {
            state.key = Some(self.key.clone());
            state.offset = 0;
        }
        let origin = row_offset.saturating_add(self.top);
        let bottom = row_offset.saturating_add(self.bottom);
        let top = origin.max(scroll);
        let height = height
            .saturating_sub(top.saturating_sub(scroll))
            .min(bottom.saturating_sub(top))
            .min(self.rows);
        if height == 0 {
            return None;
        }
        let header = self.header_rows.min(height);
        let body_height = height - header;
        let max_scroll = if body_height == 0 {
            0
        } else {
            self.rows.saturating_sub(self.header_rows + body_height)
        };
        state.offset = state.offset.min(max_scroll);
        let offset = state.offset;
        let rows: Vec<_> = (0..height)
            .map(|row| {
                (
                    self.top + row + if row < header { 0 } else { offset },
                    top + row,
                )
            })
            .collect();
        let regions = self
            .regions
            .iter()
            .filter_map(|region| {
                let (_, displayed) = rows
                    .iter()
                    .find(|(source, _)| *source == region.start_row)?;
                Some(NodeRegion {
                    start_row: *displayed,
                    end_row: displayed + 1,
                    col_start: region.col_start + column_offset,
                    col_end: region.col_end + column_offset,
                    ..region.clone()
                })
            })
            .collect();
        Some(InspectorViewport {
            key: self.key.clone(),
            mask_rows: origin..bottom,
            columns: self.columns.start.saturating_add(column_offset)
                ..self.columns.end.saturating_add(column_offset),
            rows,
            regions,
            offset,
            max_scroll,
        })
    }
}

impl InspectorViewport {
    pub fn project_region<'a>(
        &self,
        region: &'a NodeRegion,
    ) -> Option<std::borrow::Cow<'a, NodeRegion>> {
        if self.mask_rows.contains(&region.start_row) && region.col_start >= self.columns.start {
            let (_, row) = self
                .rows
                .iter()
                .find(|(source, _)| *source == region.start_row)?;
            let mut region = region.clone();
            region.start_row = *row;
            region.end_row = row + 1;
            return Some(std::borrow::Cow::Owned(region));
        }
        Some(std::borrow::Cow::Borrowed(region))
    }

    pub fn bind_item(mut self, index: usize) -> Self {
        for region in &mut self.regions {
            region.panel_item_index = index;
        }
        self
    }

    pub fn matches(&self, inspector: &Inspector) -> bool {
        self.key == inspector.key
    }

    pub fn contains(&self, row: u32, column: u16) -> bool {
        self.columns.contains(&column)
            && self.rows.first().is_some_and(|(_, top)| row >= *top)
            && self.rows.last().is_some_and(|(_, bottom)| row <= *bottom)
    }

    pub fn project(
        &self,
        source: &[Line<'static>],
        visible: &mut [Line<'static>],
        regions: &mut Vec<NodeRegion>,
        scroll: u32,
        column_offset: u16,
        fade: Option<u8>,
    ) {
        let columns = self.columns.start.saturating_sub(column_offset)
            ..self.columns.end.saturating_sub(column_offset);
        let style = Style::default().bg(crate::theme::theme().work_bg.into());
        for (index, line) in visible.iter_mut().enumerate() {
            let row = scroll.saturating_add(index as u32);
            if self.mask_rows.contains(&row) {
                replace_columns(
                    line,
                    self.columns.clone(),
                    vec![Span::styled(
                        " ".repeat(usize::from(columns.end - columns.start)),
                        style,
                    )],
                );
            }
        }
        regions.retain(|region| {
            !self.mask_rows.contains(&region.start_row) || region.col_start < self.columns.start
        });
        for &(original, displayed) in &self.rows {
            let Some(line) = source.get(original as usize) else {
                continue;
            };
            if let Some(target) = displayed
                .checked_sub(scroll)
                .and_then(|row| visible.get_mut(row as usize))
            {
                let mut content = Line::from(slice(line, columns.clone()));
                if let Some(level) = fade {
                    fade_work_fold_line(&mut content, level);
                }
                replace_columns(target, self.columns.clone(), content.spans);
            }
        }
        regions.extend(self.regions.iter().cloned());
    }
}

fn slice(line: &Line<'static>, columns: std::ops::Range<u16>) -> Vec<Span<'static>> {
    let mut position = 0;
    let mut spans = Vec::new();
    for span in &line.spans {
        let end = position + crate::width::width(span.content.as_ref());
        let start = usize::from(columns.start).max(position);
        let stop = usize::from(columns.end).min(end);
        if start < stop {
            spans.push(Span::styled(
                crate::width::trim_display_offset(
                    span.content.as_ref(),
                    start - position,
                    stop - start,
                ),
                span.style,
            ));
        }
        position = end;
        if position >= usize::from(columns.end) {
            break;
        }
    }
    spans
}

fn replace_columns(
    line: &mut Line<'static>,
    columns: std::ops::Range<u16>,
    content: Vec<Span<'static>>,
) {
    let mut spans = slice(line, 0..columns.start);
    pad_spans_to_width(&mut spans, usize::from(columns.start), line.style);
    spans.extend(content);
    spans.extend(slice(line, columns.end..u16::MAX));
    line.spans = spans;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inspector() -> Inspector {
        Inspector {
            key: ("node".into(), Section::Arguments),
            top: 3,
            bottom: 80,
            rows: 30,
            header_rows: 3,
            columns: 8..20,
            regions: Arc::from([NodeRegion {
                panel_item_index: 0,
                path_key: Action::Close.key(),
                start_row: 3,
                end_row: 4,
                col_start: 17,
                col_end: 20,
            }]),
        }
    }

    #[test]
    fn sticky_inspector_projects_paint_and_controls_without_moving_the_tree() {
        let inspector = inspector();
        let source: Vec<_> = (0..80)
            .map(|row| Line::from(format!("tree{row:04}详情{row:04}    ")))
            .collect();
        for (row_offset, column_offset) in [(0, 0), (7, 3)] {
            for scroll in [0, 3, 20, 70, 79] {
                let mut state = InspectorScroll::default();
                let Some(view) =
                    inspector.viewport(&mut state, row_offset, column_offset, scroll, 12)
                else {
                    continue;
                };
                let first = scroll.saturating_sub(row_offset).min(80) as usize;
                let mut visible = source[first..(first + 12).min(80)].to_vec();
                for line in &mut visible {
                    line.spans
                        .insert(0, Span::raw(" ".repeat(column_offset as usize)));
                }
                let before = visible.clone();
                let mut regions = Vec::new();
                view.project(
                    &source,
                    &mut visible,
                    &mut regions,
                    scroll.max(row_offset),
                    column_offset,
                    None,
                );
                for (old, new) in before.iter().zip(&visible) {
                    assert_eq!(
                        slice(old, 0..view.columns.start),
                        slice(new, 0..view.columns.start)
                    );
                }
                for region in regions {
                    assert_eq!(region.start_row, (inspector.top + row_offset).max(scroll));
                    assert_eq!(region.col_start, 17 + column_offset);
                    assert!(view.contains(region.start_row, region.col_start));
                }
                assert_eq!(view.rows[0].0, inspector.top);
                assert!(
                    view.rows
                        .iter()
                        .all(|(_, row)| *row < inspector.bottom + row_offset)
                );
            }
        }
    }

    #[test]
    fn sticky_inspector_scroll_is_local_clamped_and_bound_to_node_and_section() {
        let mut inspector = inspector();
        let mut state = InspectorScroll::default();
        let view = inspector.viewport(&mut state, 0, 0, 20, 10).unwrap();
        assert!(state.scroll(&view, i32::MAX));
        let view = inspector.viewport(&mut state, 0, 0, 20, 10).unwrap();
        assert_eq!(
            view.rows.last().unwrap().0,
            inspector.top + inspector.rows - 1
        );
        assert_eq!(view.rows[0].0, inspector.top);
        assert_eq!(view.rows[2].0, inspector.top + 2);
        inspector.key.1 = Section::Result;
        assert_eq!(
            inspector.viewport(&mut state, 0, 0, 20, 10).unwrap().offset,
            0
        );
        inspector.key.1 = Section::Arguments;
        assert_eq!(
            inspector.viewport(&mut state, 0, 0, 20, 10).unwrap().offset,
            0
        );
        assert_eq!(
            inspector.viewport(&mut state, 0, 0, 20, 50).unwrap().offset,
            0
        );
        inspector.key.1 = Section::Result;
        assert_eq!(
            inspector.viewport(&mut state, 0, 0, 20, 10).unwrap().offset,
            0
        );
        inspector.key.0 = "another".into();
        assert_eq!(
            inspector.viewport(&mut state, 0, 0, 20, 10).unwrap().offset,
            0
        );
        assert!(inspector.viewport(&mut state, 0, 0, 80, 10).is_none());
        assert!(inspector.viewport(&mut state, 0, 0, 20, 0).is_none());
    }
}
