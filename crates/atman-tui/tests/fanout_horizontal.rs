use atman_runtime::event::TurnId;
use atman_runtime::workflow::{
    NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
};
use atman_tui::app::{AppState, OutputItem};
use atman_tui::output::{LayoutCache, LayoutKey, RenderCtx, build_lines_with_ranges};
use std::collections::HashSet;

fn stmt_node(id: &str, label: &str) -> WorkflowNode {
    WorkflowNode {
        id: id.into(),
        kind: WorkflowNodeKind::Stmt {
            node_kind: atman_runtime::nodegraph::NodeKind::UserConfirm,
        },
        label: label.into(),
        status: NodeStatus::Ok,
        started_at: None,
        ended_at: None,
        output_preview: None,
        children: Vec::new(),
        parallelism: Parallelism::Serial,
        approval: None,
        llm_stats: None,
    }
}

fn fanout_branch(index: usize, leaves: Vec<WorkflowNode>) -> WorkflowNode {
    WorkflowNode {
        id: format!("b{index}"),
        kind: WorkflowNodeKind::FanoutBranch {
            branch_index: index,
        },
        label: format!("branch {index}"),
        status: NodeStatus::Ok,
        started_at: None,
        ended_at: None,
        output_preview: None,
        children: leaves,
        parallelism: Parallelism::Parallel,
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

use std::sync::Mutex;
static LEGACY_LOCK: Mutex<()> = Mutex::new(());

fn build_panel(
    root: WorkflowNode,
    width: u16,
) -> (Vec<atman_tui::output::NodeRegion>, Vec<String>) {
    let _guard = LEGACY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut graph = WorkflowGraph::new(TurnId::now());
    graph.root.push(root);
    let expanded_nodes: HashSet<String> = HashSet::new();
    let item = OutputItem::WorkflowPanel {
        turn_index: 0,
        graph,
        expanded_nodes,
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
    // SAFETY: env-var mutation guarded by LEGACY_LOCK across parallel tests.
    unsafe { std::env::set_var("ATMAN_LEGACY_WORKFLOW", "1") };
    let (lines, _ranges, regions, _rows) =
        build_lines_with_ranges(&[item], width, &ctx, &mut Vec::new(), None);
    unsafe { std::env::remove_var("ATMAN_LEGACY_WORKFLOW") };
    let flat: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    (regions, flat)
}

#[test]
fn boxed_wide_terminal_lays_fanout_branches_horizontally() {
    let branches = vec![
        fanout_branch(0, vec![stmt_node("a", "leaf-a")]),
        fanout_branch(1, vec![stmt_node("b", "leaf-b")]),
        fanout_branch(2, vec![stmt_node("c", "leaf-c")]),
    ];
    let root = root_flow(branches);
    let mut graph = WorkflowGraph::new(TurnId::now());
    graph.root.push(root);
    let expanded_nodes: HashSet<String> = HashSet::new();
    let item = OutputItem::WorkflowPanel {
        turn_index: 0,
        graph,
        expanded_nodes,
        panel_expanded: true,
        started_at: std::time::Instant::now(),
        ended_at: None,
        cancelled: false,
    };
    let ctx = RenderCtx {
        expanded_tools: &Default::default(),
        messages: &[],
        animation_frame: 0,
        panel_width: 200,
        hovered_thinking_idx: None,
    };
    let (lines, _ranges, regions, _rows) =
        build_lines_with_ranges(&[item], 200, &ctx, &mut Vec::new(), None);
    let flat: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    let joined = flat.join("\n");
    let horizontal_row = flat.iter().any(|l| {
        l.matches("branch[").count() >= 2 || (l.contains("branch[0]") && l.contains("branch[2]"))
    });
    assert!(
        horizontal_row,
        "expected 2+ branches on the same row, got:\n{joined}"
    );
    let branch_regions: Vec<_> = regions
        .iter()
        .filter(|r| r.path_key.contains('/'))
        .collect();
    let unique_col_starts: std::collections::HashSet<u16> =
        branch_regions.iter().map(|r| r.col_start).collect();
    assert!(
        unique_col_starts.len() >= 3,
        "expected at least 3 distinct column offsets for branch regions, got {}: {branch_regions:?}",
        unique_col_starts.len()
    );
}

#[test]
fn boxed_narrow_terminal_keeps_fanout_vertical() {
    let branches = vec![
        fanout_branch(0, vec![stmt_node("a", "leaf-a")]),
        fanout_branch(1, vec![stmt_node("b", "leaf-b")]),
    ];
    let root = root_flow(branches);
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
        panel_width: 80,
        hovered_thinking_idx: None,
    };
    let (lines, _ranges, _regions, _rows) =
        build_lines_with_ranges(&[item], 80, &ctx, &mut Vec::new(), None);
    let flat: Vec<String> = lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect();
    for l in &flat {
        assert!(
            l.matches("branch[").count() <= 1,
            "narrow panel should not put 2+ branches on the same row: {l:?}"
        );
    }
}

#[test]
fn wide_terminal_lays_fanout_branches_horizontally() {
    let branches = vec![
        fanout_branch(0, vec![stmt_node("a", "leaf-a")]),
        fanout_branch(1, vec![stmt_node("b", "leaf-b")]),
    ];
    let root = root_flow(branches);
    let (regions, lines) = build_panel(root, 200);
    let has_fork = lines.iter().any(|l| l.contains('┬'));
    let has_merge = lines.iter().any(|l| l.contains('┴'));
    assert!(has_fork, "expected fork line, lines={lines:?}");
    assert!(has_merge, "expected merge line, lines={lines:?}");
    let narrow = regions
        .iter()
        .filter(|r| r.col_end.saturating_sub(r.col_start) < 200)
        .count();
    assert!(
        narrow >= 2,
        "expected narrow (fanout) regions, got {} of {}",
        narrow,
        regions.len()
    );
}

#[test]
fn narrow_terminal_falls_back_to_vertical_fanout() {
    let branches = vec![
        fanout_branch(0, vec![stmt_node("a", "leaf-a")]),
        fanout_branch(1, vec![stmt_node("b", "leaf-b")]),
    ];
    let root = root_flow(branches);
    let (regions, lines) = build_panel(root, 80);
    let has_fork = lines.iter().any(|l| l.contains('┬'));
    assert!(!has_fork, "narrow width must not emit fork glyph");
    let narrow_cols = regions
        .iter()
        .filter(|r| r.col_end.saturating_sub(r.col_start) < 80)
        .count();
    assert_eq!(
        narrow_cols, 0,
        "vertical layout should not produce sub-width regions"
    );
}

#[test]
fn too_many_branches_fall_back_to_vertical() {
    let branches: Vec<WorkflowNode> = (0..5)
        .map(|i| fanout_branch(i, vec![stmt_node(&format!("leaf-{i}"), "leaf")]))
        .collect();
    let root = root_flow(branches);
    let (_regions, lines) = build_panel(root, 240);
    let has_fork = lines.iter().any(|l| l.contains('┬'));
    assert!(!has_fork, "5 branches must fall back to vertical layout");
}

#[test]
fn horizontal_hit_test_targets_the_right_branch() {
    use ratatui::layout::Rect;
    let branches = vec![
        fanout_branch(0, vec![stmt_node("a", "leaf-a")]),
        fanout_branch(1, vec![stmt_node("b", "leaf-b")]),
    ];
    let root = root_flow(branches);
    let (regions, _lines) = build_panel(root, 200);
    let mut app = AppState::new("s".into(), None);
    app.last_transcript_rect = Some(Rect::new(0, 0, 200, 30));
    app.last_node_regions = regions.clone();
    let branch_regions: Vec<_> = regions
        .iter()
        .filter(|r| r.col_end.saturating_sub(r.col_start) < 200)
        .collect();
    assert!(
        branch_regions.len() >= 4,
        "expected regions from each branch, got {branch_regions:?}"
    );
    let left = &branch_regions[0];
    let right = branch_regions
        .iter()
        .rev()
        .find(|r| r.col_start != left.col_start)
        .expect("distinct branch on the right");
    let hit_left = app
        .hit_test_node(left.col_start + 2, left.start_row)
        .expect("expected hit on left branch");
    let hit_right = app
        .hit_test_node(right.col_start + 2, right.start_row)
        .expect("expected hit on right branch");
    assert_ne!(
        hit_left.1, hit_right.1,
        "left and right branches must not collapse to the same path"
    );
}

#[test]
fn layout_cache_still_composes_valid_regions() {
    let branches = vec![
        fanout_branch(0, vec![stmt_node("a", "leaf-a")]),
        fanout_branch(1, vec![stmt_node("b", "leaf-b")]),
    ];
    let root = root_flow(branches);
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
        panel_width: 200,
        hovered_thinking_idx: None,
    };
    let mut cache = LayoutCache::default();
    let key = LayoutKey {
        items_version: 0,
        expanded_version: 0,
        width: 200,
        animation_frame: None,
    };
    let (_lines, ranges, _regions, total) = cache.get_or_build(key, &[item], &ctx, 0, 50);
    assert_eq!(ranges.len(), 1);
    assert!(total > 0);
    // regions collection TBD with virtual scroll
}
