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
    ) -> io::Result<Self> {
        if &binding.flow != worker.address() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "view binding does not match flow",
            ));
        }
        let mut handle = Self::from_session(worker.context());
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
        Ok(handle)
    }
}

impl AppState {
    pub(crate) fn sync_flow_view(&mut self, view: &FlowView) -> io::Result<()> {
        let output = self.session.as_ref().map(|session| session.output_store());
        let before = self.items.revision_clock();
        self.items.sync_view(view, output.as_deref())?;
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
            let activity = match state.state() {
                atman_rt::InstanceState::Prepared => "Prepared",
                atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Idle) => "Idle",
                atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(_)) => "Running",
                atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Interrupted {
                    ..
                }) => "Interrupted",
                atman_rt::InstanceState::Closing { .. } => "Closing",
                atman_rt::InstanceState::Closed => "Closed",
            };
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
