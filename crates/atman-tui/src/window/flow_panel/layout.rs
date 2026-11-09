//! Prepared window chrome and viewport-only legacy document composition.

use std::{collections::HashSet, sync::Arc};

use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use super::{FlowPanelContent, panel_hit_regions};
use crate::{
    app::{OutputItem, OutputRevision},
    flow_view::OutputCapture,
    output::{LayoutCache, LayoutKey, LayoutRequest, RenderCtx as BodyContext},
    window::output_panel::preparation::{Key, Rendered, Source},
    wm::{
        PanelRenderCache,
        component::{HitRegion, RenderCtx},
    },
};

pub(super) fn header(
    item: &OutputItem,
    handle: &str,
    _expanded: &HashSet<String>,
    workflow_expanded: bool,
    presentation: &crate::workflow::Presentation,
    width: u16,
    hovered: Option<&str>,
) -> Option<Rendered> {
    let theme = crate::theme::theme();
    let mut lines = Vec::new();
    let (graph, running, stopped) = match item {
        OutputItem::SubAgentActivity {
            goal,
            model,
            status,
            iteration,
            workflow_graph,
            ..
        } => {
            let iteration = if status.is_running() {
                format!(" (iter {iteration})")
            } else {
                String::new()
            };
            lines.push(Line::styled(
                format!(" {} {goal}{iteration}", status.glyph()),
                Style::default().fg(theme.accent.into()),
            ));
            for (label, value) in [
                (" flow:   ", handle),
                (" model:  ", model.as_str()),
                (" status: ", status.label()),
            ] {
                lines.push(Line::from(vec![
                    Span::styled(label, Style::default().fg(theme.subtle_fg.into())),
                    Span::styled(
                        value.to_owned(),
                        Style::default().fg(theme.tinted_fg.into()),
                    ),
                ]));
            }
            lines.push(Line::default());
            (
                workflow_graph,
                status.is_running(),
                *status == crate::agent_presentation::AgentState::Stopped,
            )
        }
        OutputItem::WorkflowPanel {
            graph,
            ended_at,
            cancelled,
            ..
        } => (graph, ended_at.is_none(), *cancelled),
        _ => return None,
    };
    let offset = lines.len();
    let (graph_lines, mut regions, mut inspector) = crate::output::prepare_presented_workflow(
        graph,
        presentation,
        workflow_expanded,
        stopped,
        if running {
            crate::output::LAYOUT_ANIMATION_FRAME
        } else {
            0
        },
        width,
        crate::output::MAX_COLLAPSED_BODY_ROWS,
        0,
        hovered,
    );
    for region in &mut regions {
        region.start_row += offset as u32;
        region.end_row += offset as u32;
    }
    if let Some(inspector) = inspector.as_mut() {
        inspector.shift_rows(offset as u32);
    }
    lines.extend(graph_lines);
    if matches!(item, OutputItem::SubAgentActivity { .. }) {
        lines.push(Line::default());
    }
    let paint = if running {
        crate::output::workflow_dynamic_paint(graph, workflow_expanded, &lines, offset)
    } else {
        Default::default()
    };
    Some(Rendered {
        lines: lines.into(),
        regions: regions.into(),
        inspector: inspector.map(Arc::new),
        paint,
        animated: running,
    })
}

impl FlowPanelContent {
    pub(super) fn clear_layout_source(&mut self) {
        self.interaction.metric_motion = Default::default();
        self.interaction.inspector_scroll = Default::default();
        self.interaction.inspector_view = None;
        self.header_preparation.clear();
        self.prepared_header_key = None;
        self.header_rows = 0;
        self.work_anchor = None;
        self.read_anchor = None;
        self.copy_document = None;
        self.restoration
            .discard(std::mem::take(&mut self.document_layout));
        self.clear_copy_source();
    }

    pub(super) fn layout_animates(&self) -> bool {
        self.document_layout.preparing()
            || self.header_preparation.is_animating()
            || self.document.work_fold_animates()
    }

    pub(super) fn anchor_work(&mut self, path: &str) {
        if let Some(fold) = self
            .document
            .work_fold_projections(None)
            .iter()
            .find(|fold| format!("{}{}", crate::output::WORK_FOLD_REGION_PREFIX, fold.key) == path)
            && let Some(row) = self.document_layout.item_row_start(fold.start_index)
        {
            self.work_anchor = Some((
                fold.key,
                i64::from(self.header_rows) + i64::from(row) - i64::from(self.scroll),
            ));
        }
    }

    pub(super) fn render_layout(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ctx: &RenderCtx<'_>,
        source: OutputCapture,
        item_index: usize,
        revision: OutputRevision,
    ) -> Vec<HitRegion> {
        self.interaction.area = area;
        self.interaction.inspector_view = None;
        self.interaction.window = Some(ctx.window_id);
        if self
            .header_preparation
            .bind_scope(ctx.items.source_identity())
        {
            self.clear_layout_source();
        }
        let child = matches!(source.item(), Some(OutputItem::SubAgentActivity { .. }));
        let workflow_expanded = self.interaction.workflow_expanded.unwrap_or(true);
        let key = Key {
            revision: OutputRevision {
                interaction: self.interaction.revision,
                ..revision
            },
            window: ctx.window_id,
            width: area.width,
            theme: crate::theme::theme(),
            preferences: crate::render_preferences::RenderPreferences::current(),
        };
        if self.prepared_header_key.is_some_and(|previous| {
            previous.window != key.window || previous.revision.id != key.revision.id
        }) {
            self.clear_layout_source();
        } else if self.prepared_header_key.is_some_and(|previous| {
            previous.width != key.width
                || previous.theme != key.theme
                || previous.preferences != key.preferences
        }) {
            self.restoration.discard(self.render_cache.take());
            self.prepared_header_key = None;
        }
        let handle = self.handle.clone();
        if let Some(item) = source.item() {
            let (values, running) = crate::output::MetricSnapshot::item(item);
            self.interaction.metric_motion.observe(
                values,
                running,
                crate::render_preferences::RenderPreferences::current().metrics_animation,
                std::time::Instant::now(),
            );
        }
        let expanded = self.interaction.expanded_nodes.clone().unwrap_or_default();
        let presentation = self.interaction.presentation.clone();
        let hover = self
            .interaction
            .hovered
            .or(self.interaction.focused)
            .and_then(|index| self.interaction.hits.get(index))
            .and_then(|hit| match &hit.target {
                crate::wm::component::HitTarget::WorkflowNode(_, key) => Some(key.clone()),
                _ => None,
            });
        self.header_preparation
            .update_retaining(key, || Source::Render {
                surface: (!child).then_some("workflow-output"),
                operation: Box::new(move || {
                    header(
                        source.item()?,
                        &handle,
                        &expanded,
                        workflow_expanded,
                        &presentation,
                        key.width,
                        hover.as_deref(),
                    )
                }),
            });
        if let Some(ready) = self.header_preparation.ready.as_ref().filter(|ready| {
            ready.key.width == key.width
                && ready.key.theme == key.theme
                && ready.key.preferences == key.preferences
        }) && (self.prepared_header_key != Some(ready.key) || self.render_cache.is_none())
        {
            let cache = PanelRenderCache {
                #[cfg(test)]
                item_id: ready.key.revision.id,
                #[cfg(test)]
                content_revision: ready.key.revision.layout,
                #[cfg(test)]
                interaction_revision: ready.key.revision.interaction,
                #[cfg(test)]
                width: key.width,
                #[cfg(test)]
                workflow_expanded,
                lines: ready.lines.clone(),
                dynamic_paint: ready.paint.clone(),
                regions: ready.regions.clone(),
                document_regions: Arc::from([]),
            };
            self.restoration.discard(self.render_cache.replace(cache));
            self.prepared_header_key = Some(ready.key);
        }

        if self.read_anchor.is_none()
            && self.work_anchor.is_none()
            && self.scroll >= self.header_rows
        {
            self.read_anchor = self
                .document_layout
                .reading_anchor(self.scroll - self.header_rows);
        }
        let header_rows = self
            .render_cache
            .as_ref()
            .map_or(0, |cache| cache.lines.len().min(u32::MAX as usize) as u32);
        let body_offset = self.scroll.saturating_sub(header_rows);
        let body_top = header_rows
            .saturating_sub(self.scroll)
            .min(u32::from(area.height)) as u16;
        let body_height = area.height.saturating_sub(body_top);
        let folds = self.document.work_fold_projections(None);
        self.document_layout.enable_background();
        self.document_layout.set_work_folds(folds.clone());
        let body_context = BodyContext {
            expanded_tools: &self.interaction.expanded_groups,
            animation_frame: ctx.animation_frame,
            panel_width: area.width,
            hovered_thinking_idx: None,
            hovered_output_node: None,
        };
        let request = LayoutRequest {
            scroll_offset: body_offset,
            viewport_rows: u32::from(body_height),
            follow_tail_rows: None,
        };
        let layout_key = LayoutKey {
            width: area.width,
            theme: crate::theme::current_mode(),
        };
        let metrics =
            self.document_layout
                .update_dirty(layout_key, &self.document, &body_context, request);
        let settled =
            !self.document_layout.preparing() && self.document_layout.preparation_error().is_none();
        if let Some((fold_key, offset)) = self.work_anchor {
            if let Some(fold) = folds.iter().find(|fold| fold.key == fold_key)
                && let Some(row) = self.document_layout.item_row_start(fold.start_index)
            {
                self.scroll = (i64::from(header_rows) + i64::from(row) - offset)
                    .clamp(0, i64::from(u32::MAX)) as u32;
            } else {
                self.work_anchor = None;
            }
        } else if settled {
            if let Some(anchor) = self.read_anchor.take()
                && let Some(offset) = self.document_layout.anchored_offset(anchor)
            {
                self.scroll = header_rows.saturating_add(offset);
            }
            self.scroll = self.scroll.min(
                header_rows
                    .saturating_add(metrics.total_rows)
                    .saturating_sub(u32::from(area.height)),
            );
        }
        self.header_rows = header_rows;
        let body_offset = self.scroll.saturating_sub(header_rows);
        let body_top = header_rows
            .saturating_sub(self.scroll)
            .min(u32::from(area.height)) as u16;
        let body_area = Rect {
            y: area.y.saturating_add(body_top),
            height: area.height.saturating_sub(body_top),
            ..area
        };
        if body_offset != request.scroll_offset
            || u32::from(body_area.height) != request.viewport_rows
        {
            self.document_layout.update_dirty(
                layout_key,
                &self.document,
                &body_context,
                LayoutRequest {
                    scroll_offset: body_offset,
                    viewport_rows: u32::from(body_area.height),
                    ..request
                },
            );
        }
        if let Some(cache) = &self.render_cache {
            let start = (self.scroll as usize).min(cache.lines.len());
            let end = start
                .saturating_add(usize::from(area.height))
                .min(cache.lines.len());
            let mut lines = cache.lines[start..end].to_vec();
            crate::output::patch_animation_lines(
                &mut lines,
                &cache.dynamic_paint,
                ctx.animation_frame,
                start,
            );
            crate::output::patch_metric_lines(
                &mut lines,
                &cache.dynamic_paint,
                &self.interaction.metric_motion,
                start,
            );
            if let Some(inspector) = self
                .header_preparation
                .ready
                .as_ref()
                .filter(|ready| self.prepared_header_key == Some(ready.key))
                .and_then(|ready| ready.inspector.as_ref())
                && let Some(view) = inspector.viewport(
                    &mut self.interaction.inspector_scroll,
                    0,
                    0,
                    self.scroll,
                    u32::from(area.height),
                )
            {
                view.project(
                    &cache.lines,
                    &mut lines,
                    &mut Vec::new(),
                    self.scroll,
                    0,
                    None,
                );
                self.interaction.inspector_view = Some(view);
            }
            frame.render_widget(Paragraph::new(lines), area);
        }
        let visible = self.document_layout.visible_slice(
            body_offset,
            u32::from(body_area.height),
            ctx.animation_frame,
        );
        if self.render_cache.is_some() && child {
            frame.render_widget(Paragraph::new(visible.lines), body_area);
        }
        if let Some(cache) = self.render_cache.as_mut() {
            cache.document_regions = visible
                .regions
                .into_iter()
                .map(|mut region| {
                    region.start_row = region.start_row.saturating_add(header_rows);
                    region.end_row = region.end_row.saturating_add(header_rows);
                    region
                })
                .collect::<Vec<_>>()
                .into();
        }
        self.prepare_document_selection(area, child, &folds);
        self.render_cache.as_ref().map_or_else(Vec::new, |cache| {
            panel_hit_regions(
                cache,
                area,
                self.scroll,
                item_index,
                self.interaction.inspector_view.as_ref(),
            )
        })
    }

    fn prepare_document_selection(
        &mut self,
        area: Rect,
        child: bool,
        folds: &[crate::output::WorkFoldProjection],
    ) {
        if !child {
            self.selection_projection = self
                .header_preparation
                .ready
                .as_ref()
                .filter(|ready| self.prepared_header_key == Some(ready.key))
                .and_then(|ready| {
                    ready.text.as_ref().map(|text| {
                        text.visible_inspector(
                            area,
                            self.scroll,
                            self.interaction.inspector_view.as_ref(),
                        )
                    })
                });
            return;
        }
        let document_key = (
            self.document.identity(),
            self.document.revision_clock(),
            self.interaction.revision,
        );
        let lines = self.render_cache.as_ref().map(|cache| &cache.lines);
        let same_lines = lines.is_some_and(|lines| {
            self.copy_lines
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .is_some_and(|previous| Arc::ptr_eq(&previous, lines))
        });
        if self.copy_document.as_ref() != Some(&document_key) || !same_lines {
            self.copy_preparation.clear();
            self.copy_document = Some(document_key);
            self.copy_lines = lines.map(Arc::downgrade);
        }
        if self.document_layout.preparing()
            || self.header_preparation.is_pending()
            || self.restoration.is_pending()
            || folds.iter().any(|fold| fold.animating)
        {
            return;
        }
        let Some(cache) = self.render_cache.as_ref() else {
            return;
        };
        let Some(mut key) = self.prepared_header_key else {
            return;
        };
        key.revision.layout = self.document.revision_clock();
        let expanded = &self.interaction.expanded_groups;
        self.copy_preparation.update(key, || {
            let captured = self.document.capture();
            let folds = folds.to_vec();
            let expanded = expanded.clone();
            let header_lines = cache.lines.clone();
            let paint = cache.dynamic_paint.clone();
            Source::Render {
                surface: Some("working-output"),
                operation: Box::new(move || {
                    let _measure = crate::render_preferences::RenderPass::Measure.enter();
                    let mut layout = LayoutCache::default();
                    layout.set_work_folds(folds);
                    let context = BodyContext {
                        expanded_tools: &expanded,
                        panel_width: key.width,
                        ..BodyContext::empty()
                    };
                    let metrics = layout.update_dirty(
                        LayoutKey {
                            width: key.width,
                            theme: crate::theme::current_mode(),
                        },
                        captured.document(),
                        &context,
                        LayoutRequest {
                            scroll_offset: 0,
                            viewport_rows: u32::MAX,
                            follow_tail_rows: None,
                        },
                    );
                    let mut lines = header_lines.to_vec();
                    lines.extend(layout.visible_slice(0, metrics.total_rows, 0).lines);
                    Some(Rendered {
                        lines: lines.into(),
                        paint,
                        regions: Arc::from([]),
                        inspector: None,
                        animated: false,
                    })
                }),
            }
        });
        self.selection_projection = self
            .copy_preparation
            .ready
            .as_ref()
            .filter(|ready| ready.key == key)
            .and_then(|ready| {
                ready.text.as_ref().map(|text| {
                    text.visible_inspector(
                        area,
                        self.scroll,
                        self.interaction.inspector_view.as_ref(),
                    )
                })
            });
    }
}
