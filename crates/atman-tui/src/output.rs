use atman_runtime::message::{Message, MessagePart, MessageRole};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{NoteLevel, OutputItem, ToolStatus};

const RESET: Style = Style::new();
const EXPANDED_CONTENT_MAX_CHARS: usize = 4096;

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
) -> (Vec<Line<'static>>, Vec<ItemRange>) {
    let mut all_lines: Vec<Line<'static>> = Vec::with_capacity(items.len() * 3);
    let mut ranges: Vec<ItemRange> = Vec::with_capacity(items.len());
    let mut cursor: u16 = 0;
    for (idx, item) in items.iter().enumerate() {
        let item_lines = render_item(item, ctx);
        let rows = if width == 0 {
            item_lines.len() as u16
        } else {
            let paragraph = Paragraph::new(item_lines.clone()).wrap(Wrap { trim: false });
            paragraph.line_count(width) as u16
        };
        ranges.push(ItemRange {
            item_index: idx,
            start_row: cursor,
            end_row: cursor.saturating_add(rows),
        });
        cursor = cursor.saturating_add(rows);
        all_lines.extend(item_lines);
    }
    (all_lines, ranges)
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
            let mut lines: Vec<Line<'static>> = if *streaming {
                md.lines().map(|l| Line::from(l.to_string())).collect()
            } else {
                crate::markdown::render_markdown(md)
            };
            if *streaming {
                lines.push(Line::from(Span::styled(
                    "▏".to_string(),
                    Style::default().add_modifier(Modifier::SLOW_BLINK),
                )));
            }
            lines
        }
        OutputItem::ToolCall {
            tool,
            args,
            status,
            result,
            tool_use_id,
        } => render_tool_call(
            tool,
            args,
            *status,
            result.as_deref(),
            tool_use_id.as_deref(),
            ctx,
        ),
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
        OutputItem::FlowPanel {
            flow_name,
            node_states,
            ended_at,
            expanded,
            graph,
            ..
        } => render_flow_panel(
            flow_name,
            graph,
            node_states,
            *ended_at,
            *expanded,
            ctx.animation_frame,
        ),
    };
    lines.push(Line::from(Span::styled(String::new(), RESET)));
    lines
}

fn render_tool_call(
    tool: &str,
    args: &str,
    status: ToolStatus,
    result_preview: Option<&str>,
    tool_use_id: Option<&str>,
    ctx: &RenderCtx<'_>,
) -> Vec<Line<'static>> {
    let expanded = tool_use_id
        .map(|id| ctx.expanded_tools.contains(id))
        .unwrap_or(false);
    let (mark, mark_style) = match status {
        ToolStatus::Running => ("⟶", Style::default().fg(Color::Yellow)),
        ToolStatus::Ok => ("✓", Style::default().fg(Color::Green)),
        ToolStatus::Err => ("✗", Style::default().fg(Color::Red)),
    };
    let fold_glyph = if expanded { "▼" } else { "▶" };
    let head = Line::from(vec![
        Span::styled(
            format!(" {fold_glyph} "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled(mark.to_string(), mark_style),
        Span::raw(" ".to_string()),
        Span::styled(
            tool.to_string(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw("(".to_string()),
        Span::styled(args.to_string(), Style::default().fg(Color::DarkGray)),
        Span::raw(")".to_string()),
        Span::styled(
            result_preview
                .filter(|_| !expanded)
                .map(|p| format!(" → {p}"))
                .unwrap_or_default(),
            Style::default().fg(Color::DarkGray),
        ),
    ]);
    if !expanded {
        return vec![head];
    }
    let mut lines = vec![head];
    let (full_args, full_result) = lookup_full(tool_use_id, ctx.messages);
    if let Some(args_json) = full_args.as_deref().or(Some(args)) {
        lines.push(Line::from(Span::styled(
            "     args:".to_string(),
            Style::default().fg(Color::DarkGray),
        )));
        for line in pretty_wrap(args_json).lines() {
            lines.push(Line::from(Span::styled(
                format!("       {line}"),
                Style::default().fg(Color::LightBlue),
            )));
        }
    }
    let result_body = full_result.as_deref().or(result_preview);
    if let Some(r) = result_body {
        lines.push(Line::from(Span::styled(
            "     result:".to_string(),
            Style::default().fg(Color::DarkGray),
        )));
        let (body, more) = cap_content(r, EXPANDED_CONTENT_MAX_CHARS);
        for line in body.lines() {
            lines.push(Line::from(Span::styled(
                format!("       {line}"),
                Style::default().fg(Color::DarkGray),
            )));
        }
        if more > 0 {
            lines.push(Line::from(Span::styled(
                format!("       …{more} chars more…"),
                Style::default().fg(Color::DarkGray),
            )));
        }
    } else if matches!(status, ToolStatus::Running) {
        lines.push(Line::from(Span::styled(
            "     (running…)".to_string(),
            Style::default().fg(Color::DarkGray),
        )));
    }
    lines
}

fn lookup_full(
    tool_use_id: Option<&str>,
    messages: &[Message],
) -> (Option<String>, Option<String>) {
    let Some(id) = tool_use_id else {
        return (None, None);
    };
    let mut args_json: Option<String> = None;
    let mut result_content: Option<String> = None;
    for msg in messages {
        match msg.role {
            MessageRole::Assistant => {
                for part in &msg.parts {
                    if let MessagePart::ToolUse { id: tid, input, .. } = part
                        && tid == id
                    {
                        args_json = Some(serde_json::to_string_pretty(input).unwrap_or_default());
                    }
                }
            }
            MessageRole::Tool => {
                for part in &msg.parts {
                    if let MessagePart::ToolResult {
                        tool_use_id: tid,
                        content,
                        ..
                    } = part
                        && tid == id
                    {
                        result_content = Some(content.clone());
                    }
                }
            }
            _ => {}
        }
    }
    (args_json, result_content)
}

fn cap_content(s: &str, max: usize) -> (String, usize) {
    if s.chars().count() <= max {
        return (s.to_string(), 0);
    }
    let mut kept = String::with_capacity(max);
    for (i, c) in s.chars().enumerate() {
        if i >= max {
            break;
        }
        kept.push(c);
    }
    let total = s.chars().count();
    (kept, total - max)
}

fn render_flow_panel(
    flow_name: &str,
    graph: &atman_runtime::nodegraph::FlowGraph,
    node_states: &std::collections::HashMap<String, atman_runtime::event::FlowNodeStatus>,
    ended_at: Option<std::time::Instant>,
    expanded: bool,
    animation_frame: u32,
) -> Vec<Line<'static>> {
    let running = ended_at.is_none();
    let fold_glyph = if expanded { "▼" } else { "▶" };
    let status_short = if running {
        "running…"
    } else if node_states
        .values()
        .any(|s| matches!(s, atman_runtime::event::FlowNodeStatus::Err))
    {
        "err"
    } else {
        "ok"
    };
    let total = count_nodes(&graph.root);
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
            format!("{flow_glyph} {flow_name}"),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" · {total} nodes · ")),
        Span::styled(
            status_short.to_string(),
            match status_short {
                "running…" => Style::default().fg(Color::Yellow),
                "ok" => Style::default().fg(Color::Green),
                "err" => Style::default().fg(Color::Red),
                _ => Style::default(),
            },
        ),
    ]);
    let mut lines = vec![header];
    if expanded {
        for node in &graph.root {
            append_flow_node_lines(&mut lines, node, node_states, 1, animation_frame, running);
        }
    }
    lines
}

fn count_nodes(nodes: &[atman_runtime::nodegraph::StaticNode]) -> usize {
    let mut n = 0;
    for node in nodes {
        n += 1;
        n += count_nodes(&node.children);
    }
    n
}

fn append_flow_node_lines(
    out: &mut Vec<Line<'static>>,
    node: &atman_runtime::nodegraph::StaticNode,
    node_states: &std::collections::HashMap<String, atman_runtime::event::FlowNodeStatus>,
    depth: usize,
    animation_frame: u32,
    flow_running: bool,
) {
    let indent = "  ".repeat(depth);
    let status = node_states.get(&node.node_id);
    let (glyph_str, style) = match status {
        Some(atman_runtime::event::FlowNodeStatus::Ok) => {
            ("✓".to_string(), Style::default().fg(Color::Green))
        }
        Some(atman_runtime::event::FlowNodeStatus::Err) => {
            ("✗".to_string(), Style::default().fg(Color::Red))
        }
        Some(atman_runtime::event::FlowNodeStatus::Cancelled) => {
            ("─".to_string(), Style::default().fg(Color::DarkGray))
        }
        None => {
            if flow_running {
                (
                    spinner_char(animation_frame).to_string(),
                    Style::default().fg(Color::Yellow),
                )
            } else {
                ("○".to_string(), Style::default().fg(Color::DarkGray))
            }
        }
    };
    let box_color = match status {
        Some(atman_runtime::event::FlowNodeStatus::Ok) => Color::Green,
        Some(atman_runtime::event::FlowNodeStatus::Err) => Color::Red,
        Some(atman_runtime::event::FlowNodeStatus::Cancelled) => Color::DarkGray,
        None if flow_running => Color::Yellow,
        None => Color::DarkGray,
    };
    let box_style = Style::default().fg(box_color);
    let label_width = node.label.chars().count().max(12);
    let inner_width = label_width + 4;

    out.push(Line::from(Span::styled(
        format!("{indent}┌{}┐", "─".repeat(inner_width)),
        box_style,
    )));
    out.push(Line::from(vec![
        Span::styled(format!("{indent}│ "), box_style),
        Span::styled(glyph_str, style),
        Span::raw(" "),
        Span::raw(format!("{:<w$}", node.label, w = label_width)),
        Span::styled(" │".to_string(), box_style),
    ]));
    out.push(Line::from(Span::styled(
        format!("{indent}└{}┘", "─".repeat(inner_width)),
        box_style,
    )));

    for child in &node.children {
        out.push(Line::from(Span::styled(
            format!("{indent}      ↓"),
            Style::default().fg(Color::DarkGray),
        )));
        append_flow_node_lines(
            out,
            child,
            node_states,
            depth + 1,
            animation_frame,
            flow_running,
        );
    }
}

fn pretty_wrap(s: &str) -> String {
    serde_json::from_str::<serde_json::Value>(s)
        .ok()
        .and_then(|v| serde_json::to_string_pretty(&v).ok())
        .unwrap_or_else(|| s.to_string())
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
            OutputItem::ToolCall {
                tool: "t".into(),
                args: "a".into(),
                status: ToolStatus::Ok,
                result: Some("r".into()),
                tool_use_id: None,
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
    fn tool_call_folded_is_single_line() {
        let item = OutputItem::ToolCall {
            tool: "fs.read".into(),
            args: "path".into(),
            status: ToolStatus::Ok,
            result: Some("42 bytes".into()),
            tool_use_id: Some("tc_1".into()),
        };
        let lines = render_item(&item, &RenderCtx::empty());
        assert_eq!(lines.len(), 2, "1 head + 1 trailing empty reset");
        let head_text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(head_text.contains("▶"));
        assert!(head_text.contains("→ 42 bytes"));
    }

    #[test]
    fn tool_call_expanded_shows_args_and_result_blocks() {
        let mut expanded = std::collections::HashSet::new();
        expanded.insert("tc_2".to_string());
        let ctx = RenderCtx {
            expanded_tools: &expanded,
            messages: &[],
            animation_frame: 0,
        };
        let item = OutputItem::ToolCall {
            tool: "fs.read".into(),
            args: "{\"path\": \"foo\"}".into(),
            status: ToolStatus::Ok,
            result: Some("hello world".into()),
            tool_use_id: Some("tc_2".into()),
        };
        let lines = render_item(&item, &ctx);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(flat.contains("▼"), "expanded indicator missing: {flat}");
        assert!(flat.contains("args:"));
        assert!(flat.contains("result:"));
        assert!(flat.contains("hello world"));
    }

    #[test]
    fn build_lines_with_ranges_gives_one_range_per_item() {
        let items = vec![
            OutputItem::UserTurn { text: "hi".into() },
            OutputItem::Divider,
        ];
        let (_lines, ranges) = build_lines_with_ranges(&items, 80, &RenderCtx::empty());
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0].item_index, 0);
        assert_eq!(ranges[1].item_index, 1);
        assert!(ranges[0].end_row <= ranges[1].start_row);
    }

    #[test]
    fn build_lines_with_ranges_empty_items_returns_empty_vecs() {
        let (lines, ranges) = build_lines_with_ranges(&[], 80, &RenderCtx::empty());
        assert!(lines.is_empty());
        assert!(ranges.is_empty());
    }

    #[test]
    fn tool_call_expanded_caps_long_result() {
        let mut expanded = std::collections::HashSet::new();
        expanded.insert("tc_3".to_string());
        let ctx = RenderCtx {
            expanded_tools: &expanded,
            messages: &[],
            animation_frame: 0,
        };
        let long = "x".repeat(5000);
        let item = OutputItem::ToolCall {
            tool: "t".into(),
            args: "".into(),
            status: ToolStatus::Ok,
            result: Some(long),
            tool_use_id: Some("tc_3".into()),
        };
        let lines = render_item(&item, &ctx);
        let flat: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(flat.contains("chars more"), "want cap: {flat}");
    }
}
