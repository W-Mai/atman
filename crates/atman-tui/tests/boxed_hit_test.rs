use atman_runtime::event::TurnId;
use atman_runtime::workflow::{
    NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
};
use atman_tui::app::{AppState, OutputItem};
use atman_tui::output::{NodeRegion, RenderCtx, build_lines_with_ranges};
use ratatui::layout::Rect;
use std::collections::HashSet;

fn tool_node(id: &str, label: &str, children: Vec<WorkflowNode>) -> WorkflowNode {
    WorkflowNode {
        id: id.into(),
        kind: WorkflowNodeKind::ToolCall {
            tool_use_id: id.into(),
            tool: label.into(),
            args_preview: String::new(),
            result_preview: None,
        },
        label: label.into(),
        status: NodeStatus::Ok,
        started_at: None,
        ended_at: None,
        output_preview: None,
        children,
        parallelism: Parallelism::Serial,
        approval: None,
        llm_stats: None,
    }
}

fn root_flow(children: Vec<WorkflowNode>) -> WorkflowNode {
    WorkflowNode {
        id: "root".into(),
        kind: WorkflowNodeKind::Flow {
            run_id: "r".into(),
            flow_name: "root".into(),
        },
        label: "root".into(),
        status: NodeStatus::Running,
        started_at: None,
        ended_at: None,
        output_preview: None,
        children,
        parallelism: Parallelism::Serial,
        approval: None,
        llm_stats: None,
    }
}

fn build_boxed(root: WorkflowNode, width: u16) -> Vec<NodeRegion> {
    let mut graph = WorkflowGraph::new(TurnId::now());
    graph.root.push(root);
    let item = OutputItem::WorkflowPanel {
        turn_index: 0,
        graph,
        expanded_nodes: HashSet::new(),
        panel_expanded: true,
        started_at: std::time::Instant::now(),
        ended_at: None,
        cancelled: false,
    };
    let ctx = RenderCtx {
        expanded_tools: &Default::default(),
        messages: &[],
        animation_frame: 0,
        panel_width: width,
        hovered_thinking_idx: None,
    };
    let (_lines, _ranges, regions, _rows) =
        build_lines_with_ranges(&[item], width, &ctx, &mut Vec::new(), None);
    regions
}

fn app_with(regions: Vec<NodeRegion>, width: u16, height: u16) -> AppState {
    let mut app = AppState::new("s".into(), None);
    app.last_transcript_rect = Some(Rect::new(0, 0, width, height));
    app.last_node_regions = regions;
    app
}

#[test]
fn boxed_hit_test_five_coordinates_map_to_correct_paths() {
    let root = root_flow(vec![
        tool_node(
            "a",
            "step-a",
            vec![tool_node("a-1", "child-of-a", Vec::new())],
        ),
        tool_node("b", "step-b", Vec::new()),
    ]);
    let regions = build_boxed(root, 120);
    let app = app_with(regions.clone(), 120, 60);

    let flow = regions
        .iter()
        .find(|r| r.path_key == "0")
        .expect("flow region");
    let a = regions
        .iter()
        .find(|r| r.path_key == "0/0")
        .expect("step-a region");
    let a_child = regions
        .iter()
        .find(|r| r.path_key == "0/0/0")
        .expect("step-a child region");
    let b = regions
        .iter()
        .find(|r| r.path_key == "0/1")
        .expect("step-b region");

    let hit_flow = app
        .hit_test_node(flow.col_start + 2, flow.start_row + 1)
        .expect("flow top-left interior");
    assert_eq!(hit_flow.1, "0", "top-left of flow box");

    let hit_a_body = app
        .hit_test_node(a.col_start + 3, a.start_row + 1)
        .expect("step-a interior");
    assert_eq!(hit_a_body.1, "0/0", "interior of step-a");

    let hit_a_border = app
        .hit_test_node(a.col_start, a.start_row)
        .expect("step-a top-left border");
    assert_eq!(hit_a_border.1, "0/0", "top-left border still counts");

    let hit_child = app
        .hit_test_node(a_child.col_start + 2, a_child.start_row + 1)
        .expect("nested child interior");
    assert_eq!(hit_child.1, "0/0/0", "path-key length tiebreaker wins");

    let hit_b = app
        .hit_test_node(b.col_start + 2, b.start_row + 1)
        .expect("step-b interior");
    assert_eq!(hit_b.1, "0/1", "second sibling");
}

#[test]
fn boxed_edge_fixture_narrow_deep_tree_stays_valid() {
    let mut deepest = tool_node("d6", "level-6", Vec::new());
    for depth in (0..6).rev() {
        deepest = tool_node(
            &format!("d{depth}"),
            &format!("level-{depth}"),
            vec![deepest],
        );
    }
    let root = root_flow(vec![deepest]);
    let regions = build_boxed(root, 50);

    assert!(
        regions.len() >= 7,
        "expected at least 7 regions for depth-6 tree, got {}",
        regions.len()
    );
    for r in &regions {
        assert!(
            r.col_end > r.col_start,
            "empty box for {:?}: {}..{}",
            r.path_key,
            r.col_start,
            r.col_end
        );
        assert!(
            r.col_end <= 50,
            "box overflows panel_width=50 for {:?}: col_end={}",
            r.path_key,
            r.col_end
        );
        assert!(
            r.end_row > r.start_row,
            "empty row range for {:?}",
            r.path_key
        );
    }
}
