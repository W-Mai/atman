use crate::UiState;
use std::io::Stdout;

use anyhow::Result;
use crossterm::event::{Event as CtEvent, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::ReaderGuard;
use crate::app;
use crate::app::AppState;
use crate::input::{InputEditor, cursor_from_wrapped};
use crate::key_handler;
use crate::keys::map as map_key;
use crate::layout;
use crate::render::{ANIMATION_TICK_MS, check_latest_release, rect_contains, render_frame};
use crate::{
    TuiCommand, TuiControl, TuiHandle, TuiNote, build_sigterm_stream, spawn_event_reader,
    wait_sigterm,
};
use atman_runtime::stream::StreamFrame;

pub(crate) async fn run_frames(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut handle: TuiHandle,
) -> Result<()> {
    let transcript_bookmark_tx = handle.transcript_bookmark_tx.take();
    let mut daemon_state = handle
        .daemon_state_rx
        .as_ref()
        .map(|rx| rx.borrow().clone());
    let daemon_projection = daemon_state
        .as_ref()
        .map(|state| {
            crate::projection_adapter::TuiSessionProjection::try_from_state(state, None, None)
        })
        .transpose()?;
    let ordered_daemon_updates = handle.daemon_updates_rx.is_some();
    if let Some(projected) = daemon_projection.as_ref() {
        handle.session_name = projected.session_name.clone();
        handle.project_root = projected.project_root.clone();
        handle.goal = projected.goal.clone();
    }
    let mut app = AppState::new(handle.session_id.clone(), handle.goal.clone())
        .with_initial_items(std::mem::take(&mut handle.initial_items))
        .with_session_dir(handle.session_dir.clone())
        .with_session_identity(handle.session_name.clone(), handle.project_root.clone())
        .with_flow_names(std::mem::take(&mut handle.flow_names))
        .with_session(handle.session.clone())
        .with_trust(handle.trust.clone());
    if let Some(tr) = handle.task_registry.take() {
        app = app.with_task_registry(tr);
    }
    let mut app = UiState::new(app);
    let ui_state = crate::states::PersistedUiState::load();
    ui_state.apply(&mut app.app);
    if let Some(projected) = daemon_projection {
        apply_daemon_projection(&mut app, projected);
    }
    if let Some(bookmark) = handle.initial_transcript_bookmark.take() {
        app.app.restore_transcript_bookmark(bookmark);
    }
    if handle.onboarding_recommended && !app.app.onboarding_skipped {
        app.wm.modals.onboarding_open = true;
        if let Some(tx) = handle.control_tx.as_ref() {
            let _ = tx.send(TuiControl::OnboardingInit);
        }
    }
    app.app.startup_intro = handle.startup_intro.take();
    // Reset started_at so the 300ms fade begins now, not when the
    // switch was requested (which may have been seconds ago).
    if let Some(ref mut intro) = app.app.startup_intro {
        intro.started_at = std::time::Instant::now();
    }
    // Carry boot toasts into the live app so they persist seamlessly.
    if !handle.boot_toasts.is_empty() {
        for toast in std::mem::take(&mut handle.boot_toasts) {
            app.app
                .push_toast(toast.message, toast.level, toast.ttl, toast.position);
        }
    }
    if let Some(rx) = handle.context_rx.as_ref() {
        app.app.replace_context_snapshot(rx.borrow().clone());
    }
    if app.app.reconcile_input_reasoning() {
        app.app.save_ui_state();
    }
    if let Some(rx) = handle.goal_rx.as_ref() {
        app.app.goal = rx.borrow().clone();
    }
    if let Some(rx) = handle.attach_rx.as_ref() {
        app.app.attach_count = *rx.borrow();
    }
    if let Some(rx) = handle.todos_rx.as_ref() {
        app.app.todos = rx.borrow().clone();
    }
    if let Some(rx) = handle.plans_rx.as_ref() {
        app.app.plans = rx.borrow().clone();
    }
    if let Some(rx) = handle.form_rx.as_ref() {
        app.wm.modals.form_modal.reconcile(&rx.borrow());
    }
    if let Some(rx) = handle.compact_review_rx.as_ref() {
        crate::compact_review_modal::CompactReviewModal::reconcile(
            &mut app.wm.modals.compact_review,
            &rx.borrow(),
        );
    }
    if let Some(rx) = handle.trust_rx.as_ref() {
        let theme = app.app.trust.theme;
        app.app.trust = rx.borrow().clone();
        app.app.trust.theme = theme;
    }
    let mut editor = InputEditor::default();
    if let Some(sess) = handle.session.as_ref() {
        let past: Vec<String> = sess
            .messages_full()
            .iter()
            .filter(|m| {
                matches!(m.role, atman_runtime::message::MessageRole::User)
                    && matches!(m.origin, atman_runtime::message::MessageOrigin::User)
            })
            .map(|m| m.text_concat())
            .filter(|s| !s.trim().is_empty())
            .collect();
        editor.seed_history(past);
        editor.reconcile_images(&sess.pending_images());
    }
    let (mut key_events, reader_shutdown) = spawn_event_reader();
    // Flush stale input events accumulated during boot animation.
    // Without this, mouse events from the boot phase flood the first
    // render frame and cause a visible stutter.
    while key_events.try_recv().is_ok() {}
    let mut interrupt_prompt: Option<std::time::Instant> = None;
    let mut shutdown = handle.shutdown_rx.take();
    let mut sigterm = build_sigterm_stream();
    let mut animation_tick = tokio::time::interval(std::time::Duration::from_millis(100));
    let mut intro_tick = tokio::time::interval(std::time::Duration::from_millis(ANIMATION_TICK_MS));
    animation_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Without Skip the interval bursts every missed tick when it wakes,
    // so an idle timer that sat unpolled for 3 s while the user read
    // the splash would fire ~180 times in a row and the whole slide
    // would blow past in one frame.
    intro_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut toast_tick = tokio::time::interval(std::time::Duration::from_millis(100));
    toast_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let _reader_guard = ReaderGuard(reader_shutdown);
    let mut update_check = tokio::spawn(check_latest_release());

    // Fan-in merge: session broadcast (session-level frames) + each FlowRun's
    // frame_tx (per-FlowRun frames). Forwarders are spawned on SubAgentStarted
    // / root FlowStart so the TUI receives every frame through one channel.
    let (merge_tx, mut merge_rx) = tokio::sync::mpsc::unbounded_channel::<StreamFrame>();
    if let Some(mut srx) = handle.stream_rx.take() {
        let fwd_tx = merge_tx.clone();
        tokio::spawn(async move {
            while let Ok(f) = srx.recv().await {
                let _ = fwd_tx.send(f);
            }
        });
    }
    let frame_merge_tx = merge_tx.clone();
    let session_for_sub = handle.session.clone();

    loop {
        app.app.tick_toasts();
        terminal.draw(|f| render_frame(f, &mut app, &editor))?;
        if let Some(kind) = app.wm.top_kind() {
            if app.wm.modals.cursor_visible(kind) {
                terminal.show_cursor()?;
            } else {
                terminal.hide_cursor()?;
            }
        } else {
            terminal.show_cursor()?;
        }
        app.app.tick = app.app.tick.wrapping_add(1);

        if app.app.should_quit {
            break;
        }

        tokio::select! {
            biased;
            _ = wait_shutdown(shutdown.as_mut()) => {
                break;
            }
            _ = wait_sigterm(sigterm.as_mut()) => {
                break;
            }
            _ = animation_tick.tick(), if app.app.has_active_animation() => {
                app.app.animation_frame = app.app.animation_frame.wrapping_add(1);
            }
            _ = intro_tick.tick(), if app.app.startup_intro.is_some() => {
                app.app.animation_frame = app.app.animation_frame.wrapping_add(1);
            }
            _ = toast_tick.tick(), if !app.app.toasts.is_empty() => {}
            latest = poll_update_check(&mut update_check), if !update_check.is_finished() => {
                app.app.latest_release = latest;
            }

            key = key_events.recv() => {
                if std::env::var_os("ATMAN_TRACE_EVENTS").is_some() {
                    atman_runtime::notify!(debug, "event: {key:?}");
                }
                let mut current = key;
                let mut scroll_delta: i32 = 0;
                let mut drained: u32 = 0;
                loop {
                    match current {
                        Some(Ok(CtEvent::Mouse(me)))
                            if app.wm.modals.history_search.open =>
                        {
                            match me.kind {
                                MouseEventKind::ScrollUp => {
                                    if let Some(crate::history_search_modal::HistoryArea::Preview) =
                                        app.wm.modals.history_search.hit_test(me.column, me.row)
                                    {
                                        app.wm.modals.history_search.scroll_preview(true, 3);
                                    } else {
                                        app.wm.modals.history_search.move_up();
                                        crate::history_search_modal::refresh_history_preview(&mut app);
                                    }
                                }
                                MouseEventKind::ScrollDown => {
                                    if let Some(crate::history_search_modal::HistoryArea::Preview) =
                                        app.wm.modals.history_search.hit_test(me.column, me.row)
                                    {
                                        app.wm.modals.history_search.scroll_preview(false, 3);
                                    } else {
                                        app.wm.modals.history_search.move_down();
                                        crate::history_search_modal::refresh_history_preview(&mut app);
                                    }
                                }
                                MouseEventKind::Down(MouseButton::Left) => {
                                    if let Some(idx) =
                                        app.wm.modals.history_search.click_result(me.column, me.row)
                                    {
                                        if app.wm.modals.history_search.selected != idx {
                                            app.wm.modals.history_search.selected = idx;
                                            app.wm.modals.history_search.preview_scroll = 0;
                                            crate::history_search_modal::refresh_history_preview(&mut app);
                                        }
                                    }
                                }
                                _ => {}
                            }
                            interrupt_prompt = None;
                        }
                        Some(Ok(CtEvent::Mouse(me)))
                            if matches!(
                                me.kind,
                                MouseEventKind::ScrollUp
                                    | MouseEventKind::ScrollDown
                                    | MouseEventKind::ScrollLeft
                                    | MouseEventKind::ScrollRight
                            ) =>
                        {
                            let (consumed, commands) = app.wm.dispatch_mouse(
                                &me,
                                &mut app.app,
                                handle.control_tx.as_ref(),
                            );
                            app.wm.apply_commands(
                                &mut app.app,
                                commands,
                                handle.control_tx.as_ref(),
                            );
                            if !consumed {
                            let over_input = app
                                .input_rect
                                .map(|r| rect_contains(r, me.column, me.row))
                                .unwrap_or(false);
                            let over_sidebar = app
                                .last_sidebar_rect
                                .map(|r| rect_contains(r, me.column, me.row))
                                .unwrap_or(false);
                            if over_input {
                                let cw = if let Some(r) = app.app.input_rect {
                                    r.width.saturating_sub(layout::INPUT_H_OVERHEAD) as usize
                                } else {
                                    80
                                };
                                for _ in 0..3 {
                                    if matches!(me.kind, MouseEventKind::ScrollUp) {
                                        if !editor.move_line_up_visual(cw) {
                                            break;
                                        }
                                    } else if !editor.move_line_down_visual(cw) {
                                        break;
                                    }
                                }
                            } else if over_sidebar {
                                let over_goal = app.app.last_goal_rect.map(|r| rect_contains(r, me.column, me.row)).unwrap_or(false);
                                let over_plan = app.app.last_plan_rect.map(|r| rect_contains(r, me.column, me.row)).unwrap_or(false);
                                let over_todo = app.app.last_todo_rect.map(|r| rect_contains(r, me.column, me.row)).unwrap_or(false);
                                let up = matches!(me.kind, MouseEventKind::ScrollUp);
                                if over_goal {
                                    if up { app.app.goal_scroll = app.app.goal_scroll.saturating_sub(1); }
                                    else { app.app.goal_scroll = app.app.goal_scroll.saturating_add(1); }
                                } else if over_plan {
                                    if up { app.app.plans_scroll = app.app.plans_scroll.saturating_sub(1); }
                                    else { app.app.plans_scroll = app.app.plans_scroll.saturating_add(1); }
                                } else if over_todo {
                                    if up { app.app.todos_scroll = app.app.todos_scroll.saturating_sub(1); }
                                    else { app.app.todos_scroll = app.app.todos_scroll.saturating_add(1); }
                                }
                            } else if matches!(me.kind, MouseEventKind::ScrollUp) {
                                scroll_delta = scroll_delta.saturating_sub(3);
                            } else if matches!(me.kind, MouseEventKind::ScrollDown) {
                                scroll_delta = scroll_delta.saturating_add(3);
                            }
                            }
                            interrupt_prompt = None;
                        }
                        Some(Ok(CtEvent::Key(ke)))
                            if matches!(ke.kind, crossterm::event::KeyEventKind::Press) =>
                        {
                            let action = map_key(ke);
                            let (consumed, commands) = app.wm.dispatch_key(
                                &action,
                                &mut app.app,
                                handle.control_tx.as_ref(),
                            );
                            app.wm.apply_commands(
                                &mut app.app,
                                commands,
                                handle.control_tx.as_ref(),
                            );
                            if !consumed {
                                key_handler::handle_key(
                                    action,
                                    &mut app,
                                    &mut editor,
                                    &mut interrupt_prompt,
                                    handle.submit_tx.as_ref(),
                                    handle.control_tx.as_ref(),
                                );
                            }
                        }
                        Some(Ok(CtEvent::Paste(s))) => {
                            if app.wm.modals.any_open() {
                                app.wm.modals.dispatch_paste(&s, &mut app.app, handle.control_tx.as_ref());
                            } else {
                                editor.ingest_paste(&s);
                                interrupt_prompt = None;
                                app.app.refresh_popup(editor.buf());
                            }
                        }
                        Some(Ok(CtEvent::Mouse(me))) => {
                            let (consumed, commands) = app.wm.dispatch_mouse(
                                &me,
                                &mut app.app,
                                handle.control_tx.as_ref(),
                            );
                            app.wm.apply_commands(
                                &mut app.app,
                                commands,
                                handle.control_tx.as_ref(),
                            );
                            if !consumed {
                            // Check floating panels/modals BEFORE input_rect so clicks
                            // on overlapping panels don't pass through to the input box.
                            if let MouseEventKind::Down(MouseButton::Left) = me.kind
                                && !(app.wm.hit_test_panel(me.column, me.row).is_some()
                                    || app.wm.modals.form_modal.open
                                    || app.wm.modals.compact_review.is_some()
                                    || app.wm.modals.session_switcher.open
                                    || app.wm.modals.history_search.open
                                    || app.wm.modals.provider_manager.open
                                    || app.wm.modals.alias_manager.open
                                    || app.wm.modals.model_picker.open
                                    || app.wm.modals.onboarding_open
                                    || app.wm.modals.palette.open
                                    || app.wm.modals.theme_picker_open)
                                && let Some(rect) = app.app.input_rect
                                && rect_contains(rect, me.column, me.row)
                            {
                                let inner_x = me.column.saturating_sub(rect.x + layout::INPUT_LEFT);
                                let inner_y = me.row.saturating_sub(rect.y + 1);
                                let cw = (rect.width.saturating_sub(layout::INPUT_H_OVERHEAD)) as usize;
                                let pos = cursor_from_wrapped(
                                    editor.buf(),
                                    inner_y as usize,
                                    inner_x as usize,
                                    cw,
                                );
                                editor.set_cursor(pos);
                            } else if let MouseEventKind::Down(MouseButton::Left) = me.kind {
                                let topmost = app.wm
                                    .hit_test_panel(me.column, me.row)
                                    .map(|p| (p.id, p.rect));
                                if let Some((panel_id, pr)) = topmost {
                                if let Some(min_id) =
                                    app.wm.hit_test_minimize(me.column, me.row)
                                {
                                    app.wm.close(min_id);
                                } else if let Some(max_id) =
                                    app.wm.hit_test_maximize(me.column, me.row)
                                {
                                    let canvas = app.app.maximized_canvas();
                                    app.wm.toggle_maximize(max_id, canvas);
                                    if let Some(p) = app.wm.panels.iter().find(|p| p.id == max_id) {
                                        if matches!(p.content_kind, crate::wm::WindowContent::Task { kind: atman_runtime::TaskKind::Terminal, .. }) {
                                            let inner_cols = p.rect.width.saturating_sub(8);
                                            let inner_rows = p.rect.height.saturating_sub(5);
                                            if inner_cols > 0 && inner_rows > 0 {
                                                let terminal_handle = app.wm
                                                    .content_kind(max_id)
                                                    .and_then(|kind| match kind {
                                                        crate::wm::WindowContent::Task { handle, .. } => Some(handle),
                                                        _ => None,
                                                    });
                                                if let (Some(tx), Some(resource_id)) = (
                                                    &handle.control_tx,
                                                    terminal_handle.and_then(|handle| app.app.task_resource_id(handle)),
                                                ) {
                                                    let _ = tx.send(TuiControl::Domain(crate::TuiDomainCommand::TermResize {
                                                        resource_id,
                                                        rows: inner_rows,
                                                        cols: inner_cols,
                                                    }));
                                                }
                                            }
                                        }
                                    }
                                } else if let Some(close_id) =
                                    app.wm.hit_test_close(me.column, me.row)
                                {
                                    let close_handle = app.wm.content_kind(close_id).and_then(|kind| {
                                        match kind {
                                            crate::wm::WindowContent::Task { handle, .. } => {
                                                Some(handle.clone())
                                            }
                                            _ => None,
                                        }
                                    });
                                    if let (Some(registry), Some(handle)) =
                                        (&app.app.task_registry, close_handle)
                                        && let Some(task) = registry.lookup_by_handle_in_session(
                                            &handle,
                                            &app.app.session_id,
                                        )
                                        && task.is_running()
                                    {
                                        let armed = app
                                            .wm
                                            .interaction
                                            .panel_close_armed_id
                                            .as_deref()
                                            == Some(handle.as_str())
                                            && !app.wm.panel_close_arm_expired();
                                        if armed {
                                            let _ = registry.kill_by_handle_from_operator(
                                                &handle,
                                                &app.app.session_id,
                                            );
                                            app.wm.clear_panel_close_arm();
                                        } else {
                                            app.wm.arm_panel_close(handle);
                                            app.app.push_note(
                                                format!(
                                                    "press ✕ again to kill {}",
                                                    task.label
                                                ),
                                                app::NoteLevel::Warn,
                                            );
                                        }
                                    }
                                } else if let Some(resize_id) =
                                    app.wm.hit_test_resize(me.column, me.row)
                                {
                                    app.wm.focus(resize_id);
                                    app.wm.interaction.resize_target = Some(resize_id);
                                    app.wm.interaction.resize_offset = (me.column, me.row);
                                } else if let Some(fp) = app.wm
                                    .hit_test_titlebar(me.column, me.row)
                                {
                                    let id = fp.id;
                                    let now = std::time::Instant::now();
                                    let is_double = app.wm.interaction.last_titlebar_click
                                        .as_ref()
                                        .is_some_and(|(prev_id, ts)| {
                                            *prev_id == id && now.duration_since(*ts).as_millis() < 400
                                        });
                                    if is_double {
                                        let canvas = app.app.last_transcript_rect.unwrap_or_default();
                                        app.wm.toggle_maximize(id, canvas);
                                        app.wm.interaction.last_titlebar_click = None;
                                    } else {
                                        app.wm.focus(id);
                                        app.wm.interaction.drag_target = Some(id);
                                        app.wm.interaction.drag_offset = (me.column, me.row);
                                        app.wm.interaction.last_titlebar_click = Some((id, now));
                                    }
                                } else {
                                    let history_hit = app.wm.interaction.last_hitmap
                                        .history_row_rects
                                        .iter()
                                        .find(|(pid, _, r)| {
                                            pid == &panel_id
                                                && rect_contains(*r, me.column, me.row)
                                                && rect_contains(pr, r.x, r.y)
                                        })
                                        .map(|(_, h, _)| h.clone());
                                    let mcp_hit = app.wm.interaction.last_hitmap
                                        .mcp_row_rects
                                        .iter()
                                        .find(|(pid, _, r)| {
                                            pid == &panel_id
                                                && rect_contains(*r, me.column, me.row)
                                                && rect_contains(pr, r.x, r.y)
                                        })
                                        .map(|(_, n, _)| n.clone());
                                    let wf_hit = app.wm.interaction.last_hitmap
                                        .workflow_node_rects
                                        .iter()
                                        .find(|(pid, _, _, r)| {
                                            pid == &panel_id
                                                && rect_contains(*r, me.column, me.row)
                                                && rect_contains(pr, r.x, r.y)
                                        })
                                        .map(|(_, i, p, _)| (*i, p.clone()));
                                    if let Some(handle) = history_hit {
                                        let canvas =
                                            app.app.last_transcript_rect.unwrap_or_default();
                                        app.open_task_panel(&handle, canvas);
                                    } else if let Some(name) = mcp_hit {
                                        if !app.app.expanded_mcp_servers.remove(&name) {
                                            app.app.expanded_mcp_servers.insert(name);
                                        }
                                    } else if let Some((panel_idx, path)) = wf_hit {
                                        if path.is_empty() {
                                            app.app.toggle_workflow_panel_expansion(panel_idx);
                                        } else {
                                            app.app.toggle_workflow_node(panel_idx, &path);
                                        }
                                    } else if let Some((_pid, tool_id, _)) = app.wm.interaction.last_hitmap
                                        .tool_header_rects
                                        .iter()
                                        .find(|(pid, _, r)| {
                                            pid == &panel_id
                                                && rect_contains(*r, me.column, me.row)
                                        })
                                        .cloned()
                                    {
                                        if let Some(panel) = app
                                            .wm
                                            .panels
                                            .iter_mut()
                                            .find(|panel| panel.id == panel_id)
                                        {
                                            if !panel.expanded_tools.remove(&tool_id) {
                                                panel.expanded_tools.insert(tool_id);
                                            }
                                            panel.interaction_revision =
                                                panel.interaction_revision.wrapping_add(1);
                                        }
                                    } else {
                                        app.wm.focus(panel_id);
                                    }
                                }
                            } else if !app.wm.layers.modal_stack.is_empty() {
                                // Modal is open — swallow the click (blocks base layer).
                            } else {
                                if let Some(r) = app.app.last_upper_title_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    if app.app.sidebar_upper_runtime_collapsed {
                                        app.app.sidebar_upper_runtime_collapsed = false;
                                    } else {
                                        app.app.sidebar_upper_collapsed =
                                            !app.app.sidebar_upper_collapsed;
                                    }
                                } else if let Some(r) = app.app.last_lower_title_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    if app.app.sidebar_lower_runtime_collapsed {
                                        app.app.sidebar_lower_runtime_collapsed = false;
                                    } else {
                                        app.app.sidebar_lower_collapsed =
                                            !app.app.sidebar_lower_collapsed;
                                    }
                                } else if let Some(r) = app.app.last_sidebar_more_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    let canvas = app.app.last_transcript_rect.unwrap_or_default();
                                    app.wm.open(
                                        "mcp-manager",
                                        crate::wm::ContentKey::Mcp,
                                        crate::wm::WindowContent::Mcp,
                                        "MCP Servers",
                                        canvas,
                                    );
                                    if let Some(p) =
                                        app.wm.panels.iter_mut().find(|p| p.content_key == crate::wm::ContentKey::Mcp)
                                    {
                                        p.content = Some(Box::new(
                                            crate::window::mcp_panel::McpPanelContent::default(),
                                        ));
                                    }
                                } else if let Some(r) = app.app.last_goal_hdr_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.app.goal_collapsed = !app.app.goal_collapsed;
                                    app.app.save_ui_state();
                                } else if let Some(r) = app.app.last_plan_hdr_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.app.plan_collapsed = !app.app.plan_collapsed;
                                    app.app.save_ui_state();
                                } else if let Some(r) = app.app.last_todo_hdr_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.app.todo_collapsed = !app.app.todo_collapsed;
                                    app.app.save_ui_state();
                                } else if app.app.sidebar_popup.is_some()
                                    && app
                                        .last_sidebar_popup_rect
                                        .map(|r| !rect_contains(r, me.column, me.row))
                                        .unwrap_or(true)
                                {
                                    app.app.sidebar_popup = None;
                                } else if let Some((key, _)) = app
                                    .last_sidebar_strip_rects
                                    .iter()
                                    .find(|(_, r)| rect_contains(**r, me.column, me.row))
                                {
                                    if let Some(idx) = key
                                        .strip_prefix("plan:")
                                        .and_then(|s| s.parse::<usize>().ok())
                                    {
                                        app.app.sidebar_popup =
                                            Some(crate::sidebar::SidebarPopupKind::Plan(idx));
                                    } else if let Some(idx) = key
                                        .strip_prefix("todo:")
                                        .and_then(|s| s.parse::<usize>().ok())
                                    {
                                        app.app.sidebar_popup =
                                            Some(crate::sidebar::SidebarPopupKind::Todo(idx));
                                    }
                                } else if let Some(r) = app.app.last_ctx_hdr_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.app.context_collapsed = !app.app.context_collapsed;
                                    app.app.save_ui_state();
                                } else if let Some(r) = app.app.last_meta_hdr_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.app.meta_collapsed = !app.app.meta_collapsed;
                                    app.app.save_ui_state();
                                } else if let Some(r) = app.app.last_mcp_hdr_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.app.mcp_collapsed = !app.app.mcp_collapsed;
                                    app.app.save_ui_state();
                                } else if let Some(r) = app.app.last_collapse_btn_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.app.sidebar_collapsed = true;
                                    app.app.sidebar_upper_runtime_collapsed = false;
                                    app.app.save_ui_state();
                                } else if let Some(r) = app.app.last_expand_btn_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.app.sidebar_collapsed = false;
                                    app.app.sidebar_upper_runtime_collapsed = false;
                                    app.app.save_ui_state();
                                } else if let Some(r) = app.app.last_task_panel_rect
                                    && me.column >= r.x
                                    && me.column < r.x + 10
                                    && me.row == r.y + 1
                                {
                                    if app.app.task_panel_runtime_collapsed {
                                        app.app.task_panel_runtime_collapsed = false;
                                    } else {
                                        app.app.task_panel_collapsed =
                                            !app.app.task_panel_collapsed;
                                        app.app.task_panel_runtime_collapsed = false;
                                        app.app.save_ui_state();
                                    }
                                } else if let Some(r) = app.app.last_task_panel_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    let hm = &app.app.last_task_panel_hitmap;
                                    let hit_kill = hm
                                        .kill_rects
                                        .iter()
                                        .find(|(_, kr)| rect_contains(*kr, me.column, me.row))
                                        .map(|(t, _)| t.clone());
                                    if let Some(tid) = hit_kill {
                                        if app.app.kill_armed_id == Some(tid.clone())
                                            && !app.app.kill_arm_expired()
                                        {
                                            if let Some(tr) = &app.app.task_registry {
                                                let _ = tr.kill_from_operator(&tid);
                                            }
                                            app.app.clear_kill_arm();
                                        } else {
                                            let label = app
                                                .task_snapshots
                                                .iter()
                                                .find(|s| s.id == tid)
                                                .map(|s| s.label.clone())
                                                .unwrap_or_default();
                                            app.app.arm_kill(tid.clone());
                                            app.app.push_note(
                                                format!("press ✕ again to kill {label}"),
                                                app::NoteLevel::Warn,
                                            );
                                        }
                                    } else if let Some((handle, _)) = hm
                                        .insert_rects
                                        .iter()
                                        .find(|(_, ir)| rect_contains(*ir, me.column, me.row))
                                    {
                                        editor.prefill(&format!("{handle} "));
                                        app.app.refresh_popup(editor.buf());
                                        app.app.clear_kill_arm();
                                    } else if let Some((kind, _)) = hm
                                        .group_header_rects
                                        .iter()
                                        .find(|(_, hr)| rect_contains(*hr, me.column, me.row))
                                    {
                                        if app.app.task_panel_collapsed_groups.contains(kind) {
                                            app.app.task_panel_collapsed_groups.remove(kind);
                                        } else {
                                            app.app.task_panel_collapsed_groups.insert(*kind);
                                        }
                                    } else if let Some(hr) = &hm.history_btn_rect
                                        && rect_contains(*hr, me.column, me.row)
                                    {
                                        let canvas = app.app.last_transcript_rect.unwrap_or_default();
                                        let (pw, ph) = app
                                            .panel_sizes
                                            .get("__history__")
                                            .copied()
                                            .unwrap_or((0, 0));
                                        app.wm.open_with_size(
                                            "__history__",
                                            crate::wm::ContentKey::History,
                                            crate::wm::OpenPolicy::ReuseExisting,
                                            crate::wm::WindowContent::History,
                                            "History",
                                            canvas,
                                            pw,
                                            ph,
                                            false,
                                        );
                                        if let Some(p) =
                                            app.wm.panels.iter_mut().find(|p| p.content_key == crate::wm::ContentKey::History)
                                        {
                                            p.content = Some(Box::new(
                                                crate::window::history_panel::HistoryPanelContent::default(),
                                            ));
                                        }
                                    } else if let Some((run_id, node_id, _)) = hm
                                        .activity_rects
                                        .iter()
                                        .find(|(_, _, ar)| rect_contains(*ar, me.column, me.row))
                                        .map(|(r, n, a)| (r, n, a))
                                    {
                                        let canvas = app.app.last_transcript_rect.unwrap_or_default();
                                        let panel_id = format!("{run_id}:{node_id}");
                                        let label = app
                                            .app
                                            .activity_nodes
                                            .iter()
                                            .find(|n| n.run_id == *run_id && n.node_id == *node_id)
                                            .map(|n| n.label.clone());
                                        if let Some(label) = label {
                                            app.wm.open(
                                                &panel_id,
                                                crate::wm::ContentKey::Activity(panel_id.clone()),
                                                crate::wm::WindowContent::Activity { run_id: run_id.clone() },
                                                &label,
                                                canvas,
                                            );
                                            if let Some(p) = app.wm
                                                .panels
                                                .iter_mut()
                                                .find(|p| p.content_key == crate::wm::ContentKey::Activity(panel_id.clone()))
                                            {
                                                p.content = Some(Box::new(
                                                    crate::window::activity_panel::ActivityPanelContent {
                                                        run_id: panel_id.clone(),
                                                        scroll: 0,
                                                    },
                                                ));
                                            }
                                        }
                                    } else {
                                        let hit_task = hm
                                            .task_rects
                                            .iter()
                                            .find(|(_, tr)| rect_contains(*tr, me.column, me.row))
                                            .map(|(h, _)| h.clone());
                                        let hit_header = hm
                                            .task_header_rects
                                            .iter()
                                            .find(|(_, r)| rect_contains(*r, me.column, me.row))
                                            .map(|(h, _)| h.clone());
                                        if let Some(handle) = hit_task {
                                            app.app.clear_kill_arm();
                                            let is_header = hit_header.as_deref() == Some(&handle);
                                            let is_expanded =
                                                app.app.expanded_tasks.contains(&handle);
                                            if is_header && is_expanded {
                                                app.app.expanded_tasks.remove(&handle);
                                            } else if is_header && !is_expanded {
                                                app.app.expanded_tasks.insert(handle.clone());
                                            } else {
                                                let canvas =
                                                    app.app.last_transcript_rect.unwrap_or_default();
                                                app.open_task_panel(&handle, canvas);
                                            }
                                        }
                                    }
                                } else if let Some((panel_idx, node_id)) =
                                    app.app.hit_test_node(me.column, me.row)
                                {
                                    if let Some(tool_use_id) = node_id
                                        .strip_prefix(crate::output::TOOL_CALL_REGION_PREFIX)
                                    {
                                        if handle.daemon_state_rx.is_some()
                                            && let Some(tx) = handle.control_tx.as_ref()
                                        {
                                            let _ = tx.send(TuiControl::LoadToolDetail {
                                                tool_use_id: tool_use_id.to_owned(),
                                            });
                                        }
                                        app.app.cycle_tool_call_disclosure(
                                            panel_idx,
                                            tool_use_id,
                                        );
                                    } else if let Some(tool_use_id) = node_id
                                        .strip_prefix(crate::output::TOOL_FULLSCREEN_REGION_PREFIX)
                                        .or_else(|| {
                                            node_id.strip_prefix(
                                                crate::output::TOOL_DETAIL_FULLSCREEN_REGION_PREFIX,
                                            )
                                        })
                                    {
                                        if handle.daemon_state_rx.is_some()
                                            && let Some(tx) = handle.control_tx.as_ref()
                                        {
                                            let _ = tx.send(TuiControl::LoadToolDetail {
                                                tool_use_id: tool_use_id.to_owned(),
                                            });
                                        }
                                        if let Some(handle) = app
                                            .app
                                            .tool_call_detail_handle(panel_idx, tool_use_id)
                                        {
                                            app.open_task_panel_maximized(&handle);
                                        } else if !app.open_tool_output_panel(
                                            panel_idx,
                                            tool_use_id,
                                        ) {
                                            app.app.cycle_tool_call_disclosure(
                                                panel_idx,
                                                tool_use_id,
                                            );
                                        }
                                    } else if node_id
                                        == crate::output::COLLAPSED_CARD_FULLSCREEN_KEY
                                    {
                                        if let Some(handle) =
                                            app.app.workflow_panel_task_handle(panel_idx)
                                        {
                                            app.open_task_panel_maximized(&handle);
                                        }
                                    } else if node_id
                                        == crate::output::TERMINAL_FULLSCREEN_KEY
                                    {
                                        if let Some(handle) = app.app.terminal_item_handle(panel_idx) {
                                            let canvas =
                                                app.app.last_transcript_rect.unwrap_or_default();
                                            app.open_task_panel(&handle, canvas);
                                        }
                                    } else if node_id == crate::output::BASH_FULLSCREEN_KEY
                                    {
                                        if let Some(handle) = app.app.bash_item_handle(panel_idx) {
                                            let canvas =
                                                app.app.last_transcript_rect.unwrap_or_default();
                                            app.open_task_panel(&handle, canvas);
                                        }
                                    } else if node_id
                                        == crate::output::SUB_AGENT_FULLSCREEN_KEY
                                    {
                                        if let Some(handle) =
                                            app.app.sub_agent_item_handle(panel_idx)
                                        {
                                            let canvas =
                                                app.app.last_transcript_rect.unwrap_or_default();
                                            app.open_task_panel(&handle, canvas);
                                        }
                                    } else if node_id == crate::output::MERMAID_FULLSCREEN_KEY
                                    {
                                        let canvas =
                                            app.app.last_transcript_rect.unwrap_or_default();
                                        app.open_mermaid_panel(panel_idx, canvas);
                                    } else if node_id.is_empty() {
                                        app.app.toggle_workflow_panel_expansion(panel_idx);
                                    } else {
                                        app.app.toggle_workflow_node(panel_idx, &node_id);
                                    }
                                } else if let Some(idx) = app.app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::Thinking { .. }) =
                                        app.app.items.get(idx)
                                {
                                    app.app.cycle_thinking_disclosure(idx);
                                } else if let Some(idx) = app.app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::WorkflowPanel { .. }) =
                                        app.app.items.get(idx)
                                {
                                    if me.modifiers.contains(KeyModifiers::SHIFT) {
                                        if let Some(handle) = app.app.workflow_panel_task_handle(idx) {
                                            app.open_task_panel_maximized(&handle);
                                        }
                                    } else {
                                        app.app.toggle_workflow_panel_expansion(idx);
                                    }
                                } else if let Some(idx) = app.app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::Terminal { .. }) =
                                        app.app.items.get(idx)
                                {
                                    app.app.toggle_terminal_expand(idx);
                                } else if let Some(idx) = app.app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::Bash { .. }) =
                                        app.app.items.get(idx)
                                {
                                    app.app.toggle_bash_expand(idx);
                                } else if let Some(idx) = app.app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::SubAgentActivity { .. }) =
                                        app.app.items.get(idx)
                                {
                                    app.app.toggle_sub_agent_expand(idx);
                                } else if let Some(idx) = app.app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::DiffPreview { .. }) =
                                        app.app.items.get(idx)
                                {
                                    app.app.toggle_diff_preview_expand(idx);
                                } else if let Some(idx) = app.app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::CompactionSummary { .. }) =
                                        app.app.items.get(idx)
                                {
                                    app.app.cycle_compaction_summary_disclosure(idx);
                                }
                                }
                            } else if let MouseEventKind::Drag(MouseButton::Left) = me.kind {
                                if let Some(id) = app.wm.interaction.resize_target {
                                    let (ox, oy) = app.wm.interaction.resize_offset;
                                    let dx = me.column as i32 - ox as i32;
                                    let dy = me.row as i32 - oy as i32;
                                    if dx != 0 || dy != 0 {
                                        let was_maximized = app.wm
                                            .panels
                                            .iter()
                                            .find(|p| p.id == id)
                                            .is_some_and(|p| p.maximized);
                                        if was_maximized {
                                            let canvas = app.app.last_transcript_rect.unwrap_or_default();
                                            app.wm.unmaximize(id, canvas)
                                        }
                                        if let Some(p) = app.wm
                                            .panels
                                            .iter()
                                            .find(|p| p.id == id)
                                        {
                                            let is_term = matches!(p.content_kind, crate::wm::WindowContent::Task { kind: atman_runtime::TaskKind::Terminal, .. });
                                            let nw = (p.rect.width as i32 + dx).max(20) as u16;
                                            let nh = (p.rect.height as i32 + dy).max(6) as u16;
                                            let canvas = app.app.last_transcript_rect.unwrap_or_default();
                                            app.wm.resize_panel(id, nw, nh, canvas);
                                            app.wm.interaction.resize_offset = (me.column, me.row);
                                            if is_term {
                                                let inner_cols = nw.saturating_sub(8);
                                                let inner_rows = nh.saturating_sub(5);
                                                if inner_cols > 0 && inner_rows > 0 {
                                                    let terminal_handle = app.wm
                                                        .content_kind(id)
                                                        .and_then(|kind| match kind {
                                                            crate::wm::WindowContent::Task { handle, .. } => Some(handle),
                                                            _ => None,
                                                        });
                                                    if let (Some(tx), Some(resource_id)) = (
                                                        &handle.control_tx,
                                                        terminal_handle.and_then(|handle| app.app.task_resource_id(handle)),
                                                    ) {
                                                        let _ = tx.send(TuiControl::Domain(crate::TuiDomainCommand::TermResize {
                                                            resource_id,
                                                            rows: inner_rows,
                                                            cols: inner_cols,
                                                        }));
                                                    }
                                                }
                                            }
                                        }
                                    }
                                } else if let Some(id) = app.wm.interaction.drag_target {
                                    let (ox, oy) = app.wm.interaction.drag_offset;
                                    let dx = me.column as i32 - ox as i32;
                                    let dy = me.row as i32 - oy as i32;
                                    if dx != 0 || dy != 0 {
                                        let was_maximized = app.wm
                                            .panels
                                            .iter()
                                            .find(|p| p.id == id)
                                            .is_some_and(|p| p.maximized);
                                        if was_maximized {
                                            let canvas = app.app.last_transcript_rect.unwrap_or_default();
                                            app.wm.unmaximize(id, canvas)
                                        }
                                        if let Some(p) = app.wm
                                            .panels
                                            .iter()
                                            .find(|p| p.id == id)
                                        {
                                            let nx = (p.rect.x as i32 + dx).max(0) as u16;
                                            let ny = (p.rect.y as i32 + dy).max(0) as u16;
                                            let canvas = app.app.last_transcript_rect.unwrap_or_default();
                                            app.wm.move_panel(id, nx, ny, canvas);
                                            app.wm.interaction.drag_offset = (me.column, me.row);
                                        }
                                    }
                                }
                            } else if let MouseEventKind::Up(MouseButton::Left) = me.kind {
                                if app.wm.interaction.resize_target.is_some() {
                                    let id = app.wm.interaction.resize_target;
                                    if let Some(id) = id {
                                        if let Some(p) = app.wm.panels.iter().find(|p| p.id == id) {
                                            if let Some(label) = app.wm.label(id) {
                                                app.app.panel_sizes.insert(label.to_string(), (p.rect.width, p.rect.height));
                                            }
                                            app.app.save_ui_state();
                                        }
                                    }
                                }
                                app.wm.interaction.drag_target = None;
                                app.wm.interaction.resize_target = None;
                            } else if let MouseEventKind::Moved = me.kind {
                                let skip_hover = app.app.startup_intro.is_some()
                                    || matches!(
                                        app.app.items.first(),
                                        Some(crate::app::OutputItem::StartupCard { .. })
                                    );
                                if !skip_hover {
                                let topmost_panel = app.wm
                                    .hit_test_panel(me.column, me.row)
                                    .map(|p| (p.id, p.rect));
                                if let Some((panel_id, pr)) = topmost_panel {
                                    // floating panel button hover (topmost only)
                                    let btn_hover = app.wm
                                        .hit_test_btn(me.column, me.row);
                                    if app.wm.interaction.hovered_panel_btn != btn_hover {
                                        app.wm.interaction.hovered_panel_btn = btn_hover;
                                        app.wm_visual_version = app.wm_visual_version.wrapping_add(1);
                                    }

                                    // floating panel history row hover (only in this panel)
                                    let history_hover = app.wm.interaction.last_hitmap
                                        .history_row_rects
                                        .iter()
                                        .find(|(pid, _, r)| {
                                            pid == &panel_id
                                                && rect_contains(*r, me.column, me.row)
                                                && rect_contains(pr, r.x, r.y)
                                        })
                                        .map(|(_, h, _)| h.clone());
                                    if app.wm.interaction.hovered_history_row != history_hover {
                                        app.wm.interaction.hovered_history_row = history_hover;
                                        app.app.mark_visual_dirty();
                                    }

                                    let mcp_hover = app.wm.interaction.last_hitmap
                                        .mcp_row_rects
                                        .iter()
                                        .find(|(pid, _, r)| {
                                            pid == &panel_id
                                                && rect_contains(*r, me.column, me.row)
                                                && rect_contains(pr, r.x, r.y)
                                        })
                                        .map(|(_, n, _)| n.clone());
                                    if app.wm.interaction.hovered_mcp_row != mcp_hover {
                                        app.wm.interaction.hovered_mcp_row = mcp_hover;
                                    }

                                    // Over a floating panel — base-layer hovers cleared.
                                    app.app.hovered_sidebar_row = None;
                                    app.app.hovered_sidebar_hamburger = false;
                                    app.app.hovered_sidebar_lower = false;
                                    app.app.hovered_sidebar_more = false;
                                    app.app.set_hovered_thinking(None);
                                    app.app.set_hovered_output_node(None);
                                    app.app.set_hovered_kill(None);
                                    app.app.set_hovered_task(None);
                                    app.app.set_hovered_insert(None);
                                    app.app.set_hovered_activity(None);
                                    app.app.set_hovered_history_btn(false);
                                    app.app.set_hovered_hamburger(false);
                                } else {
                                    // Not over a panel — panel hovers cleared.
                                    if app.wm.interaction.hovered_panel_btn.is_some() {
                                        app.wm.interaction.hovered_panel_btn = None;
                                        app.wm_visual_version = app.wm_visual_version.wrapping_add(1);
                                    }
                                    if app.wm.interaction.hovered_history_row.is_some() {
                                        app.wm.interaction.hovered_history_row = None;
                                        app.app.mark_visual_dirty();
                                    }
                                    if app.wm.interaction.hovered_mcp_row.is_some() {
                                        app.wm.interaction.hovered_mcp_row = None;
                                    }
                                    // Sidebar strip hover
                                let sidebar_hover = app
                                    .last_sidebar_strip_rects
                                    .iter()
                                    .find(|(_, r)| rect_contains(**r, me.column, me.row))
                                    .map(|(k, _)| k.clone());
                                if app.app.hovered_sidebar_row != sidebar_hover {
                                    app.app.hovered_sidebar_row = sidebar_hover;
                                }

                                // Sidebar hamburger hover (upper panel title)
                                let ham_hover = app
                                    .last_collapse_btn_rect
                                    .or(app.app.last_upper_title_rect)
                                    .map(|r| rect_contains(r, me.column, me.row))
                                    .unwrap_or(false);
                                if app.app.hovered_sidebar_hamburger != ham_hover {
                                    app.app.hovered_sidebar_hamburger = ham_hover;
                                }

                                // Sidebar lower panel title hover
                                let lower_hover = app
                                    .last_lower_title_rect
                                    .map(|r| rect_contains(r, me.column, me.row))
                                    .unwrap_or(false);
                                if app.app.hovered_sidebar_lower != lower_hover {
                                    app.app.hovered_sidebar_lower = lower_hover;
                                }

                                // Sidebar MCP "more" row hover
                                let more_hover = app
                                    .last_sidebar_more_rect
                                    .map(|r| rect_contains(r, me.column, me.row))
                                    .unwrap_or(false);
                                if app.app.hovered_sidebar_more != more_hover {
                                    app.app.hovered_sidebar_more = more_hover;
                                }

                                if let Some(idx) = app.app.hit_test(me.column, me.row)
                                    && let Some(crate::app::OutputItem::Thinking { .. }) =
                                        app.app.items.get(idx)
                                {
                                    app.app.set_hovered_thinking(Some(idx));
                                } else {
                                    app.app.set_hovered_thinking(None);
                                }
                                let hovered_output_node =
                                    app.app.hit_test_node(me.column, me.row);
                                app.app.set_hovered_output_node(hovered_output_node);
                                let hm = &app.app.last_task_panel_hitmap;
                                if let Some(kr) = hm
                                    .kill_rects
                                    .iter()
                                    .find(|(_, r)| rect_contains(*r, me.column, me.row))
                                {
                                    app.app.set_hovered_kill(Some(kr.0.clone()));
                                    app.app.set_hovered_task(None);
                                    app.app.set_hovered_insert(None);
                                    app.app.set_hovered_activity(None);
                                    app.app.set_hovered_history_btn(false);
                                } else if let Some(ir) = hm
                                    .insert_rects
                                    .iter()
                                    .find(|(_, r)| rect_contains(*r, me.column, me.row))
                                {
                                    app.app.set_hovered_insert(Some(ir.0.clone()));
                                    app.app.set_hovered_kill(None);
                                    app.app.set_hovered_task(None);
                                    app.app.set_hovered_activity(None);
                                    app.app.set_hovered_history_btn(false);
                                } else if let Some(tr) = hm
                                    .task_rects
                                    .iter()
                                    .find(|(_, r)| rect_contains(*r, me.column, me.row))
                                {
                                    let snap = app
                                        .task_snapshots
                                        .iter()
                                        .find(|s| s.source_handle == tr.0);
                                    app.app.set_hovered_task(snap.map(|s| s.id.clone()));
                                    app.app.set_hovered_kill(None);
                                    app.app.set_hovered_insert(None);
                                    app.app.set_hovered_activity(None);
                                    app.app.set_hovered_history_btn(false);
                                } else if let Some(ar) = hm
                                    .activity_rects
                                    .iter()
                                    .find(|(_, _, r)| rect_contains(*r, me.column, me.row))
                                {
                                    app.app.set_hovered_activity(Some((ar.0.clone(), ar.1.clone())));
                                    app.app.set_hovered_kill(None);
                                    app.app.set_hovered_task(None);
                                    app.app.set_hovered_insert(None);
                                    app.app.set_hovered_history_btn(false);
                                } else if let Some(hr) = &hm.history_btn_rect
                                    && rect_contains(*hr, me.column, me.row)
                                {
                                    app.app.set_hovered_history_btn(true);
                                    app.app.set_hovered_hamburger(false);
                                    app.app.set_hovered_kill(None);
                                    app.app.set_hovered_task(None);
                                    app.app.set_hovered_insert(None);
                                    app.app.set_hovered_activity(None);
                                } else if let Some(hr) = &hm.hamburger_rect
                                    && rect_contains(*hr, me.column, me.row)
                                {
                                    app.app.set_hovered_hamburger(true);
                                    app.app.set_hovered_history_btn(false);
                                    app.app.set_hovered_kill(None);
                                    app.app.set_hovered_task(None);
                                    app.app.set_hovered_insert(None);
                                    app.app.set_hovered_activity(None);
                                } else {
                                    app.app.set_hovered_kill(None);
                                    app.app.set_hovered_task(None);
                                    app.app.set_hovered_insert(None);
                                    app.app.set_hovered_activity(None);
                                    app.app.set_hovered_history_btn(false);
                                    app.app.set_hovered_hamburger(false);
                                }
                                }
                                } // if !skip_hover
                            }
                            }
                            interrupt_prompt = None;
                        }
                        Some(Ok(CtEvent::Resize(cols, rows))) => {
                            let canvas = ratatui::layout::Rect::new(0, 0, cols, rows);
                            app.wm.clamp_to_canvas(canvas);
                            // Keep the focused terminal panel's PTY in sync with its
                            // clamped size after the terminal resize.
                            if let Some(id) = app.wm.focused_id() {
                                if app.wm.panels.iter().find(|p| p.id == id).is_some_and(|p| {
                                    matches!(p.content_kind,
                                        crate::wm::WindowContent::Task {
                                            kind: atman_runtime::TaskKind::Terminal,
                                            ..
                                        })
                                }) {
                                    if let Some(p) = app.wm.panels.iter().find(|p| p.id == id) {
                                        let inner_cols = p.rect.width.saturating_sub(8);
                                        let inner_rows = p.rect.height.saturating_sub(5);
                                        if inner_cols > 0 && inner_rows > 0 {
                                            let terminal_handle = app.wm
                                                .content_kind(id)
                                                .and_then(|kind| match kind {
                                                    crate::wm::WindowContent::Task { handle, .. } => Some(handle),
                                                    _ => None,
                                                });
                                            if let (Some(tx), Some(resource_id)) = (
                                                &handle.control_tx,
                                                terminal_handle.and_then(|handle| app.app.task_resource_id(handle)),
                                            ) {
                                                let _ = tx.send(TuiControl::Domain(crate::TuiDomainCommand::TermResize {
                                                    resource_id,
                                                    rows: inner_rows,
                                                    cols: inner_cols,
                                                }));
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    drained = drained.saturating_add(1);
                    if drained >= 100 {
                        break;
                    }
                    match key_events.try_recv() {
                        Ok(next) => current = Some(next),
                        Err(_) => break,
                    }
                }
                if scroll_delta < 0 {
                    app.app.scroll_up((-scroll_delta) as u32);
                    if app.app.near_history_start()
                        && let Some(tx) = &handle.control_tx
                    {
                        let _ = tx.send(TuiControl::LoadOlderHistory);
                    }
                } else if scroll_delta > 0 {
                    app.app.scroll_down(scroll_delta as u32);
                    if app.app.near_history_end()
                        && let Some(tx) = &handle.control_tx
                    {
                        let _ = tx.send(TuiControl::LoadNewerHistory);
                    }
                }
            }
            frame = merge_rx.recv() => {
                if let Some(frame) = frame {
                    // Spawn per-FlowRun frame_tx forwarders when new FlowRuns appear.
                    let new_handle: Option<String> = match &frame {
                        StreamFrame::FlowStart { run_id, parent_run_id: None, .. } => {
                            Some(run_id.clone())
                        }
                        StreamFrame::SubAgentStarted { handle, .. } => Some(handle.clone()),
                        _ => None,
                    };
                    if let Some(h) = new_handle
                        && let Some(sess) = &session_for_sub
                        && let Ok(entry) = sess.flow_registry.lookup(&h)
                    {
                        let tx = frame_merge_tx.clone();
                        let mut rx = entry.frame_tx.subscribe();
                        tokio::spawn(async move {
                            while let Ok(f) = rx.recv().await {
                                let _ = tx.send(f);
                            }
                        });
                    }
                    app.app.apply_stream_frame(frame);
                    let mut drained = 0u32;
                    while drained < 256 {
                        match merge_rx.try_recv() {
                            Ok(extra) => {
                                app.app.apply_stream_frame(extra);
                                drained += 1;
                            }
                            Err(_) => break,
                        }
                    }
                } else {
                    break;
                }
            }
            ev = recv_task_event(handle.task_event_rx.as_mut()) => {
                if let Some(ev) = ev {
                    app.app.apply_task_event(ev);
                }
            }
            note = recv_note(handle.note_rx.as_mut()) => {
                if let Some(n) = note {
                    let (text, level) = n.into_parts();
                    app.app.push_note(text, level);
                }
            }
            update = recv_daemon_update(handle.daemon_updates_rx.as_mut()) => {
                match update {
                    Some(Ok(update)) => {
                        if let Err(error) = apply_daemon_update(&mut app, &mut daemon_state, update) {
                            app.app.push_note(
                                format!("daemon update rejected: {error}"),
                                app::NoteLevel::Error,
                            );
                        }
                    }
                    Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                        if let Some(rx) = handle.daemon_state_rx.as_ref() {
                            let latest = rx.borrow().clone();
                            match crate::projection_adapter::TuiSessionProjection::try_from_state(
                                &latest, None, None,
                            ) {
                                Ok(projected) => {
                                    daemon_state = Some(latest);
                                    apply_daemon_projection(&mut app, projected);
                                }
                                Err(error) => app.app.push_note(
                                    format!("daemon resync rejected: {error}"),
                                    app::NoteLevel::Error,
                                ),
                            }
                        }
                    }
                    Some(Err(tokio::sync::broadcast::error::RecvError::Closed)) => {
                        handle.daemon_updates_rx = None;
                    }
                    None => {}
                }
            }
            _ = wait_daemon_state_change(handle.daemon_state_rx.as_mut(), !ordered_daemon_updates) => {
                if let Some(rx) = handle.daemon_state_rx.as_mut() {
                    let transcript_revision = app.app.daemon_transcript_revision();
                    let resources_revision = app.app.daemon_resources_revision();
                    let projected = {
                        let state = rx.borrow();
                        if app
                            .app
                            .daemon_revision
                            .is_some_and(|revision| revision >= state.projection().revision.0)
                            && app.app.daemon_generation()
                                == Some(state.snapshot().daemon_generation.0.as_str())
                            && transcript_revision
                                .is_some_and(|revision| revision >= state.transcript_revision())
                            && resources_revision
                                .is_some_and(|revision| revision >= state.resources_revision())
                        {
                            Ok(None)
                        } else {
                            crate::projection_adapter::TuiSessionProjection::try_from_state(
                                &state,
                                transcript_revision,
                                resources_revision,
                            )
                            .map(Some)
                        }
                    };
                    match projected {
                        Ok(Some(projected)) => {
                            daemon_state = Some(rx.borrow().clone());
                            apply_daemon_projection(&mut app, projected);
                        }
                        Ok(None) => {}
                        Err(error) => app.app.push_note(
                            format!("daemon projection rejected: {error}"),
                            app::NoteLevel::Error,
                        ),
                    }
                }
            }
            _ = wait_goal_change(handle.goal_rx.as_mut()) => {
                if let Some(rx) = handle.goal_rx.as_mut() {
                    app.app.goal = rx.borrow().clone();
                }
            }
            _ = wait_context_change(handle.context_rx.as_mut()) => {
                if let Some(rx) = handle.context_rx.as_mut() {
                    if apply_context_snapshot(
                        &mut app.app,
                        rx.borrow().clone(),
                        app.wm.modals.model_picker.is_pending(),
                    ) {
                        app.app.save_ui_state();
                    }
                }
            }
            _ = wait_attach_change(handle.attach_rx.as_mut()) => {
                if let Some(rx) = handle.attach_rx.as_mut() {
                    app.app.attach_count = *rx.borrow();
                }
                if let Some(session) = handle.session.as_ref() {
                    editor.reconcile_images(&session.pending_images());
                }
            }
            _ = wait_todos_change(handle.todos_rx.as_mut()) => {
                if let Some(rx) = handle.todos_rx.as_mut() {
                    app.app.todos = rx.borrow().clone();
                    let first_pending = app.app.todos.iter().position(|t| !matches!(t.status, atman_runtime::memory::todo::TodoStatus::Done | atman_runtime::memory::todo::TodoStatus::Cancelled));
                    if let Some(idx) = first_pending {
                        if idx > 0 {
                            app.app.todos_scroll = ((idx - 1) * 2) as u16;
                        } else {
                            app.app.todos_scroll = 0;
                        }
                    }
                }
            }
            _ = wait_plans_change(handle.plans_rx.as_mut()) => {
                if let Some(rx) = handle.plans_rx.as_mut() {
                    app.app.plans = rx.borrow().clone();
                    let first_pending_step = app.app.plans.iter().max_by_key(|p| p.updated_at)
                        .and_then(|p| p.steps.iter().position(|s| !s.done));
                    if let Some(idx) = first_pending_step {
                        if idx > 0 {
                            app.app.plans_scroll = idx as u16;
                        } else {
                            app.app.plans_scroll = 0;
                        }
                    }
                }
            }
            _ = wait_trust_change(handle.trust_rx.as_mut()) => {
                if let Some(rx) = handle.trust_rx.as_mut() {
                    let theme = app.app.trust.theme;
                    app.app.trust = rx.borrow().clone();
                    app.app.trust.theme = theme;
                    app.app.mark_visual_dirty();
                }
            }
            inj = recv_injection(handle.injection_rx.as_mut()) => {
                if let Some(inj) = inj {
                    // Keep only pending injections, drop consumed/cancelled ones.
                    if matches!(inj.state, atman_runtime::injection::InjectionState::Pending) {
                        if let Some(existing) = app.app.pending_injections.iter_mut().find(|i| i.id == inj.id) {
                            *existing = inj;
                        } else {
                            app.app.pending_injections.push(inj);
                        }
                    } else {
                        app.app.pending_injections.retain(|i| i.id != inj.id);
                    }
                    app.app.mark_visual_dirty();
                }
            }
            _ = wait_compact_review_change(handle.compact_review_rx.as_mut()) => {
                if let Some(rx) = handle.compact_review_rx.as_mut() {
                    if crate::compact_review_modal::CompactReviewModal::reconcile(
                        &mut app.wm.modals.compact_review,
                        &rx.borrow(),
                    ) {
                        app.app.mark_visual_dirty();
                    }
                }
            }
            _ = wait_form_change(handle.form_rx.as_mut()) => {
                if let Some(rx) = handle.form_rx.as_mut() {
                    if app.wm.modals.form_modal.reconcile(&rx.borrow()) {
                        app.app.mark_visual_dirty();
                    }
                }
            }
            cmd = recv_cmd(handle.cmd_rx.as_mut()) => {
                if let Some(cmd) = cmd {
                    match cmd {
                        TuiCommand::SetSidebar(mode) => {
                            app.app.sidebar_mode = mode;
                        }
                        TuiCommand::OpenSessionSwitcher => {
                            let scope = crate::session_switcher::SessionScope::Project;
                            let rows = key_handler::request_session_rows(
                                &app,
                                handle.control_tx.as_ref(),
                                scope,
                            );
                            app.wm.modals.session_switcher.open_with(rows, scope);
                        }
                        TuiCommand::SessionListUpdated { scope, rows } => {
                            app.app.session_rows = rows.clone();
                            if app.wm.modals.session_switcher.open
                                && app.wm.modals.session_switcher.scope == scope
                            {
                                app.wm.modals.session_switcher.set_rows(rows);
                            }
                        }
                        TuiCommand::SessionNameUpdated(name) => {
                            app.app.session_name = Some(name);
                            if app.wm.modals.session_switcher.open {
                                let scope = app.wm.modals.session_switcher.scope;
                                let rows = key_handler::request_session_rows(
                                    &app,
                                    handle.control_tx.as_ref(),
                                    scope,
                                );
                                app.wm.modals.session_switcher.set_rows(rows);
                            }
                        }
                        TuiCommand::OpenSessionMoveForm(form) => {
                            app.wm.modals.form_modal.attach_host(form);
                            app.app.mark_visual_dirty();
                        }
                        TuiCommand::CloseSessionMoveForm(form_id) => {
                            if app.wm.modals.form_modal.pending.as_ref().is_some_and(|form| form.form_id == form_id) {
                                app.wm.modals.form_modal.reconcile(&[]);
                                app.app.mark_visual_dirty();
                            }
                        }
                        TuiCommand::OpenSuggestionForm(form) => {
                            app.wm.modals.form_modal.attach_host(form);
                            app.app.mark_visual_dirty();
                        }
                        TuiCommand::CloseSuggestionForm(form_id) => {
                            if app.wm.modals.form_modal.pending.as_ref().is_some_and(|form| form.form_id == form_id) {
                                app.wm.modals.form_modal.reconcile(&[]);
                                app.app.mark_visual_dirty();
                            }
                        }
                        TuiCommand::OpenTrustModePicker => {
                            app.wm.modals.open_trust_mode_picker(&mut app.app);
                        }
                        TuiCommand::OpenThemePicker => {
                            app.wm.modals.theme_picker_open = true;
                        }
                        TuiCommand::OpenModelPicker => {
                            app.wm.modals.model_picker.open();
                        }
                        TuiCommand::AddDraftAttachment(source) => {
                            let number = editor.attach_image(source);
                            app.app.attach_count = editor.pending_images().len();
                            app.app.push_note(
                                format!("attached image as [image {number}] ({} pending)", app.app.attach_count),
                                crate::app::NoteLevel::Info,
                            );
                        }
                        TuiCommand::ClearDraftAttachments => {
                            while editor.remove_last_image().is_some() {}
                            app.app.attach_count = 0;
                            app.app.push_note(
                                "pending attachments cleared",
                                crate::app::NoteLevel::Info,
                            );
                        }
                        TuiCommand::ListDraftAttachments => {
                            if editor.pending_images().is_empty() {
                                app.app.push_note(
                                    "no pending attachments",
                                    crate::app::NoteLevel::Info,
                                );
                            } else {
                                let names = editor
                                    .pending_images()
                                    .iter()
                                    .map(|image| format!("[image {}] {}", image.number, image.name))
                                    .collect::<Vec<_>>()
                                    .join(" · ");
                                app.app.push_note(names, crate::app::NoteLevel::Info);
                            }
                        }
                        TuiCommand::ModelSwitchResult {
                            request_id,
                            model,
                            result,
                        } => {
                            app.wm.modals.resolve_model_switch(
                                &mut app.app,
                                request_id,
                                model,
                                result,
                            );
                        }
                        TuiCommand::ProviderCatalogChanged { added_provider } => {
                            apply_provider_catalog_changed(&mut app, added_provider);
                        }
                        TuiCommand::ProviderCatalogRefreshResult {
                            provider_id,
                            result,
                        } => {
                            apply_provider_catalog_refresh_result(&mut app, &provider_id, result);
                        }
                        TuiCommand::ProviderMutationResult { request, result } => {
                            apply_provider_mutation_result(&mut app, request, result);
                        }
                        TuiCommand::ProviderTestResult((msg, ok)) => {
                            let level = if ok {
                                app::NoteLevel::Success
                            } else {
                                app::NoteLevel::Error
                            };
                            app.app.push_toast(
                                msg,
                                level,
                                std::time::Duration::from_secs(5),
                                app::ToastPosition::TopRight,
                            );
                        }
                        TuiCommand::McpTestResult { name, message, ok } => {
                            let level = if ok {
                                app::NoteLevel::Success
                            } else {
                                app::NoteLevel::Error
                            };
                            app.app.push_toast(
                                format!("{name}: {message}"),
                                level,
                                std::time::Duration::from_secs(5),
                                app::ToastPosition::TopRight,
                            );
                        }
                        TuiCommand::McpReloaded { active_runs } => {
                            let message = match active_runs {
                                Some(0) => "MCP configuration loaded; connections start with the next run".into(),
                                Some(count) => format!("MCP servers reloaded for {count} active run(s)"),
                                None => "MCP servers reloaded".into(),
                            };
                            app.app.push_toast(
                                message,
                                app::NoteLevel::Success,
                                std::time::Duration::from_secs(3),
                                app::ToastPosition::TopRight,
                            );
                        }
                        TuiCommand::McpResourcesResult { name, resources } => {
                            app.app.replace_mcp_resources(name, resources);
                        }
                        TuiCommand::McpPromptsResult { name, prompts } => {
                            app.app.replace_mcp_prompts(name, prompts);
                        }
                    }
                }
            }
        }
    }
    if let Some(tx) = transcript_bookmark_tx {
        let _ = tx.send(app.app.transcript_bookmark());
    }
    Ok(())
}

fn apply_provider_catalog_changed(app: &mut UiState, added_provider: Option<String>) {
    app.wm.modals.provider_manager.refresh_list();
    app.wm.modals.model_manager.refresh();
    if !app.wm.modals.model_picker.is_pending() && app.app.reconcile_input_reasoning() {
        app.app.save_ui_state();
    }
    if let Some(name) = added_provider {
        let needs_model = atman_runtime::model_registry::all_provider_groups_with_empty()
            .into_iter()
            .find(|group| group.provider_name == name)
            .is_none_or(|group| group.models.is_empty());
        if needs_model {
            app.wm.modals.model_manager.open_with_provider(&name);
        }
        if app.wm.modals.onboarding_open {
            app.wm.modals.onboarding.provider_added(Some(&name));
            app.wm.modals.onboarding.try_advance_to_model_select();
        }
    }
}

fn apply_provider_catalog_refresh_result(
    app: &mut UiState,
    provider_id: &str,
    result: Result<atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome, String>,
) {
    match result {
        Ok(atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome::AlreadyInFlight) => {}
        Ok(atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome::NotNeeded) => {
            apply_provider_catalog_changed(app, None);
        }
        Ok(atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome::CatalogUpdated(
            delta,
        )) => {
            apply_provider_catalog_changed(app, None);
            app.app.push_toast(
                format!(
                    "{provider_id}: models refreshed · +{} ~{} -{} · {} total",
                    delta.added, delta.updated, delta.removed, delta.total
                ),
                app::NoteLevel::Success,
                std::time::Duration::from_secs(5),
                app::ToastPosition::TopRight,
            );
        }
        Ok(_) => {}
        Err(error) => app.app.push_note(
            format!("{provider_id}: background model refresh failed: {error}"),
            app::NoteLevel::Error,
        ),
    }
}

fn apply_provider_mutation_result(
    app: &mut UiState,
    request: crate::ProviderMutationRequest,
    result: Result<crate::ProviderMutationSuccess, String>,
) {
    let resolution = app
        .wm
        .modals
        .provider_manager
        .resolve_mutation(&request, &result);
    if matches!(
        resolution,
        crate::provider_manager::ProviderMutationResolution::Ignored
    ) {
        return;
    }
    if let crate::provider_manager::ProviderMutationResolution::ConfigSaved {
        name,
        created: true,
    } = &resolution
    {
        apply_provider_catalog_changed(app, Some(name.clone()));
    } else {
        apply_provider_catalog_changed(app, None);
    }
    if let crate::provider_manager::ProviderMutationResolution::Installed { name } = &resolution
        && app.wm.modals.onboarding_open
    {
        app.wm.modals.onboarding.provider_added(Some(name));
        app.wm.modals.onboarding.try_advance_to_model_select();
    }
    let (message, level) = if matches!(
        resolution,
        crate::provider_manager::ProviderMutationResolution::ProtocolError
    ) {
        (
            "provider mutation returned a mismatched result".to_string(),
            app::NoteLevel::Error,
        )
    } else {
        match result {
            Ok(crate::ProviderMutationSuccess::Installed { name, delta, .. }) => (
                format!("{name} added · {} models", delta.total),
                app::NoteLevel::Success,
            ),
            Ok(crate::ProviderMutationSuccess::StateChanged { enabled, .. }) => {
                let message = match enabled {
                    Some(true) => "provider enabled",
                    Some(false) => "provider disabled",
                    None => "provider removed",
                };
                (message.to_string(), app::NoteLevel::Success)
            }
            Ok(crate::ProviderMutationSuccess::Refreshed { delta, .. }) => (
                format!(
                    "models refreshed · +{} ~{} -{} · {} total",
                    delta.added, delta.updated, delta.removed, delta.total
                ),
                app::NoteLevel::Success,
            ),
            Ok(crate::ProviderMutationSuccess::ConfigSaved { name, created }) => (
                if created {
                    format!("{name} added")
                } else {
                    format!("{name} updated")
                },
                app::NoteLevel::Success,
            ),
            Err(error) => (
                format!("provider update failed: {error}"),
                app::NoteLevel::Error,
            ),
        }
    };
    app.app.push_toast(
        message,
        level,
        std::time::Duration::from_secs(5),
        app::ToastPosition::TopRight,
    );
}

pub(crate) async fn recv_cmd(
    rx: Option<&mut mpsc::UnboundedReceiver<TuiCommand>>,
) -> Option<TuiCommand> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

fn apply_context_snapshot(
    app: &mut AppState,
    mut context: atman_runtime::ContextSnapshot,
    model_switch_pending: bool,
) -> bool {
    if model_switch_pending {
        context.model.clone_from(&app.context.model);
        context.provider.clone_from(&app.context.provider);
        app.replace_context_snapshot(context);
        false
    } else {
        app.replace_context_snapshot(context);
        app.reconcile_input_reasoning()
    }
}

fn apply_daemon_projection(
    app: &mut UiState,
    mut projected: crate::projection_adapter::TuiSessionProjection,
) {
    let generation_changed = projected
        .daemon_generation
        .as_deref()
        .is_some_and(|generation| app.app.daemon_generation() != Some(generation));
    let generation_matches = projected
        .daemon_generation
        .as_deref()
        .is_none_or(|generation| app.app.daemon_generation() == Some(generation));
    if generation_matches
        && app
            .app
            .daemon_revision
            .is_some_and(|revision| revision >= projected.revision)
        && projected.transcript.as_ref().is_none_or(|_| {
            app.app
                .daemon_transcript_revision()
                .is_some_and(|revision| revision >= projected.transcript_revision)
        })
        && projected.task_snapshots.as_ref().is_none_or(|_| {
            app.app
                .daemon_resources_revision()
                .is_some_and(|revision| revision >= projected.resources_revision)
        })
    {
        return;
    }
    let theme = app.app.trust.theme;
    projected.trust.theme = theme;
    for (group_id, group) in &mut projected.pending_permission_groups {
        group.expanded = app
            .app
            .pending_permission_groups
            .get(group_id)
            .is_some_and(|existing| existing.expanded);
    }
    if app
        .app
        .selected_permission_group
        .as_ref()
        .is_some_and(|id| !projected.pending_permission_groups.contains_key(id))
    {
        app.app.selected_permission_group = None;
    }

    if generation_changed {
        app.app.reset_daemon_projection_slices();
    }
    if let Some(generation) = projected.daemon_generation {
        app.app.daemon_generation = Some(generation);
    }
    app.app.daemon_revision = Some(projected.revision);
    app.app.session_name = projected.session_name;
    app.app.project_root = projected.project_root;
    app.app.goal = projected.goal;
    app.app.replace_context_snapshot(projected.context);
    app.app.todos = projected.todos;
    app.app.plans = projected.plans;
    app.app.trust = projected.trust;
    app.app.pending_permissions = projected.pending_permissions;
    app.app.pending_permission_groups = projected.pending_permission_groups;
    app.app.grouped_permission_request_ids = app
        .app
        .pending_permission_groups
        .values()
        .flat_map(|group| group.payload.request_ids.iter().cloned())
        .collect();
    app.app.pending_injections = projected.pending_injections;
    if let Some(transcript) = projected.transcript {
        let (transcript, sequences) = transcript
            .into_iter()
            .map(|item| (item.output, item.sequence))
            .unzip();
        app.app
            .reconcile_daemon_transcript(transcript, sequences, projected.transcript_revision);
    }
    if let Some(task_snapshots) = projected.task_snapshots {
        app.app
            .reconcile_daemon_tasks(task_snapshots, projected.resources_revision);
    }
    app.wm.modals.form_modal.reconcile(&projected.pending_forms);
    crate::compact_review_modal::CompactReviewModal::reconcile(
        &mut app.wm.modals.compact_review,
        &projected.pending_compact_reviews,
    );
    if app.app.reconcile_input_reasoning() {
        app.app.save_ui_state();
    }
}

fn apply_daemon_update(
    app: &mut UiState,
    state: &mut Option<atman_client::SessionState>,
    update: atman_client::SessionUpdate,
) -> Result<()> {
    match update {
        atman_client::SessionUpdate::Reset(next) => {
            let next = *next;
            let projected =
                crate::projection_adapter::TuiSessionProjection::try_from_state(&next, None, None)?;
            *state = Some(next);
            apply_daemon_projection(app, projected);
        }
        atman_client::SessionUpdate::Changed {
            state: next,
            signals,
        } => {
            for signal in signals {
                apply_daemon_signal(app, signal, next.projection())?;
            }
            let projected = crate::projection_adapter::TuiSessionProjection::try_from_state(
                &next,
                app.app.daemon_transcript_revision(),
                app.app.daemon_resources_revision(),
            )?;
            *state = Some(next);
            apply_daemon_projection(app, projected);
        }
        atman_client::SessionUpdate::HistoryPrepended {
            state: next,
            loaded_items: _,
        } => {
            app.app.preserve_scroll_for_history_change();
            let projected = crate::projection_adapter::TuiSessionProjection::try_from_state(
                &next,
                app.app.daemon_transcript_revision(),
                app.app.daemon_resources_revision(),
            )?;
            *state = Some(next);
            apply_daemon_projection(app, projected);
        }
        atman_client::SessionUpdate::HistoryAppended {
            state: next,
            loaded_items: _,
            has_more,
        } => {
            if has_more || !app.app.follow_tail {
                app.app.preserve_scroll_for_history_change();
            }
            let projected = crate::projection_adapter::TuiSessionProjection::try_from_state(
                &next,
                app.app.daemon_transcript_revision(),
                app.app.daemon_resources_revision(),
            )?;
            *state = Some(next);
            apply_daemon_projection(app, projected);
        }
        atman_client::SessionUpdate::HistoryDetailLoaded {
            state: next,
            tool_use_id,
        } => {
            let projected =
                crate::projection_adapter::TuiSessionProjection::try_from_state(&next, None, None)?;
            *state = Some(next);
            apply_daemon_projection(app, projected);
            app.refresh_open_tool_output_panel(&tool_use_id);
        }
    }
    Ok(())
}

fn apply_daemon_signal(
    app: &mut UiState,
    signal: atman_proto::SessionSignal,
    projection: &atman_proto::SessionProjection,
) -> Result<()> {
    match crate::projection_adapter::daemon_signal(signal, projection)? {
        crate::projection_adapter::TuiDaemonSignal::Frame(frame) => {
            app.app.apply_stream_frame(*frame);
        }
        crate::projection_adapter::TuiDaemonSignal::Progress { run_id, label } => {
            app.app.push_status(daemon_progress_key(&run_id), label);
        }
        crate::projection_adapter::TuiDaemonSignal::LlmDone {
            run_id,
            total_tokens,
        } => {
            app.app.remove_status(&daemon_progress_key(&run_id));
            app.app.apply_stream_frame(StreamFrame::LlmDone {
                total_tokens,
                run_id: Some(run_id),
            });
        }
    }
    Ok(())
}

fn daemon_progress_key(run_id: &str) -> String {
    format!("daemon-progress:{run_id}")
}

pub(crate) async fn recv_daemon_update(
    rx: Option<&mut tokio::sync::broadcast::Receiver<atman_client::SessionUpdate>>,
) -> Option<Result<atman_client::SessionUpdate, tokio::sync::broadcast::error::RecvError>> {
    match rx {
        Some(rx) => Some(rx.recv().await),
        None => std::future::pending().await,
    }
}

pub(crate) async fn wait_daemon_state_change(
    rx: Option<&mut tokio::sync::watch::Receiver<atman_client::SessionState>>,
    enabled: bool,
) {
    match (rx, enabled) {
        (Some(rx), true) => {
            let _ = rx.changed().await;
        }
        _ => std::future::pending().await,
    }
}

pub(crate) async fn wait_goal_change(
    rx: Option<&mut tokio::sync::watch::Receiver<Option<String>>>,
) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

pub(crate) async fn wait_context_change(
    rx: Option<&mut tokio::sync::watch::Receiver<atman_runtime::ContextSnapshot>>,
) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

pub(crate) async fn wait_attach_change(rx: Option<&mut tokio::sync::watch::Receiver<usize>>) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

pub(crate) async fn wait_todos_change(
    rx: Option<&mut tokio::sync::watch::Receiver<Vec<atman_runtime::memory::todo::Todo>>>,
) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

pub(crate) async fn wait_plans_change(
    rx: Option<&mut tokio::sync::watch::Receiver<Vec<atman_runtime::memory::plan::Plan>>>,
) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

pub(crate) async fn wait_trust_change(
    rx: Option<&mut tokio::sync::watch::Receiver<atman_runtime::trust::TrustConfig>>,
) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

pub(crate) async fn wait_compact_review_change(
    rx: Option<&mut tokio::sync::watch::Receiver<Vec<atman_runtime::PendingCompactReview>>>,
) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

pub(crate) async fn wait_form_change(
    rx: Option<&mut tokio::sync::watch::Receiver<Vec<atman_runtime::form::PendingForm>>>,
) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

pub(crate) async fn recv_note(
    rx: Option<&mut mpsc::UnboundedReceiver<TuiNote>>,
) -> Option<TuiNote> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

pub(crate) async fn recv_injection(
    rx: Option<&mut tokio::sync::broadcast::Receiver<atman_runtime::injection::Injection>>,
) -> Option<atman_runtime::injection::Injection> {
    match rx {
        Some(r) => r.recv().await.ok(),
        None => std::future::pending().await,
    }
}

pub(crate) async fn recv_task_event(
    rx: Option<&mut tokio::sync::broadcast::Receiver<atman_runtime::TaskEvent>>,
) -> Option<atman_runtime::TaskEvent> {
    match rx {
        Some(r) => r.recv().await.ok(),
        None => std::future::pending().await,
    }
}

pub(crate) async fn wait_shutdown(rx: Option<&mut tokio::sync::oneshot::Receiver<()>>) {
    match rx {
        Some(r) => {
            let _ = r.await;
        }
        None => std::future::pending().await,
    }
}

pub(crate) async fn poll_update_check(
    handle: &mut tokio::task::JoinHandle<Option<String>>,
) -> Option<String> {
    handle.await.ok().flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projected_session(
        revision: u64,
        goal: &str,
        status: atman_runtime::workflow::NodeStatus,
    ) -> crate::projection_adapter::TuiSessionProjection {
        let turn_id = atman_runtime::event::TurnId::now();
        let graph = atman_runtime::workflow::WorkflowGraph {
            turn_id,
            root: vec![atman_runtime::workflow::WorkflowNode {
                id: "run".into(),
                kind: atman_runtime::workflow::WorkflowNodeKind::Flow {
                    run_id: "run".into(),
                    flow_name: "agent".into(),
                },
                label: "agent".into(),
                status,
                started_at: Some(chrono::Utc::now()),
                ended_at: None,
                output_preview: None,
                children: Vec::new(),
                parallelism: atman_runtime::workflow::Parallelism::Serial,
                approval: None,
                llm_stats: None,
            }],
            permission_requests: Default::default(),
            permission_groups: Default::default(),
            resolved_permission_groups: Default::default(),
        };
        let workflow = graph.into();
        crate::projection_adapter::TuiSessionProjection {
            daemon_generation: Some("generation-a".into()),
            revision,
            session_name: Some("remote".into()),
            project_root: Some("/workspace".into()),
            goal: Some(goal.into()),
            context: Default::default(),
            todos: Vec::new(),
            plans: Vec::new(),
            trust: Default::default(),
            pending_permissions: Default::default(),
            pending_permission_groups: Default::default(),
            pending_forms: Vec::new(),
            pending_compact_reviews: Vec::new(),
            pending_injections: Vec::new(),
            transcript: Some(vec![crate::projection_adapter::TuiTranscriptItem {
                output: crate::app::OutputItem::WorkflowPanel {
                    turn_index: 0,
                    graph: workflow,
                    expanded_nodes: Default::default(),
                    panel_expanded: true,
                    started_at: std::time::Instant::now(),
                    ended_at: (!matches!(
                        status,
                        atman_runtime::workflow::NodeStatus::Pending
                            | atman_runtime::workflow::NodeStatus::Running
                    ))
                    .then(std::time::Instant::now),
                    cancelled: matches!(status, atman_runtime::workflow::NodeStatus::Cancelled),
                },
                sequence: 1,
            }]),
            transcript_revision: revision,
            task_snapshots: Some(Vec::new()),
            resources_revision: revision,
        }
    }

    #[test]
    fn daemon_revision_reconciles_domain_state_without_overwriting_local_ui_state() {
        let mut app = UiState::new(AppState::new("session".into(), None));
        app.app.input = "unsent draft".into();
        app.app.trust.theme = atman_runtime::trust::Theme::Wuxia;
        let task_id = atman_runtime::TaskId::now();
        let mut initial =
            projected_session(2, "current", atman_runtime::workflow::NodeStatus::Running);
        initial.task_snapshots = Some(vec![atman_runtime::TaskSnapshot {
            id: task_id.clone(),
            kind: atman_runtime::TaskKind::Terminal,
            label: "Run server".into(),
            command: Some("cargo run".into()),
            status: atman_runtime::TaskStatus::Running,
            started_at: std::time::Instant::now(),
            ended_at: None,
            source_handle: "term-1".into(),
            session_id: "session".into(),
            workspace_id: None,
            flow_run_id: None,
            termination: None,
        }]);

        apply_daemon_projection(&mut app, initial);
        assert_eq!(app.app.goal.as_deref(), Some("current"));
        assert_eq!(app.app.input, "unsent draft");
        assert_eq!(app.app.trust.theme, atman_runtime::trust::Theme::Wuxia);
        assert_eq!(app.app.daemon_revision, Some(2));
        assert_eq!(app.app.task_snapshots.len(), 1);
        assert_eq!(app.app.task_id_index.get(&task_id), Some(&0));
        assert_eq!(app.app.task_handle_index.get("term-1"), Some(&0));
        assert!(matches!(
            &*app.app.items,
            [app::OutputItem::WorkflowPanel { ended_at: None, .. }]
        ));
        app.app.toggle_workflow_panel_expansion(0);
        app.app.push_item(app::OutputItem::AssistantMd {
            md: "ephemeral".into(),
            streaming: true,
            retried: false,
        });
        app.app.push_note("local diagnostic", app::NoteLevel::Warn);

        let mut metadata_only =
            projected_session(3, "metadata", atman_runtime::workflow::NodeStatus::Running);
        metadata_only.transcript_revision = 2;
        metadata_only.transcript = None;
        metadata_only.resources_revision = 2;
        metadata_only.task_snapshots = None;
        apply_daemon_projection(&mut app, metadata_only);
        assert_eq!(app.app.goal.as_deref(), Some("metadata"));
        assert_eq!(app.app.task_snapshots.len(), 1);
        assert!(app.app.items.iter().any(|item| matches!(
            item,
            app::OutputItem::AssistantMd { md, .. } if md == "ephemeral"
        )));

        apply_daemon_projection(
            &mut app,
            projected_session(1, "stale", atman_runtime::workflow::NodeStatus::Ok),
        );
        assert_eq!(app.app.goal.as_deref(), Some("metadata"));

        let mut terminal = projected_session(4, "done", atman_runtime::workflow::NodeStatus::Ok);
        terminal.task_snapshots = Some(Vec::new());
        if let app::OutputItem::WorkflowPanel { graph, .. } =
            &mut terminal.transcript.as_mut().unwrap()[0].output
        {
            let mut updated = graph.clone().into_graph();
            updated.turn_id = match &app.app.items[0] {
                app::OutputItem::WorkflowPanel { graph, .. } => graph.graph().turn_id.clone(),
                _ => unreachable!(),
            };
            *graph = updated.into();
        }
        apply_daemon_projection(&mut app, terminal);
        assert_eq!(app.app.goal.as_deref(), Some("done"));
        assert!(app.app.task_snapshots.is_empty());
        assert!(app.app.task_id_index.is_empty());
        assert!(app.app.task_handle_index.is_empty());
        assert!(matches!(
            app.app.items.first(),
            Some(app::OutputItem::WorkflowPanel {
                ended_at: Some(_),
                panel_expanded: false,
                ..
            })
        ));
        assert!(app.app.items.iter().all(|item| !matches!(
            item,
            app::OutputItem::AssistantMd { md, .. } if md == "ephemeral"
        )));
        assert!(matches!(
            app.app.items.last(),
            Some(app::OutputItem::SystemNote { text, level: app::NoteLevel::Warn })
                if text == "local diagnostic"
        ));
    }

    #[test]
    fn daemon_generation_change_replaces_equal_or_lower_projection_revisions() {
        let mut app = UiState::new(AppState::new("session".into(), None));
        let mut before = projected_session(
            5,
            "before restart",
            atman_runtime::workflow::NodeStatus::Running,
        );
        before.daemon_generation = Some("generation-a".into());
        apply_daemon_projection(&mut app, before);

        let mut after =
            projected_session(1, "after restart", atman_runtime::workflow::NodeStatus::Ok);
        after.daemon_generation = Some("generation-b".into());
        after.transcript_revision = 5;
        after.resources_revision = 5;
        apply_daemon_projection(&mut app, after);

        assert_eq!(app.app.daemon_generation(), Some("generation-b"));
        assert_eq!(app.app.daemon_revision, Some(1));
        assert_eq!(app.app.goal.as_deref(), Some("after restart"));
        assert!(matches!(
            app.app.items.first(),
            Some(app::OutputItem::WorkflowPanel {
                ended_at: Some(_),
                ..
            })
        ));

        let mut resynced = projected_session(
            1,
            "after resync",
            atman_runtime::workflow::NodeStatus::Running,
        );
        resynced.daemon_generation = Some("generation-b".into());
        resynced.transcript_revision = 6;
        resynced.resources_revision = 6;
        apply_daemon_projection(&mut app, resynced);
        assert_eq!(app.app.daemon_revision, Some(1));
        assert_eq!(app.app.goal.as_deref(), Some("after resync"));
        assert_eq!(app.app.daemon_transcript_revision(), Some(6));
        assert_eq!(app.app.daemon_resources_revision(), Some(6));
    }

    #[test]
    fn pending_model_switch_defers_context_model_and_reasoning_changes() {
        let mut app = AppState::new("session".into(), None);
        app.context.model = "old-model".into();
        app.context.provider = "old-provider".into();
        app.context.tokens_in = 4;
        app.input_reasoning = Some(atman_runtime::provider::ReasoningSelection::Disabled);
        let incoming = atman_runtime::ContextSnapshot {
            model: "new-model".into(),
            tokens_in: 9,
            ..Default::default()
        };

        assert!(!apply_context_snapshot(&mut app, incoming, true));
        assert_eq!(app.context.model, "old-model");
        assert_eq!(app.context.provider, "old-provider");
        assert_eq!(app.context.tokens_in, 9);
        assert_eq!(
            app.input_reasoning,
            Some(atman_runtime::provider::ReasoningSelection::Disabled)
        );
    }

    fn pending_provider_request(
        app: &mut UiState,
        action: crate::ProviderMutation,
    ) -> crate::ProviderMutationRequest {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert_eq!(
            app.wm
                .modals
                .provider_manager
                .begin_mutation(action, Some(&tx)),
            crate::provider_manager::ProviderDispatchOutcome::Started
        );
        match rx.try_recv().unwrap() {
            crate::TuiControl::MutateProvider(request) => request,
            _ => panic!("expected provider mutation"),
        }
    }

    fn config_upsert(name: &str, create: bool) -> crate::ProviderMutation {
        crate::ProviderMutation::UpsertConfig {
            name: name.into(),
            kind: "openai-compat".into(),
            api_key: "test-key".into(),
            api_key_env: String::new(),
            base_url: "https://gateway.example/v1".into(),
            max_tokens: None,
            reasoning_format: "thinking-toggle".into(),
            enabled: true,
            create,
        }
    }

    #[test]
    fn provider_login_advances_onboarding_only_after_success() {
        let mut app = UiState::new(AppState::new("session".into(), None));
        app.wm.modals.onboarding_open = true;
        let action = crate::ProviderMutation::Login {
            kind: atman_runtime::auth_store::ProviderKind::Codex,
            name: "OAuth".into(),
        };
        let failed_request = pending_provider_request(&mut app, action.clone());

        apply_provider_mutation_result(&mut app, failed_request, Err("login failed".into()));
        assert_eq!(
            app.wm.modals.onboarding.step,
            crate::onboarding::OnboardingStep::ProviderSelect
        );
        assert_eq!(app.app.toasts.last().unwrap().level, app::NoteLevel::Error);

        let successful_request = pending_provider_request(&mut app, action);
        apply_provider_mutation_result(
            &mut app,
            successful_request,
            Ok(crate::ProviderMutationSuccess::Installed {
                provider_id: "provider-id".into(),
                name: "OAuth".into(),
                kind: atman_runtime::auth_store::ProviderKind::Codex,
                delta: Default::default(),
            }),
        );
        assert_eq!(
            app.wm.modals.onboarding.step,
            crate::onboarding::OnboardingStep::ModelSelect
        );
        assert_eq!(
            app.app.toasts.last().unwrap().level,
            app::NoteLevel::Success
        );
    }

    #[test]
    fn config_create_advances_onboarding_and_opens_model_manager_after_success() {
        let mut app = UiState::new(AppState::new("session".into(), None));
        app.wm.modals.onboarding_open = true;
        let provider = "config-ack-empty";
        let failed_request = pending_provider_request(&mut app, config_upsert(provider, true));

        apply_provider_mutation_result(&mut app, failed_request, Err("save failed".into()));
        assert_eq!(
            app.wm.modals.onboarding.step,
            crate::onboarding::OnboardingStep::ProviderSelect
        );
        assert!(!app.wm.modals.model_manager.open);
        assert_eq!(app.app.toasts.last().unwrap().level, app::NoteLevel::Error);

        let successful_request = pending_provider_request(&mut app, config_upsert(provider, true));
        apply_provider_mutation_result(
            &mut app,
            successful_request,
            Ok(crate::ProviderMutationSuccess::ConfigSaved {
                name: provider.into(),
                created: true,
            }),
        );
        assert_eq!(
            app.wm.modals.onboarding.step,
            crate::onboarding::OnboardingStep::ModelSelect
        );
        assert!(app.wm.modals.model_manager.open);
        assert!(app.app.toasts.last().unwrap().message.contains("added"));
    }

    #[test]
    fn config_update_refreshes_without_reopening_onboarding() {
        let mut app = UiState::new(AppState::new("session".into(), None));
        app.wm.modals.onboarding_open = true;
        let provider = "config-ack-update";
        let request = pending_provider_request(&mut app, config_upsert(provider, false));

        apply_provider_mutation_result(
            &mut app,
            request,
            Ok(crate::ProviderMutationSuccess::ConfigSaved {
                name: provider.into(),
                created: false,
            }),
        );

        assert_eq!(
            app.wm.modals.onboarding.step,
            crate::onboarding::OnboardingStep::ProviderSelect
        );
        assert!(!app.wm.modals.model_manager.open);
        assert!(app.app.toasts.last().unwrap().message.contains("updated"));
    }

    #[test]
    fn stale_provider_result_is_silent_and_does_not_consume_pending_request() {
        let mut app = UiState::new(AppState::new("session".into(), None));
        let action = crate::ProviderMutation::Refresh {
            provider_id: "provider-id".into(),
        };
        let request = pending_provider_request(&mut app, action);
        let mut stale = request.clone();
        stale.request_id += 1;

        apply_provider_mutation_result(
            &mut app,
            stale,
            Ok(crate::ProviderMutationSuccess::Refreshed {
                provider_id: "provider-id".into(),
                delta: Default::default(),
            }),
        );
        assert!(app.app.toasts.is_empty());

        apply_provider_mutation_result(
            &mut app,
            request,
            Ok(crate::ProviderMutationSuccess::Refreshed {
                provider_id: "provider-id".into(),
                delta: Default::default(),
            }),
        );
        assert_eq!(app.app.toasts.len(), 1);
        assert!(app.app.toasts[0].message.contains("models refreshed"));
    }

    #[test]
    fn only_provider_add_catalog_changes_advance_onboarding() {
        let mut app = UiState::new(AppState::new("session".into(), None));
        app.wm.modals.onboarding_open = true;

        apply_provider_catalog_changed(&mut app, None);
        assert_eq!(
            app.wm.modals.onboarding.step,
            crate::onboarding::OnboardingStep::ProviderSelect
        );

        apply_provider_catalog_changed(&mut app, Some("Provider".into()));
        assert_eq!(
            app.wm.modals.onboarding.step,
            crate::onboarding::OnboardingStep::ModelSelect
        );
    }

    #[test]
    fn background_provider_refresh_is_silent_when_no_longer_needed() {
        let mut app = UiState::new(AppState::new("session".into(), None));

        apply_provider_catalog_refresh_result(
            &mut app,
            "provider-id",
            Ok(atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome::NotNeeded),
        );

        assert!(app.app.items.is_empty());
        assert!(app.app.toasts.is_empty());
    }

    #[test]
    fn background_provider_refresh_failure_is_persistent() {
        let mut app = UiState::new(AppState::new("session".into(), None));

        apply_provider_catalog_refresh_result(
            &mut app,
            "provider-id",
            Err("discovery unavailable".into()),
        );

        assert!(app.app.toasts.is_empty());
        assert!(matches!(
            &*app.app.items,
            [app::OutputItem::SystemNote { text, level }]
                if text.contains("provider-id")
                    && text.contains("discovery unavailable")
                    && *level == app::NoteLevel::Error
        ));
    }

    #[test]
    fn background_provider_refresh_success_updates_catalog_without_onboarding() {
        let mut app = UiState::new(AppState::new("session".into(), None));
        app.wm.modals.onboarding_open = true;

        apply_provider_catalog_refresh_result(
            &mut app,
            "provider-id",
            Ok(
                atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome::CatalogUpdated(
                    atman_runtime::model_registry::CatalogDelta {
                        added: 2,
                        updated: 1,
                        removed: 0,
                        total: 3,
                    },
                ),
            ),
        );

        assert!(app.app.items.is_empty());
        assert_eq!(app.app.toasts.len(), 1);
        assert_eq!(app.app.toasts[0].level, app::NoteLevel::Success);
        assert!(app.app.toasts[0].message.contains("+2 ~1 -0 · 3 total"));
        assert_eq!(
            app.wm.modals.onboarding.step,
            crate::onboarding::OnboardingStep::ProviderSelect
        );
    }
}
