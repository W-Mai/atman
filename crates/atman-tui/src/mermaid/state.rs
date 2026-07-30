use super::parser::{Direction, Edge, EdgeStyle, Flowchart, Node, NodeShape};
use super::render::render_flowchart_struct;

pub fn parse_state_diagram(source: &str) -> Flowchart {
    let mut fc = Flowchart::default();
    let mut state_labels: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut direction = Direction::TD;

    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if line == "stateDiagram-v2" || line == "stateDiagram" {
            continue;
        }
        if let Some(rest) = line.strip_prefix("direction ") {
            direction = match rest.trim().to_uppercase().as_str() {
                "LR" => Direction::LR,
                "RL" => Direction::RL,
                "BT" => Direction::BT,
                _ => Direction::TD,
            };
            continue;
        }
        if let Some(rest) = line.strip_prefix("state ") {
            let rest = rest.trim();
            if let Some(rest) = rest.strip_prefix('"') {
                if let Some(end) = rest.find('"') {
                    let label = rest[..end].to_string();
                    let after = rest[end + 1..].trim();
                    if let Some(id) = after.strip_prefix("as ") {
                        let id = id.trim().to_string();
                        state_labels.insert(id.clone(), label);
                        register_state(&mut fc, &id, state_labels.get(&id));
                    }
                    continue;
                }
            }
            if let Some(pos) = rest.find(':') {
                let id = rest[..pos].trim().to_string();
                let label = rest[pos + 1..].trim().to_string();
                state_labels.insert(id.clone(), label);
                register_state(&mut fc, &id, state_labels.get(&id));
                continue;
            }
            continue;
        }
        if let Some(pos) = line.find("-->") {
            let from_raw = line[..pos].trim().to_string();
            let after = line[pos + 3..].trim();
            let (to_raw, label) = if let Some(cpos) = after.find(':') {
                (
                    after[..cpos].trim().to_string(),
                    Some(after[cpos + 1..].trim().to_string()),
                )
            } else {
                (after.to_string(), None)
            };

            let from_id = resolve_state(&mut fc, &from_raw, &mut state_labels, true);
            let to_id = resolve_state(&mut fc, &to_raw, &mut state_labels, false);

            fc.edges.push(Edge {
                from: from_id,
                to: to_id,
                label,
                style: EdgeStyle::Solid,
            });
        }
    }

    fc.direction = direction;
    for (i, node) in fc.nodes.iter().enumerate() {
        fc.node_map.insert(node.id.clone(), i);
    }
    fc
}

fn register_state(fc: &mut Flowchart, id: &str, label: Option<&String>) {
    if fc.node_map.contains_key(id) {
        return;
    }
    let (label, shape) = state_node_shape(id, label);
    fc.node_map.insert(id.to_string(), fc.nodes.len());
    fc.nodes.push(Node {
        id: id.to_string(),
        label,
        shape,
    });
}

fn state_node_shape(id: &str, label: Option<&String>) -> (String, NodeShape) {
    if id == "[*]" {
        return ("●".to_string(), NodeShape::Circle);
    }
    let label = label.cloned().unwrap_or_else(|| id.to_string());
    (label, NodeShape::Rounded)
}

fn resolve_state(
    fc: &mut Flowchart,
    raw: &str,
    labels: &mut std::collections::HashMap<String, String>,
    is_source: bool,
) -> String {
    if raw == "[*]" {
        let id = if is_source { "__start__" } else { "__end__" };
        if !fc.node_map.contains_key(id) {
            let label = if is_source { "●" } else { "◎" };
            let shape = if is_source {
                NodeShape::Circle
            } else {
                NodeShape::DoubleCircle
            };
            fc.node_map.insert(id.to_string(), fc.nodes.len());
            fc.nodes.push(Node {
                id: id.to_string(),
                label: label.to_string(),
                shape,
            });
        }
        return id.to_string();
    }
    let id = raw.to_string();
    register_state(fc, &id, labels.get(&id));
    id
}

pub fn render_state_diagram(source: &str, width: u16) -> Vec<String> {
    let fc = parse_state_diagram(source);
    let _ = width;
    render_flowchart_struct(&fc, source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_states() {
        let src = "stateDiagram-v2\n[*] --> Idle\nIdle --> Running : Start\nRunning --> [*]";
        let fc = parse_state_diagram(src);
        assert_eq!(fc.nodes.len(), 4);
        assert_eq!(fc.edges.len(), 3);
        assert_eq!(fc.edges[1].label.as_deref(), Some("Start"));
    }

    #[test]
    fn parse_state_label() {
        let src = "stateDiagram-v2\nstate \"待机状态\" as Idle\nIdle --> Active";
        let fc = parse_state_diagram(src);
        let idle = fc.nodes.iter().find(|n| n.id == "Idle").unwrap();
        assert_eq!(idle.label, "待机状态");
    }

    #[test]
    fn parse_direction() {
        let src = "stateDiagram-v2\ndirection LR\nA --> B";
        let fc = parse_state_diagram(src);
        assert_eq!(fc.direction, Direction::LR);
    }

    #[test]
    fn render_state_diagram_cjk() {
        let src = "stateDiagram-v2\n[*] --> 待机\n待机 --> 运行 : 启动\n运行 --> 待机 : 停止\n运行 --> [*]";
        let lines = render_state_diagram(src, 120);
        let all: String = lines.join("\n");
        assert!(all.contains("待机"), "state label should appear:\n{all}");
        assert!(all.contains("运行"), "state label should appear:\n{all}");
        assert!(
            all.contains("启动") || all.contains("停止"),
            "transition label should appear:\n{all}"
        );
    }

    #[test]
    fn render_state_diagram_no_crash() {
        let src = "stateDiagram-v2\n[*] --> A\nA --> [*]";
        let lines = render_state_diagram(src, 80);
        assert!(!lines.is_empty());
    }
}
