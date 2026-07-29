use std::collections::HashMap;

#[derive(Debug, Clone, PartialEq)]
pub enum NodeShape {
    Rectangle,
    Rounded,
    Stadium,
    Subroutine,
    Cylinder,
    Circle,
    DoubleCircle,
    Diamond,
    Hexagon,
    Parallelogram,
    TrapezoidUp,
    TrapezoidDown,
    Asymmetric,
}

#[derive(Debug, Clone, PartialEq)]
pub enum EdgeStyle {
    Solid,
    Dotted,
    Thick,
}

#[derive(Debug, Clone)]
pub struct Node {
    pub id: String,
    pub label: String,
    pub shape: NodeShape,
}

#[derive(Debug, Clone)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub label: Option<String>,
    pub style: EdgeStyle,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum Direction {
    LR,
    #[default]
    TD,
    RL,
    BT,
}

#[derive(Debug, Clone, Default)]
pub struct Subgraph {
    pub id: String,
    pub label: String,
    pub node_ids: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Flowchart {
    pub direction: Direction,
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub subgraphs: Vec<Subgraph>,
    pub node_map: HashMap<String, usize>,
}

pub fn parse_flowchart(source: &str) -> Flowchart {
    let mut fc = Flowchart::default();
    let mut current_subgraph: Option<Subgraph> = None;

    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }

        if let Some(rest) = line
            .strip_prefix("graph ")
            .or_else(|| line.strip_prefix("flowchart "))
        {
            let dir = rest.split_whitespace().next().unwrap_or("TD");
            fc.direction = parse_direction(dir);
            continue;
        }

        if line.starts_with("subgraph") {
            let rest = line.strip_prefix("subgraph").unwrap_or("").trim();
            let parts: Vec<&str> = rest.splitn(2, '[').collect();
            let id = parts[0].trim().to_string();
            let label = if parts.len() > 1 {
                parts[1].trim_end_matches(']').trim().to_string()
            } else {
                id.clone()
            };
            current_subgraph = Some(Subgraph {
                id,
                label,
                node_ids: Vec::new(),
            });
            continue;
        }

        if line == "end" {
            if let Some(sg) = current_subgraph.take() {
                fc.subgraphs.push(sg);
            }
            continue;
        }

        parse_edge_line(line, &mut fc, &mut current_subgraph);
    }

    for (i, node) in fc.nodes.iter().enumerate() {
        fc.node_map.insert(node.id.clone(), i);
    }
    fc
}

fn parse_direction(s: &str) -> Direction {
    match s.to_uppercase().as_str() {
        "LR" => Direction::LR,
        "TD" | "TB" => Direction::TD,
        "RL" => Direction::RL,
        "BT" => Direction::BT,
        _ => Direction::TD,
    }
}

fn parse_edge_line(line: &str, fc: &mut Flowchart, current_subgraph: &mut Option<Subgraph>) {
    let (from_id, from_shape, rest) = match parse_node_start(line) {
        Some(v) => v,
        None => return,
    };

    let (edge, rest) = match parse_edge(rest) {
        Some(v) => v,
        None => {
            register_node(fc, &from_id, from_shape, current_subgraph);
            return;
        }
    };

    let (to_id, to_shape, _) = match parse_node_start(rest.trim()) {
        Some(v) => v,
        None => return,
    };

    register_node(fc, &from_id, from_shape, current_subgraph);
    register_node(fc, &to_id, to_shape, current_subgraph);
    let mut edge = edge;
    edge.from = from_id.clone();
    edge.to = to_id.clone();
    fc.edges.push(edge);
}

fn register_node(
    fc: &mut Flowchart,
    id: &str,
    shape: Option<(String, NodeShape)>,
    current_subgraph: &mut Option<Subgraph>,
) {
    if !fc.node_map.contains_key(id) {
        let (label, shape) = shape.unwrap_or_else(|| (id.to_string(), NodeShape::Rectangle));
        fc.node_map.insert(id.to_string(), fc.nodes.len());
        fc.nodes.push(Node {
            id: id.to_string(),
            label,
            shape,
        });
    }
    if let Some(sg) = current_subgraph {
        if !sg.node_ids.contains(&id.to_string()) {
            sg.node_ids.push(id.to_string());
        }
    }
}

#[allow(clippy::type_complexity)]
fn parse_node_start(line: &str) -> Option<(String, Option<(String, NodeShape)>, &str)> {
    let chars: Vec<char> = line.chars().collect();
    if chars.is_empty() {
        return None;
    }

    let id: String = chars
        .iter()
        .take_while(|c| c.is_alphanumeric() || **c == '_')
        .collect();
    if id.is_empty() {
        return None;
    }

    let rest = &line[id.len()..];
    let rest = rest.trim_start();

    if rest.is_empty() {
        return Some((id, None, rest));
    }

    let first = rest.chars().next().unwrap();
    match first {
        '[' => {
            let close = rest.find(']')?;
            let label = rest[1..close].to_string();
            let shape = if label.starts_with('(') && label.ends_with(')') {
                NodeShape::Stadium
            } else {
                NodeShape::Rectangle
            };
            Some((id, Some((label, shape)), &rest[close + 1..]))
        }
        '(' => {
            let close = rest.find(')')?;
            let inner = &rest[1..close];
            if inner.starts_with('(') && inner.ends_with(')') {
                let label = &inner[1..inner.len() - 1];
                Some((
                    id,
                    Some((label.to_string(), NodeShape::DoubleCircle)),
                    &rest[close + 1..],
                ))
            } else if inner.starts_with('[') && inner.ends_with(']') {
                let label = &inner[1..inner.len() - 1];
                Some((
                    id,
                    Some((label.to_string(), NodeShape::Subroutine)),
                    &rest[close + 1..],
                ))
            } else {
                Some((
                    id,
                    Some((inner.to_string(), NodeShape::Rounded)),
                    &rest[close + 1..],
                ))
            }
        }
        '{' => {
            let close = rest.find('}')?;
            let inner = &rest[1..close];
            if inner.starts_with('{') && inner.ends_with('}') {
                let label = &inner[1..inner.len() - 1];
                Some((
                    id,
                    Some((label.to_string(), NodeShape::Hexagon)),
                    &rest[close + 1..],
                ))
            } else {
                Some((
                    id,
                    Some((inner.to_string(), NodeShape::Diamond)),
                    &rest[close + 1..],
                ))
            }
        }
        _ => Some((id, None, rest)),
    }
}

fn parse_edge(line: &str) -> Option<(Edge, &str)> {
    let arrows = ["==>", "-->", "-.->", "---", "--o", "--x", "<-->"];
    for arrow in &arrows {
        if let Some(pos) = line.find(arrow) {
            let after = &line[pos + arrow.len()..];

            let (label, after_edge) = parse_edge_label(after);

            let style = match *arrow {
                "==>" => EdgeStyle::Thick,
                "-.->" => EdgeStyle::Dotted,
                _ => EdgeStyle::Solid,
            };

            return Some((
                Edge {
                    from: String::new(),
                    to: String::new(),
                    label,
                    style,
                },
                after_edge,
            ));
        }
    }
    None
}

fn parse_edge_label(rest: &str) -> (Option<String>, &str) {
    let rest = rest.trim_start();
    if let Some(stripped) = rest.strip_prefix('|') {
        if let Some(close) = stripped.find('|') {
            let label = stripped[..close].to_string();
            return (Some(label), &stripped[close + 1..]);
        }
    }
    (None, rest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_flowchart() {
        let fc = parse_flowchart("graph TD\nA[Start] --> B[End]");
        assert_eq!(fc.direction, Direction::TD);
        assert_eq!(fc.nodes.len(), 2);
        assert_eq!(fc.nodes[0].id, "A");
        assert_eq!(fc.nodes[0].label, "Start");
        assert_eq!(fc.nodes[0].shape, NodeShape::Rectangle);
        assert_eq!(fc.edges.len(), 1);
        assert_eq!(fc.edges[0].from, "A");
    }

    #[test]
    fn parse_directions() {
        assert_eq!(parse_direction("LR"), Direction::LR);
        assert_eq!(parse_direction("TD"), Direction::TD);
        assert_eq!(parse_direction("TB"), Direction::TD);
    }

    #[test]
    fn parse_node_shapes() {
        let fc = parse_flowchart("graph LR\nA[Rect] --> B(Round)");
        assert_eq!(fc.nodes[0].shape, NodeShape::Rectangle);
        assert_eq!(fc.nodes[1].shape, NodeShape::Rounded);
    }

    #[test]
    fn parse_edge_label() {
        let fc = parse_flowchart("graph LR\nA -->|yes| B");
        assert_eq!(fc.edges[0].label.as_deref(), Some("yes"));
    }

    #[test]
    fn parse_subgraph() {
        let fc = parse_flowchart("graph TD\nsubgraph SG [Group]\nA --> B\nend");
        assert_eq!(fc.subgraphs.len(), 1);
        assert_eq!(fc.subgraphs[0].id, "SG");
        assert_eq!(fc.subgraphs[0].label, "Group");
    }
}
