use atman_runtime::event::TurnId;
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

use std::sync::Mutex;
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn snapshot(width: u16, height: u16, boxed: bool) -> Buffer {
    let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = root_flow(vec![tool_node("a", "read"), tool_node("b", "write")]);
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
        hovered_thinking_idx: None,
    };
    // SAFETY: env-var mutation guarded by ENV_LOCK.
    if !boxed {
        unsafe { std::env::set_var("ATMAN_LEGACY_WORKFLOW", "1") };
    }
    let (lines, _, _, _) = build_lines_with_ranges(&[item], width, &ctx);
    if !boxed {
        unsafe { std::env::remove_var("ATMAN_LEGACY_WORKFLOW") };
    }

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
    let buf = snapshot(60, 10, false);
    let rendered = buffer_to_lines(&buf);
    let expected = [
        " ▼ ⚡  workflow · 3 nodes · 0s · ok",
        "└─ ✓   ⚡  flow",
        "   ├─ ✓ ▸ 🔧  read()",
        "   └─ ✓ ▸ 🔧  write()",
    ];
    assert_eq!(
        rendered.len(),
        expected.len(),
        "row count drift: got {rendered:#?}"
    );
    for (i, (got, want)) in rendered.iter().zip(expected.iter()).enumerate() {
        assert_eq!(
            got, want,
            "line {i} drift\n got={got:?}\nwant={want:?}\nfull={rendered:#?}"
        );
    }
}

#[test]
fn boxed_two_tool_children_render_snapshot() {
    let buf = snapshot(60, 20, true);
    let rendered = buffer_to_lines(&buf);
    let joined = rendered.join("\n");
    assert!(
        joined.contains("╭"),
        "expected rounded box top-left, got:\n{joined}"
    );
    assert!(
        joined.contains("╯"),
        "expected rounded box bottom-right, got:\n{joined}"
    );
    assert!(
        joined.contains("read"),
        "first child label missing, got:\n{joined}"
    );
    assert!(
        joined.contains("write"),
        "second child label missing, got:\n{joined}"
    );
    let box_rows = rendered.iter().filter(|l| l.contains("╭")).count();
    assert!(
        box_rows >= 3,
        "expected at least 3 box tops (flow + 2 tools), got {box_rows}:\n{joined}"
    );
}
