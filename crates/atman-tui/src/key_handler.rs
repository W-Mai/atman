use crate::UiState;
use tokio::sync::mpsc;

use super::TuiControl;
use crate::app::AppState;
use crate::input::InputEditor;
use crate::keys::KeyAction;
use crate::{app, layout};

pub(crate) fn yank_candidate_indices(app: &AppState) -> Vec<usize> {
    app.items
        .iter()
        .enumerate()
        .filter_map(|(i, it)| match it {
            app::OutputItem::AssistantMd { .. } | app::OutputItem::UserTurn { .. } => Some(i),
            _ => None,
        })
        .collect()
}

pub(crate) fn emit_yank_selection_note(app: &mut AppState, cands: &[usize]) {
    let total = cands.len();
    let cur = app.yank_index.min(total.saturating_sub(1)) + 1;
    let kind = cands
        .get(app.yank_index)
        .and_then(|i| app.items.get(*i))
        .map(|it| match it {
            app::OutputItem::AssistantMd { .. } => "assistant",
            app::OutputItem::UserTurn { .. } => "user",
            _ => "other",
        })
        .unwrap_or("?");
    app.push_note(format!("yank {cur}/{total} — {kind}"), app::NoteLevel::Info);
}

pub(crate) fn yank_selected_text(app: &AppState) -> Option<String> {
    let cands = yank_candidate_indices(app);
    let item_idx = *cands.get(app.yank_index)?;
    match app.items.get(item_idx)? {
        app::OutputItem::AssistantMd { md, .. } => Some(md.clone()),
        app::OutputItem::UserTurn { text } => Some(text.clone()),
        _ => None,
    }
}

pub(crate) fn copy_last_message(app: &mut AppState) {
    let text = app.items.iter().rev().find_map(|item| match item {
        app::OutputItem::AssistantMd { md, .. } => Some(md.clone()),
        _ => None,
    });
    match text {
        Some(t) if !t.is_empty() => {
            let n = t.chars().count();
            crate::clipboard::write_osc52(&t);
            app.push_note(
                format!("copied {n} chars from last message"),
                app::NoteLevel::Info,
            );
        }
        _ => app.push_note("no assistant message to copy", app::NoteLevel::Warn),
    }
}

pub(crate) fn copy_last_tool(app: &mut AppState) {
    let text = app.items.iter().rev().find_map(|item| match item {
        app::OutputItem::Bash { output, .. } if !output.trim().is_empty() => Some(output.clone()),
        _ => None,
    });
    match text {
        Some(t) => {
            let n = t.chars().count();
            crate::clipboard::write_osc52(&t);
            app.push_note(
                format!("copied {n} chars from last tool output"),
                app::NoteLevel::Info,
            );
        }
        _ => app.push_note("no tool output to copy", app::NoteLevel::Warn),
    }
}

pub(crate) fn enumerate_session_rows(
    app: &AppState,
    scope: crate::session_switcher::SessionScope,
) -> Vec<crate::SessionPickerRow> {
    let Some(session) = &app.session else {
        return Vec::new();
    };
    let session_dir = session.dir();
    let Some(sessions_root) = session_dir.parent() else {
        return Vec::new();
    };
    let current_meta = session.meta();
    let query = match scope {
        crate::session_switcher::SessionScope::All => {
            atman_runtime::session_meta::SessionDiscoveryQuery::all_projects()
        }
        crate::session_switcher::SessionScope::Project => {
            let Some(project_root) = current_meta
                .as_ref()
                .and_then(|meta| meta.project_root.as_deref())
            else {
                return Vec::new();
            };
            atman_runtime::session_meta::SessionDiscoveryQuery::current_project(project_root)
        }
    };
    let mut rows = Vec::new();
    let Ok(entries) = std::fs::read_dir(sessions_root) else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        if !entry.path().is_dir() {
            continue;
        }
        let sid = entry.file_name().to_string_lossy().to_string();
        let is_current = sid == session.id().to_string();
        let meta = atman_runtime::session_meta::SessionMeta::load(&entry.path());
        if !query.matches_meta(meta.as_ref()) {
            continue;
        }
        let Some(metadata) = meta.as_ref() else {
            continue;
        };
        if metadata.project_root.is_none() || metadata.project_fingerprint.is_none() {
            continue;
        }
        let project = metadata
            .project_root
            .as_ref()
            .map(|p| p.display().to_string());
        let events_path = entry.path().join("events.jsonl");
        let updated_at = std::fs::metadata(&events_path)
            .and_then(|m| m.modified())
            .or_else(|_| entry.metadata().and_then(|m| m.modified()))
            .ok()
            .map(|st| {
                let ts: chrono::DateTime<chrono::Local> = st.into();
                ts.to_rfc3339()
            })
            .unwrap_or_default();
        let (user_count, total_count) = count_message_events(&events_path);
        if user_count == 0 {
            continue;
        }
        let goal = atman_runtime::memory::goal::GoalStore::at(entry.path())
            .get()
            .ok();
        rows.push(crate::SessionPickerRow {
            id: sid,
            is_current,
            name: metadata.title.clone(),
            project,
            message_count: total_count,
            updated_at,
            goal,
        });
    }
    rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    rows.truncate(200);
    rows
}

pub(crate) fn count_message_events(path: &std::path::Path) -> (usize, usize) {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return (0, 0),
    };
    let mut user = 0usize;
    let mut total = 0usize;
    for l in contents.lines() {
        let is_user = l.contains("\"type\":\"user_msg\"");
        let is_assistant = l.contains("\"type\":\"assistant_msg\"");
        let is_tool = l.contains("\"type\":\"tool_result_msg\"");
        if is_user {
            user += 1;
        }
        if is_user || is_assistant || is_tool {
            total += 1;
        }
    }
    (user, total)
}

pub(crate) fn handle_yank_key(action: &KeyAction, app: &mut AppState) -> bool {
    let cands = yank_candidate_indices(app);
    if cands.is_empty() {
        app.yank_mode = false;
        return true;
    }
    match action {
        KeyAction::Escape => {
            app.yank_mode = false;
            app.push_note("yank cancelled", app::NoteLevel::Info);
            true
        }
        KeyAction::Char('y') | KeyAction::Char('Y') => {
            app.yank_mode = false;
            true
        }
        KeyAction::Char('j') | KeyAction::HistoryDown | KeyAction::CursorRight => {
            app.yank_index = (app.yank_index + 1).min(cands.len().saturating_sub(1));
            emit_yank_selection_note(app, &cands);
            true
        }
        KeyAction::Char('k') | KeyAction::HistoryUp | KeyAction::CursorLeft => {
            app.yank_index = app.yank_index.saturating_sub(1);
            emit_yank_selection_note(app, &cands);
            true
        }
        KeyAction::Char('g') => {
            app.yank_index = 0;
            emit_yank_selection_note(app, &cands);
            true
        }
        KeyAction::Char('G') => {
            app.yank_index = cands.len().saturating_sub(1);
            emit_yank_selection_note(app, &cands);
            true
        }
        KeyAction::Submit => {
            if let Some(text) = yank_selected_text(app) {
                let n = text.chars().count();
                crate::clipboard::write_osc52(&text);
                app.push_note(
                    format!("yanked {n} chars to clipboard (OSC 52)"),
                    app::NoteLevel::Info,
                );
            } else {
                app.push_note("yank: nothing selected", app::NoteLevel::Warn);
            }
            app.yank_mode = false;
            true
        }
        _ => true,
    }
}

pub(crate) fn is_approval_key(action: &KeyAction) -> bool {
    matches!(
        action,
        KeyAction::Char('1'..='9')
            | KeyAction::Char('a')
            | KeyAction::Char('A')
            | KeyAction::Char('d')
            | KeyAction::Char('D')
            | KeyAction::Escape
    )
}

pub(crate) fn handle_approval_key(
    action: &KeyAction,
    app: &mut AppState,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) -> bool {
    let Some(tx) = control_tx else {
        return false;
    };
    let queue = &app.pending_approvals;
    let deny_armed = app
        .deny_arm
        .map(|t| t.elapsed() < std::time::Duration::from_millis(2000))
        .unwrap_or(false);
    if !deny_armed {
        app.deny_arm = None;
    }
    match action {
        KeyAction::Char(c) => match c {
            '1'..='9' if deny_armed => {
                let idx = (*c as u8 - b'1') as usize;
                if let Some(p) = queue.get(idx) {
                    let _ = tx.send(TuiControl::DenyTool {
                        tool_use_id: p.tool_use_id.clone(),
                        reason: "denied by user".into(),
                    });
                    app.push_note(format!("denied {}", p.tool_name), app::NoteLevel::Warn);
                }
                app.deny_arm = None;
                true
            }
            '1'..='9' => {
                let idx = (*c as u8 - b'1') as usize;
                if let Some(p) = queue.get(idx) {
                    let _ = tx.send(TuiControl::ApproveTool(p.tool_use_id.clone()));
                    app.push_note(
                        format!("approved {} ({})", p.tool_name, p.tool_use_id),
                        app::NoteLevel::Info,
                    );
                }
                true
            }
            'a' | 'A' => {
                let _ = tx.send(TuiControl::ApproveAllPending);
                app.push_note(
                    format!("approved all {} pending", queue.len()),
                    app::NoteLevel::Info,
                );
                app.deny_arm = None;
                true
            }
            'd' | 'D' => {
                let deny_first = queue.len() <= 1 || deny_armed;
                if deny_first {
                    if let Some(p) = queue.first() {
                        let _ = tx.send(TuiControl::DenyTool {
                            tool_use_id: p.tool_use_id.clone(),
                            reason: "denied by user".into(),
                        });
                        app.push_note(format!("denied {}", p.tool_name), app::NoteLevel::Warn);
                    }
                    app.deny_arm = None;
                } else {
                    app.deny_arm = Some(std::time::Instant::now());
                    app.push_note(
                        format!("d + N to deny nth, dd to deny first (of {})", queue.len()),
                        app::NoteLevel::Info,
                    );
                }
                true
            }
            _ => {
                app.deny_arm = None;
                false
            }
        },
        KeyAction::Escape => {
            let _ = tx.send(TuiControl::DenyAllPending {
                reason: "user pressed Esc".into(),
            });
            let _ = tx.send(TuiControl::CancelFlow);
            app.push_note(
                format!("denied all {} pending, flow cancelled", queue.len()),
                app::NoteLevel::Warn,
            );
            app.deny_arm = None;
            app.cancel_running_activities();
            true
        }
        _ => false,
    }
}

pub(crate) fn handle_key(
    action: KeyAction,
    app: &mut UiState,
    editor: &mut InputEditor,
    interrupt_prompt: &mut Option<std::time::Instant>,
    submit_tx: Option<&mpsc::UnboundedSender<String>>,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) {
    // MCP add form intercepts all keys when open
    if app.mcp_add_form.is_some() {
        let mut form = app.mcp_add_form.take().unwrap();
        let mut close = false;
        let mut reload = false;
        let mut toast: Option<(String, app::NoteLevel)> = None;

        match action {
            KeyAction::Escape => {
                close = true;
            }
            KeyAction::Tab => {
                form.next_field();
            }
            KeyAction::BackTab => {
                form.prev_field();
            }
            KeyAction::Submit => match form.build_config() {
                Ok(cfg) => {
                    match atman_runtime::config_hub::ConfigHub::global()
                        .and_then(|hub| hub.upsert_mcp(cfg))
                    {
                        Ok(()) => {
                            reload = true;
                            close = true;
                            toast = Some(("MCP server added".into(), app::NoteLevel::Success));
                        }
                        Err(e) => form.error = Some(format!("save failed: {e}")),
                    }
                }
                Err(e) => form.error = Some(e),
            },
            KeyAction::Char(_)
            | KeyAction::Backspace
            | KeyAction::Delete
            | KeyAction::DeleteWordBackward
            | KeyAction::CursorHome
            | KeyAction::CursorEnd
            | KeyAction::Newline => {
                form.error = None;
                let editor = match form.field {
                    0 => Some(&mut form.name),
                    2 if form.transport_idx == 0 => Some(&mut form.command),
                    2 => Some(&mut form.url),
                    3 => Some(&mut form.args),
                    4 => Some(&mut form.env),
                    _ => None,
                };
                if let Some(editor) = editor {
                    editor.handle_key(&action);
                }
            }
            KeyAction::CursorLeft => {
                form.error = None;
                match form.field {
                    1 if form.transport_idx > 0 => form.transport_idx -= 1,
                    5 if form.tier_idx > 0 => form.tier_idx -= 1,
                    0 => form.name.move_left(),
                    2 if form.transport_idx == 0 => form.command.move_left(),
                    2 => form.url.move_left(),
                    3 => form.args.move_left(),
                    4 => form.env.move_left(),
                    _ => {}
                }
            }
            KeyAction::CursorRight => {
                form.error = None;
                match form.field {
                    1 if form.transport_idx < 2 => form.transport_idx += 1,
                    5 if form.tier_idx < 2 => form.tier_idx += 1,
                    0 => form.name.move_right(),
                    2 if form.transport_idx == 0 => form.command.move_right(),
                    2 => form.url.move_right(),
                    3 => form.args.move_right(),
                    4 => form.env.move_right(),
                    _ => {}
                }
            }
            _ => {}
        }

        if close {
            app.mcp_add_form = None;
        } else {
            app.mcp_add_form = Some(form);
        }
        if reload {
            if let Some(tx) = control_tx {
                let _ = tx.send(TuiControl::McpReload);
            }
        }
        if let Some((msg, level)) = toast {
            app.push_toast(
                msg,
                level,
                std::time::Duration::from_secs(3),
                app::ToastPosition::TopRight,
            );
        }
        return;
    }

    match action {
        crate::keys::KeyAction::CyclePanelForward => {
            app.wm.cycle_focus(true);
            return;
        }
        crate::keys::KeyAction::CyclePanelBackward => {
            app.wm.cycle_focus(false);
            return;
        }
        _ => {}
    }

    if let KeyAction::Tab = action {
        if let Some(id) = app.wm.focused_id()
            && let Some(panel) = app.wm.panels.iter_mut().find(|p| p.id == id)
            && matches!(panel.content_kind, crate::wm::WindowContent::Mermaid { .. })
        {
            panel.split = !panel.split;
            app.mark_items_dirty();
            return;
        }
    }

    if let Some(id) = app.wm.focused_id()
        && app
            .wm
            .panels
            .iter()
            .any(|p| p.id == id && matches!(p.content_kind, crate::wm::WindowContent::Mcp))
    {
        let server_count = app.context.mcp_servers.len();
        match action {
            KeyAction::Tab => {
                app.mcp_browser_tab = app.mcp_browser_tab.next();
                if let Some(s) = app.context.mcp_servers.get(app.mcp_selected) {
                    let name = s.name.clone();
                    match app.mcp_browser_tab {
                        crate::mcp_manager::McpBrowserTab::Resources
                            if !app.mcp_resources_cache.contains_key(&name) =>
                        {
                            if let Some(tx) = control_tx {
                                let _ = tx.send(TuiControl::McpListResources { name });
                                app.push_toast(
                                    "loading resources…".to_string(),
                                    app::NoteLevel::Info,
                                    std::time::Duration::from_secs(3),
                                    app::ToastPosition::TopRight,
                                );
                            }
                        }
                        crate::mcp_manager::McpBrowserTab::Prompts
                            if !app.mcp_prompts_cache.contains_key(&name) =>
                        {
                            if let Some(tx) = control_tx {
                                let _ = tx.send(TuiControl::McpListPrompts { name });
                                app.push_toast(
                                    "loading prompts…".to_string(),
                                    app::NoteLevel::Info,
                                    std::time::Duration::from_secs(3),
                                    app::ToastPosition::TopRight,
                                );
                            }
                        }
                        _ => {}
                    }
                }
                return;
            }
            KeyAction::HistoryUp | KeyAction::Char('k') => {
                app.mcp_remove_armed = None;
                if app.mcp_selected > 0 {
                    app.mcp_selected -= 1;
                }
                return;
            }
            KeyAction::HistoryDown | KeyAction::Char('j') => {
                app.mcp_remove_armed = None;
                if app.mcp_selected + 1 < server_count {
                    app.mcp_selected += 1;
                }
                return;
            }
            KeyAction::Submit => {
                app.mcp_remove_armed = None;
                if let Some(s) = app.context.mcp_servers.get(app.mcp_selected) {
                    let name = s.name.clone();
                    if !app.expanded_mcp_servers.remove(&name) {
                        app.expanded_mcp_servers.insert(name);
                    }
                }
                return;
            }
            KeyAction::Char('d') => {
                if let Some(s) = app.context.mcp_servers.get(app.mcp_selected) {
                    let name = s.name.clone();
                    match atman_runtime::config_hub::ConfigHub::global()
                        .and_then(|hub| hub.toggle_mcp(&name))
                    {
                        Ok(disabled) => {
                            let msg = if disabled {
                                format!("disabled {name} — reloading…")
                            } else {
                                format!("enabled {name} — reloading…")
                            };
                            app.push_toast(
                                msg,
                                app::NoteLevel::Info,
                                std::time::Duration::from_secs(3),
                                app::ToastPosition::TopRight,
                            );
                            if let Some(tx) = control_tx {
                                let _ = tx.send(TuiControl::McpReload);
                            }
                        }
                        Err(e) => {
                            app.push_toast(
                                format!("toggle failed: {e}"),
                                app::NoteLevel::Error,
                                std::time::Duration::from_secs(5),
                                app::ToastPosition::TopRight,
                            );
                        }
                    }
                }
                return;
            }
            KeyAction::Char('r') => {
                if let Some(s) = app.context.mcp_servers.get(app.mcp_selected) {
                    let name = s.name.clone();
                    app.mcp_remove_armed = Some(name.clone());
                    app.modal_notification = Some(format!(
                        "Remove MCP server \"{name}\"?\n\n  Enter = confirm   Esc = cancel"
                    ));
                }
                return;
            }
            KeyAction::Char('a') => {
                app.mcp_add_form = Some(crate::mcp_manager::McpAddForm::default());
                return;
            }
            KeyAction::Char('t') => {
                if let Some(s) = app.context.mcp_servers.get(app.mcp_selected) {
                    let name = s.name.clone();
                    if let Some(tx) = control_tx {
                        let _ = tx.send(TuiControl::McpTest { name: name.clone() });
                        app.push_toast(
                            format!("testing {name}…"),
                            app::NoteLevel::Info,
                            std::time::Duration::from_secs(10),
                            app::ToastPosition::TopRight,
                        );
                    } else {
                        app.push_toast(
                            "test not available (no control channel)",
                            app::NoteLevel::Warn,
                            std::time::Duration::from_secs(3),
                            app::ToastPosition::TopRight,
                        );
                    }
                }
                return;
            }
            KeyAction::Char('q') => {
                app.wm.close(id);
                return;
            }
            _ => {}
        }
    }
    if app.modal_notification.is_some() {
        if app.mcp_remove_armed.is_some() {
            match action {
                KeyAction::Submit => {
                    let name = app.mcp_remove_armed.take().unwrap();
                    app.modal_notification = None;
                    match atman_runtime::config_hub::ConfigHub::global()
                        .and_then(|hub| hub.remove_mcp(&name))
                    {
                        Ok(()) => {
                            app.push_toast(
                                format!("removed {name} — reloading…"),
                                app::NoteLevel::Info,
                                std::time::Duration::from_secs(3),
                                app::ToastPosition::TopRight,
                            );
                            if let Some(tx) = control_tx {
                                let _ = tx.send(TuiControl::McpReload);
                            }
                        }
                        Err(e) => {
                            app.push_toast(
                                format!("remove failed: {e}"),
                                app::NoteLevel::Error,
                                std::time::Duration::from_secs(5),
                                app::ToastPosition::TopRight,
                            );
                        }
                    }
                    return;
                }
                KeyAction::Escape => {
                    app.mcp_remove_armed = None;
                    app.modal_notification = None;
                    return;
                }
                _ => return,
            }
        }
        if matches!(action, KeyAction::Escape) {
            app.modal_notification = None;
        }
        return;
    }
    app.wm.sync_modals();
    if let KeyAction::OpenCommandPalette = action {
        app.wm.modals.palette.open();
        return;
    }
    if matches!(action, KeyAction::Char('x'))
        && matches!(
            app.items.first(),
            Some(crate::app::OutputItem::StartupCard { .. })
        )
        && !app.hints_dismissed
    {
        app.hints_dismissed = true;
        app.save_ui_state();
        return;
    }
    if let Some(crate::app::OutputItem::StartupCard { recent, .. }) = app.items.first() {
        // The overlay only animates away when the user actually starts
        // a session:
        //   * a digit 1-9 → resume that recent session
        //   * Enter (Submit) with input in the editor → begin a new
        //     session interaction
        // Plain char keys just type into the editor and the overlay
        // stays put with the growing text visible in its input slot.
        if editor.buf().is_empty()
            && let KeyAction::Char(c) = &action
            && let Some(digit) = c.to_digit(10)
            && (1..=9).contains(&digit)
        {
            let idx = (digit as usize) - 1;
            if let Some(entry) = recent.get(idx) {
                let session_id = entry.session_id.clone();
                request_session_switch(app, control_tx, session_id);
                return;
            }
        }
        if matches!(action, KeyAction::Submit)
            && !editor.buf().trim().is_empty()
            && app.startup_intro.is_none()
        {
            let (version, recent) = match app.items.first() {
                Some(crate::app::OutputItem::StartupCard { version, recent }) => {
                    (version.clone(), recent.clone())
                }
                _ => (String::new(), Vec::new()),
            };
            app.items.remove(0);
            app.items_version = app.items_version.wrapping_add(1);
            app.startup_intro = Some(crate::app::StartupIntro {
                started_at: std::time::Instant::now(),
                version,
                recent,
            });
        }
    }
    if app.wm.modals.trust_mode_picker_open {
        let modes = atman_runtime::trust::TrustMode::all();
        let max = modes.len();
        match action {
            KeyAction::Escape => {
                app.wm.modals.trust_mode_picker_open = false;
            }
            KeyAction::HistoryUp | KeyAction::CursorLeft => {
                app.picker_selected = app.picker_selected.checked_sub(1).unwrap_or(max - 1);
            }
            KeyAction::HistoryDown | KeyAction::CursorRight => {
                app.picker_selected = (app.picker_selected + 1) % max;
            }
            KeyAction::Submit | KeyAction::Char('\r') => {
                let new_mode = modes[app.picker_selected.min(max - 1)];
                let prev = app.trust.mode;
                app.trust.mode = new_mode;
                app.wm.modals.trust_mode_picker_open = false;
                app.save_ui_state();
                if new_mode != prev {
                    if let Some(sess) = app.session.as_ref() {
                        sess.approval().set_auto_ceiling(new_mode.auto_ceiling());
                    }
                    let display = app.trust.theme.display(new_mode);
                    if let Some(warning) = new_mode.warning(&display) {
                        app.push_note(&warning, app::NoteLevel::Warn);
                    }
                }
            }
            KeyAction::Quit => app.should_quit = true,
            _ => {}
        }
        return;
    }
    if app.popup.is_open() {
        match &action {
            KeyAction::Escape => {
                app.popup.close();
                return;
            }
            KeyAction::HistoryUp => {
                app.popup.prev();
                return;
            }
            KeyAction::HistoryDown => {
                app.popup.next();
                return;
            }
            KeyAction::Tab => {
                if let Some(item) = app.popup.accept() {
                    editor.replace_with(&item.insert);
                }
                app.refresh_popup(editor.buf());
                return;
            }
            _ => {
                app.popup.close();
            }
        }
    }
    if !app.pending_approvals.is_empty() && is_approval_key(&action) {
        handle_approval_key(&action, app, control_tx);
        return;
    }
    if app.yank_mode && handle_yank_key(&action, app) {
        return;
    }
    let mut edited = false;
    match action {
        KeyAction::Char(c) => {
            editor.insert_char(c);
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::OpenCommandPalette => {
            app.wm.modals.palette.open();
            *interrupt_prompt = None;
        }
        KeyAction::SearchHistory => {
            app.wm.modals.history_search.open();
            *interrupt_prompt = None;
        }
        KeyAction::Backspace => {
            editor.backspace();
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::Delete => {
            editor.delete_forward();
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::DeleteWordBackward => {
            editor.delete_word_backward();
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::Newline => {
            editor.insert_newline();
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::Submit => {
            if let Some(line) = editor.submit() {
                if !app.has_running_workflow() {
                    app.push_user_turn(line.clone());
                }
                if let Some(tx) = submit_tx {
                    let _ = tx.send(line);
                }
            }
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::HistoryUp => {
            let cw = app
                .input_rect
                .map(|r| r.width.saturating_sub(layout::INPUT_H_OVERHEAD) as usize)
                .unwrap_or(80);
            if !editor.move_line_up_visual(cw) {
                editor.history_up();
            }
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::HistoryDown => {
            let cw = app
                .input_rect
                .map(|r| r.width.saturating_sub(layout::INPUT_H_OVERHEAD) as usize)
                .unwrap_or(80);
            if !editor.move_line_down_visual(cw) {
                editor.history_down();
            }
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::CursorLeft => {
            editor.move_left();
            *interrupt_prompt = None;
        }
        KeyAction::CursorRight => {
            editor.move_right();
            *interrupt_prompt = None;
        }
        KeyAction::CursorHome => {
            editor.move_home();
            *interrupt_prompt = None;
        }
        KeyAction::CursorEnd => {
            editor.move_end();
            *interrupt_prompt = None;
        }
        KeyAction::Tab => {
            if app.trust.mode == atman_runtime::trust::TrustMode::Eager {
                app.trust.outside = app.trust.outside.next();
                app.mark_items_dirty();
                app.save_ui_state();
            } else if editor.expand_paste_at_cursor() {
                edited = true;
            }
            *interrupt_prompt = None;
        }
        KeyAction::NudgePrefill => {
            editor.prefill("!nudge ");
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::CoursePrefill => {
            editor.prefill("!course-correct ");
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::RedirectPrefill => {
            editor.prefill("!redirect ");
            *interrupt_prompt = None;
            edited = true;
        }
        KeyAction::HardStop => {
            if let Some(tx) = control_tx {
                let _ = tx.send(TuiControl::HardStop);
            }
            app.cancel_running_activities();
            *interrupt_prompt = None;
        }
        KeyAction::ScrollUp | KeyAction::PageUp => {
            if let Some(id) = app.wm.focused_id()
                && let Some(p) = app.wm.panels.iter_mut().find(|p| p.id == id)
            {
                p.scroll = p
                    .scroll
                    .saturating_sub(if matches!(action, KeyAction::PageUp) {
                        10
                    } else {
                        3
                    });
                return;
            }
            app.scroll_up(if matches!(action, KeyAction::PageUp) {
                10
            } else {
                1
            });
            *interrupt_prompt = None;
        }
        KeyAction::ScrollDown | KeyAction::PageDown => {
            if let Some(id) = app.wm.focused_id()
                && let Some(p) = app.wm.panels.iter_mut().find(|p| p.id == id)
            {
                p.scroll = p
                    .scroll
                    .saturating_add(if matches!(action, KeyAction::PageDown) {
                        10
                    } else {
                        3
                    });
                return;
            }
            app.scroll_down(if matches!(action, KeyAction::PageDown) {
                10
            } else {
                1
            });
            *interrupt_prompt = None;
        }
        KeyAction::Home => {
            app.scroll_to_top();
            *interrupt_prompt = None;
        }
        KeyAction::End => {
            app.scroll_to_tail();
            *interrupt_prompt = None;
        }
        KeyAction::Escape => {
            if let Some(id) = app.wm.focused_id() {
                if app.wm.panels.iter().any(|p| p.id == id) {
                    app.wm.close(id);
                    return;
                }
            }
            if app.streaming || app.has_running_workflow() {
                if let Some(tx) = control_tx {
                    let _ = tx.send(TuiControl::CancelFlow);
                }
                app.push_note("cancel requested", app::NoteLevel::Warn);
                app.cancel_running_activities();
            }
            *interrupt_prompt = None;
        }
        KeyAction::ToggleSidebar => {
            app.sidebar_collapsed = !app.sidebar_collapsed;
            app.sidebar_upper_runtime_collapsed = false;
            app.sidebar_lower_runtime_collapsed = false;
            app.items_version = app.items_version.wrapping_add(1);
            app.save_ui_state();
            *interrupt_prompt = None;
        }
        KeyAction::ToggleMouseCapture => {
            let now_on = app.toggle_mouse_capture();
            app.save_ui_state();
            if let Err(e) = crate::terminal_guard::set_mouse_capture(now_on) {
                app.push_note(
                    format!("mouse capture toggle failed: {e}"),
                    app::NoteLevel::Warn,
                );
            } else if !now_on && !app.select_mode_hinted {
                app.push_note(
                    "SELECT MODE — drag mouse to copy; press F3 to resume interaction",
                    app::NoteLevel::Info,
                );
                app.select_mode_hinted = true;
            }
            *interrupt_prompt = None;
        }
        KeyAction::ToggleLastTool => {
            app.toggle_last_tool_expansion();
            *interrupt_prompt = None;
        }
        KeyAction::HelpModal => {
            let canvas = app.last_transcript_rect.unwrap_or_default();
            app.wm.open(
                "cheatsheet",
                crate::wm::ContentKey::Cheatsheet,
                crate::wm::WindowContent::Cheatsheet,
                "Keybindings",
                canvas,
            );
            if let Some(p) = app
                .wm
                .panels
                .iter_mut()
                .find(|p| p.content_key == crate::wm::ContentKey::Cheatsheet)
            {
                p.content = Some(Box::new(
                    crate::window::cheatsheet_panel::CheatsheetPanelContent { scroll: 0 },
                ));
            }
            *interrupt_prompt = None;
        }
        KeyAction::Interrupt => {
            // Ctrl+C priority: clear input first, then stop flow, then quit.
            if !editor.buf().is_empty() {
                editor.clear();
                edited = true;
                *interrupt_prompt = None;
            } else if app.streaming || app.has_running_workflow() {
                if let Some(tx) = control_tx {
                    let _ = tx.send(TuiControl::HardStop);
                }
                app.push_note("flow stopped (Ctrl+C)", app::NoteLevel::Warn);
                app.cancel_running_activities();
                *interrupt_prompt = None;
            } else {
                let within_window = interrupt_prompt
                    .map(|t| t.elapsed() < std::time::Duration::from_millis(1500))
                    .unwrap_or(false);
                if within_window {
                    app.should_quit = true;
                } else {
                    *interrupt_prompt = Some(std::time::Instant::now());
                    app.push_note("press Ctrl+C again to quit", app::NoteLevel::Warn);
                }
            }
        }
        KeyAction::Quit => {
            app.should_quit = true;
        }
        KeyAction::Ignore => {
            *interrupt_prompt = None;
        }
        KeyAction::BackTab => {}
        KeyAction::CyclePanelForward | KeyAction::CyclePanelBackward => {}
    }
    if edited {
        app.refresh_popup(editor.buf());
    }
}

// The outgoing tui exits fast; the incoming tui plays the fade+slide
// intro on top of the freshly rendered new session so content appears
// first, then the banner/sessions fade out and input docks bottom.
pub(crate) fn request_session_switch(
    app: &mut AppState,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
    sid: String,
) {
    let intro = match app.items.first() {
        Some(crate::app::OutputItem::StartupCard { version, recent }) => crate::app::StartupIntro {
            started_at: std::time::Instant::now(),
            version: version.clone(),
            recent: recent.clone(),
        },
        _ => {
            // No StartupCard (e.g. mid-session switch): still play the
            // fade transition so the user sees a smooth hand-off.
            crate::app::StartupIntro {
                started_at: std::time::Instant::now(),
                version: env!("CARGO_PKG_VERSION").to_string(),
                recent: Vec::new(),
            }
        }
    };
    if let Some(tx) = control_tx {
        let _ = tx.send(TuiControl::SwitchSession {
            sid,
            intro: intro.clone(),
        });
    }
    app.should_quit = true;
}

#[cfg(test)]
mod tests {
    use crate::history_search_modal::extract_event_text;

    #[test]
    fn extract_event_text_user_msg() {
        let payload = r#"{"type":"user_msg","seq":1,"message":{"role":"user","parts":[{"type":"text","text":"hello world"}]}}"#;
        assert_eq!(
            extract_event_text("user_msg", payload),
            Some("hello world".into())
        );
    }

    #[test]
    fn extract_event_text_assistant_with_thinking() {
        let payload = r#"{"type":"assistant_msg","message":{"role":"assistant","parts":[{"type":"thinking","thinking":"let me think"},{"type":"text","text":"answer"}]}}"#;
        let text = extract_event_text("assistant_msg", payload).unwrap();
        assert!(text.contains("answer"));
        assert!(text.contains("let me think"));
    }

    #[test]
    fn extract_event_text_tool_result_wraps_in_code_block() {
        let payload = r#"{"type":"tool_result_msg","message":{"role":"tool","parts":[{"type":"tool_result","tool_use_id":"x","content":"line1\nline2","is_error":false}]}}"#;
        let text = extract_event_text("tool_result_msg", payload).unwrap();
        assert!(text.contains("```"));
        assert!(text.contains("line1"));
    }

    #[test]
    fn extract_event_text_unknown_kind_returns_none() {
        let payload = r#"{"type":"flow_start"}"#;
        assert_eq!(extract_event_text("flow_start", payload), None);
    }
}
