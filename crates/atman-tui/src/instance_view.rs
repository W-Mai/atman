//! Persistent view attachment: one committed source and one immutable control address.

use std::{io, num::NonZeroUsize};

use atman_runtime::flow_runtime::{FlowView, FlowWorker};

use crate::{
    ControlSender, TuiControl, TuiHandle,
    app::AppState,
    control::{BoundControl, ViewBinding},
};

impl TuiHandle {
    pub async fn from_flow(
        worker: &FlowWorker,
        binding: ViewBinding,
        commands: tokio::sync::mpsc::UnboundedSender<BoundControl>,
        config: atman_runtime::config_hub::ConfigHub,
    ) -> io::Result<Self> {
        if &binding.flow != worker.address() {
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
        handle.live_output = Some(worker.subscribe_live_output());
        handle.flow_view = Some(
            worker
                .subscribe_view(NonZeroUsize::new(64).unwrap())
                .await?,
        );
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

    pub(crate) fn sync_flow_view(&mut self, view: &FlowView) -> io::Result<()> {
        let output = self.session.as_ref().map(|session| session.output_store());
        let before = self.items.revision_clock();
        self.items.sync_view(view, output.as_deref())?;
        self.streaming = self.items.has_live_output();
        for &index in self.items.updated_workflows() {
            let crate::app::OutputItem::WorkflowPanel { graph, .. } = &self.items[index] else {
                continue;
            };
            self.last_workflow_panel_idx = Some(
                self.last_workflow_panel_idx
                    .map_or(index, |last| last.max(index)),
            );
            let mut nodes = graph.root.iter().collect::<Vec<_>>();
            while let Some(node) = nodes.pop() {
                if let atman_runtime::workflow::WorkflowNodeKind::Flow { run_id, .. } = &node.kind {
                    self.workflow_run_to_panel.insert(run_id.clone(), index);
                }
                nodes.extend(node.children.iter());
            }
        }
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
                    snapshot.pending.len()
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
