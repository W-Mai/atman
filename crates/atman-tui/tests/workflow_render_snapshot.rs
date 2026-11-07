type TurnId = atman_rt::TurnId<atman_runtime::event::AtmanUuid>;
use atman_runtime::workflow::{
    NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
};
use atman_tui::app::OutputItem;
use atman_tui::output::{RenderCtx, build_lines_with_ranges};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;
use ratatui::widgets::Paragraph;
use std::collections::HashSet;

fn tool_node(id: &str, label: &str) -> WorkflowNode {
    WorkflowNode {
        id: id.into(),
        kind: WorkflowNodeKind::ToolCall {
            tool_use_id: id.into(),
            tool: label.into(),
            args_preview: String::new(),
            call_intent: None,
            result_preview: None,
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

fn root_flow(children: Vec<WorkflowNode>) -> WorkflowNode {
    WorkflowNode {
        id: "root".into(),
        kind: WorkflowNodeKind::Flow {
            run_id: "r".into(),
            flow_name: "flow".into(),
        },
        label: "flow".into(),
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

fn snapshot(width: u16, height: u16) -> Buffer {
    let root = root_flow(vec![tool_node("a", "read"), tool_node("b", "write")]);
    let mut graph = WorkflowGraph::new(TurnId::now());
    graph.root.push(root);
    let item = OutputItem::WorkflowPanel {
        turn_index: 0,
        graph: graph.into(),
        presentation: Default::default(),
        expanded_nodes: HashSet::new(),
        panel_expanded: true,
        started_at: std::time::Instant::now(),
        ended_at: None,
        cancelled: false,
    };
    let ctx = RenderCtx {
        expanded_tools: &HashSet::new(),
        animation_frame: 0,
        panel_width: width,
        hovered_thinking_idx: None,
        hovered_output_node: None,
    };
    let (lines, _, _, _) = build_lines_with_ranges(&[item], width, &ctx);

    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("terminal");
    terminal
        .draw(|f| {
            let para = Paragraph::new(lines).scroll((0, 0));
            f.render_widget(para, f.area());
        })
        .expect("draw");
    terminal.backend().buffer().clone()
}

fn buffer_to_lines(buf: &Buffer) -> Vec<String> {
    let mut out = Vec::with_capacity(buf.area.height as usize);
    for y in 0..buf.area.height {
        let mut row = String::new();
        for x in 0..buf.area.width {
            row.push_str(buf[(x, y)].symbol());
        }
        out.push(row.trim_end().to_string());
    }
    while out.last().is_some_and(|s| s.is_empty()) {
        out.pop();
    }
    out
}

#[test]
fn line_based_two_tool_children_render_snapshot() {
    let buf = snapshot(60, 10);
    let rendered = buffer_to_lines(&buf);
    let joined = rendered.join("\n");
    assert!(joined.contains("workflow"));
    assert!(joined.contains("read") && joined.contains("write"));
    assert!(joined.contains('≡') && joined.contains('⇄') && joined.contains('↗'));
    assert!(!joined.contains(['╭', '╮', '╰', '╯']));
}

#[test]
fn expanded_workflow_preserves_child_order_without_decorative_borders() {
    let buf = snapshot(60, 20);
    let rendered = buffer_to_lines(&buf);
    let joined = rendered.join("\n");
    assert!(
        !joined.contains(['╭', '╮', '╰', '╯']),
        "unexpected decorative border:\n{joined}"
    );
    let positions = ["flow", "read", "write"].map(|label| {
        joined
            .find(label)
            .unwrap_or_else(|| panic!("missing {label}:\n{joined}"))
    });
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "workflow child order changed:\n{joined}"
    );
}
