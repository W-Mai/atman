mod views;

use crate::control::FlowPlacement;
use crate::{FlowViewSource, UiState};
use std::{io::Stdout, time::Duration};
use views::{ViewKey, Workspace};

use anyhow::Result;
use crossterm::event::{Event as CtEvent, KeyModifiers, MouseButton, MouseEventKind};
use crossterm::terminal::{BeginSynchronizedUpdate, EndSynchronizedUpdate};
use crossterm::{ExecutableCommand, QueueableCommand};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::mpsc;

use crate::app;
use crate::app::AppState;
use crate::input::{InputEditor, cursor_from_wrapped};
use crate::key_handler;
use crate::keys::{KeyAction, map as map_key};
use crate::layout;
use crate::render::{ANIMATION_TICK_MS, check_latest_release, rect_contains, render_frame};
use crate::{
    TuiCommand, TuiControl, TuiHandle, TuiNote, build_sigterm_stream, spawn_event_reader,
    wait_sigterm,
};
use atman_runtime::stream::StreamFrame;

fn is_live_instance_frame(frame: &StreamFrame) -> bool {
    matches!(
        frame,
        StreamFrame::PermissionRequestCreated { .. }
            | StreamFrame::PermissionRequestTargeted { .. }
            | StreamFrame::PermissionRequestDeferred { .. }
            | StreamFrame::PermissionRequestApproved { .. }
            | StreamFrame::PermissionRequestDenied { .. }
            | StreamFrame::PermissionRequestCancelled { .. }
            | StreamFrame::PermissionGroupCreated { .. }
            | StreamFrame::PermissionGroupUpdated { .. }
            | StreamFrame::PermissionGroupResolved { .. }
            | StreamFrame::PermissionGrantCreated { .. }
            | StreamFrame::PermissionGrantExpired { .. }
    )
}

async fn recv_input_event(
    rx: &mut mpsc::UnboundedReceiver<std::io::Result<CtEvent>>,
    pending: &mut Option<std::io::Result<CtEvent>>,
) -> Option<std::io::Result<CtEvent>> {
    let mut event = match pending.take() {
        Some(event) => event,
        None => rx.recv().await?,
    };
    if let Ok(CtEvent::Mouse(mouse)) = &event
        && matches!(mouse.kind, MouseEventKind::Drag(_))
    {
        let kind = mouse.kind;
        loop {
            match rx.try_recv() {
                Ok(next) if matches!(&next, Ok(CtEvent::Mouse(mouse)) if mouse.kind == kind) => {
                    event = next;
                }
                Ok(next) => {
                    *pending = Some(next);
                    break;
                }
                Err(_) => break,
            }
        }
    }
    Some(event)
}

fn attach_view(
    handle: &mut TuiHandle,
    preferences: &crate::states::PreferenceStore,
) -> Result<(UiState, InputEditor)> {
    let mut app = AppState::new(handle.session_id.clone(), handle.goal.clone())
        .with_initial_items(std::mem::take(&mut handle.initial_items))
        .with_session_dir(handle.session_dir.clone())
        .with_session_identity(handle.session_name.clone(), handle.project_root.clone())
        .with_flow_names(std::mem::take(&mut handle.flow_names))
        .with_session(handle.session.clone())
        .with_trust(handle.trust.clone());
    app.settings = handle.settings.clone();
    app.layout_cache.enable_background();
    app.presentation_settings
        .get_or_insert_with(Default::default);
    app.host_controls = handle
        .control_tx
        .as_ref()
        .and_then(crate::ControlSender::control_work)
        .unwrap_or_default();
    app.continued_session = handle.continued_session.clone();
    app.project_sidebar.lock().unwrap().directory.projects = std::mem::take(&mut handle.projects);
    if let Some(tr) = handle.task_registry.take() {
        app = app.with_task_registry(tr);
    }
    let mut app = UiState::new(app);
    refresh_activity(&mut app, handle);
    refresh_session_name(&mut app, handle);
    app.app.preferences = Some(preferences.bind(&mut app.app));
    open_onboarding_if_ready(&mut app, handle);
    app.app.startup_card = handle.startup_card.take();
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
        app.app.sync_queue(
            rx.borrow()
                .iter()
                .cloned()
                .map(crate::submission_queue::QueueItem::Session)
                .collect(),
        );
    }
    prepare_attachment_source(handle);
    let mut editor = InputEditor::default();
    if handle.flow_view.is_none()
        && let Some(sess) = handle.session.as_ref()
    {
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
    }
    app.sync_composer_images(&mut editor);
    if handle.flow_view.is_some() {
        refresh_instance_live(&mut app, &mut editor, handle);
        refresh_live_output(&mut app, handle)?;
    }
    Ok((app, editor))
}

fn prepare_attachment_source(handle: &mut TuiHandle) {
    if let Some(source) = handle.flow_view.take() {
        let output = handle
            .session
            .as_ref()
            .map(|session| session.output_store());
        handle.flow_view = Some(source.prepare_initial(handle.session.clone(), output, true));
    }
}

fn defer_navigation_reply(
    ui: &mut UiState,
    ticket: Option<crate::control::ControlTicket>,
    mut command: TuiCommand,
) {
    match &mut command {
        TuiCommand::FocusFlow {
            attachment: Some(handle),
            ..
        }
        | TuiCommand::SessionCreated {
            attachment: handle, ..
        } => prepare_attachment_source(handle),
        _ => {}
    }
    ui.navigation_reply = Some((ticket, command));
    refresh_navigation_progress(&mut ui.app, None, true);
}

enum CatalogReply {
    Applied,
    Focus(crate::control::ViewAddress),
    SwitchSessions,
    Failed(String),
}

fn receive_catalog_reply(
    navigation: &mut Option<crate::flow_hub::FlowHub>,
    history: &mut crate::flow_hub::History,
    ticket: Option<&crate::control::ControlTicket>,
    result: Result<Box<crate::flow_hub::FlowHubPage>, String>,
    app: &mut AppState,
    control: Option<&crate::ControlSender>,
) -> CatalogReply {
    if !crate::panel_registry::EXTENDED
        && navigation
            .as_ref()
            .is_some_and(|hub| hub.owns_reply(ticket))
    {
        match result
            .as_deref()
            .map_err(String::as_str)
            .map(|page| page.session_destination())
        {
            Ok(Some(Ok(flow))) => {
                return CatalogReply::Focus(flow);
            }
            Ok(Some(Err(error))) | Err(error) => {
                let error = error.to_owned();
                return CatalogReply::Failed(error);
            }
            Ok(None) => {}
        }
        if matches!(
            result.as_deref(),
            Ok(crate::flow_hub::FlowHubPage::Sessions(_))
        ) {
            *navigation = None;
            return CatalogReply::SwitchSessions;
        }
    }
    if let Some(hub) = navigation.as_mut() {
        let accepted = hub.owns_reply(ticket) && result.is_ok();
        history.receive(hub, ticket, result);
        if accepted
            && let Some(flow) = control
                .and_then(|tx| tx.binding())
                .and_then(|binding| binding.flow.instance())
            && let Some(snapshot) = hub.neighbors_for(flow)
        {
            app.flow_links.update(snapshot);
        }
    }
    CatalogReply::Applied
}

fn navigation_failed(app: &mut AppState, error: String) {
    app.push_toast(
        error,
        crate::app::NoteLevel::Error,
        Duration::from_secs(6),
        crate::app::ToastPosition::TopRight,
    );
}

#[cfg(test)]
async fn initialize_view(handle: &mut TuiHandle) -> Result<(UiState, InputEditor)> {
    let mut preferences = crate::states::PreferenceStore::start();
    let (mut ui, mut editor) = attach_view(handle, &preferences)?;
    preferences.ready().await;
    handle.settings.ready().await;
    apply_provider_catalog_changed(&mut ui, None);
    ui.app.refresh_preferences();
    open_onboarding_if_ready(&mut ui, handle);
    finish_test_attachment(&mut ui, &mut editor, handle).await?;
    Ok((ui, editor))
}

fn open_onboarding_if_ready(ui: &mut UiState, handle: &mut TuiHandle) {
    if handle.onboarding_recommended
        && !ui.app.settings.is_waiting()
        && ui.app.settings.load_auth().is_ok()
        && ui
            .app
            .preferences
            .as_ref()
            .is_none_or(|binding| !binding.needs_initial_load())
    {
        handle.onboarding_recommended = false;
        if !ui.app.onboarding_skipped {
            ui.wm.modals.onboarding_open = true;
            if let Some(tx) = &handle.control_tx {
                let _ = tx.send(TuiControl::OnboardingInit);
            }
        }
    }
}

#[cfg(test)]
async fn finish_test_attachment(
    ui: &mut UiState,
    editor: &mut InputEditor,
    handle: &mut TuiHandle,
) -> Result<()> {
    if ui.app.flow_loading.is_some() {
        let result = handle
            .flow_view
            .as_mut()
            .unwrap()
            .next()
            .await
            .and_then(|content| ui.app.sync_flow_view(content));
        if let Err(error) = result {
            disconnect_view(ui, &error);
            return Err(error.into());
        }
        refresh_instance_live(ui, editor, handle);
        refresh_live_output(ui, handle)?;
    }
    Ok(())
}

fn refresh_session_name(ui: &mut UiState, handle: &mut TuiHandle) {
    if let Some(rx) = &mut handle.session_name_rx {
        let name = rx.borrow_and_update().clone();
        if let Some(binding) = handle.control_tx.as_ref().and_then(|tx| tx.binding()) {
            ui.wm
                .modals
                .session_switcher
                .name_changed(&binding.flow.session_target(), name.as_deref());
        }
        ui.app.session_name = name.clone();
        handle.session_name = name;
    }
}

fn refresh_activity(ui: &mut UiState, handle: &mut TuiHandle) {
    if let Some(reader) = &mut handle.activity {
        ui.app.sync_activity(reader.snapshot());
    }
}

async fn activity_changed(
    reader: &mut Option<atman_runtime::flow_runtime::activity::ActivityReader>,
) {
    match reader {
        Some(reader) => reader.changed().await,
        None => std::future::pending().await,
    }
}

fn refresh_instance_live(ui: &mut UiState, editor: &mut InputEditor, handle: &TuiHandle) {
    ui.app.flow_loading = handle.flow_view.as_ref().and_then(FlowViewSource::loading);
    if let Some(history) = ui.app.prepared_input_history.take() {
        editor.seed_history(history);
    }
    ui.app.sync_instance_permissions();
    if let Some(rx) = &handle.context_rx {
        ui.app.replace_context_snapshot(rx.borrow().clone());
    }
    if let Some(rx) = &handle.trust_rx {
        let theme = ui.app.trust.theme;
        ui.app.trust = rx.borrow().clone();
        ui.app.trust.theme = theme;
    }
    ui.app.sync_composer_images(editor);
    if let Some(rx) = &handle.form_rx {
        ui.wm.modals.form_modal.sync_runtime(rx.borrow().first());
    }
    if let Some(rx) = &handle.compact_review_rx {
        match rx.borrow().as_ref() {
            Some(pending)
                if ui
                    .wm
                    .modals
                    .compact_review
                    .as_ref()
                    .is_none_or(|modal| modal.pending.review_id != pending.review_id) =>
            {
                ui.wm.modals.compact_review = Some(
                    crate::compact_review_modal::CompactReviewModal::new(pending.clone()),
                );
            }
            None => ui.wm.modals.compact_review = None,
            _ => {}
        }
    }
}

fn refresh_live_output(ui: &mut UiState, handle: &mut TuiHandle) -> std::io::Result<()> {
    if let Some(reader) = &mut handle.live_output {
        let snapshot = reader.snapshot();
        if ui.app.flow_loading.is_none() {
            ui.app.sync_live_output(snapshot)?;
        }
    }
    Ok(())
}

fn disconnect_view(ui: &mut UiState, error: &std::io::Error) {
    ui.app.flow_loading = None;
    ui.app.flow_view_error = Some(error.to_string());
    ui.app
        .status_notes
        .insert("flow".into(), "Disconnected · showing last commit".into());
    ui.app.push_toast(
        format!(
            "View disconnected: {error}. Open Switch session and select this session to reconnect."
        ),
        crate::app::NoteLevel::Error,
        std::time::Duration::from_secs(8),
        crate::app::ToastPosition::TopRight,
    );
}

fn update_live_output(ui: &mut UiState, handle: &mut TuiHandle) {
    if let Err(error) = refresh_live_output(ui, handle) {
        handle.live_output = None;
        ui.app.push_toast(
            format!("Preview disconnected: {error}"),
            crate::app::NoteLevel::Error,
            std::time::Duration::from_secs(8),
            crate::app::ToastPosition::TopRight,
        );
    }
}

/// Only the failed subscription stops. Drafts, controls and other views stay attached.
fn update_flow_view(ui: &mut UiState, handle: &mut TuiHandle, result: std::io::Result<()>) {
    match result {
        Ok(()) => {
            update_live_output(ui, handle);
            if let Some(snapshot) = ui.app.items.snapshot() {
                ui.wm.flow_snapshot(snapshot.clone());
            }
        }
        Err(error) => disconnect_view(ui, &error),
    }
    refresh_flow_progress(&mut ui.app, handle.flow_view.as_ref());
}

fn refresh_flow_progress(app: &mut AppState, source: Option<&FlowViewSource>) {
    app.flow_loading = source.and_then(FlowViewSource::loading);
    let started = source
        .filter(|_| app.flow_view_error.is_none())
        .and_then(FlowViewSource::read_started_at);
    app.flow_refreshing = started.is_some();
    if started.is_some_and(|started| started.elapsed() >= crate::loading::REFRESH_DELAY) {
        app.status_notes.insert(
            "view-read".into(),
            format!("{} Updating", crate::loading::glyph(app.animation_frame)),
        );
    } else {
        app.status_notes.remove("view-read");
    }
}

fn refresh_navigation_progress(
    app: &mut AppState,
    navigation: Option<&crate::flow_hub::FlowHub>,
    completed: bool,
) {
    let started = navigation.and_then(|hub| hub.opening_started_at());
    if !completed
        && started.is_some_and(|started| started.elapsed() >= crate::loading::REFRESH_DELAY)
    {
        app.status_notes.insert(
            "view-open".into(),
            format!("{} Opening", crate::loading::glyph(app.animation_frame)),
        );
    } else {
        app.status_notes.remove("view-open");
    }
}

async fn live_output_change(
    reader: &mut Option<atman_runtime::flow_runtime::live::LiveOutputReader>,
) {
    match reader {
        Some(reader) => reader.changed().await,
        None => std::future::pending().await,
    }
}

pub(crate) async fn run_frames(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    mut handle: TuiHandle,
    preferences: crate::states::PreferenceStore,
    clipboard_work: crate::clipboard::images::ClipboardWork,
    clipboard_copies: crate::clipboard::writes::CopyService,
) -> Result<()> {
    let (mut app, mut editor) = attach_view(&mut handle, &preferences)?;
    app.app.image_paste.work = clipboard_work.clone();
    app.app.clipboard_copies.service = Some(clipboard_copies.clone());
    let mut clipboard_changes = clipboard_work.subscribe();
    let mut copy_changes = clipboard_copies.work.subscribe();
    let mut render_changes = crate::render_work::subscribe();
    let mut control_changes = app.app.host_controls.subscribe();
    let persistent = handle.flow_view.is_some();
    let mut views = Workspace::new(&handle);
    app.app
        .project_sidebar
        .lock()
        .unwrap()
        .request(handle.control_tx.as_ref(), None);
    let mut hub: Option<crate::flow_hub::FlowHub> = None;
    let (mut key_events, _reader_guard) = spawn_event_reader();
    let mut input_closed = false;
    let mut exit_requested = false;
    let mut mouse_captured = true;
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
    let mut selection_tick = tokio::time::interval(std::time::Duration::from_millis(50));
    toast_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    selection_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut pending_input_event = None;
    let mut goal_changes = handle
        .session
        .as_ref()
        .filter(|_| !persistent)
        .map(|s| s.goal_control.subscribe());

    let mut update_check = tokio::spawn(check_latest_release());

    // Keep bulk process output off the semantic lane so a noisy command cannot
    // delay lifecycle, completion, approval, or cancellation frames.
    let (semantic_tx, mut semantic_rx) = tokio::sync::mpsc::channel::<StreamFrame>(512);
    let (bulk_tx, mut bulk_rx) = tokio::sync::mpsc::channel::<StreamFrame>(256);
    if !persistent && let Some(srx) = handle.stream_rx.as_mut() {
        let semantic_tx = semantic_tx.clone();
        let bulk_tx = bulk_tx.clone();
        let replacement = srx.resubscribe();
        let mut srx = std::mem::replace(srx, replacement);
        tokio::spawn(async move {
            loop {
                match srx.recv().await {
                    Ok(frame) => {
                        if persistent && !is_live_instance_frame(&frame) {
                            continue;
                        }
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

    loop {
        // Exit belongs to the workspace even when a pending write allows further focus changes.
        exit_requested |= app.app.should_quit;
        app.app.should_quit = exit_requested;
        if exit_requested {
            clipboard_work.close();
            app.app.host_controls.close_publications();
        }
        if exit_requested && clipboard_work.is_publishing() {
            let notice = format!(
                "{} Finishing images",
                crate::loading::glyph(app.app.animation_frame)
            );
            app.app.status_notes.insert("attachments".into(), notice);
        } else {
            app.app.status_notes.remove("attachments");
        }
        app.app.refresh_preferences();
        app.app.refresh_settings_status();
        if app.app.host_controls.is_waiting() {
            let glyph = crate::loading::glyph(app.app.animation_frame);
            app.app.status_notes.insert(
                "controls".into(),
                if exit_requested {
                    format!("{glyph} Finishing")
                } else {
                    glyph.to_string()
                },
            );
        } else {
            app.app.status_notes.remove("controls");
        }
        app.app
            .refresh_clipboard_status(exit_requested && clipboard_copies.work.is_waiting());
        open_onboarding_if_ready(&mut app, &mut handle);
        refresh_flow_progress(&mut app.app, handle.flow_view.as_ref());
        if let Some(history) = app.app.prepared_input_history.take() {
            editor.seed_history(history);
        }
        if let Some((window, request, sort)) = app.wm.modals.form_modal.take_project_sort() {
            app.wm.apply_project_sort(window, &request, sort);
            app.app.mark_visual_dirty();
        }
        if let Some(sort) = app.wm.modals.form_modal.take_activity_sort() {
            app.app.flow_activity.set_sort(sort);
            app.app.last_task_panel_hitmap = Default::default();
            app.app.mark_visual_dirty();
        }
        if app.app.flow_activity.take_sort_request() {
            if let Err(error) = app
                .wm
                .modals
                .form_modal
                .open_activity_sort(app.app.flow_activity.sort)
            {
                app.app.push_note(error, crate::app::NoteLevel::Error);
            }
            app.wm.sync_modals();
            app.app.mark_visual_dirty();
        }
        if std::mem::take(&mut app.app.startup_open_projects) {
            app.wm.open_project_hub(
                app.app.maximized_canvas(),
                app.app.project_sidebar.clone(),
                app.app.project_root.as_deref(),
                handle.control_tx.as_ref(),
            );
        }
        if let Some(action) = app.app.open_flow_hub.take() {
            let action = crate::panel_registry::navigation(action);
            if matches!(action, crate::flow_hub::Action::SwitchSessions) {
                if !crate::panel_registry::EXTENDED {
                    hub = None;
                }
                app.wm
                    .modals
                    .session_switcher
                    .open_for(&app.app, handle.control_tx.as_ref());
            } else if !matches!(action, crate::flow_hub::Action::None)
                && let Some(tx) = handle.control_tx.as_ref()
                && let Some(binding) = tx.binding()
            {
                if let Some(previous) = hub.take() {
                    views.directory_history().save(previous);
                }
                let (mut opened, action) = views
                    .directory_history()
                    .open(action, binding.flow.directory());
                opened.opening_focus = app.wm.focused_id();
                let reuse = hub_view_key(&action).and_then(|key| views.binding(&key, &handle));
                dispatch_hub_action(&mut opened, action, tx, reuse);
                hub = Some(opened);
            }
        }
        if let Some(error) = hub
            .as_ref()
            .and_then(|hub| hub.navigation_error())
            .map(str::to_owned)
        {
            hub = None;
            app.app.push_toast(
                error,
                crate::app::NoteLevel::Error,
                std::time::Duration::from_secs(6),
                crate::app::ToastPosition::TopRight,
            );
        }
        refresh_navigation_progress(&mut app.app, hub.as_ref(), app.navigation_reply.is_some());
        app.app.tick_toasts();
        // Never wait for an OSC write; its completion wakes this loop for the next frame.
        if let Some(_output) = crate::clipboard::try_output() {
            if mouse_captured != app.app.mouse_captured {
                crate::terminal_guard::set_mouse_capture(app.app.mouse_captured)?;
                mouse_captured = app.app.mouse_captured;
            }
            terminal.backend_mut().queue(BeginSynchronizedUpdate)?;
            let mut composition = Ok(());
            let draw_result = terminal
                .draw(|f| {
                    if let Some(hub) = hub.as_mut().filter(|hub| hub.visible()) {
                        let flow = &handle.control_tx.as_ref().unwrap().binding().unwrap().flow;
                        let area = app.app.project_sidebar.lock().unwrap().layout(f.area());
                        hub.render_in(f, area, flow);
                        app.app.project_sidebar.lock().unwrap().render(
                            f,
                            Some(&flow.session_target()),
                            app.app.animation_frame,
                        );
                        app.wm.render(f, area, &mut app.app);
                    } else {
                        composition = views.render(f, &mut app, &editor);
                    }
                })
                .map(|_| ());
            let cursor_result = if (hub.as_ref().is_some_and(|hub| hub.visible())
                && app.wm.top_kind().is_none())
                || app.app.modal_notification.is_some()
                || (persistent && !views.cursor_visible)
            {
                terminal.hide_cursor()
            } else if let Some(kind) = app.wm.top_kind() {
                if app.wm.modals.cursor_visible(kind) {
                    terminal.show_cursor()
                } else {
                    terminal.hide_cursor()
                }
            } else if app.wm.focused_content_cursor_visible()
                || (app.wm.focused_id().is_none()
                    && (!app.app.submission_focus || app.app.queued_submission_edit.is_some()))
            {
                terminal.show_cursor()
            } else {
                terminal.hide_cursor()
            };
            let sync_result = terminal.backend_mut().execute(EndSynchronizedUpdate);
            draw_result?;
            composition?;
            cursor_result?;
            sync_result?;
            app.app.tick = app.app.tick.wrapping_add(1);
        }

        if exit_requested
            && !preferences.is_waiting()
            && !clipboard_work.is_publishing()
            && !clipboard_copies.work.is_waiting()
            && !app.app.host_controls.is_waiting()
        {
            return preferences.result().map_err(anyhow::Error::msg);
        }

        // Fair polling keeps continuously ready view updates from starving input and replies.
        let mut read_progress = handle
            .flow_view
            .as_ref()
            .and_then(FlowViewSource::subscribe_read_progress);
        let navigation_ready = app.navigation_can_open(hub.as_ref());
        tokio::select! {
            _ = render_changes.changed() => {}
            _ = crate::instance_view::read_progress_changed(&mut read_progress) => {}
            result = app.app.clipboard_copies.next() => {
                app.app.clipboard_copy_ready(result);
            }
            _ = copy_changes.changed() => {}
            (request, result) = app.app.image_paste.next() => {
                app.app.clipboard_image_ready(&mut editor, request, result);
            }
            _ = control_changes.changed() => {}
            _ = clipboard_changes.changed() => {}
            _ = views::changed(handle.session_name_rx.as_mut()) => {
                refresh_session_name(&mut app, &mut handle);
            }
            _ = handle.settings.changed() => {
                apply_provider_catalog_changed(&mut app, None);
            }
            _ = crate::settings::optional_changed(&mut app.app.presentation_settings) => {
                app.app.mark_visual_dirty();
            }
            _ = wait_shutdown(shutdown.as_mut()), if !app.app.should_quit => {
                app.app.should_quit = true;
            }
            _ = wait_sigterm(sigterm.as_mut()), if !app.app.should_quit => {
                app.app.should_quit = true;
            }
            _ = animation_tick.tick(), if app.has_active_animation() || views.has_active_animation() || app.navigation_animates(hub.as_ref()) || app.navigation_text_animates(hub.as_ref()) => {
                app.app.animation_frame = app.app.animation_frame.wrapping_add(1);
                views.tick_animation();
            }
            _ = intro_tick.tick(), if app.app.startup_intro.is_some() || app.app.tail_scroll_needs_tick() || views.animating() => {
                app.app.animation_frame = app.app.animation_frame.wrapping_add(1);
            }
            _ = toast_tick.tick(), if !app.app.toasts.is_empty() => {}
            _ = selection_tick.tick(), if transcript_selection_needs_tick(&app.app) => {
                tick_transcript_selection_drag(&mut app.app);
            }
            latest = poll_update_check(&mut update_check), if !update_check.is_finished() => {
                app.app.latest_release = latest;
            }
            view = async {
                match handle.flow_view.as_mut() {
                    Some(reader) => {
                        reader.set_pending_offset(app.app.pending_page_offset);
                        reader.next().await
                    },
                    None => std::future::pending().await,
                }
            }, if app.app.flow_view_error.is_none() => {
                let result = view.and_then(|view| app.app.sync_flow_view(view));
                update_flow_view(&mut app, &mut handle, result);
            }

            _ = live_output_change(&mut handle.live_output) => {
                update_live_output(&mut app, &mut handle);
            }

            _ = views::permission_change(&mut handle.stream_rx), if persistent => {
                app.app.sync_instance_permissions();
            }
            _ = views.next_background(), if persistent => {}
            _ = app.app.popup.next_update() => { app.app.mark_visual_dirty(); }
            _ = app.wm.modals.session_switcher.next_legacy_update() => { app.app.mark_visual_dirty(); }
            _ = app.wm.modals.form_modal.next_preference_update() => { app.app.mark_visual_dirty(); }
            _ = crate::states::preference_change(&mut app.app.preferences) => {}
            key = recv_input_event(&mut key_events, &mut pending_input_event), if !input_closed => {
                let key: Option<std::io::Result<CtEvent>> = match key {
                    Some(event) => Some(Ok(event?)),
                    None => { input_closed = true; app.app.should_quit = true; continue; },
                };
                if std::env::var_os("ATMAN_TRACE_EVENTS").is_some() {
                    atman_runtime::notify!(debug, "event: {key:?}");
                }
                if hub.as_ref().is_some_and(|hub| hub.visible()) && app.wm.focused_id().is_none() && !app.wm.modals.any_open() && app.app.modal_notification.is_none()
                    && let Some(Ok(event)) = &key {
                    if matches!(event, CtEvent::Key(key) if key.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) && matches!(key.code, crossterm::event::KeyCode::Tab | crossterm::event::KeyCode::BackTab)) {
                        let mut sidebar = app.app.project_sidebar.lock().unwrap();
                        if sidebar.available() { sidebar.focused = !sidebar.focused; continue; }
                    }
                    if crate::project_sidebar::handle_event(&mut app, event, handle.control_tx.as_ref()) { continue; }
                }
                if let Some(opened) = hub.as_mut().filter(|hub| hub.visible())
                    && app.wm.focused_id().is_none() && !app.wm.modals.any_open() && app.app.modal_notification.is_none() {
                    let mut action = match key {
                        Some(Ok(CtEvent::Key(key))) if map_key(key) == KeyAction::Quit => { app.app.should_quit = true; continue; },
                        Some(Ok(CtEvent::Key(key))) => opened.key(map_key(key)),
                        Some(Ok(CtEvent::Mouse(mouse))) => opened.mouse(mouse),
                        Some(Ok(CtEvent::Paste(text))) => opened.paste(&text),
                        _ => crate::flow_hub::Action::None,
                    };
                    if let crate::flow_hub::Action::Load(request) = &action
                        && opened.leaves_directory(request) && opened.restores_directory(request) {
                        action = crate::flow_hub::Action::Load(views.directory_history().request(request.clone()));
                    }
                    let action = crate::panel_registry::navigation(action);
                    if matches!(action, crate::flow_hub::Action::Return) {
                        let (restored, action) = views.directory_history().open(action, crate::flow_hub::HubRequest::Sessions { before: None });
                        *opened = restored;
                        if let Some(tx) = handle.control_tx.as_ref() { dispatch_hub_action(opened, action, tx, None); }
                    }
                    else if matches!(action, crate::flow_hub::Action::SwitchSessions) {
                        if !crate::panel_registry::EXTENDED { hub = None; }
                        app.wm.modals.session_switcher.open_for(&app.app, handle.control_tx.as_ref());
                    }
                    else if matches!(action, crate::flow_hub::Action::Close) { views.directory_history().save(hub.take().unwrap()); }
                    else if let Some(tx) = handle.control_tx.as_ref() { let reuse = hub_view_key(&action).and_then(|key| views.binding(&key, &handle)); dispatch_hub_action(opened, action, tx, reuse); }
                    continue;
                }
                if !views.editing_geometry() && app.wm.focused_id().is_none() && !app.wm.modals.any_open() && app.selection_menu.is_none() && app.app.modal_notification.is_none()
                    && let Some(Ok(event)) = &key
                    && crate::project_sidebar::handle_event(&mut app, event, handle.control_tx.as_ref()) {
                    continue;
                }
                if !views.editing_geometry() && app.wm.focused_id().is_none() && !app.wm.modals.any_open() && app.selection_menu.is_none() && app.app.modal_notification.is_none()
                    && let Some(Ok(event)) = &key
                    && app.app.activity_key(event) {
                    continue;
                }
                if !views.editing_geometry() && app.wm.focused_id().is_none() && !app.wm.modals.any_open() && app.selection_menu.is_none() && app.app.modal_notification.is_none()
                    && let Some(Ok(event)) = &key
                    && !matches!(event, CtEvent::Mouse(mouse) if app.app.composer_rect.is_some_and(|r| r.contains((mouse.column, mouse.row).into())))
                    && app.app.focus_page_event(event) {
                    continue;
                }
                if persistent && !hub.as_ref().is_some_and(|hub| hub.visible()) {
                    let shortcuts = app.wm.focused_id().is_none() && !app.wm.modals.any_open() && app.selection_menu.is_none() && app.app.modal_notification.is_none() && !app.app.submission_focus;
                    if let Some(Ok(event)) = &key {
                        let result = match views.event(event, shortcuts) {
                            views::Action::Pass => Ok(true),
                            views::Action::Consume => Ok(false),
                            views::Action::Focus { key, forward } => { interrupt_prompt = None; views.focus_with_navigation(&mut hub, &mut app, &mut editor, &mut handle, key, None).await.map(|()| forward) },
                            views::Action::Close(key) => views.close_with_navigation(&mut hub, &mut app, &mut editor, &mut handle, &key).map(|()| false),
                            views::Action::Promote(key) => views.focus_with_navigation(&mut hub, &mut app, &mut editor, &mut handle, key, None).await.map(|()| { views.promote(); false }),
                        };
                        match result {
                            Ok(true) => {},
                            Ok(false) => { interrupt_prompt = None; continue; },
                            Err(error) => { app.app.push_toast(format!("{error:#}"), crate::app::NoteLevel::Error, std::time::Duration::from_secs(6), crate::app::ToastPosition::TopRight); continue; },
                        }
                    }
                    let escape = matches!(&key, Some(Ok(CtEvent::Key(key))) if map_key(*key) == KeyAction::Escape)
                        && shortcuts && crate::panel_registry::EXTENDED;
                    let mut clicked = false;
                    if let Some(Ok(CtEvent::Mouse(mouse))) = &key {
                        if app.wm.focused_id().is_none() && !app.wm.modals.any_open() && app.selection_menu.is_none() && app.app.modal_notification.is_none()
                            && !app.app.composer_rect.is_some_and(|r| r.contains((mouse.column, mouse.row).into()))
                            && let Some(action) = app.app.workspace_mouse(*mouse) {
                            app.app.open_flow_hub = Some(action);
                            continue;
                        }
                        if app.wm.focused_id().is_none() && !app.wm.modals.any_open() && app.selection_menu.is_none() && app.app.modal_notification.is_none()
                            && let Some(flow) = handle.control_tx.as_ref().and_then(|tx| tx.binding()).and_then(|binding| binding.flow.instance())
                            && !app.app.composer_rect.is_some_and(|r| r.contains((mouse.column, mouse.row).into()))
                            && let Some(action) = app.app.flow_links.mouse(*mouse, flow) {
                            app.app.open_flow_hub = Some(action);
                            continue;
                        }
                        app.app.flow_hub_hovered = app.app.flow_hub_rect.is_some_and(|rect| rect.contains((mouse.column, mouse.row).into()));
                        clicked = app.app.flow_hub_hovered && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) && !app.wm.modals.any_open();
                    }
                    if escape || clicked {
                        app.app.open_flow_hub = Some(crate::flow_hub::Action::Return);
                        continue;
                    }
                }
                if let Some(Ok(CtEvent::Mouse(mouse))) = &key
                    && app.wm.focused_id().is_none() && !app.wm.modals.any_open() && app.selection_menu.is_none() && app.app.modal_notification.is_none()
                    && !app.app.composer_rect.is_some_and(|r| r.contains((mouse.column, mouse.row).into()))
                    && let Some(action) = app.app.focus_rail.mouse(*mouse) {
                    app.app.apply_focus_action(action);
                    continue;
                }
                if let Some(Ok(CtEvent::Mouse(mouse))) = &key
                    && app.wm.focused_id().is_none() && !app.wm.modals.any_open() && app.selection_menu.is_none() && app.app.modal_notification.is_none()
                    && !app.app.composer_rect.is_some_and(|r| r.contains((mouse.column, mouse.row).into()))
                    && app.app.activity_mouse(*mouse) {
                    continue;
                }
                if let Some(Ok(event)) = &key && app.layout_event(event) { continue; }
                let mut current = key;
                let mut scroll_delta: i32 = 0;
                let mut drained: u32 = 0;
                loop {
                    if app.wm.modals.any_open() || app.app.modal_notification.is_some() {
                        app.selection_menu = None;
                    }
                    match current {
                        Some(Ok(CtEvent::Mouse(me))) if app.selection_menu.is_some() => {
                            if handle_selection_menu_mouse(&mut app, &mut editor, &me) {
                                interrupt_prompt = None;
                                break;
                            }
                        }
                        Some(Ok(CtEvent::Mouse(me)))
                            if {
                                app.wm.sync_modals();
                                app.wm.top_kind() == Some(crate::wm::modal::ModalKind::HistorySearch)
                            } =>
                        {
                            if let Some(action) = app.wm.modals.history_search.mouse_action(&me) {
                                app.wm.dispatch_key(&action, &mut app.app, handle.control_tx.as_ref());
                            }
                            app.wm.modals.history_search.handle_mouse(&me);
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
                            if !consumed && handle_composer_mouse(&mut app, &mut editor, &me, handle.submit_tx.as_ref(), handle.control_tx.as_ref()) {
                                interrupt_prompt = None;
                                break;
                            }
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
                            if !consumed && handle_quote_mouse(&mut app.app, &me) {
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
                            if app.selection_menu.is_some()
                                && handle_selection_menu_key(&mut app, &mut editor, &action)
                            {
                                continue;
                            }
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
                                if matches!(action, crate::keys::KeyAction::Escape)
                                    && app.app.selection.is_some()
                                {
                                    app.app.selection = crate::selection::selection_clear();
                                    app.app.transcript_selection_pointer = None;
                                    app.app.thinking_click_origin = None;
                                    continue;
                                }
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
                                let saving = app.app.queue_saving();
                                if app.app.submission_focus && let Some(edit) = app.app.queued_submission_edit.as_mut() {
                                    if !saving {
                                        edit.editor.insert_str(&s);
                                    }
                                } else {
                                    editor.ingest_paste(&s);
                                    interrupt_prompt = None;
                                    app.app.refresh_popup(editor.buf());
                                }
                            }
                        }
                        Some(Ok(CtEvent::Mouse(me))) => {
                            if matches!(
                                me.kind,
                                MouseEventKind::Down(MouseButton::Left)
                                    | MouseEventKind::Drag(MouseButton::Left)
                            ) {
                                app.app.thinking_click_origin = None;
                            }
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
                            if !consumed && handle_composer_mouse(&mut app, &mut editor, &me, handle.submit_tx.as_ref(), handle.control_tx.as_ref()) {
                                interrupt_prompt = None;
                                break;
                            }
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
                            if !consumed && handle_quote_mouse(&mut app.app, &me) {
                                interrupt_prompt = None;
                                break;
                            }
                            if !consumed {
                                if handle_goal_card_mouse(&mut app, &me, handle.control_tx.as_ref()) {
                                    interrupt_prompt = None;
                                    break;
                                }
                                if app.wm.interaction.resize_target.is_none()
                                    && app.wm.interaction.drag_target.is_none()
                                    && (handle_sidebar_selection_mouse(&mut app.app, &me)
                                        || handle_window_selection_mouse(&mut app.app, &me)
                                        || handle_transcript_selection_mouse(&mut app.app, &me))
                                {
                                    if matches!(me.kind, MouseEventKind::Up(MouseButton::Left))
                                        && app
                                            .app
                                            .selection
                                            .as_ref()
                                            .is_some_and(crate::selection::selection_is_non_empty)
                                    {
                                        app.selection_menu = Some(
                                            crate::selection_menu::SelectionMenu::new(
                                                me.column, me.row,
                                            ),
                                        );
                                    }
                                    interrupt_prompt = None;
                                    break;
                                }
                            if let MouseEventKind::Down(MouseButton::Left) = me.kind {
                                let topmost = app.wm
                                    .hit_test_panel(me.column, me.row)
                                    .map(|p| (p.id, p.rect));
                                if let Some((panel_id, pr)) = topmost {
                                if let Some(min_id) =
                                    app.wm.hit_test_minimize(me.column, me.row)
                                {
                                    app.wm.request_close(min_id, &mut app.app);
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
                                        (Some(_), Some(task))
                                            if task.is_running()
                                                && task.kind == atman_runtime::TaskKind::Flow =>
                                        {
                                            app.app.request_task_kill(task.id);
                                            app.wm.clear_panel_close_arm();
                                            app.wm.request_close(close_id, &mut app.app);
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
                                        _ => app.wm.request_close(close_id, &mut app.app),
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
                                            *prev_id == id && now.duration_since(*ts) < crate::wm::shell::DOUBLE_CLICK_INTERVAL
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
                                        let key = match action {
                                            crate::wm::component::McpPanelAction::Add => KeyAction::Char('a'),
                                            crate::wm::component::McpPanelAction::Edit => KeyAction::Char('e'),
                                            crate::wm::component::McpPanelAction::Test => KeyAction::Char('t'),
                                            crate::wm::component::McpPanelAction::Toggle => KeyAction::Char('d'),
                                            crate::wm::component::McpPanelAction::Remove => KeyAction::Char('r'),
                                            crate::wm::component::McpPanelAction::CancelRemove => KeyAction::Escape,
                                            crate::wm::component::McpPanelAction::NextTab => KeyAction::Tab,
                                        };
                                        crate::key_handler::handle_mcp_panel_key(&key, &mut app, handle.control_tx.as_ref());
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
                                        app.app.mcp_remove_armed = None;
                                        crate::key_handler::handle_mcp_panel_key(&KeyAction::Submit, &mut app, handle.control_tx.as_ref());
                                    } else if let Some((panel_idx, path)) = wf_hit {
                                        if path.is_empty() {
                                            app.app.toggle_workflow_panel_expansion(panel_idx);
                                        } else {
                                            app.app.toggle_workflow_node(panel_idx, &path);
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
                                    app.app.toggle_focus_sidebar_panel(true);
                                } else if let Some(r) = app.app.last_lower_title_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.app.toggle_focus_sidebar_panel(false);
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
                                    if key == "goal-control" {
                                        if let Some(command) = app.app.goal_toggle_control() && let Some(tx) = handle.control_tx.as_ref() {
                                            let _ = tx.send(command);
                                        }
                                    } else if let Some(idx) = key
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
                                    app.app.set_focus_sidebar(crate::sidebar::SidebarMode::Closed);
                                } else if let Some(r) = app.app.last_expand_btn_rect
                                    && rect_contains(r, me.column, me.row)
                                {
                                    app.app.set_focus_sidebar(crate::sidebar::SidebarMode::Open);
                                } else if let Some(r) = app.app.last_task_panel_rect
                                    && me.column >= r.x
                                    && me.column < r.x + 10
                                    && me.row == r.y + 1
                                {
                                    app.app.toggle_focus_sidebar();
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
                                        app.app.request_task_kill(tid);
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
                                let skip_hover = app.app.startup_intro.is_some()
                                    || app.app.startup_card.is_some();
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

                                    let action_hover = app.wm.interaction.last_hitmap.mcp_action_rects.iter()
                                        .find(|(pid, _, rect)| *pid == panel_id && rect_contains(*rect, me.column, me.row))
                                        .map(|(_, action, _)| *action);
                                    app.wm.interaction.hovered_mcp_action = action_hover;

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
                                    app.wm.interaction.hovered_mcp_action = None;
                                    // Sidebar strip hover
                                let sidebar_hover = app
                                    .last_sidebar_strip_rects
                                    .iter()
                                    .find(|(_, r)| rect_contains(**r, me.column, me.row))
                                    .map(|(k, _)| k.clone())
                                    .or_else(|| app.app.last_goal_rect
                                        .filter(|r| rect_contains(*r, me.column, me.row))
                                        .map(|_| "goal-card".to_string()));
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
                            app.app.transcript_selection_pointer = None;
                            app.app.thinking_click_origin = None;
                            let canvas = ratatui::layout::Rect::new(0, 0, cols, rows);
                            let canvas = app.app.project_sidebar.lock().unwrap().layout(canvas);
                            views.constrain_panels(&mut app, canvas, handle.control_tx.as_ref());
                        }
                        Some(Err(error)) => return Err(error.into()),
                        _ => {}
                    }
                    drained = drained.saturating_add(1);
                    if drained >= 100 || hub.as_ref().is_some_and(|hub| hub.visible()) || app.app.open_flow_hub.is_some() {
                        break;
                    }
                    match pending_input_event.take().or_else(|| key_events.try_recv().ok()) {
                        Some(next) if app.app.focus_rail.page_open() || (persistent && !matches!(&next, Ok(CtEvent::Paste(_)) | Ok(CtEvent::Key(crossterm::event::KeyEvent { code: crossterm::event::KeyCode::Char(_), modifiers: KeyModifiers::NONE | KeyModifiers::SHIFT, .. })))) => {
                            // Navigation and pointer events must pass through workspace routing.
                            pending_input_event = Some(next);
                            break;
                        }
                        Some(next) => current = Some(next),
                        None => break,
                    }
                }
                if scroll_delta < 0 {
                    app.app.scroll_up((-scroll_delta) as u32);
                } else if scroll_delta > 0 {
                    app.app.scroll_down(scroll_delta as u32);
                }
            }
            frame = semantic_rx.recv(), if !app.app.should_quit => {
                if let Some(frame) = frame {
                    // Spawn per-FlowRun frame_tx forwarders when new FlowRuns appear.
                    let new_handle: Option<String> = match &frame {
                        StreamFrame::FlowStart { parent_run_id: None, .. } => {
                            handle.session.as_ref().and_then(|s| s.current_root())
                        }
                        StreamFrame::SubAgentStarted { handle, .. } => Some(handle.clone()),
                        _ => None,
                    };
                    if let Some(h) = new_handle
                        && let Some(sess) = &handle.session
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
                } else { app.app.should_quit = true; }
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
            _ = activity_changed(&mut handle.activity) => {
                refresh_activity(&mut app, &mut handle);
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
            _ = async {
                match goal_changes.as_mut() {
                    Some(changes) => { let _ = changes.changed().await; }
                    None => std::future::pending::<()>().await,
                }
            } => {
                app.app.refresh_goal_activity();
                app.app.mark_visual_dirty();
                app.wm_visual_version = app.wm_visual_version.wrapping_add(1);
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
                app.app.sync_composer_images(&mut editor);
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
                    app.app.sync_queue(rx.borrow().iter().cloned().map(crate::submission_queue::QueueItem::Session).collect());
                    app.app.mark_visual_dirty();
                }
            }
            inj = recv_injection(handle.injection_rx.as_mut()) => {
                if let Some(inj) = inj {
                    // Keep only pending injections, drop consumed/cancelled ones.
                    if matches!(
                        inj.state,
                        atman_runtime::injection::InjectionState::Pending
                            | atman_runtime::injection::InjectionState::Reserved
                    ) {
                        if !app.app.pending_injections.iter().any(|i| i.id == inj.id) {
                            app.app.pending_injections.push(inj);
                        } else if let Some(existing) = app
                            .app
                            .pending_injections
                            .iter_mut()
                            .find(|existing| existing.id == inj.id)
                        {
                            *existing = inj;
                        }
                    } else {
                        app.app.pending_injections.retain(|i| i.id != inj.id);
                        if inj.state == atman_runtime::injection::InjectionState::Cancelled
                            && inj.queued_submission_id.is_some()
                        {
                            app.app.push_toast(
                                "Not inserted; returned to Next",
                                crate::app::NoteLevel::Info,
                                std::time::Duration::from_secs(4),
                                crate::app::ToastPosition::TopRight,
                            );
                        }
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
                    app.wm.modals.form_modal.sync_runtime(rx.borrow().first());
                }
            }
            cmd = recv_navigation_command(&mut app.navigation_reply, &mut handle.cmd_rx, &mut handle.control_reply_rx, handle.control_tx.as_ref().and_then(|tx| tx.binding()), navigation_ready) => {
                if let Some((ticket, cmd)) = cmd {
                    let from_project = matches!(&cmd, TuiCommand::SessionCreated { .. })
                        && ticket.as_ref().is_some_and(|ticket| app.app.project_sidebar.lock().unwrap().creation_finished(ticket.request_id, None));
                    let cmd = match cmd {
                        TuiCommand::SessionCreated { flow, attachment } => {
                            app.app.project_sidebar.lock().unwrap().request(handle.control_tx.as_ref(), None);
                            TuiCommand::FocusFlow {
                            flow,
                            placement: views.active_placement(),
                            attachment: Some(attachment),
                        }},
                        command => command,
                    };
                    match cmd {
                        TuiCommand::NewSessionLocations(locations) => {
                            if !hub.as_ref().is_some_and(|hub| hub.owns_reply(ticket.as_ref())) { continue; }
                            hub = None;
                            if let Err(error) = app.wm.modals.form_modal.open_new_session(locations) {
                                app.app.push_note(error, crate::app::NoteLevel::Error);
                            }
                        }
                        TuiCommand::FlowCatalog { result } => {
                            match receive_catalog_reply(&mut hub, views.directory_history(), ticket.as_ref(), result, &mut app.app, handle.control_tx.as_ref()) {
                                CatalogReply::Focus(flow) => {
                                    let action = crate::flow_hub::Action::Focus(flow);
                                    let reuse = hub_view_key(&action).and_then(|key| views.binding(&key, &handle));
                                    if let Some(tx) = &handle.control_tx { dispatch_hub_action(hub.as_mut().unwrap(), action, tx, reuse); }
                                }
                                CatalogReply::SwitchSessions => app.wm.modals.session_switcher.open_for(&app.app, handle.control_tx.as_ref()),
                                CatalogReply::Failed(error) => { hub = None; navigation_failed(&mut app.app, error); },
                                CatalogReply::Applied => {},
                            }
                        }
                        TuiCommand::FlowResolved { flow } => {
                            let Some(hub) = hub.as_mut().filter(|hub| hub.owns_reply(ticket.as_ref())) else { continue; };
                            let key = ViewKey { flow: flow.clone(), placement: FlowPlacement::Main };
                            let reuse = views.binding(&key, &handle);
                            if let Some(tx) = &handle.control_tx {
                                dispatch_hub_action(hub, crate::flow_hub::Action::Focus(flow), tx, reuse);
                            }
                        }
                        TuiCommand::SubmissionContinued { flow, attachment } => {
                            if !app.app.pending_submission.as_ref().is_some_and(|pending| Some(&pending.ticket) == ticket.as_ref()) { continue; }
                            app.app.submission_result(&mut editor, ticket.as_ref(), Ok(()));
                            let placement = views.active_placement();
                            hub = None;
                            refresh_navigation_progress(&mut app.app, None, false);
                            match views.focus_with_navigation(&mut hub, &mut app, &mut editor, &mut handle, ViewKey { flow: flow.clone(), placement }, Some(attachment)).await {
                                Ok(()) => { interrupt_prompt = None; }
                                Err(error) => app.app.push_toast(format!("Input accepted in session {}; opening view failed: {error}", flow.session_id()), crate::app::NoteLevel::Error, Duration::from_secs(10), crate::app::ToastPosition::BottomRight),
                            }
                        }
                        TuiCommand::FocusFlow { flow, placement, attachment } => {
                            // Closing the directory cancels navigation, never the submitted host work.
                            let from_form = app.wm.modals.form_modal.owns_session_reply(ticket.as_ref());
                            if !from_project && !from_form && !hub.as_ref().is_some_and(|hub| hub.owns_reply(ticket.as_ref())) { continue; }
                            if hub.as_ref().is_some_and(|hub| hub.owns_reply(ticket.as_ref())) && !app.navigation_can_open(hub.as_ref()) {
                                defer_navigation_reply(&mut app, ticket, TuiCommand::FocusFlow { flow, placement, attachment });
                                continue;
                            }
                            if from_form { app.wm.modals.form_modal.close(); }
                            match views.complete_navigation(&mut hub, &mut app, &mut editor, &mut handle, ViewKey { flow, placement }, attachment).await {
                                Ok(()) => { interrupt_prompt = None; }
                                Err(error) => {
                                    if let Some(hub) = hub.as_mut() { hub.failed(error.to_string()); } else { app.app.push_note(error.to_string(), crate::app::NoteLevel::Error); }
                                }
                            }
                        }
                        command => {
                            if let TuiCommand::Toast { message, .. } = &command
                                && let Some(hub) = hub.as_mut()
                                && hub.owns_reply(ticket.as_ref())
                            { hub.failed(message.clone()); }
                            apply_view_command(&mut app, &mut editor, ticket.as_ref(), command, handle.control_tx.as_ref());
                        }
                    }
                }
            }
        }
    }
}

fn apply_view_command(
    app: &mut UiState,
    editor: &mut InputEditor,
    ticket: Option<&crate::control::ControlTicket>,
    command: TuiCommand,
    control_tx: Option<&crate::ControlSender>,
) {
    if matches!(&command, TuiCommand::SessionCreated { .. })
        && let Some(ticket) = ticket
    {
        let mut sidebar = app.app.project_sidebar.lock().unwrap();
        sidebar.creation_finished(ticket.request_id, None);
        sidebar.request(control_tx, None);
    }
    match command {
        TuiCommand::NewSessionFailed { request, error } => {
            if app
                .app
                .project_sidebar
                .lock()
                .unwrap()
                .creation_finished(request, Some(error.clone()))
            {
                return;
            }
            app.wm
                .modals
                .form_modal
                .session_failed(ticket, request, error);
        }
        TuiCommand::FocusFlow { flow, .. } | TuiCommand::SessionCreated { flow, .. }
            if app.wm.modals.form_modal.owns_session_reply(ticket) =>
        {
            app.wm.modals.form_modal.close();
            app.app.push_note(
                format!("Session {} created", flow.session_id()),
                crate::app::NoteLevel::Info,
            );
        }
        TuiCommand::NewSessionLocations(_) => {}
        TuiCommand::HistorySearchResults { result } => {
            app.wm.modals.history_search.receive(ticket, result)
        }
        TuiCommand::SessionChoices { result } => {
            app.wm.modals.session_switcher.receive(ticket, result)
        }
        TuiCommand::SessionChoiceRenamed { result } => {
            app.wm.modals.session_switcher.renamed(ticket, result)
        }
        // Navigation belongs to the workspace; a late response cannot move a background view.
        TuiCommand::FlowCatalog { .. }
        | TuiCommand::FocusFlow { .. }
        | TuiCommand::SessionCreated { .. }
        | TuiCommand::FlowResolved { .. } => {}
        TuiCommand::InboxEdited { request_id, result } => {
            app.app.queue_result(ticket, request_id, result)
        }
        TuiCommand::SubmissionResult { result } => {
            if !app.app.queue_recovery_result(ticket, &result) {
                app.app.submission_result(editor, ticket, result);
            }
        }
        TuiCommand::SubmissionContinued { flow, .. } => {
            if app
                .app
                .pending_submission
                .as_ref()
                .is_some_and(|pending| Some(&pending.ticket) == ticket)
            {
                app.app.submission_result(editor, ticket, Ok(()));
                app.app.push_toast(
                    format!("Input accepted in continued session {}", flow.session_id()),
                    crate::app::NoteLevel::Info,
                    Duration::from_secs(10),
                    crate::app::ToastPosition::BottomRight,
                );
            }
        }
        TuiCommand::AttachmentImported { result } => {
            app.app.attachment_imported(editor, ticket, result);
        }
        TuiCommand::FormSubmissionResult {
            request_id,
            form_id,
            result,
        } => {
            app.wm
                .modals
                .form_modal
                .submission_result(&form_id, request_id, result);
        }
        TuiCommand::SetSidebar(mode) => {
            app.app.set_focus_sidebar(mode);
        }
        TuiCommand::CopyText(text) => {
            let message = format!("copied {} chars", text.chars().count());
            key_handler::copy_text(&mut app.app, &text, message);
        }
        TuiCommand::OpenSessionSwitcher => {
            app.wm
                .modals
                .session_switcher
                .open_for(&app.app, control_tx);
        }
        TuiCommand::OpenProjectHub => app.app.startup_open_projects = true,
        TuiCommand::OpenProjectStorageScopePicker => {
            app.wm
                .modals
                .form_modal
                .open_preference(crate::settings::Preference::ProjectStorage, &mut app.app);
        }
        TuiCommand::SessionNameUpdated(name) => {
            if let Some(binding) = control_tx.and_then(|tx| tx.binding()) {
                app.wm
                    .modals
                    .session_switcher
                    .name_changed(&binding.flow.session_target(), Some(&name));
            }
            app.app.session_name = Some(name);
            if app.wm.modals.session_switcher.open && app.app.items.flow().is_none() {
                app.wm.modals.session_switcher.refresh_legacy(&app.app);
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
            app.wm
                .modals
                .resolve_model_switch(&mut app.app, request_id, model, result);
        }
        TuiCommand::ProviderCatalogChanged { added_provider } => {
            apply_provider_catalog_changed(app, added_provider);
        }
        TuiCommand::ProviderCatalogRefreshResult {
            provider_id,
            result,
        } => {
            apply_provider_catalog_refresh_result(app, &provider_id, result);
        }
        TuiCommand::ProviderMutationResult { request, result } => {
            apply_provider_mutation_result(app, request, result);
        }
        TuiCommand::ModelMutationResult { request, result } => {
            apply_model_mutation_result(app, request, result);
        }
        TuiCommand::SettingsMutationResult { request, result } => {
            if result.is_ok() {
                app.app.settings.invalidate();
            }
            app.wm
                .modals
                .form_modal
                .settings_result(request.id, &result);
            app.wm
                .modals
                .alias_manager
                .mutation_result(request.id, &result);
            app.wm
                .modals
                .mcp_editor
                .mutation_result(request.id, &result);
            let setup_complete = app
                .wm
                .modals
                .onboarding
                .mutation_result(request.id, &result);
            if setup_complete {
                app.wm.modals.dispatch_onboarding_event(
                    crate::onboarding::OnboardingEvent::Completed,
                    &mut app.app,
                    None,
                );
            }
            if app.app.mcp_pending_mutation == Some(request.id) {
                app.app.mcp_pending_mutation = None;
            }
            if result.is_ok() {
                apply_provider_catalog_changed(app, None);
            }
            if setup_complete {
                return;
            }
            let (message, level) = match result {
                Ok(message) => (message, app::NoteLevel::Success),
                Err(message) => (message, app::NoteLevel::Error),
            };
            app.app.push_toast(
                message,
                level,
                std::time::Duration::from_secs(5),
                app::ToastPosition::TopRight,
            );
        }
        TuiCommand::ProviderTestResult { request_id, result } => {
            if app
                .wm
                .modals
                .provider_manager
                .resolve_test(request_id, &result)
            {
                let (message, level) = match result {
                    Ok(message) => (message, app::NoteLevel::Success),
                    Err(message) => (message, app::NoteLevel::Error),
                };
                app.app.push_toast(
                    message,
                    level,
                    std::time::Duration::from_secs(5),
                    app::ToastPosition::TopRight,
                );
            }
        }
        TuiCommand::ModelTestResult { request_id, result } => {
            if app
                .wm
                .modals
                .model_manager
                .resolve_test(request_id, &result)
            {
                let (message, level) = match result {
                    Ok(message) => (message, app::NoteLevel::Success),
                    Err(message) => (message, app::NoteLevel::Error),
                };
                app.app.push_toast(
                    message,
                    level,
                    std::time::Duration::from_secs(8),
                    app::ToastPosition::TopRight,
                );
            }
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
        TuiCommand::KnowledgeResult { request_id, result } => {
            let mut state = app.app.knowledge_state.lock().unwrap();
            if !state.finish_read(request_id) {
                return;
            }
            match result {
                Ok((confessions, rules)) => {
                    state.read_error = None;
                    if !state.loading
                        && !state.applying
                        && !state.archiving
                        && (request_id.is_some()
                            || state.message.is_empty()
                            || state.message.contains(" confessions · ")
                            || state.message.starts_with("Refreshing memory")
                            || state.message.starts_with("Reloading rules"))
                    {
                        state.message =
                            format!("{} confessions · {} rules", confessions.len(), rules.len());
                    }
                    state.replace_catalog(confessions, rules);
                }
                Err(error) => {
                    state.read_error = Some(error.clone());
                    state.message = error;
                }
            }
        }
        TuiCommand::ConfessionHistoryResult {
            request_id,
            id,
            result,
        } => {
            let mut state = app.app.knowledge_state.lock().unwrap();
            if !state.finish_history(request_id, &id) {
                return;
            }
            match result {
                Ok(changes) => {
                    state.history_id = Some(id);
                    state.replace_history(changes);
                }
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
        TuiCommand::ConfessionArchived => {
            let mut state = app.app.knowledge_state.lock().unwrap();
            state.archiving = false;
            state.message = "Confession archived · active memory and search updated".into();
        }
        TuiCommand::ConfessionArchiveFailed(error) => {
            let mut state = app.app.knowledge_state.lock().unwrap();
            state.archiving = false;
            state.message = format!("Archive failed: {error}");
        }
        TuiCommand::OrganizationProgress {
            request_id,
            completed,
            total,
        } => {
            let mut state = app.app.knowledge_state.lock().unwrap();
            if request_id != state.organize_request {
                return;
            }
            state.message = format!("Analyzing confessions {completed}/{total}…");
        }
        TuiCommand::OrganizationResult { request_id, result } => {
            let mut state = app.app.knowledge_state.lock().unwrap();
            if request_id != state.organize_request {
                return;
            }
            state.loading = false;
            match result {
                Ok(proposals) => {
                    state.message = if proposals.is_empty() {
                        "No active confessions to organize".into()
                    } else {
                        format!(
                            "{} suggestions · select with Space, apply with A",
                            proposals.len()
                        )
                    };
                    state.replace_proposals(proposals);
                }
                Err(error) => state.message = format!("Organization failed: {error}"),
            }
        }
        TuiCommand::OrganizationApplied => {
            let mut state = app.app.knowledge_state.lock().unwrap();
            state.applying = false;
            state.replace_proposals(Vec::new());
            state.message = "Organization applied".into();
        }
        TuiCommand::OrganizationApplyFailed(error) => {
            let mut state = app.app.knowledge_state.lock().unwrap();
            state.applying = false;
            state.message = error;
        }
        TuiCommand::ProjectCatalogUpdated {
            request_id,
            success_message,
            result,
        } => {
            app.app
                .project_sidebar
                .lock()
                .unwrap()
                .result(request_id, &result);
            app.app
                .project_sidebar
                .lock()
                .unwrap()
                .refresh_if_needed(control_tx);
            if app.wm.project_result(request_id, &result) && !success_message.is_empty() {
                app.app.push_toast(
                    success_message,
                    app::NoteLevel::Success,
                    std::time::Duration::from_secs(3),
                    app::ToastPosition::TopRight,
                );
            }
        }
        TuiCommand::GoalSaved { request_id, result } => {
            app.wm.goal_saved(request_id, result);
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

fn apply_provider_catalog_changed(app: &mut UiState, added_provider: Option<String>) {
    app.app.settings.take_changes();
    app.wm.modals.bind_settings(app.app.settings.clone());
    if app.app.settings.is_waiting() || app.app.settings.failure().is_some() {
        if added_provider.is_some() {
            app.app.prepared_provider_added = added_provider;
        }
        app.app.mark_visual_dirty();
        return;
    }
    let added_provider = added_provider.or_else(|| app.app.prepared_provider_added.take());
    app.wm.modals.settings_prepared();
    app.wm.modals.provider_manager.refresh_list();
    app.wm.modals.model_manager.refresh();
    app.wm.modals.alias_manager.catalog_changed();
    app.wm.modals.model_picker.catalog_changed();
    if !app.wm.modals.model_picker.is_pending() && app.app.reconcile_input_reasoning() {
        app.app.save_ui_state();
    }
    if let Some(name) = added_provider {
        let needs_model = app
            .app
            .settings
            .models()
            .all_provider_groups_with_empty()
            .into_iter()
            .find(|group| group.provider_name == name)
            .is_none_or(|group| group.models.is_empty());
        if needs_model && (app.wm.modals.provider_manager.open || app.wm.modals.onboarding_open) {
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
    if result.is_ok() {
        app.app.settings.invalidate();
    }
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
    if result.is_ok() {
        app.app.settings.invalidate();
    }
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

async fn recv_optional<T>(rx: Option<&mut mpsc::UnboundedReceiver<T>>) -> Option<T> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

async fn recv_navigation_command(
    navigation: &mut Option<(Option<crate::control::ControlTicket>, TuiCommand)>,
    rx: &mut Option<mpsc::UnboundedReceiver<TuiCommand>>,
    bound: &mut Option<mpsc::UnboundedReceiver<crate::control::ControlReply>>,
    binding: Option<&crate::control::ViewBinding>,
    ready: bool,
) -> Option<(Option<crate::control::ControlTicket>, TuiCommand)> {
    if ready
        && let Some((ticket, command)) = navigation.take()
        && ticket.as_ref().map(|ticket| &ticket.binding) == binding
    {
        return Some((ticket, command));
    }
    recv_cmd(rx, bound, binding).await
}

pub(crate) async fn recv_cmd(
    rx: &mut Option<mpsc::UnboundedReceiver<TuiCommand>>,
    bound: &mut Option<mpsc::UnboundedReceiver<crate::control::ControlReply>>,
    binding: Option<&crate::control::ViewBinding>,
) -> Option<(Option<crate::control::ControlTicket>, TuiCommand)> {
    loop {
        tokio::select! {
            command = recv_optional(rx.as_mut()) => match command {
                Some(command) => return Some((None, command)),
                None => *rx = None,
            },
            reply = recv_optional(bound.as_mut()) => match reply {
                Some(reply) if Some(&reply.ticket.binding) == binding => return Some((Some(reply.ticket), reply.command)),
                Some(_) => {},
                None => *bound = None,
            },
        }
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
    control_tx: Option<&crate::ControlSender>,
) -> bool {
    if app.startup_intro.is_some() || app.startup_card.is_none() {
        app.startup_focus = app::StartupFocus::Input;
        app.startup_hovered_session = None;
        app.startup_projects_hovered = false;
        app.startup_last_click = None;
        return false;
    }
    let projects_hit = app
        .startup_projects_rect
        .is_some_and(|rect| rect_contains(rect, event.column, event.row));
    app.startup_projects_hovered = projects_hit;
    if projects_hit && event.kind == MouseEventKind::Down(MouseButton::Left) {
        match app.startup_card.as_ref().unwrap().directory {
            app::StartupDirectory::Projects => app.startup_open_projects = true,
            app::StartupDirectory::Sessions => {
                app.open_flow_hub = Some(crate::flow_hub::Action::Load(
                    crate::flow_hub::HubRequest::Sessions { before: None },
                ));
            }
        }
        return true;
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
        return in_container || projects_hit;
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
        if double_click {
            key_handler::request_startup_session(app, control_tx, index);
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

fn handle_goal_card_mouse(
    ui: &mut crate::ui_state::UiState,
    event: &crossterm::event::MouseEvent,
    tx: Option<&crate::ControlSender>,
) -> bool {
    let point = (event.column, event.row);
    let blocked = ui.wm.any_modal_open()
        || ui.app.modal_notification.is_some()
        || ui.wm.hit_test_panel(point.0, point.1).is_some()
        || ui.wm.interaction.drag_target.is_some()
        || ui.wm.interaction.resize_target.is_some();
    let on_card = !blocked
        && ui
            .app
            .last_goal_rect
            .is_some_and(|r| r.contains(point.into()))
        && !ui
            .app
            .last_goal_hdr_rect
            .is_some_and(|r| r.contains(point.into()))
        && !ui
            .app
            .last_sidebar_strip_rects
            .get("goal-control")
            .is_some_and(|r| r.contains(point.into()));
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            ui.app.goal_click_origin = on_card.then_some(point);
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            if ui.app.goal_click_origin != Some(point) {
                ui.app.goal_click_origin = None;
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if ui.app.goal_click_origin.take() == Some(point) && on_card {
                ui.app.selection = None;
                return ui.wm.dispatch_meta_command(":goal", &mut ui.app, tx);
            }
        }
        _ => {}
    }
    false
}

fn handle_sidebar_selection_mouse(
    app: &mut AppState,
    event: &crossterm::event::MouseEvent,
) -> bool {
    let projection = &app.last_sidebar_selection_projection;
    let point = projection
        .surface_at(event.row, event.column)
        .and_then(|surface| {
            surface.point_at(event.row, event.column).map(|grapheme| {
                let section = match surface.section {
                    crate::sidebar_selection::SidebarSection::Goal => {
                        crate::selection::SidebarSection::Goal
                    }
                    crate::sidebar_selection::SidebarSection::Plan => {
                        crate::selection::SidebarSection::Plan
                    }
                    crate::sidebar_selection::SidebarSection::Todo => {
                        crate::selection::SidebarSection::Todo
                    }
                    crate::sidebar_selection::SidebarSection::Context => {
                        crate::selection::SidebarSection::Context
                    }
                    crate::sidebar_selection::SidebarSection::Mcp => {
                        crate::selection::SidebarSection::Mcp
                    }
                };
                crate::selection::SemanticPoint {
                    domain: crate::selection::SelectionDomain::Sidebar { section },
                    ordinal: 0,
                    grapheme,
                    affinity: crate::selection::Affinity::Before,
                }
            })
        });
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(point) = point else {
                return false;
            };
            app.selection = Some(crate::selection::selection_begin(
                point,
                crate::app::OutputRevision::default(),
                app.items.structure_revision(),
            ));
            false
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some(state) = app.selection.as_ref() else {
                return false;
            };
            if !matches!(
                state.anchor.domain,
                crate::selection::SelectionDomain::Sidebar { .. }
            ) {
                return false;
            }
            let Some(point) = point else {
                return true;
            };
            let next = crate::selection::selection_extend(
                state,
                point,
                state.owner_revision,
                state.structure_revision,
            );
            if let Some(next) = next {
                app.selection = Some(next);
                app.selection
                    .as_ref()
                    .is_some_and(crate::selection::selection_is_non_empty)
            } else {
                true
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            let Some(state) = app.selection.as_ref() else {
                return false;
            };
            if !matches!(
                state.anchor.domain,
                crate::selection::SelectionDomain::Sidebar { .. }
            ) {
                return false;
            }
            let state = app.selection.take().expect("selection checked above");
            if !crate::selection::selection_is_non_empty(&state) {
                app.selection = Some(state);
                return false;
            }
            let Some(surface) = projection.surface_at(event.row, event.column) else {
                app.selection = Some(state);
                return false;
            };
            let expected_section = match &state.anchor.domain {
                crate::selection::SelectionDomain::Sidebar { section } => section,
                _ => unreachable!("sidebar domain checked above"),
            };
            let actual_section = match surface.section {
                crate::sidebar_selection::SidebarSection::Goal => {
                    crate::selection::SidebarSection::Goal
                }
                crate::sidebar_selection::SidebarSection::Plan => {
                    crate::selection::SidebarSection::Plan
                }
                crate::sidebar_selection::SidebarSection::Todo => {
                    crate::selection::SidebarSection::Todo
                }
                crate::sidebar_selection::SidebarSection::Context => {
                    crate::selection::SidebarSection::Context
                }
                crate::sidebar_selection::SidebarSection::Mcp => {
                    crate::selection::SidebarSection::Mcp
                }
            };
            if &actual_section != expected_section {
                app.selection = Some(state);
                return true;
            }
            let Some(end) = surface.point_at(event.row, event.column) else {
                app.selection = Some(state);
                return false;
            };
            let range =
                state.anchor.grapheme.min(end)..state.anchor.grapheme.max(end).saturating_add(1);
            if let Some(text) = surface.serialize(range) {
                app.copy_to_clipboard(
                    crate::clipboard::writes::Payload::Text(text),
                    None,
                    Some(state.clone()),
                );
                app.selection = Some(state);
                true
            } else {
                app.selection = Some(state);
                false
            }
        }
        _ => false,
    }
}

fn handle_selection_menu_mouse(
    ui: &mut UiState,
    editor: &mut InputEditor,
    event: &crossterm::event::MouseEvent,
) -> bool {
    let Some(menu) = ui.selection_menu.as_mut() else {
        return false;
    };
    match event.kind {
        MouseEventKind::Moved => {
            menu.update_hover(event.column, event.row);
            true
        }
        MouseEventKind::Down(MouseButton::Left) => {
            let action = menu
                .action_at(event.column, event.row)
                .unwrap_or(crate::selection_menu::SelectionAction::Copy);
            apply_selection_action(ui, editor, action);
            true
        }
        MouseEventKind::Down(MouseButton::Right) => {
            apply_selection_action(ui, editor, crate::selection_menu::SelectionAction::Cancel);
            true
        }
        _ => true,
    }
}

fn handle_composer_mouse(
    app: &mut UiState,
    editor: &mut InputEditor,
    me: &crossterm::event::MouseEvent,
    submit_tx: Option<&tokio::sync::mpsc::UnboundedSender<crate::TuiSubmission>>,
    control_tx: Option<&crate::ControlSender>,
) -> bool {
    use crate::input::InputAction;
    if app.wm.captures_pointer(me) {
        return false;
    }
    let hovered_action = app
        .app
        .submission_queue_hitmap
        .action_at(me.column, me.row)
        .map(|(_, action)| action);
    let hovered_row = app.app.submission_queue_hitmap.row_at(me.column, me.row);
    if app.app.hovered_submission_action != hovered_action
        || app.app.hovered_submission != hovered_row
    {
        app.app.hovered_submission_action = hovered_action;
        app.app.hovered_submission = hovered_row;
        app.app.mark_visual_dirty();
    }
    let action = app
        .app
        .input_controls
        .iter()
        .find(|(_, rect)| rect.contains((me.column, me.row).into()))
        .map(|(action, _)| *action);
    if app.app.input_hover != action {
        app.app.input_hover = action;
        app.app.mark_visual_dirty();
    }
    if !app
        .app
        .composer_rect
        .is_some_and(|r| r.contains((me.column, me.row).into()))
    {
        return false;
    }
    if app.app.transcript_selection_pointer.is_some()
        && matches!(me.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_))
    {
        return false;
    }
    if me.kind == MouseEventKind::Down(MouseButton::Left) {
        app.wm.blur();
    }
    if handle_quote_mouse(&mut app.app, me) {
        return true;
    }
    if let MouseEventKind::Down(MouseButton::Left) = me.kind {
        if app
            .app
            .input_rect
            .is_some_and(|rect| rect.contains((me.column, me.row).into()))
        {
            app.app.submission_focus = false;
            app.app.startup_focus = crate::app::StartupFocus::Input;
        }
        if let Some(action) = action {
            let key = match action {
                InputAction::Model if !app.app.items.is_archive() => {
                    app.wm.modals.model_picker.open();
                    None
                }
                InputAction::Trust if !app.app.items.is_archive() => {
                    app.wm.modals.open_trust_mode_picker(&mut app.app);
                    None
                }
                InputAction::Reasoning if !app.app.items.is_archive() => {
                    Some(KeyAction::CycleReasoning)
                }
                InputAction::Escalation => Some(KeyAction::Tab),
                InputAction::Attach => Some(KeyAction::PasteImage),
                InputAction::Newline => Some(KeyAction::Newline),
                InputAction::Latest => Some(KeyAction::End),
                InputAction::Send
                    if app.app.pending_submission.is_none()
                        && !editor.has_preparing_images()
                        && (!editor.buf().trim().is_empty()
                            || !editor.pending_images().is_empty()) =>
                {
                    Some(KeyAction::Submit)
                }
                _ => None,
            };
            if let Some(key) = key {
                key_handler::handle_key(key, app, editor, &mut None, submit_tx, control_tx);
            }
            return true;
        }
        if let Some(click) = app.app.approval_hitmap.at(me.column, me.row) {
            key_handler::dispatch_approval_click(click, &mut app.app, control_tx);
            return true;
        }
        if let Some((index, action)) = app.app.submission_queue_hitmap.action_at(me.column, me.row)
        {
            key_handler::dispatch_submission_queue_action(&mut app.app, index, action, control_tx);
            return true;
        }
        if let Some((rect, scroll)) = app.app.submission_queue_hitmap.edit_area
            && rect_contains(rect, me.column, me.row)
            && let Some(edit) = &mut app.app.queued_submission_edit
        {
            let cursor = crate::input::cursor_from_wrapped(
                edit.editor.buf(),
                scroll + me.row.saturating_sub(rect.y) as usize,
                me.column.saturating_sub(rect.x) as usize,
                rect.width as usize,
            );
            edit.editor.set_cursor(cursor);
            app.app.submission_focus = true;
            return true;
        }
        if let Some(index) = app.app.submission_queue_hitmap.row_at(me.column, me.row) {
            app.app.select_submission(index);
            app.app.submission_focus = true;
            return true;
        }
    }

    if let Some(rect) = app.app.input_rect.map(layout::input_editor_rect)
        && rect_contains(rect, me.column, me.row)
    {
        if matches!(me.kind, MouseEventKind::Down(MouseButton::Left)) {
            let inner_x = me.column.saturating_sub(rect.x);
            let inner_y = me.row.saturating_sub(rect.y);
            let cw = rect.width as usize;
            let pos = cursor_from_wrapped(
                editor.buf(),
                inner_y as usize + app.app.input_scroll_row as usize,
                inner_x as usize,
                cw,
            );
            editor.set_cursor(pos);
            app.app.submission_focus = false;
        } else if matches!(
            me.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            let width = rect.width as usize;
            for _ in 0..3 {
                let moved = if me.kind == MouseEventKind::ScrollUp {
                    editor.move_line_up_visual(width)
                } else {
                    editor.move_line_down_visual(width)
                };
                if !moved {
                    break;
                }
            }
        }
    }
    true
}

fn handle_quote_mouse(app: &mut AppState, event: &crossterm::event::MouseEvent) -> bool {
    let Some(surface) = app
        .pending_quote
        .as_ref()
        .and_then(|_| app.quote_rect.or(app.input_rect))
    else {
        app.quote_close_hovered = false;
        return false;
    };
    let close = crate::selection_menu::quote_close_rect(surface)
        .is_some_and(|rect| rect_contains(rect, event.column, event.row));
    app.quote_close_hovered = close;
    let over_card = app
        .quote_rect
        .is_some_and(|rect| rect_contains(rect, event.column, event.row));
    if over_card && app.quote_expanded {
        let max_scroll = crate::render::quote_card_max_scroll(
            app.pending_quote.as_deref().unwrap_or_default(),
            app.quote_rect.expect("quote card is visible"),
        );
        match event.kind {
            MouseEventKind::ScrollDown => {
                app.quote_scroll = app.quote_scroll.saturating_add(2).min(max_scroll);
                return true;
            }
            MouseEventKind::ScrollUp => {
                app.quote_scroll = app.quote_scroll.saturating_sub(2);
                return true;
            }
            _ => {}
        }
    }
    if !matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) {
        return false;
    }
    if close {
        app.pending_quote = None;
        app.quote_close_hovered = false;
        app.quote_expanded = false;
        app.quote_scroll = 0;
        return true;
    }
    if over_card {
        app.quote_expanded = !app.quote_expanded;
        app.quote_scroll = 0;
    }
    over_card
}

fn handle_selection_menu_key(
    ui: &mut UiState,
    editor: &mut InputEditor,
    action: &crate::keys::KeyAction,
) -> bool {
    let execute = {
        let Some(menu) = ui.selection_menu.as_mut() else {
            return false;
        };
        match action {
            crate::keys::KeyAction::CursorLeft
            | crate::keys::KeyAction::HistoryUp
            | crate::keys::KeyAction::BackTab => {
                menu.move_previous();
                None
            }
            crate::keys::KeyAction::CursorRight
            | crate::keys::KeyAction::HistoryDown
            | crate::keys::KeyAction::Tab => {
                menu.move_next();
                None
            }
            crate::keys::KeyAction::Submit => Some(menu.focused_action()),
            crate::keys::KeyAction::Escape => Some(crate::selection_menu::SelectionAction::Cancel),
            _ => return false,
        }
    };
    if let Some(action) = execute {
        apply_selection_action(ui, editor, action);
    }
    true
}

fn selection_action_payload(ui: &UiState) -> Option<crate::selection::CopyPayload> {
    ui.app.selection.as_ref().and_then(|state| {
        let projection = if matches!(
            state.anchor.domain,
            crate::selection::SelectionDomain::Window { .. }
        ) {
            &ui.app.last_window_selection_projection
        } else {
            &ui.app.last_selection_projection
        };
        crate::selection::selection_copy_payload(projection, state)
    })
}

fn apply_selection_action(
    ui: &mut UiState,
    _editor: &mut InputEditor,
    action: crate::selection_menu::SelectionAction,
) {
    let payload = selection_action_payload(ui);
    match action {
        crate::selection_menu::SelectionAction::Copy => {
            if let Some(payload) = payload {
                let text = match payload {
                    crate::selection::CopyPayload::Markdown(text)
                    | crate::selection::CopyPayload::PlainText(text)
                    | crate::selection::CopyPayload::Preview(text) => text,
                };
                key_handler::copy_text(&mut ui.app, &text, "copied selection".into());
            }
        }
        crate::selection_menu::SelectionAction::RichCopy => {
            if let Some(payload) = payload {
                ui.app.copy_to_clipboard(
                    crate::clipboard::writes::Payload::Rich(payload),
                    Some("copied rich selection".into()),
                    None,
                );
            }
        }
        crate::selection_menu::SelectionAction::Quote => {
            if let Some(payload) = payload {
                let text = match payload {
                    crate::selection::CopyPayload::Markdown(text)
                    | crate::selection::CopyPayload::PlainText(text)
                    | crate::selection::CopyPayload::Preview(text) => text,
                };
                ui.app.pending_quote = Some(text.into());
                ui.app.quote_close_hovered = false;
                ui.app.quote_expanded = false;
                ui.app.quote_scroll = 0;
                ui.app.submission_focus = false;
            }
        }
        crate::selection_menu::SelectionAction::Cancel => {}
    }
    ui.app.selection = crate::selection::selection_clear();
    ui.selection_menu = None;
}

fn handle_window_selection_mouse(app: &mut AppState, event: &crossterm::event::MouseEvent) -> bool {
    let projection = &app.last_window_selection_projection;
    let row = u32::from(event.row);
    let col = event.column;
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(point) = projection.isolated_point_at(row, col) else {
                return false;
            };
            let Some(surface) = projection.surface_at_row(row) else {
                return false;
            };
            app.selection = Some(crate::selection::selection_begin(
                point,
                surface.revision,
                projection.structure_revision,
            ));
            true
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some(state) = app.selection.as_ref() else {
                return false;
            };
            if !matches!(
                state.anchor.domain,
                crate::selection::SelectionDomain::Window { .. }
            ) {
                return false;
            }
            let Some(point) = projection.isolated_point_at(row, col) else {
                return true;
            };
            app.selection = crate::selection::selection_extend(
                state,
                point,
                state.owner_revision,
                projection.structure_revision,
            );
            app.selection
                .as_ref()
                .is_some_and(crate::selection::selection_is_non_empty)
        }
        MouseEventKind::Up(MouseButton::Left) => app.selection.as_ref().is_some_and(|state| {
            matches!(
                state.anchor.domain,
                crate::selection::SelectionDomain::Window { .. }
            ) && crate::selection::selection_is_non_empty(state)
        }),
        _ => false,
    }
}

const SELECTION_SCROLL_MAX_STEP: u32 = 4;

fn selection_scroll_step(distance: u16) -> u32 {
    if distance == 0 {
        0
    } else {
        (1 + u32::from(distance - 1) / 3).min(SELECTION_SCROLL_MAX_STEP)
    }
}

fn transcript_selection_viewport(app: &AppState) -> Option<ratatui::layout::Rect> {
    let mut rect = app.last_transcript_rect?;
    rect.height = rect
        .height
        .saturating_sub(app.last_input_overlay_rows.min(u32::from(u16::MAX)) as u16)
        .saturating_sub(layout::INPUT_TOP_GAP);
    (rect.height > 0).then_some(rect)
}

fn transcript_selection_needs_tick(app: &AppState) -> bool {
    let (Some((_, row)), Some(rect)) = (
        app.transcript_selection_pointer,
        transcript_selection_viewport(app),
    ) else {
        return false;
    };
    app.selection.is_some()
        && ((row < rect.y && app.scroll_offset > 0)
            || (row >= rect.bottom() && app.scroll_offset < app.max_scroll_offset()))
}

fn resolve_transcript_selection_edge(app: &mut AppState, focus_last: bool) {
    let Some(state) = app.selection.as_ref() else {
        return;
    };
    let Some(viewport) = transcript_selection_viewport(app) else {
        return;
    };
    let visible_end = app.scroll_offset.saturating_add(u32::from(viewport.height));
    let points = app
        .last_selection_projection
        .visual_points()
        .into_iter()
        .filter(|visual| {
            crate::selection::transcript_domains_compatible(
                &state.anchor.domain,
                &visual.point.domain,
            ) && visual.row >= app.scroll_offset
                && visual.row < visible_end
        })
        .map(|visual| visual.point);
    let point = if focus_last {
        points.max_by(|left, right| left.position_cmp(right))
    } else {
        points.min_by(|left, right| left.position_cmp(right))
    };
    if let Some(point) = point {
        app.selection = extend_mouse_selection(
            state,
            point,
            state.owner_revision,
            app.last_selection_projection.structure_revision,
        );
        if app.selection.is_none() {
            app.transcript_selection_pointer = None;
        }
    }
}

pub(crate) fn resolve_transcript_selection_after_render(app: &mut AppState) {
    let (Some((_, pointer_row)), Some(rect)) = (
        app.transcript_selection_pointer,
        transcript_selection_viewport(app),
    ) else {
        return;
    };
    if pointer_row < rect.y {
        resolve_transcript_selection_edge(app, false);
    } else if pointer_row >= rect.bottom() {
        resolve_transcript_selection_edge(app, true);
    }
}

fn tick_transcript_selection_drag(app: &mut AppState) {
    if app.selection.is_none() {
        return;
    }
    let Some((_, pointer_row)) = app.transcript_selection_pointer else {
        return;
    };
    let Some(rect) = transcript_selection_viewport(app) else {
        return;
    };
    let top = rect.y;
    let bottom = rect.bottom();
    let delta = if pointer_row < top {
        -(selection_scroll_step(top.saturating_sub(pointer_row)) as i32)
    } else if pointer_row >= bottom {
        selection_scroll_step(pointer_row.saturating_sub(bottom).saturating_add(1)) as i32
    } else {
        return;
    };

    let max = app.max_scroll_offset();
    let next = if delta.is_negative() {
        app.scroll_offset
            .saturating_sub(delta.unsigned_abs())
            .min(max)
    } else {
        app.scroll_offset.saturating_add(delta as u32).min(max)
    };
    if next == app.scroll_offset {
        return;
    }
    app.clear_work_fold_scroll_anchor();
    app.scroll_offset = next;
    app.pause_tail_following();
}

fn extend_mouse_selection(
    state: &crate::selection::SelectionState,
    point: crate::selection::SemanticPoint,
    owner_revision: crate::app::OutputRevision,
    structure_revision: u64,
) -> Option<crate::selection::SelectionState> {
    let mut state = state.clone();
    let mut point = point;
    if point.position_cmp(&state.anchor).is_lt() {
        if state.anchor.affinity == crate::selection::Affinity::Before {
            state.anchor.grapheme = state.anchor.grapheme.saturating_add(1);
            state.anchor.affinity = crate::selection::Affinity::After;
        }
    } else if point.position_cmp(&state.anchor).is_gt() {
        point.grapheme = point.grapheme.saturating_add(1);
        point.affinity = crate::selection::Affinity::After;
    }
    crate::selection::selection_extend(&state, point, owner_revision, structure_revision)
}

fn handle_transcript_selection_mouse(
    app: &mut AppState,
    event: &crossterm::event::MouseEvent,
) -> bool {
    let Some(rect) = transcript_selection_viewport(app) else {
        if matches!(event.kind, MouseEventKind::Up(MouseButton::Left)) {
            app.transcript_selection_pointer = None;
            app.thinking_click_origin = None;
        }
        return matches!(
            event.kind,
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
        ) && app.selection.is_some();
    };
    let row = u32::from(event.row.saturating_sub(rect.y)).saturating_add(app.scroll_offset);
    let col = event.column.saturating_sub(rect.x);
    let visible_rows = app.scroll_offset..app.scroll_offset.saturating_add(u32::from(rect.height));
    match event.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if event.row < rect.y || event.row >= rect.bottom() {
                return false;
            }
            app.thinking_click_origin = app.hit_test(event.column, event.row).and_then(|index| {
                matches!(
                    app.items.get(index),
                    Some(crate::app::OutputItem::Thinking { .. })
                )
                .then_some((index, event.column, event.row))
            });
            let point = app
                .last_selection_projection
                .prose_point_at(row, col)
                .or_else(|| app.last_selection_projection.code_point_at(row, col))
                .or_else(|| app.last_selection_projection.isolated_point_at(row, col))
                .or_else(|| {
                    app.last_selection_projection.nearest_point_at(
                        row,
                        col,
                        None,
                        visible_rows.clone(),
                    )
                });
            let Some(point) = point else {
                return false;
            };
            let Some(surface) = app.last_selection_projection.surface_at_row(row) else {
                return false;
            };
            app.selection = Some(crate::selection::selection_begin(
                point,
                surface.revision,
                app.last_selection_projection.structure_revision,
            ));
            app.transcript_selection_pointer = Some((event.column, event.row));
            app.pause_tail_following();
            false
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            app.thinking_click_origin = None;
            let Some(state) = app.selection.as_ref() else {
                return false;
            };
            app.transcript_selection_pointer = Some((event.column, event.row));
            if event.row < rect.y || event.row >= rect.bottom() {
                return true;
            }
            let point = match &state.anchor.domain {
                crate::selection::SelectionDomain::TranscriptProse
                | crate::selection::SelectionDomain::MarkdownCode { .. } => app
                    .last_selection_projection
                    .prose_point_at(row, col)
                    .or_else(|| app.last_selection_projection.code_point_at(row, col)),
                _ => app.last_selection_projection.isolated_point_at(row, col),
            }
            .filter(|point| {
                crate::selection::transcript_domains_compatible(&state.anchor.domain, &point.domain)
            })
            .or_else(|| {
                app.last_selection_projection.nearest_point_at(
                    row,
                    col,
                    Some(&state.anchor.domain),
                    visible_rows.clone(),
                )
            });
            let Some(point) = point else {
                return true;
            };
            app.selection = extend_mouse_selection(
                state,
                point,
                state.owner_revision,
                app.last_selection_projection.structure_revision,
            );
            if app.selection.is_none() {
                app.transcript_selection_pointer = None;
            }
            app.selection
                .as_ref()
                .is_some_and(crate::selection::selection_is_non_empty)
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some((index, column, row)) = app.thinking_click_origin.take()
                && (event.column, event.row) == (column, row)
                && app.hit_test(event.column, event.row) == Some(index)
            {
                app.selection = crate::selection::selection_clear();
                app.transcript_selection_pointer = None;
                app.cycle_thinking_disclosure(index);
                return true;
            }
            if event.row < rect.y {
                resolve_transcript_selection_edge(app, false);
            } else if event.row >= rect.bottom() {
                resolve_transcript_selection_edge(app, true);
            } else if let Some(state) = app.selection.as_ref() {
                let point = match &state.anchor.domain {
                    crate::selection::SelectionDomain::TranscriptProse
                    | crate::selection::SelectionDomain::MarkdownCode { .. } => app
                        .last_selection_projection
                        .prose_point_at(row, col)
                        .or_else(|| app.last_selection_projection.code_point_at(row, col)),
                    _ => app.last_selection_projection.isolated_point_at(row, col),
                }
                .filter(|point| {
                    crate::selection::transcript_domains_compatible(
                        &state.anchor.domain,
                        &point.domain,
                    )
                })
                .or_else(|| {
                    app.last_selection_projection.nearest_point_at(
                        row,
                        col,
                        Some(&state.anchor.domain),
                        visible_rows.clone(),
                    )
                });
                if let Some(point) = point {
                    app.selection = extend_mouse_selection(
                        state,
                        point,
                        state.owner_revision,
                        app.last_selection_projection.structure_revision,
                    );
                }
            }
            app.transcript_selection_pointer = None;
            let Some(state) = app.selection.take() else {
                return false;
            };
            let consumed = crate::selection::selection_is_non_empty(&state);
            app.selection = consumed.then_some(state);
            consumed
        }
        _ => false,
    }
}

fn hub_view_key(action: &crate::flow_hub::Action) -> Option<ViewKey> {
    let (flow, placement) = match action {
        crate::flow_hub::Action::Focus(flow) => (flow, FlowPlacement::Main),
        crate::flow_hub::Action::Float(flow) => (flow, FlowPlacement::Floating),
        _ => return None,
    };
    Some(ViewKey {
        flow: flow.clone(),
        placement,
    })
}

fn dispatch_hub_action(
    hub: &mut crate::flow_hub::FlowHub,
    action: crate::flow_hub::Action,
    tx: &crate::ControlSender,
    reuse: Option<crate::control::ViewBinding>,
) {
    let command = match crate::panel_registry::navigation(action) {
        crate::flow_hub::Action::ChangeSessionProject(change) => {
            TuiControl::ChangeSessionProject(change)
        }
        crate::flow_hub::Action::SelectDefault(change) => TuiControl::SelectDefaultFlow(change),
        crate::flow_hub::Action::ArchiveFlow(change) => TuiControl::ArchiveFlow(change),
        crate::flow_hub::Action::SubmitControls(request) => TuiControl::SubmitControls(request),
        crate::flow_hub::Action::RetryControls(id) => TuiControl::RetryControls(id),
        crate::flow_hub::Action::UpdateManagement(change) => TuiControl::UpdateManagement(change),
        crate::flow_hub::Action::SelectManagement(change) => TuiControl::SelectManagement(change),
        crate::flow_hub::Action::ReleaseWorkspace(change) => TuiControl::ReleaseWorkspace(change),
        crate::flow_hub::Action::RemoveSession(change) => TuiControl::RemoveSession(change),
        crate::flow_hub::Action::SetWorkspaceRetention(change) => {
            TuiControl::SetWorkspaceRetention(change)
        }
        crate::flow_hub::Action::NewSession => TuiControl::NewSession,
        crate::flow_hub::Action::OpenManagement => TuiControl::OpenManagement,
        crate::flow_hub::Action::OpenLegacySession(session) => {
            TuiControl::OpenLegacySession(session)
        }
        crate::flow_hub::Action::OpenSession(session) => TuiControl::OpenSession(session),
        crate::flow_hub::Action::RecoverIndexedHistory(review) => {
            TuiControl::RecoverIndexedHistory(review)
        }
        crate::flow_hub::Action::Load(request) => TuiControl::BrowseFlows(request),
        crate::flow_hub::Action::CancelWatcher { owner, id } => {
            TuiControl::CancelWatcher { owner, id }
        }
        crate::flow_hub::Action::ReplaceWatcher {
            owner,
            expected,
            replacement,
        } => TuiControl::ReplaceWatcher {
            owner,
            expected,
            replacement,
        },
        crate::flow_hub::Action::RegisterWatcher {
            owner,
            registration,
        } => TuiControl::RegisterWatcher {
            owner,
            registration,
        },
        crate::flow_hub::Action::Focus(flow) => TuiControl::FocusFlow {
            flow,
            placement: FlowPlacement::Main,
            reuse,
        },
        crate::flow_hub::Action::Float(flow) => TuiControl::FocusFlow {
            flow,
            placement: FlowPlacement::Floating,
            reuse,
        },
        _ => return,
    };
    match tx.send_tracked(command) {
        Ok(Some(ticket)) => hub.begin(ticket),
        _ => hub.failed("Flow directory is disconnected".into()),
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn hidden_navigation_feedback_uses_header_visibility_and_ends_without_work() {
        use super::*;
        if crate::panel_registry::EXTENDED {
            return;
        }
        let mut ui = UiState::new(AppState::new("navigation".into(), None));
        ui.app.last_full_rect = Some(ratatui::layout::Rect::new(0, 0, 120, 40));
        let (commands, _incoming) = mpsc::unbounded_channel();
        let flow = crate::control::ViewAddress::Legacy {
            session: uuid::Uuid::now_v7(),
            owner: atman_runtime::event_log::legacy::LegacyFlowKey::Default,
        };
        let binding = crate::control::ViewBinding {
            view_id: uuid::Uuid::now_v7(),
            revision: 1,
            flow,
        };
        let (control, _replies) = crate::ControlSender::for_flow(binding, commands);
        let mut hub = crate::flow_hub::FlowHub::default();
        dispatch_hub_action(
            &mut hub,
            crate::flow_hub::Action::OpenSession(uuid::Uuid::now_v7()),
            &control,
            None,
        );
        assert!(!hub.visible());
        assert!(ui.navigation_animates(Some(&hub)));
        assert!(ui.navigation_can_open(Some(&hub)));
        refresh_navigation_progress(&mut ui.app, Some(&hub), false);
        assert!(!ui.app.status_notes.contains_key("view-open"));
        tokio::time::sleep(crate::loading::REFRESH_DELAY + Duration::from_millis(1)).await;
        refresh_navigation_progress(&mut ui.app, Some(&hub), false);
        assert!(ui.app.status_notes["view-open"].contains("Opening"));
        let covering = ui.wm.open(
            "cover",
            crate::wm::ContentKey::Cheatsheet,
            crate::wm::WindowContent::Cheatsheet,
            "Covered",
            ratatui::layout::Rect::new(0, 0, 120, 40),
        );
        ui.wm
            .move_panel(covering, 0, 0, ratatui::layout::Rect::new(0, 0, 120, 40));
        ui.wm
            .resize_panel(covering, 120, 40, ratatui::layout::Rect::new(0, 0, 120, 40));
        assert!(
            ui.wm
                .covers_region(ratatui::layout::Rect::new(0, 0, 120, 1))
        );
        assert!(!ui.navigation_animates(Some(&hub)));
        assert!(!ui.navigation_can_open(Some(&hub)));
        hub.opening_focus = Some(covering);
        assert!(
            ui.navigation_can_open(Some(&hub)),
            "the originating panel does not block its own navigation"
        );
        ui.wm.close(covering);
        assert!(ui.navigation_can_open(Some(&hub)));
        assert!(ui.navigation_animates(Some(&hub)));
        refresh_navigation_progress(&mut ui.app, Some(&hub), true);
        assert!(!ui.app.status_notes.contains_key("view-open"));
        hub.failed("original navigation error".into());
        assert!(!ui.navigation_animates(Some(&hub)));
        assert_eq!(hub.navigation_error(), Some("original navigation error"));
        assert!(ui.app.items.is_empty());
    }

    #[test]
    fn composer_click_transfers_keyboard_focus_from_interactive_panels() {
        use super::*;
        for (border, overlap) in [(false, false), (true, false), (false, true), (true, true)] {
            let mut ui = UiState::new(AppState::new("session".into(), None));
            let mut editor = InputEditor::default();
            let (sender, mut incoming) = crate::ControlSender::channel();
            let window = ui.wm.open(
                "knowledge",
                crate::wm::ContentKey::Knowledge,
                crate::wm::WindowContent::Knowledge,
                "Memory & Rules",
                ratatui::layout::Rect::new(0, 0, 120, 40),
            );
            ui.wm
                .panels
                .iter_mut()
                .find(|panel| panel.id == window)
                .unwrap()
                .content = Some(Box::new(
                crate::window::knowledge_panel::KnowledgePanelContent::new(
                    ui.app.knowledge_state.clone(),
                    Some(sender.clone()),
                ),
            ));
            ui.app
                .knowledge_state
                .lock()
                .unwrap()
                .request_read(Some(&sender), false);
            assert!(matches!(
                incoming.try_recv(),
                Ok(TuiControl::ListKnowledge { .. })
            ));
            assert!(ui.wm.focused_id().is_some());
            let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 45)).unwrap();
            terminal
                .draw(|frame| render_frame(frame, &mut ui, &editor))
                .unwrap();
            let rect = ui.app.input_rect.unwrap();
            let editor_rect = crate::layout::input_editor_rect(rect);
            let mouse = crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: editor_rect.x,
                row: if border { rect.y } else { editor_rect.y },
                modifiers: KeyModifiers::NONE,
            };
            if overlap {
                ui.wm
                    .panels
                    .iter_mut()
                    .find(|panel| panel.id == window)
                    .unwrap()
                    .rect = rect;
            }
            ui.wm.interaction.drag_target = Some(window);
            let drag = crossterm::event::MouseEvent {
                kind: MouseEventKind::Drag(MouseButton::Left),
                ..mouse
            };
            assert!(!handle_composer_mouse(
                &mut ui,
                &mut editor,
                &drag,
                None,
                Some(&sender)
            ));
            assert_eq!(ui.wm.focused_id(), Some(window));
            ui.wm.interaction.drag_target = None;
            let (consumed, commands) = ui.wm.dispatch_mouse(&mouse, &mut ui.app, Some(&sender));
            assert!(commands.is_empty());
            assert!(!consumed, "visible composer owns its pointer hit");
            assert!(handle_composer_mouse(
                &mut ui,
                &mut editor,
                &mouse,
                None,
                Some(&sender)
            ));
            for ch in "A retained draft".chars() {
                let action = KeyAction::Char(ch);
                let (consumed, commands) = ui.wm.dispatch_key(&action, &mut ui.app, Some(&sender));
                assert!(commands.is_empty());
                if !consumed {
                    key_handler::handle_key(
                        action,
                        &mut ui,
                        &mut editor,
                        &mut None,
                        None,
                        Some(&sender),
                    );
                }
            }
            assert_eq!(editor.buf(), "A retained draft");
            assert!(ui.wm.focused_id().is_none());
            assert_eq!(
                ui.wm.panels.len(),
                1,
                "transferring focus retains the panel"
            );
            assert!(
                ui.app
                    .knowledge_state
                    .lock()
                    .unwrap()
                    .read_request
                    .is_some()
            );
            assert!(incoming.try_recv().is_err());
            terminal
                .draw(|frame| render_frame(frame, &mut ui, &editor))
                .unwrap();
            let rect = crate::layout::input_editor_rect(ui.app.input_rect.unwrap());
            let row: String = (rect.x..rect.right())
                .map(|x| terminal.backend().buffer()[(x, rect.y)].symbol())
                .collect();
            assert!(
                row.contains("A retained draft"),
                "ordinary panels must not paint over input: {row}"
            );
            ui.app.modal_notification = Some("MODAL-OVER-COMPOSER".into());
            terminal
                .draw(|frame| {
                    crate::render::render_window_layers(frame, &mut ui, |frame, _| {
                        let area = frame.area();
                        for y in area.y..area.bottom() {
                            for x in area.x..area.right() {
                                frame.buffer_mut()[(x, y)].set_symbol("z");
                            }
                        }
                    })
                })
                .unwrap();
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            assert!(
                text.contains("MODAL-OVER-COMPOSER"),
                "blocking overlays remain above input"
            );
        }
    }

    #[test]
    fn knowledge_reads_accept_only_their_original_completion_and_preserve_drafts() {
        use super::*;
        let mut ui = UiState::new(AppState::new("session".into(), None));
        let mut editor = InputEditor::default();
        editor.insert_str("retained draft");
        let (tx, mut incoming) = crate::ControlSender::channel();
        ui.app
            .knowledge_state
            .lock()
            .unwrap()
            .request_read(Some(&tx), false);
        let TuiControl::ListKnowledge { request_id } = incoming.try_recv().unwrap() else {
            panic!("catalog request")
        };
        apply_view_command(
            &mut ui,
            &mut editor,
            None,
            TuiCommand::KnowledgeResult {
                request_id: Some(uuid::Uuid::now_v7()),
                result: Err("stale".into()),
            },
            Some(&tx),
        );
        assert_eq!(
            ui.app.knowledge_state.lock().unwrap().read_request,
            Some(request_id)
        );
        assert!(ui.app.knowledge_state.lock().unwrap().read_error.is_none());
        apply_view_command(
            &mut ui,
            &mut editor,
            None,
            TuiCommand::KnowledgeResult {
                request_id: None,
                result: Ok((Vec::new(), Vec::new())),
            },
            Some(&tx),
        );
        assert_eq!(
            ui.app.knowledge_state.lock().unwrap().read_request,
            Some(request_id)
        );
        apply_view_command(
            &mut ui,
            &mut editor,
            None,
            TuiCommand::KnowledgeResult {
                request_id: Some(request_id),
                result: Err("read failed".into()),
            },
            Some(&tx),
        );
        assert!(
            ui.app
                .knowledge_state
                .lock()
                .unwrap()
                .read_request
                .is_none()
        );
        assert_eq!(
            ui.app.knowledge_state.lock().unwrap().read_error.as_deref(),
            Some("read failed")
        );
        ui.app
            .knowledge_state
            .lock()
            .unwrap()
            .request_read(Some(&tx), true);
        let TuiControl::ReloadRules { request_id: retry } = incoming.try_recv().unwrap() else {
            panic!("reload request")
        };
        assert_ne!(retry, request_id);
        apply_view_command(
            &mut ui,
            &mut editor,
            None,
            TuiCommand::KnowledgeResult {
                request_id: Some(retry),
                result: Ok((Vec::new(), Vec::new())),
            },
            Some(&tx),
        );
        assert!(ui.app.knowledge_state.lock().unwrap().read_error.is_none());
        assert_eq!(
            ui.app.knowledge_state.lock().unwrap().message,
            "0 confessions · 0 rules"
        );
        apply_view_command(
            &mut ui,
            &mut editor,
            None,
            TuiCommand::KnowledgeResult {
                request_id: Some(request_id),
                result: Err("late old error".into()),
            },
            Some(&tx),
        );
        assert!(ui.app.knowledge_state.lock().unwrap().read_error.is_none());
        let id = atman_runtime::memory::MemoryId::now();
        let history_request = uuid::Uuid::now_v7();
        ui.app.knowledge_state.lock().unwrap().history_request =
            Some((history_request, id.clone()));
        apply_view_command(
            &mut ui,
            &mut editor,
            None,
            TuiCommand::ConfessionHistoryResult {
                request_id: uuid::Uuid::now_v7(),
                id: id.clone(),
                result: Ok(Vec::new()),
            },
            Some(&tx),
        );
        assert!(
            ui.app
                .knowledge_state
                .lock()
                .unwrap()
                .history_request
                .is_some()
        );
        apply_view_command(
            &mut ui,
            &mut editor,
            None,
            TuiCommand::ConfessionHistoryResult {
                request_id: history_request,
                id: id.clone(),
                result: Ok(Vec::new()),
            },
            Some(&tx),
        );
        assert!(
            ui.app
                .knowledge_state
                .lock()
                .unwrap()
                .history_request
                .is_none()
        );
        assert_eq!(
            ui.app.knowledge_state.lock().unwrap().history_id.as_ref(),
            Some(&id)
        );
        assert_eq!(editor.buf(), "retained draft");
    }

    #[tokio::test]
    async fn inbox_recovery_keeps_drafts_and_acknowledgements_with_the_original_view() {
        use super::*;
        use crate::control::ViewBinding;
        use crate::submission_queue::QueueAction;
        use atman_runtime::flow_store::{
            EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowTransition, RuntimeStore,
            SessionConfiguration, SessionDescriptor, completion::InvocationResult, inbox::*,
        };
        let temporary = tempfile::tempdir().unwrap();
        let config =
            atman_runtime::config_hub::ConfigHub::from_config_dir(temporary.path().join("config"));
        config.models().replace_config_text("[models.smart]\nprovider='fixture'\nmodel='vendor/model'\ncontext_budget=8192\n[alias.cheap]\nmodel='smart'\n").unwrap();
        let flow = FlowRef {
            session_id: uuid::Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let root = RuntimeStore::open(temporary.path()).unwrap();
        let mut session = root
            .create_session(SessionDescriptor {
                id: flow.session_id.0,
                project: None,
                configuration: SessionConfiguration::Atman(config.source()),
                title: "Recovery".into(),
            })
            .unwrap();
        let mut store = session
            .create_flow(FlowDescriptor {
                address: flow.clone(),
                creation_request: uuid::Uuid::now_v7(),
                created_by: None,
                content: EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: atman_runtime::source_program::RevisionBundle {
                        entry: "main.at".into(),
                        sources: [("main.at".into(), "flow main() {}".into())].into(),
                        edges: Default::default(),
                    },
                }
                .into(),
            })
            .unwrap();
        store
            .transition(store.state().revision(), FlowTransition::Activated)
            .unwrap();
        store
            .transition(
                store.state().revision(),
                FlowTransition::TurnStarted {
                    turn: atman_rt::TurnId::now(),
                    run: atman_rt::RunId::now(),
                },
            )
            .unwrap();
        let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) =
            store.state().state()
        else {
            panic!("active turn")
        };
        let lease = active.lease.clone();
        for text in ["first queued", "second queued"] {
            store
                .accept_input(
                    DeliveryRequest {
                        request_id: uuid::Uuid::now_v7(),
                        sender: SenderIdentity::User(uuid::Uuid::now_v7()),
                        recipient: flow.clone(),
                        delivery: DeliveryMode::NextTurn,
                        cause: None,
                        correlation: None,
                        scope: None,
                        payload: InboxPayload::UserSubmission {
                            content: UserContent {
                                text: text.into(),
                                images: vec![],
                                presentation: None,
                            },
                            invocation: Default::default(),
                        },
                    },
                    |_| Ok(()),
                )
                .unwrap();
        }
        store
            .finish_turn(
                lease,
                InvocationResult::Unobserved(atman_rt::TurnOutcome::Interrupted(
                    atman_rt::Interruption::Cancelled,
                )),
            )
            .unwrap();
        let worker = atman_runtime::flow_runtime::FlowWorker::start(store)
            .await
            .unwrap();
        worker
            .context()
            .bind_model_registry(config.models().clone())
            .unwrap();
        let (commands, mut requests) = mpsc::unbounded_channel();
        let mut handle = TuiHandle::from_flow(
            &worker,
            ViewBinding {
                view_id: uuid::Uuid::now_v7(),
                revision: 1,
                flow: flow.clone().into(),
            },
            commands,
            config,
        )
        .await
        .unwrap();
        let (mut ui, mut editor) = initialize_view(&mut handle).await.unwrap();
        editor.insert_str("keep draft");
        ui.app.pending_quote = Some("keep quote".into());
        key_handler::handle_key(
            KeyAction::Submit,
            &mut ui,
            &mut editor,
            &mut None,
            None,
            handle.control_tx.as_ref(),
        );
        let input = requests.try_recv().unwrap();
        ui.app
            .queue_action(0, QueueAction::Recover, handle.control_tx.as_ref());
        assert!(
            requests.try_recv().is_err(),
            "recovery must wait for an in-flight submission"
        );
        apply_view_command(
            &mut ui,
            &mut editor,
            Some(&input.ticket),
            TuiCommand::SubmissionResult {
                result: Err("input retained".into()),
            },
            handle.control_tx.as_ref(),
        );
        ui.app.submission_focus = true;
        ui.app.queue_action(0, QueueAction::Recover, None);
        assert_eq!(
            ui.app.queue_recovery_error.as_deref(),
            Some("Recovery controls are disconnected")
        );
        key_handler::handle_key(
            KeyAction::Char('r'),
            &mut ui,
            &mut editor,
            &mut None,
            None,
            handle.control_tx.as_ref(),
        );
        let first = requests.try_recv().unwrap();
        let TuiControl::Submit(submission) = &first.command else {
            panic!("existing control command")
        };
        assert_eq!(submission.control_command(), Some(":recover"));
        assert_eq!(first.ticket.binding.flow.instance(), Some(&flow));
        assert!(
            submission.images.is_empty()
                && submission.reasoning.is_none()
                && submission.presentation.is_none()
        );
        key_handler::handle_key(
            KeyAction::Char('r'),
            &mut ui,
            &mut editor,
            &mut None,
            None,
            handle.control_tx.as_ref(),
        );
        assert!(
            requests.try_recv().is_err(),
            "pending recovery cannot be duplicated"
        );
        let mut stale = first.ticket.clone();
        stale.request_id = uuid::Uuid::now_v7();
        apply_view_command(
            &mut ui,
            &mut editor,
            Some(&stale),
            TuiCommand::SubmissionResult { result: Ok(()) },
            handle.control_tx.as_ref(),
        );
        assert!(ui.app.queue_recovery_ticket.is_some());
        apply_view_command(
            &mut ui,
            &mut editor,
            Some(&first.ticket),
            TuiCommand::SubmissionResult {
                result: Err("retry after reconnect".into()),
            },
            handle.control_tx.as_ref(),
        );
        assert!(ui.app.queue_recovery_ticket.is_none());
        assert_eq!(
            ui.app.queue_recovery_error.as_deref(),
            Some("retry after reconnect")
        );
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| render_frame(frame, &mut ui, &editor))
            .unwrap();
        let rect = ui
            .app
            .submission_queue_hitmap
            .actions
            .iter()
            .find(|(_, action, _)| *action == QueueAction::Recover)
            .unwrap()
            .2;
        assert_eq!(rect.height, crate::submission_queue::HEADER_ROWS);
        let mouse = |column, row| crossterm::event::MouseEvent {
            kind: MouseEventKind::Moved,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        };
        assert!(handle_composer_mouse(
            &mut ui,
            &mut editor,
            &mouse(rect.x, rect.y),
            None,
            handle.control_tx.as_ref()
        ));
        assert_eq!(ui.app.hovered_submission_action, Some(QueueAction::Recover));
        terminal
            .draw(|frame| render_frame(frame, &mut ui, &editor))
            .unwrap();
        let hovered_bg = terminal.backend().buffer()[(rect.x, rect.y)].bg;
        assert_eq!(hovered_bg, crate::theme::theme().work_hover_bg.into());
        let recover_label = (rect.x..rect.right())
            .map(|x| terminal.backend().buffer()[(x, rect.y + rect.height / 2)].symbol())
            .collect::<String>();
        assert!(
            recover_label.contains("Recover R"),
            "recovery retains its internal vertical padding"
        );
        assert!(!handle_composer_mouse(
            &mut ui,
            &mut editor,
            &mouse(0, 0),
            None,
            handle.control_tx.as_ref()
        ));
        assert_eq!(ui.app.hovered_submission_action, None);
        assert!(handle_composer_mouse(
            &mut ui,
            &mut editor,
            &crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x,
                row: rect.bottom() - 1,
                modifiers: KeyModifiers::NONE,
            },
            None,
            handle.control_tx.as_ref()
        ));
        let retry = requests.try_recv().unwrap();
        assert_ne!(retry.ticket, first.ticket);
        apply_view_command(
            &mut ui,
            &mut editor,
            Some(&first.ticket),
            TuiCommand::SubmissionResult { result: Ok(()) },
            handle.control_tx.as_ref(),
        );
        assert!(ui.app.queue_recovery_ticket.is_some());
        apply_view_command(
            &mut ui,
            &mut editor,
            Some(&retry.ticket),
            TuiCommand::SubmissionResult { result: Ok(()) },
            handle.control_tx.as_ref(),
        );
        assert!(ui.app.queue_recovery_ticket.is_none());
        assert_eq!(editor.buf(), "keep draft");
        assert_eq!(ui.app.pending_quote.as_deref(), Some("keep quote"));
        assert_eq!(
            ui.app.queued_submissions.len(),
            2,
            "only durable consumption removes queued input"
        );
        assert!(ui.app.pending_submission.is_none());
        worker.shutdown().await.unwrap();
    }

    #[test]
    fn history_search_routes_ctrl_k_and_replies_by_view_and_request() {
        use super::*;
        use crate::control::{ControlSender, ViewAddress, ViewBinding};
        use atman_runtime::{
            event_log::legacy::LegacyFlowKey,
            history_search::{HistoryHit, HistoryLocation, SearchResults, SearchScope},
        };
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        fn press(
            ui: &mut UiState,
            editor: &mut InputEditor,
            sender: &ControlSender,
            action: KeyAction,
        ) {
            let (consumed, commands) = ui.wm.dispatch_key(&action, &mut ui.app, Some(sender));
            assert!(commands.is_empty());
            if !consumed {
                key_handler::handle_key(action, ui, editor, &mut None, None, Some(sender));
            }
        }
        let session = uuid::Uuid::now_v7();
        let (commands, mut incoming) = mpsc::unbounded_channel();
        let binding = ViewBinding {
            view_id: uuid::Uuid::now_v7(),
            revision: 1,
            flow: ViewAddress::Legacy {
                session,
                owner: LegacyFlowKey::Default,
            },
        };
        let (sender, mut replies) = ControlSender::for_flow(binding.clone(), commands.clone());
        let (other_sender, _other_replies) = ControlSender::for_flow(
            ViewBinding {
                view_id: uuid::Uuid::now_v7(),
                ..binding
            },
            commands,
        );
        let mut ui = UiState::new(AppState::default());
        let mut other = UiState::new(AppState::default());
        let mut editor = InputEditor::default();
        let key = crate::keys::map(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));
        press(&mut ui, &mut editor, &sender, key);
        assert!(ui.wm.modals.history_search.open);
        assert!(
            ui.app.session.is_none(),
            "search must not depend on a legacy execution Session"
        );
        ui.wm.modals.history_search.editor.replace_with("needle");
        press(&mut ui, &mut editor, &sender, KeyAction::Submit);
        let first = incoming.try_recv().unwrap();
        assert!(
            matches!(&first.command, TuiControl::SearchHistory { request, .. } if request.query == "needle" && request.scope == SearchScope::Session)
        );
        let result = || SearchResults {
            hits: (1..=2)
                .map(|seq| HistoryHit {
                    location: HistoryLocation::Indexed {
                        index: "/index.db".into(),
                        session: session.to_string(),
                        seq,
                    },
                    session_id: session.to_string(),
                    seq,
                    ts: String::new(),
                    kind: "user_msg".into(),
                    snippet: "needle".into(),
                    preview: vec![format!("context {seq}")],
                })
                .collect(),
            warnings: Vec::new(),
        };
        other.wm.modals.history_search.open();
        other.wm.modals.history_search.editor.replace_with("needle");
        other
            .wm
            .modals
            .history_search
            .run_search(Some(&other_sender));
        incoming.try_recv().unwrap();
        apply_view_command(
            &mut other,
            &mut editor,
            Some(&first.ticket),
            TuiCommand::HistorySearchResults {
                result: Ok(result()),
            },
            Some(&other_sender),
        );
        assert!(other.wm.modals.history_search.results.is_empty());
        first
            .replies
            .send(TuiCommand::HistorySearchResults {
                result: Ok(result()),
            })
            .unwrap();
        let reply = replies.try_recv().unwrap();
        apply_view_command(
            &mut ui,
            &mut editor,
            Some(&reply.ticket),
            reply.command,
            Some(&sender),
        );
        assert_eq!(ui.wm.modals.history_search.preview_lines, ["context 1"]);
        press(&mut ui, &mut editor, &sender, KeyAction::HistoryDown);
        assert_eq!(ui.wm.modals.history_search.preview_lines, ["context 2"]);
        press(&mut ui, &mut editor, &sender, KeyAction::Tab);
        let project = incoming.try_recv().unwrap();
        assert!(
            matches!(&project.command, TuiControl::SearchHistory { request, .. } if request.scope == SearchScope::Project)
        );
        apply_view_command(
            &mut ui,
            &mut editor,
            Some(&first.ticket),
            TuiCommand::HistorySearchResults {
                result: Ok(result()),
            },
            Some(&sender),
        );
        assert!(ui.wm.modals.history_search.results.is_empty());
        press(&mut ui, &mut editor, &sender, KeyAction::Char('!'));
        assert!(
            matches!(&project.command, TuiControl::SearchHistory { cancellation, .. } if cancellation.is_cancelled())
        );
        apply_view_command(
            &mut ui,
            &mut editor,
            Some(&project.ticket),
            TuiCommand::HistorySearchResults {
                result: Ok(result()),
            },
            Some(&sender),
        );
        assert!(ui.wm.modals.history_search.results.is_empty());
        press(&mut ui, &mut editor, &sender, KeyAction::Submit);
        let closed = incoming.try_recv().unwrap();
        press(&mut ui, &mut editor, &sender, KeyAction::Escape);
        assert!(
            matches!(&closed.command, TuiControl::SearchHistory { cancellation, .. } if cancellation.is_cancelled())
        );
        ui.wm.modals.history_search.open();
        apply_view_command(
            &mut ui,
            &mut editor,
            Some(&closed.ticket),
            TuiCommand::HistorySearchResults {
                result: Ok(result()),
            },
            Some(&sender),
        );
        assert!(ui.wm.modals.history_search.results.is_empty());
        ui.wm.modals.history_search.editor.replace_with("needle");
        ui.wm.modals.history_search.run_search(None);
        assert!(
            ui.wm
                .modals
                .history_search
                .error
                .as_ref()
                .unwrap()
                .contains("unavailable")
        );
    }

    #[tokio::test]
    async fn historical_composer_submits_through_its_binding_and_keeps_failed_or_newer_drafts() {
        use super::*;
        use crate::control::{ViewAddress, ViewBinding};
        use atman_runtime::{
            event::{Event, EventEnvelope},
            event_log::legacy::LegacyFlowKey,
            message::Message,
            projection::flow::LegacyHistory,
        };
        let data = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::now_v7();
        let turn = atman_rt::TurnId::now();
        let source = data.path().join("events.jsonl");
        let event = EventEnvelope::new(
            1,
            Event::UserMsg {
                turn_id: turn.clone(),
                flow_run_id: None,
                message: Message::user_text(turn, "past request"),
                presentation: None,
                injection_id: None,
            },
        );
        std::fs::write(
            &source,
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        let view = LegacyHistory::inspect(&source, session)
            .unwrap()
            .read_flow(&LegacyFlowKey::Default)
            .unwrap();
        let binding = ViewBinding {
            view_id: uuid::Uuid::now_v7(),
            revision: 1,
            flow: ViewAddress::Legacy {
                session,
                owner: LegacyFlowKey::Default,
            },
        };
        let (commands, mut incoming) = mpsc::unbounded_channel();
        let mut handle =
            TuiHandle::from_legacy(view, binding.clone(), commands.clone(), None, None).unwrap();
        let (mut ui, mut editor) = initialize_view(&mut handle).await.unwrap();
        assert!(ui.app.items.is_archive());
        editor.insert_str("continue this conversation");
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 40)).unwrap();
        terminal
            .draw(|frame| crate::render::render_frame(frame, &mut ui, &editor))
            .unwrap();
        let send = ui
            .app
            .input_controls
            .iter()
            .find(|(action, _)| *action == crate::input::InputAction::Send)
            .unwrap()
            .1;
        assert!(handle_composer_mouse(
            &mut ui,
            &mut editor,
            &crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: send.x,
                row: send.y,
                modifiers: KeyModifiers::NONE,
            },
            None,
            handle.control_tx.as_ref(),
        ));
        let request = incoming.recv().await.unwrap();
        assert_eq!(request.ticket.binding, binding);
        assert!(ui.app.pending_submission.is_some());
        apply_view_command(
            &mut ui,
            &mut editor,
            Some(&request.ticket),
            TuiCommand::SubmissionResult {
                result: Err("source unavailable".into()),
            },
            handle.control_tx.as_ref(),
        );
        assert_eq!(editor.buf(), "continue this conversation");
        assert!(ui.app.pending_submission.is_none());
        key_handler::handle_key(
            KeyAction::Submit,
            &mut ui,
            &mut editor,
            &mut None,
            None,
            handle.control_tx.as_ref(),
        );
        let retry = incoming.recv().await.unwrap();
        apply_view_command(
            &mut ui,
            &mut editor,
            Some(&request.ticket),
            TuiCommand::SubmissionResult { result: Ok(()) },
            handle.control_tx.as_ref(),
        );
        assert!(ui.app.pending_submission.is_some());
        editor.insert_str(" with another detail");
        apply_view_command(
            &mut ui,
            &mut editor,
            Some(&retry.ticket),
            TuiCommand::SubmissionResult { result: Ok(()) },
            handle.control_tx.as_ref(),
        );
        assert_eq!(
            editor.buf(),
            "continue this conversation with another detail"
        );
        assert!(ui.app.pending_submission.is_none());
        assert!(ui.app.items.is_archive());
        assert_eq!(
            handle.control_tx.as_ref().unwrap().binding(),
            Some(&binding)
        );
        key_handler::handle_key(
            KeyAction::Submit,
            &mut ui,
            &mut editor,
            &mut None,
            None,
            handle.control_tx.as_ref(),
        );
        let background = incoming.recv().await.unwrap();
        let make_handle = || {
            TuiHandle::from_legacy(
                LegacyHistory::inspect(&source, session)
                    .unwrap()
                    .read_flow(&LegacyFlowKey::Default)
                    .unwrap(),
                ViewBinding {
                    view_id: uuid::Uuid::now_v7(),
                    ..binding.clone()
                },
                commands.clone(),
                None,
                None,
            )
            .unwrap()
        };
        let mut views = Workspace::new(&handle);
        let main = ViewKey {
            flow: binding.flow.clone(),
            placement: FlowPlacement::Main,
        };
        views
            .focus_ready(
                &mut ui,
                &mut editor,
                &mut handle,
                ViewKey {
                    flow: binding.flow.clone(),
                    placement: FlowPlacement::Floating,
                },
                Some(Box::new(make_handle())),
            )
            .await
            .unwrap();
        editor.insert_str("separate floating draft");
        background
            .replies
            .send(TuiCommand::SubmissionContinued {
                flow: binding.flow.clone(),
                attachment: Box::new(make_handle()),
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while views.views[&main].ui.app.pending_submission.is_some() {
                views.next_background().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(editor.buf(), "separate floating draft");
        assert_eq!(views.active_placement(), FlowPlacement::Floating);
        assert!(views.views[&main].editor.buf().is_empty());
        assert!(views.views[&main].ui.app.items.is_archive());
    }

    #[tokio::test]
    async fn cached_focus_preserves_the_reader_cursor_and_independent_drafts() {
        use crate::control::{ViewAddress, ViewBinding};
        use atman_runtime::{
            event::{Event, EventEnvelope},
            event_log::legacy::LegacyFlowKey,
            message::Message,
            projection::flow::LegacyHistory,
        };
        let data = tempfile::tempdir().unwrap();
        let session = uuid::Uuid::now_v7();
        let turn = atman_rt::TurnId::now();
        let source = data.path().join("events.jsonl");
        let event = EventEnvelope::new(
            1,
            Event::UserMsg {
                turn_id: turn.clone(),
                flow_run_id: None,
                message: Message::user_text(turn, "Past request"),
                presentation: None,
                injection_id: None,
            },
        );
        std::fs::write(
            &source,
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        let (commands, _) = mpsc::unbounded_channel();
        let address = ViewAddress::Legacy {
            session,
            owner: LegacyFlowKey::Default,
        };
        let make_handle = || {
            TuiHandle::from_legacy(
                LegacyHistory::inspect(&source, session)
                    .unwrap()
                    .read_flow(&LegacyFlowKey::Default)
                    .unwrap(),
                ViewBinding {
                    view_id: uuid::Uuid::now_v7(),
                    revision: 1,
                    flow: address.clone(),
                },
                commands.clone(),
                None,
                None,
            )
            .unwrap()
        };
        let mut handle = make_handle();
        let (mut ui, mut editor) = initialize_view(&mut handle).await.unwrap();
        editor.insert_str("Main draft");
        ui.app.pending_quote = Some("Main quote".into());
        let mut views = Workspace::new(&handle);
        let main = ViewKey {
            flow: address.clone(),
            placement: FlowPlacement::Main,
        };
        views
            .focus_ready(
                &mut ui,
                &mut editor,
                &mut handle,
                ViewKey {
                    flow: address.clone(),
                    placement: FlowPlacement::Floating,
                },
                Some(Box::new(make_handle())),
            )
            .await
            .unwrap();
        editor.insert_str("Floating draft");
        let cached = views.views.get_mut(&main).unwrap();
        let crate::FlowViewSource::Legacy { delivered, .. } =
            cached.handle.flow_view.as_mut().unwrap()
        else {
            unreachable!()
        };
        *delivered = false;
        let revision = cached.ui.app.items.revision_clock();
        views
            .focus_ready(&mut ui, &mut editor, &mut handle, main, None)
            .await
            .unwrap();
        assert!(
            matches!(
                handle.flow_view.as_ref(),
                Some(crate::FlowViewSource::Legacy {
                    delivered: false,
                    ..
                })
            ),
            "changing focus must not consume or rebuild its reader"
        );
        assert_eq!(ui.app.items.revision_clock(), revision);
        assert_eq!(editor.buf(), "Main draft");
        assert_eq!(ui.app.pending_quote.as_deref(), Some("Main quote"));
        let update = handle.flow_view.as_mut().unwrap().next().await.unwrap();
        ui.app.sync_flow_view(update).unwrap();
        assert_eq!(ui.app.items.revision_clock(), revision);
        assert_eq!(editor.buf(), "Main draft");
    }

    #[tokio::test]
    async fn disconnected_views_replace_sources_without_losing_drafts() {
        use crate::control::ViewBinding;
        use atman_runtime::{
            flow_runtime::FlowWorker,
            flow_store::{
                EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, RuntimeStore,
                SessionDescriptor,
            },
            source_program::RevisionBundle,
        };
        use uuid::Uuid;

        let directory = tempfile::tempdir().unwrap();
        let config =
            atman_runtime::config_hub::ConfigHub::from_config_dir(directory.path().join("config"));
        let root = RuntimeStore::open(directory.path()).unwrap();
        let id = Uuid::now_v7();
        let mut session = root
            .create_session(SessionDescriptor {
                id,
                project: None,
                configuration: Default::default(),
                title: "Reconnect".into(),
            })
            .unwrap();
        let address = FlowRef {
            session_id: id.into(),
            instance_id: FlowInstanceId::now(),
        };
        let store = session
            .create_flow(FlowDescriptor {
                address: address.clone(),
                creation_request: Uuid::now_v7(),
                created_by: None,
                content: EntrySnapshot {
                    name: "main".into(),
                    path: "main.at".into(),
                    bundle: RevisionBundle {
                        entry: "main.at".into(),
                        edges: Default::default(),
                        sources: [("main.at".into(), "flow main() {}".into())].into(),
                    },
                }
                .into(),
            })
            .unwrap();
        let worker = FlowWorker::start(store).await.unwrap();
        worker
            .context()
            .bind_model_registry(config.models().clone())
            .unwrap();
        let (commands, mut incoming) = mpsc::unbounded_channel();
        let mut handle = TuiHandle::from_flow(
            &worker,
            ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: address.clone().into(),
            },
            commands.clone(),
            config.clone(),
        )
        .await
        .unwrap();
        let (mut ui, mut editor) = initialize_view(&mut handle).await.unwrap();
        editor.insert_str("main draft");
        ui.app.pending_quote = Some("main quote".into());
        ui.app.follow_tail = false;
        ui.app.scroll_offset = 42;
        let mut views = Workspace::new(&handle);
        let main = ViewKey {
            flow: address.clone().into(),
            placement: FlowPlacement::Main,
        };
        let floating = ViewKey {
            placement: FlowPlacement::Floating,
            ..main.clone()
        };
        let attached = TuiHandle::from_flow(
            &worker,
            ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: address.clone().into(),
            },
            commands.clone(),
            config.clone(),
        )
        .await
        .unwrap();
        views
            .focus_ready(
                &mut ui,
                &mut editor,
                &mut handle,
                floating.clone(),
                Some(Box::new(attached)),
            )
            .await
            .unwrap();
        editor.insert_str("floating draft");
        ui.app.pending_quote = Some("floating quote".into());
        ui.app.follow_tail = false;
        ui.app.scroll_offset = 13;

        let store = worker.clone().shutdown().await.unwrap();
        update_flow_view(&mut ui, &mut handle, Err(worker.state().await.unwrap_err()));
        assert!(
            views
                .focus_ready(&mut ui, &mut editor, &mut handle, floating.clone(), None)
                .await
                .is_err()
        );
        assert_eq!(editor.buf(), "floating draft");
        let reopened = FlowWorker::start(store).await.unwrap();
        let reconnected_config = atman_runtime::config_hub::ConfigHub::from_config_dir(
            directory.path().join("reconnected-config"),
        );
        reopened
            .context()
            .bind_model_registry(reconnected_config.models().clone())
            .unwrap();
        for (key, draft, quote, scroll) in [
            (floating, "floating draft", "floating quote", 13),
            (main, "main draft", "main quote", 42),
        ] {
            let binding = ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: address.clone().into(),
            };
            let mut attached = TuiHandle::from_flow(
                &reopened,
                binding.clone(),
                commands.clone(),
                reconnected_config.clone(),
            )
            .await
            .unwrap();
            attached.settings.ready().await;
            views
                .focus_ready(
                    &mut ui,
                    &mut editor,
                    &mut handle,
                    key,
                    Some(Box::new(attached)),
                )
                .await
                .unwrap();
            assert!(ui.app.flow_view_error.is_none());
            assert_eq!(
                handle.control_tx.as_ref().unwrap().binding(),
                Some(&binding)
            );
            assert!(std::sync::Arc::ptr_eq(
                ui.app.session.as_ref().unwrap(),
                &reopened.context()
            ));
            assert_eq!(editor.buf(), draft);
            assert_eq!(ui.app.pending_quote.as_deref(), Some(quote));
            assert_eq!(ui.app.scroll_offset, scroll);
            assert!(!ui.app.follow_tail);
            for settings in [
                &ui.app.settings,
                &ui.wm.modals.settings,
                &ui.wm.modals.model_picker.settings,
            ] {
                assert_eq!(
                    settings.config().unwrap().config_dir(),
                    reconnected_config.config_dir()
                );
            }
            handle
                .control_tx
                .as_ref()
                .unwrap()
                .send(TuiControl::Submit(crate::TuiSubmission {
                    text: draft.into(),
                    images: vec![],
                    reasoning: None,
                    presentation: None,
                }))
                .unwrap();
            assert_eq!(incoming.recv().await.unwrap().ticket.binding, binding);
        }
        reopened.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn native_focus_preserves_view_ownership_and_late_acknowledgements() {
        use crate::control::ViewBinding;
        use atman_runtime::{
            flow_runtime::FlowWorker,
            flow_store::{
                EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowTransition,
                RuntimeStore, SessionDescriptor,
            },
            source_program::RevisionBundle,
        };
        use uuid::Uuid;
        let directory = tempfile::tempdir().unwrap();
        let config =
            atman_runtime::config_hub::ConfigHub::from_config_dir(directory.path().join("config"));
        config.models().replace_config_text(
            "[models.smart]\nprovider = 'vendor'\nmodel = 'example'\ncontext_budget = 8192\n[models.cheap]\nprovider = 'vendor'\nmodel = 'example'\ncontext_budget = 8192\n"
        ).unwrap();
        let root = RuntimeStore::open(directory.path()).unwrap();
        let sid = Uuid::now_v7();
        let mut session = root
            .create_session(SessionDescriptor {
                id: sid,
                project: None,
                configuration: Default::default(),
                title: "Focus test".into(),
            })
            .unwrap();
        let (commands, mut incoming) = mpsc::unbounded_channel();
        let mut workers = Vec::new();
        let mut handles = Vec::new();
        for name in ["a", "b"] {
            let address = FlowRef {
                session_id: sid.into(),
                instance_id: FlowInstanceId::now(),
            };
            let mut store = session
                .create_flow(FlowDescriptor {
                    address: address.clone(),
                    creation_request: Uuid::now_v7(),
                    created_by: None,
                    content: EntrySnapshot {
                        name: "main".into(),
                        path: format!("/example/{name}.at").into(),
                        bundle: RevisionBundle {
                            entry: "main.at".into(),
                            sources: [("main.at".into(), "flow main() -> int { return 7 }".into())]
                                .into(),
                            edges: Default::default(),
                        },
                    }
                    .into(),
                })
                .unwrap();
            store
                .transition(store.state().revision(), FlowTransition::Activated)
                .unwrap();
            let worker = FlowWorker::start(store).await.unwrap();
            worker
                .context()
                .bind_model_registry(config.models().clone())
                .unwrap();
            handles.push(
                TuiHandle::from_flow(
                    &worker,
                    ViewBinding {
                        view_id: Uuid::now_v7(),
                        revision: 1,
                        flow: address.into(),
                    },
                    commands.clone(),
                    config.clone(),
                )
                .await
                .unwrap(),
            );
            workers.push(worker);
        }
        let host = atman_runtime::flow_runtime::host::RuntimeHost::default();
        let session = host.attach_session(session).unwrap();
        for handle in &mut handles {
            handle.session_name_rx = Some(session.subscribe_title());
        }
        let b = handles.pop().unwrap();
        let mut handle = handles.pop().unwrap();
        handle.startup_card = startup_app().startup_card;
        let a_binding = handle
            .control_tx
            .as_ref()
            .unwrap()
            .binding()
            .unwrap()
            .clone();
        let (mut ui, mut editor) = initialize_view(&mut handle).await.unwrap();
        assert_eq!(ui.app.session_name.as_deref(), Some("Focus test"));
        assert!(ui.app.startup_card.is_some());
        assert!(handle.startup_card.is_none());
        assert!(ui.app.items.is_empty());
        let revision = ui.app.items.revision_clock();
        editor.insert_str("draft A");
        ui.app.scroll_offset = 42;
        ui.app.follow_tail = false;
        ui.app.pending_quote = Some("quote A".into());
        update_flow_view(
            &mut ui,
            &mut handle,
            Err(std::io::Error::other("subscription read failed")),
        );
        assert!(handle.flow_view.is_some());
        assert!(ui.app.flow_view_error.is_some());
        assert_eq!(editor.buf(), "draft A");
        assert!(!ui.app.should_quit);
        let mut retry = Workspace::new(&handle);
        let same_view = ViewKey {
            flow: a_binding.flow.clone(),
            placement: FlowPlacement::Main,
        };
        retry
            .focus_ready(&mut ui, &mut editor, &mut handle, same_view, None)
            .await
            .unwrap();
        assert!(ui.app.flow_view_error.is_none());
        assert_eq!(editor.buf(), "draft A");
        assert_eq!(ui.app.scroll_offset, 42);
        assert!(!ui.app.status_notes["flow"].contains("Disconnected"));
        let snapshot = ui.app.items.snapshot().unwrap().clone();
        key_handler::handle_key(
            KeyAction::Submit,
            &mut ui,
            &mut editor,
            &mut None,
            None,
            handle.control_tx.as_ref(),
        );
        assert!(ui.app.pending_submission.is_some());
        assert!(ui.app.startup_card.is_none());
        assert!(ui.app.startup_intro.is_some());
        assert_eq!(ui.app.items.revision_clock(), revision);
        assert!(std::sync::Arc::ptr_eq(
            &snapshot,
            ui.app.items.snapshot().unwrap()
        ));
        ui.app.startup_intro = None;
        let submitted = incoming.recv().await.unwrap();
        ui.app.input_reasoning = Some(atman_runtime::provider::ReasoningSelection::Disabled);
        ui.app.save_ui_state();
        let mut views = Workspace::new(&handle);
        views
            .focus_ready(
                &mut ui,
                &mut editor,
                &mut handle,
                ViewKey {
                    flow: workers[1].address().clone().into(),
                    placement: FlowPlacement::Main,
                },
                Some(Box::new(b)),
            )
            .await
            .unwrap();
        assert!(ui.app.startup_card.is_none());
        assert_eq!(ui.app.input_reasoning, None);
        editor.insert_str("draft B");
        submitted
            .replies
            .send(TuiCommand::SubmissionResult { result: Ok(()) })
            .unwrap();
        assert!(
            handle
                .control_reply_rx
                .as_mut()
                .unwrap()
                .try_recv()
                .is_err()
        );
        assert_eq!(editor.buf(), "draft B");
        assert_eq!(
            ui.app.items.snapshot().unwrap().status.execution.address(),
            workers[1].address()
        );
        views
            .focus_ready(
                &mut ui,
                &mut editor,
                &mut handle,
                ViewKey {
                    flow: workers[0].address().clone().into(),
                    placement: FlowPlacement::Main,
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            handle.control_tx.as_ref().unwrap().binding(),
            Some(&a_binding)
        );
        assert_eq!(editor.buf(), "draft A");
        assert_eq!(
            ui.app.input_reasoning,
            Some(atman_runtime::provider::ReasoningSelection::Disabled)
        );
        assert_eq!(ui.app.pending_quote.as_deref(), Some("quote A"));
        assert_eq!(ui.app.scroll_offset, 42);
        assert!(!ui.app.follow_tail);
        let (reply_ticket, TuiCommand::SubmissionResult { result }) = recv_cmd(
            &mut handle.cmd_rx,
            &mut handle.control_reply_rx,
            Some(&a_binding),
        )
        .await
        .unwrap() else {
            panic!("missing original ACK")
        };
        ui.app
            .submission_result(&mut editor, reply_ticket.as_ref(), result);
        assert!(editor.buf().is_empty());
        views
            .focus_ready(
                &mut ui,
                &mut editor,
                &mut handle,
                ViewKey {
                    flow: workers[1].address().clone().into(),
                    placement: FlowPlacement::Main,
                },
                None,
            )
            .await
            .unwrap();
        assert_eq!(editor.buf(), "draft B");
        assert!(
            views
                .focus_ready(
                    &mut ui,
                    &mut editor,
                    &mut handle,
                    ViewKey {
                        flow: FlowRef {
                            session_id: sid.into(),
                            instance_id: FlowInstanceId::now()
                        }
                        .into(),
                        placement: FlowPlacement::Main
                    },
                    None
                )
                .await
                .is_err()
        );
        assert_eq!(editor.buf(), "draft B");
        // The same flow may be displayed twice, with separate drafts and reply addresses.
        let a_main = ViewKey {
            flow: workers[0].address().clone().into(),
            placement: FlowPlacement::Main,
        };
        views
            .focus_ready(&mut ui, &mut editor, &mut handle, a_main.clone(), None)
            .await
            .unwrap();
        editor.insert_str("main draft");
        let image = workers[0]
            .context()
            .import_image_bytes(b"\x89PNG\r\n\x1a\n", Some("main.png"))
            .unwrap();
        ui.app.attach_composer_image(&mut editor, image.clone());
        let main_draft = editor.buf().to_owned();
        assert_eq!(workers[0].context().pending_image_count(), 0);
        let floating = ViewKey {
            flow: workers[0].address().clone().into(),
            placement: FlowPlacement::Floating,
        };
        let mut floating_handle = TuiHandle::from_flow(
            &workers[0],
            ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: floating.flow.clone(),
            },
            commands.clone(),
            config.clone(),
        )
        .await
        .unwrap();
        floating_handle.session_name_rx = Some(session.subscribe_title());
        let floating_binding = floating_handle
            .control_tx
            .as_ref()
            .unwrap()
            .binding()
            .unwrap()
            .clone();
        assert_ne!(floating_binding.view_id, a_binding.view_id);
        views
            .focus_ready(
                &mut ui,
                &mut editor,
                &mut handle,
                floating.clone(),
                Some(Box::new(floating_handle)),
            )
            .await
            .unwrap();
        assert!(editor.buf().is_empty());
        editor.insert_str("floating draft");
        assert!(editor.pending_images().is_empty());
        ui.app.attach_composer_image(&mut editor, image.clone());
        let floating_draft = editor.buf().to_owned();
        assert_eq!(views.views[&a_main].editor.buf(), main_draft);
        let hidden = ViewKey {
            flow: workers[1].address().clone().into(),
            placement: FlowPlacement::Main,
        };
        session
            .set_title(
                session.snapshot().await.unwrap().cursor,
                Some("Renamed".into()),
            )
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while views.views[&a_main].ui.app.session_name.as_deref() != Some("Renamed") {
                views.next_background().await;
            }
        })
        .await
        .unwrap();
        views::changed(handle.session_name_rx.as_mut()).await;
        refresh_session_name(&mut ui, &mut handle);
        assert_eq!(ui.app.session_name.as_deref(), Some("Renamed"));
        assert_eq!(
            views.views[&hidden].ui.app.session_name.as_deref(),
            Some("Focus test")
        );
        assert_eq!(editor.buf(), floating_draft);
        assert_eq!(views.views[&a_main].editor.buf(), main_draft);
        views
            .views
            .get_mut(&hidden)
            .unwrap()
            .ui
            .wm
            .modals
            .alias_manager
            .open();
        views
            .views
            .get_mut(&a_main)
            .unwrap()
            .ui
            .wm
            .modals
            .alias_manager
            .open();
        ui.wm.modals.alias_manager.open();
        config.add_alias("shared", "example").unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while views.views[&a_main]
                .ui
                .wm
                .modals
                .alias_manager
                .aliases
                .is_empty()
            {
                views.next_background().await;
            }
        })
        .await
        .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), handle.settings.changed())
            .await
            .unwrap();
        apply_provider_catalog_changed(&mut ui, None);
        assert_eq!(
            ui.wm.modals.alias_manager.aliases,
            [("shared".into(), "example".into())]
        );
        assert!(
            views.views[&hidden]
                .ui
                .wm
                .modals
                .alias_manager
                .aliases
                .is_empty()
        );
        let task_id = workers[0].resources().tasks().register(
            atman_runtime::TaskKind::Bash,
            "view activity".into(),
            "bg_activity".into(),
            workers[0].context().id().to_string(),
            Default::default(),
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while !views.views[&a_main]
                .ui
                .app
                .task_snapshots
                .iter()
                .any(|task| task.id == task_id)
            {
                views.next_background().await;
            }
            while !ui.app.task_snapshots.iter().any(|task| task.id == task_id) {
                activity_changed(&mut handle.activity).await;
                refresh_activity(&mut ui, &mut handle);
            }
        })
        .await
        .unwrap();
        assert!(views.views[&hidden].ui.app.task_snapshots.is_empty());
        refresh_instance_live(&mut ui, &mut editor, &handle);
        assert_eq!(editor.buf(), floating_draft);
        assert_eq!(views.views[&a_main].editor.buf(), main_draft);
        assert_eq!(editor.pending_images().len(), 1);
        assert_eq!(views.views[&a_main].editor.pending_images().len(), 1);
        views
            .views
            .get_mut(&a_main)
            .unwrap()
            .ui
            .wm
            .modals
            .alias_manager
            .close();
        ui.wm.modals.alias_manager.close();
        let main_view = views.views.get_mut(&a_main).unwrap();
        update_flow_view(
            &mut main_view.ui,
            &mut main_view.handle,
            Err(std::io::Error::other("background subscription read failed")),
        );
        assert!(main_view.handle.flow_view.is_some());
        assert!(main_view.ui.app.flow_view_error.is_some());
        assert!(ui.app.flow_view_error.is_none());
        let ticket = main_view
            .handle
            .control_tx
            .as_ref()
            .unwrap()
            .send_tracked(TuiControl::Submit(crate::TuiSubmission {
                text: "main draft".into(),
                images: vec![image.clone()],
                reasoning: None,
                presentation: None,
            }))
            .unwrap()
            .unwrap();
        main_view.ui.app.pending_submission = Some(crate::composer::PendingSubmission {
            ticket,
            draft: main_view.editor.prepare_submission(),
            quote: main_view.ui.app.pending_quote.clone(),
            consumes_quote: false,
            images: vec![image.clone()],
        });
        incoming
            .recv()
            .await
            .unwrap()
            .replies
            .send(TuiCommand::SubmissionResult { result: Ok(()) })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while views.views[&a_main].ui.app.pending_submission.is_some() {
                views.next_background().await;
            }
        })
        .await
        .unwrap();
        assert!(views.views[&a_main].editor.buf().is_empty());
        assert_eq!(views.views[&a_main].ui.app.attach_count, 0);
        let imported = workers[0]
            .context()
            .import_image_bytes(b"\x89PNG\r\n\x1a\nimported", Some("imported.png"))
            .unwrap();
        let main_view = views.views.get_mut(&a_main).unwrap();
        main_view.editor.insert_str(":attach imported.png");
        crate::key_handler::handle_key(
            KeyAction::Submit,
            &mut main_view.ui,
            &mut main_view.editor,
            &mut None,
            None,
            main_view.handle.control_tx.as_ref(),
        );
        incoming
            .recv()
            .await
            .unwrap()
            .replies
            .send(TuiCommand::AttachmentImported {
                result: Ok(imported.clone()),
            })
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while views.views[&a_main].ui.app.pending_submission.is_some() {
                views.next_background().await;
            }
        })
        .await
        .unwrap();
        let main_view = views.views.get_mut(&a_main).unwrap();
        assert_eq!(
            main_view.editor.pending_images()[0].source(),
            Some(&imported)
        );
        main_view.editor.insert_str(":attach clear");
        crate::key_handler::handle_key(
            KeyAction::Submit,
            &mut main_view.ui,
            &mut main_view.editor,
            &mut None,
            None,
            main_view.handle.control_tx.as_ref(),
        );
        assert!(main_view.editor.pending_images().is_empty());
        assert!(incoming.try_recv().is_err());
        refresh_instance_live(&mut ui, &mut editor, &handle);
        assert_eq!(editor.buf(), floating_draft);
        assert_eq!(editor.pending_images().len(), 1);
        assert_eq!(ui.app.attach_count, 1);
        assert_eq!(editor.remove_last_image(), Some(image.clone()));
        ui.app.remove_composer_images(&mut editor, &[image]);
        assert_eq!(editor.buf(), "floating draft");
        views
            .views
            .get_mut(&a_main)
            .unwrap()
            .editor
            .insert_str("main draft");
        let mut terminal = Terminal::new(ratatui::backend::TestBackend::new(160, 50)).unwrap();
        terminal
            .draw(|frame| views.render(frame, &mut ui, &editor).unwrap())
            .unwrap();
        let content = ui.app.last_full_rect.unwrap();
        assert!(content.x > 0 && content.y > 0 && content.width < 160 && content.height < 50);
        let composer = ui.app.composer_rect.unwrap();
        assert!(composer.y >= content.bottom());
        assert!(composer.bottom() <= 50);
        assert!(views.views[&a_main].ui.app.input_rect.is_none());
        let cells = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(cells.contains("floating draft"));
        assert!(!cells.contains("main draft"));
        let click = crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: composer.x + 4,
            row: composer.y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(matches!(
            views.event(&CtEvent::Mouse(click), true),
            views::Action::Pass
        ));
        assert!(handle_composer_mouse(
            &mut ui,
            &mut editor,
            &click,
            handle.submit_tx.as_ref(),
            handle.control_tx.as_ref()
        ));
        assert_eq!(editor.buf(), "floating draft");
        assert_eq!(
            views.views[&a_main].ui.app.last_full_rect,
            Some(ratatui::layout::Rect::new(30, 0, 130, 50))
        );
        assert!(std::sync::Arc::ptr_eq(
            &ui.app.project_sidebar,
            &views.views[&a_main].ui.app.project_sidebar
        ));
        let key = |code| CtEvent::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE));
        views.event(&key(crossterm::event::KeyCode::F(6)), true);
        views.event(&key(crossterm::event::KeyCode::Right), true);
        views.event(&key(crossterm::event::KeyCode::Enter), true);
        terminal
            .draw(|frame| views.render(frame, &mut ui, &editor).unwrap())
            .unwrap();
        assert_eq!(ui.app.last_full_rect.unwrap().x, content.x + 2);
        views.event(&key(crossterm::event::KeyCode::F(7)), true);
        views.event(&key(crossterm::event::KeyCode::Down), true);
        views.event(&key(crossterm::event::KeyCode::Enter), true);
        terminal
            .draw(|frame| views.render(frame, &mut ui, &editor).unwrap())
            .unwrap();
        assert_eq!(ui.app.last_full_rect.unwrap().height, content.height + 1);
        views.event(&key(crossterm::event::KeyCode::F(10)), true);
        terminal
            .draw(|frame| views.render(frame, &mut ui, &editor).unwrap())
            .unwrap();
        assert!(ui.app.last_full_rect.unwrap().width > content.width);
        views.event(&key(crossterm::event::KeyCode::F(10)), true);
        // Small viewports retain a keyboard escape route without out-of-bounds rendering.
        let mut tiny = Terminal::new(ratatui::backend::TestBackend::new(4, 3)).unwrap();
        tiny.draw(|frame| views.render(frame, &mut ui, &editor).unwrap())
            .unwrap();
        terminal
            .draw(|frame| views.render(frame, &mut ui, &editor).unwrap())
            .unwrap();
        assert!(matches!(
            views.event(&key(crossterm::event::KeyCode::Esc), false),
            views::Action::Pass
        ));
        assert!(matches!(
            views.event(&key(crossterm::event::KeyCode::Esc), true),
            views::Action::Close(_)
        ));
        // Closing a window must not depend on refreshing the retained main view.
        let original_reader = views
            .views
            .get_mut(&a_main)
            .unwrap()
            .handle
            .flow_view
            .replace(crate::instance_view::FlowViewSource::live(
                workers[1]
                    .subscribe_view(std::num::NonZeroUsize::new(16).unwrap())
                    .await
                    .unwrap(),
            ));
        assert!(
            views
                .focus_ready(&mut ui, &mut editor, &mut handle, a_main.clone(), None)
                .await
                .is_err()
        );
        assert_eq!(editor.buf(), "floating draft");
        views
            .close(&mut ui, &mut editor, &mut handle, &floating)
            .unwrap();
        assert_eq!(editor.buf(), "main draft");
        assert!(ui.app.flow_view_error.is_some());
        assert!(
            incoming.try_recv().is_err(),
            "view closure must not send execution controls"
        );
        handle.flow_view = original_reader;
        terminal
            .draw(|frame| views.render(frame, &mut ui, &editor).unwrap())
            .unwrap();
        assert_eq!(
            ui.app.last_full_rect,
            Some(ratatui::layout::Rect::new(30, 0, 130, 50))
        );
        assert_eq!(
            handle.control_tx.as_ref().unwrap().binding(),
            Some(&a_binding)
        );
        views
            .focus_ready(&mut ui, &mut editor, &mut handle, floating.clone(), None)
            .await
            .unwrap();
        assert_eq!(editor.buf(), "floating draft");
        views.promote();
        assert_eq!(
            handle.control_tx.as_ref().unwrap().binding(),
            Some(&floating_binding)
        );
        assert_eq!(views.views[&floating].editor.buf(), "main draft");
        terminal
            .draw(|frame| views.render(frame, &mut ui, &editor).unwrap())
            .unwrap();
        assert_eq!(
            ui.app.last_full_rect,
            Some(ratatui::layout::Rect::new(30, 0, 130, 50))
        );
        // Closing and promoting views must not send runtime controls.
        assert!(incoming.try_recv().is_err());
        views
            .focus_ready(&mut ui, &mut editor, &mut handle, hidden, None)
            .await
            .unwrap();
        assert_eq!(
            ui.wm.modals.alias_manager.aliases,
            [("shared".into(), "example".into())]
        );
        assert_eq!(editor.buf(), "draft B");
        assert_eq!(ui.app.session_name.as_deref(), Some("Renamed"));
        session
            .set_title(session.snapshot().await.unwrap().cursor, None)
            .await
            .unwrap();
        views::changed(handle.session_name_rx.as_mut()).await;
        refresh_session_name(&mut ui, &mut handle);
        assert!(ui.app.session_name.is_none());
        // Failed refreshes retain the destination's draft and original reply mailbox.
        let destination = views.views.get_mut(&a_main).unwrap();
        let original_live = destination
            .handle
            .live_output
            .replace(workers[1].subscribe_live_output());
        let draft = destination.editor.buf().to_owned();
        destination.ui.app.scroll_offset = 17;
        destination.ui.app.follow_tail = false;
        let sender = destination.handle.control_tx.as_ref().unwrap().clone();
        assert!(
            views
                .focus_ready(&mut ui, &mut editor, &mut handle, a_main.clone(), None)
                .await
                .is_err()
        );
        assert_eq!(editor.buf(), "draft B");
        let destination = views
            .views
            .get_mut(&a_main)
            .expect("failed target retained");
        assert_eq!(destination.editor.buf(), draft);
        assert_eq!(destination.ui.app.scroll_offset, 17);
        assert_eq!(
            destination.handle.control_tx.as_ref().unwrap().binding(),
            sender.binding()
        );
        sender.send(TuiControl::CancelFlow).unwrap();
        incoming
            .recv()
            .await
            .unwrap()
            .replies
            .send(TuiCommand::Toast {
                message: "original mailbox".into(),
                level: crate::app::NoteLevel::Info,
            })
            .unwrap();
        destination.handle.live_output = original_live;
        views
            .focus_ready(&mut ui, &mut editor, &mut handle, a_main, None)
            .await
            .unwrap();
        assert_eq!(editor.buf(), draft);
        let (_, command) = recv_cmd(
            &mut handle.cmd_rx,
            &mut handle.control_reply_rx,
            sender.binding(),
        )
        .await
        .unwrap();
        assert!(
            matches!(command, TuiCommand::Toast { message, .. } if message == "original mailbox")
        );
        host.shutdown().await.unwrap();
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                views::changed(handle.session_name_rx.as_mut())
            )
            .await
            .is_err(),
            "closed metadata subscriptions must not cause redraw loops"
        );
        for worker in workers {
            assert!(matches!(
                worker.state().await.unwrap().state(),
                atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Idle)
            ));
        }
    }

    use super::*;

    #[test]
    fn goal_card_click_preserves_controls_and_text_selection() {
        use ratatui::{Terminal, backend::TestBackend};
        for (width, height) in [(160, 45), (220, 60)] {
            let mut ui = UiState::new(AppState::new("goal-card".into(), None));
            let session = std::sync::Arc::new(atman_runtime::Session::open_ephemeral());
            session
                .goal_control
                .set(
                    &"Verify goal lifecycle and inspect the result. 验证目标状态与交互。".repeat(8),
                    None,
                )
                .unwrap();
            ui.app.session = Some(session);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            let editor = InputEditor::default();
            terminal
                .draw(|frame| crate::render::render_frame(frame, &mut ui, &editor))
                .unwrap();
            let card = ui.app.last_goal_rect.expect("visible goal card");
            let arrow = ui.app.last_goal_hdr_rect.unwrap();
            let control = ui.app.last_sidebar_strip_rects["goal-control"];
            assert_eq!(control.height, 3);
            assert!(card.contains((control.x, control.bottom() - 1).into()));
            let before = terminal.backend().buffer()[(card.x, card.y)].bg;
            ui.app.hovered_sidebar_row = Some("goal-card".into());
            terminal
                .draw(|frame| crate::render::render_frame(frame, &mut ui, &editor))
                .unwrap();
            assert_ne!(terminal.backend().buffer()[(card.x, card.y)].bg, before);
            let mouse = |kind, column, row| crossterm::event::MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::NONE,
            };
            for rect in [arrow, control] {
                for kind in [
                    MouseEventKind::Down(MouseButton::Left),
                    MouseEventKind::Up(MouseButton::Left),
                ] {
                    assert!(!handle_goal_card_mouse(
                        &mut ui,
                        &mouse(kind, rect.x, rect.y),
                        None
                    ));
                }
                assert!(
                    ui.wm.panels.is_empty(),
                    "nested controls must not open the panel"
                );
            }
            let x = card.x + 3;
            let y = card.y + 3;
            let down = mouse(MouseEventKind::Down(MouseButton::Left), x, y);
            assert!(!handle_goal_card_mouse(&mut ui, &down, None));
            handle_sidebar_selection_mouse(&mut ui.app, &down);
            let drag = mouse(MouseEventKind::Drag(MouseButton::Left), x + 4, y);
            assert!(!handle_goal_card_mouse(&mut ui, &drag, None));
            handle_sidebar_selection_mouse(&mut ui.app, &drag);
            assert!(
                ui.app
                    .selection
                    .as_ref()
                    .is_some_and(crate::selection::selection_is_non_empty)
            );
            assert!(!handle_goal_card_mouse(
                &mut ui,
                &mouse(MouseEventKind::Up(MouseButton::Left), x + 4, y),
                None
            ));
            assert!(ui.wm.panels.is_empty());
            // Padding and body text both use the card's actual rendered hitbox.
            for (column, row) in [(card.x + 1, card.y), (x, y)] {
                handle_goal_card_mouse(
                    &mut ui,
                    &mouse(MouseEventKind::Down(MouseButton::Left), column, row),
                    None,
                );
                assert!(handle_goal_card_mouse(
                    &mut ui,
                    &mouse(MouseEventKind::Up(MouseButton::Left), column, row),
                    None
                ));
                assert!(matches!(
                    ui.wm.panels[0].content_key,
                    crate::wm::ContentKey::Goal
                ));
                let id = ui.wm.panels[0].id;
                ui.wm.close(id);
            }
        }
    }

    #[tokio::test]
    async fn drag_events_coalesce_without_losing_mouse_up() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mouse = |kind, column| {
            Ok(CtEvent::Mouse(crossterm::event::MouseEvent {
                kind,
                column,
                row: 4,
                modifiers: KeyModifiers::NONE,
            }))
        };
        tx.send(mouse(MouseEventKind::Drag(MouseButton::Left), 3))
            .unwrap();
        tx.send(mouse(MouseEventKind::Drag(MouseButton::Left), 5))
            .unwrap();
        tx.send(mouse(MouseEventKind::Drag(MouseButton::Left), 8))
            .unwrap();
        tx.send(mouse(MouseEventKind::Up(MouseButton::Left), 8))
            .unwrap();
        let mut pending = None;
        let latest = recv_input_event(&mut rx, &mut pending)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(latest, CtEvent::Mouse(mouse) if mouse.column == 8 && matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)))
        );
        let release = recv_input_event(&mut rx, &mut pending)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(release, CtEvent::Mouse(mouse) if mouse.column == 8 && matches!(mouse.kind, MouseEventKind::Up(MouseButton::Left)))
        );
    }

    #[test]
    fn selection_scroll_only_ticks_outside_the_viewport() {
        assert_eq!(selection_scroll_step(0), 0);
        for distance in 1..30 {
            assert!(selection_scroll_step(distance) >= selection_scroll_step(distance - 1));
            assert!(selection_scroll_step(distance) <= SELECTION_SCROLL_MAX_STEP);
        }
        let mut app = AppState::new("session".into(), None);
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 30));
        app.last_total_rows = 100;
        app.last_document_visible_rows = 30;
        app.last_input_overlay_rows = 5;
        app.selection = Some(crate::selection::selection_begin(
            crate::selection::SemanticPoint {
                domain: crate::selection::SelectionDomain::TranscriptProse,
                ordinal: 0,
                grapheme: 0,
                affinity: crate::selection::Affinity::Before,
            },
            crate::app::OutputRevision::default(),
            0,
        ));
        app.transcript_selection_pointer = Some((4, 10));
        assert!(!transcript_selection_needs_tick(&app));
        app.transcript_selection_pointer = Some((4, 26));
        assert!(transcript_selection_needs_tick(&app));
        app.scroll_offset = app.max_scroll_offset();
        assert!(!transcript_selection_needs_tick(&app));
    }

    #[test]
    fn visual_points_match_direct_hits_for_wide_prose_and_code() {
        let items = app::FlowViewModel::from(vec![app::OutputItem::AssistantMd {
            md: "汉🙂 **bold**\n\n```rust\nlet name = \"汉🙂\";\n```".into(),
            streaming: false,
            retried: false,
        }]);
        let mut cache = crate::output::LayoutCache::default();
        let metrics = cache.update_dirty(
            crate::output::LayoutKey {
                width: 80,
                theme: crate::theme::ThemeMode::Dark,
            },
            &items,
            &crate::output::RenderCtx::empty(),
            crate::output::LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 40,
                follow_tail_rows: None,
            },
        );
        let projection = cache.visible_slice(metrics.scroll_offset, 40, 0).selection;
        let points = projection.visual_points();
        assert!(points.iter().any(|point| point.cell_width == 2));
        assert!(points.iter().any(|point| matches!(
            point.point.domain,
            crate::selection::SelectionDomain::MarkdownCode { .. }
        )));
        for visual in points {
            let direct = projection
                .prose_point_at(visual.row, visual.col)
                .or_else(|| projection.code_point_at(visual.row, visual.col))
                .or_else(|| projection.isolated_point_at(visual.row, visual.col));
            assert_eq!(Some(visual.point), direct);
        }
    }

    #[test]
    fn quote_close_uses_the_rendered_hitbox_and_card_blocks_selection() {
        let mut app = AppState::new("session".into(), None);
        app.pending_quote = Some("selected text".into());
        app.quote_rect = Some(ratatui::layout::Rect::new(10, 5, 20, 3));
        app.input_rect = Some(ratatui::layout::Rect::new(10, 18, 20, 5));
        let click = |column, row| crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };

        assert!(handle_quote_mouse(&mut app, &click(12, 6)));
        assert!(app.pending_quote.is_some());
        assert!(!handle_quote_mouse(&mut app, &click(31, 5)));
        assert!(app.pending_quote.is_some());
        assert!(handle_quote_mouse(&mut app, &click(27, 5)));
        assert!(app.pending_quote.is_none());

        app.pending_quote = Some("selected text".into());
        app.quote_rect = None;
        assert!(handle_quote_mouse(&mut app, &click(27, 18)));
        assert!(app.pending_quote.is_none());
    }

    #[test]
    fn pending_drag_scrolls_from_the_visible_edge_then_uses_the_new_projection() {
        let items = app::FlowViewModel::from(vec![app::OutputItem::AssistantMd {
            md: (0..80)
                .map(|line| format!("paragraph {line}"))
                .collect::<Vec<_>>()
                .join("\n\n"),
            streaming: false,
            retried: false,
        }]);
        let mut cache = crate::output::LayoutCache::default();
        let key = crate::output::LayoutKey {
            width: 80,
            theme: crate::theme::ThemeMode::Dark,
        };
        let metrics = cache.update_dirty(
            key,
            &items,
            &crate::output::RenderCtx::empty(),
            crate::output::LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 20,
                follow_tail_rows: None,
            },
        );
        let initial = cache.visible_slice(0, 20, 0);
        let surface = initial.selection.surfaces.first().expect("surface");
        let owner_revision = surface.revision;
        let anchor = initial
            .selection
            .visual_points()
            .into_iter()
            .find(|visual| {
                visual.point.domain == crate::selection::SelectionDomain::TranscriptProse
            })
            .expect("prose point")
            .point;
        let mut app = AppState::new("session".into(), None);
        app.items = items;
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 20));
        app.last_document_visible_rows = 20;
        app.last_input_overlay_rows = 7;
        app.last_total_rows = metrics.total_rows;
        app.last_selection_projection = initial.selection;

        let below = crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 5,
            row: 12,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert!(!handle_transcript_selection_mouse(&mut app, &below));
        assert!(app.selection.is_none());

        app.selection = Some(crate::selection::selection_begin(
            anchor,
            owner_revision,
            app.last_selection_projection.structure_revision,
        ));
        app.transcript_selection_pointer = Some((5, 12));
        tick_transcript_selection_drag(&mut app);
        assert!(app.scroll_offset > 0);
        assert_eq!(
            app.selection.as_ref().map(|state| state.phase),
            Some(crate::selection::SelectionPhase::Pending)
        );

        let next = cache.visible_slice(app.scroll_offset, 20, 0);
        let new_last = next
            .selection
            .visual_points()
            .into_iter()
            .filter(|visual| {
                visual.point.domain == crate::selection::SelectionDomain::TranscriptProse
                    && visual.row >= app.scroll_offset
                    && visual.row < app.scroll_offset + 11
            })
            .map(|visual| visual.point)
            .max_by(|left, right| left.position_cmp(right))
            .expect("new edge point");
        app.last_selection_projection = next.selection;
        resolve_transcript_selection_after_render(&mut app);
        let selection = app.selection.as_ref().expect("selection");
        assert_eq!(selection.phase, crate::selection::SelectionPhase::Active);
        assert_eq!(selection.focus.ordinal, new_last.ordinal);
        assert_eq!(selection.focus.grapheme, new_last.grapheme + 1);
        assert_eq!(selection.focus.affinity, crate::selection::Affinity::After);
    }

    fn startup_app() -> AppState {
        let mut app = AppState::new("current".into(), None);
        app.startup_card = Some(app::StartupCard {
            version: "1.0.0".into(),
            directory: app::StartupDirectory::Projects,
            recent: vec![app::StartupSessionEntry {
                target: app::StartupSessionTarget::Legacy("target".into()),
                short_id: "target00".into(),
                goal: Some("resume work".into()),
                project: Some("project".into()),
                activity: "1m ago  ·  7 events".into(),
            }],
        });
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
    fn transcript_mouse_selection_reaches_copy_handler() {
        let items = app::FlowViewModel::from(vec![app::OutputItem::AssistantMd {
            md: "ordinary assistant prose".into(),
            streaming: false,
            retried: false,
        }]);
        let mut cache = crate::output::LayoutCache::default();
        let metrics = cache.update_dirty(
            crate::output::LayoutKey {
                width: 80,
                theme: crate::theme::ThemeMode::Dark,
            },
            &items,
            &crate::output::RenderCtx::empty(),
            crate::output::LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 40,
                follow_tail_rows: None,
            },
        );
        let visible = cache.visible_slice(metrics.scroll_offset, metrics.total_rows, 0);
        let surface = visible.selection.surfaces.first().expect("surface");
        let atom = surface.prose_atoms.first().expect("prose atom");
        let start_col = atom.atom.cols.start;
        let row = atom.screen_row as u16;
        let mut app = AppState::new("session".into(), None);
        app.items = items;
        app.scroll_offset = 0;
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 40));
        app.last_selection_projection = visible.selection;

        let mouse = |kind, column| crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert!(!handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Down(MouseButton::Left), start_col),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Drag(MouseButton::Left), 79),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Up(MouseButton::Left), 79),
        ));
        assert_eq!(
            app.selection.as_ref().map(|state| state.phase),
            Some(crate::selection::SelectionPhase::Active)
        );
        assert_eq!(
            app.selection.as_ref().map(|state| state.focus.grapheme),
            Some("ordinary assistant prose".len())
        );
    }

    #[test]
    fn transcript_mouse_selection_preserves_markdown_for_rendered_blocks() {
        let cases = [
            (
                "- one\n- **two**\n- [ ] three\n",
                "- one\n- **two**\n- [ ] three",
            ),
            (
                "| A | B |\n|---|---|\n| 1 | 2 |\n",
                "| A | B |\n|---|---|\n| 1 | 2 |",
            ),
            ("```rust\nfn main() {}\n```\n", "```rust\nfn main() {}\n```"),
        ];
        for (markdown, expected) in cases {
            let items = app::FlowViewModel::from(vec![app::OutputItem::AssistantMd {
                md: markdown.into(),
                streaming: false,
                retried: false,
            }]);
            let mut cache = crate::output::LayoutCache::default();
            let metrics = cache.update_dirty(
                crate::output::LayoutKey {
                    width: 80,
                    theme: crate::theme::ThemeMode::Dark,
                },
                &items,
                &crate::output::RenderCtx::empty(),
                crate::output::LayoutRequest {
                    scroll_offset: 0,
                    viewport_rows: 40,
                    follow_tail_rows: None,
                },
            );
            let visible = cache.visible_slice(metrics.scroll_offset, metrics.total_rows, 0);
            let points = visible.selection.visual_points();
            let first = points.first().expect("rendered selection start");
            let last = points.last().expect("rendered selection end");
            let mut app = AppState::new("session".into(), None);
            app.items = items;
            app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 40));
            app.last_selection_projection = visible.selection;
            let mouse = |kind, point: &crate::selection::VisualSelectionPoint| {
                crossterm::event::MouseEvent {
                    kind,
                    column: point.col,
                    row: point.row as u16,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                }
            };
            assert!(!handle_transcript_selection_mouse(
                &mut app,
                &mouse(MouseEventKind::Down(MouseButton::Left), first),
            ));
            assert!(handle_transcript_selection_mouse(
                &mut app,
                &mouse(MouseEventKind::Drag(MouseButton::Left), last),
            ));
            assert!(handle_transcript_selection_mouse(
                &mut app,
                &mouse(MouseEventKind::Up(MouseButton::Left), last),
            ));
            let ui = UiState {
                app,
                ..UiState::new(AppState::new("unused".into(), None))
            };
            let payload = selection_action_payload(&ui).expect("copy payload");
            let copied = match payload {
                crate::selection::CopyPayload::Markdown(text)
                | crate::selection::CopyPayload::PlainText(text)
                | crate::selection::CopyPayload::Preview(text) => text,
            };
            assert_eq!(copied, expected, "markdown input: {markdown:?}");
        }
    }

    #[test]
    fn transcript_mouse_selection_copies_code_when_dragged_from_prose() {
        let markdown = "Before text\n\n```rust\nfn main() {}\n```\n\nAfter text\n";
        let items = app::FlowViewModel::from(vec![app::OutputItem::AssistantMd {
            md: markdown.into(),
            streaming: false,
            retried: false,
        }]);
        let mut cache = crate::output::LayoutCache::default();
        let metrics = cache.update_dirty(
            crate::output::LayoutKey {
                width: 80,
                theme: crate::theme::ThemeMode::Dark,
            },
            &items,
            &crate::output::RenderCtx::empty(),
            crate::output::LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 40,
                follow_tail_rows: None,
            },
        );
        let visible = cache.visible_slice(metrics.scroll_offset, metrics.total_rows, 0);
        let prose = visible
            .selection
            .visual_points()
            .into_iter()
            .find(|point| {
                matches!(
                    point.point.domain,
                    crate::selection::SelectionDomain::TranscriptProse
                )
            })
            .expect("prose point");
        let code = visible
            .selection
            .visual_points()
            .into_iter()
            .filter(|point| {
                matches!(
                    point.point.domain,
                    crate::selection::SelectionDomain::MarkdownCode { .. }
                )
            })
            .max_by_key(|point| (point.row, point.col))
            .expect("code point");
        let mut app = AppState::new("session".into(), None);
        app.items = items;
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 40));
        app.last_selection_projection = visible.selection;
        let mouse =
            |kind, point: &crate::selection::VisualSelectionPoint| crossterm::event::MouseEvent {
                kind,
                column: point.col,
                row: point.row as u16,
                modifiers: crossterm::event::KeyModifiers::NONE,
            };
        assert!(!handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Down(MouseButton::Left), &prose),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Drag(MouseButton::Left), &code),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Up(MouseButton::Left), &code),
        ));
        let ui = UiState {
            app,
            ..UiState::new(AppState::new("unused".into(), None))
        };
        let payload = selection_action_payload(&ui).expect("cross-block copy payload");
        let copied = match payload {
            crate::selection::CopyPayload::Markdown(text)
            | crate::selection::CopyPayload::PlainText(text)
            | crate::selection::CopyPayload::Preview(text) => text,
        };
        assert_eq!(copied, "Before text\n\n```rust\nfn main() {}\n```");
    }

    #[test]
    fn transcript_mouse_selection_keeps_middle_code_between_prose_endpoints() {
        let markdown = "Before text\n\n```rust\nfn main() {}\n```\n\nAfter text\n";
        let items = app::FlowViewModel::from(vec![app::OutputItem::AssistantMd {
            md: markdown.into(),
            streaming: false,
            retried: false,
        }]);
        let mut cache = crate::output::LayoutCache::default();
        let metrics = cache.update_dirty(
            crate::output::LayoutKey {
                width: 80,
                theme: crate::theme::ThemeMode::Dark,
            },
            &items,
            &crate::output::RenderCtx::empty(),
            crate::output::LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 40,
                follow_tail_rows: None,
            },
        );
        let visible = cache.visible_slice(metrics.scroll_offset, metrics.total_rows, 0);
        let points = visible.selection.visual_points();
        let prose_points = points
            .iter()
            .filter(|point| {
                matches!(
                    point.point.domain,
                    crate::selection::SelectionDomain::TranscriptProse
                )
            })
            .collect::<Vec<_>>();
        let first = prose_points.first().expect("leading prose point");
        let last = prose_points.last().expect("trailing prose point");
        let mut app = AppState::new("session".into(), None);
        app.items = items;
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 40));
        app.last_selection_projection = visible.selection;
        let mouse =
            |kind, point: &crate::selection::VisualSelectionPoint| crossterm::event::MouseEvent {
                kind,
                column: point.col,
                row: point.row as u16,
                modifiers: crossterm::event::KeyModifiers::NONE,
            };
        assert!(!handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Down(MouseButton::Left), first),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Drag(MouseButton::Left), last),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Up(MouseButton::Left), last),
        ));
        let ui = UiState {
            app,
            ..UiState::new(AppState::new("unused".into(), None))
        };
        let payload = selection_action_payload(&ui).expect("middle code payload");
        let copied = match payload {
            crate::selection::CopyPayload::Markdown(text)
            | crate::selection::CopyPayload::PlainText(text)
            | crate::selection::CopyPayload::Preview(text) => text,
        };
        assert_eq!(
            copied,
            "Before text\n\n```rust\nfn main() {}\n```\n\nAfter text"
        );
    }

    #[test]
    fn transcript_mouse_selection_keeps_code_when_prose_starts_mid_fragment() {
        let markdown = "Before text\n\n```rust\nfn main() {}\n```\n";
        let items = app::FlowViewModel::from(vec![app::OutputItem::AssistantMd {
            md: markdown.into(),
            streaming: false,
            retried: false,
        }]);
        let mut cache = crate::output::LayoutCache::default();
        let metrics = cache.update_dirty(
            crate::output::LayoutKey {
                width: 80,
                theme: crate::theme::ThemeMode::Dark,
            },
            &items,
            &crate::output::RenderCtx::empty(),
            crate::output::LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 40,
                follow_tail_rows: None,
            },
        );
        let visible = cache.visible_slice(metrics.scroll_offset, metrics.total_rows, 0);
        let points = visible.selection.visual_points();
        let prose = points
            .iter()
            .find(|point| {
                matches!(
                    point.point.domain,
                    crate::selection::SelectionDomain::TranscriptProse
                ) && point.point.grapheme > 2
            })
            .expect("mid-fragment prose point");
        let code = points
            .iter()
            .filter(|point| {
                matches!(
                    point.point.domain,
                    crate::selection::SelectionDomain::MarkdownCode { .. }
                )
            })
            .max_by_key(|point| (point.row, point.col))
            .expect("code point");
        let mut app = AppState::new("session".into(), None);
        app.items = items;
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 40));
        app.last_selection_projection = visible.selection;
        let mouse =
            |kind, point: &crate::selection::VisualSelectionPoint| crossterm::event::MouseEvent {
                kind,
                column: point.col,
                row: point.row as u16,
                modifiers: crossterm::event::KeyModifiers::NONE,
            };
        assert!(!handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Down(MouseButton::Left), prose),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Drag(MouseButton::Left), code),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Up(MouseButton::Left), code),
        ));
        let ui = UiState {
            app,
            ..UiState::new(AppState::new("unused".into(), None))
        };
        let payload = selection_action_payload(&ui).expect("mid-fragment cross-block payload");
        let copied = match payload {
            crate::selection::CopyPayload::Markdown(text)
            | crate::selection::CopyPayload::PlainText(text)
            | crate::selection::CopyPayload::Preview(text) => text,
        };
        assert!(copied.contains("```rust\nfn main() {}\n```"));
        assert!(!matches!(
            selection_action_payload(&ui),
            Some(crate::selection::CopyPayload::PlainText(_))
        ));
    }

    #[test]
    fn transcript_mouse_selection_copies_code_across_items() {
        let items = app::FlowViewModel::from(vec![
            app::OutputItem::AssistantMd {
                md: "Before text".into(),
                streaming: false,
                retried: false,
            },
            app::OutputItem::AssistantMd {
                md: "```rust\nfn main() {}\n```".into(),
                streaming: false,
                retried: false,
            },
        ]);
        let mut cache = crate::output::LayoutCache::default();
        let metrics = cache.update_dirty(
            crate::output::LayoutKey {
                width: 80,
                theme: crate::theme::ThemeMode::Dark,
            },
            &items,
            &crate::output::RenderCtx::empty(),
            crate::output::LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 40,
                follow_tail_rows: None,
            },
        );
        let visible = cache.visible_slice(metrics.scroll_offset, metrics.total_rows, 0);
        let points = visible.selection.visual_points();
        let prose = points
            .iter()
            .find(|point| {
                matches!(
                    point.point.domain,
                    crate::selection::SelectionDomain::TranscriptProse
                )
            })
            .expect("prose point");
        let code = points
            .iter()
            .filter(|point| {
                matches!(
                    point.point.domain,
                    crate::selection::SelectionDomain::MarkdownCode { .. }
                )
            })
            .max_by_key(|point| (point.row, point.col))
            .expect("code point");
        let mut app = AppState::new("session".into(), None);
        app.items = items;
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 40));
        app.last_selection_projection = visible.selection;
        let mouse =
            |kind, point: &crate::selection::VisualSelectionPoint| crossterm::event::MouseEvent {
                kind,
                column: point.col,
                row: point.row as u16,
                modifiers: crossterm::event::KeyModifiers::NONE,
            };
        assert!(!handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Down(MouseButton::Left), prose),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Drag(MouseButton::Left), code),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Up(MouseButton::Left), code),
        ));
        let ui = UiState {
            app,
            ..UiState::new(AppState::new("unused".into(), None))
        };
        let payload = selection_action_payload(&ui).expect("cross-item copy payload");
        let copied = match payload {
            crate::selection::CopyPayload::Markdown(text)
            | crate::selection::CopyPayload::PlainText(text)
            | crate::selection::CopyPayload::Preview(text) => text,
        };
        assert_eq!(copied, "Before text\n\n```rust\nfn main() {}\n```");
    }

    #[test]
    fn window_mouse_selection_reaches_copy_handler() {
        let revision = crate::app::OutputRevision {
            id: 42,
            layout: 7,
            ..Default::default()
        };
        let domain = crate::selection::SelectionDomain::Window {
            window_id: crate::wm::WindowId(3),
            surface: "mermaid-source".into(),
        };
        let source = "flowchart TD".to_string();
        let atoms = crate::selection::atom_runs(6, 12, &source, 0).0;
        let mut raw_graphemes = crate::selection::SourceGraphemeCursor::new(&source);
        let projection = crate::selection::VisibleSelectionProjection {
            structure_revision: 7,
            surfaces: vec![crate::selection::VisibleSurface {
                item_index: usize::MAX,
                revision,
                start_row: 6,
                end_row: 7,
                source: std::sync::Arc::new(crate::selection::ItemSemanticSource {
                    owner_revision: revision,
                    source: source.clone(),
                    isolated: vec![crate::selection::IsolatedSource {
                        domain: domain.clone(),
                        fragments: vec![crate::selection::CopyFragment::plain_text(source.clone())],
                    }],
                    ..Default::default()
                }),
                prose_atoms: Vec::new(),
                code_atoms: Vec::new(),
                isolated_atoms: atoms
                    .into_iter()
                    .map(|atom| crate::selection::VisibleIsolatedAtom::Raw {
                        domain: domain.clone(),
                        grapheme_start: raw_graphemes.at(atom.source.start),
                        atom: crate::selection::VisibleAtom {
                            screen_row: 6,
                            cols: atom.cols,
                            cell_width: atom.cell_width,
                            source: atom.source,
                        },
                    })
                    .collect(),
            }],
        };
        let (start_col, end_col) = match &projection.surfaces[0].isolated_atoms[0] {
            crate::selection::VisibleIsolatedAtom::Raw { atom, .. } => {
                (atom.cols.start, atom.cols.end.saturating_sub(1))
            }
            _ => unreachable!(),
        };
        let mut app = AppState::new("session".into(), None);
        app.last_window_selection_projection = projection;
        let mouse = |kind, column| crossterm::event::MouseEvent {
            kind,
            column,
            row: 6,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };

        assert!(handle_window_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Down(MouseButton::Left), start_col),
        ));
        assert!(handle_window_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Drag(MouseButton::Left), end_col),
        ));
        assert!(handle_window_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Up(MouseButton::Left), end_col),
        ));
        let state = app.selection.as_ref().expect("window selection");
        assert!(
            crate::selection::selection_copy_payload(&app.last_window_selection_projection, state)
                .is_some()
        );
    }

    #[test]
    fn bash_output_mouse_selection_reaches_copy_handler() {
        let items = app::FlowViewModel::from(vec![app::OutputItem::Bash {
            handle: "bash-1".into(),
            title: Some("command".into()),
            command: Some("printf output".into()),
            output: "tool output line".into(),
            done: true,
            expanded: true,
        }]);
        let mut cache = crate::output::LayoutCache::default();
        let metrics = cache.update_dirty(
            crate::output::LayoutKey {
                width: 80,
                theme: crate::theme::ThemeMode::Dark,
            },
            &items,
            &crate::output::RenderCtx::empty(),
            crate::output::LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 40,
                follow_tail_rows: None,
            },
        );
        let visible = cache.visible_slice(metrics.scroll_offset, metrics.total_rows, 0);
        let surface = visible.selection.surfaces.first().expect("bash surface");
        let atom = surface.isolated_atoms.first().expect("bash selection atom");
        let crate::selection::VisibleIsolatedAtom::Raw { atom, .. } = atom else {
            panic!("bash should use raw geometry");
        };
        let row = atom.screen_row as u16;
        let start_col = atom.cols.start;
        let end_col = atom.cols.end.saturating_sub(1);
        let mut app = AppState::new("session".into(), None);
        app.items = items;
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 40));
        app.last_selection_projection = visible.selection;
        let mouse = |kind, column| crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };

        assert!(!handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Down(MouseButton::Left), start_col),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Drag(MouseButton::Left), end_col),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Up(MouseButton::Left), end_col),
        ));
        let state = app.selection.as_ref().expect("bash selection");
        assert!(matches!(
            state.anchor.domain,
            crate::selection::SelectionDomain::RawOutput { .. }
        ));
        assert!(
            crate::selection::selection_copy_payload(&app.last_selection_projection, state)
                .is_some()
        );
    }

    #[test]
    fn thinking_mouse_selection_reaches_copy_handler() {
        let items = app::FlowViewModel::from(vec![app::OutputItem::Thinking {
            text: "reasoning details".into(),
            done: true,
            disclosure: app::Disclosure::Full,
            retried: false,
        }]);
        let mut cache = crate::output::LayoutCache::default();
        let metrics = cache.update_dirty(
            crate::output::LayoutKey {
                width: 80,
                theme: crate::theme::ThemeMode::Dark,
            },
            &items,
            &crate::output::RenderCtx::empty(),
            crate::output::LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 40,
                follow_tail_rows: None,
            },
        );
        let visible = cache.visible_slice(metrics.scroll_offset, metrics.total_rows, 0);
        let surface = visible
            .selection
            .surfaces
            .first()
            .expect("thinking surface");
        let atom = surface
            .isolated_atoms
            .first()
            .expect("thinking selection atom");
        let (row, start_col, end_col) = match atom {
            crate::selection::VisibleIsolatedAtom::Markdown {
                atom, screen_row, ..
            } => (
                *screen_row as u16,
                atom.cols.start,
                atom.cols.end.saturating_sub(1),
            ),
            crate::selection::VisibleIsolatedAtom::Raw { .. } => {
                panic!("thinking should use markdown geometry")
            }
        };
        let mut app = AppState::new("session".into(), None);
        app.items = items;
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 40));
        app.last_selection_projection = visible.selection;
        let mouse = |kind, column| crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };

        assert!(!handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Down(MouseButton::Left), start_col),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Drag(MouseButton::Left), end_col),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Up(MouseButton::Left), end_col),
        ));
        let state = app.selection.as_ref().expect("thinking selection");
        assert_eq!(state.phase, crate::selection::SelectionPhase::Active);
        assert!(matches!(
            state.anchor.domain,
            crate::selection::SelectionDomain::Thinking { .. }
        ));
        assert!(
            crate::selection::selection_copy_payload(&app.last_selection_projection, state)
                .is_some()
        );
        assert!(matches!(
            app.items.first(),
            Some(app::OutputItem::Thinking {
                disclosure: app::Disclosure::Full,
                ..
            })
        ));
    }

    #[test]
    fn collapsed_thinking_click_expands_without_selection_atoms() {
        let items = app::FlowViewModel::from(vec![app::OutputItem::Thinking {
            text: "reasoning details".into(),
            done: true,
            disclosure: app::Disclosure::Summary,
            retried: false,
        }]);
        let mut cache = crate::output::LayoutCache::default();
        let metrics = cache.update_dirty(
            crate::output::LayoutKey {
                width: 80,
                theme: crate::theme::ThemeMode::Dark,
            },
            &items,
            &crate::output::RenderCtx::empty(),
            crate::output::LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 40,
                follow_tail_rows: None,
            },
        );
        let visible = cache.visible_slice(metrics.scroll_offset, metrics.total_rows, 0);
        assert!(visible.selection.surfaces[0].isolated_atoms.is_empty());
        let row = visible.ranges[0].start_row as u16 + 1;
        let mut app = AppState::new("session".into(), None);
        app.items = items;
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 40));
        app.last_item_ranges = visible.ranges;
        app.last_selection_projection = visible.selection;
        let mouse = |kind| crossterm::event::MouseEvent {
            kind,
            column: 4,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };

        assert!(!handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Down(MouseButton::Left)),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Up(MouseButton::Left)),
        ));
        assert!(matches!(
            app.items.first(),
            Some(app::OutputItem::Thinking {
                disclosure: app::Disclosure::Full,
                ..
            })
        ));
        assert!(app.selection.is_none());
    }

    #[test]
    fn user_turn_mouse_selection_reaches_copy_handler() {
        let items = app::FlowViewModel::from(vec![app::OutputItem::UserTurn {
            text: "你好呀".into(),
            presentation: None,
        }]);
        let mut cache = crate::output::LayoutCache::default();
        let metrics = cache.update_dirty(
            crate::output::LayoutKey {
                width: 80,
                theme: crate::theme::ThemeMode::Dark,
            },
            &items,
            &crate::output::RenderCtx::empty(),
            crate::output::LayoutRequest {
                scroll_offset: 0,
                viewport_rows: 40,
                follow_tail_rows: None,
            },
        );
        let visible = cache.visible_slice(metrics.scroll_offset, metrics.total_rows, 0);
        let surface = visible.selection.surfaces.first().expect("user surface");
        let atom = surface.prose_atoms.first().expect("user prose atom");
        assert_eq!(atom.screen_row, 1);
        let row = atom.screen_row as u16;
        let start_col = atom.atom.cols.start;
        let end_col = atom.atom.cols.end.saturating_sub(1);
        let mut app = AppState::new("session".into(), None);
        app.items = items;
        app.last_transcript_rect = Some(ratatui::layout::Rect::new(0, 0, 80, 40));
        app.last_selection_projection = visible.selection;
        let mouse = |kind, column| crossterm::event::MouseEvent {
            kind,
            column,
            row,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        assert!(!handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Down(MouseButton::Left), start_col),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Drag(MouseButton::Left), end_col),
        ));
        assert!(handle_transcript_selection_mouse(
            &mut app,
            &mouse(MouseEventKind::Up(MouseButton::Left), end_col),
        ));
        assert_eq!(
            app.selection.as_ref().map(|state| state.phase),
            Some(crate::selection::SelectionPhase::Active)
        );
    }

    #[test]
    fn startup_session_four_rows_single_click_selects_and_double_click_switches() {
        for row in 20..24 {
            let mut app = startup_app();
            let (tx, mut rx) = crate::ControlSender::channel();
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
            directory: app::StartupDirectory::Projects,
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

    #[test]
    fn startup_flow_mouse_navigation_uses_the_shared_directory() {
        let mut app = startup_app();
        let target = atman_runtime::flow_store::FlowRef {
            session_id: uuid::Uuid::now_v7().into(),
            instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
        };
        let card = app.startup_card.as_mut().unwrap();
        card.directory = app::StartupDirectory::Sessions;
        card.recent[0].target = app::StartupSessionTarget::Flow(target.clone());
        let click = startup_mouse(MouseEventKind::Down(MouseButton::Left), 21);
        assert!(handle_startup_session_mouse(&mut app, &click, None));
        assert!(app.open_flow_hub.is_none());
        assert!(handle_startup_session_mouse(&mut app, &click, None));
        assert!(
            matches!(app.open_flow_hub.take(), Some(crate::flow_hub::Action::Focus(flow)) if flow.instance() == Some(&target))
        );
        assert!(!app.should_quit);
        app.startup_projects_rect = Some(ratatui::layout::Rect::new(18, 25, 40, 4));
        let click = startup_mouse(MouseEventKind::Down(MouseButton::Left), 26);
        assert!(handle_startup_session_mouse(&mut app, &click, None));
        assert!(matches!(
            app.open_flow_hub,
            Some(crate::flow_hub::Action::Load(
                crate::flow_hub::HubRequest::Sessions { before: None }
            ))
        ));
        assert!(!app.startup_open_projects);
    }

    #[test]
    fn startup_all_projects_card_opens_project_hub() {
        let mut app = startup_app();
        app.startup_projects_rect = Some(ratatui::layout::Rect::new(18, 19, 20, 4));
        let click = startup_mouse(MouseEventKind::Down(MouseButton::Left), 21);

        assert!(handle_startup_session_mouse(&mut app, &click, None));
        assert!(app.startup_projects_hovered);
        assert!(app.startup_open_projects);
        assert!(!app.should_quit);
    }

    #[tokio::test]
    async fn saturated_bulk_lane_does_not_block_semantic_frames() {
        let (semantic_tx, mut semantic_rx) = tokio::sync::mpsc::channel(1);
        let (bulk_tx, _bulk_rx) = tokio::sync::mpsc::channel(1);
        let bulk = StreamFrame::BashChunk {
            start: Default::default(),
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
        let (tx, mut rx) = crate::ControlSender::channel();
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
            api: None,
            max_tools: None,
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

    #[tokio::test]
    async fn config_create_advances_onboarding_and_opens_model_manager_after_success() {
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
            crate::onboarding::OnboardingStep::ProviderSelect
        );
        app.app.settings.ready().await;
        apply_provider_catalog_changed(&mut app, None);
        assert_eq!(
            app.wm.modals.onboarding.step,
            crate::onboarding::OnboardingStep::ModelSelect
        );
        assert!(app.wm.modals.model_manager.open);
        assert!(app.app.toasts.last().unwrap().message.contains("added"));
    }

    #[test]
    fn hidden_provider_completion_keeps_managers_hidden() {
        let mut app = UiState::new(AppState::new("session".into(), None));
        let provider = "hidden-config-ack-empty";
        let request = pending_provider_request(&mut app, config_upsert(provider, true));
        app.wm.modals.provider_manager.close();
        apply_provider_mutation_result(
            &mut app,
            request,
            Ok(crate::ProviderMutationSuccess::ConfigSaved {
                name: provider.into(),
                created: true,
            }),
        );
        assert!(!app.wm.modals.provider_manager.open);
        assert!(!app.wm.modals.model_manager.open);
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

    #[tokio::test]
    async fn only_provider_add_catalog_changes_advance_onboarding() {
        let mut app = UiState::new(AppState::new("session".into(), None));
        app.wm.modals.onboarding_open = true;

        apply_provider_catalog_changed(&mut app, None);
        assert_eq!(
            app.wm.modals.onboarding.step,
            crate::onboarding::OnboardingStep::ProviderSelect
        );

        apply_provider_catalog_changed(&mut app, Some("Provider".into()));
        app.app.settings.ready().await;
        apply_provider_catalog_changed(&mut app, None);
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
        assert_eq!(app.app.items.len(), 1);
        assert!(matches!(
            app.app.items.first(),
            Some(app::OutputItem::SystemNote { text, level })
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
