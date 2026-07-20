//! Preview fixture: dump the boxed workflow with three simultaneous pending
//! approvals so a human can eyeball the layout. Ignored by default; run with:
//!
//!     cargo test -p atman-tui --test workflow_preview -- --ignored --nocapture

use atman_runtime::event::TurnId;
use atman_runtime::workflow::{
    ApprovalState, NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
};
use atman_tui::app::OutputItem;
use atman_tui::output::{RenderCtx, build_lines_with_ranges};
use std::collections::HashSet;

fn pending_tool(id: &str, tool: &str, args: &str, level: &str) -> WorkflowNode {
    WorkflowNode {
        id: id.into(),
        kind: WorkflowNodeKind::ToolCall {
            tool_use_id: id.into(),
            tool: tool.into(),
            args_preview: args.into(),
            result_preview: None,
        },
        label: tool.into(),
        status: NodeStatus::Pending,
        started_at: None,
        ended_at: None,
        output_preview: None,
        children: Vec::new(),
        parallelism: Parallelism::Serial,
        llm_stats: None,
        approval: Some(ApprovalState::Pending {
            level: level.into(),
            preview: None,
        }),
    }
}

fn ok_tool(id: &str, tool: &str, args: &str, result: &str) -> WorkflowNode {
    WorkflowNode {
        id: id.into(),
        kind: WorkflowNodeKind::ToolCall {
            tool_use_id: id.into(),
            tool: tool.into(),
            args_preview: args.into(),
            result_preview: Some(result.into()),
        },
        label: tool.into(),
        status: NodeStatus::Ok,
        started_at: None,
        ended_at: None,
        output_preview: None,
        children: Vec::new(),
        parallelism: Parallelism::Serial,
        llm_stats: None,
        approval: Some(ApprovalState::Approved),
    }
}

fn root_flow(children: Vec<WorkflowNode>) -> WorkflowNode {
    WorkflowNode {
        id: "root".into(),
        kind: WorkflowNodeKind::Flow {
            run_id: "r".into(),
            flow_name: "demo_flow".into(),
        },
        label: "demo_flow".into(),
        status: NodeStatus::Running,
        started_at: None,
        ended_at: None,
        output_preview: None,
        children,
        parallelism: Parallelism::Serial,
        llm_stats: None,
        approval: None,
    }
}

fn render_boxed_to_string(root: WorkflowNode, width: u16) -> String {
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
        expanded_tools: &HashSet::new(),
        messages: &[],
        animation_frame: 0,
        panel_width: width,
        hovered_thinking_idx: None,
    };
    let (lines, _, _, _) = build_lines_with_ranges(&[item], width, &ctx, &mut Vec::new(), None);
    lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
#[ignore]
fn preview_collapsed_workflow_card() {
    let root = root_flow(vec![
        ok_tool("t0", "fs.read", "path=lib.rs", "ok · 200 lines"),
        pending_tool("t1", "agent.spawn", "goal=research auth flow", "auto"),
        pending_tool("t2", "shell.exec", "cargo test -p atman-tui", "dangerous"),
        pending_tool("t3", "fs.edit", "src/output.rs:120..145", "approve"),
        ok_tool("t4", "fs.write", "path=log.txt", "wrote 42 bytes"),
    ]);
    let mut graph = WorkflowGraph::new(TurnId::now());
    graph.root.push(root);
    let item = OutputItem::WorkflowPanel {
        turn_index: 0,
        graph,
        expanded_nodes: HashSet::new(),
        panel_expanded: false,
        started_at: std::time::Instant::now(),
        ended_at: None,
        cancelled: false,
    };
    let ctx = RenderCtx {
        expanded_tools: &HashSet::new(),
        messages: &[],
        animation_frame: 0,
        panel_width: 100,
        hovered_thinking_idx: None,
    };
    let (lines, _ranges, _regions, _rows) =
        atman_tui::output::build_lines_with_ranges(&[item], 100, &ctx, &mut Vec::new(), None);
    let output: String = lines
        .iter()
        .map(|l| {
            l.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n");
    println!();
    println!("=== collapsed workflow card (panel_expanded=false, panel_width=100) ===");
    println!("{output}");
    println!();
}

#[test]
#[ignore]
fn preview_fanout_branches_after_flip() {
    use atman_runtime::workflow::WorkflowNodeKind;
    fn branch(i: usize, leaves: Vec<WorkflowNode>) -> WorkflowNode {
        WorkflowNode {
            id: format!("b{i}"),
            kind: WorkflowNodeKind::FanoutBranch { branch_index: i },
            label: format!("branch {i}"),
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
    let root = root_flow(vec![
        branch(0, vec![ok_tool("l0", "fs.read", "path=a", "ok")]),
        branch(1, vec![ok_tool("l1", "fs.read", "path=b", "ok")]),
        branch(2, vec![ok_tool("l2", "fs.read", "path=c", "ok")]),
    ]);
    let output = render_boxed_to_string(root, 160);
    println!();
    println!("=== boxed workflow · 3-branch fanout at width 160 ===");
    println!("{output}");
    println!();
}

#[test]
#[ignore]
fn preview_three_pending_approvals_side_by_side() {
    let root = root_flow(vec![
        ok_tool("t0", "fs.read", "path=README.md", "ok · 128 lines"),
        pending_tool("t1", "shell.exec", "rm -rf ./tmp/staging", "dangerous"),
        pending_tool("t2", "shell.exec", "chmod +x deploy.sh", "dangerous"),
        pending_tool(
            "t3",
            "http.post",
            "https://api.example.com/prod/apply",
            "dangerous",
        ),
        ok_tool("t4", "fs.write", "path=./log.txt", "wrote 42 bytes"),
    ]);
    let output = render_boxed_to_string(root, 100);
    println!();
    println!(
        "=== boxed workflow · 3 concurrent pending approvals ({} chars wide) ===",
        100
    );
    println!("{output}");
    println!();
}
