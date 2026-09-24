use crate::UiState;
use tokio::sync::mpsc;

use super::TuiControl;
use crate::app::AppState;
use crate::input::InputEditor;
use crate::keys::KeyAction;
use crate::{app, layout};

pub(crate) fn enter_selection_mode(app: &mut AppState) -> bool {
    let points = app.last_selection_projection.visual_points();
    let Some(index) = points.len().checked_sub(1) else {
        app.yank_mode = false;
        app.selection = None;
        app.push_note("no visible selectable content", app::NoteLevel::Warn);
        return false;
    };
    let current = &points[index];
    app.yank_mode = true;
    app.yank_index = index;
    app.selection = Some(crate::selection::selection_begin(
        current.point.clone(),
        current.owner_revision,
        app.last_selection_projection.structure_revision,
    ));
    app.push_note(
        "select & copy — move, v anchor, Enter copy, Esc cancel",
        app::NoteLevel::Info,
    );
    true
}

fn selection_cursor_note(app: &mut AppState, points: &[crate::selection::VisualSelectionPoint]) {
    let Some(current) = points.get(app.yank_index) else {
        return;
    };
    app.push_note(
        format!(
            "select & copy — row {} col {}",
            current.row + 1,
            current.col + 1
        ),
        app::NoteLevel::Info,
    );
}

fn copy_text(app: &mut AppState, text: &str, success: String) {
    match crate::clipboard::write_text(text) {
        Ok(()) => app.push_note(success, app::NoteLevel::Info),
        Err(error) => app.push_note(
            format!("clipboard write failed: {error}"),
            app::NoteLevel::Error,
        ),
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
            copy_text(app, &t, format!("copied {n} chars from last message"));
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
            copy_text(app, &t, format!("copied {n} chars from last tool output"));
        }
        _ => app.push_note("no tool output to copy", app::NoteLevel::Warn),
    }
}

pub(crate) fn open_project_storage_scope_picker(app: &mut AppState) {
    let Some(session) = app.session.as_ref() else {
        app.push_note("project storage is unavailable", app::NoteLevel::Warn);
        return;
    };
    let Some(project_root) = session.meta().and_then(|meta| meta.project_root) else {
        app.push_note("current session has no project", app::NoteLevel::Warn);
        return;
    };
    let current = match atman_runtime::storage::load_storage_config(Some(&project_root))
        .scope
        .unwrap_or_default()
    {
        atman_runtime::storage::StorageScope::Global => "global",
        atman_runtime::storage::StorageScope::Local => "local",
    };
    let prompt = format!(
        "Store Atman data for {} where? Current: {current}. The change applies to new sessions.",
        project_root.display()
    );
    let kind = atman_runtime::form::FormKind::SingleSelect {
        prompt,
        options: vec!["Global".into(), "Local (.atman)".into()],
    };
    let form = atman_runtime::form::PendingForm {
        form_id: crate::PROJECT_STORAGE_SCOPE_FORM_ID.to_string(),
        run_id: atman_runtime::event::FlowRunId::now(),
        tool_use_id: crate::PROJECT_STORAGE_SCOPE_FORM_ID.to_string(),
        kind: kind.clone(),
        form: atman_runtime::form::CompositeForm {
            questions: vec![atman_runtime::form::FormQuestion {
                id: "scope".into(),
                kind,
            }],
        },
        emitted_at: chrono::Utc::now(),
    };
    session.forms().request(form);
}

pub(crate) fn open_formula_rendering_picker(app: &mut AppState) {
    let Some(session) = app.session.as_ref() else {
        app.push_note("formula setting is unavailable", app::NoteLevel::Warn);
        return;
    };
    let current = match atman_runtime::config_hub::ConfigHub::global()
        .and_then(|hub| hub.math_rendering_enabled())
    {
        Ok(true) => "Rendered",
        Ok(false) => "Raw Markdown",
        Err(error) => {
            app.push_note(
                format!("formula setting unavailable: {error}"),
                app::NoteLevel::Error,
            );
            return;
        }
    };
    let kind = atman_runtime::form::FormKind::SingleSelect {
        prompt: format!("Formula display (current: {current}). Restart Atman to apply changes."),
        options: vec!["Rendered".into(), "Raw Markdown".into()],
    };
    let form = atman_runtime::form::PendingForm {
        form_id: crate::FORMULA_RENDERING_FORM_ID.to_string(),
        run_id: atman_runtime::event::FlowRunId::now(),
        tool_use_id: crate::FORMULA_RENDERING_FORM_ID.to_string(),
        kind: kind.clone(),
        form: atman_runtime::form::CompositeForm {
            questions: vec![atman_runtime::form::FormQuestion {
                id: "math".into(),
                kind,
            }],
        },
        emitted_at: chrono::Utc::now(),
    };
    session.forms().request(form);
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
        let stats = atman_runtime::session_meta::SessionStats::load_or_rebuild(&entry.path())
            .unwrap_or_default();
        if stats.user_message_count == 0 {
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
            message_count: stats.message_count as usize,
            updated_at,
            goal,
        });
    }
    rows.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
    rows.truncate(200);
    rows
}

pub(crate) fn handle_yank_key(action: &KeyAction, app: &mut AppState) -> bool {
    let points = app.last_selection_projection.visual_points();
    if points.is_empty() {
        app.yank_mode = false;
        app.selection = None;
        app.push_note("no visible selectable content", app::NoteLevel::Warn);
        return true;
    }
    app.yank_index = app.yank_index.min(points.len() - 1);
    let anchored = app
        .selection
        .as_ref()
        .is_some_and(|state| state.phase != crate::selection::SelectionPhase::Pending);
    let domain = app
        .selection
        .as_ref()
        .map(|state| state.anchor.domain.clone());

    let eligible = |index: usize| !anchored || domain.as_ref() == Some(&points[index].point.domain);
    let move_to = |app: &mut AppState, index: usize| {
        let current = &points[index];
        app.yank_index = index;
        app.selection = match app.selection.as_ref() {
            Some(state) if anchored => crate::selection::selection_extend(
                state,
                current.point.clone(),
                state.owner_revision,
                app.last_selection_projection.structure_revision,
            ),
            _ => Some(crate::selection::selection_begin(
                current.point.clone(),
                current.owner_revision,
                app.last_selection_projection.structure_revision,
            )),
        };
    };

    match action {
        KeyAction::Escape => {
            app.yank_mode = false;
            app.selection = None;
            app.push_note("selection cancelled", app::NoteLevel::Info);
        }
        KeyAction::Char('v') | KeyAction::Char('V') => {
            let current = &points[app.yank_index];
            app.selection = Some(crate::selection::selection_begin(
                current.point.clone(),
                current.owner_revision,
                app.last_selection_projection.structure_revision,
            ));
            if let Some(state) = app.selection.as_mut() {
                state.phase = crate::selection::SelectionPhase::Active;
            }
            app.push_note("selection anchor set", app::NoteLevel::Info);
        }
        KeyAction::Char('h') | KeyAction::CursorLeft => {
            if let Some(index) = (0..app.yank_index).rev().find(|index| eligible(*index)) {
                move_to(app, index);
                selection_cursor_note(app, &points);
            }
        }
        KeyAction::Char('l') | KeyAction::CursorRight => {
            if let Some(index) = (app.yank_index + 1..points.len()).find(|index| eligible(*index)) {
                move_to(app, index);
                selection_cursor_note(app, &points);
            }
        }
        KeyAction::Char('j') | KeyAction::HistoryDown => {
            let current = &points[app.yank_index];
            if let Some(row) = points
                .iter()
                .enumerate()
                .filter(|(index, point)| eligible(*index) && point.row > current.row)
                .map(|(_, point)| point.row)
                .min()
                && let Some((index, _)) = points
                    .iter()
                    .enumerate()
                    .filter(|(index, point)| eligible(*index) && point.row == row)
                    .min_by_key(|(_, point)| point.col.abs_diff(current.col))
            {
                move_to(app, index);
                selection_cursor_note(app, &points);
            }
        }
        KeyAction::Char('k') | KeyAction::HistoryUp => {
            let current = &points[app.yank_index];
            if let Some(row) = points
                .iter()
                .enumerate()
                .filter(|(index, point)| eligible(*index) && point.row < current.row)
                .map(|(_, point)| point.row)
                .max()
                && let Some((index, _)) = points
                    .iter()
                    .enumerate()
                    .filter(|(index, point)| eligible(*index) && point.row == row)
                    .min_by_key(|(_, point)| point.col.abs_diff(current.col))
            {
                move_to(app, index);
                selection_cursor_note(app, &points);
            }
        }
        KeyAction::Char('g') | KeyAction::Char('G') => {
            let surface = points[app.yank_index].surface;
            let mut candidates = points
                .iter()
                .enumerate()
                .filter(|(index, point)| eligible(*index) && point.surface == surface)
                .map(|(index, _)| index);
            let index = if matches!(action, KeyAction::Char('g')) {
                candidates.next()
            } else {
                candidates.next_back()
            };
            if let Some(index) = index {
                move_to(app, index);
                selection_cursor_note(app, &points);
            }
        }
        KeyAction::Tab | KeyAction::BackTab if !anchored => {
            let surface = points[app.yank_index].surface;
            let index = if matches!(action, KeyAction::Tab) {
                points.iter().position(|point| point.surface > surface)
            } else {
                points
                    .iter()
                    .enumerate()
                    .rev()
                    .find(|(_, point)| point.surface < surface)
                    .map(|(index, _)| index)
            };
            if let Some(index) = index {
                move_to(app, index);
                selection_cursor_note(app, &points);
            }
        }
        KeyAction::Submit => {
            let payload = app.selection.as_ref().and_then(|state| {
                crate::selection::selection_copy_payload(&app.last_selection_projection, state)
            });
            match payload {
                Some(crate::selection::CopyPayload::Markdown(text))
                | Some(crate::selection::CopyPayload::PlainText(text))
                | Some(crate::selection::CopyPayload::Preview(text)) => {
                    let n = text.chars().count();
                    copy_text(app, &text, format!("copied {n} chars"));
                    app.yank_mode = false;
                }
                None => app.push_note("set an anchor and select text first", app::NoteLevel::Warn),
            }
        }
        _ => {}
    }
    true
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
        .filter(|p| !app.grouped_permission_request_ids.contains(&p.request_id))
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
                    app.push_toast(
                        format!("denied {}", p.payload.tool),
                        app::NoteLevel::Warn,
                        std::time::Duration::from_secs(3),
                        app::ToastPosition::TopRight,
                    );
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
                    app.push_toast(
                        format!("allowed {}", p.payload.tool),
                        app::NoteLevel::Info,
                        std::time::Duration::from_secs(3),
                        app::ToastPosition::TopRight,
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
                app.push_toast(
                    format!("grant scope: {label}"),
                    app::NoteLevel::Info,
                    std::time::Duration::from_secs(3),
                    app::ToastPosition::TopRight,
                );
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
                    app.push_toast(
                        format!("permission group: {action:?}"),
                        if action == atman_runtime::permission::PermissionAction::Deny {
                            app::NoteLevel::Warn
                        } else {
                            app::NoteLevel::Info
                        },
                        std::time::Duration::from_secs(3),
                        app::ToastPosition::TopRight,
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
                app.push_toast(
                    format!("allowed all {} pending", canonical.len()),
                    app::NoteLevel::Info,
                    std::time::Duration::from_secs(3),
                    app::ToastPosition::TopRight,
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
                        app.push_toast(
                            format!("denied {}", p.payload.tool),
                            app::NoteLevel::Warn,
                            std::time::Duration::from_secs(3),
                            app::ToastPosition::TopRight,
                        );
                    }
                    app.deny_arm = None;
                } else {
                    app.deny_arm = Some(std::time::Instant::now());
                    app.push_toast(
                        format!("d + N to deny nth, dd to deny first (of {pending_len})"),
                        app::NoteLevel::Info,
                        std::time::Duration::from_secs(3),
                        app::ToastPosition::TopRight,
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
            app.push_toast(
                format!("denied all {} pending, flow cancelled", canonical.len()),
                app::NoteLevel::Warn,
                std::time::Duration::from_secs(3),
                app::ToastPosition::TopRight,
            );
            app.deny_arm = None;
            app.cancel_running_activities();
            true
        }
        _ => false,
    }
}

pub(crate) fn dispatch_approval_click(
    click: crate::approval_bar::ApprovalClick,
    app: &mut AppState,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) {
    use crate::approval_bar::ApprovalClick;
    match click {
        ApprovalClick::ApproveRequest(index) if index < 9 => {
            let key = KeyAction::Char(char::from(b'1' + index as u8));
            handle_approval_key(&key, app, control_tx);
        }
        ApprovalClick::DenyRequest(index) if index < 9 => {
            app.deny_arm = Some(std::time::Instant::now());
            let key = KeyAction::Char(char::from(b'1' + index as u8));
            handle_approval_key(&key, app, control_tx);
        }
        ApprovalClick::ToggleGroup(group_id) => {
            app.selected_permission_group = Some(group_id);
            handle_approval_key(&KeyAction::Char('x'), app, control_tx);
        }
        ApprovalClick::ApproveGroup(group_id) => {
            app.selected_permission_group = Some(group_id);
            handle_approval_key(&KeyAction::Char('g'), app, control_tx);
        }
        ApprovalClick::DeferGroup(group_id) => {
            app.selected_permission_group = Some(group_id);
            handle_approval_key(&KeyAction::Char('f'), app, control_tx);
        }
        ApprovalClick::ApproveAll => {
            handle_approval_key(&KeyAction::Char('a'), app, control_tx);
        }
        ApprovalClick::DenyAll => {
            app.deny_arm = Some(std::time::Instant::now());
            handle_approval_key(&KeyAction::Escape, app, control_tx);
        }
        _ => {}
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
            app.mark_visual_dirty();
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
                app.wm.modals.open_mcp_add();
                return;
            }
            KeyAction::Char('e') => {
                if let Some(server) = app.context.mcp_servers.get(app.mcp_selected) {
                    let name = server.name.clone();
                    if let Err(error) = app.wm.modals.open_mcp_edit(&name) {
                        app.push_toast(
                            error,
                            app::NoteLevel::Warn,
                            std::time::Duration::from_secs(4),
                            app::ToastPosition::TopRight,
                        );
                    }
                }
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
    if app.submission_focus {
        handle_submission_queue_key(&action, app, control_tx);
        return;
    }
    if matches!(action, KeyAction::BackTab) && !app.queued_submissions.is_empty() {
        app.submission_focus = true;
        app.selected_submission = app
            .selected_submission
            .min(app.queued_submissions.len().saturating_sub(1));
        app.popup.close();
        return;
    }
    app.wm.sync_modals();
    if let KeyAction::OpenCommandPalette = action {
        app.wm.modals.palette.open();
        return;
    }
    if let KeyAction::OpenProjectHub = action {
        let canvas = app
            .last_full_rect
            .or(app.last_transcript_rect)
            .unwrap_or_default();
        let session = app.session.clone();
        app.wm
            .open_project_hub(canvas, app.projects.clone(), session.as_deref());
        return;
    }
    let startup_recent_len = app.items.first().and_then(|item| match item {
        crate::app::OutputItem::StartupCard { recent, .. } => {
            Some(recent.len().min(app.startup_session_rects.len()))
        }
        _ => None,
    });
    let mut startup_released_to_input = false;
    if app.startup_intro.is_none() {
        match (app.startup_focus, startup_recent_len) {
            (crate::app::StartupFocus::Recent, Some(len)) if len > 0 => match &action {
                KeyAction::Tab | KeyAction::Escape => {
                    app.startup_focus = crate::app::StartupFocus::Input;
                    app.startup_last_click = None;
                    return;
                }
                KeyAction::BackTab => return,
                KeyAction::HistoryUp => {
                    app.startup_selected_session = app.startup_selected_session.saturating_sub(1);
                    return;
                }
                KeyAction::HistoryDown => {
                    app.startup_selected_session =
                        (app.startup_selected_session + 1).min(len.saturating_sub(1));
                    return;
                }
                KeyAction::Char(c) if c.is_ascii_digit() && *c != '0' => {
                    let index = (*c as usize) - ('1' as usize);
                    if index < len {
                        app.startup_selected_session = index;
                    }
                    return;
                }
                KeyAction::Submit => {
                    let index = app.startup_selected_session.min(len.saturating_sub(1));
                    let session_id = app.items.first().and_then(|item| match item {
                        crate::app::OutputItem::StartupCard { recent, .. } => {
                            recent.get(index).map(|entry| entry.session_id.clone())
                        }
                        _ => None,
                    });
                    if let Some(session_id) = session_id {
                        request_session_switch(app, control_tx, session_id);
                    }
                    return;
                }
                KeyAction::Char(_) => {
                    app.startup_focus = crate::app::StartupFocus::Input;
                    app.startup_last_click = None;
                    startup_released_to_input = true;
                }
                _ => return,
            },
            (crate::app::StartupFocus::Input, Some(len))
                if len > 0 && matches!(action, KeyAction::BackTab) =>
            {
                app.startup_focus = crate::app::StartupFocus::Recent;
                app.startup_selected_session = app.startup_selected_session.min(len - 1);
                app.startup_last_click = None;
                app.popup.close();
                return;
            }
            (crate::app::StartupFocus::Recent, _) => {
                app.startup_focus = crate::app::StartupFocus::Input;
                app.startup_last_click = None;
            }
            _ => {}
        }
    }
    if matches!(action, KeyAction::Char('x'))
        && !startup_released_to_input
        && startup_recent_len.is_some()
        && !app.hints_dismissed
    {
        app.hints_dismissed = true;
        app.save_ui_state();
        return;
    }
    if startup_recent_len.is_some()
        && matches!(action, KeyAction::Submit)
        && !editor.buf().trim().is_empty()
        && app.startup_intro.is_none()
        && !app.popup.is_open()
    {
        let (version, recent) = match app.items.first() {
            Some(crate::app::OutputItem::StartupCard {
                version, recent, ..
            }) => (version.clone(), recent.clone()),
            _ => (String::new(), Vec::new()),
        };
        let _ = app.remove_item(0);
        app.inline_note_indices.clear();
        app.startup_focus = crate::app::StartupFocus::Input;
        app.startup_hovered_session = None;
        app.startup_container_rect = None;
        app.startup_projects_rect = None;
        app.startup_projects_hovered = false;
        app.startup_session_rects.clear();
        app.startup_last_click = None;
        app.startup_intro = Some(crate::app::StartupIntro {
            started_at: std::time::Instant::now(),
            version,
            recent,
        });
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
            KeyAction::Tab | KeyAction::Submit => {
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
    if (!app.pending_permissions.is_empty() || !app.pending_permission_groups.is_empty())
        && is_approval_key(&action)
    {
        handle_approval_key(&action, app, control_tx);
        return;
    }
    if app.yank_mode && handle_yank_key(&action, app) {
        return;
    }
    let mut edited = false;
    match action {
        KeyAction::ToggleQuote => {
            if app.pending_quote.is_some() {
                app.quote_expanded = !app.quote_expanded;
                app.quote_scroll = 0;
            }
            return;
        }
        KeyAction::PageUp
            if app.pending_quote.is_some() && app.quote_expanded && app.quote_rect.is_some() =>
        {
            app.quote_scroll = app.quote_scroll.saturating_sub(4);
            return;
        }
        KeyAction::PageDown
            if app.pending_quote.is_some() && app.quote_expanded && app.quote_rect.is_some() =>
        {
            let max_scroll = crate::render::quote_card_max_scroll(
                app.pending_quote.as_deref().unwrap_or_default(),
                app.quote_rect.expect("quote card is visible"),
            );
            app.quote_scroll = app.quote_scroll.saturating_add(4).min(max_scroll);
            return;
        }
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
            if app.pending_quote.take().is_some() {
                app.quote_expanded = false;
                app.quote_scroll = 0;
                app.push_note("removed quoted selection", app::NoteLevel::Info);
                *interrupt_prompt = None;
                return;
            }
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
            if input_has_focus(app) {
                let reconciled = app.reconcile_input_reasoning();
                let cycled = app.cycle_input_reasoning();
                if reconciled || cycled {
                    app.save_ui_state();
                }
                let reasoning = app
                    .input_reasoning
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_else(|| "model default".into());
                app.push_note(
                    format!("input reasoning: {reasoning}"),
                    app::NoteLevel::Info,
                );
            }
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
            let editor_submission = editor.submit_with_images().or_else(|| {
                app.pending_quote
                    .as_ref()
                    .map(|_| crate::input::EditorSubmission {
                        text: String::new(),
                        images: Vec::new(),
                    })
            });
            if let Some(editor_submission) = editor_submission {
                if app.reconcile_input_reasoning() {
                    app.save_ui_state();
                }
                let quote = app.pending_quote.take();
                let presentation =
                    quote
                        .as_ref()
                        .map(|quote| atman_runtime::user_input::UserInputPresentation {
                            prompt: editor_submission.text.clone(),
                            quote: Some(atman_runtime::user_input::QuoteSnapshot {
                                text: quote.clone(),
                            }),
                        });
                let line = presentation.as_ref().map_or_else(
                    || editor_submission.text.clone(),
                    |value| value.model_text(),
                );
                if !app.has_running_workflow() {
                    app.push_user_turn_with_presentation(line.clone(), presentation.clone());
                }
                if control_tx.is_some() || submit_tx.is_some() {
                    let images = if line.trim_start().starts_with(':') {
                        Vec::new()
                    } else {
                        app.session
                            .as_ref()
                            .map(|session| session.take_pending_images())
                            .unwrap_or_default()
                    };
                    let submission = crate::TuiSubmission {
                        text: line,
                        images,
                        reasoning: app.input_reasoning_for_submission(),
                        presentation,
                    };
                    let failed = if let Some(tx) = control_tx {
                        tx.send(TuiControl::Submit(submission)).err().and_then(
                            |error| match error.0 {
                                TuiControl::Submit(submission) => Some(submission),
                                _ => None,
                            },
                        )
                    } else {
                        submit_tx
                            .and_then(|tx| tx.send(submission).err())
                            .map(|error| error.0)
                    };
                    if let Some(failed) = failed {
                        app.pending_quote = quote;
                        if let Some(session) = app.session.clone() {
                            app.attach_count = session.restore_pending_images(failed.images);
                            editor.reconcile_images(&session.pending_images());
                        }
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
        KeyAction::ToggleLastWork => {
            app.toggle_latest_work_fold();
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
            if !input_has_focus(app) {
                *interrupt_prompt = None;
            } else if !editor.buf().is_empty() {
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
        KeyAction::MoveItemUp | KeyAction::MoveItemDown => {}
        KeyAction::CyclePanelForward | KeyAction::CyclePanelBackward => {}
        KeyAction::OpenProjectHub => {}
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

pub(crate) fn input_has_focus(app: &UiState) -> bool {
    app.wm.focused_id().is_none()
        && !app.wm.any_modal_open()
        && app.modal_notification.is_none()
        && !app.submission_focus
}

pub(crate) fn dispatch_submission_queue_action(
    app: &mut AppState,
    index: usize,
    action: crate::submission_queue::QueueAction,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) {
    let Some(submission) = app.queued_submissions.get(index).cloned() else {
        return;
    };
    app.selected_submission = index;
    app.submission_focus = true;
    match action {
        crate::submission_queue::QueueAction::Insert => {
            let reason = submission.insert_block_reason.clone().or_else(|| {
                app.session
                    .as_ref()
                    .and_then(|session| session.current_turn())
                    .is_none()
                    .then(|| "no active flow".to_owned())
            });
            if let Some(reason) = reason {
                app.push_toast(
                    reason,
                    app::NoteLevel::Warn,
                    std::time::Duration::from_secs(3),
                    app::ToastPosition::TopRight,
                );
                return;
            }
            if let Some(tx) = control_tx {
                let _ = tx.send(TuiControl::InsertQueuedSubmission {
                    id: submission.id,
                    expected_revision: submission.revision,
                });
            }
        }
        crate::submission_queue::QueueAction::Edit => {
            let mut editor = InputEditor::default();
            editor.replace_with(&submission.text);
            app.queued_submission_edit = Some(crate::app::QueuedSubmissionEdit {
                id: submission.id,
                revision: submission.revision,
                editor,
            });
        }
        crate::submission_queue::QueueAction::MoveUp
        | crate::submission_queue::QueueAction::MoveDown => {
            let direction = if matches!(action, crate::submission_queue::QueueAction::MoveUp) {
                atman_runtime::SubmissionMove::Up
            } else {
                atman_runtime::SubmissionMove::Down
            };
            if let Some(tx) = control_tx {
                let _ = tx.send(TuiControl::MoveQueuedSubmission {
                    id: submission.id,
                    expected_revision: submission.revision,
                    direction,
                });
            }
        }
        crate::submission_queue::QueueAction::Delete => {
            if let Some(tx) = control_tx {
                let _ = tx.send(TuiControl::DeleteQueuedSubmission {
                    id: submission.id,
                    expected_revision: submission.revision,
                });
            }
        }
    }
}

fn handle_submission_queue_key(
    action: &KeyAction,
    app: &mut AppState,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
) {
    if let Some(edit) = app.queued_submission_edit.as_mut() {
        match action {
            KeyAction::Submit => {
                let text = edit.editor.buf().trim().to_owned();
                if text.is_empty()
                    && !app.queued_submissions.iter().any(|submission| {
                        submission.id == edit.id
                            && submission
                                .presentation
                                .as_ref()
                                .and_then(|value| value.quote.as_ref())
                                .is_some()
                    })
                {
                    app.push_toast(
                        "queued message cannot be empty",
                        app::NoteLevel::Warn,
                        std::time::Duration::from_secs(3),
                        app::ToastPosition::TopRight,
                    );
                } else if let Some(tx) = control_tx {
                    let _ = tx.send(TuiControl::EditQueuedSubmission {
                        id: edit.id.clone(),
                        expected_revision: edit.revision,
                        text,
                    });
                    app.queued_submission_edit = None;
                }
            }
            KeyAction::Escape => app.queued_submission_edit = None,
            KeyAction::Tab | KeyAction::BackTab => {
                app.queued_submission_edit = None;
                app.submission_focus = false;
            }
            _ => {
                edit.editor.handle_key(action);
            }
        }
        return;
    }
    match action {
        KeyAction::Tab | KeyAction::BackTab | KeyAction::Escape => {
            app.submission_focus = false;
        }
        KeyAction::HistoryUp => {
            app.selected_submission = app.selected_submission.saturating_sub(1);
        }
        KeyAction::HistoryDown => {
            app.selected_submission =
                (app.selected_submission + 1).min(app.queued_submissions.len().saturating_sub(1));
        }
        KeyAction::MoveItemUp => dispatch_submission_queue_action(
            app,
            app.selected_submission,
            crate::submission_queue::QueueAction::MoveUp,
            control_tx,
        ),
        KeyAction::MoveItemDown => dispatch_submission_queue_action(
            app,
            app.selected_submission,
            crate::submission_queue::QueueAction::MoveDown,
            control_tx,
        ),
        KeyAction::Submit => dispatch_submission_queue_action(
            app,
            app.selected_submission,
            crate::submission_queue::QueueAction::Edit,
            control_tx,
        ),
        KeyAction::Char('e') => dispatch_submission_queue_action(
            app,
            app.selected_submission,
            crate::submission_queue::QueueAction::Edit,
            control_tx,
        ),
        KeyAction::Char('i') => dispatch_submission_queue_action(
            app,
            app.selected_submission,
            crate::submission_queue::QueueAction::Insert,
            control_tx,
        ),
        KeyAction::Delete | KeyAction::Backspace => dispatch_submission_queue_action(
            app,
            app.selected_submission,
            crate::submission_queue::QueueAction::Delete,
            control_tx,
        ),
        _ => {}
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
            project_root: None,
        });
    }
    app.startup_focus = crate::app::StartupFocus::Input;
    app.startup_hovered_session = None;
    app.startup_container_rect = None;
    app.startup_session_rects.clear();
    app.startup_last_click = None;
    app.should_quit = true;
}

pub(crate) fn request_project_session_switch(
    app: &mut AppState,
    control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
    sid: String,
    project_root: std::path::PathBuf,
) {
    let intro = crate::app::StartupIntro {
        started_at: std::time::Instant::now(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        recent: Vec::new(),
    };
    if let Some(tx) = control_tx {
        let _ = tx.send(TuiControl::SwitchSession {
            sid,
            intro,
            project_root: Some(project_root),
        });
    }
    app.startup_focus = crate::app::StartupFocus::Input;
    app.startup_hovered_session = None;
    app.startup_container_rect = None;
    app.startup_session_rects.clear();
    app.startup_last_click = None;
    app.should_quit = true;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history_search_modal::extract_event_text;

    struct ModelConfigReset;

    impl Drop for ModelConfigReset {
        fn drop(&mut self) {
            atman_runtime::model_registry::set_provider_config(Default::default());
        }
    }

    const PNG_BYTES: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d,
    ];

    fn startup_state() -> crate::UiState {
        let recent = (1..=3)
            .map(|index| crate::app::StartupSessionEntry {
                session_id: format!("session-{index}"),
                short_id: format!("session{index}"),
                goal: Some(format!("goal {index}")),
                project: Some("project".into()),
                age_label: "1m ago".into(),
                event_count: index,
            })
            .collect();
        let mut state = crate::UiState::new(
            AppState::new("current".into(), None).with_initial_items(vec![
                crate::app::OutputItem::StartupCard {
                    version: "1.0.0".into(),
                    recent,
                },
            ]),
        );
        state.app.startup_session_rects = (0..3)
            .map(|index| ratatui::layout::Rect::new(10, 20 + index * 4, 40, 4))
            .collect();
        state
    }

    fn press_startup_key(
        state: &mut crate::UiState,
        editor: &mut InputEditor,
        action: KeyAction,
        control_tx: Option<&mpsc::UnboundedSender<TuiControl>>,
    ) {
        let mut interrupt_prompt = None;
        handle_key(
            action,
            state,
            editor,
            &mut interrupt_prompt,
            None,
            control_tx,
        );
    }

    #[test]
    fn startup_input_keeps_digits_until_recent_is_focused() {
        let mut state = startup_state();
        let mut editor = InputEditor::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        press_startup_key(&mut state, &mut editor, KeyAction::Char('2'), Some(&tx));

        assert_eq!(editor.buf(), "2");
        assert_eq!(state.app.startup_focus, crate::app::StartupFocus::Input);
        assert!(!state.app.should_quit);
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn startup_ctrl_l_opens_maximized_project_hub() {
        let mut state = startup_state();
        state.app.last_full_rect = Some(ratatui::layout::Rect::new(0, 0, 120, 40));
        let mut editor = InputEditor::default();

        press_startup_key(&mut state, &mut editor, KeyAction::OpenProjectHub, None);

        let panel = state
            .wm
            .panels
            .iter()
            .find(|panel| panel.content_key == crate::wm::ContentKey::Projects)
            .expect("project hub panel");
        assert!(panel.maximized);
    }

    #[test]
    fn startup_recent_focus_selects_before_enter_switches() {
        let mut state = startup_state();
        let mut editor = InputEditor::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        press_startup_key(&mut state, &mut editor, KeyAction::BackTab, Some(&tx));
        assert_eq!(state.app.startup_focus, crate::app::StartupFocus::Recent);

        press_startup_key(&mut state, &mut editor, KeyAction::Char('3'), Some(&tx));
        assert_eq!(state.app.startup_selected_session, 2);
        assert!(rx.try_recv().is_err());

        press_startup_key(&mut state, &mut editor, KeyAction::HistoryUp, Some(&tx));
        assert_eq!(state.app.startup_selected_session, 1);
        press_startup_key(&mut state, &mut editor, KeyAction::HistoryDown, Some(&tx));
        assert_eq!(state.app.startup_selected_session, 2);
        assert!(rx.try_recv().is_err());

        press_startup_key(&mut state, &mut editor, KeyAction::Submit, Some(&tx));
        assert!(state.app.should_quit);
        assert!(matches!(
            rx.try_recv(),
            Ok(TuiControl::SwitchSession { sid, .. }) if sid == "session-3"
        ));
    }

    #[test]
    fn startup_recent_focus_returns_to_input_without_losing_chars() {
        for exit in [KeyAction::Tab, KeyAction::Escape] {
            let mut state = startup_state();
            let mut editor = InputEditor::default();
            press_startup_key(&mut state, &mut editor, KeyAction::BackTab, None);
            press_startup_key(&mut state, &mut editor, exit, None);
            assert_eq!(state.app.startup_focus, crate::app::StartupFocus::Input);
        }

        let mut state = startup_state();
        let mut editor = InputEditor::default();
        press_startup_key(&mut state, &mut editor, KeyAction::BackTab, None);
        press_startup_key(&mut state, &mut editor, KeyAction::Char('q'), None);
        assert_eq!(state.app.startup_focus, crate::app::StartupFocus::Input);
        assert_eq!(editor.buf(), "q");
    }

    #[test]
    fn startup_recent_selection_is_limited_to_rendered_rows() {
        let mut state = startup_state();
        state.app.startup_session_rects.truncate(1);
        let mut editor = InputEditor::default();
        press_startup_key(&mut state, &mut editor, KeyAction::BackTab, None);
        press_startup_key(&mut state, &mut editor, KeyAction::Char('3'), None);
        press_startup_key(&mut state, &mut editor, KeyAction::HistoryDown, None);
        assert_eq!(state.app.startup_selected_session, 0);
    }

    #[test]
    fn enter_accepts_colon_and_slash_completions_without_submitting() {
        for (partial, completed, flows) in [
            (":he", ":help ", Vec::new()),
            (
                "/re",
                "/review ",
                vec![("review".into(), "review code".into())],
            ),
        ] {
            let app = AppState::new("session".into(), None).with_flow_names(flows);
            let mut state = crate::UiState::new(app);
            let mut editor = InputEditor::default();
            editor.replace_with(partial);
            state.app.refresh_popup(editor.buf());
            assert!(state.app.popup.is_open());
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

            press_startup_key(&mut state, &mut editor, KeyAction::Submit, Some(&tx));

            assert_eq!(editor.buf(), completed);
            assert!(!state.app.popup.is_open());
            assert!(rx.try_recv().is_err());
        }
    }

    #[test]
    fn empty_input_exits_on_second_interrupt() {
        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        let mut editor = InputEditor::default();
        let mut interrupt_prompt = None;

        handle_key(
            KeyAction::Interrupt,
            &mut state,
            &mut editor,
            &mut interrupt_prompt,
            None,
            None,
        );
        assert!(interrupt_prompt.is_some());
        assert!(!state.app.should_quit);

        handle_key(
            KeyAction::Interrupt,
            &mut state,
            &mut editor,
            &mut interrupt_prompt,
            None,
            None,
        );
        assert!(state.app.should_quit);
    }

    #[test]
    fn interrupt_is_inert_while_floating_panel_has_focus() {
        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        state.wm.open(
            "cheatsheet",
            crate::wm::ContentKey::Cheatsheet,
            crate::wm::WindowContent::Cheatsheet,
            "Keybindings",
            ratatui::layout::Rect::new(0, 0, 80, 24),
        );
        let mut editor = InputEditor::default();
        let mut interrupt_prompt = None;

        handle_key(
            KeyAction::Interrupt,
            &mut state,
            &mut editor,
            &mut interrupt_prompt,
            None,
            None,
        );

        assert!(interrupt_prompt.is_none());
        assert!(!state.app.should_quit);
    }

    #[test]
    fn reasoning_cycle_is_inert_while_floating_panel_has_focus() {
        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        state.app.input_reasoning = Some(atman_runtime::provider::ReasoningSelection::Auto {
            execution_mode: None,
        });
        state.wm.open(
            "cheatsheet",
            crate::wm::ContentKey::Cheatsheet,
            crate::wm::WindowContent::Cheatsheet,
            "Keybindings",
            ratatui::layout::Rect::new(0, 0, 80, 24),
        );
        let mut editor = InputEditor::default();
        let mut interrupt_prompt = None;

        handle_key(
            KeyAction::CycleReasoning,
            &mut state,
            &mut editor,
            &mut interrupt_prompt,
            None,
            None,
        );

        assert_eq!(
            state.app.input_reasoning,
            Some(atman_runtime::provider::ReasoningSelection::Auto {
                execution_mode: None,
            })
        );
    }

    #[test]
    fn modal_state_is_not_input_focus() {
        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        state.wm.modals.theme_picker_open = true;
        assert!(!input_has_focus(&state));
    }

    #[test]
    fn pending_model_switch_blocks_submission_routing() {
        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        state.wm.modals.model_picker.open = true;
        state
            .wm
            .modals
            .model_picker
            .begin_switch("provider:model".into())
            .unwrap();
        let mut editor = InputEditor::default();
        editor.insert_str("do not submit yet");
        let (submit_tx, mut submit_rx) = mpsc::unbounded_channel();
        let (control_tx, _control_rx) = mpsc::unbounded_channel();
        let mut interrupt_prompt = None;

        let (escape_consumed, commands) =
            state
                .wm
                .dispatch_key(&KeyAction::Escape, &mut state.app, Some(&control_tx));
        state
            .wm
            .apply_commands(&mut state.app, commands, Some(&control_tx));
        assert!(escape_consumed);
        assert!(state.wm.modals.model_picker.open);

        let (consumed, commands) =
            state
                .wm
                .dispatch_key(&KeyAction::Submit, &mut state.app, Some(&control_tx));
        state
            .wm
            .apply_commands(&mut state.app, commands, Some(&control_tx));
        if !consumed {
            handle_key(
                KeyAction::Submit,
                &mut state,
                &mut editor,
                &mut interrupt_prompt,
                Some(&submit_tx),
                Some(&control_tx),
            );
        }

        assert!(consumed);
        assert!(submit_rx.try_recv().is_err());
        assert_eq!(editor.buf(), "do not submit yet");
        assert!(state.wm.modals.model_picker.open);
        assert!(state.wm.modals.model_picker.is_pending());
    }

    #[test]
    fn submission_uses_control_channel_when_available() {
        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        let mut editor = InputEditor::default();
        editor.insert_str("run after mode update");
        let (submit_tx, mut submit_rx) = mpsc::unbounded_channel();
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        let mut interrupt_prompt = None;

        handle_key(
            KeyAction::Submit,
            &mut state,
            &mut editor,
            &mut interrupt_prompt,
            Some(&submit_tx),
            Some(&control_tx),
        );

        assert!(submit_rx.try_recv().is_err());
        let TuiControl::Submit(submission) = control_rx.try_recv().unwrap() else {
            panic!("submission must share the ordered control channel");
        };
        assert_eq!(submission.text, "run after mode update");
    }

    #[test]
    fn submission_restores_pending_quote_as_markdown() {
        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        state.app.pending_quote = Some("quoted line".into());
        let mut editor = InputEditor::default();
        editor.insert_str("follow up");
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        let mut interrupt_prompt = None;

        handle_key(
            KeyAction::Submit,
            &mut state,
            &mut editor,
            &mut interrupt_prompt,
            None,
            Some(&control_tx),
        );

        let TuiControl::Submit(submission) = control_rx.try_recv().unwrap() else {
            panic!("quote submission must be sent");
        };
        assert_eq!(submission.text, "> quoted line\n\nfollow up");
        assert!(state.app.pending_quote.is_none());
    }

    #[test]
    fn quote_only_submission_is_not_dropped() {
        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        state.app.pending_quote = Some("quoted line".into());
        let mut editor = InputEditor::default();
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        let mut interrupt_prompt = None;

        handle_key(
            KeyAction::Submit,
            &mut state,
            &mut editor,
            &mut interrupt_prompt,
            None,
            Some(&control_tx),
        );

        let TuiControl::Submit(submission) = control_rx.try_recv().unwrap() else {
            panic!("quote-only submission must be sent");
        };
        assert_eq!(submission.text, "> quoted line\n\n");
    }

    #[test]
    fn trust_update_precedes_the_following_submission() {
        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        let (submit_tx, mut submit_rx) = mpsc::unbounded_channel();
        let (control_tx, mut control_rx) = mpsc::unbounded_channel();
        state.wm.modals.open_trust_mode_picker(&mut state.app);
        state.app.picker_selected = atman_runtime::trust::TrustMode::all()
            .iter()
            .position(|mode| *mode == atman_runtime::trust::TrustMode::Reckless)
            .unwrap();

        let (consumed, commands) =
            state
                .wm
                .dispatch_key(&KeyAction::Submit, &mut state.app, Some(&control_tx));
        state
            .wm
            .apply_commands(&mut state.app, commands, Some(&control_tx));
        assert!(consumed);

        let mut editor = InputEditor::default();
        editor.insert_str("start the new flow");
        let mut interrupt_prompt = None;
        handle_key(
            KeyAction::Submit,
            &mut state,
            &mut editor,
            &mut interrupt_prompt,
            Some(&submit_tx),
            Some(&control_tx),
        );

        let TuiControl::UpdateTrust(trust) = control_rx.try_recv().unwrap() else {
            panic!("trust update must be queued first");
        };
        assert_eq!(trust.mode, atman_runtime::trust::TrustMode::Reckless);
        let TuiControl::Submit(submission) = control_rx.try_recv().unwrap() else {
            panic!("submission must follow the trust update");
        };
        assert_eq!(submission.text, "start the new flow");
        assert!(submit_rx.try_recv().is_err());
    }

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
                call_intent: None,
                tier: atman_runtime::tool::Tier::Two,
                execution_boundary: Default::default(),
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
    fn pending_approval_intercepts_escape_before_focused_flow_panel() {
        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        let pending = pending_permission(1);
        state
            .app
            .pending_permissions
            .insert(pending.request_id.clone(), pending);
        state.wm.open(
            "child",
            crate::wm::ContentKey::Task("child".into()),
            crate::wm::WindowContent::Task {
                handle: "child".into(),
                kind: atman_runtime::TaskKind::Flow,
            },
            "child",
            ratatui::layout::Rect::new(0, 0, 80, 24),
        );

        let (consumed, commands) = state
            .wm
            .dispatch_key(&KeyAction::Escape, &mut state.app, None);

        assert!(!consumed, "approval handling must receive Escape first");
        assert!(commands.is_empty());
        assert_eq!(state.wm.panels.len(), 1, "the flow panel must stay open");
    }

    #[test]
    fn submit_captures_the_current_composer_snapshot() {
        let session = std::sync::Arc::new(atman_runtime::Session::open_ephemeral());
        session
            .queue_image_bytes(PNG_BYTES, Some("first.png"))
            .unwrap();
        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        state.app.session = Some(session.clone());
        state.app.attach_count = 1;
        state.app.input_reasoning = Some(atman_runtime::provider::ReasoningSelection::Disabled);
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
        assert_eq!(
            submission.reasoning,
            Some(atman_runtime::provider::ReasoningSelection::Disabled)
        );
        assert_eq!(
            state.app.input_reasoning,
            Some(atman_runtime::provider::ReasoningSelection::Disabled)
        );
        assert_eq!(session.pending_image_count(), 1);
    }

    #[test]
    fn model_default_submission_captures_effective_reasoning() {
        use atman_runtime::model_registry::{
            AliasEntry, ModelEntry, ProviderConfig, ProviderEntry,
        };
        use atman_runtime::provider::{ReasoningEffort, ReasoningSelection};
        use atman_runtime::providers::openai::OpenAiReasoningFormat;

        let _lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _reset = ModelConfigReset;
        atman_runtime::model_registry::set_provider_config(ProviderConfig {
            providers: std::collections::HashMap::from([(
                "gateway".into(),
                ProviderEntry {
                    kind: "openai".into(),
                    reasoning_format: Some(OpenAiReasoningFormat::Official),
                    ..Default::default()
                },
            )]),
            models: std::collections::HashMap::from([(
                "reasoning-model".into(),
                ModelEntry {
                    model: "vendor/reasoning-model".into(),
                    provider: Some("gateway".into()),
                    context_budget: Some(128_000),
                    reasoning: Some("ultra".into()),
                    ..Default::default()
                },
            )]),
            aliases: std::collections::HashMap::from([(
                "smart".into(),
                AliasEntry {
                    model: "reasoning-model".into(),
                },
            )]),
        });

        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        assert_eq!(state.app.input_reasoning, None);
        assert_eq!(
            state.app.effective_input_reasoning_badge().as_deref(),
            Some("ultra")
        );
        let mut editor = InputEditor::default();
        editor.insert_str("first request after restart");
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

        assert_eq!(
            rx.try_recv().unwrap().reasoning,
            Some(ReasoningSelection::Effort {
                effort: ReasoningEffort::Ultra,
                execution_mode: None,
            })
        );
        assert_eq!(state.app.input_reasoning, None);
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
        let transcript_len = app.items.len();

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
        assert_eq!(app.items.len(), transcript_len);
        assert_eq!(app.toasts.len(), 1);

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
    fn next_queue_enter_edits_without_cancelling_the_flow() {
        let session = atman_runtime::Session::open_ephemeral();
        session
            .enqueue_submission(
                "urgent next turn",
                Vec::new(),
                atman_runtime::InvocationEnv::default(),
                atman_runtime::message::MessageOrigin::User,
            )
            .unwrap();
        let cancel = session.flow_cancel_token();
        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        state.app.queued_submissions = session.queued_submissions();
        let mut editor = InputEditor::default();
        let mut interrupt_prompt = None;
        let (tx, mut rx) = mpsc::unbounded_channel();

        handle_key(
            KeyAction::BackTab,
            &mut state,
            &mut editor,
            &mut interrupt_prompt,
            None,
            Some(&tx),
        );
        assert!(state.app.submission_focus);
        handle_key(
            KeyAction::Submit,
            &mut state,
            &mut editor,
            &mut interrupt_prompt,
            None,
            Some(&tx),
        );

        assert_eq!(
            state
                .app
                .queued_submission_edit
                .as_ref()
                .map(|edit| edit.editor.buf()),
            Some("urgent next turn")
        );
        assert!(rx.try_recv().is_err());
        assert!(!cancel.is_cancelled());
        assert!(editor.buf().is_empty());
    }

    #[test]
    fn next_queue_accepts_mac_delete_key_for_removal() {
        let session = atman_runtime::Session::open_ephemeral();
        let queued = session
            .enqueue_submission(
                "remove me",
                Vec::new(),
                atman_runtime::InvocationEnv::default(),
                atman_runtime::message::MessageOrigin::User,
            )
            .unwrap();
        let cancel = session.flow_cancel_token();
        let mut state = crate::UiState::new(AppState::new("session".into(), None));
        state.app.queued_submissions = session.queued_submissions();
        state.app.submission_focus = true;
        let mut editor = InputEditor::default();
        let mut interrupt_prompt = None;
        let (tx, mut rx) = mpsc::unbounded_channel();

        handle_key(
            KeyAction::Backspace,
            &mut state,
            &mut editor,
            &mut interrupt_prompt,
            None,
            Some(&tx),
        );

        let TuiControl::DeleteQueuedSubmission {
            id,
            expected_revision,
        } = rx.try_recv().expect("delete control")
        else {
            panic!("expected queued submission deletion");
        };
        assert_eq!(id, queued.id);
        assert_eq!(expected_revision, queued.revision);
        assert!(!cancel.is_cancelled());
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
