use super::grid::{Grid, PositionedNode, layout};
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
            draw_edge(&mut grid, from, to, &edge.style, edge.label.as_deref());
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

fn draw_edge(
    grid: &mut Grid,
    from: &PositionedNode,
    to: &PositionedNode,
    style: &EdgeStyle,
    label: Option<&str>,
) {
    let style_str = match style {
        EdgeStyle::Dotted => "dotted",
        EdgeStyle::Thick => "thick",
        EdgeStyle::Solid => "solid",
    };

    let from_right = from.x + from.w + 1;
    let from_bottom = from.y + from.h + 1;
    let from_center_y = from.y + 1;
    let from_center_x = from.x + from.w / 2 + 1;

    let to_left = to.x;
    let to_top = to.y;
    let to_center_y = to.y + 1;
    let to_center_x = to.x + to.w / 2 + 1;

    if to.x > from_right {
        // Rightward edge
        let mid_x = (from_right + to_left) / 2;
        grid.draw_h_line(from_right, mid_x, from_center_y, style_str);
        if from_center_y != to_center_y {
            grid.draw_v_line(
                mid_x,
                from_center_y.min(to_center_y),
                from_center_y.max(to_center_y) + 1,
                style_str,
            );
            grid.draw_h_line(mid_x, to_left, to_center_y, style_str);
        } else {
            grid.draw_h_line(mid_x, to_left, to_center_y, style_str);
        }
        grid.set(to_left - 1, to_center_y, '→');
    } else if to.y > from_bottom {
        // Downward edge
        let mid_y = (from_bottom + to_top) / 2;
        grid.draw_v_line(from_center_x, from_bottom, mid_y, style_str);
        if from_center_x != to_center_x {
            grid.draw_h_line(
                from_center_x.min(to_center_x),
                from_center_x.max(to_center_x) + 1,
                mid_y,
                style_str,
            );
            grid.draw_v_line(to_center_x, mid_y, to_top, style_str);
        } else {
            grid.draw_v_line(from_center_x, mid_y, to_top, style_str);
        }
        grid.set(to_center_x, to_top - 1, '↓');
    } else if to_right_of_from(grid.width, from, to) {
        // Already handled above
    } else {
        // Fallback: simple L-shaped path going right then down
        grid.draw_h_line(from_right, to_left, from_center_y, style_str);
        grid.draw_v_line(to_left, from_center_y, to_center_y + 1, style_str);
    }

    if let Some(label) = label {
        let label_w = crate::width::width(label);
        let lx = (from_right + to_left) / 2 - label_w / 2;
        let ly = from_center_y.saturating_sub(1);
        let mut i = 0usize;
        for (g, gw) in crate::width::graphemes(label) {
            for c in g.chars() {
                grid.set(lx + i, ly, c);
            }
            i += gw;
        }
    }
}

fn to_right_of_from(_grid_w: usize, _from: &PositionedNode, _to: &PositionedNode) -> bool {
    false
}
