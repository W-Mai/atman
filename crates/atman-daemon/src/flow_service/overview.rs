use std::io;

use anyhow::Result;
use atman_proto::{
    flows::{FlowAddress, FlowRequest},
    overview::*,
};
use atman_runtime::{
    flow_runtime::catalog::SessionCatalog,
    flow_store::{FlowOverviewFacts, workspace::WorkspaceSource},
    memory::{
        goal::{GoalPauseReason, GoalStatus},
        todo::TodoStatus,
    },
};
use serde_json::Value;

use crate::DaemonState;

const MAX_TEXT_BYTES: usize = 512;
const MAX_PATH_BYTES: usize = 4096;
const MAX_ITEMS: usize = 24;

pub(super) async fn snapshot(state: &DaemonState, request: FlowRequest) -> Result<Value> {
    let catalog = SessionCatalog::new(
        state
            .session_service
            .get()
            .map(|service| service.host.clone()),
        Some(state.data_dir().to_path_buf()),
    );
    let address = super::core_address(request.flow.clone());
    let response = catalog
        .flow_overview(
            &address,
            |_| Ok(()),
            move |facts| project(request.flow.clone(), facts),
        )
        .await?;
    Ok(serde_json::to_value(response)?)
}

fn clipped(value: &str, max: usize) -> OverviewText {
    let end = (0..=max.min(value.len()))
        .rev()
        .find(|&index| value.is_char_boundary(index))
        .unwrap_or(0);
    OverviewText {
        text: value[..end].to_owned(),
        truncated: end < value.len(),
    }
}

fn text(value: &str) -> OverviewText {
    clipped(value, MAX_TEXT_BYTES)
}

fn project(flow: FlowAddress, facts: FlowOverviewFacts<'_>) -> io::Result<FlowOverviewResponse> {
    let goal = facts.goal.map(|goal| OverviewGoal {
        id: goal.id.clone(),
        revision: goal.revision,
        objective: text(&goal.objective),
        status: match goal.status {
            GoalStatus::Active => OverviewGoalStatus::Active,
            GoalStatus::Paused => OverviewGoalStatus::Paused,
        },
        pause_reason: goal.pause.as_ref().map(|pause| match pause.reason {
            GoalPauseReason::UserRequested => OverviewGoalPauseReason::UserRequested,
            GoalPauseReason::ParentRequested => OverviewGoalPauseReason::ParentRequested,
            GoalPauseReason::Completed => OverviewGoalPauseReason::Completed,
            GoalPauseReason::NeedsInput => OverviewGoalPauseReason::NeedsInput,
            GoalPauseReason::Blocked => OverviewGoalPauseReason::Blocked,
            GoalPauseReason::Cancelled => OverviewGoalPauseReason::Cancelled,
            GoalPauseReason::RunFailed => OverviewGoalPauseReason::RunFailed,
            GoalPauseReason::Interrupted => OverviewGoalPauseReason::Interrupted,
        }),
    });
    let plan = facts.planning.plan(None).map(|plan| {
        let (done, total) = plan.progress();
        OverviewPlan {
            id: text(&plan.id),
            title: text(&plan.title),
            steps: plan
                .steps
                .iter()
                .take(MAX_ITEMS)
                .map(|step| OverviewPlanStep {
                    index: step.index,
                    text: text(&step.text),
                    done: step.done,
                })
                .collect(),
            done,
            total,
            steps_truncated: total > MAX_ITEMS,
        }
    });
    let mut todo_counts = OverviewTodoCounts::default();
    let mut todos = Vec::new();
    for todo in &facts.planning.todos {
        let status = match todo.status {
            TodoStatus::Pending => {
                todo_counts.pending += 1;
                OverviewTodoStatus::Pending
            }
            TodoStatus::InProgress => {
                todo_counts.in_progress += 1;
                OverviewTodoStatus::InProgress
            }
            TodoStatus::Done => {
                todo_counts.done += 1;
                OverviewTodoStatus::Done
            }
            TodoStatus::Cancelled => {
                todo_counts.cancelled += 1;
                OverviewTodoStatus::Cancelled
            }
        };
        if todos.len() < MAX_ITEMS {
            todos.push(OverviewTodo {
                id: todo.id.0,
                where_: text(&todo.where_),
                why: text(&todo.why),
                how: text(&todo.how),
                expected_result: text(&todo.expected_result),
                status,
            });
        }
    }
    let workspace = facts.workspace.map(|request| OverviewWorkspace {
        path: clipped(&request.binding.path.to_string_lossy(), MAX_PATH_BYTES),
        kind: match request.source {
            WorkspaceSource::Allocate { .. } => OverviewWorkspaceKind::Allocate,
            WorkspaceSource::Share { .. } => OverviewWorkspaceKind::Share,
        },
    });
    Ok(FlowOverviewResponse {
        flow,
        cursor: super::wire::cursor(facts.cursor)?,
        project_root: facts
            .project_root
            .map(|path| clipped(&path.to_string_lossy(), MAX_PATH_BYTES)),
        goal,
        planning: OverviewPlanning {
            revision: facts.planning.revision,
            plan,
            todos_truncated: facts.planning.todos.len() > todos.len(),
            todos,
            todo_counts,
        },
        workspace,
    })
}
