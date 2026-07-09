use atman_runtime::event::TurnId;
use atman_runtime::workflow::{
    NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
};
use atman_tui::app::{AppState, OutputItem};
use std::collections::HashSet;

fn make_panel_item() -> OutputItem {
    let root = WorkflowNode {
        id: "r".into(),
        kind: WorkflowNodeKind::Flow {
            run_id: "r".into(),
            flow_name: "demo".into(),
        },
        label: "demo".into(),
        status: NodeStatus::Running,
        started_at: None,
        ended_at: None,
        output_preview: None,
        children: vec![WorkflowNode {
            id: "t".into(),
            kind: WorkflowNodeKind::ToolCall {
                tool_use_id: "t".into(),
                tool: "fs.read".into(),
                args_preview: "path=x".into(),
                result_preview: None,
            },
            label: "fs.read".into(),
            status: NodeStatus::Ok,
            started_at: None,
            ended_at: None,
            output_preview: None,
            children: Vec::new(),
            parallelism: Parallelism::Serial,
            approval: None,
        }],
        parallelism: Parallelism::Serial,
        approval: None,
    };
    let mut graph = WorkflowGraph::new(TurnId::now());
    graph.root.push(root);
    OutputItem::WorkflowPanel {
        turn_index: 0,
        graph,
        expanded_nodes: HashSet::new(),
        panel_expanded: true,
        started_at: std::time::Instant::now(),
        ended_at: None,
    }
}

#[test]
fn double_click_within_300ms_opens_viewer() {
    let mut app = AppState::new("s".into(), None);
    app.items.push(make_panel_item());
    assert!(!app.is_workflow_double_click(10, 5));
    app.remember_workflow_click(10, 5);
    assert!(app.is_workflow_double_click(10, 5));
    app.open_workflow_viewer(0);
    assert!(app.workflow_viewer.open);
    assert_eq!(app.workflow_viewer.panel_item_index, 0);
}

#[test]
fn double_click_requires_same_cell() {
    let mut app = AppState::new("s".into(), None);
    app.remember_workflow_click(10, 5);
    assert!(!app.is_workflow_double_click(11, 5));
    assert!(!app.is_workflow_double_click(10, 6));
}

#[test]
fn double_click_memory_cleared_when_needed() {
    let mut app = AppState::new("s".into(), None);
    app.remember_workflow_click(10, 5);
    app.clear_workflow_click_memory();
    assert!(!app.is_workflow_double_click(10, 5));
}

#[test]
fn esc_closes_viewer_but_preserves_offset() {
    let mut app = AppState::new("s".into(), None);
    app.workflow_viewer.last_content_width = 200;
    app.workflow_viewer.last_visible_cols = 100;
    app.open_workflow_viewer(2);
    app.workflow_viewer.scroll_right(30);
    assert_eq!(app.workflow_viewer.h_offset, 30);
    app.close_workflow_viewer();
    assert!(!app.workflow_viewer.open);
    app.open_workflow_viewer(2);
    assert_eq!(
        app.workflow_viewer.h_offset, 30,
        "reopening the same panel should preserve the offset"
    );
}

#[test]
fn opening_a_different_panel_resets_offset() {
    let mut app = AppState::new("s".into(), None);
    app.workflow_viewer.last_content_width = 200;
    app.workflow_viewer.last_visible_cols = 100;
    app.open_workflow_viewer(2);
    app.workflow_viewer.scroll_right(30);
    app.open_workflow_viewer(5);
    assert_eq!(app.workflow_viewer.h_offset, 0);
    assert_eq!(app.workflow_viewer.panel_item_index, 5);
}
