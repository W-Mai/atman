//! Persistent view attachment: one committed source and one immutable control address.

use std::{io, num::NonZeroUsize};

use atman_runtime::flow_runtime::{FlowView, FlowWorker};
use atman_runtime::projection::flow::LegacyFlowView;

use crate::{
    ControlSender, TuiControl, TuiHandle,
    app::AppState,
    control::{BoundControl, ViewAddress, ViewBinding},
};

mod preparation;
pub use preparation::ViewPreparation;

/// A live commit reader or a complete immutable archive, independent of surface placement.
pub enum FlowViewSource {
    Preparing(Box<ViewPreparation>),
    Live(Box<atman_runtime::flow_runtime::FlowViewReader>),
    Archive {
        view: Box<FlowView>,
        delivered: bool,
    },
    Legacy {
        view: Box<LegacyFlowView>,
        delivered: bool,
    },
}

pub enum FlowViewContent<'a> {
    Initial {
        document: Box<crate::flow_view::FlowViewModel>,
        history: Vec<String>,
    },
    Instance(&'a FlowView),
    Legacy(&'a LegacyFlowView),
}

impl<'a> From<&'a FlowView> for FlowViewContent<'a> {
    fn from(view: &'a FlowView) -> Self {
        Self::Instance(view)
    }
}

impl crate::flow_view::FlowViewModel {
    pub fn sync_content<'a>(
        &mut self,
        source: impl Into<FlowViewContent<'a>>,
        output: Option<&atman_runtime::tools::tool_output::OutputStore>,
    ) -> io::Result<bool> {
        match source.into() {
            FlowViewContent::Initial { document, .. } => {
                *self = *document;
                Ok(true)
            }
            FlowViewContent::Instance(view) => self.sync_view(view, output),
            FlowViewContent::Legacy(view) => self.sync_legacy(view, output),
        }
    }
}

impl FlowViewSource {
    pub(crate) fn address(&self) -> ViewAddress {
        match self {
            Self::Preparing(preparation) => preparation.address.clone(),
            Self::Live(reader) => reader.address().clone().into(),
            Self::Archive { view, .. } => view.transcript().flow().clone().into(),
            Self::Legacy { view, .. } => ViewAddress::Legacy {
                session: view.session_id(),
                owner: view.owner().clone(),
            },
        }
    }

    pub(crate) fn set_pending_offset(&mut self, offset: usize) {
        if let Self::Preparing(preparation) = self {
            preparation.set_pending_offset(offset);
        }
        if let Self::Live(reader) = self {
            reader.set_pending_offset(offset);
        }
    }
    pub async fn next(&mut self) -> io::Result<FlowViewContent<'_>> {
        if matches!(self, Self::Preparing(_)) {
            return self.finish_initial().await;
        }
        self.read_content().await
    }

    async fn read_content(&mut self) -> io::Result<FlowViewContent<'_>> {
        match self {
            Self::Preparing(_) => Err(io::Error::other("view source is already preparing")),
            Self::Live(reader) => reader.next().await.map(FlowViewContent::Instance),
            Self::Archive { view, delivered } if !*delivered => {
                *delivered = true;
                Ok(FlowViewContent::Instance(view))
            }
            Self::Legacy { view, delivered } if !*delivered => {
                *delivered = true;
                Ok(FlowViewContent::Legacy(view))
            }
            Self::Archive { .. } | Self::Legacy { .. } => std::future::pending().await,
        }
    }

    pub async fn refresh(&mut self) -> io::Result<FlowViewContent<'_>> {
        if matches!(self, Self::Preparing(_)) {
            return self.finish_initial().await;
        }
        self.refresh_content().await
    }

    async fn refresh_content(&mut self) -> io::Result<FlowViewContent<'_>> {
        match self {
            Self::Preparing(_) => Err(io::Error::other("view source is already preparing")),
            Self::Live(reader) => reader.refresh().await.map(FlowViewContent::Instance),
            Self::Archive { view, delivered } => {
                *delivered = true;
                Ok(FlowViewContent::Instance(view))
            }
            Self::Legacy { view, delivered } => {
                *delivered = true;
                Ok(FlowViewContent::Legacy(view))
            }
        }
    }

    fn current_content(&self) -> io::Result<FlowViewContent<'_>> {
        match self {
            Self::Live(reader) => reader
                .view()
                .map(FlowViewContent::Instance)
                .ok_or_else(|| io::Error::other("prepared source has no committed view")),
            Self::Archive { view, .. } => Ok(FlowViewContent::Instance(view)),
            Self::Legacy { view, .. } => Ok(FlowViewContent::Legacy(view)),
            Self::Preparing(_) => Err(io::Error::other("view is still preparing")),
        }
    }
}

impl TuiHandle {
    pub fn from_archive(
        view: FlowView,
        binding: ViewBinding,
        commands: tokio::sync::mpsc::UnboundedSender<BoundControl>,
        title: Option<String>,
        project: Option<String>,
    ) -> io::Result<Self> {
        if binding.flow.instance() != Some(view.transcript().flow())
            || !matches!(
                view.transcript().content(),
                Some(atman_runtime::flow_store::view::FlowContentSummary::History(_))
            )
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "archive view binding mismatch",
            ));
        }
        Ok(Self::history_view(
            FlowViewSource::Archive {
                view: Box::new(view),
                delivered: false,
            },
            binding,
            commands,
            title,
            project,
        ))
    }

    pub fn from_legacy(
        view: LegacyFlowView,
        binding: ViewBinding,
        commands: tokio::sync::mpsc::UnboundedSender<BoundControl>,
        title: Option<String>,
        project: Option<String>,
    ) -> io::Result<Self> {
        let expected = ViewAddress::Legacy {
            session: view.session_id(),
            owner: view.owner().clone(),
        };
        if binding.flow != expected {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "legacy view binding mismatch",
            ));
        }
        Ok(Self::history_view(
            FlowViewSource::Legacy {
                view: Box::new(view),
                delivered: false,
            },
            binding,
            commands,
            title,
            project,
        ))
    }

    fn history_view(
        source: FlowViewSource,
        binding: ViewBinding,
        commands: tokio::sync::mpsc::UnboundedSender<BoundControl>,
        title: Option<String>,
        project: Option<String>,
    ) -> Self {
        let session_id = binding.flow.session_id().to_string();
        let (sender, replies) = ControlSender::for_flow(binding, commands);
        Self {
            settings: crate::settings::SettingsSource::unavailable(
                "Archived flow has no execution settings",
            ),
            session_id,
            session_dir: String::new(),
            session_name: title,
            session_name_rx: None,
            continued_session: None,
            project_root: project,
            projects: Vec::new(),
            goal: None,
            stream_rx: None,
            task_event_rx: None,
            submit_tx: None,
            note_rx: None,
            shutdown_rx: None,
            control_tx: Some(sender),
            cmd_rx: None,
            control_reply_rx: Some(replies),
            flow_view: Some(source),
            live_output: None,
            activity: None,
            initial_items: Vec::new(),
            goal_rx: None,
            context_rx: None,
            attach_rx: None,
            todos_rx: None,
            plans_rx: None,
            trust_rx: None,
            compact_review_rx: None,
            form_rx: None,
            injection_rx: None,
            queued_submission_rx: None,
            flow_names: Vec::new(),
            session: None,
            startup_card: None,
            startup_intro: None,
            onboarding_recommended: false,
            trust: Default::default(),
            task_registry: None,
            permission_client: None,
            boot_toasts: Vec::new(),
        }
    }

    pub async fn from_flow(
        worker: &FlowWorker,
        binding: ViewBinding,
        commands: tokio::sync::mpsc::UnboundedSender<BoundControl>,
        config: atman_runtime::config_hub::ConfigHub,
    ) -> io::Result<Self> {
        if binding.flow.instance() != Some(worker.address()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "view binding does not match flow",
            ));
        }
        let mut handle = Self::from_session(worker.context());
        if !worker
            .context()
            .model_registry()
            .is_some_and(|models| std::sync::Arc::ptr_eq(models, config.models()))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "view configuration does not match flow model scope",
            ));
        }
        handle.settings = config.into();
        handle.activity = Some(worker.subscribe_activity().await?);
        handle.task_registry = Some(worker.resources().tasks().clone());
        handle.live_output = Some(worker.subscribe_live_output());
        handle.flow_view = Some(FlowViewSource::Live(Box::new(
            worker
                .subscribe_view(NonZeroUsize::new(64).unwrap())
                .await?,
        )));
        let (sender, replies) = ControlSender::for_flow(binding, commands);
        handle.control_tx = Some(sender);
        handle.control_reply_rx = Some(replies);
        // Persistent state is captured with the transcript, never from parallel watches.
        handle.goal_rx = None;
        handle.todos_rx = None;
        handle.plans_rx = None;
        handle.injection_rx = None;
        handle.queued_submission_rx = None;
        handle.attach_rx = None;
        Ok(handle)
    }
}

impl AppState {
    pub(crate) fn sync_live_output(
        &mut self,
        snapshot: atman_runtime::flow_runtime::live::LiveOutputSnapshot,
    ) -> io::Result<()> {
        if self.items.sync_live(snapshot)? {
            self.items_version = self.items_version.wrapping_add(1);
            self.streaming = self.items.has_live_output();
            self.mark_visual_dirty();
        }
        Ok(())
    }

    pub(crate) fn sync_instance_permissions(&mut self) {
        let Some(session) = self.session.as_ref() else {
            return;
        };
        let (requests, groups) = session
            .permission_broker()
            .user_list(&session.id().to_string());
        self.pending_permissions = requests
            .into_iter()
            .map(|request| {
                let memberships = groups
                    .iter()
                    .filter(|group| group.request_ids.contains(&request.request_id))
                    .map(|group| group.group_id.clone())
                    .collect();
                let payload = atman_runtime::permission_audit::PermissionRequestAudit::from_request(
                    &request,
                    memberships,
                    None,
                    request.requested_at,
                );
                (
                    request.request_id.clone(),
                    crate::app::PendingPermission {
                        request_id: request.request_id,
                        revision: request.revision,
                        payload,
                    },
                )
            })
            .collect();
        self.pending_permission_groups = groups
            .into_iter()
            .map(|group| {
                let payload = atman_runtime::permission_audit::PermissionGroupAudit::from_group(
                    &group,
                    &session.id().to_string(),
                    chrono::Utc::now(),
                );
                let expanded = self
                    .pending_permission_groups
                    .get(&group.group_id)
                    .is_some_and(|group| group.expanded);
                (
                    group.group_id.clone(),
                    crate::app::PendingPermissionGroup {
                        group_id: group.group_id,
                        revision: group.revision,
                        payload,
                        expanded,
                    },
                )
            })
            .collect();
        self.grouped_permission_request_ids = self
            .pending_permission_groups
            .values()
            .flat_map(|group| group.payload.request_ids.iter().cloned())
            .collect();
        self.mark_visual_dirty();
    }

    pub(crate) fn sync_flow_view<'a>(
        &mut self,
        view: impl Into<FlowViewContent<'a>>,
    ) -> io::Result<()> {
        let mut view = view.into();
        if let FlowViewContent::Initial { history, .. } = &mut view {
            self.prepared_input_history = Some(std::mem::take(history));
        }
        let output = self.session.as_ref().map(|session| session.output_store());
        let before = self.items.revision_clock();
        self.items.sync_content(view, output.as_deref())?;
        self.flow_loading = None;
        self.flow_view_error = None;
        if let Some(snapshot) = self.items.snapshot() {
            let flow = snapshot.status.execution.address().clone();
            let items = snapshot
                .pending
                .iter()
                .cloned()
                .map(|entry| crate::submission_queue::QueueItem::Instance {
                    flow: flow.clone(),
                    entry: Box::new(entry),
                })
                .collect();
            self.sync_queue(items);
        }
        if self.startup_card.is_some()
            && (self.items.has_work_input()
                || self.items.iter().any(|item| {
                    matches!(
                        item,
                        crate::app::OutputItem::UserTurn { .. }
                            | crate::app::OutputItem::Interjection { .. }
                    )
                }))
        {
            self.startup_card = None;
            self.startup_intro = None;
        }
        self.streaming = self.items.has_live_output();
        self.sync_workflow_activity();
        if self.items.revision_clock() != before {
            self.items_version = self.items_version.wrapping_add(1);
        }
        if let Some(snapshot) = self.items.snapshot() {
            self.goal = snapshot
                .status
                .goal
                .as_ref()
                .map(|goal| goal.objective.clone());
            self.todos = snapshot.planning.todos.clone();
            self.plans = snapshot.planning.plans.clone();
            let state = &snapshot.status.execution;
            let activity = crate::flow_hub::execution_label(state.state());
            self.status_notes.insert(
                "flow".into(),
                format!(
                    "{activity} · {} execution pauses · {} pending",
                    state.pauses().len(),
                    snapshot.pending_order.len()
                ),
            );
        }
        self.mark_visual_dirty();
        Ok(())
    }

    pub(crate) fn goal_record(&self) -> Option<atman_runtime::memory::goal::GoalRecord> {
        if let Some(snapshot) = self.items.snapshot() {
            return snapshot.status.goal.clone();
        }
        self.session
            .as_ref()
            .and_then(|session| session.goal_control.record())
    }

    pub(crate) fn goal_presentation(&self) -> Option<crate::agent_presentation::GoalPresentation> {
        if let Some(snapshot) = self.items.snapshot() {
            snapshot.status.goal.as_ref()?;
            return Some(crate::agent_presentation::GoalPresentation::for_instance(
                snapshot,
            ));
        }
        let snapshot = self.session.as_ref()?.goal_control.snapshot();
        snapshot.goal.as_ref()?;
        Some(crate::agent_presentation::GoalPresentation::new(&snapshot))
    }

    pub(crate) fn goal_toggle_control(&self) -> Option<TuiControl> {
        let goal = self.goal_record()?;
        let action = self.goal_presentation()?.action?;
        Some(TuiControl::GoalControl {
            request_id: None,
            target: None,
            goal_id: Some(goal.id),
            revision: Some(goal.revision),
            action,
        })
    }
}
