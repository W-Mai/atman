//! Current-user approval review; every decision captures the displayed authority.

use crate::app::{AppState, PendingPermission, PendingPermissionGroup};
use atman_runtime::permission::{
    GrantScope, PermissionAction, PermissionRequestId, PermissionSelector,
};
use ratatui::{
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalClick {
    Open,
    Close,
    Select(usize),
    Page(bool),
    Scroll(bool),
    Refresh,
    Scope(u8),
    Members,
    Audit,
    Decide(crate::approval::DecisionRequest),
    ApproveAll,
}

#[derive(Debug, Clone, Default)]
pub struct ApprovalHitMap(pub Vec<(Rect, ApprovalClick)>);

impl ApprovalHitMap {
    pub fn at(&self, x: u16, y: u16) -> Option<ApprovalClick> {
        self.0
            .iter()
            .find_map(|(rect, action)| rect.contains((x, y).into()).then(|| action.clone()))
    }
}

#[derive(Clone)]
enum Reviewed {
    Request(Box<PendingPermission>),
    Group(PendingPermissionGroup, Vec<PendingPermission>),
}

impl Reviewed {
    fn ids(&self) -> Vec<PermissionRequestId> {
        match self {
            Self::Request(request) => vec![request.request_id.clone()],
            Self::Group(group, _) => group.payload.request_ids.clone(),
        }
    }
    fn current(&self, app: &AppState) -> bool {
        match self {
            Self::Request(request) => app
                .pending_permissions
                .get(&request.request_id)
                .is_some_and(|current| current.revision == request.revision),
            Self::Group(group, _) => app
                .pending_permission_groups
                .get(&group.group_id)
                .is_some_and(|current| {
                    current.revision == group.revision
                        && current.payload.request_ids == group.payload.request_ids
                }),
        }
    }
}

#[derive(Default)]
pub(crate) struct Review {
    pub open: bool,
    pub page: usize,
    page_size: usize,
    pub focused: bool,
    pub pointer: Option<(u16, u16)>,
    pub actions_visible: bool,
    canonical: std::collections::BTreeMap<PermissionRequestId, PendingPermission>,
    pub scroll: usize,
    pub members: bool,
    pub audit: bool,
    reviewed: Option<Reviewed>,
    pub requests: Vec<PendingPermission>,
    pub groups: Vec<PendingPermissionGroup>,
    pub numbered: Vec<PendingPermission>,
}

impl Review {
    pub fn capture(
        &mut self,
        app_requests: &std::collections::BTreeMap<PermissionRequestId, PendingPermission>,
        app_groups: &std::collections::BTreeMap<
            atman_runtime::permission::PermissionGroupId,
            PendingPermissionGroup,
        >,
        grouped: &std::collections::BTreeSet<PermissionRequestId>,
    ) {
        if !app_requests
            .values()
            .map(|request| (&request.request_id, request.revision))
            .eq(self
                .canonical
                .values()
                .map(|request| (&request.request_id, request.revision)))
        {
            self.canonical = app_requests.clone();
        }
        let page_size = self.page_size.max(1);
        let requests = app_requests
            .values()
            .filter(|request| !grouped.contains(&request.request_id));
        if !requests
            .clone()
            .map(|request| (&request.request_id, request.revision))
            .eq(self
                .requests
                .iter()
                .map(|request| (&request.request_id, request.revision)))
        {
            self.requests = requests.cloned().collect();
        }
        if !app_groups
            .values()
            .map(|group| (&group.group_id, group.revision, group.expanded))
            .eq(self
                .groups
                .iter()
                .map(|group| (&group.group_id, group.revision, group.expanded)))
        {
            self.groups = app_groups.values().cloned().collect();
        }
        self.page = self.page.min(self.len().saturating_sub(1) / page_size);
        let start = self.page * page_size;
        self.numbered = self
            .requests
            .iter()
            .enumerate()
            .filter(|(index, _)| {
                let row = index + self.groups.len();
                row >= start && row < start + page_size
            })
            .map(|(_, request)| request.clone())
            .collect();
    }

    fn len(&self) -> usize {
        self.groups.len() + self.requests.len()
    }

    fn entry(
        &self,
        index: usize,
        _canonical: &std::collections::BTreeMap<PermissionRequestId, PendingPermission>,
    ) -> Option<Reviewed> {
        if let Some(group) = self.groups.get(index) {
            return Some(Reviewed::Group(
                group.clone(),
                group
                    .payload
                    .request_ids
                    .iter()
                    .filter_map(|id| self.canonical.get(id).cloned())
                    .collect(),
            ));
        }
        self.requests
            .get(index.checked_sub(self.groups.len())?)
            .cloned()
            .map(|request| Reviewed::Request(Box::new(request)))
    }

    pub fn select_request(&mut self, request: &PendingPermission) {
        self.open = true;
        self.focused = true;
        self.scroll = 0;
        self.reviewed = Some(Reviewed::Request(Box::new(request.clone())));
    }

    pub fn select_group(
        &mut self,
        group: &PendingPermissionGroup,
        canonical: &std::collections::BTreeMap<PermissionRequestId, PendingPermission>,
    ) {
        self.open = true;
        self.focused = true;
        self.scroll = 0;
        self.reviewed = Some(Reviewed::Group(
            group.clone(),
            group
                .payload
                .request_ids
                .iter()
                .filter_map(|id| canonical.get(id).cloned())
                .collect(),
        ));
    }
}

pub(crate) fn height(app: &AppState) -> u16 {
    if app.approval_review.open {
        24
    } else if !app.pending_permissions.is_empty() || app.approval_decisions.is_waiting() {
        1
    } else {
        0
    }
}

pub(crate) fn owns_key(action: &crate::keys::KeyAction, app: &AppState) -> bool {
    use crate::keys::KeyAction::*;
    app.approval_review.open
        && app.approval_review.focused
        && matches!(
            action,
            Tab | BackTab
                | HistoryUp
                | HistoryDown
                | CursorLeft
                | CursorRight
                | PageUp
                | PageDown
                | Char('r' | 'R')
        )
}

pub(crate) fn handle_key(action: &crate::keys::KeyAction, app: &mut AppState) {
    use crate::keys::KeyAction::*;
    let click = match action {
        CursorLeft => ApprovalClick::Page(false),
        CursorRight => ApprovalClick::Page(true),
        PageUp => ApprovalClick::Scroll(false),
        PageDown => ApprovalClick::Scroll(true),
        Char('r' | 'R') => ApprovalClick::Refresh,
        _ => {
            let current = match app.approval_review.reviewed.as_ref() {
                Some(Reviewed::Request(request)) => app
                    .approval_review
                    .requests
                    .iter()
                    .position(|entry| entry.request_id == request.request_id)
                    .map(|index| index + app.approval_review.groups.len()),
                Some(Reviewed::Group(group, _)) => app
                    .approval_review
                    .groups
                    .iter()
                    .position(|entry| entry.group_id == group.group_id),
                None => None,
            }
            .unwrap_or(0);
            let len = app.approval_review.len().max(1);
            let next = if matches!(action, BackTab | HistoryUp) {
                current.checked_sub(1).unwrap_or(len - 1)
            } else {
                (current + 1) % len
            };
            app.approval_review.page = next / app.approval_review.page_size.max(1);
            ApprovalClick::Select(next)
        }
    };
    dispatch(click, app, None);
}

struct Rows {
    lines: Vec<Line<'static>>,
    hits: Vec<(usize, u16, u16, ApprovalClick)>,
    width: u16,
}

impl Rows {
    fn new(width: u16) -> Self {
        Self {
            lines: Vec::new(),
            hits: Vec::new(),
            width,
        }
    }
    fn line(&mut self, spans: Vec<Span<'static>>) {
        self.lines.push(Line::from(crate::width::truncate_spans(
            spans,
            self.width as usize,
            None,
        )));
    }
    fn button(&mut self, spans: &mut Vec<Span<'static>>, label: &str, action: ApprovalClick) {
        let x = crate::width::spans_width(spans.iter()) as u16;
        let end = x
            .saturating_add(crate::width::width(label) as u16)
            .min(self.width);
        if x < end {
            self.hits.push((self.lines.len(), x, end, action));
        }
        spans.push(Span::styled(
            label.to_owned(),
            Style::default()
                .fg(crate::theme::theme().accent.into())
                .add_modifier(Modifier::BOLD),
        ));
    }
    fn detail(&mut self, label: &str, text: &str) {
        let t = crate::theme::theme();
        for (index, line) in crate::width::word_wrap(
            &format!("{label}  {text}"),
            self.width.saturating_sub(2).max(1) as usize,
        )
        .into_iter()
        .enumerate()
        {
            self.line(vec![Span::styled(
                format!(" {line}"),
                Style::default().fg(if index == 0 {
                    t.tinted_fg
                } else {
                    t.work_meta_fg
                }
                .into()),
            )]);
        }
    }
}

fn request_details(rows: &mut Rows, request: &PendingPermission, audit: bool) {
    let payload = &request.payload;
    let provenance = &payload.provenance;
    let (title, _) = approval_labels(&payload.tool, payload.call_intent.as_ref());
    rows.detail(&title, &payload.tool);
    if let Some(root) = &provenance.workspace_root {
        rows.detail("Workspace", root);
    }
    if let Some(path) = &provenance.path {
        rows.detail("Path", path);
    }
    if let Some(cwd) = &provenance.cwd {
        rows.detail("Cwd", cwd);
    }
    for target in &provenance.targets {
        rows.detail("Target", target);
    }
    if provenance.risks.contains("ProcessSpawn") {
        rows.detail(
            "Execution",
            match payload.execution_boundary {
                Some(atman_runtime::permission::ExecutionBoundary::Direct) => "Direct",
                Some(atman_runtime::permission::ExecutionBoundary::Sandboxed) => "Sandboxed",
                None => "Not executed",
            },
        );
    }
    if !provenance.risks.is_empty() {
        rows.detail(
            "Risks",
            &provenance
                .risks
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        );
    }
    if provenance.network {
        rows.detail("Network", "requested");
    }
    if let Some(reason) = &payload.reason {
        rows.detail("Reason", reason);
    }
    if audit {
        rows.detail(
            "Request",
            &format!("{} · revision {}", request.request_id, request.revision),
        );
        rows.detail("Run", &payload.requesting_run_id.to_string());
        rows.detail("Call", &payload.tool_use_id);
        rows.detail(
            "Policy",
            &format!(
                "{} / {}",
                payload.policy.snapshot_id, payload.policy.rule_id
            ),
        );
        if let Some(actor) = &payload.actor {
            rows.detail("Actor", &format!("{actor:?}"));
        }
        if let Some(scope) = &payload.scope {
            rows.detail("Scope", &format!("{scope:?}"));
        }
    }
}

fn decision(
    reviewed: &Reviewed,
    action: PermissionAction,
    scope_index: u8,
) -> Option<crate::approval::DecisionRequest> {
    let (selector, expected_revision, grant_scope) = match reviewed {
        Reviewed::Request(request) => (
            PermissionSelector::RequestIds(vec![request.request_id.clone()]),
            request.revision,
            if action == PermissionAction::Approve {
                Some(crate::approval::grant_scope(request, scope_index)?)
            } else {
                None
            },
        ),
        Reviewed::Group(group, _) => (
            PermissionSelector::Group(group.group_id.clone()),
            group.revision,
            (action == PermissionAction::Approve).then_some(GrantScope::CurrentCall),
        ),
    };
    Some(crate::approval::DecisionRequest {
        id: uuid::Uuid::now_v7(),
        selector,
        expected_revision,
        action,
        grant_scope,
        reason: (action == PermissionAction::Deny).then(|| "denied by user".into()),
    })
}

pub(crate) fn dispatch(
    click: ApprovalClick,
    app: &mut AppState,
    sender: Option<&crate::ControlSender>,
) {
    app.approval_review.focused = true;
    match click {
        ApprovalClick::Open => app.approval_review.open = true,
        ApprovalClick::Close => {
            app.approval_review.open = false;
            app.approval_review.focused = false;
        }
        ApprovalClick::Select(index) => {
            if let Some(group) = app.approval_review.groups.get(index) {
                app.selected_permission_group = Some(group.group_id.clone());
            }
            app.approval_review.reviewed =
                app.approval_review.entry(index, &app.pending_permissions);
            app.approval_review.scroll = 0;
        }
        ApprovalClick::Page(next) => {
            if next {
                app.approval_review.page += 1;
            } else {
                app.approval_review.page = app.approval_review.page.saturating_sub(1);
            }
        }
        ApprovalClick::Scroll(down) => {
            if down {
                app.approval_review.scroll = app.approval_review.scroll.saturating_add(3);
            } else {
                app.approval_review.scroll = app.approval_review.scroll.saturating_sub(3);
            }
        }
        ApprovalClick::Refresh => {
            app.approval_review.capture(
                &app.pending_permissions,
                &app.pending_permission_groups,
                &app.grouped_permission_request_ids,
            );
            let fresh = match app.approval_review.reviewed.as_ref() {
                Some(Reviewed::Request(request)) => app
                    .pending_permissions
                    .get(&request.request_id)
                    .cloned()
                    .map(|request| Reviewed::Request(Box::new(request))),
                Some(Reviewed::Group(group, _)) => app
                    .approval_review
                    .groups
                    .iter()
                    .position(|current| current.group_id == group.group_id)
                    .and_then(|index| app.approval_review.entry(index, &app.pending_permissions)),
                None => app.approval_review.entry(0, &app.pending_permissions),
            };
            if fresh.is_some() {
                app.approval_review.reviewed = fresh;
            }
        }
        ApprovalClick::Scope(index) => app.approval_scope_index = index,
        ApprovalClick::Members => app.approval_review.members = !app.approval_review.members,
        ApprovalClick::Audit => app.approval_review.audit = !app.approval_review.audit,
        ApprovalClick::Decide(request) => {
            if let Some(sender) = sender {
                app.submit_permission(sender, request);
            }
        }
        ApprovalClick::ApproveAll => {
            crate::key_handler::handle_approval_key(
                &crate::keys::KeyAction::Char('a'),
                app,
                sender,
            );
        }
    }
    app.mark_visual_dirty();
}

pub fn render(frame: &mut ratatui::Frame, area: Rect, app: &mut AppState) -> ApprovalHitMap {
    app.approval_review.actions_visible = area.width >= 32 && area.height >= 10;
    app.approval_review.page_size = if app.approval_review.open {
        (area.height as usize).saturating_sub(12).clamp(1, 9)
    } else {
        9
    };
    let page_size = app.approval_review.page_size;
    app.approval_review.capture(
        &app.pending_permissions,
        &app.pending_permission_groups,
        &app.grouped_permission_request_ids,
    );
    let mut rows = Rows::new(area.width);
    let t = crate::theme::theme();
    let mut header = vec![Span::styled(
        format!(" ◷ {} ", app.pending_permissions.len()),
        Style::default()
            .fg(t.warn.into())
            .add_modifier(Modifier::BOLD),
    )];
    if !app.approval_review.open {
        rows.button(&mut header, " ↗ ", ApprovalClick::Open);
        if app.approval_decisions.is_waiting() {
            header.push(Span::styled(
                " …",
                Style::default().fg(t.work_meta_fg.into()),
            ));
        }
        rows.line(header);
    } else {
        rows.button(&mut header, " × ", ApprovalClick::Close);
        rows.button(&mut header, " ↻ ", ApprovalClick::Refresh);
        rows.button(&mut header, " ‹ ", ApprovalClick::Page(false));
        header.push(Span::styled(
            format!(
                "{}/{}",
                app.approval_review.page + 1,
                app.approval_review.len().saturating_sub(1) / page_size + 1
            ),
            Style::default().fg(t.work_meta_fg.into()),
        ));
        rows.button(&mut header, " › ", ApprovalClick::Page(true));
        if area.width >= 64 && !app.approval_review.requests.is_empty() {
            rows.button(&mut header, " Allow all ", ApprovalClick::ApproveAll);
        }
        rows.line(header);
        if app.approval_review.reviewed.is_none() {
            app.approval_review.reviewed = app.approval_review.entry(
                app.approval_review.page * page_size,
                &app.pending_permissions,
            );
        }
        let start = app.approval_review.page * page_size;
        let mut number = 0;
        for index in start..(start + page_size).min(app.approval_review.len()) {
            let mut spans = Vec::new();
            let label = if let Some(group) = app.approval_review.groups.get(index) {
                format!(
                    " Ⓖ {}  {}",
                    group.payload.request_ids.len(),
                    group.payload.label
                )
            } else {
                number += 1;
                let request =
                    &app.approval_review.requests[index - app.approval_review.groups.len()];
                format!(
                    " {number}  {}",
                    approval_labels(&request.payload.tool, request.payload.call_intent.as_ref()).0
                )
            };
            rows.button(
                &mut spans,
                &crate::width::truncate(&label, area.width as usize),
                ApprovalClick::Select(index),
            );
            rows.line(spans);
        }
        if let Some(reviewed) = app.approval_review.reviewed.as_ref() {
            let mut details = Rows::new(area.width);
            match reviewed {
                Reviewed::Request(request) => {
                    request_details(&mut details, request, app.approval_review.audit);
                    let mut scopes = Vec::new();
                    for (index, label) in [(0, " 1× "), (1, " ↻ tool "), (2, " = path ")] {
                        if crate::approval::grant_scope(request, index).is_some() {
                            rows.button(&mut scopes, label, ApprovalClick::Scope(index));
                            if index != app.approval_scope_index {
                                if let Some(span) = scopes.last_mut() {
                                    span.style = Style::default().fg(t.work_meta_fg.into());
                                }
                            }
                        }
                    }
                    rows.line(scopes);
                    let scope = crate::approval::grant_scope(request, app.approval_scope_index);
                    let scope_label = match scope {
                        Some(GrantScope::CurrentCall) => "Current call".into(),
                        Some(GrantScope::ChildRunSameTool { run_id, tool_name }) => {
                            format!("Same run/tool · {run_id} / {tool_name}")
                        }
                        Some(GrantScope::ChildRunSamePathRule {
                            run_id,
                            tool_name,
                            workspace_relative_path,
                        }) => format!(
                            "Exact path · {run_id} / {tool_name} / {workspace_relative_path}"
                        ),
                        _ => "Exact path unavailable for this request".into(),
                    };
                    details.detail("Scope", &scope_label);
                }
                Reviewed::Group(group, members) => {
                    details.detail(
                        &group.payload.label,
                        &format!(
                            "{} requests · Current call",
                            group.payload.request_ids.len()
                        ),
                    );
                    if app.approval_review.members {
                        for member in members {
                            request_details(&mut details, member, app.approval_review.audit);
                        }
                        if members.len() < group.payload.request_ids.len() {
                            details.detail(
                                "Members",
                                "Some requests have already changed. Refresh before deciding.",
                            );
                        }
                    }
                    if app.approval_review.audit {
                        details.detail(
                            "Group",
                            &format!("{} · revision {}", group.group_id, group.revision),
                        );
                    }
                    let mut controls = Vec::new();
                    rows.button(
                        &mut controls,
                        if app.approval_review.members {
                            " − members "
                        } else {
                            " + members "
                        },
                        ApprovalClick::Members,
                    );
                    rows.line(controls);
                }
            }
            let mut controls = Vec::new();
            rows.button(
                &mut controls,
                if app.approval_review.audit {
                    " − # "
                } else {
                    " + # "
                },
                ApprovalClick::Audit,
            );
            rows.button(&mut controls, " ↑ ", ApprovalClick::Scroll(false));
            rows.button(&mut controls, " ↓ ", ApprovalClick::Scroll(true));
            rows.line(controls);
            let members = reviewed.ids();
            let feedback = app.approval_decisions.feedback_for(&members);
            if app.approval_review.audit {
                if let Some(result) = app.approval_decisions.result_for(&members) {
                    match result {
                        Ok(outcomes) => {
                            for outcome in outcomes {
                                details.detail("Decision", &format!("{outcome:?}"));
                            }
                        }
                        Err(error) => details.detail("Decision", error),
                    }
                }
            }
            let waiting = app.approval_decisions.blocks(&members);
            if reviewed.current(app) && !waiting && app.approval_review.actions_visible {
                let mut actions = Vec::new();
                for (label, action) in [
                    (" Allow ", PermissionAction::Approve),
                    (" Deny ", PermissionAction::Deny),
                    (" Cancel requests ", PermissionAction::Defer),
                ] {
                    if let Some(request) = decision(reviewed, action, app.approval_scope_index) {
                        rows.button(&mut actions, label, ApprovalClick::Decide(request));
                    }
                }
                rows.line(actions);
            } else if !waiting {
                rows.detail("", "Changed / handled elsewhere · ↻");
            }
            if let Some((message, failed, _)) = feedback {
                rows.line(vec![Span::styled(
                    format!(" {message}"),
                    Style::default().fg(if failed { t.error } else { t.work_meta_fg }.into()),
                )]);
            }
            // Fixed actions stay visible; long targets/audit scroll independently.
            let remaining = area.height.saturating_sub(rows.lines.len() as u16 + 2) as usize;
            app.approval_review.scroll = app
                .approval_review
                .scroll
                .min(details.lines.len().saturating_sub(remaining));
            let insert = rows
                .lines
                .len()
                .min(1 + (app.approval_review.len() - start).min(page_size));
            let visible = details
                .lines
                .into_iter()
                .skip(app.approval_review.scroll)
                .take(remaining)
                .collect::<Vec<_>>();
            let count = visible.len();
            rows.lines.splice(insert..insert, visible);
            for (row, _, _, _) in &mut rows.hits {
                if *row >= insert {
                    *row += count;
                }
            }
        }
        rows.line(vec![Span::styled(
            " Cancel requests cancels now.",
            Style::default().fg(t.work_meta_fg.into()),
        )]);
        rows.line(vec![Span::styled(
            " Esc: deny all + cancel flow",
            Style::default().fg(t.work_meta_fg.into()),
        )]);
    }
    let hits = rows
        .hits
        .into_iter()
        .filter(|(row, _, _, _)| *row < area.height as usize)
        .map(|(row, x, end, action)| {
            (
                Rect::new(area.x + x, area.y + row as u16, end - x, 1),
                action,
            )
        })
        .collect();
    let hitmap = ApprovalHitMap(hits);
    if let Some((x, y)) = app.approval_review.pointer {
        if let Some((rect, _)) = hitmap
            .0
            .iter()
            .find(|(rect, _)| rect.contains((x, y).into()))
        {
            if let Some(line) = rows.lines.get_mut((rect.y - area.y) as usize) {
                let mut column = area.x;
                for span in &mut line.spans {
                    let end = column.saturating_add(crate::width::width(&span.content) as u16);
                    if end > rect.x && column < rect.right() {
                        span.style = span.style.bg(t.work_hover_bg.into());
                    }
                    column = end;
                }
            }
        }
    }
    frame.render_widget(Paragraph::new(rows.lines), area);
    hitmap
}

fn approval_labels(
    tool: &str,
    call_intent: Option<&atman_runtime::message::ToolCallIntent>,
) -> (String, Option<String>) {
    match call_intent {
        Some(intent) => (intent.as_str().to_owned(), Some(tool.to_owned())),
        None => (
            fallback_approval_label(tool).to_owned(),
            Some(tool.to_owned()),
        ),
    }
}

fn fallback_approval_label(tool: &str) -> &'static str {
    if tool.starts_with("bash.") || tool.starts_with("terminal.") {
        "Run command"
    } else if tool.starts_with("fs.write") || tool.starts_with("fs.edit") {
        "Modify files"
    } else if tool.starts_with("fs.") || tool == "image.read" {
        "Read files"
    } else if tool.starts_with("web.") || tool.starts_with("http.") {
        "Access network"
    } else if tool.starts_with("flow.") {
        "Start flow"
    } else {
        "Allow tool call"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_labels_put_intent_before_technical_tool_name() {
        let intent = atman_runtime::message::ToolCallIntent::new("检查活动进程").unwrap();
        let (title, technical) = approval_labels("bash.spawn", Some(&intent));
        assert_eq!(title, "检查活动进程");
        assert_eq!(technical.as_deref(), Some("bash.spawn"));
    }

    #[test]
    fn approval_labels_keep_tool_as_legacy_fallback() {
        let (title, technical) = approval_labels("bash.spawn", None);
        assert_eq!(title, "Run command");
        assert_eq!(technical.as_deref(), Some("bash.spawn"));
    }
}
