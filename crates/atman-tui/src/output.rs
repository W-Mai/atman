use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};

use crate::app::{NoteLevel, OutputItem, ToolStatus};

pub fn build_list<'a>(items: &'a [OutputItem]) -> List<'a> {
    let list_items: Vec<ListItem<'a>> = items.iter().map(render_item).collect();
    List::new(list_items).block(Block::default().borders(Borders::NONE))
}

fn render_item(item: &OutputItem) -> ListItem<'_> {
    match item {
        OutputItem::UserTurn { text } => ListItem::new(vec![Line::from(vec![
            Span::styled(
                "❯ ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw(text.clone()),
        ])]),
        OutputItem::AssistantMd { md, streaming } => {
            let mut lines: Vec<Line<'_>> = if *streaming {
                md.lines().map(|l| Line::from(l.to_string())).collect()
            } else {
                crate::markdown::render_markdown(md)
            };
            if *streaming {
                lines.push(Line::from(Span::styled(
                    "▏",
                    Style::default().add_modifier(Modifier::SLOW_BLINK),
                )));
            }
            ListItem::new(lines)
        }
        OutputItem::ToolCall {
            tool,
            args,
            status,
            result,
        } => {
            let (mark, style) = match status {
                ToolStatus::Running => ("  ⟶", Style::default().fg(Color::Yellow)),
                ToolStatus::Ok => ("  ✓", Style::default().fg(Color::Green)),
                ToolStatus::Err => ("  ✗", Style::default().fg(Color::Red)),
            };
            let head = Line::from(vec![
                Span::styled(mark, style),
                Span::raw(" "),
                Span::styled(tool.clone(), Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("("),
                Span::styled(args.clone(), Style::default().fg(Color::DarkGray)),
                Span::raw(")"),
            ]);
            let mut lines = vec![head];
            if let Some(r) = result {
                lines.push(Line::from(vec![
                    Span::raw("     "),
                    Span::styled(r.clone(), Style::default().fg(Color::DarkGray)),
                ]));
            }
            ListItem::new(lines)
        }
        OutputItem::SystemNote { text, level } => {
            let color = match level {
                NoteLevel::Info => Color::Blue,
                NoteLevel::Warn => Color::Yellow,
                NoteLevel::Error => Color::Red,
            };
            ListItem::new(vec![Line::from(vec![
                Span::styled("[atman] ", Style::default().fg(color)),
                Span::raw(text.clone()),
            ])])
        }
        OutputItem::Divider => ListItem::new(vec![Line::from(Span::styled(
            "─".repeat(40),
            Style::default().fg(Color::DarkGray),
        ))]),
    }
}

pub fn empty_hint<'a>() -> Paragraph<'a> {
    Paragraph::new("plain text → agent · :help for builtins · Ctrl+C to interrupt")
        .style(Style::default().fg(Color::DarkGray))
        .wrap(Wrap { trim: true })
}
