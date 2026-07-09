//! W0 baseline for workflow-widgetize spec.
//!
//! Ignored by default. Run with:
//!     cargo test -p atman-tui --test workflow_render_bench -- --ignored --nocapture
//!
//! Prints per-frame ms and section breakdowns at 100 / 500 / 1000 nodes.
//! Numbers here set the budget for W3 stress test (target: <= 2x baseline).

use std::collections::HashSet;
use std::time::Instant;

use atman_runtime::event::TurnId;
use atman_runtime::nodegraph::NodeKind;
use atman_runtime::workflow::{
    NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
};
use atman_tui::app::OutputItem;
use atman_tui::output::{RenderCtx, build_lines_with_ranges};

fn make_tool_node(idx: usize, running: bool) -> WorkflowNode {
    WorkflowNode {
        id: format!("tool:run-a:tuse-{idx}"),
        kind: WorkflowNodeKind::ToolCall {
            tool_use_id: format!("tuse-{idx}"),
            tool: format!("fs.read_{}", idx % 4),
            args_preview: format!(
                "{{\"path\":\"src/lib_{idx}.rs\",\"start\":1,\"limit\":40,\"n\":{idx}}}"
            ),
            result_preview: Some(format!(
                "lines 1..40 from src/lib_{idx}.rs — some content about the file at index {idx} that spans a few characters"
            )),
        },
        label: format!("fs.read_{}", idx % 4),
        status: if running {
            NodeStatus::Running
        } else if idx % 5 == 0 {
            NodeStatus::Err
        } else {
            NodeStatus::Ok
        },
        started_at: Some(chrono::Utc::now()),
        ended_at: if running {
            None
        } else {
            Some(chrono::Utc::now())
        },
        output_preview: Some(format!(
            "output preview for node {idx} — usually a snippet of stdout or the tool return value truncated to ~200 chars"
        )),
        children: Vec::new(),
        parallelism: Parallelism::Serial,
        approval: None,
    }
}

fn make_stmt_wrapper(kind: NodeKind, children: Vec<WorkflowNode>) -> WorkflowNode {
    WorkflowNode {
        id: format!("run-a::stmt-{}", children.len()),
        kind: WorkflowNodeKind::Stmt { node_kind: kind },
        label: "step".into(),
        status: NodeStatus::Ok,
        started_at: Some(chrono::Utc::now()),
        ended_at: Some(chrono::Utc::now()),
        output_preview: None,
        children,
        parallelism: Parallelism::Serial,
        approval: None,
    }
}

fn build_workflow_of_size(target: usize) -> WorkflowGraph {
    let mut tools = Vec::with_capacity(target);
    for i in 0..target {
        tools.push(make_tool_node(i, i == target - 1));
    }
    let mut cursor = tools;
    while cursor.len() > 1 {
        let chunk_size = 8.min(cursor.len());
        let chunk: Vec<_> = cursor.drain(..chunk_size).collect();
        let stmt = make_stmt_wrapper(
            NodeKind::ToolCall {
                path: "fs.read".into(),
            },
            chunk,
        );
        cursor.push(stmt);
    }
    let root = WorkflowNode {
        id: "run-a".into(),
        kind: WorkflowNodeKind::Flow {
            run_id: uuid::Uuid::now_v7().to_string(),
            flow_name: "bench_flow".into(),
        },
        label: "bench_flow".into(),
        status: NodeStatus::Running,
        started_at: Some(chrono::Utc::now()),
        ended_at: None,
        output_preview: None,
        children: cursor,
        parallelism: Parallelism::Serial,
        approval: None,
    };
    WorkflowGraph {
        turn_id: TurnId::now(),
        root: vec![root],
    }
}

fn run_bench(node_count: usize, iterations: u32) -> (f64, usize) {
    let graph = build_workflow_of_size(node_count);
    let items = vec![OutputItem::WorkflowPanel {
        turn_index: 0,
        graph,
        expanded_nodes: HashSet::new(),
        panel_expanded: true,
        started_at: std::time::Instant::now(),
        ended_at: None,
    }];
    let ctx = RenderCtx {
        expanded_tools: &HashSet::new(),
        messages: &[],
        animation_frame: 0,
        panel_width: 120,
    };
    let (lines, _, _, _) = build_lines_with_ranges(&items, 120, &ctx);
    let line_count = lines.len();
    let start = Instant::now();
    for _ in 0..iterations {
        let _ = build_lines_with_ranges(&items, 120, &ctx);
    }
    let ms = start.elapsed().as_secs_f64() * 1000.0 / iterations as f64;
    (ms, line_count)
}

#[test]
#[ignore]
fn baseline_100_500_1000_nodes() {
    for &n in &[100usize, 500, 1000] {
        let (ms, lines) = run_bench(n, 10);
        println!("{n:>5} nodes -> {lines:>6} lines -> {ms:>8.3} ms / frame");
    }
}

#[test]
fn boxed_1000_nodes_stays_under_budget() {
    // SAFETY: cargo test defaults to a single-threaded runner per binary
    // in this workspace, and this test never spawns threads; env-var
    // mutation is limited to its own scope.
    unsafe { std::env::set_var("ATMAN_BOXED_WORKFLOW", "1") };
    let (ms, _lines) = run_bench(1000, 5);
    unsafe { std::env::remove_var("ATMAN_BOXED_WORKFLOW") };
    let budget_ms = 20.0;
    assert!(
        ms < budget_ms,
        "boxed rendering regressed: {ms:.3} ms/frame (budget {budget_ms})"
    );
}
