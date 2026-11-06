//! Immutable options shared by one synchronous rendering operation.

use std::{cell::Cell, marker::PhantomData, rc::Rc};

use atman_runtime::config_hub::DiffLayout;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RenderPreferences {
    pub math: bool,
    pub diff_layout: DiffLayout,
}

impl Default for RenderPreferences {
    fn default() -> Self {
        Self {
            math: true,
            diff_layout: DiffLayout::Split,
        }
    }
}

thread_local! {
    static CURRENT: Cell<RenderPreferences> = const { Cell::new(RenderPreferences {
        math: true, diff_layout: DiffLayout::Split,
    }) };
    static PASS: Cell<RenderPass> = const { Cell::new(RenderPass::Paint) };
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum RenderPass {
    Paint,
    Measure,
}

struct PassScope {
    previous: RenderPass,
    _local: PhantomData<Rc<()>>,
}

impl Drop for PassScope {
    fn drop(&mut self) {
        PASS.with(|pass| pass.set(self.previous));
    }
}

impl RenderPass {
    pub(crate) fn is_measurement() -> bool {
        PASS.with(|pass| pass.get() == Self::Measure)
    }

    pub(crate) fn enter(self) -> impl Drop {
        PassScope {
            previous: PASS.with(|pass| pass.replace(self)),
            _local: PhantomData,
        }
    }
}

struct Scope {
    previous: RenderPreferences,
    _local: PhantomData<Rc<()>>,
}

impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with(|current| current.set(self.previous));
    }
}

impl RenderPreferences {
    pub(crate) fn current() -> Self {
        CURRENT.with(Cell::get)
    }

    /// Render with captured options. Nested rendering shares these options; returning or unwinding restores the previous scope. The callback must complete synchronously.
    pub fn with<T>(self, render: impl FnOnce() -> T) -> T {
        let _scope = self.enter();
        render()
    }

    pub(crate) fn enter(self) -> impl Drop {
        Scope {
            previous: CURRENT.with(|current| current.replace(self)),
            _local: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scopes_are_nested_thread_local_and_restore_after_unwind() {
        let outer = RenderPreferences {
            math: false,
            diff_layout: DiffLayout::Unified,
        };
        outer.with(|| {
            assert_eq!(RenderPreferences::current(), outer);
            std::thread::spawn(|| {
                assert_eq!(RenderPreferences::current(), RenderPreferences::default())
            })
            .join()
            .unwrap();
            let result = std::panic::catch_unwind(|| {
                RenderPreferences::default().with(|| panic!("render failed"))
            });
            assert!(result.is_err());
            assert_eq!(RenderPreferences::current(), outer);
        });
        assert_eq!(RenderPreferences::current(), RenderPreferences::default());
    }

    #[test]
    fn layout_and_streaming_interpret_the_same_captured_options() {
        use crate::{
            app::{FlowDocument, OutputItem},
            output::{LayoutCache, LayoutKey, LayoutRequest, RenderCtx},
        };
        let text = "$$\nx = y\n$$\n\nplain\n\nnext\n\nlast";
        let document = FlowDocument::from(vec![OutputItem::AssistantMd {
            md: text.into(),
            streaming: false,
            retried: false,
        }]);
        let key = LayoutKey {
            width: 80,
            theme: crate::theme::current_mode(),
        };
        let request = LayoutRequest {
            scroll_offset: 0,
            viewport_rows: 40,
            follow_tail_rows: None,
        };
        let context = RenderCtx::empty();
        let mut cached = LayoutCache::default();
        let mut streaming = crate::markdown::StreamingMarkdownProjection::new(1);
        for math in [true, false, true] {
            RenderPreferences {
                math,
                ..Default::default()
            }
            .with(|| {
                let actual = cached.update_dirty(key, &document, &context, request);
                let actual = cached.visible_slice(actual.scroll_offset, 40, 0);
                let mut fresh = LayoutCache::default();
                let expected = fresh.update_dirty(key, &document, &context, request);
                let expected = fresh.visible_slice(expected.scroll_offset, 40, 0);
                assert_eq!(actual.lines, expected.lines);
                assert_eq!(
                    actual.selection.surfaces.len(),
                    expected.selection.surfaces.len()
                );
                for (actual, expected) in actual
                    .selection
                    .surfaces
                    .iter()
                    .zip(&expected.selection.surfaces)
                {
                    assert_eq!(actual.source.prose, expected.source.prose);
                    assert_eq!(actual.source.prose_atoms, expected.source.prose_atoms);
                }
                streaming.update(text, 1, 80, std::time::Instant::now(), true);
                let mut lines = Vec::new();
                streaming.append_range(0, streaming.rows(), &mut lines);
                assert_eq!(
                    lines.last_mut().unwrap().spans.pop(),
                    Some(ratatui::text::Span::styled(
                        "▏",
                        ratatui::style::Style::default()
                            .add_modifier(ratatui::style::Modifier::SLOW_BLINK)
                    ))
                );
                assert_eq!(lines, crate::markdown::render_markdown_with_width(text, 80));
            });
        }
    }
}
