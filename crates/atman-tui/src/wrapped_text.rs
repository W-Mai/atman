#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SourceRow {
    line_start: usize,
    start: usize,
    end: usize,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum WrappedLineMode {
    #[default]
    Lines,
    SplitNewline,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WrappedRowUpdate {
    pub rebuilt: bool,
    pub indexed_bytes: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct WrappedRowIndex {
    generation: u64,
    source_len: usize,
    body_width: usize,
    line_mode: WrappedLineMode,
    rows: Vec<SourceRow>,
    source_guard: Vec<u8>,
}

impl WrappedRowIndex {
    const SOURCE_GUARD_BYTES: usize = 32;

    pub(crate) fn update(
        &mut self,
        source: &str,
        generation: u64,
        body_width: usize,
        line_mode: WrappedLineMode,
    ) -> WrappedRowUpdate {
        let body_width = body_width.max(1);
        let append_compatible = self.generation == generation
            && self.body_width == body_width
            && self.line_mode == line_mode
            && self.source_len <= source.len()
            && self.guard_matches(source);
        if !append_compatible {
            self.rows.clear();
            self.generation = generation;
            self.body_width = body_width;
            self.line_mode = line_mode;
            self.source_len = 0;
            self.index_suffix(source, 0);
            self.source_len = source.len();
            self.refresh_guard(source);
            return WrappedRowUpdate {
                rebuilt: true,
                indexed_bytes: source.len(),
            };
        }
        if self.source_len == source.len() {
            return WrappedRowUpdate {
                rebuilt: false,
                indexed_bytes: 0,
            };
        }

        let scan_start = if self.source_len > 0
            && source.as_bytes().get(self.source_len.saturating_sub(1)) != Some(&b'\n')
        {
            self.rows.last().map_or(0, |row| row.line_start)
        } else {
            self.source_len
        };
        self.rows.retain(|row| row.line_start < scan_start);
        self.index_suffix(source, scan_start);
        self.source_len = source.len();
        self.refresh_guard(source);
        WrappedRowUpdate {
            rebuilt: false,
            indexed_bytes: source.len().saturating_sub(scan_start),
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    pub(crate) fn row<'a>(&self, source: &'a str, index: usize) -> Option<&'a str> {
        let row = self.rows.get(index)?;
        source.get(row.start..row.end)
    }

    fn guard_matches(&self, source: &str) -> bool {
        if self.source_len == 0 {
            return true;
        }
        let start = self.source_len.saturating_sub(self.source_guard.len());
        source
            .as_bytes()
            .get(start..self.source_len)
            .is_some_and(|guard| guard == self.source_guard)
    }

    fn refresh_guard(&mut self, source: &str) {
        let start = source.len().saturating_sub(Self::SOURCE_GUARD_BYTES);
        self.source_guard.clear();
        self.source_guard
            .extend_from_slice(&source.as_bytes()[start..]);
    }

    fn index_suffix(&mut self, source: &str, mut line_start: usize) {
        if source.is_empty() && self.line_mode == WrappedLineMode::SplitNewline {
            self.index_line(source, 0, 0);
            return;
        }
        while line_start < source.len() {
            let newline = source[line_start..]
                .find('\n')
                .map(|relative| line_start.saturating_add(relative));
            let raw_end = newline.unwrap_or(source.len());
            let line_end = if self.line_mode == WrappedLineMode::Lines
                && raw_end > line_start
                && source.as_bytes().get(raw_end.saturating_sub(1)) == Some(&b'\r')
            {
                raw_end.saturating_sub(1)
            } else {
                raw_end
            };
            self.index_line(source, line_start, line_end);
            let Some(newline) = newline else {
                break;
            };
            line_start = newline.saturating_add(1);
            if line_start == source.len() {
                if self.line_mode == WrappedLineMode::SplitNewline {
                    self.index_line(source, line_start, line_start);
                }
                break;
            }
        }
    }

    fn index_line(&mut self, source: &str, line_start: usize, line_end: usize) {
        if line_start == line_end {
            self.rows.push(SourceRow {
                line_start,
                start: line_start,
                end: line_end,
            });
            return;
        }
        let mut row_start = line_start;
        let mut cursor = line_start;
        let mut row_width = 0usize;
        for (grapheme, grapheme_width) in crate::width::graphemes(&source[line_start..line_end]) {
            if row_width.saturating_add(grapheme_width) > self.body_width && row_start < cursor {
                self.rows.push(SourceRow {
                    line_start,
                    start: row_start,
                    end: cursor,
                });
                row_start = cursor;
                row_width = 0;
            }
            cursor = cursor.saturating_add(grapheme.len());
            row_width = row_width.saturating_add(grapheme_width);
        }
        self.rows.push(SourceRow {
            line_start,
            start: row_start,
            end: line_end,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn direct_rows(source: &str, body_width: usize) -> Vec<String> {
        source
            .lines()
            .flat_map(|line| crate::output::wrap_with_prefix(line, body_width + 2, "", ""))
            .map(|row| row.body)
            .collect()
    }

    fn indexed_rows(index: &WrappedRowIndex, source: &str) -> Vec<String> {
        (0..index.len())
            .map(|row| index.row(source, row).unwrap().to_string())
            .collect()
    }

    #[test]
    fn matches_direct_wrapping_corpus() {
        let corpus = [
            "",
            "one",
            "one\n",
            "one\n\nthree",
            "one\r\ntwo\r\n",
            "你好世界",
            "a😀b😀c",
            "e\u{301}e\u{301}e\u{301}",
            "abcdefghi",
            "\n",
        ];
        for source in corpus {
            for body_width in 1..=8 {
                let mut index = WrappedRowIndex::default();
                index.update(source, 1, body_width, WrappedLineMode::Lines);
                assert_eq!(
                    indexed_rows(&index, source),
                    direct_rows(source, body_width),
                    "source={source:?}, body_width={body_width}"
                );
            }
        }
    }

    #[test]
    fn line_aligned_append_indexes_only_new_bytes() {
        let mut index = WrappedRowIndex::default();
        let mut source = String::new();
        let mut indexed_bytes = 0usize;
        for line in ["alpha\n", "你好\n", "\n", "omega\n"] {
            source.push_str(line);
            indexed_bytes = indexed_bytes.saturating_add(
                index
                    .update(&source, 7, 4, WrappedLineMode::Lines)
                    .indexed_bytes,
            );
        }
        assert_eq!(indexed_bytes, source.len());
        assert_eq!(indexed_rows(&index, &source), direct_rows(&source, 4));
    }

    #[test]
    fn partial_line_append_reindexes_only_that_line() {
        let mut index = WrappedRowIndex::default();
        let mut source = "stable\npartial".to_string();
        index.update(&source, 9, 4, WrappedLineMode::Lines);
        source.push_str(" suffix\nnext\n");
        let update = index.update(&source, 9, 4, WrappedLineMode::Lines);

        assert!(!update.rebuilt);
        assert_eq!(update.indexed_bytes, "partial suffix\nnext\n".len());
        assert_eq!(indexed_rows(&index, &source), direct_rows(&source, 4));
    }

    #[test]
    fn replacement_shrink_width_and_guard_mismatch_rebuild() {
        let mut index = WrappedRowIndex::default();
        assert!(
            index
                .update("alpha\nbeta", 1, 5, WrappedLineMode::Lines)
                .rebuilt
        );
        assert!(
            index
                .update("alpha\ngamma", 2, 5, WrappedLineMode::Lines)
                .rebuilt
        );
        assert!(index.update("tiny", 2, 5, WrappedLineMode::Lines).rebuilt);
        assert!(index.update("tiny", 2, 3, WrappedLineMode::Lines).rebuilt);
        assert!(
            index
                .update("TINY plus", 2, 3, WrappedLineMode::Lines)
                .rebuilt
        );
    }

    #[test]
    fn split_newline_mode_matches_panel_semantics() {
        for source in ["", "one", "one\n", "\n", "one\n\nthree", "one\r\ntwo\r\n"] {
            let mut index = WrappedRowIndex::default();
            index.update(source, 1, 6, WrappedLineMode::SplitNewline);
            let direct = crate::output::wrap_with_prefix(source, 8, "", "")
                .into_iter()
                .map(|row| row.body)
                .collect::<Vec<_>>();
            assert_eq!(indexed_rows(&index, source), direct, "source={source:?}");
        }
    }
}
