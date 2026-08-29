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
            | KeyAction::Char('s')
            | KeyAction::Char('S')
            | KeyAction::Char('x')
            | KeyAction::Char('X')
            | KeyAction::Char('g')
            | KeyAction::Char('G')
            | KeyAction::Char('f')
            | KeyAction::Char('F')
            | KeyAction::Char('[')
            | KeyAction::Char(']')
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
    let groups: Vec<_> = app.pending_permission_groups.values().cloned().collect();
    let canonical: Vec<_> = app
        .pending_permissions
        .values()
        .filter(|p| {
            p.payload.group_ids.is_empty()
                || !groups.iter().any(|group| {
                    group
                        .payload
                        .request_ids
                        .iter()
                        .any(|id| id == &p.request_id)
                })
        })
        .cloned()
        .collect();
    let scope = |p: &crate::app::PendingPermission, index: u8| match index {
        1 => Some(atman_runtime::permission::GrantScope::ChildRunSameTool {
            run_id: p.payload.requesting_run_id.clone(),
            tool_name: p.payload.tool.clone(),
        }),
        2 => p.payload.provenance.path.as_ref().and_then(|path| {
            let root = p.payload.provenance.workspace_root.as_ref()?;
            let relative = std::path::Path::new(path)
                .strip_prefix(root)
                .ok()?
                .to_string_lossy()
                .into_owned();
            Some(
                atman_runtime::permission::GrantScope::ChildRunSamePathRule {
                    run_id: p.payload.requesting_run_id.clone(),
                    tool_name: p.payload.tool.clone(),
                    workspace_relative_path: relative,
                },
            )
        }),
        _ => Some(atman_runtime::permission::GrantScope::CurrentCall),
    };
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
                if let Some(p) = canonical.get(idx) {
                    let _ = tx.send(TuiControl::ResolvePermission {
                        selector: atman_runtime::permission::PermissionSelector::RequestIds(vec![
                            p.request_id.clone(),
                        ]),
                        expected_revision: p.revision,
                        action: atman_runtime::permission::PermissionAction::Deny,
                        grant_scope: None,
                        reason: Some("denied by user".into()),
                    });
                    app.push_note(format!("denied {}", p.payload.tool), app::NoteLevel::Warn);
                }
                app.deny_arm = None;
                true
            }
            '1'..='9' => {
                let idx = (*c as u8 - b'1') as usize;
                if let Some(p) = canonical.get(idx) {
                    let _ = tx.send(TuiControl::ResolvePermission {
                        selector: atman_runtime::permission::PermissionSelector::RequestIds(vec![
                            p.request_id.clone(),
                        ]),
                        expected_revision: p.revision,
                        action: atman_runtime::permission::PermissionAction::Approve,
                        grant_scope: scope(p, app.approval_scope_index).or_else(|| {
                            (app.approval_scope_index == 2)
                                .then_some(atman_runtime::permission::GrantScope::CurrentCall)
                        }),
                        reason: None,
                    });
                    app.push_note(
                        format!("approved {} ({})", p.payload.tool, p.request_id),
                        app::NoteLevel::Info,
                    );
                }
                true
            }
            '[' | ']' => {
                let ids: Vec<_> = app.pending_permission_groups.keys().cloned().collect();
                if !ids.is_empty() {
                    let current = app
                        .selected_permission_group
                        .as_ref()
                        .and_then(|id| ids.iter().position(|candidate| candidate == id))
                        .unwrap_or(0);
                    let next = if *c == '[' {
                        current.checked_sub(1).unwrap_or(ids.len() - 1)
                    } else {
                        (current + 1) % ids.len()
                    };
                    app.selected_permission_group = Some(ids[next].clone());
                }
                true
            }
            's' | 'S' => {
                let has_path_scope = !canonical.is_empty()
                    && canonical.iter().all(|p| {
                        p.payload.provenance.path.is_some()
                            && p.payload.provenance.workspace_root.is_some()
                            && p.payload
                                .provenance
                                .path
                                .as_ref()
                                .zip(p.payload.provenance.workspace_root.as_ref())
                                .is_some_and(|(path, root)| {
                                    std::path::Path::new(path).strip_prefix(root).is_ok()
                                })
                    });
                let available = if has_path_scope { 3 } else { 2 };
                app.approval_scope_index = (app.approval_scope_index + 1) % available;
                let label = match app.approval_scope_index {
                    0 => "call",
                    1 => "tool",
                    2 if has_path_scope => "path",
                    _ => "call",
                };
                app.push_note(format!("grant scope: {label}"), app::NoteLevel::Info);
                true
            }
            'x' | 'X' => {
                if let Some(group_id) = app
                    .selected_permission_group
                    .clone()
                    .or_else(|| app.pending_permission_groups.keys().next().cloned())
                    && let Some(group) = app.pending_permission_groups.get_mut(&group_id)
                {
                    group.expanded = !group.expanded;
                    app.selected_permission_group = Some(group_id);
                }
                true
            }
            'g' | 'G' | 'f' | 'F' => {
                let selected_group = app
                    .selected_permission_group
                    .clone()
                    .or_else(|| app.pending_permission_groups.keys().next().cloned());
                if let Some(group_id) = selected_group
                    && let Some(group) = app.pending_permission_groups.get(&group_id)
                {
                    let action = if matches!(c, 'f' | 'F') {
                        atman_runtime::permission::PermissionAction::Defer
                    } else if deny_armed {
                        atman_runtime::permission::PermissionAction::Deny
                    } else {
                        atman_runtime::permission::PermissionAction::Approve
                    };
                    let _ = tx.send(TuiControl::ResolvePermission {
                        selector: atman_runtime::permission::PermissionSelector::Group(
                            group.group_id.clone(),
                        ),
                        expected_revision: group.revision,
                        action,
                        grant_scope: (action
                            == atman_runtime::permission::PermissionAction::Approve)
                            .then_some(atman_runtime::permission::GrantScope::CurrentCall),
                        reason: (action == atman_runtime::permission::PermissionAction::Deny)
                            .then(|| "denied by user".into()),
                    });
                    app.push_note(
                        format!("group {}: {action:?}", group.group_id),
                        if action == atman_runtime::permission::PermissionAction::Deny {
                            app::NoteLevel::Warn
                        } else {
                            app::NoteLevel::Info
                        },
                    );
                }
                app.deny_arm = None;
                true
            }
            'a' | 'A' => {
                for group in &groups {
                    let _ = tx.send(TuiControl::ResolvePermission {
                        selector: atman_runtime::permission::PermissionSelector::Group(
                            group.group_id.clone(),
                        ),
                        expected_revision: group.revision,
                        action: atman_runtime::permission::PermissionAction::Approve,
                        grant_scope: Some(atman_runtime::permission::GrantScope::CurrentCall),
                        reason: None,
                    });
                }
                for p in &canonical {
                    let _ = tx.send(TuiControl::ResolvePermission {
                        selector: atman_runtime::permission::PermissionSelector::RequestIds(vec![
                            p.request_id.clone(),
                        ]),
                        expected_revision: p.revision,
                        action: atman_runtime::permission::PermissionAction::Approve,
                        grant_scope: scope(p, app.approval_scope_index).or_else(|| {
                            (app.approval_scope_index == 2)
                                .then_some(atman_runtime::permission::GrantScope::CurrentCall)
                        }),
                        reason: None,
                    });
                }
                app.push_note(
                    format!("approved all {} pending", canonical.len()),
                    app::NoteLevel::Info,
                );
                app.deny_arm = None;
                true
            }
            'd' | 'D' => {
                let pending_len = canonical.len();
                let deny_first = pending_len <= 1 || deny_armed;
                if deny_first {
                    if let Some(p) = canonical.first() {
                        let _ = tx.send(TuiControl::ResolvePermission {
                            selector: atman_runtime::permission::PermissionSelector::RequestIds(
                                vec![p.request_id.clone()],
                            ),
                            expected_revision: p.revision,
                            action: atman_runtime::permission::PermissionAction::Deny,
                            grant_scope: None,
                            reason: Some("denied by user".into()),
                        });
                        app.push_note(format!("denied {}", p.payload.tool), app::NoteLevel::Warn);
                    }
                    app.deny_arm = None;
                } else {
                    app.deny_arm = Some(std::time::Instant::now());
                    app.push_note(
                        format!("d + N to deny nth, dd to deny first (of {pending_len})"),
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
            for group in &groups {
                let _ = tx.send(TuiControl::ResolvePermission {
                    selector: atman_runtime::permission::PermissionSelector::Group(
                        group.group_id.clone(),
                    ),
                    expected_revision: group.revision,
                    action: atman_runtime::permission::PermissionAction::Deny,
                    grant_scope: None,
                    reason: Some("user pressed Esc".into()),
                });
            }
            for p in &canonical {
                let _ = tx.send(TuiControl::ResolvePermission {
                    selector: atman_runtime::permission::PermissionSelector::RequestIds(vec![
                        p.request_id.clone(),
                    ]),
                    expected_revision: p.revision,
                    action: atman_runtime::permission::PermissionAction::Deny,
                    grant_scope: None,
                    reason: Some("user pressed Esc".into()),
                });
            }
            let _ = tx.send(TuiControl::CancelFlow);
            app.push_note(
                format!("denied all {} pending, flow cancelled", canonical.len()),
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
    submit_tx: Option<&mpsc::UnboundedSender<crate::TuiSubmission>>,
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
                    if let Some(session) = app.session.as_ref() {
                        for source in editor.prune_missing_image_references() {
                            session.remove_pending_image(&source);
                        }
                        app.attach_count = session.pending_image_count();
                    }
                }
                app.refresh_popup(editor.buf());
                return;
            }
            _ => {
                app.popup.close();
            }
        }
    }
    if !app.pending_permissions.is_empty() && is_approval_key(&action) {
        handle_approval_key(&action, app, control_tx);
        return;
    }
    if app.yank_mode && handle_yank_key(&action, app) {
        return;
    }
    let mut edited = false;
    match action {
        KeyAction::PasteImage => {
            let Some(session) = app.session.clone() else {
                app.push_note(
                    "image paste requires an active session",
                    app::NoteLevel::Warn,
                );
                return;
            };
            match crate::clipboard::read_image_png().and_then(|bytes| {
                session
                    .import_image_bytes(&bytes, Some("clipboard.png"))
                    .map_err(Into::into)
            }) {
                Ok(source) => {
                    let count = session.queue_image_source(source.clone());
                    let number = editor.attach_image(source);
                    app.attach_count = count;
                    app.push_note(
                        format!("attached clipboard image as [image {number}] ({count} pending)"),
                        app::NoteLevel::Info,
                    );
                }
                Err(error) => app.push_note(
                    format!("clipboard does not contain a usable image: {error}"),
                    app::NoteLevel::Warn,
                ),
            }
            *interrupt_prompt = None;
        }
        KeyAction::RemoveAttachment => {
            let Some(session) = app.session.clone() else {
                return;
            };
            editor.reconcile_images(&session.pending_images());
            match editor.remove_last_image() {
                Some(source) => {
                    session.remove_pending_image(&source);
                    let count = session.pending_image_count();
                    app.attach_count = count;
                    app.push_note(
                        format!(
                            "removed attachment {} ({} pending)",
                            atman_runtime::attachment_store::display_name(&source),
                            count
                        ),
                        app::NoteLevel::Info,
                    );
                }
                None => app.push_note("no pending image attachment", app::NoteLevel::Warn),
            }
            *interrupt_prompt = None;
        }
        KeyAction::CycleReasoning => {
            let Some(session) = app.session.clone() else {
                return;
            };
            let choices = reasoning_choices(&session);
            let current = session.reasoning_override();
            let index = choices
                .iter()
                .position(|choice| *choice == current)
                .unwrap_or(0);
            let next = choices[(index + 1) % choices.len()].clone();
            session.set_reasoning_override(next.clone());
            app.push_note(
                format!(
                    "session reasoning: {}",
                    next.map(|selection| selection.to_string())
                        .unwrap_or_else(|| "model default".into())
                ),
                app::NoteLevel::Info,
            );
            *interrupt_prompt = None;
        }
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
            if let Some(session) = app.session.as_ref() {
                editor.reconcile_images(&session.pending_images());
            }
            if let Some(editor_submission) = editor.submit_with_images() {
                let line = editor_submission.text;
                if !app.has_running_workflow() {
                    app.push_user_turn(line.clone());
                }
                if let Some(tx) = submit_tx {
                    let images = if line.trim_start().starts_with(':') {
                        Vec::new()
                    } else {
                        app.session
                            .as_ref()
                            .map(|session| session.take_pending_images())
                            .unwrap_or_default()
                    };
                    let submission = crate::TuiSubmission { text: line, images };
                    if let Err(error) = tx.send(submission)
                        && let Some(session) = app.session.clone()
                    {
                        app.attach_count = session.restore_pending_images(error.0.images);
                        editor.reconcile_images(&session.pending_images());
                    } else if let Some(session) = app.session.clone() {
                        app.attach_count = session.pending_image_count();
                        if !editor_submission.images.is_empty() {
                            editor.reconcile_images(&session.pending_images());
                        }
                    }
                } else if let Some(session) = app.session.clone() {
                    editor.reconcile_images(&session.pending_images());
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
                let mut trust = app.trust.clone();
                trust.escalation = trust.escalation.next();
                if let Some(tx) = control_tx {
                    let _ = tx.send(TuiControl::UpdateTrust(trust));
                }
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
        if let Some(session) = app.session.as_ref() {
            for source in editor.prune_missing_image_references() {
                session.remove_pending_image(&source);
            }
            app.attach_count = session.pending_image_count();
        }
        app.refresh_popup(editor.buf());
    }
}

fn reasoning_choices(
    session: &atman_runtime::Session,
) -> Vec<Option<atman_runtime::provider::ReasoningSelection>> {
    use atman_runtime::provider::{ReasoningEffort, ReasoningSelection};

    let info = atman_runtime::model_registry::model_info(&session.last_model());
    let mut choices = vec![
        None,
        Some(ReasoningSelection::Disabled),
        Some(ReasoningSelection::Auto {
            execution_mode: None,
        }),
    ];
    let openai_reasoning_format = atman_runtime::model_registry::model_entry(&session.last_model())
        .and_then(|model| model.provider)
        .and_then(|provider| {
            atman_runtime::model_registry::all_provider_entries()
                .into_iter()
                .find(|(name, _)| name == &provider)
                .and_then(|(_, entry)| {
                    matches!(entry.kind.as_str(), "openai" | "openai-compat").then(|| {
                        entry.reasoning_format.unwrap_or_else(|| {
                            atman_runtime::providers::openai::OpenAiReasoningFormat::for_provider_kind(
                                &entry.kind,
                            )
                        })
                    })
                })
        });
    let fallback_efforts = [
        ReasoningEffort::Minimal,
        ReasoningEffort::Low,
        ReasoningEffort::Medium,
        ReasoningEffort::High,
        ReasoningEffort::XHigh,
        ReasoningEffort::Max,
        ReasoningEffort::Ultra,
    ];
    let efforts = match openai_reasoning_format {
        Some(atman_runtime::providers::openai::OpenAiReasoningFormat::CompatibleThinking) => &[],
        Some(atman_runtime::providers::openai::OpenAiReasoningFormat::Official)
            if info.capabilities.reasoning_efforts.is_empty() =>
        {
            fallback_efforts.as_slice()
        }
        _ => info.capabilities.reasoning_efforts.as_slice(),
    };
    for effort in efforts {
        let selection = Some(ReasoningSelection::Effort {
            effort: effort.clone(),
            execution_mode: None,
        });
        if !choices.contains(&selection) {
            choices.push(selection);
        }
    }
    choices
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
    use super::*;
    use crate::history_search_modal::extract_event_text;

    const PNG_BYTES: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d,
    ];

    fn pending_permission(revision: u64) -> crate::app::PendingPermission {
        use atman_runtime::event::FlowRunId;
        use atman_runtime::permission::PermissionRequestId;
        use atman_runtime::permission_audit::{
            PermissionAuditTarget, PermissionPolicyReference, PermissionProvenanceSummary,
            PermissionRequestAudit,
        };

        let request_id = PermissionRequestId::now();
        let run_id = FlowRunId::now();
        crate::app::PendingPermission {
            request_id: request_id.clone(),
            revision,
            payload: PermissionRequestAudit {
                request_id: Some(request_id),
                revision,
                session_id: "session".into(),
                requesting_run_id: run_id.clone(),
                parent_run_id: None,
                root_run_id: run_id,
                tool_use_id: format!("tool-{revision}"),
                tool: "fs.read".into(),
                tier: atman_runtime::tool::Tier::Two,
                provenance: PermissionProvenanceSummary::default(),
                target: PermissionAuditTarget::User,
                group_ids: Vec::new(),
                policy: PermissionPolicyReference {
                    snapshot_id: "snapshot".into(),
                    rule_id: "rule".into(),
                },
                escalation_path: Vec::new(),
                decision_id: None,
                actor: None,
                scope: None,
                reason: None,
                at: chrono::Utc::now(),
            },
        }
    }

    #[test]
    fn submit_captures_the_current_attachment_snapshot() {
        let session = std::sync::Arc::new(atman_runtime::Session::open_ephemeral());
        session
            .queue_image_bytes(PNG_BYTES, Some("first.png"))
            .unwrap();
        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        state.app.session = Some(session.clone());
        state.app.attach_count = 1;
        let mut editor = InputEditor::default();
        editor.insert_str("/agent inspect");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut interrupt_prompt = None;

        handle_key(
            KeyAction::Submit,
            &mut state,
            &mut editor,
            &mut interrupt_prompt,
            Some(&tx),
            None,
        );
        session
            .queue_image_bytes(&[PNG_BYTES, &[0x01]].concat(), Some("second.png"))
            .unwrap();

        let submission = rx.try_recv().unwrap();
        assert_eq!(submission.text, "/agent inspect");
        assert_eq!(submission.images.len(), 1);
        assert_eq!(session.pending_image_count(), 1);
    }

    #[test]
    fn failed_submit_restores_captured_attachments() {
        let session = std::sync::Arc::new(atman_runtime::Session::open_ephemeral());
        session
            .queue_image_bytes(PNG_BYTES, Some("clipboard.png"))
            .unwrap();
        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        state.app.session = Some(session.clone());
        let mut editor = InputEditor::default();
        editor.insert_str("/agent inspect");
        let (tx, rx) = mpsc::unbounded_channel();
        drop(rx);
        let mut interrupt_prompt = None;

        handle_key(
            KeyAction::Submit,
            &mut state,
            &mut editor,
            &mut interrupt_prompt,
            Some(&tx),
            None,
        );

        assert_eq!(session.pending_image_count(), 1);
        assert_eq!(state.app.attach_count, 1);
    }

    #[test]
    fn canonical_numeric_selection_sends_stable_request_id_and_revision() {
        let mut app = AppState::new("session".into(), None);
        let first = pending_permission(7);
        let second = pending_permission(11);
        app.pending_permissions
            .insert(first.request_id.clone(), first.clone());
        app.pending_permissions
            .insert(second.request_id.clone(), second.clone());
        let selected = app.pending_permissions.values().next().unwrap().clone();
        let (tx, mut rx) = mpsc::unbounded_channel();

        assert!(handle_approval_key(
            &KeyAction::Char('1'),
            &mut app,
            Some(&tx)
        ));
        let TuiControl::ResolvePermission {
            selector,
            expected_revision,
            action,
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("expected canonical permission control");
        };
        assert_eq!(
            selector,
            atman_runtime::permission::PermissionSelector::RequestIds(vec![
                selected.request_id.clone()
            ])
        );
        assert_eq!(expected_revision, selected.revision);
        assert_eq!(action, atman_runtime::permission::PermissionAction::Approve);

        app.pending_permissions.remove(&selected.request_id);
        let remaining = app.pending_permissions.values().next().unwrap().clone();
        assert!(handle_approval_key(
            &KeyAction::Char('1'),
            &mut app,
            Some(&tx)
        ));
        let TuiControl::ResolvePermission { selector, .. } = rx.try_recv().unwrap() else {
            panic!("expected canonical permission control");
        };
        assert_eq!(
            selector,
            atman_runtime::permission::PermissionSelector::RequestIds(vec![remaining.request_id])
        );
    }

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
