use atman_runtime::message::Message;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{NoteLevel, OutputItem};

const RESET: Style = Style::new();

pub struct RenderCtx<'a> {
    pub expanded_tools: &'a std::collections::HashSet<String>,
    pub messages: &'a [Message],
    pub animation_frame: u32,
}

impl<'a> RenderCtx<'a> {
    pub fn empty() -> RenderCtx<'a> {
        static EMPTY_SET: std::sync::OnceLock<std::collections::HashSet<String>> =
            std::sync::OnceLock::new();
        RenderCtx {
            expanded_tools: EMPTY_SET.get_or_init(std::collections::HashSet::new),
            messages: &[],
            animation_frame: 0,
        }
    }
}

const SPINNER: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spinner_char(frame: u32) -> &'static str {
    SPINNER[(frame as usize) % SPINNER.len()]
}

pub fn build_lines(items: &[OutputItem], ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(items.len() * 3);
    for item in items {
        out.extend(render_item(item, ctx));
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ItemRange {
    pub item_index: usize,
    pub start_row: u16,
    pub end_row: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRegion {
    pub panel_item_index: usize,
    pub node_id: String,
    pub start_row: u16,
    pub end_row: u16,
}

pub fn build_lines_with_ranges(
    items: &[OutputItem],
    width: u16,
    ctx: &RenderCtx<'_>,
) -> (Vec<Line<'static>>, Vec<ItemRange>, Vec<NodeRegion>, u16) {
    let mut all_lines: Vec<Line<'static>> = Vec::with_capacity(items.len() * 3);
    let mut ranges: Vec<ItemRange> = Vec::with_capacity(items.len());
    let mut node_regions: Vec<NodeRegion> = Vec::new();
    let mut cursor: u16 = 0;
    for (idx, item) in items.iter().enumerate() {
        let (item_lines, mut item_regions) = render_item_with_regions(item, ctx);
        let rows = if width == 0 {
            item_lines.len() as u16
        } else {
            let p = Paragraph::new(item_lines.clone()).wrap(Wrap { trim: false });
            p.line_count(width) as u16
        };
        ranges.push(ItemRange {
            item_index: idx,
            start_row: cursor,
            end_row: cursor.saturating_add(rows),
        });
        for r in item_regions.iter_mut() {
            r.panel_item_index = idx;
            r.start_row = r.start_row.saturating_add(cursor);
            r.end_row = r.end_row.saturating_add(cursor);
        }
        node_regions.extend(item_regions);
        cursor = cursor.saturating_add(rows);
        all_lines.extend(item_lines);
    }
    (all_lines, ranges, node_regions, cursor)
}

pub fn render_item_with_regions(
    item: &OutputItem,
    ctx: &RenderCtx<'_>,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    if let OutputItem::WorkflowPanel {
        graph,
        expanded_nodes,
        panel_expanded,
        ..
    } = item
    {
        render_workflow_panel_with_regions(
            graph,
            expanded_nodes,
            *panel_expanded,
            ctx.animation_frame,
        )
    } else {
        (render_item(item, ctx), Vec::new())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutKey {
    pub items_version: u64,
    pub expanded_version: u64,
    pub width: u16,
    pub animation_frame: Option<u32>,
}

#[derive(Default)]
pub struct LayoutCache {
    key: Option<LayoutKey>,
    lines: Vec<Line<'static>>,
    ranges: Vec<ItemRange>,
    node_regions: Vec<NodeRegion>,
    total_rows: u16,
}

impl LayoutCache {
    pub fn get_or_build(
        &mut self,
        key: LayoutKey,
        items: &[OutputItem],
        ctx: &RenderCtx<'_>,
    ) -> (&[Line<'static>], &[ItemRange], &[NodeRegion], u16) {
        if self.key != Some(key) {
            let (lines, ranges, node_regions, total) =
                build_lines_with_ranges(items, key.width, ctx);
            self.lines = lines;
            self.ranges = ranges;
            self.node_regions = node_regions;
            self.total_rows = total;
            self.key = Some(key);
        }
        (
            &self.lines,
            &self.ranges,
            &self.node_regions,
            self.total_rows,
        )
    }

    pub fn invalidate(&mut self) {
        self.key = None;
    }
}

impl std::fmt::Debug for LayoutCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayoutCache")
            .field("key", &self.key)
            .field("total_rows", &self.total_rows)
            .finish()
    }
}

pub fn render_item(item: &OutputItem, ctx: &RenderCtx<'_>) -> Vec<Line<'static>> {
    let mut lines = match item {
        OutputItem::UserTurn { text } => vec![Line::from(vec![
            Span::styled(
                "❯ ".to_string(),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(text.clone()),
        ])],
        OutputItem::AssistantMd { md, streaming } => {
            let mut lines = crate::markdown::render_markdown(md);
            if *streaming {
                lines.push(Line::from(Span::styled(
                    "▏".to_string(),
                    Style::default().add_modifier(Modifier::SLOW_BLINK),
                )));
            }
            lines
        }
        OutputItem::SystemNote { text, level } => {
            let (glyph, color) = match level {
                NoteLevel::Info => ("·", Color::Blue),
                NoteLevel::Warn => ("!", Color::Yellow),
                NoteLevel::Error => ("✗", Color::Red),
            };
            let cleaned = text
                .strip_prefix("[atman] ")
                .or_else(|| text.strip_prefix("[atman]"))
                .unwrap_or(text)
                .to_string();
            vec![Line::from(vec![
                Span::styled(format!(" {glyph} "), Style::default().fg(color)),
                Span::raw(cleaned),
            ])]
        }
        OutputItem::Divider => vec![
            Line::from(""),
            Line::from(Span::styled(
                "─".repeat(60),
                Style::default().fg(Color::DarkGray),
            )),
        ],
        OutputItem::WorkflowPanel {
            graph,
            expanded_nodes,
            panel_expanded,
            started_at,
            ended_at,
            ..
        } => render_workflow_panel(
            graph,
            expanded_nodes,
            *panel_expanded,
            *started_at,
            *ended_at,
            ctx.animation_frame,
        ),
    };
    lines.push(Line::from(Span::styled(String::new(), RESET)));
    lines
}

fn render_workflow_panel(
    graph: &atman_runtime::workflow::WorkflowGraph,
    expanded_nodes: &std::collections::HashSet<String>,
    panel_expanded: bool,
    _started_at: std::time::Instant,
    _ended_at: Option<std::time::Instant>,
    animation_frame: u32,
) -> Vec<Line<'static>> {
    render_workflow_panel_with_regions(graph, expanded_nodes, panel_expanded, animation_frame).0
}

fn render_workflow_panel_with_regions(
    graph: &atman_runtime::workflow::WorkflowGraph,
    expanded_nodes: &std::collections::HashSet<String>,
    panel_expanded: bool,
    animation_frame: u32,
) -> (Vec<Line<'static>>, Vec<NodeRegion>) {
    let count = count_workflow_nodes(&graph.root);
    let (status_str, status_style, running) = workflow_overall_status(&graph.root);
    let elapsed = compute_elapsed_secs(&graph.root, running);
    let fold_glyph = if panel_expanded { "▼" } else { "▶" };
    let flow_glyph = if running {
        spinner_char(animation_frame)
    } else {
        "⚡"
    };
    let header = Line::from(vec![
        Span::styled(
            format!(" {fold_glyph} "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(
            format!("{flow_glyph} workflow"),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" · {count} nodes · {elapsed}s · ")),
        Span::styled(status_str, status_style),
    ]);
    let mut lines = vec![header];
    let mut regions: Vec<NodeRegion> = Vec::new();
    if panel_expanded {
        let child_count = graph.root.len();
        for (i, node) in graph.root.iter().enumerate() {
            let is_last = i + 1 == child_count;
            append_workflow_node(
                &mut lines,
                &mut regions,
                node,
                expanded_nodes,
                "",
                is_last,
                animation_frame,
                running,
            );
        }
    }
    (lines, regions)
}

fn compute_elapsed_secs(nodes: &[atman_runtime::workflow::WorkflowNode], running: bool) -> i64 {
    let mut min: Option<chrono::DateTime<chrono::Utc>> = None;
    let mut max: Option<chrono::DateTime<chrono::Utc>> = None;
    for n in nodes {
        if let Some(t) = n.started_at {
            min = Some(min.map(|m| m.min(t)).unwrap_or(t));
        }
        if let Some(t) = n.ended_at {
            max = Some(max.map(|m| m.max(t)).unwrap_or(t));
        }
    }
    let Some(start) = min else {
        return 0;
    };
    let end = if running {
        chrono::Utc::now()
    } else {
        max.unwrap_or(start)
    };
    (end - start).num_seconds().max(0)
}

fn count_workflow_nodes(nodes: &[atman_runtime::workflow::WorkflowNode]) -> usize {
    nodes
        .iter()
        .map(|n| 1 + count_workflow_nodes(&n.children))
        .sum()
}

fn workflow_overall_status(
    nodes: &[atman_runtime::workflow::WorkflowNode],
) -> (String, Style, bool) {
    use atman_runtime::workflow::NodeStatus;
    fn walk(ns: &[atman_runtime::workflow::WorkflowNode], running: &mut bool, err: &mut bool) {
        for n in ns {
            match n.status {
                NodeStatus::Running | NodeStatus::Pending => *running = true,
                NodeStatus::Err => *err = true,
                _ => {}
            }
            walk(&n.children, running, err);
        }
    }
    let mut has_running = false;
    let mut has_err = false;
    walk(nodes, &mut has_running, &mut has_err);
    if has_running {
        ("running…".into(), Style::default().fg(Color::Yellow), true)
    } else if has_err {
        ("err".into(), Style::default().fg(Color::Red), false)
    } else if nodes.is_empty() {
        ("empty".into(), Style::default().fg(Color::DarkGray), false)
    } else {
        ("ok".into(), Style::default().fg(Color::Green), false)
    }
}

#[allow(clippy::too_many_arguments)]
fn append_workflow_node(
    out: &mut Vec<Line<'static>>,
    regions: &mut Vec<NodeRegion>,
    node: &atman_runtime::workflow::WorkflowNode,
    expanded_nodes: &std::collections::HashSet<String>,
    ancestor_prefix: &str,
    is_last: bool,
    animation_frame: u32,
    flow_running: bool,
) {
    use atman_runtime::workflow::{NodeStatus, WorkflowNodeKind};
    let start_row = out.len() as u16;
    let effective = node;
    let (branch_glyph, branch_color) = if matches!(node.kind, WorkflowNodeKind::FanoutBranch { .. })
    {
        if is_last {
            ("╚═", Color::Magenta)
        } else {
            ("╠═", Color::Magenta)
        }
    } else if is_last {
        ("└─", Color::DarkGray)
    } else {
        ("├─", Color::DarkGray)
    };
    let (status_glyph, status_style) = match effective.status {
        NodeStatus::Ok => ("✓", Style::default().fg(Color::Green)),
        NodeStatus::Err => ("✗", Style::default().fg(Color::Red)),
        NodeStatus::Cancelled => ("⊘", Style::default().fg(Color::DarkGray)),
        NodeStatus::Running | NodeStatus::Pending => {
            if flow_running {
                (
                    spinner_char(animation_frame),
                    Style::default().fg(Color::Yellow),
                )
            } else {
                ("○", Style::default().fg(Color::DarkGray))
            }
        }
    };
    let collapsed_tool = collapsed_tool_view(effective);
    let (kind_glyph, kind_color) = match &effective.kind {
        WorkflowNodeKind::Flow { .. } => ("⚡", Color::Cyan),
        WorkflowNodeKind::Subflow { .. } => ("↳", Color::Cyan),
        WorkflowNodeKind::Stmt { node_kind } => {
            if collapsed_tool.is_some() {
                ("🔧", Color::Blue)
            } else {
                stmt_kind_glyph(node_kind)
            }
        }
        WorkflowNodeKind::ToolCall { .. } => ("🔧", Color::Blue),
        WorkflowNodeKind::FanoutBranch { .. } => ("⇉", Color::Magenta),
    };
    let base_label = if let Some((tool, args)) = &collapsed_tool {
        format!("{tool}({})", truncate_preview(args, 60))
    } else {
        match &effective.kind {
            WorkflowNodeKind::ToolCall {
                tool, args_preview, ..
            } => format!("{tool}({})", truncate_preview(args_preview, 60)),
            WorkflowNodeKind::FanoutBranch { branch_index } => {
                format!("branch[{branch_index}]  {}", effective.label)
            }
            _ => effective.label.clone(),
        }
    };
    let label = base_label;
    out.push(Line::from(vec![
        Span::styled(
            format!("{ancestor_prefix}{branch_glyph} "),
            Style::default().fg(branch_color),
        ),
        Span::styled(format!("{status_glyph} "), status_style),
        Span::styled(format!("{kind_glyph} "), Style::default().fg(kind_color)),
        Span::raw(label),
    ]));
    regions.push(NodeRegion {
        panel_item_index: 0,
        node_id: node.id.clone(),
        start_row,
        end_row: start_row.saturating_add(1),
    });
    let vertical = if is_last { "   " } else { "│  " };
    let child_prefix = format!("{ancestor_prefix}{vertical}");
    if expanded_nodes.contains(&effective.id)
        && let Some(preview) = effective.output_preview.as_deref()
    {
        for line in preview.lines().take(6) {
            out.push(Line::from(vec![
                Span::styled(
                    format!("{child_prefix}  "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(line.to_string(), Style::default().fg(Color::DarkGray)),
            ]));
        }
    }
    if collapsed_tool.is_some() {
        return;
    }
    let child_count = effective.children.len();
    for (i, child) in effective.children.iter().enumerate() {
        let child_last = i + 1 == child_count;
        append_workflow_node(
            out,
            regions,
            child,
            expanded_nodes,
            &child_prefix,
            child_last,
            animation_frame,
            flow_running,
        );
    }
}

fn collapsed_tool_view(node: &atman_runtime::workflow::WorkflowNode) -> Option<(String, String)> {
    use atman_runtime::nodegraph::NodeKind;
    use atman_runtime::workflow::WorkflowNodeKind;
    let is_tool_stmt = matches!(
        &node.kind,
        WorkflowNodeKind::Stmt {
            node_kind: NodeKind::ToolCall { .. }
        }
    );
    if !is_tool_stmt || node.children.len() != 1 {
        return None;
    }
    match &node.children[0].kind {
        WorkflowNodeKind::ToolCall {
            tool, args_preview, ..
        } => Some((tool.clone(), args_preview.clone())),
        _ => None,
    }
}

fn stmt_kind_glyph(kind: &atman_runtime::nodegraph::NodeKind) -> (&'static str, Color) {
    use atman_runtime::nodegraph::NodeKind;
    match kind {
        NodeKind::Llm { .. } => ("✦", Color::Magenta),
        NodeKind::ToolCall { .. } => ("🔧", Color::Blue),
        NodeKind::Fanout { .. } => ("⇉", Color::Magenta),
        NodeKind::UserConfirm => ("?", Color::LightCyan),
        NodeKind::Subflow { .. } => ("↳", Color::Cyan),
        NodeKind::Message { .. } => ("✉", Color::White),
        NodeKind::FixUntilTest => ("↻", Color::LightMagenta),
        NodeKind::When { .. } => ("⋯", Color::DarkGray),
        NodeKind::Return => ("←", Color::Green),
    }
}

fn truncate_preview(s: &str, max: usize) -> String {
    let mut acc = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= max {
            acc.push('…');
            return acc;
        }
        acc.push(ch);
    }
    acc
}

pub fn empty_hint<'a>() -> Paragraph<'a> {
    Paragraph::new("plain text → agent · :help for builtins · Ctrl+C to interrupt")
        .style(Style::default().fg(Color::DarkGray))
        .wrap(Wrap { trim: true })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_variant_ends_with_reset_empty_line() {
        for item in [
            OutputItem::UserTurn { text: "hi".into() },
            OutputItem::AssistantMd {
                md: "one line".into(),
                streaming: false,
            },
            OutputItem::SystemNote {
                text: "note".into(),
                level: NoteLevel::Info,
            },
            OutputItem::Divider,
        ] {
            let lines = render_item(&item, &RenderCtx::empty());
            let last = lines.last().expect("non-empty");
            let text: String = last.spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(
                text.is_empty(),
                "expected empty trailing line, got {text:?}"
            );
        }
    }

    #[test]
    fn divider_produces_three_lines() {
        let lines = render_item(&OutputItem::Divider, &RenderCtx::empty());
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn build_lines_concats_all_items() {
        let items = vec![
            OutputItem::UserTurn { text: "hi".into() },
            OutputItem::Divider,
        ];
        let out = build_lines(&items, &RenderCtx::empty());
        assert!(out.len() >= 4);
    }

    #[test]
    fn build_lines_with_ranges_gives_one_range_per_item() {
        let items = vec![
            OutputItem::UserTurn { text: "hi".into() },
            OutputItem::Divider,
        ];
        let (_lines, ranges, _regions, total) =
            build_lines_with_ranges(&items, 80, &RenderCtx::empty());
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].item_index, 0);
        assert_eq!(ranges[1].item_index, 1);
        assert!(ranges[0].end_row <= ranges[1].start_row);
        assert_eq!(total, ranges[1].end_row);
    }

    #[test]
    fn build_lines_with_ranges_empty_items_returns_empty_vecs() {
        let (lines, ranges, _regions, total) =
            build_lines_with_ranges(&[], 80, &RenderCtx::empty());
        assert!(lines.is_empty());
        assert!(ranges.is_empty());
        assert_eq!(total, 0);
    }

    fn flatten_lines(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn workflow_panel_renders_linear_chain_with_tree_glyphs() {
        use atman_runtime::workflow::{
            NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
        };
        let mut graph = WorkflowGraph::new(atman_runtime::event::TurnId::now());
        graph.root.push(WorkflowNode {
            id: "r".into(),
            kind: WorkflowNodeKind::Flow {
                run_id: "r".into(),
                flow_name: "f".into(),
            },
            label: "flow".into(),
            status: NodeStatus::Ok,
            started_at: None,
            ended_at: None,
            output_preview: None,
            children: vec![
                WorkflowNode {
                    id: "s0".into(),
                    kind: WorkflowNodeKind::Stmt {
                        node_kind: atman_runtime::nodegraph::NodeKind::UserConfirm,
                    },
                    label: "step0".into(),
                    status: NodeStatus::Ok,
                    started_at: None,
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
                },
                WorkflowNode {
                    id: "s1".into(),
                    kind: WorkflowNodeKind::Stmt {
                        node_kind: atman_runtime::nodegraph::NodeKind::UserConfirm,
                    },
                    label: "step1".into(),
                    status: NodeStatus::Ok,
                    started_at: None,
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
                },
            ],
            parallelism: Parallelism::Serial,
        });
        let panel = OutputItem::WorkflowPanel {
            turn_index: 0,
            graph,
            expanded_nodes: std::collections::HashSet::new(),
            panel_expanded: true,
            started_at: std::time::Instant::now(),
            ended_at: Some(std::time::Instant::now()),
        };
        let lines = render_item(&panel, &RenderCtx::empty());
        let flat = flatten_lines(&lines);
        assert!(flat.contains("workflow"), "header missing: {flat}");
        assert!(flat.contains("step0"));
        assert!(flat.contains("step1"));
        assert!(flat.contains("├─"));
        assert!(flat.contains("└─"));
    }

    #[test]
    fn workflow_panel_collapsed_hides_children() {
        use atman_runtime::workflow::{
            NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
        };
        let mut graph = WorkflowGraph::new(atman_runtime::event::TurnId::now());
        graph.root.push(WorkflowNode {
            id: "r".into(),
            kind: WorkflowNodeKind::Flow {
                run_id: "r".into(),
                flow_name: "f".into(),
            },
            label: "flow".into(),
            status: NodeStatus::Ok,
            started_at: None,
            ended_at: None,
            output_preview: None,
            children: vec![WorkflowNode {
                id: "child".into(),
                kind: WorkflowNodeKind::Stmt {
                    node_kind: atman_runtime::nodegraph::NodeKind::UserConfirm,
                },
                label: "hidden-child".into(),
                status: NodeStatus::Ok,
                started_at: None,
                ended_at: None,
                output_preview: None,
                children: Vec::new(),
                parallelism: Parallelism::Serial,
            }],
            parallelism: Parallelism::Serial,
        });
        let panel = OutputItem::WorkflowPanel {
            turn_index: 0,
            graph,
            expanded_nodes: std::collections::HashSet::new(),
            panel_expanded: false,
            started_at: std::time::Instant::now(),
            ended_at: Some(std::time::Instant::now()),
        };
        let lines = render_item(&panel, &RenderCtx::empty());
        let flat = flatten_lines(&lines);
        assert!(flat.contains("workflow"));
        assert!(!flat.contains("hidden-child"), "collapsed leaks: {flat}");
    }

    #[test]
    fn recursive_subflow_chain_preserves_every_iteration() {
        use atman_runtime::workflow::{
            NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
        };

        fn subflow_layer(depth: usize, remaining: usize) -> WorkflowNode {
            let deeper = if remaining > 0 {
                vec![subflow_layer(depth + 1, remaining - 1)]
            } else {
                vec![WorkflowNode {
                    id: format!("leaf_{depth}"),
                    kind: WorkflowNodeKind::Stmt {
                        node_kind: atman_runtime::nodegraph::NodeKind::Return,
                    },
                    label: "final".into(),
                    status: NodeStatus::Ok,
                    started_at: None,
                    ended_at: None,
                    output_preview: None,
                    children: Vec::new(),
                    parallelism: Parallelism::Serial,
                }]
            };
            WorkflowNode {
                id: format!("loop_{depth}"),
                kind: WorkflowNodeKind::Subflow {
                    run_id: format!("r_{depth}"),
                    flow_name: "agent_loop".into(),
                },
                label: "agent_loop".into(),
                status: NodeStatus::Ok,
                started_at: None,
                ended_at: None,
                output_preview: None,
                children: deeper,
                parallelism: Parallelism::Serial,
            }
        }

        let mut graph = WorkflowGraph::new(atman_runtime::event::TurnId::now());
        graph.root.push(subflow_layer(0, 4));
        let panel = OutputItem::WorkflowPanel {
            turn_index: 0,
            graph,
            expanded_nodes: std::collections::HashSet::new(),
            panel_expanded: true,
            started_at: std::time::Instant::now(),
            ended_at: Some(std::time::Instant::now()),
        };
        let lines = render_item(&panel, &RenderCtx::empty());
        let flat = flatten_lines(&lines);
        assert!(
            flat.matches("agent_loop").count() >= 5,
            "each iteration must render, got: {flat}"
        );
        assert!(flat.contains("final"));
    }
}
