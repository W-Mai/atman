use crate::UiState;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::input::{InputEditor, input_paragraph};
use crate::{approval_bar, completion, layout, output, sidebar, status, submission_queue};

pub(crate) fn quote_visual_lines(text: &str, card_width: u16) -> Vec<String> {
    let max_width = card_width.saturating_sub(5).max(1) as usize;
    let mut lines = Vec::new();
    for hard_line in text.split('\n') {
        let mut line = String::new();
        let mut width = 0;
        for (grapheme, grapheme_width) in crate::width::graphemes(hard_line) {
            if width > 0 && width + grapheme_width > max_width {
                lines.push(std::mem::take(&mut line));
                width = 0;
            }
            line.push_str(grapheme);
            width += grapheme_width;
        }
        lines.push(line);
    }
    lines
}

fn quote_card_text_rows(total: usize, height: u16, expanded: bool) -> usize {
    total
        .min(if expanded { 8 } else { 3 })
        .min(height.saturating_sub(2) as usize)
}

pub(crate) fn quote_card_max_scroll(text: &str, area: ratatui::layout::Rect) -> usize {
    let total = quote_visual_lines(text, area.width).len();
    total.saturating_sub(quote_card_text_rows(total, area.height, true))
}

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

/// Exact critically damped spring update, independent of frame cadence.
pub(crate) fn spring_step(position: f64, velocity: f64, target: f64, seconds: f64) -> (f64, f64) {
    let frequency = 28.0;
    let displacement = position - target;
    let momentum = velocity + frequency * displacement;
    let decay = (-frequency * seconds).exp();
    (
        target + (displacement + momentum * seconds) * decay,
        (velocity - frequency * momentum * seconds) * decay,
    )
}

pub(crate) fn spring_progress(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    if t == 1.0 {
        1.0
    } else {
        spring_step(
            0.0,
            0.0,
            1.0,
            f64::from(t) * STARTUP_SLIDE_MS as f64 / 1000.0,
        )
        .0 as f32
    }
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

mod composer;
pub(crate) use composer::{ComposerLayout, render_composer};

#[derive(Clone, Copy)]
pub(crate) enum InputLayer<'a> {
    Local,
    Detached(Option<&'a ComposerLayout>),
}

pub(crate) fn render_frame(f: &mut ratatui::Frame, ui: &mut UiState, editor: &InputEditor) {
    render_flow(f, ui, editor, InputLayer::Local);
}

pub(crate) fn render_flow(
    f: &mut ratatui::Frame,
    ui: &mut UiState,
    editor: &InputEditor,
    layer: InputLayer<'_>,
) {
    let app = &mut ui.app;
    let _preferences = app.render_preferences().enter();
    app.workspace_rect = None;
    app.last_node_regions.clear();
    let area = f.area();
    if app.last_full_rect != Some(area) {
        let responsive_narrow =
            area.width < layout::SIDEBAR_MIN_TOTAL_WIDTH && !app.focus_rail.page_open();
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
    let startup_active = app.startup_card.is_some();
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
    let status_height: u16 = 1;
    let pending_count = app.pending_permissions.len();
    let mut l = layout::compute_ex(area, status_height);
    let flow_id = app
        .items
        .snapshot()
        .map(|snapshot| snapshot.status.execution.address().instance_id.to_string());
    let mut status_area = l.status;
    app.flow_hub_rect = flow_id
        .as_ref()
        .filter(|_| crate::panel_registry::EXTENDED)
        .map(|_| {
            let width = 16.min(status_area.width);
            status_area.width = status_area.width.saturating_sub(width);
            ratatui::layout::Rect::new(
                status_area.right(),
                status_area.y,
                width,
                status_area.height,
            )
        });
    if app.flow_hub_rect.is_some() {
        status_area.width = status_area.width.saturating_sub(2);
    }
    if app.flow_view_error.is_some() {
        f.render_widget(
            Paragraph::new(" View disconnected · Switch session to reconnect")
                .style(Style::default().fg(crate::theme::theme().warn.into())),
            status_area,
        );
    } else {
        f.render_widget(
            status::render_bar(status::StatusInputs {
                flow_id,
                session_id: &app.session_id,
                session_name: app
                    .items
                    .snapshot()
                    .map(|snapshot| snapshot.display_title.as_str())
                    .or(app.session_name.as_deref()),
                goal: app.goal.as_deref(),
                streaming: app.streaming,
                waiting_for_llm: app.waiting_for_llm,
                status_notes: &app.status_notes,
                activity: Some(&app.session_activity),
                pending_permissions: pending_count,
            }),
            status_area,
        );
    }
    if let Some(rect) = app.flow_hub_rect {
        let t = crate::theme::theme();
        f.render_widget(
            Paragraph::new(" FlowHub  Esc ").style(Style::default().fg(t.heading.into()).bg(
                if app.flow_hub_hovered {
                    t.work_hover_bg.into()
                } else {
                    t.work_bg.into()
                },
            )),
            rect,
        );
    }
    let expanded = app.focus_sidebar_expanded();
    let focus = layout::FocusLayout::new(l.transcript, show_sidebar, expanded);
    l.transcript = focus.conversation;
    app.configure_focus_rail(focus.tabs.is_some());
    if let Some(tabs) = focus.tabs {
        let activity_frame = app.activity_running().then_some(app.animation_frame);
        app.focus_rail.render_tabs(f, tabs, activity_frame);
    }
    if app.focus_rail.page_open() {
        app.last_transcript_rect = None;
        app.last_selection_projection = Default::default();
        app.input_rect = None;
        app.composer_rect = None;
        app.quote_rect = None;
        app.submission_queue_rect = None;
        app.submission_queue_hitmap = Default::default();
        app.approval_hitmap = Default::default();
        let mut content = l.transcript;
        content.height = content.height.saturating_sub(1);
        let viewport = app.focus_rail.viewport(content);
        render_auxiliary_page(f, app, viewport);
        let viewport = app.focus_rail.viewport(content);
        render_auxiliary_popup(f, app);
        f.render_widget(
            Paragraph::new(format!(
                " ↑↓ {}–{}/{} · Tab · Shift+wheel text",
                viewport.visible.y + 1,
                viewport.visible.bottom(),
                viewport.canvas.height
            ))
            .style(Style::default().fg(crate::theme::theme().subtle_fg.into())),
            ratatui::layout::Rect::new(
                l.transcript.x,
                l.transcript.bottom() - 1,
                l.transcript.width,
                1,
            ),
        );
        if matches!(layer, InputLayer::Local) {
            render_windows_and_selection(f, ui);
        }
        return;
    }
    let rail_content = focus
        .rail
        .map(crate::focus_rail::FocusRail::content_area)
        .filter(|area| area.height >= 4);
    let transcript_content = layout::compute_content_rect(l.transcript);
    let local_composer = ComposerLayout::for_view(app, editor, l.transcript);
    let input_rect = local_composer.input_rect;
    let composer = match layer {
        InputLayer::Local => Some(&local_composer),
        InputLayer::Detached(composer) => composer,
    };
    if let Some(composer) = composer {
        composer.apply(app);
    } else {
        app.input_rect = None;
        app.composer_rect = None;
        app.quote_rect = None;
        app.submission_queue_rect = None;
        app.submission_queue_hitmap = Default::default();
        app.approval_hitmap = Default::default();
    }
    let transcript_area = transcript_content;
    app.last_transcript_rect = Some(transcript_area);
    let document_visible_rows = layout::document_visible_rows(transcript_area.height);
    let input_overlay_rows = composer.map_or(0, |c| c.occluded_rows(transcript_area));
    let effective_viewport = document_visible_rows.max(1);
    if let Some(loading) = app.flow_loading {
        app.last_item_ranges.clear();
        app.last_selection_projection = Default::default();
        crate::loading::render_view(
            f,
            transcript_area,
            app.session_name.as_deref().unwrap_or("Conversation"),
            loading,
            app.animation_frame,
        );
    } else if app
        .presentation_settings
        .as_ref()
        .is_some_and(crate::settings::SettingsSource::initial_loading)
    {
        app.last_item_ranges.clear();
        app.last_selection_projection = Default::default();
        f.render_widget(
            Paragraph::new(crate::loading::line(
                "Loading display",
                app.animation_frame,
                transcript_area.width,
            )),
            transcript_area,
        );
    } else if startup_active {
        if let Some(crate::app::StartupCard {
            version,
            recent,
            directory,
        }) = app.startup_card.as_ref()
        {
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
                    directory: *directory,
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
        app.resolve_scroll(0, effective_viewport, 0);
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
            app.resolve_scroll(0, effective_viewport, 0);
            app.last_item_ranges.clear();
            // Clear outside content padding so the sliding startup overlay cannot leak through.
            f.render_widget(ratatui::widgets::Clear, l.transcript);
            f.render_widget(output::empty_hint(), transcript_area);
        } else {
            let work_folds = app.work_fold_projections();
            // Retain a clicked header through the settled frame and subsequent idle redraws.
            // Explicit scrolling releases this reading anchor.
            let anchor_fold = work_folds
                .iter()
                .filter(|_| !app.scrolling_to_tail())
                .find(|fold| {
                    app.work_fold_scroll_anchor(fold.key).is_some()
                        && (!app.follow_tail || fold.animating)
                })
                .or_else(|| {
                    work_folds
                        .iter()
                        .rev()
                        .find(|fold| fold.animating && !app.scrolling_to_tail())
                })
                .map(|fold| (fold.key, fold.start_index));
            let mut cache = std::mem::take(&mut app.layout_cache);
            if let Some((key, start_index)) = anchor_fold {
                if app.work_fold_scroll_anchor(key).is_none()
                    && let Some(row) = cache.item_row_start(start_index)
                    && row >= app.scroll_offset
                    && row < app.scroll_offset.saturating_add(effective_viewport)
                {
                    app.set_work_fold_scroll_anchor(
                        key,
                        i64::from(row) - i64::from(app.scroll_offset),
                    );
                }
            } else {
                app.clear_work_fold_scroll_anchor();
            }
            // Keep a visible item stable when folds above it change height. Appended
            // output below the viewport must not be mistaken for a fold height change.
            let preceding_fold_anchor = (!app.follow_tail && !app.scrolling_to_tail())
                .then(|| app.last_item_ranges.first())
                .flatten()
                .filter(|range| {
                    work_folds
                        .iter()
                        .any(|fold| fold.end_index < range.item_index)
                })
                .and_then(|range| {
                    cache
                        .item_row_start(range.item_index)
                        .map(|row| (range.item_index, row))
                });
            cache.set_work_folds(work_folds);
            let ctx = output::RenderCtx {
                expanded_tools: &app.expanded_tools,
                animation_frame: app.animation_frame,
                panel_width: transcript_area.width,
                hovered_thinking_idx: app.hovered_thinking_idx,
                hovered_output_node: app.hovered_output_node.as_ref(),
            };
            let cache_key = output::LayoutKey {
                width: transcript_area.width,
                theme: crate::theme::current_mode(),
            };
            let request = output::LayoutRequest {
                scroll_offset: app.scroll_offset,
                viewport_rows: effective_viewport,
                follow_tail_rows: None,
            };
            let metrics = cache.update_dirty(cache_key, &app.items, &ctx, request);
            let header_offset = anchor_fold.and_then(|(key, start_index)| {
                Some(
                    (i64::from(cache.item_row_start(start_index)?)
                        - app.work_fold_scroll_anchor(key)?)
                    .clamp(0, i64::from(u32::MAX)) as u32,
                )
            });
            let reading_offset = header_offset.or_else(|| {
                let (index, old_row) = preceding_fold_anchor?;
                let new_row = cache.item_row_start(index)?;
                Some(if new_row >= old_row {
                    app.scroll_offset.saturating_add(new_row - old_row)
                } else {
                    app.scroll_offset.saturating_sub(old_row - new_row)
                })
            });
            // Resolve once, before materializing the slice used for paint and hit testing.
            // A reading anchor may leave blank space below a collapsed section.
            app.resolve_scroll(
                metrics.total_rows,
                document_visible_rows,
                input_overlay_rows,
            );
            if let Some(offset) = reading_offset {
                app.scroll_offset = offset;
            }
            if app.scroll_offset != metrics.scroll_offset {
                let ctx = output::RenderCtx {
                    expanded_tools: &app.expanded_tools,
                    animation_frame: app.animation_frame,
                    panel_width: transcript_area.width,
                    hovered_thinking_idx: app.hovered_thinking_idx,
                    hovered_output_node: app.hovered_output_node.as_ref(),
                };
                cache.update_dirty(
                    cache_key,
                    &app.items,
                    &ctx,
                    output::LayoutRequest {
                        scroll_offset: app.scroll_offset,
                        ..request
                    },
                );
            }
            let visible =
                cache.visible_slice(app.scroll_offset, effective_viewport, app.animation_frame);
            let lines = visible.lines;
            app.last_item_ranges = visible.ranges;
            app.last_node_regions = visible.regions;
            app.last_selection_projection = visible.selection;
            app.layout_cache = cache;
            crate::event_loop::resolve_transcript_selection_after_render(app);
            let paragraph = ratatui::widgets::Paragraph::new(lines).scroll((0, 0));
            f.render_widget(paragraph, transcript_area);
            if let Some(selection) = app.selection.as_ref() {
                render_selection_highlight(
                    f,
                    transcript_area,
                    app.scroll_offset,
                    &app.last_selection_projection,
                    selection,
                );
            }
        }
    }
    if let Some(rail) = focus.rail {
        let activity_frame = app.activity_running().then_some(app.animation_frame);
        app.focus_rail.render(f, rail, activity_frame);
    }
    render_auxiliary(f, app, rail_content);
    render_auxiliary_popup(f, app);
    if intro_active && let Some(intro) = app.startup_intro.as_ref() {
        output::render_startup_intro_fade(
            f,
            l.transcript,
            &intro.version,
            &intro.recent,
            intro.directory,
            intro_progress,
        );
    }
    if matches!(layer, InputLayer::Local) {
        let app = &mut ui.app;
        if startup_active
            && app.startup_container_rect.is_none()
            && !ui.wm.modals.onboarding_open
            && !app.hints_dismissed
            && !app.settings.is_waiting()
            && app.settings.load_auth().is_ok()
        {
            render_startup_hints(
                f,
                l.transcript,
                input_rect,
                app.settings
                    .models()
                    .is_first_run(&app.settings.load_auth().unwrap_or_default()),
            );
        }
    }
    let app = &mut ui.app;
    if intro_progress >= 1.0 && app.startup_intro.is_some() {
        app.startup_intro = None;
    }
    if matches!(layer, InputLayer::Local) {
        render_window_layers(f, ui, |f, ui| {
            render_composer(f, ui, editor, &local_composer)
        });
    }
}

fn render_auxiliary(
    f: &mut ratatui::Frame,
    app: &mut crate::app::AppState,
    rail_content: Option<ratatui::layout::Rect>,
) {
    app.flow_links.clear_hits();
    let sidebar_rect = rail_content.filter(|_| !app.focus_rail.activity);
    let mut sidebar_result = sidebar::SidebarRenderResult::empty();
    if let Some(area) = sidebar_rect {
        let project_root = app.project_root.clone().or_else(|| {
            app.session
                .as_ref()
                .and_then(|s| s.meta())
                .and_then(|m| m.project_root)
                .map(|p| p.display().to_string())
        });
        let goal_scroll = app.goal_scroll;
        let plans_scroll = app.plans_scroll;
        let todos_scroll = app.todos_scroll;
        let goal = app.goal_record();
        let goal_status = app.goal_presentation();
        let sr = sidebar::render(
            f,
            area,
            sidebar::SidebarInputs {
                workspace: app
                    .items
                    .snapshot()
                    .filter(|_| crate::panel_registry::EXTENDED)
                    .map(|snapshot| sidebar::WorkspaceSummary {
                        request: snapshot.workspace.as_ref(),
                        hovered: app.workspace_hovered,
                        auxiliary_page: app.focus_rail.page_open(),
                    }),
                neighbor_height: app
                    .items
                    .snapshot()
                    .filter(|_| crate::panel_registry::EXTENDED && area.width >= 24)
                    .map_or(0, |snapshot| {
                        app.flow_links.height(
                            snapshot.status.execution.address(),
                            area.height.saturating_sub(28).min(19),
                        )
                    }),
                goal: goal
                    .as_ref()
                    .map(|g| g.objective.as_str())
                    .or(app.goal.as_deref()),
                goal_status: goal_status.as_ref(),
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
                sidebar_collapsed: app.sidebar_collapsed || area.width < 24,
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
        if let Some(neighbors) = sr.neighbor_rect
            && let Some(snapshot) = app.items.snapshot()
        {
            let owner = snapshot.status.execution.address().clone();
            app.flow_links.render(f, neighbors, &owner);
        }
        sidebar_result = sr;
    }
    {
        let sr = sidebar_result;
        app.last_sidebar_rect = sidebar_rect;
        app.workspace_rect = sr.workspace_rect;
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
    app.flow_activity.clear_hits();
    if let Some(tp_area) = rail_content.filter(|_| app.focus_rail.activity) {
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
        app.flow_activity.animation_frame = app.animation_frame;
        let hitmap = crate::task_panel::render(
            f,
            tp_area,
            &app.task_snapshots,
            &app.activity_nodes,
            &app.items,
            &app.handle_index,
            &app.detached_task_details,
            app.sidebar_collapsed,
            &app.task_panel_collapsed_groups,
            &hover,
            watch_hub,
            &mut app.flow_activity,
        );
        app.last_task_panel_rect = Some(tp_area);
        app.last_task_panel_hitmap = hitmap;
    } else {
        app.last_task_panel_rect = None;
        app.last_task_panel_hitmap = crate::task_panel::TaskPanelHitMap::default();
    }
}

fn render_auxiliary_popup(f: &mut ratatui::Frame, app: &mut crate::app::AppState) {
    app.last_sidebar_popup_rect =
        if let (Some(kind), Some(rect)) = (app.sidebar_popup, app.last_sidebar_rect) {
            Some(crate::sidebar::render_sidebar_popup(
                f, rect, kind, &app.plans, &app.todos,
            ))
        } else {
            None
        };
}

fn render_auxiliary_page(
    frame: &mut ratatui::Frame,
    app: &mut crate::app::AppState,
    mut viewport: crate::focus_rail::AuxiliaryViewport,
) {
    let mut surface = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
        viewport.canvas.right(),
        viewport.canvas.bottom(),
    ))
    .expect("in-memory auxiliary viewport");
    let mut canvas = surface.get_frame();
    render_auxiliary(&mut canvas, app, Some(viewport.canvas));
    viewport = app.reveal_activity_viewport(viewport);
    for y in 0..viewport.target.height {
        for x in viewport.target.x..viewport.target.right() {
            frame.buffer_mut()[(x, viewport.target.y + y)] =
                canvas.buffer_mut()[(x, viewport.visible.y + y)].clone();
        }
    }
    for rect in [
        &mut app.last_sidebar_rect,
        &mut app.workspace_rect,
        &mut app.last_goal_rect,
        &mut app.last_plan_rect,
        &mut app.last_todo_rect,
        &mut app.last_goal_hdr_rect,
        &mut app.last_plan_hdr_rect,
        &mut app.last_todo_hdr_rect,
        &mut app.last_ctx_hdr_rect,
        &mut app.last_meta_hdr_rect,
        &mut app.last_mcp_hdr_rect,
        &mut app.last_collapse_btn_rect,
        &mut app.last_expand_btn_rect,
        &mut app.last_upper_title_rect,
        &mut app.last_lower_title_rect,
        &mut app.last_sidebar_more_rect,
        &mut app.last_task_panel_rect,
    ] {
        viewport.project_option(rect);
    }
    app.last_sidebar_strip_rects
        .retain(|_, rect| project_rect(viewport, rect));
    for surface in &mut app.last_sidebar_selection_projection.surfaces {
        surface
            .atoms
            .retain_mut(|atom| project_rect(viewport, &mut atom.rect));
    }
    let hits = &mut app.last_task_panel_hitmap;
    for rect in [&mut hits.history_btn_rect, &mut hits.hamburger_rect] {
        viewport.project_option(rect);
    }
    hits.kill_rects
        .retain_mut(|(_, rect)| project_rect(viewport, rect));
    hits.group_header_rects
        .retain_mut(|(_, rect)| project_rect(viewport, rect));
    hits.task_rects
        .retain_mut(|(_, rect)| project_rect(viewport, rect));
    hits.task_header_rects
        .retain_mut(|(_, rect)| project_rect(viewport, rect));
    hits.activity_rects
        .retain_mut(|(_, _, rect)| project_rect(viewport, rect));
    hits.insert_rects
        .retain_mut(|(_, rect)| project_rect(viewport, rect));
    app.flow_links.project_hits(viewport);
    app.flow_activity.project(viewport);
}

fn project_rect(
    viewport: crate::focus_rail::AuxiliaryViewport,
    rect: &mut ratatui::layout::Rect,
) -> bool {
    if let Some(visible) = viewport.project(*rect) {
        *rect = visible;
        true
    } else {
        false
    }
}

pub(crate) fn render_windows_and_selection(f: &mut ratatui::Frame, ui: &mut UiState) {
    render_window_layers(f, ui, |_, _| {});
}

pub(crate) fn render_window_layers(
    f: &mut ratatui::Frame,
    ui: &mut UiState,
    compose: impl FnOnce(&mut ratatui::Frame, &mut UiState),
) {
    let area = f.area();
    let app = &mut ui.app;
    let _preferences = app.render_preferences().enter();
    ui.wm.render_panels(f, area, app);
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
    compose(f, ui);
    let app = &mut ui.app;
    ui.wm.render_overlays(f, area, app);
    if ui.wm.modals.any_open() || app.modal_notification.is_some() {
        ui.selection_menu = None;
    } else {
        render_selection_menu(f, area, &mut ui.selection_menu);
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
    frame.render_widget(crate::surface::block(crate::surface::Role::Overlay), rect);
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
    let block = crate::surface::block(crate::surface::Role::Notice)
        .padding(ratatui::widgets::Padding::uniform(1));
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
    fn document_rendering_does_not_wait_for_the_execution_message_lock() {
        use std::sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        };
        let session = Arc::new(atman_runtime::Session::open_instance_context(
            atman_runtime::flow_store::FlowRef {
                session_id: uuid::Uuid::now_v7().into(),
                instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
            },
        ));
        let mut ui = UiState::new(crate::app::AppState::new("prepared-document".into(), None));
        ui.app.session = Some(session.clone());
        ui.app.push_item(crate::app::OutputItem::AssistantMd {
            md: "Prepared document remains visible".into(),
            streaming: false,
            retried: false,
        });
        let (started, entered) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let expired = Arc::new(AtomicBool::new(false));
        let timed_out = expired.clone();
        let blocker = std::thread::spawn(move || {
            let messages = session.messages_handle();
            let _held = messages.lock().unwrap();
            started.send(()).unwrap();
            if wait
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_err()
            {
                timed_out.store(true, Ordering::Release);
            }
        });
        entered.recv().unwrap();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
        for width in [120, 90] {
            terminal.backend_mut().resize(width, 30);
            terminal.autoresize().unwrap();
            terminal
                .draw(|frame| render_frame(frame, &mut ui, &InputEditor::default()))
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("Prepared document remains visible"));
        }
        let _ = release.send(());
        blocker.join().unwrap();
        assert!(
            !expired.load(Ordering::Acquire),
            "rendering waited for a message lock it does not use"
        );
    }

    #[test]
    fn non_document_frames_do_not_retain_document_click_targets() {
        for mode in 0..3 {
            let mut ui = UiState::new(crate::app::AppState::new("test".into(), None));
            ui.app.last_node_regions.push(crate::output::NodeRegion {
                panel_item_index: 0,
                path_key: "old-tool".into(),
                start_row: 0,
                end_row: 3,
                col_start: 0,
                col_end: 20,
            });
            if mode == 1 {
                ui.app.flow_loading = Some(crate::loading::ViewLoading {
                    phase: crate::loading::ViewPhase::Reading,
                    started_at: std::time::Instant::now(),
                });
            }
            let (width, height) = if mode == 2 { (20, 4) } else { (120, 40) };
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| render_frame(frame, &mut ui, &InputEditor::default()))
                .unwrap();
            assert!(ui.app.last_node_regions.is_empty());
        }
    }

    #[test]
    fn activity_footer_focus_projects_visible_controls_after_compact_layout_changes() {
        use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
        for (width, height) in [(95, 26), (65, 26), (95, 12)] {
            let mut ui = UiState::new(crate::app::AppState::new("test".into(), None));
            let editor = InputEditor::default();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
            let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
            terminal
                .draw(|f| render_frame(f, &mut ui, &editor))
                .unwrap();
            ui.app.select_focus_rail(true);
            assert!(ui.app.activity_key(&key(KeyCode::Tab)));
            terminal
                .draw(|f| render_frame(f, &mut ui, &editor))
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(ui.app.focus_rail.page_open());
            assert!(
                text.contains("Sort"),
                "missing focused control at {width}x{height}"
            );
            let before = terminal.backend().buffer().clone();
            terminal
                .draw(|f| render_frame(f, &mut ui, &editor))
                .unwrap();
            assert_eq!(terminal.backend().buffer(), &before);
            assert!(!ui.app.activity_key(&key(KeyCode::Esc)));
            assert!(ui.app.focus_page_event(&key(KeyCode::Esc)));
            terminal
                .draw(|f| render_frame(f, &mut ui, &editor))
                .unwrap();
            assert!(!ui.app.focus_rail.page_open());
        }
    }

    #[cfg(feature = "extended-panels")]
    #[tokio::test]
    async fn workspace_summary_shares_committed_binding_and_clipped_input_targets() {
        use atman_runtime::{
            flow_runtime::FlowWorker,
            flow_store::{
                EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, RuntimeStore,
                SessionDescriptor,
                workspace::{WorkspaceRequest, WorkspaceSource},
            },
            git_workspace::WorkspaceBinding,
            source_program::RevisionBundle,
        };
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        use ratatui::{Terminal, backend::TestBackend, layout::Rect};
        use std::num::NonZeroUsize;
        use uuid::Uuid;
        let directory = tempfile::tempdir().unwrap();
        let runtime = RuntimeStore::open(directory.path()).unwrap();
        let session_id = Uuid::now_v7();
        let mut session = runtime
            .create_session(SessionDescriptor {
                id: session_id,
                title: "Example".into(),
                configuration: Default::default(),
                project: None,
            })
            .unwrap();
        let owner = FlowRef {
            session_id: session_id.into(),
            instance_id: FlowInstanceId::now(),
        };
        let mut store = session
            .create_flow(FlowDescriptor {
                address: owner.clone(),
                creation_request: Uuid::now_v7(),
                created_by: None,
                content: EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: RevisionBundle {
                        entry: "main.at".into(),
                        sources: [("main.at".into(), "flow main() {}".into())].into(),
                        edges: Default::default(),
                    },
                }
                .into(),
            })
            .unwrap();
        let request = WorkspaceRequest {
            binding: WorkspaceBinding {
                repository_root: directory.path().into(),
                workspace_id: "recorded".into(),
                path: directory.path().join(".atman/worktrees/recorded"),
                branch: None,
            },
            source: WorkspaceSource::Share {
                sponsor: FlowRef {
                    session_id: session_id.into(),
                    instance_id: FlowInstanceId::now(),
                },
            },
        };
        store.request_workspace(request.clone()).unwrap();
        let worker = FlowWorker::start(store).await.unwrap();
        let mut reader = worker
            .subscribe_view(NonZeroUsize::new(1).unwrap())
            .await
            .unwrap();
        let view = reader.refresh().await.unwrap();
        let cursor = view.snapshot().status.cursor.clone();
        let expects_binding = |action: Option<crate::flow_hub::Action>| {
            assert!(matches!(action,
            Some(crate::flow_hub::Action::Load(crate::flow_hub::HubRequest::FlowWorkspace { owner: target })) if target == owner))
        };
        // Separate view state shares one immutable source, even if the checkout is absent.
        for x in [0, 7] {
            let mut app = crate::app::AppState::new(session_id.to_string(), None);
            app.sync_flow_view(view).unwrap();
            let mut surface = Terminal::new(TestBackend::new(x + 40, 60)).unwrap();
            surface
                .draw(|f| render_auxiliary(f, &mut app, Some(Rect::new(x, 0, 40, 60))))
                .unwrap();
            let rect = app.workspace_rect.unwrap();
            assert_eq!(rect.height, 5);
            let text = surface
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("Shared from"));
            for row in rect.y..rect.bottom() {
                expects_binding(app.workspace_mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x + 1,
                    row,
                    modifiers: KeyModifiers::NONE,
                }));
            }
            let viewport = crate::focus_rail::AuxiliaryViewport {
                canvas: Rect::new(x, 0, 40, 60),
                visible: Rect::new(x, 40, 40, 20),
                target: Rect::new(x, 3, 40, 20),
            };
            app.configure_focus_rail(true);
            app.select_focus_rail(false);
            surface
                .draw(|f| render_auxiliary_page(f, &mut app, viewport))
                .unwrap();
            assert_eq!(app.workspace_rect, viewport.project(rect));
            assert!(app.focus_page_event(&Event::Key(KeyEvent::new(
                KeyCode::Char('w'),
                KeyModifiers::NONE
            ))));
            expects_binding(app.open_flow_hub.take());
            app.select_focus_rail(true);
            surface
                .draw(|f| render_auxiliary_page(f, &mut app, viewport))
                .unwrap();
            assert!(app.workspace_rect.is_none());
            assert!(
                app.workspace_mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x + 1,
                    row: rect.y,
                    modifiers: KeyModifiers::NONE
                })
                .is_none()
            );
        }
        assert_eq!(worker.view_snapshot().await.unwrap().status.cursor, cursor);
        assert!(!request.binding.path.exists());
        worker.shutdown().await.unwrap();
    }

    #[test]
    fn auxiliary_viewport_pixels_controls_and_selection_share_one_projection() {
        use ratatui::{Terminal, backend::TestBackend, layout::Rect};
        for activity in [false, true] {
            for (x, y) in [(0, 0), (7, 6)] {
                for height in [7, 19, 40] {
                    for offset in [0, 9, 60 - height] {
                        let make_app = || {
                            let mut app = crate::app::AppState::new("test".into(), None);
                            app.goal = Some("A selectable objective with several words".into());
                            app.select_focus_rail(activity);
                            app
                        };
                        let target = Rect::new(x, y, 40, height);
                        let viewport = crate::focus_rail::AuxiliaryViewport {
                            canvas: Rect::new(x, 0, 40, 60),
                            visible: Rect::new(x, offset, 40, height),
                            target,
                        };
                        let mut baseline_app = make_app();
                        let mut baseline =
                            Terminal::new(TestBackend::new(target.right(), 60)).unwrap();
                        baseline
                            .draw(|f| render_auxiliary(f, &mut baseline_app, Some(viewport.canvas)))
                            .unwrap();
                        let mut app = make_app();
                        let mut terminal =
                            Terminal::new(TestBackend::new(target.right(), target.bottom()))
                                .unwrap();
                        terminal
                            .draw(|f| render_auxiliary_page(f, &mut app, viewport))
                            .unwrap();
                        for row in 0..height {
                            for column in x..target.right() {
                                assert_eq!(
                                    terminal.backend().buffer()[(column, y + row)],
                                    baseline.backend().buffer()[(column, offset + row)]
                                );
                            }
                        }
                        for (source, projected) in [
                            (baseline_app.last_goal_rect, app.last_goal_rect),
                            (baseline_app.last_goal_hdr_rect, app.last_goal_hdr_rect),
                            (baseline_app.last_ctx_hdr_rect, app.last_ctx_hdr_rect),
                            (
                                baseline_app.last_task_panel_hitmap.history_btn_rect,
                                app.last_task_panel_hitmap.history_btn_rect,
                            ),
                        ] {
                            assert_eq!(source.and_then(|rect| viewport.project(rect)), projected);
                        }
                        for surface in &app.last_sidebar_selection_projection.surfaces {
                            let original = baseline_app
                                .last_sidebar_selection_projection
                                .surfaces
                                .iter()
                                .find(|original| original.section == surface.section)
                                .unwrap();
                            for atom in &surface.atoms {
                                assert_eq!(atom.rect.intersection(target), atom.rect);
                                for column in atom.cols.clone() {
                                    assert_eq!(
                                        surface.point_at(atom.rect.y, column),
                                        original.point_at(atom.rect.y - y + offset, column)
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn compact_focus_pages_preserve_drafts_and_restore_only_visible_hit_targets() {
        use crossterm::event::{
            Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
        };
        let mut ui = UiState::new(crate::app::AppState::new("test".into(), None));
        ui.app.goal = Some("Inspect this flow".into());
        ui.app.sidebar_upper_collapsed = true;
        ui.app.sidebar_lower_collapsed = true;
        ui.app.pending_quote = Some("Preserved quotation".into());
        let mut editor = InputEditor::default();
        editor.insert_str("Preserved draft");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 24)).unwrap();
        let key = |code| Event::Key(KeyEvent::new(code, KeyModifiers::NONE));
        terminal
            .draw(|f| render_frame(f, &mut ui, &editor))
            .unwrap();
        assert_eq!(
            ui.app.input_rect.unwrap().width,
            layout::input_outer_width(40)
        );
        assert!(ui.app.last_sidebar_rect.is_none());
        assert!(ui.wm.dispatch_meta_command(":sidebar", &mut ui.app, None));
        terminal
            .draw(|f| render_frame(f, &mut ui, &editor))
            .unwrap();
        assert!(ui.app.focus_rail.page_open());
        assert_eq!(ui.app.last_sidebar_rect.unwrap().width, 40);
        assert!(!ui.app.sidebar_upper_collapsed);
        assert!(!ui.app.sidebar_lower_collapsed);
        assert!(ui.app.last_transcript_rect.is_none());
        assert!(ui.app.input_rect.is_none());
        assert!(ui.app.quote_rect.is_none());
        assert!(ui.app.submission_queue_hitmap.rows.is_empty());
        assert!(ui.app.focus_page_event(&key(KeyCode::Char('x'))));
        assert!(ui.app.focus_page_event(&key(KeyCode::Enter)));
        assert!(
            ui.app
                .focus_page_event(&Event::Paste("hidden input".into()))
        );
        assert!(ui.app.focus_page_event(&key(KeyCode::Tab)));
        terminal
            .draw(|f| render_frame(f, &mut ui, &editor))
            .unwrap();
        assert!(ui.app.last_sidebar_rect.is_none());
        assert!(ui.app.last_goal_rect.is_none());
        assert_eq!(ui.app.last_task_panel_rect.unwrap().width, 40);
        assert!(!ui.app.focus_page_event(&key(KeyCode::F(6))));
        assert!(ui.app.focus_page_event(&key(KeyCode::Esc)));
        assert!(!ui.app.focus_page_event(&key(KeyCode::Esc)));
        terminal
            .draw(|f| render_frame(f, &mut ui, &editor))
            .unwrap();
        assert!(ui.app.last_task_panel_rect.is_none());
        assert!(ui.app.last_transcript_rect.is_some());
        assert_eq!(
            ui.app.input_rect.unwrap().width,
            layout::input_outer_width(40)
        );
        assert_eq!(editor.buf(), "Preserved draft");
        assert_eq!(ui.app.pending_quote.as_deref(), Some("Preserved quotation"));
        // The entire three-row launcher is clickable, including its padding.
        let action = ui
            .app
            .focus_rail
            .mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 1,
                modifiers: KeyModifiers::NONE,
            })
            .unwrap();
        ui.app.apply_focus_action(action);
        terminal
            .draw(|f| render_frame(f, &mut ui, &editor))
            .unwrap();
        assert!(ui.app.focus_rail.page_open());
        let action = ui
            .app
            .focus_rail
            .mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 3,
                modifiers: KeyModifiers::NONE,
            })
            .unwrap();
        ui.app.apply_focus_action(action);
        assert!(!ui.app.focus_rail.page_open());
        let activity = ui.app.focus_rail.activity;
        ui.app.toggle_focus_activity();
        assert_eq!(ui.app.focus_rail.activity, !activity);
        terminal.backend_mut().resize(120, 45);
        terminal
            .draw(|f| render_frame(f, &mut ui, &editor))
            .unwrap();
        assert!(!ui.app.focus_rail.page_open());
        assert!(ui.app.input_rect.is_some());
        assert_eq!(editor.buf(), "Preserved draft");
    }

    #[test]
    fn focus_surfaces_keep_controls_outside_the_conversation_and_clear_hidden_hits() {
        for width in [40, 80, 120, 200] {
            for height in [12, 24, 45, 70] {
                let mut app = UiState::new(crate::app::AppState::new("test".into(), None));
                app.app.goal = Some("Review the current flow".into());
                let mut editor = InputEditor::default();
                editor.insert_str("preserved draft");
                let mut terminal =
                    ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                        .unwrap();
                for activity in [false, true, false] {
                    app.app.select_focus_rail(activity);
                    terminal
                        .draw(|frame| render_frame(frame, &mut app, &editor))
                        .unwrap();
                    let body = app.app.last_transcript_rect;
                    let rail = if activity {
                        app.app.last_task_panel_rect
                    } else {
                        app.app.last_sidebar_rect
                    };
                    if let Some(rail) = rail {
                        assert!(body.is_none_or(|body| body.intersection(rail).is_empty()));
                        assert!(
                            app.app
                                .input_rect
                                .is_none_or(|input| input.intersection(rail).is_empty())
                        );
                    }
                    if activity {
                        assert!(app.app.last_sidebar_rect.is_none());
                        assert!(app.app.last_goal_rect.is_none());
                        assert!(app.app.last_ctx_hdr_rect.is_none());
                    } else {
                        assert!(app.app.last_task_panel_rect.is_none());
                        assert!(app.app.last_task_panel_hitmap.kill_rects.is_empty());
                        if let Some(goal) = app.app.last_goal_rect {
                            assert_eq!(goal.intersection(app.app.last_sidebar_rect.unwrap()), goal);
                        }
                    }
                    assert_eq!(editor.buf(), "preserved draft");
                }
            }
        }
    }

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

    #[test]
    fn quote_card_previews_three_lines_and_expands() {
        let backend = ratatui::backend::TestBackend::new(90, 28);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        let mut ui = UiState::new(crate::app::AppState::new("session".into(), None));
        ui.app.pending_quote = Some("one\ntwo\nthree\nfour\nfive\nsix".into());
        let editor = InputEditor::default();

        terminal
            .draw(|frame| render_frame(frame, &mut ui, &editor))
            .unwrap();
        let collapsed = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(ui.app.quote_rect.is_some());
        assert!(collapsed.contains("+3 more"));
        assert!(!collapsed.contains("  six"));
        ui.app.quote_expanded = true;
        terminal
            .draw(|frame| render_frame(frame, &mut ui, &editor))
            .unwrap();
        let expanded = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(expanded.contains("  six"));
    }

    #[test]
    fn quote_disclosure_counts_wrapped_screen_rows() {
        let quote = "one two three four five six seven eight nine ten eleven twelve";
        let wrapped = quote_visual_lines(quote, 20);
        assert!(wrapped.len() > quote.lines().count());
        assert!(wrapped.iter().all(|line| crate::width::width(line) <= 15));
        let area = ratatui::layout::Rect::new(0, 0, 20, 6);
        assert_eq!(
            quote_card_max_scroll(quote, area),
            wrapped.len().saturating_sub(4)
        );
    }

    #[test]
    fn quote_wrap_preserves_source_spacing() {
        let quote = "  indented  code and words\n\n你好世界你好世界";
        let wrapped = quote_visual_lines(quote, 20);
        assert_eq!(wrapped[0].as_str(), "  indented  cod");
        assert_eq!(wrapped[1].as_str(), "e and words");
        assert_eq!(wrapped[2].as_str(), "");
        assert!(wrapped.iter().all(|line| crate::width::width(line) <= 15));
        assert_eq!(wrapped[3..].concat(), "你好世界你好世界");
    }
}

#[cfg(test)]
mod work_scroll_tests {
    use super::*;
    use crate::app::{AppState, OutputItem};
    use ratatui::{Terminal, backend::TestBackend};
    use std::time::{Duration, Instant};

    #[test]
    fn automatic_work_collapse_settles_without_a_corrective_redraw() {
        let mut app = AppState::new("auto-fold".into(), None);
        for i in 0..20 {
            app.push_item(OutputItem::AssistantMd {
                md: format!("Earlier {i}"),
                streaming: false,
                retried: false,
            });
        }
        let start = app.items.len();
        for i in 0..2 {
            app.push_item(OutputItem::AssistantMd {
                md: format!("Internal work {i}\n\nDetails"),
                streaming: false,
                retried: false,
            });
        }
        let mut ui = UiState::new(app);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        draw(&mut terminal, &mut ui);
        ui.app
            .items
            .upsert_work_fold(start..ui.app.items.len(), Some("Finished work"), false)
            .unwrap();
        ui.app.push_item(OutputItem::AssistantMd {
            md: "Final answer".into(),
            streaming: false,
            retried: false,
        });
        for elapsed in [0, 100, 300, 800] {
            ui.app.items.work_folds[0].started_at = Instant::now() - Duration::from_millis(elapsed);
            draw(&mut terminal, &mut ui);
        }
        let settled = terminal.backend().buffer().clone();
        let offset = ui.app.scroll_offset;
        draw(&mut terminal, &mut ui);
        assert_eq!(ui.app.scroll_offset, offset);
        assert_eq!(
            *terminal.backend().buffer(),
            settled,
            "automatic completion must finish its viewport in the last animation frame"
        );
        let answer = ui.app.items.len() - 1;
        ui.app
            .items
            .mutate(answer, crate::flow_view::OutputMutation::Semantic, |item| {
                let OutputItem::AssistantMd { md, .. } = item else {
                    panic!("expected final answer")
                };
                *md = "Streaming final paragraph\n\n".repeat(30);
                true
            });
        draw(&mut terminal, &mut ui);
        assert_eq!(
            ui.app.scroll_offset,
            ui.app.max_scroll_offset(),
            "automatic final output must continue following the tail"
        );
    }

    fn draw(terminal: &mut Terminal<TestBackend>, ui: &mut UiState) {
        terminal
            .draw(|frame| render_frame(frame, ui, &InputEditor::default()))
            .unwrap();
    }

    fn header(ui: &UiState) -> (String, u16, u16) {
        let region = ui
            .app
            .last_node_regions
            .iter()
            .find(|r| r.path_key.starts_with(output::WORK_FOLD_REGION_PREFIX))
            .unwrap();
        let area = ui.app.last_transcript_rect.unwrap();
        (
            region.path_key.clone(),
            area.x + region.col_start,
            area.y + region.start_row.saturating_sub(ui.app.scroll_offset) as u16,
        )
    }

    #[test]
    fn work_disclosure_keeps_rendered_and_interactive_anchors_through_settlement() {
        for (before, clipped) in [(0, false), (20, false), (20, true)] {
            for follow_tail in [false, true] {
                let mut app = AppState::new("scroll-test".into(), None);
                for i in 0..before {
                    app.push_item(OutputItem::AssistantMd {
                        md: format!("Earlier paragraph {i}"),
                        streaming: false,
                        retried: false,
                    });
                }
                let start = app.items.len();
                for i in 0..6 {
                    app.push_item(OutputItem::AssistantMd {
                        md: format!("Work member {i}\n\nsecond line\n\nthird line"),
                        streaming: false,
                        retried: false,
                    });
                }
                app.items
                    .upsert_work_fold(start..app.items.len(), Some("Anchor probe"), true)
                    .unwrap();
                app.push_item(OutputItem::AssistantMd {
                    md: if clipped {
                        "Final paragraph\n\n".repeat(30)
                    } else {
                        "Final answer".into()
                    },
                    streaming: false,
                    retried: false,
                });
                let mut ui = UiState::new(app);
                let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
                draw(&mut terminal, &mut ui);
                if clipped {
                    ui.app.follow_tail = false;
                    ui.app.scroll_offset = ui.app.layout_cache.item_row_start(start).unwrap() + 2;
                    draw(&mut terminal, &mut ui);
                }
                let original_offset = ui.app.scroll_offset;
                ui.app.follow_tail = follow_tail;
                for expand in [true, false] {
                    let (key, x, y) = header(&ui);
                    let screen_before = terminal.backend().buffer().clone();
                    assert_eq!(ui.app.hit_test_node(x, y).unwrap().1, key);
                    assert!(ui.app.toggle_work_fold(&key));
                    for elapsed in [0, 70, 170, 320, 800] {
                        ui.app.items.work_folds[0].started_at =
                            Instant::now() - Duration::from_millis(elapsed);
                        draw(&mut terminal, &mut ui);
                        assert_eq!(ui.app.has_active_animation(), elapsed < 640);
                        let (next_key, next_x, next_y) = header(&ui);
                        assert_eq!(
                            next_y, y,
                            "header moved: before={before}, follow={follow_tail}, expand={expand}, elapsed={elapsed}"
                        );
                        assert_eq!(ui.app.hit_test_node(next_x, next_y).unwrap().1, next_key);
                        // Everything above the clicked header is an unchanged visual anchor.
                        let area = ui.app.last_transcript_rect.unwrap();
                        for row in area.y..y {
                            for col in area.x..area.right() {
                                assert_eq!(
                                    terminal.backend().buffer()[(col, row)].symbol(),
                                    screen_before[(col, row)].symbol(),
                                    "paint differs from anchor: before={before}, follow={follow_tail}, expand={expand}, elapsed={elapsed}, cell={col},{row}"
                                );
                            }
                        }
                    }
                    assert!(!ui.app.has_active_animation());
                    let settled = terminal.backend().buffer().clone();
                    draw(&mut terminal, &mut ui);
                    assert_eq!(
                        *terminal.backend().buffer(),
                        settled,
                        "idle redraw must not correct a stale frame"
                    );
                    let target = ui.app.hit_test_node(x, y);
                    ui.app.set_hovered_output_node(target);
                    draw(&mut terminal, &mut ui);
                    assert_eq!(header(&ui).2, y, "hover must not move the header");
                    ui.app.set_hovered_output_node(None);
                    draw(&mut terminal, &mut ui);
                    assert_eq!(
                        *terminal.backend().buffer(),
                        settled,
                        "hover-only redraw must preserve geometry"
                    );
                    ui.app.scroll_to_tail();
                    draw(&mut terminal, &mut ui);
                    assert_eq!(ui.app.scroll_offset, ui.app.max_scroll_offset());
                    ui.app
                        .scroll_up(ui.app.scroll_offset.saturating_sub(original_offset));
                    draw(&mut terminal, &mut ui);
                }
            }
        }
    }
}
