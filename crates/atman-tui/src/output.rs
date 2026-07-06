use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

use crate::app::{NoteLevel, OutputItem, ToolStatus};

const RESET: Style = Style::new();

pub fn build_lines(items: &[OutputItem]) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(items.len() * 3);
    for item in items {
        out.extend(render_item(item));
    }
    out
}

pub fn render_item(item: &OutputItem) -> Vec<Line<'static>> {
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
            ..
        } => {
            let (mark, style) = match status {
                ToolStatus::Running => ("  ⟶", Style::default().fg(Color::Yellow)),
                ToolStatus::Ok => ("  ✓", Style::default().fg(Color::Green)),
                ToolStatus::Err => ("  ✗", Style::default().fg(Color::Red)),
            };
            let head = Line::from(vec![
                Span::styled(mark.to_string(), style),
                Span::raw(" ".to_string()),
                Span::styled(tool.clone(), Style::default().add_modifier(Modifier::BOLD)),
                Span::raw("(".to_string()),
                Span::styled(args.clone(), Style::default().fg(Color::DarkGray)),
                Span::raw(")".to_string()),
            ]);
            let mut lines = vec![head];
            if let Some(r) = result {
                lines.push(Line::from(vec![
                    Span::raw("     ".to_string()),
                    Span::styled(r.clone(), Style::default().fg(Color::DarkGray)),
                ]));
            }
            lines
        }
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
    };
    lines.push(Line::from(Span::styled(String::new(), RESET)));
    lines
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
                history_id: None,
            },
            OutputItem::SystemNote {
                text: "note".into(),
                level: NoteLevel::Info,
            },
            OutputItem::Divider,
        ] {
            let lines = render_item(&item);
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
        let lines = render_item(&OutputItem::Divider);
        assert_eq!(lines.len(), 3);
    }

    #[test]
    fn build_lines_concats_all_items() {
        let items = vec![
            OutputItem::UserTurn { text: "hi".into() },
            OutputItem::Divider,
        ];
        let out = build_lines(&items);
        assert!(out.len() >= 4);
    }
}
