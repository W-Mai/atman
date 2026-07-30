#[derive(Debug, Clone)]
pub struct Participant {
    pub id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MessageStyle {
    Solid,
    Dotted,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub from: String,
    pub to: String,
    pub label: String,
    pub style: MessageStyle,
    pub arrow: ArrowType,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ArrowType {
    Filled,
    Open,
}

#[derive(Debug, Clone, Default)]
pub struct SequenceDiagram {
    pub participants: Vec<Participant>,
    pub messages: Vec<Message>,
}

pub fn parse_sequence(source: &str) -> SequenceDiagram {
    let mut sd = SequenceDiagram::default();
    let mut participant_order: Vec<String> = Vec::new();

    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }

        if line == "sequenceDiagram" {
            continue;
        }

        if let Some(rest) = line.strip_prefix("participant ") {
            let parts: Vec<&str> = rest.splitn(2, " as ").collect();
            let id = parts[0].trim().to_string();
            let label = if parts.len() > 1 {
                parts[1].trim().to_string()
            } else {
                id.clone()
            };
            if !sd.participants.iter().any(|p| p.id == id) {
                sd.participants.push(Participant {
                    id: id.clone(),
                    label,
                });
                participant_order.push(id);
            }
            continue;
        }

        if let Some(msg) = parse_message(line) {
            for pid in [&msg.from, &msg.to] {
                if !sd.participants.iter().any(|p| p.id == *pid) {
                    sd.participants.push(Participant {
                        id: pid.clone(),
                        label: pid.clone(),
                    });
                    participant_order.push(pid.clone());
                }
            }
            sd.messages.push(msg);
        }
    }

    sd
}

fn parse_message(line: &str) -> Option<Message> {
    let arrows = [
        ("-->>", ArrowType::Open, MessageStyle::Dotted),
        ("->>", ArrowType::Filled, MessageStyle::Solid),
        ("-->", ArrowType::Open, MessageStyle::Dotted),
        ("->", ArrowType::Filled, MessageStyle::Solid),
    ];

    for (arrow_str, arrow_type, style) in &arrows {
        if let Some(pos) = line.find(arrow_str) {
            let from = line[..pos].trim().to_string();
            let rest = &line[pos + arrow_str.len()..];

            let (to, label) = if let Some(colon) = rest.find(':') {
                let to = rest[..colon].trim().to_string();
                let label = rest[colon + 1..].trim().to_string();
                (to, label)
            } else {
                (rest.trim().to_string(), String::new())
            };

            if from.is_empty() || to.is_empty() {
                continue;
            }

            return Some(Message {
                from,
                to,
                label,
                style: style.clone(),
                arrow: arrow_type.clone(),
            });
        }
    }
    None
}

pub fn render_sequence(sd: &SequenceDiagram) -> Vec<String> {
    if sd.participants.is_empty() {
        return Vec::new();
    }

    let max_name = sd
        .participants
        .iter()
        .map(|p| crate::width::width(p.label.as_str()))
        .max()
        .unwrap_or(3)
        .max(3);

    let col_width = max_name + 4;
    let n = sd.participants.len();
    let total_w = col_width * n;

    let mut lines = Vec::new();

    // Header: participant names centered
    let mut header = String::new();
    for (i, p) in sd.participants.iter().enumerate() {
        let pad = col_width.saturating_sub(crate::width::width(p.label.as_str()));
        let left = pad / 2;
        let right = pad - left;
        header.push_str(&" ".repeat(left));
        header.push_str(&p.label);
        header.push_str(&" ".repeat(right));
        let _ = i;
    }
    lines.push(header);

    // Separator line
    let mut sep = String::new();
    for _ in 0..total_w {
        sep.push('─');
    }
    lines.push(sep);

    // Messages
    for msg in &sd.messages {
        let from_idx = sd
            .participants
            .iter()
            .position(|p| p.id == msg.from)
            .unwrap_or(0);
        let to_idx = sd
            .participants
            .iter()
            .position(|p| p.id == msg.to)
            .unwrap_or(0);

        let from_x = from_idx * col_width + col_width / 2;
        let to_x = to_idx * col_width + col_width / 2;

        let (x0, x1) = if from_x < to_x {
            (from_x, to_x)
        } else {
            (to_x, from_x)
        };

        let line_char = match msg.style {
            MessageStyle::Solid => '─',
            MessageStyle::Dotted => '┄',
        };

        let arrow_char = if from_x < to_x {
            match msg.arrow {
                ArrowType::Filled => '▶',
                ArrowType::Open => '▷',
            }
        } else {
            match msg.arrow {
                ArrowType::Filled => '◀',
                ArrowType::Open => '◁',
            }
        };

        let mut row: Vec<char> = vec![' '; total_w];

        // Draw lifelines
        for i in 0..n {
            let lx = i * col_width + col_width / 2;
            if lx < row.len() {
                row[lx] = '│';
            }
        }

        // Draw arrow
        for x in (x0 + 1)..x1 {
            if x < row.len() && row[x] == '│' {
                row[x] = '┼';
            } else if x < row.len() {
                row[x] = line_char;
            }
        }
        if to_x < row.len() {
            row[to_x] = arrow_char;
        }

        let row_str: String = row.iter().collect();
        lines.push(row_str);

        // Label line
        if !msg.label.is_empty() {
            let label_mid = (x0 + x1) / 2;
            let label_half = crate::width::width(msg.label.as_str()) / 2;
            let label_start = label_mid.saturating_sub(label_half);
            let mut label_row: Vec<char> = vec![' '; total_w];
            for (i, c) in msg.label.chars().enumerate() {
                let pos = label_start + i;
                if pos < label_row.len() {
                    label_row[pos] = c;
                }
            }
            let label_str: String = label_row.iter().collect();
            lines.push(label_str);
        }
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_sequence() {
        let sd = parse_sequence("sequenceDiagram\nA->>B: Hello");
        assert_eq!(sd.participants.len(), 2);
        assert_eq!(sd.messages.len(), 1);
        assert_eq!(sd.messages[0].from, "A");
        assert_eq!(sd.messages[0].to, "B");
        assert_eq!(sd.messages[0].label, "Hello");
    }

    #[test]
    fn parse_dotted_reply() {
        let sd = parse_sequence("sequenceDiagram\nA->>B: Request\nB-->>A: Reply");
        assert_eq!(sd.messages.len(), 2);
        assert_eq!(sd.messages[1].style, MessageStyle::Dotted);
    }

    #[test]
    fn parse_participant_alias() {
        let sd = parse_sequence("sequenceDiagram\nparticipant A as Alice\nA->>B: Hi");
        assert_eq!(sd.participants[0].label, "Alice");
    }
}
