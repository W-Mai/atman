use crate::UiState;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::input::{InputEditor, InputFooter, input_paragraph};
use crate::{approval_bar, completion, layout, output, sidebar, status, submission_queue};

fn render_selection_highlight(
    frame: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    scroll_offset: u32,
    projection: &crate::selection::VisibleSelectionProjection,
    selection: &crate::selection::SelectionState,
) {
    let bg = crate::theme::theme().accent.into_inner();
    let cells = frame.buffer_mut();
    for visual in projection.visual_points() {
        let row = visual.row.saturating_sub(scroll_offset);
        if row >= u32::from(area.height)
            || !crate::selection::selection_contains(selection, &visual.point)
        {
            continue;
        }
        for col in visual.col..visual.col.saturating_add(visual.cell_width) {
            if col < area.width {
                cells[(
                    area.x.saturating_add(col),
                    area.y.saturating_add(row as u16),
                )]
                    .set_bg(bg);
            }
        }
    }
}

fn render_sidebar_selection_highlight(
    frame: &mut ratatui::Frame,
    projection: &crate::sidebar_selection::SidebarSelectionProjection,
    selection: &crate::selection::SelectionState,
) {
    let crate::selection::SelectionDomain::Sidebar { section } = &selection.anchor.domain else {
        return;
    };
    let section = match section {
        crate::selection::SidebarSection::Goal => crate::sidebar_selection::SidebarSection::Goal,
        crate::selection::SidebarSection::Plan => crate::sidebar_selection::SidebarSection::Plan,
        crate::selection::SidebarSection::Todo => crate::sidebar_selection::SidebarSection::Todo,
        crate::selection::SidebarSection::Context => {
            crate::sidebar_selection::SidebarSection::Context
        }
        crate::selection::SidebarSection::Mcp => crate::sidebar_selection::SidebarSection::Mcp,
    };
    let Some(surface) = projection
        .surfaces
        .iter()
        .find(|surface| surface.section == section)
    else {
        return;
    };
    let bg = crate::theme::theme().accent.into_inner();
    let cells = frame.buffer_mut();
    for atom in &surface.atoms {
        for col in atom.cols.clone() {
            let Some(grapheme) = surface.point_at(atom.rect.y, col) else {
                continue;
            };
            let point = crate::selection::SemanticPoint {
                domain: selection.anchor.domain.clone(),
                ordinal: 0,
                grapheme,
                affinity: crate::selection::Affinity::Before,
            };
            if crate::selection::selection_contains(selection, &point) {
                cells[(col, atom.rect.y)].set_bg(bg);
            }
        }
    }
}

pub(crate) trait ModeColorExt {
    fn ratatui(self) -> Color;
}

impl ModeColorExt for atman_runtime::trust::ModeColor {
    fn ratatui(self) -> Color {
        match self {
            atman_runtime::trust::ModeColor::Cyan => Color::Rgb(40, 180, 180),
            atman_runtime::trust::ModeColor::Green => Color::Rgb(70, 175, 70),
            atman_runtime::trust::ModeColor::Yellow => Color::Rgb(190, 175, 55),
            atman_runtime::trust::ModeColor::Orange => Color::Rgb(220, 85, 40),
            atman_runtime::trust::ModeColor::Red => Color::Rgb(190, 65, 65),
        }
    }
}

pub(crate) fn rect_union(
    a: ratatui::layout::Rect,
    b: ratatui::layout::Rect,
) -> ratatui::layout::Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.width).max(b.x + b.width);
    let bottom = (a.y + a.height).max(b.y + b.height);
    ratatui::layout::Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}

pub(crate) fn rect_contains(rect: ratatui::layout::Rect, col: u16, row: u16) -> bool {
    col >= rect.x
        && col < rect.x.saturating_add(rect.width)
        && row >= rect.y
        && row < rect.y.saturating_add(rect.height)
}

// Startup input eases from the overlay's centered slot to the normal
// bottom position. 300 ms sits inside the 200–400 ms band that feels
// like a real transition rather than a snap or a lag.
const STARTUP_SLIDE_MS: u128 = 300;
// Animation frame cadence while a slide is in flight. 60 fps so the
// panel's x / y / width interpolation looks continuous instead of
// two-or-three discrete jumps.
pub(crate) const ANIMATION_TICK_MS: u64 = 16;

// ease-out-quad — motion is immediately visible from the first frame
// and gently decelerates into the end. ease-in-out was the wrong pick:
// its slow start hides the animation in the crucial "did anything just
// happen?" first 100 ms.
pub(crate) fn ease_out(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    1.0 - (1.0 - t) * (1.0 - t)
}

// Replace wide-glyph halves straddling a floating widget's edges with
// spaces so CJK / emoji from lower layers can't bleed through the
// overlay's border. Call before each Clear + render pass on a modal.
pub(crate) fn sanitize_widget_edges(f: &mut ratatui::Frame, area: ratatui::layout::Rect) {
    use ratatui::buffer::CellDiffOption;
    if area.width == 0 || area.height == 0 {
        return;
    }
    let buf = f.buffer_mut();
    let buf_area = *buf.area();
    let inside_left = area.x;
    let inside_right = area.x + area.width - 1;
    let outside_left = area.x.checked_sub(1);
    let outside_right = if area.x + area.width < buf_area.x + buf_area.width {
        Some(area.x + area.width)
    } else {
        None
    };
    let clear_wide = |cell: &mut ratatui::buffer::Cell| {
        cell.set_symbol(" ");
        cell.set_diff_option(CellDiffOption::None);
    };
    let clear_wide_at = |buf: &mut ratatui::buffer::Buffer, x: u16, y: u16| {
        if x < buf_area.x || x >= buf_area.x + buf_area.width {
            return;
        }
        let symbol = buf[(x, y)].symbol().to_string();
        if crate::width::width(&symbol) > 1 {
            let bg = buf[(x, y)].bg;
            clear_wide(&mut buf[(x, y)]);
            if x + 1 < buf_area.x + buf_area.width {
                if matches!(buf[(x + 1, y)].bg, ratatui::style::Color::Reset) {
                    buf[(x + 1, y)].bg = bg;
                }
                clear_wide(&mut buf[(x + 1, y)]);
            }
        } else if crate::width::width(&symbol) == 1 && symbol.trim().is_empty() && x > buf_area.x {
            if crate::width::width(buf[(x - 1, y)].symbol()) <= 1 {
                return;
            }
            let bg = buf[(x - 1, y)].bg;
            if matches!(buf[(x, y)].bg, ratatui::style::Color::Reset) {
                buf[(x, y)].bg = bg;
            }
            clear_wide(&mut buf[(x, y)]);
            if x > buf_area.x {
                clear_wide(&mut buf[(x - 1, y)]);
            }
        }
    };
    for y in area.y..area.y + area.height {
        if y < buf_area.y || y >= buf_area.y + buf_area.height {
            continue;
        }
        if let Some(ox) = outside_left {
            clear_wide_at(buf, ox, y);
        }
        clear_wide_at(buf, inside_left, y);
        if inside_right != inside_left {
            clear_wide_at(buf, inside_right, y);
        }
        if let Some(rx) = outside_right {
            clear_wide_at(buf, rx, y);
        }
    }
}

pub(crate) fn render_frame(f: &mut ratatui::Frame, ui: &mut UiState, editor: &InputEditor) {
    let app = &mut ui.app;
    let area = f.area();
    if app.last_full_rect != Some(area) {
        let responsive_narrow = area.width
            < layout::SIDEBAR_MIN_TOTAL_WIDTH.max(crate::task_panel::TASK_PANEL_WIDTH + 40);
        app.sidebar_upper_runtime_collapsed = responsive_narrow;
        app.sidebar_lower_runtime_collapsed = responsive_narrow;
        app.task_panel_runtime_collapsed = responsive_narrow;
        app.last_full_rect = Some(area);
    }
    if area.width < 40 || area.height < 8 {
        let msg = Paragraph::new(Line::from("terminal too small (need 40×8)"))
            .style(Style::default().fg(crate::theme::theme().warn.into()))
            .alignment(Alignment::Center);
        f.render_widget(msg, area);
        return;
    }
    let startup_active = matches!(
        app.items.first(),
        Some(crate::app::OutputItem::StartupCard { .. })
    );
    let intro_progress = app
        .startup_intro
        .as_ref()
        .map(|i| {
            (i.started_at.elapsed().as_millis().min(STARTUP_SLIDE_MS) as f32)
                / STARTUP_SLIDE_MS as f32
        })
        .unwrap_or(1.0);
    let intro_active = app.startup_intro.is_some() && intro_progress < 1.0;
    let show_sidebar = !startup_active && !intro_active;
    let sidebar_effective_collapsed = app.sidebar_collapsed;
    let status_height: u16 = 1;
    let standalone_canonical = app
        .pending_permissions
        .keys()
        .filter(|id| !app.grouped_permission_request_ids.contains(*id))
        .count();
    let pending_count = app.pending_permissions.len();
    let visible_rows = standalone_canonical;
    let approvals_rows: u16 = if pending_count == 0 {
        0
    } else {
        let items = visible_rows.min(9) as u16;
        let overflow = if visible_rows > 9 { 1 } else { 0 };
        let groups = app.pending_permission_groups.len() as u16;
        let expanded_members = app
            .pending_permission_groups
            .values()
            .filter(|group| group.expanded)
            .map(|group| group.payload.request_ids.len() as u16)
            .sum::<u16>();
        items + overflow + groups + expanded_members + 2
    };
    let injection_rows: u16 = if app.pending_injections.is_empty() {
        0
    } else {
        // title + N items + 2 for block borders
        (app.pending_injections.len() as u16).min(5) + 3
    };
    let submission_rows: u16 = if app.queued_submissions.is_empty() {
        0
    } else {
        (app.queued_submissions.len() as u16).min(5) + 2
    };
    let attachment_rows: u16 = if editor.pending_images().is_empty() {
        0
    } else {
        let visible = editor.pending_images().len().min(4) as u16;
        let overflow = u16::from(editor.pending_images().len() > 4);
        visible + overflow + 2
    };
    let quote_rows: u16 = app.pending_quote.as_deref().map_or(0, |text| {
        text.lines().count().min(6) as u16 + 2 + u16::from(text.lines().count() > 6)
    });
    let l = layout::compute_ex(area, status_height);
    let sidebar_rect =
        layout::compute_sidebar_rect(l.transcript, show_sidebar, sidebar_effective_collapsed);
    let transcript_content = layout::compute_content_rect(l.transcript);
    let content_w = layout::input_content_width(l.transcript.width);
    let total_input_lines = crate::input::visual_line_count(editor.buf(), content_w) as u32;
    let input_buf_lines = total_input_lines.min(12);
    let bottom_rect =
        layout::compute_input_rect(l.transcript, input_buf_lines.min(u16::MAX as u32) as u16);
    let startup_slot = if startup_active {
        let recent = match app.items.first() {
            Some(crate::app::OutputItem::StartupCard { recent, .. }) => recent.clone(),
            _ => Vec::new(),
        };
        Some(output::compute_startup_overlay(l.transcript, &recent).input_slot)
    } else {
        None
    };
    let intro_slot = if intro_active {
        app.startup_intro
            .as_ref()
            .map(|i| output::compute_startup_overlay(l.transcript, &i.recent).input_slot)
    } else {
        None
    };
    let intro_overlay_area = if intro_active {
        app.startup_intro
            .as_ref()
            .map(|i| output::compute_startup_overlay(l.transcript, &i.recent).area)
    } else {
        None
    };
    let input_rect = if let Some(slot) = startup_slot {
        slot
    } else if let Some(slot) = intro_slot {
        let eased = ease_out(intro_progress);
        let mix = |a: u16, b: u16| ((a as f32) + (b as f32 - a as f32) * eased).round() as u16;
        ratatui::layout::Rect {
            x: mix(slot.x, bottom_rect.x),
            y: mix(slot.y, bottom_rect.y),
            width: mix(slot.width, bottom_rect.width),
            height: mix(slot.height, bottom_rect.height),
        }
    } else {
        bottom_rect
    };
    let content_w = (input_rect.width.saturating_sub(layout::INPUT_H_OVERHEAD)) as usize;
    let cursor_row =
        crate::input::wrapped_cursor_row(editor.buf(), editor.cursor(), content_w) as u32;
    let visible_rows = input_buf_lines.max(3);
    let scroll_row = cursor_row.saturating_sub(visible_rows.saturating_sub(1));
    let attachments_rect =
        layout::compute_stacked_rect(l.transcript, input_rect, None, attachment_rows);
    let quote_rect =
        layout::compute_stacked_rect(l.transcript, input_rect, attachments_rect, quote_rows);
    let approvals_rect = layout::compute_stacked_rect(
        l.transcript,
        input_rect,
        quote_rect.or(attachments_rect),
        approvals_rows,
    );
    let submission_queue_rect = layout::compute_stacked_rect(
        l.transcript,
        input_rect,
        approvals_rect.or(quote_rect).or(attachments_rect),
        submission_rows,
    );
    let injections_rect = (injection_rows > 0)
        .then(|| {
            layout::compute_injection_rect(
                l.transcript,
                input_rect,
                submission_queue_rect
                    .or(approvals_rect)
                    .or(quote_rect)
                    .or(attachments_rect),
                injection_rows,
            )
        })
        .flatten();
    app.quote_rect = quote_rect;
    if app.pending_quote.is_none() {
        app.quote_close_hovered = false;
    }
    app.submission_queue_rect = submission_queue_rect;
    app.input_rect = Some(input_rect);
    f.render_widget(
        status::render_bar(status::StatusInputs {
            session_id: &app.session_id,
            session_name: app.session_name.as_deref(),
            goal: app.goal.as_deref(),
            streaming: app.streaming,
            waiting_for_llm: app.waiting_for_llm,
            status_notes: &app.status_notes,
            activity: Some(&app.session_activity),
            pending_permissions: pending_count,
        }),
        l.status,
    );
    let transcript_area = transcript_content;
    app.last_transcript_rect = Some(transcript_area);
    let document_visible_rows = layout::document_visible_rows(transcript_area.height);
    let input_overlay_rows = layout::floating_overlay_rows(
        transcript_area,
        input_rect,
        &[
            attachments_rect,
            quote_rect,
            approvals_rect,
            submission_queue_rect,
            injections_rect,
        ],
    );
    let effective_viewport = document_visible_rows.max(1);
    if startup_active {
        if let Some(crate::app::OutputItem::StartupCard { version, recent }) = app.items.first() {
            f.render_widget(ratatui::widgets::Clear, l.transcript);
            let startup_layout = output::render_startup_overlay(
                f,
                output::StartupOverlayRender {
                    area: l.transcript,
                    version,
                    recent,
                    dim: false,
                    reveal_count: recent.len(),
                    focus: app.startup_focus,
                    selected: app.startup_selected_session,
                    hovered: app.startup_hovered_session,
                    projects_hovered: app.startup_projects_hovered,
                },
            );
            app.startup_container_rect = startup_layout.recent_container;
            app.startup_projects_rect = startup_layout.all_projects_rect;
            app.startup_session_rects = startup_layout.session_rects;
            if app.startup_session_rects.is_empty() {
                app.startup_focus = crate::app::StartupFocus::Input;
                app.startup_selected_session = 0;
                app.startup_hovered_session = None;
                app.startup_last_click = None;
            } else {
                app.startup_selected_session = app
                    .startup_selected_session
                    .min(app.startup_session_rects.len() - 1);
                if app
                    .startup_hovered_session
                    .is_some_and(|index| index >= app.startup_session_rects.len())
                {
                    app.startup_hovered_session = None;
                }
            }
        }
        app.resolve_scroll(0, effective_viewport, 0, app.items.len());
        app.last_item_ranges.clear();
    } else {
        app.startup_focus = crate::app::StartupFocus::Input;
        app.startup_selected_session = 0;
        app.startup_hovered_session = None;
        app.startup_container_rect = None;
        app.startup_projects_rect = None;
        app.startup_projects_hovered = false;
        app.startup_session_rects.clear();
        app.startup_last_click = None;
        if app.items.is_empty() {
            app.resolve_scroll(0, effective_viewport, 0, app.items.len());
            app.last_item_ranges.clear();
            // Clear outside content padding so the sliding startup overlay cannot leak through.
            f.render_widget(ratatui::widgets::Clear, l.transcript);
            f.render_widget(output::empty_hint(), transcript_area);
        } else {
            let messages_lock = app.session.as_ref().map(|s| s.messages_handle());
            let messages_guard = messages_lock.as_ref().and_then(|h| h.lock().ok());
            let empty_messages: Vec<atman_runtime::message::Message> = Vec::new();
            let messages: &[atman_runtime::message::Message] =
                messages_guard.as_deref().unwrap_or(&empty_messages);
            let work_folds = app.work_fold_projections();
            let animating_fold = work_folds
                .iter()
                .rev()
                .find(|fold| fold.animating)
                .map(|fold| (fold.key, fold.start_index));
            if animating_fold.is_none() {
                app.clear_work_fold_scroll_anchor();
            }
            let ctx = output::RenderCtx {
                expanded_tools: &app.expanded_tools,
                messages,
                animation_frame: app.animation_frame,
                panel_width: transcript_area.width,
                hovered_thinking_idx: app.hovered_thinking_idx,
                hovered_output_node: app.hovered_output_node.as_ref(),
            };
            let cache_key = output::LayoutKey {
                width: transcript_area.width,
                theme: crate::theme::current_mode(),
            };
            let mut cache = std::mem::take(&mut app.layout_cache);
            let old_total_rows = cache.total_rows();
            let folding_above_viewport = !app.follow_tail
                && work_folds.iter().any(|fold| {
                    cache
                        .item_row_end(fold.end_index)
                        .is_some_and(|end| end <= app.scroll_offset)
                });
            cache.set_work_folds(work_folds);
            let has_work_anchor =
                animating_fold.is_some_and(|(key, _)| app.work_fold_scroll_anchor(key).is_some());
            let follow_tail_rows = (app.follow_tail && !has_work_anchor).then(|| {
                document_visible_rows
                    .saturating_sub(input_overlay_rows)
                    .saturating_sub(crate::layout::INPUT_TOP_GAP as u32)
                    .max(1)
            });
            let request = output::LayoutRequest {
                scroll_offset: app.scroll_offset,
                viewport_rows: effective_viewport,
                follow_tail_rows,
            };
            let mut metrics = cache.update_dirty(cache_key, &app.items, &ctx, request);
            if folding_above_viewport && metrics.total_rows != old_total_rows {
                let anchored_offset = if metrics.total_rows > old_total_rows {
                    app.scroll_offset
                        .saturating_add(metrics.total_rows - old_total_rows)
                } else {
                    app.scroll_offset
                        .saturating_sub(old_total_rows - metrics.total_rows)
                };
                metrics = cache.update_dirty(
                    cache_key,
                    &app.items,
                    &ctx,
                    output::LayoutRequest {
                        scroll_offset: anchored_offset,
                        ..request
                    },
                );
            }
            if let Some((key, start_index)) = animating_fold
                && let Some(header_row) = cache.item_row_start(start_index)
            {
                if let Some(screen_row) = app.work_fold_scroll_anchor(key) {
                    let anchored_offset = header_row.saturating_sub(screen_row);
                    if anchored_offset != metrics.scroll_offset {
                        metrics = cache.update_dirty(
                            cache_key,
                            &app.items,
                            &ctx,
                            output::LayoutRequest {
                                scroll_offset: anchored_offset,
                                follow_tail_rows: None,
                                ..request
                            },
                        );
                    }
                } else if header_row >= metrics.scroll_offset
                    && header_row < metrics.scroll_offset.saturating_add(effective_viewport)
                {
                    app.set_work_fold_scroll_anchor(key, header_row - metrics.scroll_offset);
                }
            }
            let visible = cache.visible_slice(
                metrics.scroll_offset,
                effective_viewport,
                app.animation_frame,
            );
            let lines = visible.lines;
            let total_rows = metrics.total_rows;
            app.last_item_ranges = visible.ranges;
            app.last_node_regions = visible.regions;
            app.last_selection_projection = visible.selection;
            app.layout_cache = cache;
            let preserve_work_anchor =
                animating_fold.is_some_and(|(key, _)| app.work_fold_scroll_anchor(key).is_some());
            if preserve_work_anchor {
                let follow_tail = app.follow_tail;
                app.follow_tail = false;
                app.scroll_offset = metrics.scroll_offset;
                app.resolve_scroll(
                    total_rows,
                    document_visible_rows,
                    input_overlay_rows,
                    app.items.len(),
                );
                app.follow_tail = follow_tail;
            } else {
                app.resolve_scroll(
                    total_rows,
                    document_visible_rows,
                    input_overlay_rows,
                    app.items.len(),
                );
            }
            crate::event_loop::resolve_transcript_selection_after_render(app);
            let paragraph = ratatui::widgets::Paragraph::new(lines).scroll((0, 0));
            f.render_widget(paragraph, transcript_area);
            if let Some(selection) = app.selection.as_ref() {
                render_selection_highlight(
                    f,
                    transcript_area,
                    metrics.scroll_offset,
                    &app.last_selection_projection,
                    selection,
                );
            }
        }
    }
    if let Some(area) = sidebar_rect {
        let project_root = app
            .session
            .as_ref()
            .and_then(|s| s.meta())
            .and_then(|m| m.project_root)
            .map(|p| p.display().to_string());
        let goal_scroll = app.goal_scroll;
        let plans_scroll = app.plans_scroll;
        let todos_scroll = app.todos_scroll;
        let sr = sidebar::render(
            f,
            area,
            sidebar::SidebarInputs {
                goal: app.goal.as_deref(),
                context: &app.context,
                attach_count: app.attach_count,
                session_id: &app.session_id,
                session_dir: &app.session_dir,
                project_root: project_root.as_deref(),
                app_version: env!("CARGO_PKG_VERSION"),
                latest_release: app.latest_release.as_deref(),
                streaming: app.streaming,
                todos: &app.todos,
                plans: &app.plans,
                goal_scroll,
                plans_scroll,
                todos_scroll,
                goal_collapsed: app.goal_collapsed,
                plan_collapsed: app.plan_collapsed,
                todo_collapsed: app.todo_collapsed,
                context_collapsed: app.context_collapsed,
                meta_collapsed: app.meta_collapsed,
                mcp_collapsed: app.mcp_collapsed,
                sidebar_collapsed: sidebar_effective_collapsed,
                upper_collapsed: app.sidebar_upper_collapsed || app.sidebar_upper_runtime_collapsed,
                lower_collapsed: app.sidebar_lower_collapsed || app.sidebar_lower_runtime_collapsed,
                animation_frame: app.animation_frame,
                hovered_row: app.hovered_sidebar_row.as_deref(),
                hovered_hamburger: app.hovered_sidebar_hamburger,
                hovered_lower: app.hovered_sidebar_lower,
                hovered_more: app.hovered_sidebar_more,
                on_goal_scroll: &|_c| {},
                on_plans_scroll: &|_c| {},
                on_todos_scroll: &|_c| {},
            },
        );
        app.last_sidebar_rect = Some(area);
        app.last_goal_rect = sr.goal_rect;
        app.last_plan_rect = sr.plan_rect;
        app.last_todo_rect = sr.todo_rect;
        app.last_goal_hdr_rect = sr.goal_hdr_rect;
        app.last_plan_hdr_rect = sr.plan_hdr_rect;
        app.last_todo_hdr_rect = sr.todo_hdr_rect;
        app.last_ctx_hdr_rect = sr.ctx_hdr_rect;
        app.last_meta_hdr_rect = sr.meta_hdr_rect;
        app.last_mcp_hdr_rect = sr.mcp_hdr_rect;
        app.last_collapse_btn_rect = sr.collapse_btn_rect;
        app.last_expand_btn_rect = sr.expand_btn_rect;
        app.last_upper_title_rect = sr.upper_title_rect;
        app.last_lower_title_rect = sr.lower_title_rect;
        app.last_sidebar_more_rect = sr.mcp_more_rect;
        app.last_sidebar_strip_rects = sr.strip_rects;
        app.last_sidebar_selection_projection = sr.selection;
        if let Some(selection) = app.selection.as_ref() {
            render_sidebar_selection_highlight(
                f,
                &app.last_sidebar_selection_projection,
                selection,
            );
        }
    }
    // ── Sidebar popup (Plan/Todo item full text) ──
    if let Some(kind) = app.sidebar_popup {
        if let Some(sidebar_rect) = app.last_sidebar_rect {
            let popup_rect =
                crate::sidebar::render_sidebar_popup(f, sidebar_rect, kind, &app.plans, &app.todos);
            app.last_sidebar_popup_rect = Some(popup_rect);
        }
    } else {
        app.last_sidebar_popup_rect = None;
    }
    if let Some(tp_area) = crate::task_panel::compute_task_panel_rect(
        l.transcript,
        show_sidebar,
        app.task_panel_collapsed || app.task_panel_runtime_collapsed,
    ) {
        let hover = crate::task_panel::TaskPanelHover {
            hovered_task_id: app.hovered_task_id.clone(),
            hovered_kill_id: app.hovered_kill_id.clone(),
            hovered_insert_handle: app.hovered_insert_handle.clone(),
            hovered_activity: app.hovered_activity.clone(),
            hovered_history_btn: app.hovered_history_btn,
            hovered_hamburger: app.hovered_hamburger,
            kill_armed_id: app.kill_armed_id.clone(),
            kill_armed_expired: app.kill_arm_expired(),
            expanded_tasks: app.expanded_tasks.clone(),
        };
        let watch_hub = app.session.as_ref().map(|s| &*s.watch_hub);
        let hitmap = crate::task_panel::render(
            f,
            tp_area,
            &app.task_snapshots,
            &app.activity_nodes,
            &app.items,
            &app.handle_index,
            &app.detached_task_details,
            app.task_panel_collapsed,
            &app.task_panel_collapsed_groups,
            &hover,
            watch_hub,
        );
        app.last_task_panel_rect = Some(tp_area);
        app.last_task_panel_hitmap = hitmap;
    } else {
        app.last_task_panel_rect = None;
        app.last_task_panel_hitmap = crate::task_panel::TaskPanelHitMap::default();
    }
    if intro_active && let Some(intro) = app.startup_intro.as_ref() {
        output::render_startup_intro_fade(
            f,
            l.transcript,
            &intro.version,
            &intro.recent,
            intro_progress,
        );
    }
    if let Some(area) = attachments_rect {
        sanitize_widget_edges(f, area);
        f.render_widget(ratatui::widgets::Clear, area);
        let theme = crate::theme::theme();
        let mut lines: Vec<ratatui::text::Line<'_>> = editor
            .pending_images()
            .iter()
            .take(4)
            .map(|image| {
                ratatui::text::Line::from(vec![
                    ratatui::text::Span::styled(
                        format!(" {} ", image.marker),
                        ratatui::style::Style::default().fg(theme.accent.into()),
                    ),
                    ratatui::text::Span::raw(crate::width::truncate(
                        &image.name,
                        area.width.saturating_sub(16) as usize,
                    )),
                ])
            })
            .collect();
        if editor.pending_images().len() > 4 {
            lines.push(ratatui::text::Line::from(ratatui::text::Span::styled(
                format!(" +{} more", editor.pending_images().len() - 4),
                ratatui::style::Style::default().fg(theme.subtle_fg.into()),
            )));
        }
        let block = ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(ratatui::style::Style::default().fg(theme.accent.into()))
            .title(format!(" images · {} ", editor.pending_images().len()));
        f.render_widget(ratatui::widgets::Paragraph::new(lines).block(block), area);
    }
    if let (Some(area), Some(quote)) = (quote_rect, app.pending_quote.as_deref()) {
        sanitize_widget_edges(f, area);
        f.render_widget(ratatui::widgets::Clear, area);
        let theme = crate::theme::theme();
        let visible = area.height.saturating_sub(2) as usize;
        let mut lines = quote
            .lines()
            .take(visible.min(6))
            .map(|line| {
                ratatui::text::Line::from(ratatui::text::Span::styled(
                    crate::width::truncate(line, area.width.saturating_sub(4) as usize),
                    ratatui::style::Style::default().fg(theme.subtle_fg.into()),
                ))
            })
            .collect::<Vec<_>>();
        let total = quote.lines().count();
        if total > lines.len() {
            lines.push(ratatui::text::Line::from(ratatui::text::Span::styled(
                format!(" +{} more", total - lines.len()),
                ratatui::style::Style::default().fg(theme.subtle_fg.into()),
            )));
        }
        let block = ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(ratatui::style::Style::default().fg(theme.accent.into()))
            .title(format!(" quote · {total} lines · Alt+Del "));
        f.render_widget(ratatui::widgets::Paragraph::new(lines).block(block), area);
    }
    if let Some(area) = approvals_rect {
        sanitize_widget_edges(f, area);
        f.render_widget(ratatui::widgets::Clear, area);
        app.approval_hitmap = approval_bar::render(
            f,
            area,
            &app.pending_permissions,
            &app.pending_permission_groups,
            &app.grouped_permission_request_ids,
            app.selected_permission_group.as_ref(),
        );
    } else {
        app.approval_hitmap = approval_bar::ApprovalHitMap::default();
    }
    if let Some(area) = submission_queue_rect {
        sanitize_widget_edges(f, area);
        f.render_widget(ratatui::widgets::Clear, area);
        app.submission_queue_hitmap = submission_queue::render(
            f,
            area,
            &app.queued_submissions,
            app.selected_submission,
            app.submission_focus,
            app.hovered_submission,
            app.queued_submission_edit.as_ref(),
        );
    } else {
        app.submission_queue_hitmap = submission_queue::QueueHitMap::default();
    }
    // Render injection queue above approvals bar / input box.
    if let Some(area) = injections_rect {
        sanitize_widget_edges(f, area);
        f.render_widget(ratatui::widgets::Clear, area);
        let lines = output::render_injection_queue(&app.pending_injections, area.width);
        let block = ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(ratatui::style::Style::default().fg(crate::theme::theme().warn.into()));
        let para = ratatui::widgets::Paragraph::new(lines).block(block);
        f.render_widget(para, area);
    }
    // Wipe splash overlay ∪ docked rect for the entire lifetime of the intro,
    // including the very last frame where progress hits 1.0 and the banner /
    // sessions stop being drawn but still linger on screen from the frame before.
    let clear_target = if app.startup_intro.is_some() {
        if let Some(overlay) = intro_overlay_area {
            rect_union(overlay, bottom_rect)
        } else {
            input_rect
        }
    } else {
        input_rect
    };
    sanitize_widget_edges(f, clear_target);
    f.render_widget(ratatui::widgets::Clear, clear_target);
    let target_border = if app.streaming {
        crate::theme::theme().subtle_fg.into()
    } else {
        app.trust.display().color.ratatui()
    };
    if app.streaming != app.was_streaming {
        app.was_streaming = app.streaming;
        app.border_fade_at = Some(std::time::Instant::now());
    }
    let border_color = if let Some(start) = app.border_fade_at {
        let elapsed = start.elapsed().as_secs_f64();
        let prev = if app.streaming {
            app.trust.display().color.ratatui()
        } else {
            crate::theme::theme().subtle_fg.into()
        };
        if elapsed >= 0.35 {
            app.border_fade_at = None;
            target_border
        } else {
            lerp_rgb(prev, target_border, (elapsed / 0.35).clamp(0.0, 1.0))
        }
    } else {
        target_border
    };
    let reasoning_badge = app.effective_input_reasoning_badge();
    f.render_widget(
        input_paragraph(
            editor.buf(),
            border_color,
            app.pending_below_rows().min(u16::MAX as u32) as u16,
            scroll_row.min(u16::MAX as u32) as u16,
            &app.trust,
            reasoning_badge.as_deref(),
            InputFooter {
                queued_count: app.queued_submissions.len(),
                pending_quote_lines: app
                    .pending_quote
                    .as_ref()
                    .filter(|_| quote_rect.is_none())
                    .map(|quote| quote.lines().count()),
            },
        ),
        input_rect,
    );
    if app.pending_quote.is_some()
        && let Some(close) =
            crate::selection_menu::quote_close_rect(quote_rect.unwrap_or(input_rect))
    {
        f.render_widget(
            ratatui::widgets::Paragraph::new("[x]").style(
                ratatui::style::Style::default()
                    .fg(crate::theme::theme().subtle_fg.into())
                    .bg(if app.quote_close_hovered {
                        crate::theme::theme().work_hover_bg.into()
                    } else {
                        ratatui::style::Color::Reset
                    }),
            ),
            close,
        );
    }
    f.render_widget(
        ratatui::widgets::Paragraph::new(ratatui::text::Line::from(ratatui::text::Span::styled(
            "❯",
            if app.streaming {
                ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::DIM)
            } else {
                ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::BOLD)
            },
        ))),
        ratatui::layout::Rect {
            x: input_rect.x,
            y: input_rect.y + 1,
            width: 2,
            height: 1,
        },
    );
    let raw_row = crate::input::wrapped_cursor_row(editor.buf(), editor.cursor(), content_w) as u16;
    let raw_col = crate::input::wrapped_cursor_col(editor.buf(), editor.cursor(), content_w) as u16;
    if app.submission_focus {
        if let (Some(_), Some((x, y))) = (
            app.queued_submission_edit.as_ref(),
            app.submission_queue_hitmap.edit_origin,
        ) {
            f.set_cursor_position((x, y));
        }
    } else if !intro_active
        && (!startup_active || app.startup_focus == crate::app::StartupFocus::Input)
        && !ui.wm.modals.onboarding_open
        && !ui.wm.modals.provider_manager.open
        && ui.wm.focused_id().is_none()
    {
        let inner_x = input_rect.x.saturating_add(layout::INPUT_LEFT);
        let inner_y = input_rect.y.saturating_add(1);
        let mut placed = false;
        if raw_row as u32 >= scroll_row {
            let cy = inner_y + (raw_row as u32 - scroll_row) as u16;
            let cx = inner_x + raw_col;
            if cy < input_rect.y + input_rect.height.saturating_sub(1)
                && cx < input_rect.x + input_rect.width.saturating_sub(1)
            {
                f.set_cursor_position((cx, cy));
                placed = true;
            }
        }
        if !placed {
            // Fall back to the ❯ prompt only when the editor is empty
            // (e.g. just after submit).  When the user is typing the
            // primary cursor-position logic works and must win.
            if editor.buf().is_empty() {
                f.set_cursor_position((input_rect.x + layout::INPUT_LEFT, input_rect.y + 1));
            }
        }
    }
    if app.popup.is_open() {
        completion::render_popup(f, input_rect, &app.popup);
    }
    if startup_active
        && app.startup_container_rect.is_none()
        && !ui.wm.modals.onboarding_open
        && !app.hints_dismissed
    {
        render_startup_hints(
            f,
            l.transcript,
            input_rect,
            atman_runtime::model_registry::is_first_run(),
        );
    }
    render_pulse_bar(
        f,
        input_rect,
        app.tick,
        app.has_running_workflow(),
        border_color,
    );

    ui.wm.render(f, area, app);
    app.last_window_selection_projection = crate::selection::VisibleSelectionProjection {
        structure_revision: app.wm_visual_version,
        surfaces: ui
            .wm
            .interaction
            .last_hitmap
            .selection_projections
            .iter()
            .flat_map(|(_, projection)| projection.surfaces.iter().cloned())
            .collect(),
    };
    if let Some(selection) = app.selection.as_ref()
        && matches!(
            selection.anchor.domain,
            crate::selection::SelectionDomain::Window { .. }
        )
    {
        render_selection_highlight(f, area, 0, &app.last_window_selection_projection, selection);
    }
    render_selection_menu(f, area, &mut ui.selection_menu);
    if intro_progress >= 1.0 && app.startup_intro.is_some() {
        app.startup_intro = None;
    }
}

fn render_selection_menu(
    frame: &mut ratatui::Frame,
    canvas: ratatui::layout::Rect,
    menu: &mut Option<crate::selection_menu::SelectionMenu>,
) {
    let Some(menu) = menu.as_mut() else {
        return;
    };
    let Some((rect, item_rects)) = crate::selection_menu::layout_menu(canvas, menu.anchor) else {
        menu.rect = None;
        menu.item_rects.clear();
        return;
    };
    let t = crate::theme::theme();
    frame.render_widget(ratatui::widgets::Clear, rect);
    frame.render_widget(
        ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(ratatui::style::Style::default().fg(t.accent.into()))
            .style(ratatui::style::Style::default().bg(t.modal_bg.into())),
        rect,
    );
    menu.rect = Some(rect);
    menu.item_rects.clear();
    for (index, (label, item_rect)) in crate::selection_menu::SelectionMenu::LABELS
        .into_iter()
        .zip(item_rects)
        .enumerate()
    {
        menu.item_rects.push(item_rect);
        let cancel = crate::selection_menu::SelectionMenu::ACTIONS[index]
            == crate::selection_menu::SelectionAction::Cancel;
        let style =
            if menu.hovered == Some(index) || (menu.hovered.is_none() && menu.selected == index) {
                ratatui::style::Style::default()
                    .fg(ratatui::style::Color::Black)
                    .bg(t.accent.into())
                    .add_modifier(ratatui::style::Modifier::BOLD)
            } else {
                ratatui::style::Style::default()
                    .fg(if cancel { t.error } else { t.tinted_fg }.into())
                    .bg(t.modal_bg.into())
            };
        frame.render_widget(
            ratatui::widgets::Paragraph::new(format!(" {label} ")).style(style),
            item_rect,
        );
    }
}

pub(crate) fn render_startup_hints(
    f: &mut ratatui::Frame,
    transcript: ratatui::layout::Rect,
    input_rect: ratatui::layout::Rect,
    missing_provider: bool,
) {
    let theme = crate::theme::theme();
    let width = input_rect.width.min(58);
    let height = 4;
    let y = input_rect
        .y
        .saturating_add(input_rect.height)
        .saturating_add(1);
    if y.saturating_add(height) > transcript.y.saturating_add(transcript.height) {
        return;
    }
    let rect = ratatui::layout::Rect {
        x: input_rect.x + input_rect.width.saturating_sub(width) / 2,
        y,
        width,
        height,
    };
    f.render_widget(ratatui::widgets::Clear, rect);
    let msg = if missing_provider {
        "⚠ No provider configured — press Ctrl+K → Manage Providers, or edit config.toml"
    } else {
        "💡 Type a message and press Enter · Shift+Enter newline · /help for cmds"
    };
    let block = ratatui::widgets::Block::default()
        .borders(ratatui::widgets::Borders::ALL)
        .border_type(ratatui::widgets::BorderType::Rounded)
        .border_style(ratatui::style::Style::default().fg(theme.accent.into()));
    f.render_widget(
        ratatui::widgets::Paragraph::new(ratatui::text::Line::from(vec![
            ratatui::text::Span::raw(" "),
            ratatui::text::Span::styled(
                msg,
                ratatui::style::Style::default().fg(theme.meta_fg.into()),
            ),
            ratatui::text::Span::styled(
                "   [x]",
                ratatui::style::Style::default().fg(theme.meta_fg.into()),
            ),
        ]))
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: true }),
        rect,
    );
}

pub(crate) fn render_notify_modal(
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    message: &str,
) {
    use ratatui::layout::Alignment;
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Clear, Paragraph, Wrap};

    let theme = crate::theme::theme();
    let w = 60u16.min(area.width.saturating_sub(8));
    let lines: Vec<&str> = message.split('\n').collect();
    let h = (lines.len() + 6).min(area.height.saturating_sub(4) as usize) as u16;
    let x = area.x + (area.width - w) / 2;
    let y = area.y + (area.height - h) / 2;
    let rect = ratatui::layout::Rect {
        x,
        y,
        width: w,
        height: h,
    };

    f.render_widget(Clear, rect);
    let inner = crate::wm::shell::render_overlay_shell(
        f,
        rect,
        Line::default(),
        "",
        theme.accent.into(),
        false,
        &theme,
    );

    let text = Paragraph::new(Line::from(Span::styled(
        message,
        Style::default().add_modifier(Modifier::BOLD),
    )))
    .alignment(Alignment::Center)
    .wrap(Wrap { trim: true });
    let text_rect = ratatui::layout::Rect {
        x: inner.x + 2,
        y: inner.y + 1,
        width: inner.width.saturating_sub(4),
        height: inner.height.saturating_sub(2),
    };
    f.render_widget(text, text_rect);

    let hint = Paragraph::new(Line::from(Span::styled(
        "Press Esc to dismiss",
        Style::default().fg(theme.subtle_fg.into()),
    )))
    .alignment(Alignment::Center);
    let hint_rect = ratatui::layout::Rect {
        x: inner.x,
        y: inner.y + inner.height.saturating_sub(1),
        width: inner.width,
        height: 1,
    };
    f.render_widget(hint, hint_rect);
}

pub(crate) async fn check_latest_release() -> Option<String> {
    let url = "https://api.github.com/repos/W-Mai/atman/releases/latest";
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .user_agent("atman")
        .build()
    {
        Ok(c) => c,
        Err(_) => return None,
    };
    let resp = client.get(url).send().await.ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    let tag = body.get("tag_name")?.as_str()?;
    Some(tag.strip_prefix('v').unwrap_or(tag).to_string())
}

pub(crate) fn to_rgb(c: ratatui::style::Color) -> (u8, u8, u8) {
    match c {
        ratatui::style::Color::Rgb(r, g, b) => (r, g, b),
        ratatui::style::Color::Cyan => (40, 180, 180),
        ratatui::style::Color::DarkGray => (96, 96, 96),
        ratatui::style::Color::Gray => (128, 128, 128),
        ratatui::style::Color::Green => (70, 175, 70),
        ratatui::style::Color::Yellow => (190, 175, 55),
        ratatui::style::Color::Red => (190, 65, 65),
        ratatui::style::Color::Blue => (0, 0, 200),
        ratatui::style::Color::Magenta => (200, 0, 200),
        ratatui::style::Color::White => (240, 240, 240),
        ratatui::style::Color::Black => (16, 16, 16),
        _ => (128, 128, 128),
    }
}

pub(crate) fn lerp_rgb(
    a: ratatui::style::Color,
    b: ratatui::style::Color,
    t: f64,
) -> ratatui::style::Color {
    let (ar, ag, ab) = to_rgb(a);
    let (br, bg, bb) = to_rgb(b);
    let t = t.clamp(0.0, 1.0);
    ratatui::style::Color::Rgb(
        (ar as f64 + (br as f64 - ar as f64) * t).round() as u8,
        (ag as f64 + (bg as f64 - ag as f64) * t).round() as u8,
        (ab as f64 + (bb as f64 - ab as f64) * t).round() as u8,
    )
}

pub(crate) fn render_pulse_bar(
    f: &mut ratatui::Frame,
    input_rect: ratatui::layout::Rect,
    tick: u64,
    active: bool,
    border_color: ratatui::style::Color,
) {
    if !active {
        return;
    }
    use ratatui::style::{Color, Modifier};
    let w = input_rect.width.saturating_sub(2);
    if w < 8 {
        return;
    }
    let t = crate::theme::theme();
    let peak = if to_rgb(t.accent.into()) == to_rgb(border_color) {
        Color::Rgb(100, 210, 255)
    } else {
        t.accent.into()
    };
    let bar_y = input_rect.y + input_rect.height.saturating_sub(1);
    let bar_x = input_rect.x + 1;
    let time = tick as f64 * 0.05;
    let sigma = (w as f64) / 6.0;
    let pad = (w as f64) * 0.1;
    let amp = (w as f64) / 2.0 - pad;
    let mid = (w as f64) / 2.0;
    let center = mid + amp * time.sin();

    let buf = f.buffer_mut();
    for i in 0..w {
        let x = i as f64;
        let dx = (x - center) / sigma;
        let wave = (-0.5 * dx * dx).exp();
        if let Some(cell) = buf.cell_mut((bar_x + i, bar_y)) {
            cell.fg = lerp_rgb(border_color, peak, wave);
            if wave > 0.7 {
                cell.modifier.insert(Modifier::BOLD);
            } else {
                cell.modifier.remove(Modifier::BOLD);
            }
        }
    }

    // Bleed the wave into the vertical borders, fading upward.
    let left_x = input_rect.x;
    let right_x = input_rect.x + input_rect.width.saturating_sub(1);
    let bottom_wave_at = |col: u16| -> f64 {
        let x = if col >= bar_x {
            (col - bar_x) as f64
        } else {
            0.0
        };
        let dx = (x - center) / sigma;
        (-0.5 * dx * dx).exp()
    };
    let bleed_rows = 4u16;
    for dy in 0..=bleed_rows {
        let row = bar_y.saturating_sub(dy);
        if row <= input_rect.y {
            break;
        }
        let fade = if dy == 0 {
            1.0
        } else {
            (-0.6 * dy as f64).exp()
        };
        let left_wave = bottom_wave_at(left_x) * fade;
        let right_wave = bottom_wave_at(right_x) * fade;
        for (col, wave) in [(left_x, left_wave), (right_x, right_wave)] {
            if wave < 0.01 {
                continue;
            }
            if let Some(cell) = buf.cell_mut((col, row)) {
                cell.fg = lerp_rgb(border_color, peak, wave);
            }
        }
    }
}

#[cfg(test)]
mod quote_tests {
    use super::*;

    #[test]
    fn pending_quote_remains_visible_when_the_card_does_not_fit() {
        let backend = ratatui::backend::TestBackend::new(80, 10);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut ui = UiState::new(crate::app::AppState::new("session".into(), None));
        ui.app.pending_quote = Some("selected text".into());
        let editor = InputEditor::default();

        terminal
            .draw(|frame| render_frame(frame, &mut ui, &editor))
            .unwrap();

        assert!(ui.app.quote_rect.is_none());
        let rendered = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(rendered.contains("quote · 1 lines"));
        assert!(rendered.contains("[x]"));
    }
}
