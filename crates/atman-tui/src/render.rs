use crate::UiState;
use ratatui::layout::Alignment;
use ratatui::style::{Color, Style};
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

use crate::app::AppState;
use crate::input::{InputEditor, input_paragraph};
use crate::{approval_bar, completion, layout, output, sidebar, status};

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
    for y in area.y..area.y + area.height {
        if y < buf_area.y || y >= buf_area.y + buf_area.height {
            continue;
        }
        if let Some(ox) = outside_left {
            let cell = &mut buf[(ox, y)];
            if crate::width::width(cell.symbol()) > 1 {
                clear_wide(cell);
            }
        }
        {
            let cell = &mut buf[(inside_left, y)];
            if cell.symbol().is_empty() {
                clear_wide(cell);
            }
        }
        if inside_right != inside_left {
            let cell = &mut buf[(inside_right, y)];
            if crate::width::width(cell.symbol()) > 1 {
                clear_wide(cell);
            }
        }
        if let Some(rx) = outside_right {
            let cell = &mut buf[(rx, y)];
            if cell.symbol().is_empty() {
                clear_wide(cell);
            }
        }
    }
}

pub(crate) fn render_frame(f: &mut ratatui::Frame, ui: &mut UiState, editor: &InputEditor) {
    let app = &mut ui.app;
    let area = f.area();
    app.last_full_rect = Some(area);
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
    app.sidebar_collapse_locked = show_sidebar && area.width < layout::SIDEBAR_MIN_TOTAL_WIDTH;
    let sidebar_effective_collapsed = app.sidebar_collapsed || app.sidebar_collapse_locked;
    let status_height: u16 = 1;
    let pending_count = app.pending_approvals.len();
    let approvals_rows: u16 = if pending_count == 0 {
        0
    } else {
        let items = pending_count.min(9) as u16;
        let overflow = if pending_count > 9 { 1 } else { 0 };
        items + overflow + 2
    };
    let injection_rows: u16 = if app.pending_injections.is_empty() {
        0
    } else {
        // title + N items + 2 for block borders
        (app.pending_injections.len() as u16).min(5) + 3
    };
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
    let approvals_rect = layout::compute_approvals_rect(l.transcript, input_rect, approvals_rows);
    app.input_rect = Some(input_rect);
    f.render_widget(
        status::render_bar(status::StatusInputs {
            session_id: &app.session_id,
            goal: app.goal.as_deref(),
            streaming: app.streaming,
            waiting_for_llm: app.waiting_for_llm,
            status_notes: &app.status_notes,
        }),
        l.status,
    );
    let transcript_area = transcript_content;
    app.last_transcript_rect = Some(transcript_area);
    let document_visible_rows = layout::document_visible_rows(transcript_area.height);
    let input_overlay_rows = layout::input_overlay_rows(input_rect, transcript_area);
    let effective_viewport = document_visible_rows.max(1);
    if startup_active {
        if let Some(crate::app::OutputItem::StartupCard { version, recent }) = app.items.first() {
            let base = output::compute_startup_overlay(l.transcript, recent).area;
            f.render_widget(ratatui::widgets::Clear, l.transcript);
            output::render_startup_overlay(f, base, version, recent, false, recent.len());
        }
        app.resolve_scroll(0, effective_viewport, 0, app.items.len());
        app.last_item_ranges.clear();
    } else if app.items.is_empty() {
        app.resolve_scroll(0, effective_viewport, 0, app.items.len());
        app.last_item_ranges.clear();
        // Clear the full unpadded transcript rect first — otherwise the
        // 2-col padding strip on each side of transcript_area keeps
        // whatever the previous frame's overlay painted there, and the
        // startup card's animated edges leak through for one frame
        // after the slide completes.
        f.render_widget(ratatui::widgets::Clear, l.transcript);
        f.render_widget(output::empty_hint(), transcript_area);
    } else {
        let messages_lock = app.session.as_ref().map(|s| s.messages_handle());
        let messages_guard = messages_lock.as_ref().and_then(|h| h.lock().ok());
        let empty_messages: Vec<atman_runtime::message::Message> = Vec::new();
        let messages: &[atman_runtime::message::Message] =
            messages_guard.as_deref().unwrap_or(&empty_messages);
        let ctx = output::RenderCtx {
            expanded_tools: &app.expanded_tools,
            messages,
            animation_frame: app.animation_frame,
            panel_width: transcript_area.width,
            hovered_thinking_idx: app.hovered_thinking_idx,
        };
        let animation_key = if app.has_active_animation() {
            Some(app.animation_frame)
        } else {
            None
        };
        let cache_key = output::LayoutKey {
            items_version: app.items_version,
            expanded_version: app.expanded_version,
            width: transcript_area.width,
            animation_frame: animation_key,
        };
        let mut cache = std::mem::take(&mut app.layout_cache);
        // Two-phase: compute layout first (pass 1), then extract visible
        // lines with the up-to-date total_rows (pass 2).  This eliminates
        // the one-frame lag where scroll_before was based on stale
        // cached_total_rows from the previous frame.
        let (lines, ranges, node_regions, total_rows) = {
            // Phase 1 – force layout refresh so cached_total_rows is current.
            cache.get_or_build(cache_key, &app.items, &ctx, 0, 0);
            let fresh_total = cache.cached_total_rows();
            // Phase 2 – compute the correct scroll offset *after* layout.
            let scroll_before = if app.follow_tail {
                let visible_above = document_visible_rows
                    .saturating_sub(input_overlay_rows)
                    .saturating_sub(crate::layout::INPUT_TOP_GAP as u32)
                    .max(1);
                fresh_total.saturating_sub(visible_above)
            } else {
                app.scroll_offset
            };
            cache.get_or_build(
                cache_key,
                &app.items,
                &ctx,
                scroll_before,
                effective_viewport,
            )
        };
        app.last_item_ranges = ranges;
        app.last_node_regions = node_regions;
        app.layout_cache = cache;
        app.resolve_scroll(
            total_rows,
            document_visible_rows,
            input_overlay_rows,
            app.items.len(),
        );
        let paragraph = ratatui::widgets::Paragraph::new(lines).scroll((0, 0));
        f.render_widget(paragraph, transcript_area);
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
                upper_collapsed: app.sidebar_upper_collapsed,
                lower_collapsed: app.sidebar_lower_collapsed,
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
        app.task_panel_collapsed,
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
    if let Some(area) = approvals_rect {
        sanitize_widget_edges(f, area);
        f.render_widget(ratatui::widgets::Clear, area);
        approval_bar::render(f, area, &app.pending_approvals);
    }
    // Render injection queue above approvals bar / input box.
    let injections_rect = if injection_rows > 0 {
        layout::compute_injection_rect(l.transcript, input_rect, approvals_rect, injection_rows)
    } else {
        None
    };
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
    f.render_widget(
        input_paragraph(
            editor.buf(),
            editor.cursor(),
            border_color,
            app.pending_below_rows().min(u16::MAX as u32) as u16,
            scroll_row.min(u16::MAX as u32) as u16,
            &app.trust,
        ),
        input_rect,
    );
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
    if !intro_active && !app.onboarding_open && !app.provider_manager.open {
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
    if startup_active && !app.onboarding_open && !app.hints_dismissed {
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
    if intro_progress >= 1.0 && app.startup_intro.is_some() {
        app.startup_intro = None;
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

pub(crate) fn render_trust_mode_picker(
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    app: &AppState,
) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Clear, List, ListItem, ListState};

    let modes = atman_runtime::trust::TrustMode::all();
    let t = crate::theme::theme();
    let items: Vec<ListItem> = modes
        .iter()
        .map(|&m| {
            let d = app.trust.theme.display(m);
            let color = match d.color {
                atman_runtime::trust::ModeColor::Cyan => t.accent.into(),
                atman_runtime::trust::ModeColor::Green => t.success.into(),
                atman_runtime::trust::ModeColor::Yellow => t.warn.into(),
                atman_runtime::trust::ModeColor::Orange => ratatui::style::Color::Rgb(208, 135, 22),
                atman_runtime::trust::ModeColor::Red => t.error.into(),
            };
            let marker = if m == app.trust.mode {
                "← current"
            } else {
                ""
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!(" {} ", d.emoji), Style::default().fg(color)),
                Span::styled(
                    format!("{:<14}", d.name),
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {}  ", d.description)),
                Span::raw(marker),
            ]))
        })
        .collect();

    let h = items.len() as u16 + 4;
    let w = 70u16.min(area.width);
    let popup = ratatui::layout::Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    let inner = crate::wm::shell::render_overlay_shell(
        f,
        popup,
        Line::from(Span::styled(
            "Trust Mode",
            Style::default().fg(t.tinted_fg.into()),
        )),
        "⚡",
        t.accent.into(),
        true,
        &t,
    );
    let mut state = ListState::default();
    state.select(Some(app.picker_selected.min(items.len() - 1)));
    f.render_stateful_widget(
        List::new(items).highlight_style(
            Style::default()
                .bg(t.highlight_bg.into())
                .add_modifier(Modifier::BOLD),
        ),
        inner,
        &mut state,
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

pub(crate) fn render_theme_picker(
    f: &mut ratatui::Frame,
    area: ratatui::layout::Rect,
    app: &AppState,
) {
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Clear, List, ListItem, ListState};

    let t = crate::theme::theme();
    let themes = [
        ("default", "calm / steady / eager / reckless"),
        ("wuxia", "守拙 / 行云 / 破竹 / 逍遥"),
        ("animal", "🦔 hedgehog / 🐱 cat / 🐶 dog / 🦡 honey-badger"),
        ("weather", "🌧 drizzle / ☀️ clear / ⛈ storm / 🌪 tornado"),
        ("drink", "💧 water / ☕ coffee / ☕ espresso / 🧪 bleach"),
    ];
    let items: Vec<ListItem> = themes
        .iter()
        .map(|(id, desc)| {
            let is_current = app.trust.theme.to_string() == *id;
            let marker = if is_current { "  ← current" } else { "" };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!(" {:<10}", id),
                    Style::default().add_modifier(Modifier::BOLD),
                ),
                Span::raw(format!("  {}{}", desc, marker)),
            ]))
        })
        .collect();

    let h = items.len() as u16 + 4;
    let w = 70u16.min(area.width);
    let popup = ratatui::layout::Rect {
        x: area.x + (area.width - w) / 2,
        y: area.y + (area.height - h) / 2,
        width: w,
        height: h,
    };
    f.render_widget(Clear, popup);
    let inner = crate::wm::shell::render_overlay_shell(
        f,
        popup,
        Line::from(Span::styled(
            "Theme",
            Style::default().fg(t.tinted_fg.into()),
        )),
        "◐",
        t.accent.into(),
        true,
        &t,
    );
    let mut state = ListState::default();
    state.select(Some(app.picker_selected.min(items.len() - 1)));
    f.render_stateful_widget(
        List::new(items).highlight_style(
            Style::default()
                .fg(t.tinted_fg.into())
                .add_modifier(Modifier::BOLD),
        ),
        inner,
        &mut state,
    );
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
