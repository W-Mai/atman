use crate::keys::KeyAction;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserRowKind {
    Provider,
    Alias,
    Model,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserRow {
    pub kind: BrowserRowKind,
    pub label: String,
    pub value: String,
    pub selectable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserAction {
    Consumed,
    Selected,
    Cancelled,
}

#[derive(Debug, Clone, Default)]
pub struct ModelBrowser {
    rows: Vec<BrowserRow>,
    selected: usize,
    viewport: usize,
    visible_rows: usize,
}

impl ModelBrowser {
    pub fn new(rows: Vec<BrowserRow>, initial_value: Option<&str>) -> Self {
        let mut browser = Self {
            rows,
            visible_rows: 1,
            ..Self::default()
        };
        if let Some(value) = initial_value {
            browser.selected = browser
                .rows
                .iter()
                .position(|row| row.selectable && row.value == value)
                .unwrap_or_else(|| browser.first_selectable().unwrap_or(0));
        } else if !browser
            .rows
            .get(browser.selected)
            .is_some_and(|row| row.selectable)
        {
            browser.selected = browser.first_selectable().unwrap_or(0);
        }
        browser.ensure_visible(usize::MAX);
        browser
    }

    pub fn replace_rows(&mut self, rows: Vec<BrowserRow>, initial_value: Option<&str>) {
        *self = Self::new(rows, initial_value);
    }

    pub fn rows(&self) -> &[BrowserRow] {
        &self.rows
    }

    pub fn selected(&self) -> Option<&BrowserRow> {
        self.rows.get(self.selected).filter(|row| row.selectable)
    }

    fn first_selectable(&self) -> Option<usize> {
        self.rows.iter().position(|row| row.selectable)
    }

    fn move_selection(&mut self, direction: isize) {
        if self.rows.is_empty() {
            return;
        }
        let mut index = self.selected as isize;
        loop {
            let next = (index + direction).clamp(0, self.rows.len() as isize - 1);
            if next == index {
                return;
            }
            index = next;
            if self.rows[index as usize].selectable {
                self.selected = index as usize;
                return;
            }
        }
    }

    fn move_to_edge(&mut self, from_end: bool) {
        let mut iter: Box<dyn Iterator<Item = (usize, &BrowserRow)>> = if from_end {
            Box::new(self.rows.iter().enumerate().rev())
        } else {
            Box::new(self.rows.iter().enumerate())
        };
        if let Some((index, _)) = iter.find(|(_, row)| row.selectable) {
            self.selected = index;
        }
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// Paginate padded records, repeating the provider heading on continuation pages.
    pub fn grouped_pages(
        &self,
        height: u16,
        provider_height: u16,
        model_height: u16,
    ) -> Vec<Vec<usize>> {
        let height = u32::from(height);
        let provider_height = u32::from(provider_height);
        let model_height = u32::from(model_height);
        if height < provider_height + model_height || model_height == 0 {
            return Vec::new();
        }
        let mut pages = Vec::new();
        let mut page = Vec::new();
        let mut used = 0;
        let mut provider = None;
        for (index, row) in self.rows.iter().enumerate() {
            if row.kind == BrowserRowKind::Provider {
                let has_models = self
                    .rows
                    .get(index + 1)
                    .is_some_and(|next| next.kind != BrowserRowKind::Provider);
                let needed = provider_height + if has_models { model_height } else { 0 };
                if used + needed > height && !page.is_empty() {
                    pages.push(std::mem::take(&mut page));
                    used = 0;
                }
                page.push(index);
                used += provider_height;
                provider = Some(index);
            } else {
                if used + model_height > height {
                    pages.push(std::mem::take(&mut page));
                    used = 0;
                    if let Some(header) = provider {
                        page.push(header);
                        used += provider_height;
                    }
                }
                page.push(index);
                used += model_height;
            }
        }
        if !page.is_empty() {
            pages.push(page);
        }
        pages
    }

    /// Focus the first selectable record on a page, or its empty provider heading.
    pub fn focus_page(&mut self, rows: &[usize]) {
        if let Some(index) = rows
            .iter()
            .find(|index| self.rows.get(**index).is_some_and(|row| row.selectable))
            .or_else(|| rows.first())
        {
            self.selected = *index;
            self.ensure_visible(self.visible_rows);
        }
    }

    pub fn select_visible_row(&mut self, row: usize) -> bool {
        let index = self.viewport.saturating_add(row);
        if !self.rows.get(index).is_some_and(|entry| entry.selectable) {
            return false;
        }
        self.selected = index;
        self.ensure_visible(self.visible_rows);
        true
    }

    pub fn viewport(&self) -> usize {
        self.viewport
    }

    pub fn set_visible_rows(&mut self, height: usize) {
        self.visible_rows = height.max(1);
        self.ensure_visible(self.visible_rows);
    }

    pub fn visible_rows(&self, height: usize) -> std::ops::Range<usize> {
        let height = height.max(1);
        let start = self.viewport.min(self.rows.len());
        let end = (start + height).min(self.rows.len());
        start..end
    }

    pub fn handle_key(&mut self, action: &KeyAction, visible_rows: usize) -> BrowserAction {
        let visible_rows = if visible_rows == 0 {
            self.visible_rows
        } else {
            visible_rows
        };
        let len = self.rows.len();
        if len == 0 {
            return match action {
                KeyAction::Escape => BrowserAction::Cancelled,
                _ => BrowserAction::Consumed,
            };
        }
        let page = visible_rows.max(1);
        match action {
            KeyAction::HistoryUp | KeyAction::Char('k') => self.move_selection(-1),
            KeyAction::HistoryDown | KeyAction::Char('j') => self.move_selection(1),
            KeyAction::PageUp => {
                self.selected = self.selected.saturating_sub(page);
                while self.selected > 0 && !self.rows[self.selected].selectable {
                    self.selected -= 1;
                }
                if !self.rows[self.selected].selectable {
                    self.move_to_edge(false);
                }
            }
            KeyAction::PageDown => {
                self.selected = (self.selected + page).min(len - 1);
                while self.selected + 1 < len && !self.rows[self.selected].selectable {
                    self.selected += 1;
                }
                if !self.rows[self.selected].selectable {
                    self.move_to_edge(true);
                }
            }
            KeyAction::Home | KeyAction::CursorHome => self.move_to_edge(false),
            KeyAction::End | KeyAction::CursorEnd => self.move_to_edge(true),
            KeyAction::Submit => return BrowserAction::Selected,
            KeyAction::Escape => return BrowserAction::Cancelled,
            _ => return BrowserAction::Consumed,
        }
        self.ensure_visible(page);
        BrowserAction::Consumed
    }

    fn ensure_visible(&mut self, visible_rows: usize) {
        if self.rows.is_empty() {
            self.selected = 0;
            self.viewport = 0;
            return;
        }
        let height = visible_rows.max(1);
        if self.selected < self.viewport {
            self.viewport = self.selected;
        } else if self.selected >= self.viewport + height {
            self.viewport = self.selected + 1 - height;
        }
        if height > 1
            && self.selected > 0
            && self.rows[self.selected].kind == BrowserRowKind::Model
            && self.rows[self.selected - 1].kind == BrowserRowKind::Provider
        {
            self.viewport = self.viewport.min(self.selected - 1);
        }
        self.viewport = self.viewport.min(self.rows.len().saturating_sub(height));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grouped_pages_preserve_source_headers_counts_and_empty_groups() {
        let rows: Vec<_> = [("first", 5), ("empty", 0), ("last", 2)]
            .into_iter()
            .flat_map(|(provider, count)| {
                std::iter::once(BrowserRow {
                    kind: BrowserRowKind::Provider,
                    label: provider.into(),
                    value: provider.into(),
                    selectable: false,
                })
                .chain((0..count).map(move |i| BrowserRow {
                    kind: BrowserRowKind::Model,
                    label: format!("{provider}-{i}"),
                    value: format!("{provider}-{i}"),
                    selectable: true,
                }))
            })
            .collect();
        let mut browser = ModelBrowser::new(rows, None);
        for height in [7, 8, 11, 14, 25] {
            let pages = browser.grouped_pages(height, 3, 4);
            let mut models = Vec::new();
            let mut providers = std::collections::HashSet::new();
            for page in &pages {
                let mut source = None;
                let mut used = 0;
                for &index in page {
                    let row = &browser.rows()[index];
                    if row.kind == BrowserRowKind::Provider {
                        source = Some(row.value.as_str());
                        providers.insert(row.value.clone());
                        used += 3;
                    } else {
                        assert!(row.value.starts_with(source.unwrap()));
                        used += 4;
                        models.push(row.value.clone());
                    }
                }
                assert!(used <= height);
                browser.focus_page(page);
                assert!(page.contains(&browser.selected_index()));
                assert_eq!(
                    browser.selected().is_some(),
                    page.iter().any(|&i| browser.rows()[i].selectable)
                );
            }
            assert_eq!(providers.len(), 3);
            assert_eq!(models.len(), 7);
            assert_eq!(
                models
                    .iter()
                    .collect::<std::collections::HashSet<_>>()
                    .len(),
                7
            );
        }
    }

    fn browser(count: usize) -> ModelBrowser {
        ModelBrowser::new(
            (0..count)
                .map(|i| BrowserRow {
                    kind: BrowserRowKind::Model,
                    label: format!("model-{i}"),
                    value: format!("provider:model-{i}"),
                    selectable: true,
                })
                .collect(),
            None,
        )
    }

    #[test]
    fn selection_stays_visible_when_moving_past_viewport() {
        let mut b = browser(20);
        for _ in 0..19 {
            b.handle_key(&KeyAction::Char('j'), 5);
            assert!(b.selected_index() >= b.viewport());
            assert!(b.selected_index() < b.viewport() + 5);
        }
        assert_eq!(b.selected_index(), 19);
        assert_eq!(b.viewport(), 15);
    }

    #[test]
    fn page_home_end_and_up_scroll_correctly() {
        let mut b = browser(20);
        b.handle_key(&KeyAction::PageDown, 5);
        assert_eq!(b.selected_index(), 5);
        assert_eq!(b.viewport(), 1);
        b.handle_key(&KeyAction::End, 5);
        assert_eq!(b.selected_index(), 19);
        assert_eq!(b.viewport(), 15);
        b.handle_key(&KeyAction::PageUp, 5);
        assert_eq!(b.selected_index(), 14);
        assert_eq!(b.viewport(), 14);
        b.handle_key(&KeyAction::Home, 5);
        assert_eq!(b.selected_index(), 0);
        assert_eq!(b.viewport(), 0);
    }

    #[test]
    fn provider_headers_are_skipped_by_selection_but_count_in_viewport() {
        let mut b = ModelBrowser::new(
            vec![
                BrowserRow {
                    kind: BrowserRowKind::Provider,
                    label: "provider".into(),
                    value: "provider".into(),
                    selectable: false,
                },
                BrowserRow {
                    kind: BrowserRowKind::Model,
                    label: "first".into(),
                    value: "first".into(),
                    selectable: true,
                },
                BrowserRow {
                    kind: BrowserRowKind::Model,
                    label: "second".into(),
                    value: "second".into(),
                    selectable: true,
                },
            ],
            None,
        );
        assert_eq!(b.selected().unwrap().value, "first");
        b.handle_key(&KeyAction::Char('j'), 2);
        assert_eq!(b.selected().unwrap().value, "second");
        assert_eq!(b.viewport(), 1);
    }

    #[test]
    fn returning_to_group_first_model_reveals_provider_header() {
        let mut b = ModelBrowser::new(
            vec![
                BrowserRow {
                    kind: BrowserRowKind::Provider,
                    label: "provider".into(),
                    value: "provider".into(),
                    selectable: false,
                },
                BrowserRow {
                    kind: BrowserRowKind::Model,
                    label: "first".into(),
                    value: "first".into(),
                    selectable: true,
                },
                BrowserRow {
                    kind: BrowserRowKind::Model,
                    label: "second".into(),
                    value: "second".into(),
                    selectable: true,
                },
            ],
            None,
        );
        b.set_visible_rows(2);
        b.handle_key(&KeyAction::End, 0);
        assert_eq!(b.viewport(), 1);
        b.handle_key(&KeyAction::Home, 0);
        assert_eq!(b.selected().unwrap().value, "first");
        assert_eq!(b.viewport(), 0);
        assert_eq!(b.visible_rows(2).start, 0);
    }

    #[test]
    fn initial_value_selects_matching_internal_row() {
        let b = ModelBrowser::new(
            vec![
                BrowserRow {
                    kind: BrowserRowKind::Alias,
                    label: "smart".into(),
                    value: "smart".into(),
                    selectable: true,
                },
                BrowserRow {
                    kind: BrowserRowKind::Model,
                    label: "Codex / gpt".into(),
                    value: "codex-account:gpt".into(),
                    selectable: true,
                },
            ],
            Some("codex-account:gpt"),
        );
        assert_eq!(b.selected().unwrap().value, "codex-account:gpt");
    }

    #[test]
    fn initial_selection_is_visible() {
        let b = ModelBrowser::new(
            vec![BrowserRow {
                kind: BrowserRowKind::Model,
                label: "last".into(),
                value: "last".into(),
                selectable: true,
            }],
            Some("last"),
        );
        assert_eq!(b.selected_index(), 0);
        assert_eq!(b.viewport(), 0);
    }
}
