use atman_runtime::goal::{GoalAction, GoalExecution, GoalSnapshot};
use atman_runtime::memory::goal::{GoalPauseReason, GoalStatus};

/// Task lifecycle and execution activity are represented without a shared `done` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    Running,
    Ready,
    GoalPaused(GoalPauseReason),
    Paused { by_parent: bool },
    Pausing { by_parent: bool },
    Finishing,
    Completed,
    Failed,
    Stopped,
    Interrupted,
}

impl AgentState {
    pub fn from_result(status: &str) -> Self {
        match status {
            "running" => Self::Running,
            "ok" => Self::Completed,
            "err" => Self::Failed,
            "killed" => Self::Stopped,
            _ => Self::Interrupted,
        }
    }

    pub fn from_goal(snapshot: &GoalSnapshot) -> Self {
        let by_parent = snapshot.paused_by.as_ref().is_some_and(|owner| {
            snapshot
                .goal
                .as_ref()
                .is_some_and(|goal| &goal.owner != owner)
        });
        match snapshot.execution {
            GoalExecution::Running => Self::Running,
            GoalExecution::Finishing => Self::Finishing,
            GoalExecution::Pausing => Self::Pausing { by_parent },
            GoalExecution::Paused => Self::Paused { by_parent },
            GoalExecution::Idle => snapshot
                .goal
                .as_ref()
                .filter(|goal| goal.status == GoalStatus::Paused)
                .and_then(|goal| goal.pause.as_ref())
                .map_or(Self::Ready, |pause| Self::GoalPaused(pause.reason)),
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Stopped | Self::Interrupted
        )
    }

    pub fn is_running(self) -> bool {
        matches!(self, Self::Running | Self::Pausing { .. } | Self::Finishing)
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Running => "Running",
            Self::Ready => "Ready",
            Self::GoalPaused(GoalPauseReason::Completed) => "Goal completed · idle",
            Self::GoalPaused(GoalPauseReason::NeedsInput) => "Waiting for input",
            Self::GoalPaused(GoalPauseReason::Blocked) => "Goal blocked · idle",
            Self::GoalPaused(_) => "Goal paused · idle",
            Self::Paused { by_parent: true } => "Paused by parent",
            Self::Paused { by_parent: false } => "Paused",
            Self::Pausing { by_parent: true } => "Pausing by parent",
            Self::Pausing { by_parent: false } => "Pausing",
            Self::Finishing => "Pausing · finishing turn",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Stopped => "Stopped",
            Self::Interrupted => "Interrupted",
        }
    }

    pub fn glyph(self) -> &'static str {
        match self {
            Self::Completed => "✓",
            Self::Failed => "✗",
            Self::Stopped => "⊘",
            Self::Interrupted => "⚠",
            Self::Running | Self::Finishing | Self::Pausing { .. } => "◐",
            Self::Ready => "○",
            Self::GoalPaused(_) | Self::Paused { .. } => "Ⅱ",
        }
    }
}

pub(crate) struct GoalPresentation {
    pub status: String,
    pub action: Option<GoalAction>,
    pub action_label: &'static str,
}

impl GoalPresentation {
    pub fn for_flow(entry: &atman_runtime::tools::agent_ctrl::FlowEntry) -> Option<Self> {
        let mut presentation = Self::new(&entry.goal_control.get()?.snapshot());
        let lifecycle = entry.status.lock().unwrap();
        if !lifecycle.is_running() {
            let state = AgentState::from_result(lifecycle.kind_str());
            presentation.status = state.label().into();
            presentation.action = None;
            presentation.action_label = state.label();
        }
        Some(presentation)
    }

    pub fn new(snapshot: &GoalSnapshot) -> Self {
        let state = AgentState::from_goal(snapshot);
        let blocked_by_parent = matches!(
            state,
            AgentState::Paused { by_parent: true } | AgentState::Pausing { by_parent: true }
        );
        let paused_goal = snapshot
            .goal
            .as_ref()
            .is_some_and(|goal| goal.status == GoalStatus::Paused);
        let status = if state == AgentState::Running && paused_goal {
            "Goal paused · running".into()
        } else if snapshot.goal.is_none() && state == AgentState::Ready {
            "No goal".into()
        } else {
            state.label().into()
        };
        let (action, action_label) = if blocked_by_parent {
            (None, "Parent paused")
        } else if snapshot.goal.is_none() {
            (None, "No goal")
        } else if matches!(
            state,
            AgentState::Paused { .. } | AgentState::Pausing { .. } | AgentState::Finishing
        ) || (paused_goal && !state.is_running())
        {
            (Some(GoalAction::Resume), "Resume")
        } else {
            (Some(GoalAction::Pause), "Pause")
        };
        Self {
            status,
            action,
            action_label,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::goal::GoalController;
    use atman_runtime::memory::goal::GoalActor;

    #[test]
    fn goal_and_execution_states_have_consistent_actions() {
        for reason in [
            GoalPauseReason::Completed,
            GoalPauseReason::NeedsInput,
            GoalPauseReason::Blocked,
        ] {
            let parent = GoalController::ephemeral("parent".into());
            parent.set("parent", None).unwrap();
            let child = parent.child("child".into(), "child".into(), None).unwrap();
            parent
                .pause(GoalActor::Agent, reason, "done for now".into(), None)
                .unwrap();
            assert_eq!(
                AgentState::from_goal(&parent.snapshot()),
                AgentState::GoalPaused(reason)
            );
            assert!(matches!(
                GoalPresentation::new(&parent.snapshot()).action,
                Some(GoalAction::Resume)
            ));
            assert_eq!(AgentState::from_goal(&child.snapshot()), AgentState::Ready);

            parent.begin_user_turn();
            let turn = parent.start_turn();
            assert_eq!(parent.snapshot().execution, GoalExecution::Running);
            let presentation = GoalPresentation::new(&parent.snapshot());
            assert_eq!(presentation.status, "Goal paused · running");
            assert!(matches!(presentation.action, Some(GoalAction::Pause)));
            drop(turn);
            assert_eq!(parent.snapshot().execution, GoalExecution::Idle);

            parent
                .pause(
                    GoalActor::User,
                    GoalPauseReason::UserRequested,
                    "stop".into(),
                    None,
                )
                .unwrap();
            let snapshot = child.snapshot();
            let state = AgentState::from_goal(&snapshot);
            assert_eq!(state, AgentState::Paused { by_parent: true });
            assert!(!state.is_terminal());
            assert!(!state.is_running());
            assert!(GoalPresentation::new(&snapshot).action.is_none());
            parent.resume(GoalActor::User, None).unwrap();
            assert!(matches!(
                GoalPresentation::new(&child.snapshot()).action,
                Some(GoalAction::Pause)
            ));
        }
    }

    #[test]
    fn exited_flow_cannot_be_resumed_from_its_retained_goal() {
        use atman_runtime::tools::agent_ctrl::FlowRunStatus;
        let session = atman_runtime::Session::open_ephemeral();
        session.goal_control.set("parent", None).unwrap();
        let child = session
            .goal_control
            .child("child".into(), "child".into(), None)
            .unwrap();
        let entry = session.flow_registry.create_entry(
            "child".into(),
            "child".into(),
            String::new(),
            atman_rt::RunId::now(),
        );
        entry.goal_control.set(child).ok().unwrap();
        for state in [
            FlowRunStatus::Ok {
                ended_at: chrono::Utc::now(),
                final_text: "done".into(),
            },
            FlowRunStatus::Err {
                ended_at: chrono::Utc::now(),
                message: "failed".into(),
            },
            FlowRunStatus::Killed {
                ended_at: chrono::Utc::now(),
            },
        ] {
            let expected = AgentState::from_result(state.kind_str());
            *entry.status.lock().unwrap() = state;
            let presentation = GoalPresentation::for_flow(&entry).unwrap();
            assert_eq!(presentation.status, expected.label());
            assert!(presentation.action.is_none());
            assert!(expected.is_terminal());
        }
    }
}
