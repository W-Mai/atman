use atman_runtime::event::TurnId;
use atman_runtime::workflow::{
    ApprovalState, NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
};
use atman_tui::app::OutputItem;
use atman_tui::output::{RenderCtx, build_lines_with_ranges};
use std::collections::HashSet;

fn tool_with_approval(
    id: &str,
    tool: &str,
    args_preview: &str,
    approval: Option<ApprovalState>,
) -> WorkflowNode {
    WorkflowNode {
        id: id.into(),
        kind: WorkflowNodeKind::ToolCall {
            tool_use_id: id.into(),
            tool: tool.into(),
            args_preview: args_preview.into(),
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
        approval,
    }
}

fn root_flow(children: Vec<WorkflowNode>) -> WorkflowNode {
    WorkflowNode {
        id: "root".into(),
        kind: WorkflowNodeKind::Flow {
            run_id: "r".into(),
            flow_name: "flow".into(),
        },
        label: "flow".into(),
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

fn render_boxed(root: WorkflowNode, width: u16) -> Vec<String> {
    let mut graph = WorkflowGraph::new(TurnId::now());
    graph.root.push(root);
    let item = OutputItem::WorkflowPanel {
        turn_index: 0,
        graph,
        expanded_nodes: HashSet::new(),
        panel_expanded: true,
        started_at: std::time::Instant::now(),
        ended_at: None,
    };
    let ctx = RenderCtx {
        expanded_tools: &HashSet::new(),
        messages: &[],
        animation_frame: 0,
        panel_width: width,
    };
    let (lines, _, _, _) = build_lines_with_ranges(&[item], width, &ctx);
    lines
        .iter()
        .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
}

#[test]
fn pending_approval_auto_expands_box_and_shows_args() {
    let root = root_flow(vec![tool_with_approval(
        "a",
        "shell.exec",
        "rm -rf /tmp/staging",
        Some(ApprovalState::Pending {
            level: "high".into(),
            preview: None,
        }),
    )]);
    let rendered = render_boxed(root, 80);
    let joined = rendered.join("\n");
    assert!(
        joined.contains("shell.exec"),
        "tool label missing: {joined}"
    );
    assert!(
        joined.contains("args:"),
        "auto-expand should render args section header: {joined}"
    );
    assert!(
        joined.contains("rm -rf /tmp/staging"),
        "auto-expand should render args_preview body: {joined}"
    );
}

#[test]
fn approved_or_finished_does_not_auto_expand() {
    let root = root_flow(vec![tool_with_approval(
        "a",
        "shell.exec",
        "rm -rf /tmp/staging",
        Some(ApprovalState::Approved),
    )]);
    let rendered = render_boxed(root, 80);
    let joined = rendered.join("\n");
    assert!(
        joined.contains("shell.exec"),
        "tool label missing: {joined}"
    );
    assert!(
        !joined.contains("args:"),
        "approved node must not auto-expand: {joined}"
    );
}

#[test]
fn expanded_box_renders_duration_when_start_and_end_are_set() {
    let mut node = tool_with_approval(
        "a",
        "shell.exec",
        "sleep 1",
        Some(ApprovalState::Pending {
            level: "high".into(),
            preview: None,
        }),
    );
    let start = chrono::Utc::now();
    node.started_at = Some(start);
    node.ended_at = Some(start + chrono::Duration::milliseconds(1234));
    let rendered = render_boxed(root_flow(vec![node]), 80);
    let joined = rendered.join("\n");
    assert!(
        joined.contains("duration:"),
        "duration section missing: {joined}"
    );
    assert!(
        joined.contains("1.234s"),
        "expected 1.234s in duration body: {joined}"
    );
}

#[test]
fn expanded_box_renders_millisecond_duration_below_one_second() {
    let mut node = tool_with_approval(
        "a",
        "shell.exec",
        "fast",
        Some(ApprovalState::Pending {
            level: "medium".into(),
            preview: None,
        }),
    );
    let start = chrono::Utc::now();
    node.started_at = Some(start);
    node.ended_at = Some(start + chrono::Duration::milliseconds(42));
    let rendered = render_boxed(root_flow(vec![node]), 80);
    let joined = rendered.join("\n");
    assert!(
        joined.contains("42ms"),
        "expected sub-second duration in ms: {joined}"
    );
}

#[test]
fn expanded_box_omits_duration_when_timestamps_missing() {
    let node = tool_with_approval(
        "a",
        "shell.exec",
        "still-pending",
        Some(ApprovalState::Pending {
            level: "high".into(),
            preview: None,
        }),
    );
    let rendered = render_boxed(root_flow(vec![node]), 80);
    let joined = rendered.join("\n");
    assert!(
        !joined.contains("duration:"),
        "duration should not appear without start+end: {joined}"
    );
}

#[test]
fn pending_approval_gets_sequential_hotkey_up_to_nine() {
    let mut children: Vec<WorkflowNode> = (0..3)
        .map(|i| {
            tool_with_approval(
                &format!("p{i}"),
                &format!("tool_{i}"),
                &format!("arg-{i}"),
                Some(ApprovalState::Pending {
                    level: "medium".into(),
                    preview: None,
                }),
            )
        })
        .collect();
    children.push(tool_with_approval("done", "already_ran", "arg-x", None));
    let root = root_flow(children);
    let rendered = render_boxed(root, 80);
    let joined = rendered.join("\n");
    for hotkey in 1..=3u8 {
        let marker = format!("─[{hotkey}]─");
        assert!(
            joined.contains(&marker),
            "expected hotkey {marker} for pending node, got:\n{joined}"
        );
    }
    assert!(
        !joined.contains("─[4]─"),
        "non-pending node must not consume a hotkey: {joined}"
    );
    assert!(
        !joined.contains("─[5]─"),
        "non-pending node must not consume a hotkey: {joined}"
    );
}
