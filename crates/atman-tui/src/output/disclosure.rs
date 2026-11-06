//! Exact disclosure transitions prepared with the retained document body.

use super::*;
use crate::flow_view::ToolDisclosureAction;

#[derive(Default)]
pub(super) struct Hints {
    markdown: Option<Disclosure>,
    tools: Vec<(String, Disclosure, Disclosure)>,
}

impl Hints {
    pub(super) fn prepare(item: &OutputItem, width: u16) -> Arc<Self> {
        let mut hints = Self::default();
        match item {
            OutputItem::Thinking {
                text, disclosure, ..
            } => {
                hints.markdown = Some(next_thinking_disclosure(text, *disclosure, width));
            }
            OutputItem::CompactionSummary {
                summary,
                disclosure,
                ..
            } => {
                hints.markdown = Some(next_compaction_disclosure(summary, *disclosure, width));
            }
            OutputItem::ToolDispatch { calls } => {
                hints.tools = calls
                    .iter()
                    .map(|call| {
                        (
                            call.id.clone(),
                            toggle_tool_call_content_disclosure(call),
                            toggle_tool_call_detail_disclosure(call, width),
                        )
                    })
                    .collect();
            }
            _ => {}
        }
        Arc::new(hints)
    }
}

impl LayoutCache {
    fn disclosure_hints(&self, document: &FlowDocument, index: usize) -> Option<&Hints> {
        if self.source_identity.as_ref() != Some(&document.identity()) {
            return None;
        }
        let entry = self.entries.get(index)?;
        let actual = document.revisions().get(index)?;
        let prepared = entry.revision;
        (prepared.id == actual.id
            && prepared.semantic == actual.semantic
            && prepared.source_generation == actual.source_generation
            && prepared.layout == actual.layout
            && !entry.content_hidden
            && entry.has_retained_lines())
        .then_some(entry.disclosure.as_ref())
    }

    pub(crate) fn next_markdown_disclosure(
        &self,
        document: &FlowDocument,
        index: usize,
    ) -> Option<Disclosure> {
        self.disclosure_hints(document, index)?.markdown
    }

    pub(crate) fn next_tool_disclosure(
        &self,
        document: &FlowDocument,
        index: usize,
        id: &str,
        action: ToolDisclosureAction,
    ) -> Option<Disclosure> {
        if let ToolDisclosureAction::Detail { width } = action
            && self.entries.get(index)?.outer_width != width
        {
            return None;
        }
        let hints = self.disclosure_hints(document, index)?;
        let (_, content, detail) = hints.tools.iter().find(|(call, _, _)| call == id)?;
        Some(match action {
            ToolDisclosureAction::Content => *content,
            ToolDisclosureAction::Detail { .. } => *detail,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        app::{AppState, ToolCallStatus, ToolCallView},
        flow_view::OutputMutation,
    };
    use std::time::{Duration, Instant};

    fn update(cache: &mut LayoutCache, document: &FlowDocument) {
        cache.update_dirty(
            LayoutKey {
                width: 80,
                theme: crate::theme::current_mode(),
            },
            document,
            &RenderCtx::empty(),
            LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 80,
                follow_tail_rows: None,
            },
        );
    }

    fn settle(cache: &mut LayoutCache, document: &FlowDocument) {
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            update(cache, document);
            assert!(cache.preparation_error().is_none());
            if !cache.preparing() {
                break;
            }
            assert!(Instant::now() < until);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn prepared_disclosure_preserves_exact_cycles_and_rejects_changed_sources() {
        for text in [
            "short".to_owned(),
            "line 汉\n\n".repeat(100),
            "```rust\nlet value = 1;\n```".into(),
        ] {
            let mut app = AppState::new("test".into(), None);
            app.items.replace(vec![OutputItem::Thinking {
                text: text.clone(),
                disclosure: Disclosure::Summary,
                done: true,
                retried: false,
            }]);
            app.layout_cache.enable_background();
            for _ in 0..4 {
                settle(&mut app.layout_cache, &app.items);
                let OutputItem::Thinking { disclosure, .. } = &app.items[0] else {
                    unreachable!()
                };
                let expected = next_thinking_disclosure(&text, *disclosure, 80);
                reset_perf_counters();
                app.cycle_thinking_disclosure(0);
                assert!(
                    matches!(&app.items[0], OutputItem::Thinking { disclosure, .. } if *disclosure == expected)
                );
                assert_eq!(
                    perf_counters().item_renders,
                    0,
                    "input cannot render a body"
                );
            }
            settle(&mut app.layout_cache, &app.items);
            app.items.touch(0, OutputMutation::Paint);
            assert!(
                app.layout_cache
                    .next_markdown_disclosure(&app.items, 0)
                    .is_some()
            );
            app.items.touch(0, OutputMutation::Semantic);
            assert!(
                app.layout_cache
                    .next_markdown_disclosure(&app.items, 0)
                    .is_none()
            );
            let other = FlowDocument::from(vec![app.items[0].clone()]);
            assert!(
                app.layout_cache
                    .next_markdown_disclosure(&other, 0)
                    .is_none()
            );
        }
    }

    #[test]
    fn tool_transitions_use_prepared_body_and_leave_missing_preparation_untouched() {
        for long in [false, true] {
            let call = ToolCallView {
                id: "call".into(),
                tool: "fs.read".into(),
                intent: "read".into(),
                input: serde_json::json!({"path":"source.rs"}),
                status: ToolCallStatus::Ok,
                disclosure: Disclosure::Summary,
                detail: Some(std::sync::Arc::new(OutputItem::Bash {
                    handle: "log".into(),
                    title: None,
                    command: None,
                    output: "line\n".repeat(if long { 1000 } else { 1 }),
                    done: true,
                    expanded: false,
                })),
                draft_index: None,
                draft_preview: Default::default(),
                applied_edit: None,
                started_at: Instant::now(),
                ended_at: None,
            };
            let mut app = AppState::new("test".into(), None);
            app.items.replace(vec![OutputItem::ToolDispatch {
                calls: vec![call.clone()],
            }]);
            app.layout_cache.enable_background();
            app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 80));
            app.toggle_tool_call_content(0, "call");
            assert!(
                matches!(&app.items[0], OutputItem::ToolDispatch { calls } if calls[0].disclosure == Disclosure::Summary)
            );
            settle(&mut app.layout_cache, &app.items);
            app.toggle_tool_call_content(0, "call");
            settle(&mut app.layout_cache, &app.items);
            let mut preview = call;
            preview.disclosure = Disclosure::Preview;
            let expected = toggle_tool_call_detail_disclosure(&preview, 80);
            reset_perf_counters();
            app.toggle_tool_call_detail(0, "call");
            assert!(
                matches!(&app.items[0], OutputItem::ToolDispatch { calls } if calls[0].disclosure == expected)
            );
            assert_eq!(perf_counters().item_renders, 0);
            assert!(
                app.layout_cache
                    .next_tool_disclosure(&app.items, 0, "wrong", ToolDisclosureAction::Content)
                    .is_none()
            );
        }
    }
}
