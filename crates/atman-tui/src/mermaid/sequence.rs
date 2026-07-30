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

    let n = sd.participants.len();

    const MAX_GAP_W: usize = 40;
    const PADDING: usize = 4;

    let name_widths: Vec<usize> = sd
        .participants
        .iter()
        .map(|p| crate::width::width(p.label.as_str()))
        .collect();

    let max_half = MAX_GAP_W / 2;
    let mut half_label_need = vec![0usize; n];
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
        if from_idx == to_idx {
            continue;
        }
        let lw = crate::width::width(msg.label.as_str());
        let span = from_idx.abs_diff(to_idx);
        let need = lw
            .div_ceil(span.max(1))
            .saturating_add(2)
            .saturating_sub(PADDING)
            .min(max_half);
        if need > half_label_need[from_idx] {
            half_label_need[from_idx] = need;
        }
        if need > half_label_need[to_idx] {
            half_label_need[to_idx] = need;
        }
    }

    let col_widths: Vec<usize> = (0..n)
        .map(|i| name_widths[i].max(half_label_need[i]) + PADDING)
        .collect();

    let mut col_x = vec![0usize; n];
    let mut x = 0;
    for i in 0..n {
        col_x[i] = x + col_widths[i] / 2;
        x += col_widths[i];
    }
    let total_w = x;

    let mut lines = Vec::new();

    let mut header = String::new();
    for (i, p) in sd.participants.iter().enumerate() {
        let cw = col_widths[i];
        let pad = cw.saturating_sub(crate::width::width(p.label.as_str()));
        let left = pad / 2;
        let right = pad - left;
        header.push_str(&" ".repeat(left));
        header.push_str(&p.label);
        header.push_str(&" ".repeat(right));
    }
    lines.push(header);

    let mut sep = String::new();
    for _ in 0..total_w {
        sep.push('─');
    }
    lines.push(sep);

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

        let from_x = col_x[from_idx];
        let to_x = col_x[to_idx];

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

        for &lx in col_x.iter().take(n) {
            if lx < row.len() {
                row[lx] = '│';
            }
        }

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

        if !msg.label.is_empty() {
            let gap_w = x1.saturating_sub(x0).max(1);
            let wrap_w = gap_w.min(MAX_GAP_W);
            let wrapped = wrap_label(msg.label.as_str(), wrap_w);
            let label_mid = (x0 + x1) / 2;
            for line in &wrapped {
                let line_w = crate::width::width(line.as_str());
                let label_start = label_mid.saturating_sub(line_w / 2);
                let mut label_row: Vec<char> = vec![' '; total_w];
                let mut col = 0usize;
                for (g, gw) in crate::width::graphemes(line.as_str()) {
                    let pos = label_start + col;
                    let first_char = g.chars().next().unwrap_or(' ');
                    if pos < label_row.len() {
                        label_row[pos] = first_char;
                    }
                    for extra in 1..gw {
                        if pos + extra < label_row.len() {
                            label_row[pos + extra] = '\u{0}';
                        }
                    }
                    col += gw;
                }
                let label_str: String = label_row.iter().filter(|c| **c != '\u{0}').collect();
                lines.push(label_str);
            }
        }
    }

    lines
}

fn wrap_label(label: &str, max_w: usize) -> Vec<String> {
    if max_w == 0 {
        return vec![label.to_string()];
    }

    let mut tokens: Vec<(String, usize)> = Vec::new();
    let mut cur = String::new();
    let mut cur_w = 0usize;
    for (g, gw) in crate::width::graphemes(label) {
        if g == " " {
            if !cur.is_empty() {
                tokens.push((std::mem::take(&mut cur), cur_w));
                cur_w = 0;
            }
            tokens.push((" ".to_string(), 1));
        } else {
            cur.push_str(g);
            cur_w += gw;
        }
    }
    if !cur.is_empty() {
        tokens.push((cur, cur_w));
    }

    let mut result = Vec::new();
    let mut line = String::new();
    let mut line_w = 0usize;

    for (tok, tw) in tokens {
        if tok == " " {
            if line_w < max_w {
                line.push(' ');
                line_w += 1;
            } else if !line.is_empty() {
                result.push(std::mem::take(&mut line));
                line_w = 0;
            }
            continue;
        }

        if line_w + tw <= max_w {
            line.push_str(&tok);
            line_w += tw;
        } else {
            if !line.is_empty() {
                if line.ends_with(' ') {
                    line.pop();
                }
                result.push(std::mem::take(&mut line));
                line_w = 0;
            }
            if tw <= max_w {
                line.push_str(&tok);
                line_w = tw;
            } else {
                for (g, gw) in crate::width::graphemes(&tok) {
                    if line_w + gw > max_w && !line.is_empty() {
                        result.push(std::mem::take(&mut line));
                        line_w = 0;
                    }
                    line.push_str(g);
                    line_w += gw;
                }
            }
        }
    }
    if !line.is_empty() {
        while line.ends_with(' ') {
            line.pop();
        }
        result.push(line);
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
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

    #[test]
    fn render_cjk_sequence_aligned() {
        let sd = parse_sequence(
            "sequenceDiagram\nparticipant A as 爱丽丝\nparticipant B as 鲍勃\nA->B: 你好\nB->A: 收到了",
        );
        let lines = render_sequence(&sd);
        let all: String = lines.join("\n");
        assert!(all.contains("爱丽丝"), "participant label should appear");
        assert!(all.contains("你好"), "message label should appear");

        let widths: Vec<usize> = lines
            .iter()
            .filter(|l| !l.is_empty())
            .map(|l| crate::width::width(l))
            .collect();
        let min = *widths.iter().min().unwrap_or(&0);
        let max = *widths.iter().max().unwrap_or(&0);
        assert_eq!(
            min, max,
            "sequence rows have mismatched widths: min={} max={}",
            min, max
        );
    }

    #[test]
    fn render_long_label_not_truncated() {
        let sd = parse_sequence(
            "sequenceDiagram\nparticipant A as A\nparticipant B as B\nparticipant C as C\nA->>B: 这是一个超级长的标签它比两个actor的间距还宽很多很多\nB->>C: ok",
        );
        let lines = render_sequence(&sd);
        let all: String = lines.join("\n");
        for chunk in &[
            "这是一个超级长的",
            "标签它比",
            "两个actor",
            "的间距还宽很多",
            "很多",
        ] {
            assert!(
                all.contains(chunk),
                "label chunk {:?} missing from output:\n{all}",
                chunk
            );
        }

        let widths: Vec<usize> = lines
            .iter()
            .filter(|l| !l.is_empty())
            .map(|l| crate::width::width(l))
            .collect();
        let min = *widths.iter().min().unwrap_or(&0);
        let max = *widths.iter().max().unwrap_or(&0);
        assert_eq!(min, max, "rows misaligned: min={} max={}", min, max);
    }

    #[test]
    fn render_multi_actor_span_label() {
        let sd = parse_sequence(
            "sequenceDiagram\nparticipant A as 爱丽丝\nparticipant B as 鲍勃\nparticipant C as 查理\nA->>C: 跨越两个actor的长消息\nB->>C: 短消息",
        );
        let lines = render_sequence(&sd);
        let all: String = lines.join("\n");
        assert!(
            all.contains("跨越两个actor的长消息"),
            "label should appear:\n{all}"
        );
        assert!(all.contains("短消息"));
        assert!(all.contains("爱丽丝"));
        assert!(all.contains("查理"));

        let widths: Vec<usize> = lines
            .iter()
            .filter(|l| !l.is_empty())
            .map(|l| crate::width::width(l))
            .collect();
        let min = *widths.iter().min().unwrap_or(&0);
        let max = *widths.iter().max().unwrap_or(&0);
        assert_eq!(min, max, "rows misaligned: min={} max={}", min, max);
    }
}
