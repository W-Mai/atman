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

pub fn build_lines_with_ranges(
    items: &[OutputItem],
    width: u16,
    ctx: &RenderCtx<'_>,
) -> (Vec<Line<'static>>, Vec<ItemRange>, u16) {
    let mut all_lines: Vec<Line<'static>> = Vec::with_capacity(items.len() * 3);
    let mut ranges: Vec<ItemRange> = Vec::with_capacity(items.len());
    let mut cursor: u16 = 0;
    for (idx, item) in items.iter().enumerate() {
        let item_lines = render_item(item, ctx);
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
        cursor = cursor.saturating_add(rows);
        all_lines.extend(item_lines);
    }
    (all_lines, ranges, cursor)
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
    total_rows: u16,
}

impl LayoutCache {
    pub fn get_or_build(
        &mut self,
        key: LayoutKey,
        items: &[OutputItem],
        ctx: &RenderCtx<'_>,
    ) -> (&[Line<'static>], &[ItemRange], u16) {
        if self.key != Some(key) {
            let (lines, ranges, total) = build_lines_with_ranges(items, key.width, ctx);
            self.lines = lines;
            self.ranges = ranges;
            self.total_rows = total;
            self.key = Some(key);
        }
        (&self.lines, &self.ranges, self.total_rows)
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
        OutputItem::AssistantMd { md } => crate::markdown::render_markdown(md),
        OutputItem::SystemNote { text, level } => {
            let color = match level {
                NoteLevel::Info => Color::Blue,
                NoteLevel::Warn => Color::Yellow,
                NoteLevel::Error => Color::Red,
            };
            vec![Line::from(vec![
                Span::styled("[atman] ".to_string(), Style::default().fg(color)),
                Span::raw(text.clone()),
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
    started_at: std::time::Instant,
    ended_at: Option<std::time::Instant>,
    animation_frame: u32,
) -> Vec<Line<'static>> {
    let running = ended_at.is_none();
    let elapsed = ended_at
        .unwrap_or_else(std::time::Instant::now)
        .saturating_duration_since(started_at)
        .as_secs();
    let count = count_workflow_nodes(&graph.root);
    let (status_str, status_style) = workflow_overall_status(&graph.root);
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
    if panel_expanded {
        let child_count = graph.root.len();
        for (i, node) in graph.root.iter().enumerate() {
            let is_last = i + 1 == child_count;
            append_workflow_node(
                &mut lines,
                node,
                expanded_nodes,
                "",
                is_last,
                animation_frame,
                running,
            );
        }
    }
    lines
}

fn count_workflow_nodes(nodes: &[atman_runtime::workflow::WorkflowNode]) -> usize {
    nodes
        .iter()
        .map(|n| 1 + count_workflow_nodes(&n.children))
        .sum()
}

fn workflow_overall_status(nodes: &[atman_runtime::workflow::WorkflowNode]) -> (String, Style) {
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
    if has_err {
        ("err".into(), Style::default().fg(Color::Red))
    } else if has_running {
        ("running…".into(), Style::default().fg(Color::Yellow))
    } else if nodes.is_empty() {
        ("empty".into(), Style::default().fg(Color::DarkGray))
    } else {
        ("ok".into(), Style::default().fg(Color::Green))
    }
}

fn append_workflow_node(
    out: &mut Vec<Line<'static>>,
    node: &atman_runtime::workflow::WorkflowNode,
    expanded_nodes: &std::collections::HashSet<String>,
    ancestor_prefix: &str,
    is_last: bool,
    animation_frame: u32,
    flow_running: bool,
) {
    use atman_runtime::workflow::{NodeStatus, WorkflowNodeKind};
    let branch_glyph = if is_last { "└─" } else { "├─" };
    let (status_glyph, status_style) = match node.status {
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
    let (kind_glyph, kind_color) = match &node.kind {
        WorkflowNodeKind::Flow { .. } => ("⚡", Color::Cyan),
        WorkflowNodeKind::Subflow { .. } => ("↳", Color::Cyan),
        WorkflowNodeKind::Stmt => ("•", Color::White),
        WorkflowNodeKind::ToolCall { .. } => ("🔧", Color::Blue),
        WorkflowNodeKind::FanoutBranch { .. } => ("⇉", Color::Magenta),
    };
    let label = match &node.kind {
        WorkflowNodeKind::ToolCall {
            tool, args_preview, ..
        } => format!("{tool}({})", truncate_preview(args_preview, 40)),
        WorkflowNodeKind::FanoutBranch { branch_index } => {
            format!("branch[{branch_index}]  {}", node.label)
        }
        _ => node.label.clone(),
    };
    out.push(Line::from(vec![
        Span::styled(
            format!("{ancestor_prefix}{branch_glyph} "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(format!("{status_glyph} "), status_style),
        Span::styled(format!("{kind_glyph} "), Style::default().fg(kind_color)),
        Span::raw(label),
    ]));
    let vertical = if is_last { "   " } else { "│  " };
    let child_prefix = format!("{ancestor_prefix}{vertical}");
    if expanded_nodes.contains(&node.id)
        && let Some(preview) = node.output_preview.as_deref()
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
    let child_count = node.children.len();
    for (i, child) in node.children.iter().enumerate() {
        let child_last = i + 1 == child_count;
        append_workflow_node(
            out,
            child,
            expanded_nodes,
            &child_prefix,
            child_last,
            animation_frame,
            flow_running,
        );
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
        let (_lines, ranges, total) = build_lines_with_ranges(&items, 80, &RenderCtx::empty());
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].item_index, 0);
        assert_eq!(ranges[1].item_index, 1);
        assert!(ranges[0].end_row <= ranges[1].start_row);
        assert_eq!(total, ranges[1].end_row);
    }

    #[test]
    fn build_lines_with_ranges_empty_items_returns_empty_vecs() {
        let (lines, ranges, total) = build_lines_with_ranges(&[], 80, &RenderCtx::empty());
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
                    kind: WorkflowNodeKind::Stmt,
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
                    kind: WorkflowNodeKind::Stmt,
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
                kind: WorkflowNodeKind::Stmt,
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
}
