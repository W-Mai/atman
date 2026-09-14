use crate::UiState;
use std::io::Stdout;

use anyhow::Result;
use crossterm::event::{Event as CtEvent, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::{ExecutableCommand, QueueableCommand};
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
    if let Some(rx) = handle.trust_rx.as_ref() {
        let theme = app.app.trust.theme;
        app.app.trust = rx.borrow().clone();
        app.app.trust.theme = theme;
    }
    if let Some(rx) = handle.queued_submission_rx.as_ref() {
        app.app.queued_submissions = rx.borrow().clone();
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

    // Keep bulk process output off the semantic lane so a noisy command cannot
    // delay lifecycle, completion, approval, or cancellation frames.
    let (semantic_tx, mut semantic_rx) = tokio::sync::mpsc::channel::<StreamFrame>(512);
    let (bulk_tx, mut bulk_rx) = tokio::sync::mpsc::channel::<StreamFrame>(256);
    {
        let semantic_tx = semantic_tx.clone();
        let bulk_tx = bulk_tx.clone();
        let mut srx = handle.stream_rx;
        tokio::spawn(async move {
            loop {
                match srx.recv().await {
                    Ok(frame) => {
                        if !forward_ui_frame(frame, &semantic_tx, &bulk_tx).await {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
    }
    let frame_semantic_tx = semantic_tx.clone();
    let frame_bulk_tx = bulk_tx.clone();
    let session_for_sub = handle.session.clone();

    loop {
        app.app.tick_toasts();
        terminal.backend_mut().queue(BeginSynchronizedUpdate)?;
        let draw_result = terminal
            .draw(|f| render_frame(f, &mut app, &editor))
            .map(|_| ());
        let cursor_result = if let Some(kind) = app.wm.top_kind() {
            if app.wm.modals.cursor_visible(kind) {
                terminal.show_cursor()
            } else {
                terminal.hide_cursor()
            }
        } else if app.wm.focused_id().is_none()
            && (!app.app.submission_focus || app.app.queued_submission_edit.is_some())
        {
            terminal.show_cursor()
        } else {
            terminal.hide_cursor()
        };
        let sync_result = terminal.backend_mut().execute(EndSynchronizedUpdate);
        draw_result?;
        cursor_result?;
        sync_result?;
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
                            if !consumed
                                && handle_startup_session_mouse(
                                    &mut app.app,
                                    &me,
                                    handle.control_tx.as_ref(),
                                )
                            {
                                interrupt_prompt = None;
                                break;
                            }
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
                            let paste_consumed = app.wm.dispatch_paste(
                                &s,
                                &mut app.app,
                                handle.control_tx.as_ref(),
                            );
                            if !paste_consumed {
                                if let Some(edit) = app.app.queued_submission_edit.as_mut() {
                                    edit.editor.insert_str(&s);
                                } else {
                                    editor.ingest_paste(&s);
                                    interrupt_prompt = None;
                                    app.app.refresh_popup(editor.buf());
                                }
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
                            if !consumed
                                && handle_startup_session_mouse(
                                    &mut app.app,
                                    &me,
                                    handle.control_tx.as_ref(),
                                )
                            {
                                interrupt_prompt = None;
                                break;
                            }
                            if !consumed {
                            if let MouseEventKind::Down(MouseButton::Left) = me.kind {
                                if let Some(click) = app.app.approval_hitmap.at(me.column, me.row) {
                                    key_handler::dispatch_approval_click(
                                        click,
                                        &mut app.app,
                                        handle.control_tx.as_ref(),
                                    );
                                    interrupt_prompt = None;
                                    break;
                                }
                                if let Some((index, action)) = app
                                    .app
                                    .submission_queue_hitmap
                                    .action_at(me.column, me.row)
                                {
                                    key_handler::dispatch_submission_queue_action(
                                        &mut app.app,
                                        index,
                                        action,
                                        handle.control_tx.as_ref(),
                                    );
                                    interrupt_prompt = None;
                                    break;
                                }
                                if let Some(index) = app
                                    .app
                                    .submission_queue_hitmap
                                    .row_at(me.column, me.row)
                                {
                                    app.app.selected_submission = index;
                                    app.app.submission_focus = true;
                                    app.app.queued_submission_edit = None;
                                    interrupt_prompt = None;
                                    break;
                                }
                            }
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
                                app.app.submission_focus = false;
                                app.app.queued_submission_edit = None;
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
                                                if let Some(tx) = &handle.control_tx {
                                                    let _ = tx.send(TuiControl::TermResize {
                                                        handle: app.wm
                                                            .content_kind(max_id)
                                                            .and_then(|kind| match kind {
                                                                crate::wm::WindowContent::Task { handle, .. } => Some(handle.clone()),
                                                                _ => None,
                                                            })
                                                            .unwrap_or_default(),
                                                        rows: inner_rows,
                                                        cols: inner_cols,
                                                    });
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
                                    let task = close_handle.as_deref().and_then(|handle| {
                                        app.app.task_registry.as_ref()?.lookup_by_handle_in_session(
                                            handle,
                                            &app.app.session_id,
                                        )
                                    });
                                    match (close_handle, task) {
                                        (Some(handle), Some(task))
                                            if task.is_running()
                                                && task.kind == atman_runtime::TaskKind::Flow =>
                                        {
                                            if let Some(registry) = &app.app.task_registry {
                                                let _ = registry.kill_by_handle_from_operator(
                                                    &handle,
                                                    &app.app.session_id,
                                                );
                                            }
                                            app.wm.clear_panel_close_arm();
                                            app.wm.close(close_id);
                                        }
                                        (Some(handle), Some(task)) if task.is_running() => {
                                            let armed = app
                                                .wm
                                                .interaction
                                                .panel_close_armed_id
                                                .as_deref()
                                                == Some(handle.as_str())
                                                && !app.wm.panel_close_arm_expired();
                                            if armed {
                                                if let Some(registry) = &app.app.task_registry {
                                                    let _ = registry.kill_by_handle_from_operator(
                                                        &handle,
                                                        &app.app.session_id,
                                                    );
                                                }
                                                app.wm.clear_panel_close_arm();
                                            } else {
                                                app.wm.arm_panel_close(handle);
                                                app.app.push_note(
                                                    format!("press ✕ again to kill {}", task.label),
                                                    app::NoteLevel::Warn,
                                                );
                                            }
                                        }
                                        _ => app.wm.close(close_id),
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
                                    let mcp_action_hit = app
                                        .wm
                                        .interaction
                                        .last_hitmap
                                        .mcp_action_rects
                                        .iter()
                                        .find(|(pid, _, rect)| {
                                            pid == &panel_id
                                                && rect_contains(*rect, me.column, me.row)
                                                && rect_contains(pr, rect.x, rect.y)
                                        })
                                        .map(|(_, action, _)| *action);
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
                                    } else if let Some(action) = mcp_action_hit {
                                        match action {
                                            crate::wm::component::McpPanelAction::Add => {
                                                app.wm.modals.open_mcp_add();
                                            }
                                            crate::wm::component::McpPanelAction::Edit => {
                                                if let Some(server) = app
                                                    .app
                                                    .context
                                                    .mcp_servers
                                                    .get(app.app.mcp_selected)
                                                {
                                                    let name = server.name.clone();
                                                    if let Err(error) =
                                                        app.wm.modals.open_mcp_edit(&name)
                                                    {
                                                        app.app.push_toast(
                                                            error,
                                                            crate::app::NoteLevel::Warn,
                                                            std::time::Duration::from_secs(4),
                                                            crate::app::ToastPosition::TopRight,
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    } else if let Some(name) = mcp_hit {
                                        if let Some(index) = app
                                            .app
                                            .context
                                            .mcp_servers
                                            .iter()
                                            .position(|server| server.name == name)
                                        {
                                            app.app.mcp_selected = index;
                                        }
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
                                    if node_id.starts_with(
                                        crate::output::WORK_FOLD_REGION_PREFIX,
                                    ) {
                                        app.app.toggle_work_fold(&node_id);
                                    } else if node_id.starts_with(
                                        crate::output::WORKING_GROUP_REGION_PREFIX,
                                    ) {
                                        app.app.toggle_working_group_expansion(
                                            panel_idx,
                                            &node_id,
                                        );
                                    } else if let Some(tool_use_id) =
                                        node_id.strip_prefix(crate::output::TOOL_CALL_REGION_PREFIX)
                                    {
                                        app.app.toggle_tool_call_content(
                                            panel_idx,
                                            tool_use_id,
                                        );
                                    } else if let Some(tool_use_id) = node_id
                                        .strip_prefix(crate::output::TOOL_DETAIL_REGION_PREFIX)
                                    {
                                        app.app.toggle_tool_call_detail(
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
                                        if let Some(handle) = app
                                            .app
                                            .tool_call_detail_handle(panel_idx, tool_use_id)
                                        {
                                            app.open_task_panel_maximized(&handle);
                                        } else if !app.open_tool_output_panel(
                                            panel_idx,
                                            tool_use_id,
                                        ) {
                                            app.app.toggle_tool_call_content(
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
                                                    if let Some(tx) = &handle.control_tx {
                                                        let _ = tx.send(TuiControl::TermResize {
                                                            handle: app.wm
                                                                .content_kind(id)
                                                                .and_then(|kind| match kind {
                                                                    crate::wm::WindowContent::Task { handle, .. } => Some(handle.clone()),
                                                                    _ => None,
                                                                })
                                                                .unwrap_or_default(),
                                                            rows: inner_rows,
                                                            cols: inner_cols,
                                                        });
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
                                app.app.hovered_submission = app
                                    .app
                                    .submission_queue_hitmap
                                    .row_at(me.column, me.row);
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
                                    && matches!(
                                        app.app.items.get(idx),
                                        Some(
                                            crate::app::OutputItem::Thinking { .. }
                                                | crate::app::OutputItem::CompactionSummary { .. }
                                        )
                                    )
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
                                            if let Some(tx) = &handle.control_tx {
                                                let _ = tx.send(TuiControl::TermResize {
                                                    handle: app.wm
                                                        .content_kind(id)
                                                        .and_then(|kind| match kind {
                                                            crate::wm::WindowContent::Task { handle, .. } => Some(handle.clone()),
                                                            _ => None,
                                                        })
                                                        .unwrap_or_default(),
                                                    rows: inner_rows,
                                                    cols: inner_cols,
                                                });
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
                } else if scroll_delta > 0 {
                    app.app.scroll_down(scroll_delta as u32);
                }
            }
            frame = semantic_rx.recv() => {
                if let Some(frame) = frame {
                    // Spawn per-FlowRun frame_tx forwarders when new FlowRuns appear.
                    let new_handle: Option<String> = match &frame {
                        StreamFrame::FlowStart { parent_run_id: None, .. } => {
                            session_for_sub.as_ref().and_then(|s| s.current_root())
                        }
                        StreamFrame::SubAgentStarted { handle, .. } => Some(handle.clone()),
                        _ => None,
                    };
                    if let Some(h) = new_handle
                        && let Some(sess) = &session_for_sub
                        && let Ok(entry) = sess.flow_registry.lookup(&h)
                    {
                        let semantic_tx = frame_semantic_tx.clone();
                        let bulk_tx = frame_bulk_tx.clone();
                        let mut rx = entry.frame_tx.subscribe();
                        tokio::spawn(async move {
                            loop {
                                match rx.recv().await {
                                    Ok(frame) => {
                                        if !forward_ui_frame(frame, &semantic_tx, &bulk_tx).await {
                                            break;
                                        }
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                        continue;
                                    }
                                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                                }
                            }
                        });
                    }
                    app.app.apply_stream_frame(frame);
                } else {
                    break;
                }
            }
            frame = bulk_rx.recv() => {
                if let Some(frame) = frame {
                    app.app.apply_stream_frame(frame);
                    for _ in 0..255 {
                        let Ok(frame) = bulk_rx.try_recv() else {
                            break;
                        };
                        app.app.apply_stream_frame(frame);
                    }
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
            _ = wait_queued_submission_change(handle.queued_submission_rx.as_mut()) => {
                if let Some(rx) = handle.queued_submission_rx.as_mut() {
                    let selected_id = app
                        .app
                        .queued_submissions
                        .get(app.app.selected_submission)
                        .map(|item| item.id.clone());
                    app.app.queued_submissions = rx.borrow().clone();
                    if app.app.queued_submissions.is_empty() {
                        app.app.submission_focus = false;
                        app.app.selected_submission = 0;
                        app.app.queued_submission_edit = None;
                    } else {
                        app.app.selected_submission = selected_id
                            .and_then(|id| {
                                app.app
                                    .queued_submissions
                                    .iter()
                                    .position(|item| item.id == id)
                            })
                            .unwrap_or_else(|| {
                                app.app
                                    .selected_submission
                                    .min(app.app.queued_submissions.len() - 1)
                            });
                        if app.app.queued_submission_edit.as_ref().is_some_and(|edit| {
                            !app.app.queued_submissions.iter().any(|item| item.id == edit.id)
                        }) {
                            app.app.queued_submission_edit = None;
                        }
                    }
                    app.app.mark_visual_dirty();
                }
            }
            inj = recv_injection(handle.injection_rx.as_mut()) => {
                if let Some(inj) = inj {
                    // Keep only pending injections, drop consumed/cancelled ones.
                    if matches!(inj.state, atman_runtime::injection::InjectionState::Pending) {
                        if !app.app.pending_injections.iter().any(|i| i.id == inj.id) {
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
                    let latest = rx.borrow().clone();
                    match (latest, app.wm.modals.compact_review.is_some()) {
                        (Some(pending), false) => {
                            app.wm.modals.compact_review = Some(
                                crate::compact_review_modal::CompactReviewModal::new(pending),
                            );
                        }
                        (Some(pending), true) => {
                            if app
                                .wm
                                .modals
                                .compact_review
                                .as_ref()
                                .is_some_and(|m| m.pending.review_id != pending.review_id)
                            {
                                app.wm.modals.compact_review = Some(
                                    crate::compact_review_modal::CompactReviewModal::new(pending),
                                );
                            }
                        }
                        (None, _) => {
                            app.wm.modals.compact_review = None;
                        }
                    }
                }
            }
            _ = wait_form_change(handle.form_rx.as_mut()) => {
                if let Some(rx) = handle.form_rx.as_mut() {
                    let latest = rx.borrow().clone();
                    if let Some(target) = latest.first().cloned()
                        && app.wm.modals.form_modal.active_form_id()
                            != Some(target.form_id.as_str())
                    {
                        app.wm.modals.form_modal.attach(target);
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
                            let rows = key_handler::enumerate_session_rows(&app, scope);
                            app.wm.modals.session_switcher.open_with(rows, scope);
                        }
                        TuiCommand::SessionNameUpdated(name) => {
                            app.app.session_name = Some(name);
                            if app.wm.modals.session_switcher.open {
                                let scope = app.wm.modals.session_switcher.scope;
                                let rows = key_handler::enumerate_session_rows(&app, scope);
                                app.wm.modals.session_switcher.set_rows(rows);
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
                        TuiCommand::ModelMutationResult { request, result } => {
                            apply_model_mutation_result(&mut app, request, result);
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
                        TuiCommand::McpReloaded => {
                            app.app.push_toast(
                                "MCP servers reloaded",
                                app::NoteLevel::Success,
                                std::time::Duration::from_secs(3),
                                app::ToastPosition::TopRight,
                            );
                        }
                        TuiCommand::KnowledgeResult(result) => {
                            let mut state = app.app.knowledge_state.lock().unwrap();
                            match result {
                                Ok((confessions, rules)) => {
                                    state.message = format!("{} confessions · {} rules", confessions.len(), rules.len());
                                    state.confessions = confessions;
                                    state.rules = rules;
                                }
                                Err(error) => state.message = error,
                            }
                        }
                        TuiCommand::ConfessionHistoryResult { id, result } => {
                            let mut state = app.app.knowledge_state.lock().unwrap();
                            match result {
                                Ok(changes) => { state.history_id = Some(id); state.history = changes; }
                                Err(error) => state.message = error,
                            }
                        }
                        TuiCommand::ConfessionSaved(id) => {
                            app.app.knowledge_state.lock().unwrap().saved_id = Some(id);
                        }
                        TuiCommand::ConfessionSaveFailed { id, error } => {
                            let mut state = app.app.knowledge_state.lock().unwrap();
                            state.failed_save_id = Some(id);
                            state.message = error;
                        }
                        TuiCommand::OrganizationResult { request_id, result } => {
                            let mut state = app.app.knowledge_state.lock().unwrap();
                            if request_id != state.organize_request { continue; }
                            state.loading = false;
                            match result {
                                Ok(proposals) => {
                                    state.message = format!("{} suggestions · select with Space, apply with A", proposals.len());
                                    state.proposals = proposals;
                                }
                                Err(error) => state.message = error,
                            }
                        }
                        TuiCommand::OrganizationApplied => {
                            let mut state = app.app.knowledge_state.lock().unwrap();
                            state.applying = false;
                            state.proposals.clear();
                            state.message = "Organization applied".into();
                        }
                        TuiCommand::OrganizationApplyFailed(error) => {
                            let mut state = app.app.knowledge_state.lock().unwrap();
                            state.applying = false;
                            state.message = error;
                        }
                        TuiCommand::QueueMutationRejected(message) => {
                            app.app.push_toast(
                                format!("queue update rejected: {message}"),
                                app::NoteLevel::Warn,
                                std::time::Duration::from_secs(4),
                                app::ToastPosition::TopRight,
                            );
                        }
                        TuiCommand::Toast { message, level } => {
                            app.app.push_toast(
                                message,
                                level,
                                std::time::Duration::from_secs(4),
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
            Ok(crate::ProviderMutationSuccess::ConfigRemoved { name }) => {
                (format!("{name} removed"), app::NoteLevel::Success)
            }
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

fn apply_model_mutation_result(
    app: &mut UiState,
    request: crate::ModelMutationRequest,
    result: Result<crate::ModelMutationSuccess, String>,
) {
    if !app
        .wm
        .modals
        .model_manager
        .resolve_mutation(&request, &result)
    {
        return;
    }
    let matched = matches!(
        (&request.action, &result),
        (
            crate::ModelMutation::Upsert { name, .. },
            Ok(crate::ModelMutationSuccess::Saved { name: saved })
        ) if name == saved
    ) || matches!(
        (&request.action, &result),
        (
            crate::ModelMutation::Remove { name },
            Ok(crate::ModelMutationSuccess::Removed { name: removed })
        ) if name == removed
    );
    let (message, level) = match result {
        Ok(crate::ModelMutationSuccess::Saved { name }) if matched => {
            apply_provider_catalog_changed(app, None);
            (format!("{name} saved"), app::NoteLevel::Success)
        }
        Ok(crate::ModelMutationSuccess::Removed { name }) if matched => {
            apply_provider_catalog_changed(app, None);
            (format!("{name} removed"), app::NoteLevel::Success)
        }
        Ok(_) => (
            "model mutation returned a mismatched result".to_string(),
            app::NoteLevel::Error,
        ),
        Err(error) => (
            format!("model update failed: {error}"),
            app::NoteLevel::Error,
        ),
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

fn handle_startup_session_mouse(
    app: &mut AppState,
    event: &crossterm::event::MouseEvent,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) -> bool {
    if app.startup_intro.is_some()
        || !matches!(app.items.first(), Some(app::OutputItem::StartupCard { .. }))
    {
        app.startup_focus = app::StartupFocus::Input;
        app.startup_hovered_session = None;
        app.startup_last_click = None;
        return false;
    }
    let hit = app
        .startup_session_rects
        .iter()
        .position(|rect| rect_contains(*rect, event.column, event.row));
    app.startup_hovered_session = hit;
    let in_container = app
        .startup_container_rect
        .is_some_and(|rect| rect_contains(rect, event.column, event.row));
    let Some(index) = hit else {
        if event.kind == MouseEventKind::Down(MouseButton::Left) {
            app.startup_last_click = None;
        }
        return in_container;
    };
    if event.kind == MouseEventKind::Down(MouseButton::Left) {
        let now = std::time::Instant::now();
        let double_click = app.startup_last_click.is_some_and(|(last_index, at)| {
            last_index == index && now.duration_since(at) <= std::time::Duration::from_millis(400)
        });
        app.startup_focus = app::StartupFocus::Recent;
        app.startup_selected_session = index;
        app.startup_last_click = Some((index, now));
        app.popup.close();
        if double_click
            && let Some(session_id) = app.items.first().and_then(|item| match item {
                app::OutputItem::StartupCard { recent, .. } => {
                    recent.get(index).map(|entry| entry.session_id.clone())
                }
                _ => None,
            })
        {
            key_handler::request_session_switch(app, control_tx, session_id);
        }
    }
    true
}

pub(crate) async fn wait_queued_submission_change(
    rx: Option<&mut tokio::sync::watch::Receiver<Vec<atman_runtime::QueuedSubmissionView>>>,
) {
    match rx {
        Some(r) => {
            let _ = r.changed().await;
        }
        None => std::future::pending().await,
    }
}

pub(crate) async fn wait_compact_review_change(
    rx: Option<&mut tokio::sync::watch::Receiver<Option<atman_runtime::PendingCompactReview>>>,
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

fn is_bulk_ui_frame(frame: &StreamFrame) -> bool {
    matches!(
        frame,
        StreamFrame::BashChunk { .. } | StreamFrame::TerminalChunk { .. }
    )
}

async fn forward_ui_frame(
    frame: StreamFrame,
    semantic_tx: &tokio::sync::mpsc::Sender<StreamFrame>,
    bulk_tx: &tokio::sync::mpsc::Sender<StreamFrame>,
) -> bool {
    if is_bulk_ui_frame(&frame) {
        match bulk_tx.try_send(frame) {
            Ok(()) | Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => true,
            Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => false,
        }
    } else {
        semantic_tx.send(frame).await.is_ok()
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

    fn startup_app() -> AppState {
        let mut app = AppState::new("current".into(), None).with_initial_items(vec![
            app::OutputItem::StartupCard {
                version: "1.0.0".into(),
                recent: vec![app::StartupSessionEntry {
                    session_id: "target".into(),
                    short_id: "target00".into(),
                    goal: Some("resume work".into()),
                    project: Some("project".into()),
                    age_label: "1m ago".into(),
                    event_count: 7,
                }],
            },
        ]);
        app.startup_container_rect = Some(ratatui::layout::Rect::new(9, 18, 42, 9));
        app.startup_session_rects = vec![ratatui::layout::Rect::new(10, 20, 40, 4)];
        app
    }

    fn startup_mouse(kind: MouseEventKind, row: u16) -> crossterm::event::MouseEvent {
        crossterm::event::MouseEvent {
            kind,
            column: 20,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }
    }

    #[test]
    fn startup_session_four_rows_single_click_selects_and_double_click_switches() {
        for row in 20..24 {
            let mut app = startup_app();
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let click = startup_mouse(MouseEventKind::Down(MouseButton::Left), row);

            assert!(handle_startup_session_mouse(&mut app, &click, Some(&tx)));
            assert_eq!(app.startup_focus, app::StartupFocus::Recent);
            assert_eq!(app.startup_selected_session, 0);
            assert_eq!(app.startup_hovered_session, Some(0));
            assert!(!app.should_quit);
            assert!(rx.try_recv().is_err());

            assert!(handle_startup_session_mouse(&mut app, &click, Some(&tx)));
            assert!(app.should_quit);
            assert!(matches!(
                rx.try_recv(),
                Ok(TuiControl::SwitchSession { sid, .. }) if sid == "target"
            ));
        }
    }

    #[test]
    fn startup_session_container_consumes_click_and_hover_clears_outside() {
        let mut app = startup_app();
        assert!(handle_startup_session_mouse(
            &mut app,
            &startup_mouse(MouseEventKind::Moved, 20),
            None,
        ));
        assert_eq!(app.startup_hovered_session, Some(0));
        assert!(handle_startup_session_mouse(
            &mut app,
            &startup_mouse(MouseEventKind::Down(MouseButton::Left), 19),
            None,
        ));
        assert_eq!(app.startup_hovered_session, None);
        assert!(!handle_startup_session_mouse(
            &mut app,
            &startup_mouse(MouseEventKind::Moved, 30),
            None,
        ));
        assert_eq!(app.startup_hovered_session, None);

        app.startup_focus = app::StartupFocus::Recent;
        app.startup_intro = Some(app::StartupIntro {
            started_at: std::time::Instant::now(),
            version: "1.0.0".into(),
            recent: Vec::new(),
        });
        assert!(!handle_startup_session_mouse(
            &mut app,
            &startup_mouse(MouseEventKind::Down(MouseButton::Left), 20),
            None,
        ));
        assert_eq!(app.startup_focus, app::StartupFocus::Input);
        assert!(!app.should_quit);
    }

    #[tokio::test]
    async fn saturated_bulk_lane_does_not_block_semantic_frames() {
        let (semantic_tx, mut semantic_rx) = tokio::sync::mpsc::channel(1);
        let (bulk_tx, _bulk_rx) = tokio::sync::mpsc::channel(1);
        let bulk = StreamFrame::BashChunk {
            handle: "bg".into(),
            kind: "stdout".into(),
            line: "data".into(),
            call_intent: None,
            tool_use_id: None,
            run_id: None,
        };
        assert!(forward_ui_frame(bulk.clone(), &semantic_tx, &bulk_tx).await);
        assert!(forward_ui_frame(bulk, &semantic_tx, &bulk_tx).await);

        assert!(forward_ui_frame(StreamFrame::LlmRetry, &semantic_tx, &bulk_tx).await);
        assert!(matches!(
            semantic_rx.recv().await,
            Some(StreamFrame::LlmRetry)
        ));
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
