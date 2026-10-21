//! A paged instance directory. Reading and selecting rows never controls execution.

#[cfg(test)]
use atman_runtime::flow_store::FlowRef;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::Style,
    widgets::Paragraph,
};

use crate::{control::ControlTicket, keys::KeyAction};

pub const PAGE_SIZE: usize = 50;

mod controls;
mod details;
pub use details::{FlowArchiveChange, FlowCatalogSelection, FlowDetailsPage};
mod focus;
pub use controls::{ControlBatchPage, OperatorControl, OperatorControls};
mod graph;
pub(crate) use focus::{FocusLinks, NeighborSnapshot};
mod create;
pub use create::WatchDefinition;
mod inbox;
mod management;
mod navigation;
pub use management::{ManagementPage, ManagementSelection};
mod overview;
pub use overview::OverviewDirectory;
mod page;
mod project;
pub use project::{SessionProjectChange, SessionProjectPage};
mod release;
mod removal;
pub use removal::{SessionRemovalPage, SessionRemovalRequest};
mod text;
mod watch;
mod workspaces;
pub use page::{
    FlowHubPage, HubRequest, RelationProgress, RelationRange, SessionDirectory, SessionRow,
};
pub use watch::WatchDirectory;
pub use workspaces::{
    FlowWorkspacePage, WorkspaceCursor, WorkspaceDirectory, WorkspaceDirectoryEntry,
    WorkspaceFlowReference, WorkspaceLocation, WorkspaceRecordPage, WorkspaceRemoval,
    WorkspaceRetention,
};

#[derive(Clone)]
pub(crate) enum Action {
    ChangeSessionProject(Box<SessionProjectChange>),
    ArchiveFlow(Box<FlowArchiveChange>),
    SelectDefault(Box<FlowCatalogSelection>),
    RetryControls(uuid::Uuid),
    SubmitControls(Box<OperatorControls>),
    EditManagement,
    EditManagementBinding,
    SelectManagement(Box<ManagementSelection>),
    UpdateManagement(Box<atman_runtime::flow_store::profile::ReplaceManagement>),
    ReviewWorkspaceRelease(Box<WorkspaceRemoval>),
    ReleaseWorkspace(Box<WorkspaceRemoval>),
    RemoveSession(Box<SessionRemovalRequest>),
    SetWorkspaceRetention(Box<WorkspaceRetention>),
    None,
    Close,
    Return,
    Load(HubRequest),
    CancelWatcher {
        owner: atman_runtime::flow_store::FlowRef,
        id: uuid::Uuid,
    },
    RegisterWatcher {
        owner: atman_runtime::flow_store::FlowRef,
        registration: WatchDefinition,
    },
    EditWatcher {
        owner: atman_runtime::flow_store::FlowRef,
        expected: WatchDefinition,
    },
    ReplaceWatcher {
        owner: atman_runtime::flow_store::FlowRef,
        expected: WatchDefinition,
        replacement: Box<WatchDefinition>,
    },
    NewSession,
    OpenManagement,
    Focus(crate::control::ViewAddress),
    Float(crate::control::ViewAddress),
}

#[derive(Default)]
pub(crate) struct FlowHub {
    navigation: navigation::Navigation,
    details_button: Rect,
    details_hovered: bool,
    controls: Option<controls::Draft>,
    controls_button: Rect,
    controls_hovered: bool,
    edit_management: Option<management::Draft>,
    release_workspace: Option<release::Confirmation>,
    create_watch: Option<create::Draft>,
    watch_details: watch::Details,
    inbox_details: inbox::Details,
    property_details: Option<text::TextDetails>,
    watch_refresh: Option<atman_runtime::flow_store::FlowRef>,
    pub pending: Option<ControlTicket>,
    page: Option<FlowHubPage>,
    rows: Vec<page::Row>,
    endpoint: page::Endpoint,
    graph: graph::Layout,
    layout: Layout,
    graph_button: Rect,
    graph_hovered: bool,
    cursors: Vec<HubRequest>,
    index: usize,
    requested_index: Option<usize>,
    selected: usize,
    offset: usize,
    query: String,
    error: Option<String>,
    hits: Vec<(Rect, usize)>,
    float_hits: Vec<(Rect, usize)>,
    previous: Rect,
    next: Rect,
    close: Rect,
    refresh: Rect,
    relations: Rect,
    sessions: Rect,
    sessions_hovered: bool,
    relations_hovered: bool,
    refresh_hovered: bool,
    hovered: Option<usize>,
}

/// Retain directory presentation independently of focused conversations and host requests.
#[derive(Default)]
pub(crate) struct History {
    pages: Vec<FlowHub>,
    catalog: navigation::Catalog,
}

#[derive(Default, PartialEq, Eq)]
enum Layout {
    #[default]
    Graph,
    List,
}

impl History {
    pub fn request(&self, request: HubRequest) -> HubRequest {
        self.pages
            .iter()
            .find(|saved| saved.page.as_ref().unwrap().first() == request.first())
            .and_then(|saved| saved.cursors.get(saved.index))
            .cloned()
            .unwrap_or(request)
    }

    pub fn receive(
        &mut self,
        hub: &mut FlowHub,
        ticket: Option<&ControlTicket>,
        result: Result<Box<FlowHubPage>, String>,
    ) {
        if !hub.owns_reply(ticket) {
            return;
        }
        // History browsing retains the unsubmitted review as an ordinary back-stack view.
        if matches!(&result, Ok(page) if matches!(**page, FlowHubPage::ControlHistory(_)))
            && hub.controls.as_ref().is_some_and(|draft| !draft.picking)
        {
            self.save(std::mem::take(hub));
            hub.pending = ticket.cloned();
            hub.receive(ticket, result);
            return;
        }
        // Editing returns to the original directory even after choosing a different source.
        if let Some(draft) = &hub.create_watch
            && let Some(origin) = &draft.origin
            && let Ok(page) = &result
            && let FlowHubPage::Watchers(incoming) = &**page
        {
            if incoming.owner != draft.owner {
                hub.failed("Watcher edit returned a different owner".into());
                return;
            }
            if hub.page.as_ref().is_none_or(|page| page.first() != *origin) {
                let Some(index) = self.pages.iter().position(|saved| {
                    saved
                        .page
                        .as_ref()
                        .is_some_and(|page| page.first() == *origin)
                }) else {
                    hub.failed(
                        "Original watcher directory is unavailable; close and reopen WatchHub"
                            .into(),
                    );
                    return;
                };
                let draft = hub.create_watch.take();
                *hub = self.pages.remove(index);
                hub.create_watch = draft;
                hub.pending = ticket.cloned();
            }
            if matches!(&hub.page, Some(FlowHubPage::Watchers(directory)) if directory.global()) {
                hub.watch_refresh = Some(incoming.owner.clone());
            }
        }
        if let Ok(page) = &result
            && hub.leaves_directory(&page.first())
        {
            let draft = hub.create_watch.take();
            let management = hub.edit_management.take();
            let controls = hub.controls.take();
            let returning_to_management = matches!(&**page, FlowHubPage::Management(page) if page.grant.is_none())
                && (management.is_some()
                    || matches!(&hub.page, Some(FlowHubPage::Management(page)) if page.grant.is_some()));
            if returning_to_management {
                // A grant form returns to the directory, not to an older detail snapshot.
                self.pages.retain(|saved| !matches!(&saved.page, Some(FlowHubPage::Management(page)) if page.grant.is_some()));
            }
            let leaving_picker = (draft.is_some()
                && !matches!(hub.page, Some(FlowHubPage::Watchers(_))))
                || matches!((&hub.page, &**page),
                    (Some(FlowHubPage::SessionProject(editor)), FlowHubPage::Flows { session, .. })
                    if editor.session() == *session)
                || (management.is_some() && !matches!(hub.page, Some(FlowHubPage::Management(_))))
                || returning_to_management
                || controls.as_ref().is_some_and(|draft| {
                    hub.page
                        .as_ref()
                        .is_none_or(|page| page.first() != draft.origin)
                });
            let entering_picker = (draft.is_some() && !matches!(**page, FlowHubPage::Watchers(_)))
                || (management.is_some() && !matches!(**page, FlowHubPage::Management(_)))
                || (controls.is_some() && !matches!(**page, FlowHubPage::ControlBatch(_)));
            let next = if entering_picker {
                FlowHub::default()
            } else {
                self.pages
                    .iter()
                    .position(|saved| saved.page.as_ref().unwrap().first() == page.first())
                    .map(|index| self.pages.remove(index))
                    .unwrap_or_default()
            };
            let previous = std::mem::replace(hub, next);
            if !leaving_picker {
                self.save(previous);
            }
            hub.create_watch = draft;
            hub.edit_management = management;
            hub.controls = controls;
            hub.pending = ticket.cloned();
        }
        hub.receive(ticket, result);
        if let Some(page) = &hub.page {
            self.catalog.observe(page);
        }
        hub.navigation.catalog = self.catalog.clone();
    }

    pub fn save(&mut self, mut hub: FlowHub) {
        let Some(page) = &hub.page else { return };
        self.catalog.observe(page);
        let directory = page.first();
        self.pages
            .retain(|saved| saved.page.as_ref().unwrap().first() != directory);
        // Accepted host work continues, but its late reply no longer owns navigation.
        hub.pending = None;
        hub.watch_refresh = None;
        hub.requested_index = None;
        self.pages.push(hub);
    }

    pub fn open(&mut self, action: Action, fallback: HubRequest) -> (FlowHub, Action) {
        let (mut hub, action) = self.open_directory(action, fallback);
        hub.navigation.catalog = self.catalog.clone();
        (hub, action)
    }

    fn open_directory(&mut self, action: Action, fallback: HubRequest) -> (FlowHub, Action) {
        if matches!(action, Action::Return) {
            return match self.pages.pop() {
                Some(saved) => (saved, Action::None),
                None => (FlowHub::default(), Action::Load(fallback)),
            };
        }
        if let Action::Load(request) = &action
            && let Some(index) = self
                .pages
                .iter()
                .position(|saved| saved.page.as_ref().unwrap().first() == *request)
        {
            let saved = self.pages.remove(index);
            let request = saved
                .cursors
                .get(saved.index)
                .cloned()
                .unwrap_or_else(|| request.clone());
            return (saved, Action::Load(request));
        }
        (FlowHub::default(), action)
    }
}

impl FlowHub {
    pub fn neighbors_for(
        &self,
        flow: &atman_runtime::flow_store::FlowRef,
    ) -> Option<NeighborSnapshot> {
        if self.error.is_some() {
            return None;
        }
        self.page
            .as_ref()
            .and_then(|page| NeighborSnapshot::from_page(page, flow))
    }
    pub fn leaves_directory(&self, request: &HubRequest) -> bool {
        if self.watch_refresh.is_some() {
            return false;
        }
        self.page
            .as_ref()
            .is_some_and(|page| page.first() != request.first())
    }

    pub fn restores_directory(&self, request: &HubRequest) -> bool {
        (self.create_watch.is_none() || matches!(request, HubRequest::Watchers { .. }))
            && (self.edit_management.is_none() || matches!(request, HubRequest::Management { .. }))
            && (self.controls.is_none() || matches!(request, HubRequest::ControlBatch { .. }))
    }

    pub fn owns_reply(&self, ticket: Option<&ControlTicket>) -> bool {
        ticket.is_some() && ticket == self.pending.as_ref()
    }

    pub fn receive(
        &mut self,
        ticket: Option<&ControlTicket>,
        result: Result<Box<FlowHubPage>, String>,
    ) {
        if !self.owns_reply(ticket) {
            return;
        }
        self.pending = None;
        match result {
            Ok(mut page) => {
                if matches!(*page, FlowHubPage::ControlBatch(_)) {
                    self.controls = None;
                }
                if matches!(*page, FlowHubPage::Management(_)) {
                    self.edit_management = None;
                }
                if let Some(owner) = self.watch_refresh.take() {
                    let result = match (&mut self.page, &mut *page) {
                        (Some(FlowHubPage::Watchers(current)), FlowHubPage::Watchers(incoming))
                            if incoming.owner == owner =>
                        {
                            current.refresh_owner(incoming)
                        }
                        _ => Err("Watcher refresh returned a different owner".into()),
                    };
                    match result {
                        Ok(()) => {
                            if let Some(draft) = self.create_watch.take()
                                && let Some(id) = draft.saved_id()
                            {
                                self.watch_details.open(owner, id);
                            }
                            self.rows = self.page.as_ref().unwrap().rows(None, self.endpoint);
                            self.selected =
                                self.selected.min(self.filtered().len().saturating_sub(1));
                            self.offset = self.offset.min(self.selected);
                            self.hits.clear();
                            self.float_hits.clear();
                            self.error = None;
                        }
                        Err(error) => self.failed(error),
                    }
                    return;
                }
                if let Err(error) = page.append_catalog(self.page.as_mut()) {
                    self.failed(error);
                    return;
                }
                if matches!(*page, FlowHubPage::Watchers(_)) {
                    if let Some(draft) = &self.create_watch
                        && draft.origin.is_some()
                        && let Some(id) = draft.saved_id()
                    {
                        self.watch_details.open(draft.owner.clone(), id);
                    }
                    self.create_watch = None;
                }
                let selected = self
                    .filtered()
                    .get(self.selected)
                    .map(|index| self.rows[*index].key.clone());
                let changed_directory = self
                    .page
                    .as_ref()
                    .is_none_or(|current| current.first() != page.first());
                let changed_page = self
                    .requested_index
                    .is_some_and(|index| index != self.index);
                if changed_directory {
                    self.cursors = vec![page.first()];
                    self.index = 0;
                    self.requested_index = None;
                    self.query.clear();
                }
                if changed_directory {
                    self.endpoint = page::Endpoint::default();
                }
                self.rows = page.rows(None, self.endpoint);
                self.property_details = None;
                self.release_workspace = None;
                if changed_directory || changed_page {
                    self.watch_details = watch::Details::default();
                    self.inbox_details = inbox::Details::default();
                    self.graph = graph::Layout::default();
                }
                if page.nodes() {
                    self.graph.sync(&self.rows);
                }
                self.page = Some(*page);
                if let Some(index) = self.requested_index.take() {
                    self.index = index;
                }
                if changed_directory || changed_page {
                    self.selected = 0;
                    self.offset = 0;
                    self.query.clear();
                } else {
                    let filtered = self.filtered();
                    self.selected = selected
                        .and_then(|key| {
                            filtered
                                .iter()
                                .position(|index| self.rows[*index].key == key)
                        })
                        .unwrap_or(self.selected.min(filtered.len().saturating_sub(1)));
                    self.offset = self.offset.min(self.selected);
                }
                self.hits.clear();
                self.float_hits.clear();
                self.hovered = None;
                self.error = None;
            }
            Err(error) => {
                self.watch_refresh = None;
                self.requested_index = None;
                self.error = Some(error);
            }
        }
    }

    pub fn failed(&mut self, message: String) {
        self.watch_refresh = None;
        self.pending = None;
        self.requested_index = None;
        self.error = Some(message);
        self.watch_details.feedback();
    }

    fn filtered(&self) -> Vec<usize> {
        let query = self.query.to_lowercase();
        let links = self
            .page
            .as_ref()
            .and_then(|page| page.links().ok())
            .unwrap_or(&[]);
        self.rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                self.edit_management
                    .as_ref()
                    .is_none_or(|draft| draft.allows_row(&row.key))
            })
            .filter_map(|(i, row)| {
                query
                    .split_whitespace()
                    .all(|term| {
                        if let Some(kind) = term.strip_prefix("link:") {
                            graph::has_link(row, links, kind)
                        } else {
                            row.title.to_lowercase().contains(term)
                                || row.detail.to_lowercase().contains(term)
                        }
                    })
                    .then_some(i)
            })
            .collect()
    }

    fn focus(&self) -> Action {
        let rows = self.filtered();
        rows.get(self.selected)
            .map(|index| self.rows[*index].action.clone())
            .unwrap_or(Action::None)
    }

    fn float(&mut self) -> Action {
        if self.create_watch.is_some() || self.edit_management.is_some() || self.controls.is_some()
        {
            return self.activate();
        }
        match self.focus() {
            Action::Focus(flow) => Action::Float(flow),
            Action::RemoveSession(_) | Action::SelectDefault(_) | Action::ArchiveFlow(_) => {
                Action::None
            }
            _ => self.activate(),
        }
    }

    fn activate(&mut self) -> Action {
        if matches!(
            self.focus(),
            Action::RemoveSession(_) | Action::SelectDefault(_) | Action::ArchiveFlow(_)
        ) && self
            .filtered()
            .get(self.selected)
            .is_none_or(|selected| !self.hits.iter().any(|(_, index)| index == selected))
        {
            return Action::None;
        }
        if self.controls.as_ref().is_some_and(|draft| draft.picking) {
            if let Some(index) = self.filtered().get(self.selected).copied()
                && let page::RowKey::Flow(address) = &self.rows[index].key
                && let Some(address) = address.instance()
                && self.controls.as_mut().unwrap().remove(address)
            {
                self.error = None;
                return Action::None;
            }
            if let Some(target) = self.control_target() {
                self.controls.as_mut().unwrap().toggle(target);
                self.error = None;
                return Action::None;
            }
            return match self.focus() {
                action @ Action::Load(
                    HubRequest::Flows { .. }
                    | HubRequest::Overview { .. }
                    | HubRequest::Sessions { .. },
                ) => action,
                _ => {
                    self.error = Some("This record has no readable execution snapshot".into());
                    Action::None
                }
            };
        }
        let selected = self
            .filtered()
            .get(self.selected)
            .map(|index| &self.rows[*index]);
        if let Some(draft) = &mut self.edit_management
            && let Some(row) = selected
        {
            match draft.choose(&row.key) {
                Ok(true) => {
                    self.error = None;
                    return Action::None;
                }
                Err(error) => {
                    self.error = Some(error);
                    return Action::None;
                }
                Ok(false) => {
                    return match &row.action {
                        Action::Load(request)
                            if !matches!(request, HubRequest::Management { .. }) =>
                        {
                            Action::Load(request.clone())
                        }
                        _ => Action::None,
                    };
                }
            }
        }
        if matches!(
            selected.map(|row| &row.action),
            Some(Action::EditManagement | Action::EditManagementBinding)
        ) && let Some(FlowHubPage::Management(page)) = &self.page
        {
            match if matches!(
                selected.map(|row| &row.action),
                Some(Action::EditManagementBinding)
            ) {
                Ok(management::Draft::binding(page))
            } else {
                management::Draft::new(page)
            } {
                Ok(draft) => {
                    self.edit_management = Some(draft);
                    self.error = None;
                }
                Err(error) => self.error = Some(error),
            }
            return Action::None;
        }
        if let Some(draft) = &mut self.create_watch {
            if let Some(page::Row {
                key: page::RowKey::Process(source),
                title,
                ..
            }) = selected
            {
                draft.choose_process(source.clone(), title.clone());
                self.error = None;
                return Action::None;
            }
            let action = selected.map_or(Action::None, |row| row.action.clone());
            return match action {
                Action::Focus(address) => {
                    if let Some(source) = address.instance() {
                        let title = selected.map(|row| row.title.clone()).unwrap_or_default();
                        draft.choose(source.clone(), title);
                        self.error = None;
                    }
                    Action::None
                }
                Action::Load(
                    HubRequest::Flows { .. }
                    | HubRequest::Sessions { .. }
                    | HubRequest::Overview { .. },
                ) => action,
                _ => {
                    self.error = Some("Select a persistent Flow as the observation source".into());
                    Action::None
                }
            };
        }
        if let Some(page::RowKey::Delivery { id, .. }) = selected.map(|row| &row.key) {
            self.inbox_details.open(*id);
            return Action::None;
        }
        if let Some(Action::ReviewWorkspaceRelease(request)) = selected.map(|row| &row.action) {
            if self.pending.is_some() {
                return Action::None;
            }
            self.release_workspace = Some(release::Confirmation::new(*request.clone()));
            self.error = None;
            return Action::None;
        }
        if let Some(row) = selected
            && row.has_text_details()
        {
            self.property_details = Some(text::TextDetails::new(
                row.title.clone(),
                vec![row.detail.clone()],
            ));
            return Action::None;
        }
        if let Some(page::RowKey::Watcher { owner, id }) = self
            .filtered()
            .get(self.selected)
            .map(|index| &self.rows[*index].key)
        {
            self.watch_details.open(owner.clone(), *id);
            Action::None
        } else {
            self.focus()
        }
    }

    fn detail_action(&mut self, action: Action) -> Action {
        if let Action::EditWatcher { owner, expected } = action {
            if self.pending.is_none()
                && let Some(FlowHubPage::Watchers(directory)) = &self.page
            {
                self.create_watch = Some(create::Draft::edit(owner, expected, directory.first()));
                self.error = None;
                self.watch_details.id = None;
            }
            return Action::None;
        }
        if let Action::CancelWatcher { owner, .. } = &action {
            if self.pending.is_some() {
                return Action::None;
            }
            self.error = None;
            self.watch_details.feedback();
            if matches!(&self.page, Some(FlowHubPage::Watchers(directory)) if directory.global()) {
                self.watch_refresh = Some(owner.clone());
            }
        }
        action
    }

    fn picker_action(&mut self, action: Action) -> Action {
        if (self
            .create_watch
            .as_ref()
            .is_some_and(|draft| draft.picking)
            && !matches!(self.page, Some(FlowHubPage::Watchers(_))))
            || (self
                .edit_management
                .as_ref()
                .is_some_and(|draft| draft.picking())
                && !matches!(self.page, Some(FlowHubPage::Management(_))))
        {
            self.query.clear();
            self.selected = 0;
            self.offset = 0;
        }
        action
    }

    fn page(&mut self, forward: bool) -> Action {
        if self.pending.is_some() {
            return Action::None;
        }
        if matches!(
            self.page,
            Some(
                FlowHubPage::Relations { .. }
                    | FlowHubPage::Overview(_)
                    | FlowHubPage::Watchers(_)
                    | FlowHubPage::ProcessSources { .. }
                    | FlowHubPage::Workspaces(_)
                    | FlowHubPage::Workspace(_)
                    | FlowHubPage::WorkspaceRecord(_)
                    | FlowHubPage::FlowWorkspace(_)
                    | FlowHubPage::Management(_)
                    | FlowHubPage::ControlBatch(_)
            )
        ) {
            let step = self.hits.len().max(1);
            if forward
                && self.selected + step >= self.filtered().len()
                && let Some(next) = self.page.as_ref().and_then(FlowHubPage::next)
            {
                return Action::Load(next);
            }
            self.selected = if forward {
                (self.selected + step).min(self.filtered().len().saturating_sub(1))
            } else {
                self.selected.saturating_sub(step)
            };
            return Action::None;
        }
        if self.cursors.is_empty() {
            return Action::None;
        }
        let target = if forward {
            let Some(next) = self.page.as_ref().and_then(FlowHubPage::next) else {
                return Action::None;
            };
            self.cursors.truncate(self.index + 1);
            self.cursors.push(next);
            self.index + 1
        } else if self.index > 0 {
            self.index - 1
        } else {
            return Action::None;
        };
        self.requested_index = Some(target);
        Action::Load(self.cursors[target].clone())
    }

    pub fn key(&mut self, key: KeyAction) -> Action {
        if let Some(FlowHubPage::SessionProject(page)) = &mut self.page {
            return page.key(key, self.pending.is_some());
        }
        if let Some(draft) = &mut self.controls {
            if !draft.picking {
                let identity = draft.identity();
                let action = draft.key(key, self.pending.is_some());
                if identity != draft.identity() {
                    self.error = None;
                }
                if matches!(action, Action::Return) {
                    return self.close_controls();
                }
                return action;
            }
            if matches!(key, KeyAction::Escape | KeyAction::CycleReasoning) {
                draft.picking = false;
                return Action::None;
            }
            if key == KeyAction::CoursePrefill && self.pending.is_none() {
                return Action::Load(HubRequest::Overview { after: None });
            }
            if matches!(key, KeyAction::NudgePrefill | KeyAction::OpenRelations) {
                return Action::None;
            }
        }
        if let Some(draft) = &mut self.edit_management {
            if !draft.picking() || key == KeyAction::Escape {
                let action = draft.key(key, self.pending.is_some());
                return self.picker_action(action);
            }
            if matches!(key, KeyAction::NudgePrefill | KeyAction::OpenRelations) {
                return Action::None;
            }
            if key == KeyAction::RedirectPrefill && self.pending.is_none() {
                let request = draft.picker_request().unwrap();
                self.cursors = vec![request.clone()];
                self.requested_index = Some(0);
                return Action::Load(request);
            }
            if key == KeyAction::CoursePrefill && self.pending.is_none() {
                return if draft.allows_overview() {
                    Action::Load(HubRequest::Overview { after: None })
                } else {
                    Action::None
                };
            }
        }
        if let Some(confirmation) = &mut self.release_workspace {
            let action = confirmation.key(key, self.pending.is_some());
            return self.release_action(action);
        }
        if let Some(draft) = &mut self.create_watch {
            if !draft.picking {
                let action = draft.key(key, self.pending.is_some());
                return self.picker_action(action);
            }
            if key == KeyAction::Escape {
                return if draft.back_from_picker() {
                    Action::None
                } else {
                    Action::Load(HubRequest::Watchers {
                        owner: draft.owner.clone(),
                    })
                };
            }
            if matches!(key, KeyAction::NudgePrefill | KeyAction::OpenRelations) {
                return Action::None;
            }
            if key == KeyAction::CoursePrefill && self.pending.is_none() {
                let request = if matches!(self.page, Some(FlowHubPage::ProcessSources { .. })) {
                    HubRequest::Flows {
                        session: draft.owner.session_id.0,
                        before: None,
                    }
                } else {
                    HubRequest::ProcessSources {
                        owner: draft.owner.clone(),
                    }
                };
                return self.picker_action(Action::Load(request));
            }
        }
        if let Some(details) = &mut self.property_details {
            if details.key(key) {
                self.property_details = None;
            }
            return Action::None;
        }
        if self.inbox_details.id.is_some() {
            return self.inbox_details.key(key);
        }
        if self.watch_details.id.is_some() {
            let action = self.watch_details.key(key);
            return self.detail_action(action);
        }
        if self.pending.is_none()
            && self.create_watch.is_none()
            && self.edit_management.is_none()
            && self.controls.is_none()
            && let Some(action) = self.navigation.key(&key)
        {
            return action;
        }
        if key == KeyAction::Escape {
            if matches!(
                &self.page,
                Some(
                    FlowHubPage::WorkspaceRecord(_)
                        | FlowHubPage::FlowDetails(_)
                        | FlowHubPage::SessionRemoval(_)
                        | FlowHubPage::Management(_)
                        | FlowHubPage::Workspace(_)
                        | FlowHubPage::FlowWorkspace(_)
                        | FlowHubPage::ControlBatch(_)
                        | FlowHubPage::ControlHistory(_)
                )
            ) {
                return Action::Return;
            }
            if let Some(FlowHubPage::Relations {
                session, ref range, ..
            }) = self.page
            {
                if matches!(range, RelationRange::Catalog { .. }) {
                    return Action::Return;
                }
                return Action::Load(HubRequest::Flows {
                    session,
                    before: None,
                });
            }
            return Action::Close;
        }
        if self.pending.is_some() {
            return Action::None;
        }
        match key {
            KeyAction::CoursePrefill
                if self.page.as_ref().is_some_and(FlowHubPage::nodes)
                    && self.create_watch.is_none()
                    && self.edit_management.is_none()
                    && self.controls.is_none() =>
            {
                if let Action::Focus(address) = self.focus()
                    && let Some(owner) = address.instance()
                {
                    return Action::Load(HubRequest::FlowDetails {
                        owner: owner.clone(),
                    });
                }
                self.error = Some("Select a flow to view its details".into());
            }
            KeyAction::Delete
                if matches!(self.page, Some(FlowHubPage::Sessions(_)))
                    && self.create_watch.is_none()
                    && self.edit_management.is_none()
                    && self.controls.is_none() =>
            {
                if let Some(page::RowKey::Session(session)) = self
                    .filtered()
                    .get(self.selected)
                    .map(|i| &self.rows[*i].key)
                {
                    return Action::Load(HubRequest::SessionRemoval {
                        session: session.clone(),
                    });
                }
                self.error = Some("Select a session to review deletion".into());
            }
            KeyAction::CycleReasoning
                if self.page.as_ref().is_some_and(FlowHubPage::nodes)
                    && self.create_watch.is_none()
                    && self.edit_management.is_none() =>
            {
                self.controls = Some(controls::Draft::new(
                    self.control_target(),
                    self.page.as_ref().unwrap().first(),
                ));
                self.error = None;
            }
            KeyAction::CoursePrefill if matches!(self.page, Some(FlowHubPage::Sessions(_))) => {
                return Action::Load(HubRequest::Overview { after: None });
            }
            KeyAction::Tab | KeyAction::BackTab
                if matches!(self.page, Some(FlowHubPage::Inbox(_))) =>
            {
                let Some(FlowHubPage::Inbox(page)) = &self.page else {
                    unreachable!()
                };
                return Action::Load(HubRequest::Inbox {
                    owner: page.owner.clone(),
                    side: inbox::other_side(page.side),
                    before: None,
                });
            }
            KeyAction::RedirectPrefill if matches!(self.page, Some(FlowHubPage::Inbox(_))) => {
                self.requested_index = Some(0);
                return Action::Load(self.page.as_ref().unwrap().first());
            }
            KeyAction::NudgePrefill if matches!(self.page, Some(FlowHubPage::Watchers(_))) => {
                let Some(FlowHubPage::Watchers(directory)) = &self.page else {
                    unreachable!()
                };
                return Action::Load(if directory.global() {
                    HubRequest::Watchers {
                        owner: directory.owner.clone(),
                    }
                } else {
                    HubRequest::WatchCatalog {
                        owner: directory.owner.clone(),
                        after: None,
                    }
                });
            }
            KeyAction::CoursePrefill if matches!(self.page, Some(FlowHubPage::Watchers(_))) => {
                let Some(FlowHubPage::Watchers(directory)) = &self.page else {
                    unreachable!()
                };
                let draft = create::Draft::new(directory.owner.clone());
                let request = draft.source_directory();
                self.create_watch = Some(draft);
                return Action::Load(request);
            }
            KeyAction::CoursePrefill
                if matches!(self.page, Some(FlowHubPage::Relations { .. })) =>
            {
                let Some(FlowHubPage::Relations {
                    session,
                    focus,
                    range,
                    ..
                }) = &self.page
                else {
                    unreachable!()
                };
                return Action::Load(HubRequest::Relations {
                    session: *session,
                    focus: focus.clone(),
                    range: match range {
                        RelationRange::Session => RelationRange::Catalog { before: None },
                        RelationRange::Catalog { .. } => RelationRange::Session,
                    },
                });
            }
            KeyAction::Tab | KeyAction::BackTab
                if matches!(self.page, Some(FlowHubPage::Relations { .. })) =>
            {
                let selected = self
                    .filtered()
                    .get(self.selected)
                    .map(|index| self.rows[*index].key.clone());
                self.endpoint = self.endpoint.next();
                if key == KeyAction::BackTab {
                    self.endpoint = self.endpoint.next();
                }
                self.rows = self.page.as_ref().unwrap().rows(None, self.endpoint);
                let filtered = self.filtered();
                self.selected = selected
                    .and_then(|key| {
                        filtered
                            .iter()
                            .position(|index| self.rows[*index].key == key)
                    })
                    .unwrap_or(self.selected.min(filtered.len().saturating_sub(1)));
            }
            KeyAction::NudgePrefill if matches!(self.page, Some(FlowHubPage::Relations { .. })) => {
                let Some(FlowHubPage::Relations {
                    session,
                    focus,
                    range,
                    ..
                }) = &self.page
                else {
                    unreachable!()
                };
                let focus = if focus.is_some() {
                    None
                } else {
                    match self.focus() {
                        Action::Focus(address) => address.instance().cloned(),
                        _ => {
                            self.error = Some("Choose a Flow endpoint to show its links".into());
                            return Action::None;
                        }
                    }
                };
                return Action::Load(HubRequest::Relations {
                    session: *session,
                    focus,
                    range: match range {
                        RelationRange::Session => RelationRange::Session,
                        RelationRange::Catalog { .. } => RelationRange::Catalog { before: None },
                    },
                });
            }
            KeyAction::NudgePrefill if self.page.as_ref().is_some_and(FlowHubPage::nodes) => {
                self.layout = if self.layout == Layout::Graph {
                    Layout::List
                } else {
                    Layout::Graph
                };
            }
            KeyAction::RedirectPrefill => {
                return self
                    .cursors
                    .get(self.index)
                    .cloned()
                    .map_or(Action::None, Action::Load);
            }
            KeyAction::Save => return Action::Load(HubRequest::Sessions { before: None }),
            KeyAction::OpenRelations => {
                return match self.page.as_ref() {
                    Some(FlowHubPage::Overview(_)) => match self.focus() {
                        Action::Focus(address) => {
                            address.instance().map_or(Action::None, |owner| {
                                Action::Load(HubRequest::Relations {
                                    session: owner.session_id.0,
                                    focus: Some(owner.clone()),
                                    range: RelationRange::Catalog { before: None },
                                })
                            })
                        }
                        _ => Action::None,
                    },
                    Some(FlowHubPage::Flows { session, .. }) => {
                        let focus = match self.focus() {
                            Action::Focus(address) => address.instance().cloned(),
                            _ => None,
                        };
                        Action::Load(HubRequest::Relations {
                            session: *session,
                            focus,
                            range: RelationRange::Session,
                        })
                    }
                    _ => Action::None,
                };
            }
            KeyAction::Submit => return self.activate(),
            KeyAction::ToggleLastTool => return self.float(),
            KeyAction::HistoryUp => self.selected = self.selected.saturating_sub(1),
            KeyAction::HistoryDown => {
                self.selected = (self.selected + 1).min(self.filtered().len().saturating_sub(1))
            }
            KeyAction::PageUp => return self.page(false),
            KeyAction::PageDown => return self.page(true),
            KeyAction::Char(c) => {
                self.query.push(c);
                self.selected = 0;
                self.offset = 0;
            }
            KeyAction::Backspace => {
                self.query.pop();
                self.selected = 0;
                self.offset = 0;
            }
            _ => {}
        }
        Action::None
    }

    pub fn mouse(&mut self, event: MouseEvent) -> Action {
        if let Some(FlowHubPage::SessionProject(page)) = &mut self.page {
            return page.mouse(event, self.pending.is_some());
        }
        if let Some(draft) = &mut self.controls
            && !draft.picking
        {
            let identity = draft.identity();
            let action = draft.mouse(event, self.pending.is_some());
            if identity != draft.identity() {
                self.error = None;
            }
            if matches!(action, Action::Return) {
                return self.close_controls();
            }
            return action;
        }
        if let Some(draft) = &mut self.edit_management
            && !draft.picking()
        {
            let action = draft.mouse(event, self.pending.is_some());
            return self.picker_action(action);
        }
        if let Some(confirmation) = &mut self.release_workspace {
            let action = confirmation.mouse(event, self.pending.is_some());
            return self.release_action(action);
        }
        if let Some(details) = &mut self.property_details {
            if details.mouse(event) {
                self.property_details = None;
            }
            return Action::None;
        }
        if let Some(draft) = &mut self.create_watch
            && !draft.picking
        {
            let action = draft.mouse(event, self.pending.is_some());
            return self.picker_action(action);
        }
        if self.inbox_details.id.is_some() {
            return self.inbox_details.mouse(event);
        }
        if self.watch_details.id.is_some() {
            let action = self.watch_details.mouse(event);
            return self.detail_action(action);
        }
        let point = (event.column, event.row).into();
        if self.create_watch.is_none()
            && self.edit_management.is_none()
            && self.controls.is_none()
            && let Some(action) = self.navigation.mouse(event, self.pending.is_some())
        {
            return action;
        }
        self.sessions_hovered = self.sessions.contains(point);
        self.relations_hovered = self.relations.contains(point);
        self.refresh_hovered = self.refresh.contains(point);
        self.graph_hovered = self.graph_button.contains(point);
        self.controls_hovered = self.controls_button.contains(point);
        self.details_hovered = self.details_button.contains(point);
        self.hovered = self
            .hits
            .iter()
            .find(|(rect, _)| rect.contains(point))
            .map(|(_, index)| *index);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if self.close.contains(point) => {
                self.key(KeyAction::Escape)
            }
            _ if self.pending.is_some() => Action::None,
            MouseEventKind::Down(MouseButton::Left) if self.details_button.contains(point) => {
                self.key(KeyAction::CoursePrefill)
            }
            MouseEventKind::Down(MouseButton::Left) if self.controls_button.contains(point) => self
                .key(if matches!(self.page, Some(FlowHubPage::Sessions(_))) {
                    KeyAction::Delete
                } else {
                    KeyAction::CycleReasoning
                }),
            MouseEventKind::Down(MouseButton::Left) if self.graph_button.contains(point) => self
                .key(
                    if self.create_watch.is_some()
                        || self.edit_management.is_some()
                        || matches!(self.page, Some(FlowHubPage::Watchers(_)))
                    {
                        KeyAction::CoursePrefill
                    } else {
                        KeyAction::NudgePrefill
                    },
                ),
            MouseEventKind::Down(MouseButton::Left) if self.sessions.contains(point) => self.key(
                if matches!(self.page, Some(FlowHubPage::Relations { .. })) {
                    KeyAction::CoursePrefill
                } else {
                    KeyAction::Save
                },
            ),
            MouseEventKind::Down(MouseButton::Left) if self.relations.contains(point) => {
                self.key(if matches!(self.page, Some(FlowHubPage::Inbox(_))) {
                    KeyAction::Tab
                } else if matches!(self.page, Some(FlowHubPage::Watchers(_))) {
                    KeyAction::NudgePrefill
                } else if matches!(self.page, Some(FlowHubPage::Relations { .. })) {
                    KeyAction::Tab
                } else {
                    KeyAction::OpenRelations
                })
            }
            MouseEventKind::ScrollDown => self.key(KeyAction::HistoryDown),
            MouseEventKind::ScrollUp => self.key(KeyAction::HistoryUp),
            MouseEventKind::Down(MouseButton::Left) if self.previous.contains(point) => {
                self.page(false)
            }
            MouseEventKind::Down(MouseButton::Left) if self.next.contains(point) => self.page(true),
            MouseEventKind::Down(MouseButton::Left) if self.refresh.contains(point) => {
                self.key(KeyAction::RedirectPrefill)
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some((_, index)) = self
                    .float_hits
                    .iter()
                    .find(|(rect, _)| rect.contains(point))
                {
                    self.selected = *index;
                    return self.float();
                }
                if let Some(index) = self.hovered {
                    self.selected = index;
                    self.activate()
                } else {
                    Action::None
                }
            }
            _ => Action::None,
        }
    }

    pub fn render(&mut self, frame: &mut Frame, focused: &crate::control::ViewAddress) {
        self.navigation.clear();
        let t = crate::theme::theme();
        let area = frame.area();
        frame.render_widget(
            Paragraph::new("").style(Style::default().bg(t.panel_bg.into())),
            area,
        );
        self.controls_button = Rect::default();
        self.details_button = Rect::default();
        if let Some(FlowHubPage::SessionProject(page)) = &mut self.page {
            page.render(frame, area, self.pending.is_some(), self.error.as_deref());
            return;
        }
        if let Some(draft) = &mut self.controls
            && !draft.picking
        {
            draft.render(frame, area, self.pending.is_some(), self.error.as_deref());
            return;
        }
        if let Some(confirmation) = &mut self.release_workspace {
            confirmation.render(frame, area, self.pending.is_some(), self.error.as_deref());
            return;
        }
        if let Some(draft) = &mut self.edit_management
            && !draft.picking()
        {
            draft.render(frame, area, self.pending.is_some(), self.error.as_deref());
            return;
        }
        if let Some(details) = &mut self.property_details {
            details.render(frame, area);
            return;
        }
        if let Some(draft) = &mut self.create_watch
            && !draft.picking
        {
            draft.render(frame, area, self.pending.is_some(), self.error.as_deref());
            return;
        }
        if self.inbox_details.id.is_some()
            && let Some(FlowHubPage::Inbox(page)) = &self.page
        {
            self.inbox_details.render(frame, area, page);
            return;
        }
        if self.watch_details.id.is_some()
            && let Some(FlowHubPage::Watchers(directory)) = &self.page
        {
            self.watch_details.render(
                frame,
                area,
                directory,
                self.pending.is_some(),
                self.error.as_deref(),
            );
            return;
        }
        if area.width < 40 || area.height < 24 {
            frame.render_widget(Paragraph::new("FlowHub needs 40×24 · Esc back"), area);
            self.hits.clear();
            self.float_hits.clear();
            self.previous = Rect::default();
            self.next = Rect::default();
            self.close = Rect::default();
            self.refresh = Rect::default();
            self.relations = Rect::default();
            self.sessions = Rect::default();
            self.graph_button = Rect::default();
            return;
        }
        let navigation = self.create_watch.is_none()
            && self.edit_management.is_none()
            && self.controls.is_none();
        let area = if navigation {
            let Some(content) = self
                .navigation
                .render(frame, area, focused, self.page.as_ref())
            else {
                return;
            };
            content
        } else {
            area
        };
        let width = area.width.saturating_sub(4).min(132);
        let vertical_inset = u16::from(area.height >= 28);
        let content = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + vertical_inset,
            width,
            area.height.saturating_sub(vertical_inset * 2),
        );
        let fg = Style::default().fg(t.heading.into());
        let muted = Style::default().fg(t.meta_fg.into());
        let session = self.page.as_ref().map_or("Loading", FlowHubPage::title);
        let is_flows = self.page.as_ref().is_some_and(FlowHubPage::nodes);
        let is_overview = matches!(self.page, Some(FlowHubPage::Overview(_)));
        let is_relations = matches!(self.page, Some(FlowHubPage::Relations { .. }));
        let is_watchers = matches!(self.page, Some(FlowHubPage::Watchers(_)));
        let is_workspaces = matches!(
            self.page,
            Some(
                FlowHubPage::Workspaces(_)
                    | FlowHubPage::Workspace(_)
                    | FlowHubPage::WorkspaceRecord(_)
                    | FlowHubPage::FlowWorkspace(_)
            )
        );
        let is_inbox = matches!(self.page, Some(FlowHubPage::Inbox(_)));
        let is_workspace_record = matches!(
            self.page,
            Some(FlowHubPage::WorkspaceRecord(_) | FlowHubPage::FlowWorkspace(_))
        );
        let is_processes = matches!(self.page, Some(FlowHubPage::ProcessSources { .. }));
        let picking = self.create_watch.is_some()
            || self.edit_management.is_some()
            || self.controls.is_some();
        let scoped_links = matches!(
            self.page,
            Some(FlowHubPage::Relations { focus: Some(_), .. })
        );
        frame.render_widget(
            Paragraph::new(format!(
                "{}  /  {session}",
                if let Some(draft) = &self.edit_management {
                    draft.picker_label()
                } else if self.controls.is_some() {
                    "Select control targets"
                } else if picking {
                    "Select source"
                } else {
                    "FlowHub"
                }
            ))
            .style(fg),
            Rect::new(content.x, content.y, width.saturating_sub(12), 1),
        );
        self.close = Rect::new(content.right() - 10, content.y, 10, 1);
        frame.render_widget(
            Paragraph::new("Back  Esc")
                .style(muted)
                .alignment(Alignment::Right),
            self.close,
        );
        let button_width = if width < 42 { (width - 2) / 2 } else { 20 };
        self.sessions = Rect::new(content.x, content.y + 1, button_width, 3);
        self.refresh = Rect::new(
            content.right() - button_width,
            content.y + 1,
            button_width,
            3,
        );
        if is_flows && !picking {
            let cell = (width - 2) / 3;
            self.sessions.width = cell;
            self.details_button = Rect::new(content.x + cell + 1, content.y + 1, cell, 3);
            self.refresh = Rect::new(
                content.x + 2 * (cell + 1),
                content.y + 1,
                width - 2 * (cell + 1),
                3,
            );
        }
        self.relations = if (is_flows || is_relations || is_watchers || is_inbox) && !picking {
            Rect::new(
                content.x + width / 2 + 1,
                content.y + 5,
                width - width / 2 - 1,
                3,
            )
        } else {
            Rect::default()
        };
        self.graph_button = if (picking || is_flows || is_relations || is_watchers)
            && self
                .edit_management
                .as_ref()
                .is_none_or(management::Draft::allows_overview)
        {
            Rect::new(content.x, content.y + 5, width / 2 - 1, 3)
        } else {
            Rect::default()
        };
        let mut toolbar_height = 3;
        if navigation && (is_flows || is_relations || is_watchers || is_inbox) {
            if !is_relations {
                self.sessions = Rect::default();
            }
            let count = [
                &self.sessions,
                &self.details_button,
                &self.graph_button,
                &self.relations,
                &self.refresh,
            ]
            .into_iter()
            .filter(|rect| !rect.is_empty())
            .count() as u16;
            let cell = width / count;
            if cell < 21 {
                toolbar_height = 4;
            }
            let mut x = content.x;
            for rect in [
                &mut self.sessions,
                &mut self.details_button,
                &mut self.graph_button,
                &mut self.relations,
                &mut self.refresh,
            ] {
                if rect.is_empty() {
                    continue;
                }
                *rect = Rect::new(x, content.y + 1, cell.saturating_sub(1), toolbar_height);
                x += cell;
            }
        }
        for (rect, label, hovered) in [
            (
                self.sessions,
                if is_relations {
                    if matches!(
                        self.page,
                        Some(FlowHubPage::Relations {
                            range: RelationRange::Session,
                            ..
                        })
                    ) {
                        "\nGlobal Ctrl+B"
                    } else {
                        "\nSession Ctrl+B"
                    }
                } else if is_flows && !picking && width < 60 {
                    "\nSessions ^S"
                } else {
                    "\nSessions  Ctrl+S"
                },
                self.sessions_hovered,
            ),
            (
                self.refresh,
                if is_flows && !picking && width < 60 {
                    "\nRefresh ^R"
                } else {
                    "\nRefresh  Ctrl+R"
                },
                self.refresh_hovered,
            ),
            (
                self.details_button,
                if width < 60 {
                    "\nDetails ^B"
                } else {
                    "\nDetails Ctrl+B"
                },
                self.details_hovered,
            ),
            (
                self.relations,
                if is_inbox {
                    match &self.page {
                        Some(FlowHubPage::Inbox(page))
                            if page.side
                                == atman_runtime::flow_store::inbox_view::InboxSide::Received =>
                        {
                            "\nSent Tab"
                        }
                        _ => "\nReceived Tab",
                    }
                } else if is_watchers {
                    if matches!(&self.page, Some(FlowHubPage::Watchers(directory)) if directory.global())
                    {
                        "\nThis Flow Ctrl+G"
                    } else {
                        "\nGlobal Ctrl+G"
                    }
                } else if is_relations {
                    self.endpoint.label()
                } else if width < 72 {
                    "\nLinks Ctrl+⇧G"
                } else {
                    "\nLinks Ctrl+Shift+G"
                },
                self.relations_hovered,
            ),
            (
                self.graph_button,
                if self.edit_management.is_some() || self.controls.is_some() {
                    "\nAll flows Ctrl+B"
                } else if picking {
                    if is_processes {
                        "\nFlows Ctrl+B"
                    } else {
                        "\nProcesses Ctrl+B"
                    }
                } else if is_watchers {
                    "\nNew  Ctrl+B"
                } else if is_relations {
                    if scoped_links {
                        "\nAll Ctrl+G"
                    } else {
                        "\nRelated Ctrl+G"
                    }
                } else if self.layout == Layout::Graph {
                    "\nList  Ctrl+G"
                } else {
                    "\nGraph  Ctrl+G"
                },
                self.graph_hovered,
            ),
        ] {
            if rect.is_empty() {
                continue;
            }
            let label = if rect.height == 4 {
                label.trim().rsplit_once(' ').map_or_else(
                    || label.to_owned(),
                    |(title, key)| {
                        format!(
                            "\n{title}\n{}",
                            key.replace("Ctrl+", "^").replace("Shift+", "⇧")
                        )
                    },
                )
            } else {
                label.to_owned()
            };
            frame.render_widget(
                Paragraph::new(label)
                    .style(muted.bg(if hovered {
                        t.work_bg.lerp(t.work_hover_bg, 0.5)
                    } else {
                        t.work_bg.into_inner()
                    }))
                    .alignment(Alignment::Center),
                rect,
            );
        }
        let search_y = content.y
            + if !navigation && (picking || is_flows || is_relations || is_watchers || is_inbox) {
                8
            } else {
                1 + toolbar_height
            };
        frame.render_widget(
            Paragraph::new(format!("Search {}: {}", if is_overview || is_workspaces || matches!(&self.page, Some(FlowHubPage::Watchers(directory)) if directory.global()) { "loaded records" } else { "this page" }, self.query)).style(muted),
            Rect::new(content.x, search_y, width, 1),
        );
        let rows_y = search_y + 2;
        let footer = content.bottom().saturating_sub(6);
        let rows = self.filtered();
        let graph_visible = is_flows && self.layout == Layout::Graph && !picking;
        let record_height = if is_flows || matches!(self.page, Some(FlowHubPage::ControlHistory(_)))
        {
            5
        } else {
            4
        };
        let visible = usize::from(footer.saturating_sub(rows_y) / record_height);
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        if self.selected >= self.offset + visible {
            self.offset = (self.selected + 1).saturating_sub(visible);
        }
        self.hits.clear();
        self.float_hits.clear();
        let mut group_bands = Vec::new();
        let cards = if is_overview {
            let layout = self.graph.grouped_cards(
                Rect::new(
                    content.x,
                    rows_y,
                    width,
                    footer.saturating_sub(rows_y + u16::from(graph_visible)),
                ),
                &self.rows,
                &rows,
                self.selected,
                graph_visible,
            );
            group_bands = layout.headings;
            layout.cards
        } else if graph_visible {
            self.graph.cards(
                Rect::new(content.x, rows_y, width, footer.saturating_sub(rows_y + 1)),
                &self.rows,
                &rows,
                self.selected,
            )
        } else if is_workspace_record {
            workspaces::cards(
                Rect::new(content.x, rows_y, width, footer.saturating_sub(rows_y + 1)),
                &self.rows,
                &rows,
                self.selected,
                &mut self.offset,
            )
        } else {
            rows.iter()
                .enumerate()
                .skip(self.offset)
                .take(visible)
                .map(|(pos, _)| {
                    (
                        pos,
                        Rect::new(
                            content.x,
                            rows_y + (pos - self.offset) as u16 * record_height,
                            width,
                            record_height,
                        ),
                    )
                })
                .collect()
        };
        if graph_visible {
            graph::draw_links(
                frame,
                &cards,
                self,
                &rows,
                Rect::new(content.x, footer.saturating_sub(1), width, 1),
            );
        }
        if let Some(FlowHubPage::Overview(directory)) = &self.page {
            for (session, rect) in group_bands {
                frame.render_widget(
                    Paragraph::new(format!(
                        "{} · {}",
                        directory.session_title(session),
                        &session.to_string()[28..]
                    ))
                    .style(muted),
                    rect,
                );
            }
        }
        for (pos, rect) in cards {
            let row = &self.rows[rows[pos]];
            let width = rect.width;
            let selected = pos == self.selected;
            let bg = if selected {
                t.work_hover_bg.into_inner()
            } else if self.hovered == Some(pos) {
                t.work_bg.lerp(t.work_hover_bg, 0.5)
            } else {
                t.work_bg.into_inner()
            };
            frame.render_widget(Paragraph::new("").style(Style::default().bg(bg)), rect);
            if selected {
                frame.render_widget(
                    Paragraph::new("▎\n".repeat(usize::from(rect.height)))
                        .style(Style::default().fg(t.accent.into()).bg(bg)),
                    Rect::new(rect.x, rect.y, 1, rect.height),
                );
            }
            let checked = self.controls.as_ref().is_some_and(|draft| matches!(&row.key, page::RowKey::Flow(address) if address.instance().is_some_and(|address| draft.contains(address))));
            let title = if checked {
                std::borrow::Cow::Owned(format!("✓ {}", row.title))
            } else if matches!(&row.action, Action::Focus(address) if address == focused) {
                std::borrow::Cow::Owned(format!("{} · focused", row.title))
            } else {
                std::borrow::Cow::Borrowed(row.title.as_str())
            };
            let label = if picking
                && matches!(row.key, page::RowKey::Process(_) | page::RowKey::Flow(_))
                || self.edit_management.is_some() && matches!(row.key, page::RowKey::Session(_))
            {
                Some(if checked {
                    "\nSelected Enter"
                } else {
                    "\nSelect Enter"
                })
            } else if matches!(
                row.key,
                page::RowKey::Watcher { .. } | page::RowKey::Delivery { .. }
            ) {
                Some("\nDetails Enter")
            } else if row.has_text_details() {
                Some("\nRead Enter")
            } else {
                match &row.action {
                    Action::RetryControls(_) => Some("\nRetry Enter"),
                    Action::Load(HubRequest::ControlBatch { .. }) => Some("\nDetails Enter"),
                    Action::Load(HubRequest::ControlHistory { .. }) => Some("\nHistory Enter"),
                    Action::Load(HubRequest::Sessions { .. }) => Some("\nSessions Enter"),
                    Action::Load(HubRequest::SessionRemoval { .. }) => Some("\nReview Enter"),
                    Action::Focus(_) => Some("\nWindow Ctrl+O"),
                    Action::Load(
                        HubRequest::Workspace { .. }
                        | HubRequest::WorkspaceRecord { .. }
                        | HubRequest::Management { .. },
                    ) => Some("\nInspect Enter"),
                    Action::UpdateManagement(_) => Some("\nRevoke Enter"),
                    Action::EditManagement | Action::EditManagementBinding => Some("\nEdit Enter"),
                    Action::OpenManagement => Some("\nOpen Enter"),
                    Action::Load(HubRequest::Workspaces { .. }) => Some("\nBrowse Enter"),
                    Action::SetWorkspaceRetention(_) => Some("\nApply Enter"),
                    Action::ReviewWorkspaceRelease(_) => Some("\nReview Enter"),
                    Action::Load(_) => Some("\nFlows  Enter"),
                    _ => None,
                }
            };
            let compact_action = width < 56 && matches!(row.action, Action::Focus(_)) && !picking;
            let label = if compact_action {
                Some("\nCtrl+O")
            } else {
                label
            };
            let action_width = if compact_action { 8 } else { 18 };
            frame.render_widget(
                Paragraph::new(title).style(fg.bg(bg)),
                Rect::new(
                    rect.x + 2,
                    rect.y + 1,
                    width.saturating_sub(if label.is_some() { action_width + 5 } else { 4 }),
                    1,
                ),
            );
            if let Some(label) = label {
                let open = Rect::new(
                    rect.right().saturating_sub(action_width + 2),
                    rect.y,
                    action_width,
                    3,
                );
                frame.render_widget(
                    Paragraph::new(label)
                        .alignment(Alignment::Center)
                        .style(muted.bg(bg)),
                    open,
                );
                self.float_hits.push((open, pos));
            }
            let detail = if graph_visible || is_flows && width < 72 {
                std::borrow::Cow::Owned(graph::compact_detail(row))
            } else if is_workspace_record {
                std::borrow::Cow::Owned(
                    crate::width::word_wrap(&row.detail, usize::from(width.saturating_sub(4)))
                        .join("\n"),
                )
            } else {
                std::borrow::Cow::Borrowed(row.detail.as_str())
            };
            frame.render_widget(
                Paragraph::new(detail).style(muted.bg(bg)),
                Rect::new(
                    rect.x + 2,
                    rect.y + 2,
                    width.saturating_sub(4),
                    rect.height.saturating_sub(3),
                ),
            );
            self.hits.push((rect, pos));
        }
        let total = if is_overview || is_relations || is_watchers || is_processes || is_workspaces {
            self.rows.len()
        } else {
            self.page.as_ref().map_or(0, FlowHubPage::total)
        };
        let page_count = total.div_ceil(PAGE_SIZE).max(1);
        let message = self.error.clone().unwrap_or_else(|| {
            if self.pending.is_some() {
                "Loading…".into()
            } else if matches!(self.page, Some(FlowHubPage::ControlBatch(_))) {
                format!(
                    "Snapshot · Page {} / {} · Ctrl+R refresh",
                    self.selected / visible.max(1) + 1,
                    rows.len().div_ceil(visible.max(1)).max(1)
                )
            } else if let Some(FlowHubPage::Overview(directory)) = &self.page {
                format!(
                    "{} · Item {} / {}",
                    if directory.page.next.is_some() {
                        "More PgDn"
                    } else {
                        "Snapshot"
                    },
                    (self.selected + 1).min(rows.len()),
                    rows.len()
                )
            } else if is_workspace_record {
                format!(
                    "Item {} / {} · Resource snapshot",
                    (self.selected + 1).min(rows.len()),
                    rows.len()
                )
            } else if is_workspaces {
                let more = self.page.as_ref().and_then(FlowHubPage::next).is_some();
                format!(
                    "Page {} / {} · {} matches · {}",
                    self.selected / visible.max(1) + 1,
                    rows.len().div_ceil(visible.max(1)).max(1),
                    rows.len(),
                    if more {
                        "More sources PgDn"
                    } else {
                        "Snapshot"
                    }
                )
            } else if is_inbox {
                let Some(FlowHubPage::Inbox(page)) = &self.page else {
                    unreachable!()
                };
                if width < 90 {
                    return format!(
                        "Page {} / {} · {total} records · {} matches{}",
                        self.index + 1,
                        page_count,
                        rows.len(),
                        if page.trailing_bytes > 0 {
                            " · partial tail"
                        } else {
                            ""
                        }
                    );
                }
                format!(
                    "Snapshot · {} records · Page {} / {} · {} matches · commit {}{}",
                    total,
                    self.index + 1,
                    page_count,
                    rows.len(),
                    page.cursor.sequence,
                    if page.trailing_bytes > 0 {
                        " · incomplete tail"
                    } else {
                        ""
                    }
                )
            } else if is_processes && total == 0 {
                "No running Bash/Terminal resources · Ctrl+R refresh".into()
            } else if is_processes {
                format!(
                    "Live resource snapshot · {total} processes · {} matches",
                    rows.len()
                )
            } else if is_watchers {
                let Some(FlowHubPage::Watchers(directory)) = &self.page else {
                    unreachable!()
                };
                format!(
                    "Page {}/{} · {}",
                    self.selected / visible.max(1) + 1,
                    rows.len().div_ceil(visible.max(1)).max(1),
                    directory.status(rows.len(), width < 90)
                )
            } else if is_relations {
                match &self.page {
                    Some(FlowHubPage::Relations {
                        progress: Some(progress),
                        ..
                    }) => format!(
                        "{} read / {} listed · {} errors · {} matches{}",
                        progress.scanned,
                        progress.total,
                        progress.errors.len(),
                        rows.len(),
                        if progress.next.is_some() {
                            " · More PgDn"
                        } else if progress.errors.is_empty() {
                            " · end"
                        } else {
                            " · partial"
                        }
                    ),
                    _ => format!("Session links · {total} records · {} matches", rows.len()),
                }
            } else {
                format!(
                    "Snapshot · {} {} · Page {} / {} · {} matches",
                    total,
                    self.page.as_ref().map_or("flows", FlowHubPage::unit),
                    self.index + 1,
                    page_count,
                    rows.len()
                )
            }
        });
        frame.render_widget(
            Paragraph::new(message).style(muted),
            Rect::new(content.x, footer, width, 1),
        );
        self.previous = Rect::new(content.x, footer + 1, width / 2 - u16::from(width > 1), 3);
        self.next = Rect::new(
            content.x + width / 2 + 1,
            footer + 1,
            width.saturating_sub(width / 2 + 1),
            3,
        );
        if (self.page.as_ref().is_some_and(FlowHubPage::nodes)
            || self.controls.is_some()
            || matches!(self.page, Some(FlowHubPage::Sessions(_))))
            && self.create_watch.is_none()
            && self.edit_management.is_none()
        {
            let cell = (width - 2) / 3;
            self.previous.width = cell;
            self.controls_button = Rect::new(content.x + cell + 1, footer + 1, cell, 3);
            self.next = Rect::new(
                content.x + 2 * (cell + 1),
                footer + 1,
                width - 2 * (cell + 1),
                3,
            );
            crate::settings_form::render_action(
                frame,
                self.controls_button,
                &crate::settings_form::Action {
                    label: if matches!(self.page, Some(FlowHubPage::Sessions(_))) {
                        "Delete Del"
                    } else if cell < 17 {
                        "Ctrl+T"
                    } else if self.controls.is_some() {
                        "Review Ctrl+T"
                    } else {
                        "Controls Ctrl+T"
                    },
                    hovered: self.controls_hovered,
                    disabled: self.pending.is_some(),
                    ..Default::default()
                },
                &t,
            );
        }
        for (rect, label) in [
            (
                self.previous,
                if self.previous.width < 17 {
                    "\n‹ PgUp"
                } else if is_overview
                    || is_relations
                    || is_watchers
                    || is_processes
                    || is_workspaces
                {
                    "\n‹ Earlier PgUp"
                } else {
                    "\n‹ Previous  PgUp"
                },
            ),
            (
                self.next,
                if self.next.width < 17 {
                    "\nPgDn ›"
                } else if is_overview
                    || is_relations
                    || is_watchers
                    || is_processes
                    || is_workspaces
                {
                    if self.page.as_ref().and_then(FlowHubPage::next).is_some() {
                        "\nMore PgDn ›"
                    } else {
                        "\nLater PgDn ›"
                    }
                } else {
                    "\nNext  PgDn ›"
                },
            ),
        ] {
            frame.render_widget(
                Paragraph::new(label)
                    .alignment(Alignment::Center)
                    .style(muted.bg(t.work_bg.into())),
                rect,
            );
        }
        if let Some(FlowHubPage::Inbox(page)) = &self.page {
            frame.render_widget(
                Paragraph::new(format!(
                    "Flow {} · read-only delivery history",
                    page::short_flow(&page.owner)
                ))
                .style(muted),
                Rect::new(content.x, footer + 4, width, 1),
            );
        }
        if let Some(FlowHubPage::Overview(directory)) = &self.page {
            frame.render_widget(
                Paragraph::new(directory.status()).style(muted),
                Rect::new(content.x, footer + 4, width, 1),
            );
        }
        if let Some(FlowHubPage::Watchers(directory)) = &self.page {
            frame.render_widget(
                Paragraph::new(format!(
                    "New recipient: {}",
                    page::short_flow(&directory.owner)
                ))
                .style(muted),
                Rect::new(content.x, footer + 4, width, 1),
            );
        }
        frame.render_widget(
            Paragraph::new(if picking {
                if self.controls.is_some() {
                    "Enter toggle · Ctrl+T review controls · Esc back"
                } else if self
                    .edit_management
                    .as_ref()
                    .is_some_and(|draft| !draft.allows_overview())
                {
                    "↑↓ select · Enter choose · Esc back"
                } else {
                    "↑↓ select · Enter choose · Ctrl+B sources · Esc back"
                }
            } else if matches!(
                self.page,
                Some(FlowHubPage::ControlBatch(_) | FlowHubPage::ControlHistory(_))
            ) {
                "Enter details · Ctrl+R refresh · Esc back"
            } else if is_workspaces {
                "↑↓ select · Enter inspect / focus · Ctrl+O float · Esc back"
            } else if is_inbox {
                "↑↓ select · Enter details · Tab side · Esc back"
            } else if is_watchers {
                "↑↓ select · Enter details · Ctrl+B new · Ctrl+G scope · Esc back"
            } else if matches!(self.page, Some(FlowHubPage::Sessions(_))) {
                "Enter open · Del review deletion · Ctrl+B overview · Esc back"
            } else if matches!(self.page, Some(FlowHubPage::SessionRemoval(_))) {
                "↑↓ select · Enter confirm / details · Ctrl+R refresh · Esc back"
            } else if self.controls_button.width > 0 {
                "Enter open · Ctrl+O window · Ctrl+T controls · Esc back"
            } else if is_relations {
                "↑↓ select · Tab endpoint · Enter open · Ctrl+O window · Esc back"
            } else {
                "↑↓ select · Enter open · Ctrl+O window · Ctrl+S sessions · Esc back"
            })
            .style(muted),
            Rect::new(content.x, footer + 5, width, 1),
        );
    }

    pub fn paste(&mut self, text: &str) -> Action {
        if self.navigation.owns_input() {
            return Action::None;
        }
        if let Some(FlowHubPage::SessionProject(page)) = &mut self.page {
            page.paste(text, self.pending.is_some());
            return Action::None;
        }
        if self.controls.as_ref().is_some_and(|draft| !draft.picking) {
            return Action::None;
        }
        if self
            .edit_management
            .as_ref()
            .is_some_and(|draft| !draft.picking())
        {
            return Action::None;
        }
        if self.release_workspace.is_some() {
            return Action::None;
        }
        if self.pending.is_none() {
            if let Some(draft) = &mut self.create_watch
                && !draft.picking
            {
                draft.paste(text);
            } else if self.watch_details.id.is_none()
                && self.inbox_details.id.is_none()
                && self.property_details.is_none()
            {
                self.query.extend(text.chars().filter(|c| !c.is_control()));
                self.selected = 0;
                self.offset = 0;
            }
        }
        Action::None
    }

    fn release_action(&mut self, action: Action) -> Action {
        if matches!(action, Action::Return) {
            self.release_workspace = None;
            Action::None
        } else {
            action
        }
    }

    fn close_controls(&mut self) -> Action {
        let Some(draft) = self.controls.take() else {
            return Action::None;
        };
        self.pending = None;
        self.error = None;
        if self
            .page
            .as_ref()
            .is_some_and(|page| page.first() == draft.origin)
        {
            Action::None
        } else {
            Action::Load(draft.origin)
        }
    }

    fn control_target(&self) -> Option<controls::Target> {
        let row = &self.rows[*self.filtered().get(self.selected)?];
        let page::RowKey::Flow(address) = &row.key else {
            return None;
        };
        self.page
            .as_ref()?
            .control_state(address.instance()?)
            .map(|state| controls::Target {
                title: row.title.clone(),
                state,
            })
    }
}

pub(crate) fn execution_label(
    state: &atman_rt::InstanceState<atman_runtime::event::AtmanUuid>,
) -> &'static str {
    match state {
        atman_rt::InstanceState::Prepared => "Prepared",
        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Idle) => "Idle",
        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(_)) => "Running",
        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Interrupted { .. }) => {
            "Interrupted"
        }
        atman_rt::InstanceState::Closing { .. } => "Closing",
        atman_rt::InstanceState::Closed => "Closed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::{
        event_log::journal::CommitReceipt,
        flow_runtime::access::{InstancePage, InstanceSummary},
        flow_store::FlowInstanceId,
    };
    use uuid::Uuid;

    #[test]
    fn global_relation_batches_preserve_cursor_reply_and_endpoint_ownership() {
        use atman_runtime::flow_store::{
            access::AccessTarget,
            relation::{FlowRelation, RelationKind},
        };
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let source = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let cursor = Uuid::now_v7();
        let page = |before, next, records| {
            Box::new(FlowHubPage::Relations {
                session: flow.session_id.0,
                title: "Global".into(),
                focus: Some(flow.clone()),
                range: RelationRange::Catalog { before },
                relations: records,
                progress: Some(RelationProgress {
                    scanned: 1,
                    total: 2,
                    next,
                    errors: vec![],
                }),
            })
        };
        let mut ticket = ControlTicket {
            binding: crate::control::ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: flow.clone().into(),
            },
            request_id: Uuid::now_v7(),
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(Some(&ticket), Ok(page(None, Some(cursor), vec![])));
        assert!(
            matches!(hub.key(KeyAction::PageDown), Action::Load(HubRequest::Relations { range: RelationRange::Catalog { before: Some(id) }, .. }) if id == cursor)
        );
        ticket.request_id = Uuid::now_v7();
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Err("temporary read failure".into()));
        assert!(hub.page.as_ref().unwrap().next().is_some());
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Ok(page(Some(Uuid::now_v7()), None, vec![])));
        assert!(hub.error.as_ref().unwrap().contains("cursor changed"));
        assert!(hub.page.as_ref().unwrap().next().is_some());
        let relation = FlowRelation {
            source: source.clone(),
            target: AccessTarget::Flow(flow.clone()),
            kind: RelationKind::Watch,
            id: Uuid::now_v7(),
            active: true,
        };
        let batch = || page(Some(cursor), None, vec![relation.clone()]);
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Ok(batch()));
        hub.receive(Some(&ticket), Ok(batch())); // The consumed request cannot append twice.
        assert_eq!(hub.rows.len(), 1);
        assert!(
            matches!(&hub.page, Some(FlowHubPage::Relations { progress: Some(progress), .. }) if progress.scanned == 2 && progress.next.is_none())
        );
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Focus(address) if address.instance() == Some(&source))
        );
        assert!(matches!(
            hub.key(KeyAction::CoursePrefill),
            Action::Load(HubRequest::Relations {
                range: RelationRange::Session,
                ..
            })
        ));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
        terminal
            .draw(|frame| hub.render(frame, &flow.clone().into()))
            .unwrap();
        let scope = hub.sessions;
        assert!(matches!(
            hub.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: scope.x + 1,
                row: scope.y + 1,
                modifiers: crossterm::event::KeyModifiers::NONE
            }),
            Action::Load(HubRequest::Relations {
                range: RelationRange::Session,
                ..
            })
        ));
        let close = hub.close;
        assert!(matches!(
            hub.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: close.x + 1,
                row: close.y,
                modifiers: crossterm::event::KeyModifiers::NONE
            }),
            Action::Return
        ));
        let mut refreshed = page(None, None, vec![]);
        if let FlowHubPage::Relations {
            progress: Some(progress),
            ..
        } = refreshed.as_mut()
        {
            progress
                .errors
                .push((source.session_id.0, "unreadable journal".into()));
        }
        ticket.request_id = Uuid::now_v7();
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Ok(refreshed));
        assert_eq!(hub.rows.len(), 1);
        assert!(hub.rows[0].title.contains("Unavailable"));
        assert_eq!(hub.rows[0].detail, "unreadable journal");
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Load(HubRequest::Flows { session, .. }) if session == source.session_id.0)
        );
    }

    #[test]
    fn neighborhood_navigation_commits_on_reply_and_restores_the_flow_directory() {
        use atman_runtime::flow_store::{
            access::AccessTarget,
            relation::{FlowRelation, RelationKind},
        };
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let observer = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let ticket = ControlTicket {
            binding: crate::control::ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: flow.clone().into(),
            },
            request_id: Uuid::now_v7(),
        };
        let page = || {
            Box::new(FlowHubPage::Flows {
                session: flow.session_id.0,
                title: "Example".into(),
                default_flow: None,
                page: InstancePage {
                    instances: vec![InstanceSummary {
                        archived: false,
                        address: flow.clone(),
                        entry: Some("Worker".into()),
                        state: None,
                        unavailable: None,
                        loaded: false,
                        trailing_bytes: None,
                        activity: None,
                    }],
                    next: None,
                    total: 51,
                    catalog: CommitReceipt {
                        journal_id: Uuid::now_v7(),
                        sequence: 1,
                        digest: "test".into(),
                    },
                },
                relations: Ok(vec![]),
            })
        };
        let links = || {
            Box::new(FlowHubPage::Relations {
                session: flow.session_id.0,
                title: "Links".into(),
                focus: Some(flow.clone()),
                range: RelationRange::Session,
                progress: None,
                relations: vec![FlowRelation {
                    source: observer.clone(),
                    target: AccessTarget::Flow(flow.clone()),
                    kind: RelationKind::Watch,
                    id: Uuid::now_v7(),
                    active: true,
                }],
            })
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        let mut history = History::default();
        history.receive(&mut hub, Some(&ticket), Ok(page()));
        let cursor = HubRequest::Flows {
            session: flow.session_id.0,
            before: Some(FlowInstanceId::now()),
        };
        hub.cursors.push(cursor.clone());
        hub.index = 1;
        hub.query = "Worker".into();
        hub.layout = Layout::Graph;
        assert!(!hub.leaves_directory(&cursor));
        assert!(
            matches!(hub.key(KeyAction::OpenRelations), Action::Load(HubRequest::Relations { focus: Some(target), .. }) if target == flow)
        );
        hub.pending = Some(ticket.clone());
        history.receive(&mut hub, Some(&ticket), Err("cannot read links".into()));
        assert_eq!(hub.query, "Worker");
        assert!(history.pages.is_empty());
        hub.pending = Some(ticket.clone());
        history.receive(&mut hub, Some(&ticket), Ok(links()));
        assert_eq!(history.pages.len(), 1);
        assert!(hub.query.is_empty());
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Focus(address) if address.instance() == Some(&observer))
        );
        hub.key(KeyAction::Tab); // Source.
        hub.key(KeyAction::Tab); // Target.
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 24)).unwrap();
        terminal
            .draw(|frame| hub.render(frame, &flow.clone().into()))
            .unwrap();
        let row = hub.hits[0].0;
        assert!(
            matches!(hub.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: row.x + 1, row: row.y, modifiers: crossterm::event::KeyModifiers::NONE }), Action::Focus(address) if address.instance() == Some(&flow))
        );
        let endpoint = hub.relations;
        hub.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: endpoint.x + 1,
            row: endpoint.y + 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        assert!(
            matches!(hub.key(KeyAction::ToggleLastTool), Action::Float(address) if address.instance() == Some(&observer))
        );
        let Action::Load(back) = hub.key(KeyAction::Escape) else {
            panic!("missing back target")
        };
        assert_eq!(history.request(back), cursor);
        let mut fresh = ticket.clone();
        fresh.request_id = Uuid::now_v7();
        hub.pending = Some(fresh.clone());
        history.receive(&mut hub, Some(&ticket), Ok(page()));
        assert!(matches!(hub.page, Some(FlowHubPage::Relations { .. })));
        history.receive(&mut hub, Some(&fresh), Ok(page()));
        assert_eq!(hub.index, 1);
        assert_eq!(hub.query, "Worker");
        assert!(hub.layout == Layout::Graph);
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Focus(address) if address.instance() == Some(&flow))
        );
    }

    #[test]
    fn refreshed_directory_retains_identity_and_return_state_without_reusing_requests() {
        use crate::control::{ViewAddress, ViewBinding};
        use crate::project_hub::SessionTarget;
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let address = ViewAddress::Instance(flow);
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: address.clone(),
            },
            request_id: Uuid::now_v7(),
        };
        let ids: Vec<_> = (0..20).map(|_| Uuid::now_v7()).collect();
        let page = |ids: &[Uuid]| {
            Box::new(FlowHubPage::Sessions(SessionDirectory {
                sessions: ids
                    .iter()
                    .map(|id| SessionRow {
                        cleanup_pending: false,
                        target: SessionTarget::Persistent(*id),
                        title: "Same title".into(),
                        detail: id.to_string(),
                    })
                    .collect(),
                next: Some(SessionTarget::Persistent(ids[ids.len() - 1])),
                total: 60,
            }))
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(Some(&ticket), Ok(page(&ids)));
        hub.query = "same".into();
        hub.selected = 7;
        hub.offset = 4;
        let mut refreshed = vec![Uuid::now_v7()];
        refreshed.extend_from_slice(&ids);
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Ok(page(&refreshed)));
        assert_eq!(hub.query, "same");
        assert_eq!(hub.offset, 4);
        assert_eq!(hub.selected, 8);
        assert!(
            matches!(hub.focus(), Action::Load(HubRequest::Flows {session,..}) if session == ids[7])
        );
        assert!(matches!(hub.key(KeyAction::PageDown), Action::Load(_)));
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Err("retry later".into()));
        assert_eq!(hub.query, "same");
        assert_eq!(hub.selected, 8);
        let mut history = History::default();
        hub.pending = Some(ticket.clone());
        history.save(hub);
        let (mut restored, action) = history.open(Action::Return, address.directory());
        assert!(matches!(action, Action::None));
        assert!(restored.pending.is_none());
        assert_eq!(restored.offset, 4);
        assert_eq!(restored.selected, 8);
        assert_eq!(restored.query, "same");
        restored.receive(Some(&ticket), Ok(page(&ids)));
        assert_eq!(
            restored.selected, 8,
            "a reply from the closed directory cannot mutate its restored view"
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
        terminal
            .draw(|frame| restored.render(frame, &address))
            .unwrap();
        let rect = restored
            .hits
            .iter()
            .find(|(_, index)| *index == restored.selected)
            .unwrap()
            .0;
        assert!(matches!(restored.mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left), column: rect.x + 1, row: rect.y + 1,
            modifiers: crossterm::event::KeyModifiers::NONE,
        }), Action::Load(HubRequest::Flows {session,..}) if session == ids[7]));
        history.save(restored);
        let (restored, action) = history.open(
            Action::Load(HubRequest::Sessions { before: None }),
            address.directory(),
        );
        assert_eq!(restored.query, "same");
        assert!(matches!(
            action,
            Action::Load(HubRequest::Sessions { before: None })
        ));
    }

    #[test]
    fn mixed_session_rows_keep_their_directory_targets_on_keyboard_and_mouse() {
        use crate::{
            control::{ViewAddress, ViewBinding},
            project_hub::SessionTarget,
        };
        let id = Uuid::now_v7();
        let address = ViewAddress::Legacy {
            session: id,
            owner: atman_runtime::event_log::legacy::LegacyFlowKey::Default,
        };
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: address.clone(),
            },
            request_id: Uuid::now_v7(),
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(
            Some(&ticket),
            Ok(Box::new(FlowHubPage::Sessions(SessionDirectory {
                sessions: vec![
                    SessionRow {
                        cleanup_pending: false,
                        target: SessionTarget::Persistent(id),
                        title: "Same name".into(),
                        detail: "Persistent".into(),
                    },
                    SessionRow {
                        cleanup_pending: false,
                        target: SessionTarget::Legacy(id.to_string()),
                        title: "Same name".into(),
                        detail: "Original history".into(),
                    },
                ],
                next: Some(SessionTarget::Legacy(id.to_string())),
                total: 51,
            }))),
        );
        assert!(
            matches!(hub.key(KeyAction::Submit),Action::Load(HubRequest::Flows {session,..}) if session==id)
        );
        hub.key(KeyAction::HistoryDown);
        assert!(
            matches!(hub.key(KeyAction::Submit),Action::Load(HubRequest::LegacyFlows {session,..}) if session==id)
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
        terminal.draw(|f| hub.render(f, &address)).unwrap();
        for (index, (rect, _)) in hub.hits.clone().into_iter().enumerate() {
            let action = hub.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 1,
                row: rect.y + 1,
                modifiers: crossterm::event::KeyModifiers::NONE,
            });
            match action {
                Action::Load(HubRequest::Flows { session, .. }) => {
                    assert_eq!(index, 0);
                    assert_eq!(session, id);
                }
                Action::Load(HubRequest::LegacyFlows { session, .. }) => {
                    assert_eq!(index, 1);
                    assert_eq!(session, id);
                }
                Action::Load(HubRequest::Overview { after: None }) => assert_eq!(index, 2),
                Action::Load(HubRequest::Management { grant: None }) => assert_eq!(index, 3),
                _ => panic!("wrong source"),
            }
        }
        assert!(
            matches!(hub.key(KeyAction::PageDown),Action::Load(HubRequest::Sessions {before:Some(SessionTarget::Legacy(cursor))}) if cursor==id.to_string())
        );
    }

    #[test]
    fn legacy_pages_preserve_addresses_across_keyboard_mouse_and_pagination() {
        use crate::control::{ViewAddress, ViewBinding};
        use atman_runtime::event_log::legacy::LegacyFlowKey;
        let session = Uuid::now_v7();
        let owner = LegacyFlowKey::Default;
        let address = ViewAddress::Legacy {
            session,
            owner: owner.clone(),
        };
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: address.clone(),
            },
            request_id: Uuid::now_v7(),
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(
            Some(&ticket),
            Ok(Box::new(FlowHubPage::LegacyFlows {
                session,
                title: "Original history".into(),
                flows: vec![owner.clone()],
                next: Some(owner.clone()),
                total: 51,
            })),
        );
        assert!(matches!(hub.key(KeyAction::Submit), Action::Focus(target) if target == address));
        assert!(
            matches!(hub.key(KeyAction::ToggleLastTool), Action::Float(target) if target == address)
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
        terminal.draw(|f| hub.render(f, &address)).unwrap();
        assert!(
            hub.page
                .as_ref()
                .unwrap()
                .rows(Some(&address), page::Endpoint::default())[0]
                .title
                .contains("focused")
        );
        for (rect, floating) in [(hub.hits[0].0, false), (hub.float_hits[0].0, true)] {
            for row in rect.y..rect.bottom() {
                let action = hub.mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: rect.x + 1,
                    row,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                });
                match action {
                    Action::Focus(target) => {
                        assert!(!floating);
                        assert_eq!(target, address);
                    }
                    Action::Float(target) => {
                        assert!(floating);
                        assert_eq!(target, address);
                    }
                    _ => panic!("missing row action"),
                }
            }
        }
        assert!(
            matches!(hub.key(KeyAction::PageDown), Action::Load(HubRequest::LegacyFlows {session:id, after:Some(key)}) if id==session && key==owner)
        );
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Err("source unavailable".into()));
        assert_eq!(hub.index, 0);
        assert!(matches!(hub.key(KeyAction::Escape), Action::Close));
    }

    #[test]
    fn session_directory_and_flow_pages_share_navigation_and_ignore_stale_replies() {
        use crate::project_hub::SessionTarget;
        use atman_runtime::flow_runtime::catalog::SessionSummary;
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let ticket = ControlTicket {
            binding: crate::control::ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: flow.clone().into(),
            },
            request_id: Uuid::now_v7(),
        };
        let session = Uuid::now_v7();
        let page = || {
            Box::new(FlowHubPage::Sessions(SessionDirectory {
                sessions: vec![SessionSummary::unavailable(session, "unavailable fixture").into()],
                next: Some(SessionTarget::Persistent(session)),
                total: 51,
            }))
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(Some(&ticket), Ok(page()));
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Load(HubRequest::Flows { session: id, before: None }) if id == session)
        );
        assert!(
            matches!(hub.key(KeyAction::PageDown), Action::Load(HubRequest::Sessions { before: Some(SessionTarget::Persistent(id)) }) if id == session)
        );
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Ok(page()));
        assert_eq!(hub.index, 1);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
        terminal
            .draw(|f| hub.render(f, &flow.clone().into()))
            .unwrap();
        let rect = hub.hits[0].0;
        for row in rect.y..rect.bottom() {
            assert!(
                matches!(hub.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: rect.x + 1, row, modifiers: crossterm::event::KeyModifiers::NONE }), Action::Load(HubRequest::Flows { session: id, .. }) if id == session)
            );
        }
        assert!(matches!(
            hub.key(KeyAction::Save),
            Action::Load(HubRequest::Sessions { before: None })
        ));
        let button = hub.sessions;
        for row in button.y..button.bottom() {
            assert!(matches!(
                hub.mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: button.x + 1,
                    row,
                    modifiers: crossterm::event::KeyModifiers::NONE
                }),
                Action::Load(HubRequest::Sessions { before: None })
            ));
        }
        hub.query = "keep this filter".into();
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Err("unreadable session".into()));
        assert_eq!(hub.index, 1);
        assert_eq!(hub.query, "keep this filter");
        hub.pending = Some(ticket.clone());
        let mut stale = ticket.clone();
        stale.request_id = Uuid::now_v7();
        hub.receive(Some(&stale), Ok(page()));
        assert_eq!(hub.query, "keep this filter");
        hub.receive(
            Some(&ticket),
            Ok(Box::new(FlowHubPage::Flows {
                session,
                title: "Chosen session".into(),
                default_flow: None,
                page: InstancePage {
                    instances: vec![],
                    next: None,
                    total: 0,
                    catalog: CommitReceipt {
                        journal_id: Uuid::now_v7(),
                        sequence: 1,
                        digest: "test".into(),
                    },
                },
                relations: Ok(vec![]),
            })),
        );
        assert_eq!(hub.index, 0);
        assert!(hub.query.is_empty());
        assert_eq!(
            hub.cursors,
            vec![HubRequest::Flows {
                session,
                before: None
            }]
        );
    }

    #[test]
    fn native_hub_pages_commit_on_ack_and_rows_have_equal_keyboard_and_mouse_targets() {
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let ticket = ControlTicket {
            binding: crate::control::ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: flow.clone().into(),
            },
            request_id: Uuid::now_v7(),
        };
        let page = || {
            Box::new(FlowHubPage::Flows {
                session: flow.session_id.0,
                title: "Example".into(),
                default_flow: Some(flow.instance_id.clone()),
                page: InstancePage {
                    instances: vec![InstanceSummary {
                        archived: false,
                        address: flow.clone(),
                        entry: Some("Example flow".into()),
                        state: None,
                        unavailable: None,
                        loaded: false,
                        trailing_bytes: None,
                        activity: None,
                    }],
                    next: Some(flow.instance_id.clone()),
                    total: 51,
                    catalog: CommitReceipt {
                        journal_id: Uuid::now_v7(),
                        sequence: 1,
                        digest: "test".into(),
                    },
                },
                relations: Ok(vec![]),
            })
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(Some(&ticket), Ok(page()));
        assert!(matches!(
            hub.key(KeyAction::PageDown),
            Action::Load(HubRequest::Flows {
                before: Some(_),
                ..
            })
        ));
        hub.pending = Some(ticket.clone());
        hub.receive(Some(&ticket), Err("read failed".into()));
        assert_eq!(hub.index, 0);
        assert!(hub.page.is_some());
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
        terminal
            .draw(|f| hub.render(f, &flow.clone().into()))
            .unwrap();
        let rect = hub.hits[0].0;
        assert_eq!(rect.height, 5);
        assert!(
            matches!(hub.key(KeyAction::Submit), Action::Focus(target) if target.instance() == Some(&flow))
        );
        assert!(
            matches!(hub.key(KeyAction::ToggleLastTool), Action::Float(target) if target.instance() == Some(&flow))
        );
        let window = hub.float_hits[0].0;
        for row in window.y..window.bottom() {
            assert!(
                matches!(hub.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: window.x + 1, row, modifiers: crossterm::event::KeyModifiers::NONE }), Action::Float(target) if target.instance() == Some(&flow))
            );
        }
        for row in rect.y..rect.bottom() {
            assert!(
                matches!(hub.mouse(MouseEvent { kind: MouseEventKind::Down(MouseButton::Left), column: rect.x + 1, row, modifiers: crossterm::event::KeyModifiers::NONE }), Action::Focus(target) if target.instance() == Some(&flow))
            );
        }
        for width in [40, 60, 76, 120] {
            let mut compact =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 24)).unwrap();
            compact
                .draw(|frame| hub.render(frame, &flow.clone().into()))
                .unwrap();
            let controls = [hub.details_button, hub.refresh, hub.relations];
            for (index, rect) in controls.iter().enumerate() {
                assert!(rect.height >= 3);
                assert!(rect.x >= 2 && rect.right() <= width - 2);
                assert!(rect.bottom() <= hub.hits[0].0.y);
                for other in controls.iter().skip(index + 1) {
                    assert!(rect.intersection(*other).is_empty());
                }
            }
            for row in hub.relations.y..hub.relations.bottom() {
                assert!(matches!(hub.mouse(MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: hub.relations.x + 1,
                    row,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                }), Action::Load(HubRequest::Relations { session, focus: Some(ref target), .. }) if session == flow.session_id.0 && target == &flow));
            }
        }
        hub.pending = Some(ticket.clone());
        let mut stale = ticket.clone();
        stale.request_id = Uuid::now_v7();
        hub.receive(Some(&stale), Err("stale".into()));
        assert_eq!(hub.pending.as_ref(), Some(&ticket));
        assert!(matches!(hub.key(KeyAction::Escape), Action::Close));
        let mut tiny = ratatui::Terminal::new(ratatui::backend::TestBackend::new(2, 2)).unwrap();
        tiny.draw(|f| hub.render(f, &flow.clone().into())).unwrap();
        assert!(hub.hits.is_empty());
    }

    #[test]
    fn relation_page_opens_flow_targets_from_keyboard_and_mouse() {
        use atman_runtime::flow_store::relation::{FlowRelation, RelationKind};

        let source = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let target = FlowRef {
            session_id: source.session_id,
            instance_id: FlowInstanceId::now(),
        };
        let ticket = ControlTicket {
            binding: crate::control::ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: source.clone().into(),
            },
            request_id: Uuid::now_v7(),
        };
        let mut hub = FlowHub {
            pending: Some(ticket.clone()),
            ..Default::default()
        };
        hub.receive(
            Some(&ticket),
            Ok(Box::new(FlowHubPage::Relations {
                session: source.session_id.0,
                title: "Example".into(),
                focus: None,
                range: RelationRange::Session,
                progress: None,
                relations: vec![FlowRelation {
                    source,
                    target: atman_runtime::flow_store::access::AccessTarget::Flow(target.clone()),
                    kind: RelationKind::Watch,
                    id: Uuid::now_v7(),
                    active: true,
                }],
            })),
        );
        assert!(matches!(
            hub.key(KeyAction::Submit),
            Action::Focus(crate::control::ViewAddress::Instance(address))
                if address.instance_id == target.instance_id
        ));
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 36)).unwrap();
        terminal
            .draw(|frame| hub.render(frame, &target.clone().into()))
            .unwrap();
        let row = hub.hits[0].0;
        assert!(matches!(
            hub.mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: row.x + 1,
                row: row.y + 1,
                modifiers: crossterm::event::KeyModifiers::NONE,
            }),
            Action::Focus(crate::control::ViewAddress::Instance(address))
                if address.instance_id == target.instance_id
        ));
    }
}
