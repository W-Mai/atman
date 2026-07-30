use super::grid::{Dir, Grid, PositionedNode, layout};
use super::parser::{EdgeStyle, NodeShape, parse_flowchart};
use std::collections::HashMap;

pub fn render_flowchart(source: &str, _width: u16) -> Vec<String> {
    let fc = parse_flowchart(source);
    if fc.nodes.is_empty() {
        return source.lines().map(String::from).collect();
    }

    let (nodes, grid_w, grid_h) = layout(&fc);
    let mut grid = Grid::new(grid_w, grid_h);
    let node_pos: HashMap<String, &PositionedNode> =
        nodes.iter().map(|n| (n.id.clone(), n)).collect();

    for edge in &fc.edges {
        if let (Some(from), Some(to)) = (node_pos.get(&edge.from), node_pos.get(&edge.to)) {
            draw_edge(
                &mut grid,
                from,
                to,
                &edge.style,
                edge.label.as_deref(),
                &nodes,
            );
        }
    }

    for node in &nodes {
        draw_node(&mut grid, node);
    }

    grid.to_lines()
}

fn draw_node(grid: &mut Grid, node: &PositionedNode) {
    match node.shape {
        NodeShape::Diamond => {
            grid.draw_diamond(node.x, node.y, node.w, &node.label);
        }
        NodeShape::Circle | NodeShape::DoubleCircle => {
            grid.draw_circle(node.x, node.y, node.w, &node.label);
        }
        _ => {
            grid.draw_box(node.x, node.y, node.w, node.h, &node.label);
        }
    }
}

fn style_str(style: &EdgeStyle) -> &'static str {
    match style {
        EdgeStyle::Dotted => "dotted",
        EdgeStyle::Thick => "thick",
        EdgeStyle::Solid => "solid",
    }
}

fn is_blocked_by_node(x: usize, y: usize, nodes: &[PositionedNode], skip: &[&str]) -> bool {
    nodes
        .iter()
        .filter(|n| !skip.contains(&n.id.as_str()))
        .any(|n| n.contains(x, y))
}

fn draw_edge(
    grid: &mut Grid,
    from: &PositionedNode,
    to: &PositionedNode,
    style: &EdgeStyle,
    label: Option<&str>,
    nodes: &[PositionedNode],
) {
    let s = style_str(style);
    let skip = [from.id.as_str(), to.id.as_str()];

    let from_cx = from.center_x();
    let from_cy = from.center_y();
    let to_cx = to.center_x();
    let to_cy = to.center_y();

    if to.right() > from.right() && to.left() > from.right() {
        // ──→ rightward
        let start_x = from.right() + 1;
        let end_x = to.left();
        if from_cy == to_cy {
            draw_h_safe(grid, start_x, end_x, from_cy, s, nodes, &skip);
            grid.set(end_x, to_cy, '→');
        } else {
            let mid_x = (start_x + end_x) / 2;
            draw_h_safe(grid, start_x, mid_x, from_cy, s, nodes, &skip);
            grid.place_turn(
                mid_x,
                from_cy,
                Dir::Right,
                if to_cy > from_cy { Dir::Down } else { Dir::Up },
            );
            let (y0, y1) = if to_cy > from_cy {
                (from_cy + 1, to_cy)
            } else {
                (to_cy + 1, from_cy)
            };
            draw_v_safe(grid, mid_x, y0, y1, s, nodes, &skip);
            grid.place_turn(
                mid_x,
                to_cy,
                if to_cy > from_cy { Dir::Up } else { Dir::Down },
                Dir::Right,
            );
            draw_h_safe(grid, mid_x + 1, end_x, to_cy, s, nodes, &skip);
            grid.set(end_x, to_cy, '→');
        }
    } else if to.bottom() > from.bottom() && to.top() > from.bottom() {
        // ↓ downward
        let start_y = from.bottom() + 1;
        let end_y = to.top();
        if from_cx == to_cx {
            draw_v_safe(grid, from_cx, start_y, end_y, s, nodes, &skip);
            grid.set(from_cx, end_y, '↓');
        } else {
            let mid_y = (start_y + end_y) / 2;
            draw_v_safe(grid, from_cx, start_y, mid_y, s, nodes, &skip);
            grid.place_turn(
                from_cx,
                mid_y,
                Dir::Down,
                if to_cx > from_cx {
                    Dir::Right
                } else {
                    Dir::Left
                },
            );
            let (x0, x1) = if to_cx > from_cx {
                (from_cx + 1, to_cx)
            } else {
                (to_cx + 1, from_cx)
            };
            draw_h_safe(grid, x0, x1, mid_y, s, nodes, &skip);
            grid.place_turn(
                to_cx,
                mid_y,
                if to_cx > from_cx {
                    Dir::Right
                } else {
                    Dir::Left
                },
                Dir::Down,
            );
            draw_v_safe(grid, to_cx, mid_y + 1, end_y, s, nodes, &skip);
            grid.set(to_cx, end_y, '↓');
        }
    } else if to.left() < from.left() && to.right() < from.left() {
        // ← leftward
        let start_x = from.left();
        let end_x = to.right() + 1;
        if from_cy == to_cy {
            draw_h_safe(grid, end_x, start_x, from_cy, s, nodes, &skip);
            grid.set(end_x - 1, to_cy, '←');
        } else {
            let mid_x = (start_x + end_x) / 2;
            draw_h_safe(grid, mid_x, start_x, from_cy, s, nodes, &skip);
            grid.place_turn(
                mid_x,
                from_cy,
                Dir::Left,
                if to_cy > from_cy { Dir::Down } else { Dir::Up },
            );
            let (y0, y1) = if to_cy > from_cy {
                (from_cy + 1, to_cy)
            } else {
                (to_cy + 1, from_cy)
            };
            draw_v_safe(grid, mid_x, y0, y1, s, nodes, &skip);
            grid.place_turn(
                mid_x,
                to_cy,
                if to_cy > from_cy { Dir::Up } else { Dir::Down },
                Dir::Left,
            );
            draw_h_safe(grid, end_x, mid_x, to_cy, s, nodes, &skip);
            grid.set(end_x - 1, to_cy, '←');
        }
    } else {
        // fallback: go down from from.bottom(), then horizontal to to_cx, then down to to.top()
        let start_y = from.bottom() + 1;
        let mid_y = start_y + 1;
        draw_v_safe(grid, from_cx, start_y, mid_y, s, nodes, &skip);
        let (x0, x1) = if to_cx > from_cx {
            (from_cx + 1, to_cx)
        } else {
            (to_cx + 1, from_cx)
        };
        if x0 < x1 {
            grid.place_turn(
                from_cx,
                mid_y,
                Dir::Down,
                if to_cx > from_cx {
                    Dir::Right
                } else {
                    Dir::Left
                },
            );
            draw_h_safe(grid, x0, x1, mid_y, s, nodes, &skip);
            grid.place_turn(
                to_cx,
                mid_y,
                if to_cx > from_cx {
                    Dir::Right
                } else {
                    Dir::Left
                },
                Dir::Down,
            );
            draw_v_safe(grid, to_cx, mid_y + 1, to.top(), s, nodes, &skip);
        } else {
            draw_v_safe(grid, from_cx, mid_y, to.top(), s, nodes, &skip);
        }
        grid.set(to_cx, to.top(), '↓');
    }

    if let Some(label) = label {
        let label_w = crate::width::width(label);
        let mid_x = if to.left() > from.right() {
            (from.right() + to.left()) / 2
        } else {
            (from_cx + to_cx) / 2
        };
        let lx = mid_x.saturating_sub(label_w / 2);
        let ly = from_cy.saturating_sub(1);
        let mut i = 0usize;
        for (g, gw) in crate::width::graphemes(label) {
            let first_char = g.chars().next().unwrap_or(' ');
            grid.set(lx + i, ly, first_char);
            i += gw;
        }
    }
}

fn draw_h_safe(
    grid: &mut Grid,
    x1: usize,
    x2: usize,
    y: usize,
    style: &str,
    nodes: &[PositionedNode],
    skip: &[&str],
) {
    let c = match style {
        "dotted" => '┄',
        "thick" => '═',
        _ => '─',
    };
    for x in x1.min(x2)..x1.max(x2) {
        if is_blocked_by_node(x, y, nodes, skip) {
            continue;
        }
        let existing = grid.get(x, y);
        if existing == ' ' || existing == '\u{0}' {
            grid.set(x, y, c);
        }
    }
}

fn draw_v_safe(
    grid: &mut Grid,
    x: usize,
    y1: usize,
    y2: usize,
    style: &str,
    nodes: &[PositionedNode],
    skip: &[&str],
) {
    let c = match style {
        "dotted" => '┆',
        "thick" => '║',
        _ => '│',
    };
    for y in y1.min(y2)..y1.max(y2) {
        if is_blocked_by_node(x, y, nodes, skip) {
            continue;
        }
        let existing = grid.get(x, y);
        if existing == ' ' || existing == '\u{0}' {
            grid.set(x, y, c);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::parser::parse_flowchart;
    use super::*;

    fn source() -> &'static str {
        "flowchart TB\n\
         Start([🚀 嘿嘿大师启动]) --> Check{今天要干啥?}\n\
         Check -->|测 UI| Top[起 top]\n\
         Check -->|摸鱼| Game[连灰桥村]\n\
         Top --> TopOK{top 活着?}\n\
         TopOK -->|是| KeepTop[继续跑]\n\
         TopOK -->|否| Restart[重启再来]\n\
         Restart --> Top\n\
         KeepTop --> End([🌙 收工睡觉])\n\
         Game --> End\n"
    }

    #[test]
    fn layout_all_nodes_same_box_width() {
        let fc = parse_flowchart(source());
        let (nodes, _, _) = layout(&fc);
        assert!(!nodes.is_empty());
        let w0 = nodes[0].w;
        for (i, n) in nodes.iter().enumerate() {
            assert_eq!(n.w, w0, "node {} box width {} != {}", i, n.w, w0);
        }
    }

    #[test]
    fn layout_box_width_matches_max_label_display_width() {
        let fc = parse_flowchart(source());
        let (nodes, _, _) = layout(&fc);
        let max_label_w = fc
            .nodes
            .iter()
            .map(|n| crate::width::width(n.label.as_str()))
            .max()
            .unwrap_or(0);
        let expected_w = max_label_w + 2;
        for (i, n) in nodes.iter().enumerate() {
            assert_eq!(
                n.w, expected_w,
                "node {} box width {} != expected {}",
                i, n.w, expected_w
            );
        }
    }

    #[test]
    fn layout_no_overlap_within_layer() {
        let fc = parse_flowchart(source());
        let (nodes, _, _) = layout(&fc);
        for i in 0..nodes.len() {
            for j in (i + 1)..nodes.len() {
                let a = &nodes[i];
                let b = &nodes[j];
                let x_overlap = a.left() < b.right() && b.left() < a.right();
                let y_overlap = a.top() < b.bottom() && b.top() < a.bottom();
                assert!(!(x_overlap && y_overlap), "nodes {} and {} overlap", i, j);
            }
        }
    }

    #[test]
    fn render_cjk_flowchart_no_crash_on_cyclic() {
        let lines = render_flowchart("flowchart TB\nA --> B\nB --> A", 80);
        assert!(!lines.is_empty());
    }

    #[test]
    fn render_cjk_flowchart_label_visible() {
        let lines = render_flowchart(source(), 120);
        let all: String = lines.join("\n");
        assert!(all.contains("嘿嘿大师"), "label should appear");
        assert!(all.contains("收工睡觉"), "label should appear");
    }

    #[test]
    fn render_cjk_flowchart_all_lines_fit_width() {
        let fc = parse_flowchart(source());
        let (nodes, grid_w, grid_h) = layout(&fc);
        let grid = {
            let mut g = Grid::new(grid_w, grid_h);
            for n in &nodes {
                g.draw_box(n.x, n.y, n.w, n.h, &n.label);
            }
            g
        };
        let lines = grid.to_lines();
        for (i, l) in lines.iter().enumerate() {
            let dw = crate::width::width(l);
            assert!(
                dw <= grid_w,
                "line {} display width {} > grid width {}",
                i,
                dw,
                grid_w
            );
        }
    }

    #[test]
    fn render_edges_use_corner_chars() {
        let lines = render_flowchart(source(), 120);
        let all: String = lines.join("\n");
        let has_corner =
            all.contains('┌') || all.contains('┐') || all.contains('└') || all.contains('┘');
        assert!(has_corner, "edges should use corner chars, got:\n{all}");
    }

    #[test]
    fn render_edges_dont_cross_nodes() {
        let fc = parse_flowchart(source());
        let (nodes, _grid_w, _grid_h) = layout(&fc);
        let lines = render_flowchart(source(), 120);

        for node in &nodes {
            for y in node.top()..=node.bottom() {
                if y >= lines.len() {
                    continue;
                }
                let line = &lines[y];
                let mut col = 0usize;
                for (g, gw) in crate::width::graphemes(line) {
                    if col >= node.left() && col <= node.right() {
                        let c = g.chars().next().unwrap_or(' ');
                        if c == '─' || c == '│' || c == '┄' || c == '┆' {
                            // Lines inside node area are only OK if they're part of the node border
                            let is_border = col == node.left()
                                || col == node.right()
                                || y == node.top()
                                || y == node.bottom();
                            assert!(
                                is_border,
                                "edge char '{}' found inside node '{}' at ({},{}) — not on border",
                                c, node.id, col, y
                            );
                        }
                    }
                    col += gw;
                }
            }
        }
    }
}
