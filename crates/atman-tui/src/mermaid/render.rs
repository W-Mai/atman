use super::grid::{Dir, EdgePath, Grid, LaidOutNode, layout};
use super::parser::{EdgeStyle, Flowchart, NodeShape, parse_flowchart};

pub fn render_flowchart(source: &str, _width: u16) -> Vec<String> {
    let fc = parse_flowchart(source);
    render_flowchart_struct(&fc, source)
}

/// Render a Flowchart struct directly (state.rs builds a Flowchart without
/// going through source text).
pub fn render_flowchart_struct(fc: &Flowchart, fallback_source: &str) -> Vec<String> {
    if fc.nodes.is_empty() {
        return fallback_source.lines().map(String::from).collect();
    }

    let result = layout(fc);
    let mut grid = Grid::new(result.grid_width, result.grid_height);

    for edge in &result.edges {
        draw_edge(&mut grid, edge, &result.nodes);
    }

    for node in &result.nodes {
        draw_node(&mut grid, node);
    }

    let mut lines = grid.to_lines();
    for sg in &fc.subgraphs {
        if !sg.label.is_empty() {
            lines.insert(0, format!("  subgraph {}", sg.label));
        }
    }
    lines
}

fn draw_node(grid: &mut Grid, node: &LaidOutNode) {
    match node.shape {
        NodeShape::Diamond => {
            grid.draw_diamond(node.x, node.y, node.w, &node.label);
        }
        NodeShape::Circle | NodeShape::DoubleCircle => {
            grid.draw_circle(node.x, node.y, node.w, &node.label);
        }
        NodeShape::Stadium => {
            grid.draw_stadium(node.x, node.y, node.w, node.h, &node.label);
        }
        _ => {
            grid.draw_box(node.x, node.y, node.w, node.h, &node.label);
        }
    }
}

fn style_h(style: &EdgeStyle) -> char {
    match style {
        EdgeStyle::Dotted => '┄',
        EdgeStyle::Thick => '═',
        EdgeStyle::Solid => '─',
    }
}

fn style_v(style: &EdgeStyle) -> char {
    match style {
        EdgeStyle::Dotted => '┆',
        EdgeStyle::Thick => '║',
        EdgeStyle::Solid => '│',
    }
}

fn draw_edge(grid: &mut Grid, edge: &super::grid::LaidOutEdge, _nodes: &[LaidOutNode]) {
    let h = style_h(&edge.style);
    let v = style_v(&edge.style);

    match &edge.path {
        EdgePath::Direct { from_y, to_y, x } => {
            if from_y < to_y {
                for y in (*from_y + 1)..(*to_y - 1) {
                    grid.merge_v(*x, y, v);
                }
                grid.set(*x, *to_y - 1, '↓');
            } else {
                for y in (*to_y + 2)..*from_y {
                    grid.merge_v(*x, y, v);
                }
                grid.set(*x, *to_y + 1, '↑');
            }
        }
        EdgePath::SelfLoop { x, y } => {
            grid.set(*x, *y, '┐');
            grid.set(*x, *y + 1, '│');
            grid.set(*x, *y + 2, '┘');
            grid.set(*x - 1, *y + 2, '─');
            grid.set(*x - 2, *y + 2, '─');
            grid.set(*x - 3, *y + 2, '←');
        }
        EdgePath::Corner {
            from_x,
            from_y,
            to_x,
            to_y,
            horizontal_y,
        } => {
            let p = EdgeDraw {
                from_x: *from_x,
                from_y: *from_y,
                to_x: *to_x,
                to_y: *to_y,
                bend: *horizontal_y,
                h,
                v,
            };
            draw_corner_edge(grid, p);
        }
        EdgePath::SideChannel {
            from_x,
            from_y,
            to_x,
            to_y,
            channel_x,
        } => {
            let p = EdgeDraw {
                from_x: *from_x,
                from_y: *from_y,
                to_x: *to_x,
                to_y: *to_y,
                bend: *channel_x,
                h,
                v,
            };
            draw_side_channel_edge(grid, p, _nodes);
        }
    }

    if let (Some(label), Some((lx, ly))) = (&edge.label, edge.label_pos.as_ref()) {
        let mut col = 0usize;
        for (g, gw) in crate::width::graphemes(label.as_str()) {
            let pos = *lx + col;
            if grid.is_empty(pos, *ly) {
                let first_char = g.chars().next().unwrap_or(' ');
                grid.set(pos, *ly, first_char);
                for extra in 1..gw {
                    grid.set(pos + extra, *ly, '\u{0}');
                }
            }
            col += gw;
        }
    }
}

struct EdgeDraw {
    from_x: usize,
    from_y: usize,
    to_x: usize,
    to_y: usize,
    bend: usize,
    h: char,
    v: char,
}

fn draw_corner_edge(grid: &mut Grid, p: EdgeDraw) {
    let EdgeDraw {
        from_x,
        from_y,
        to_x,
        to_y,
        bend: horizontal_y,
        h,
        v,
    } = p;
    let (sy, ey) = ordered_range(from_y + 1, horizontal_y);
    for y in sy..ey {
        grid.merge_v(from_x, y, v);
    }
    let from_turn = if from_y < horizontal_y {
        Dir::Down
    } else {
        Dir::Up
    };
    let to_horizontal = if to_x > from_x { Dir::Right } else { Dir::Left };
    grid.place_turn(from_x, horizontal_y, from_turn, to_horizontal);

    let (x0, x1) = if to_x > from_x {
        (from_x + 1, to_x)
    } else {
        (to_x + 1, from_x)
    };
    for x in x0..x1 {
        grid.merge_h(x, horizontal_y, h);
    }

    let to_turn = if to_y < horizontal_y {
        Dir::Up
    } else {
        Dir::Down
    };
    grid.place_turn(to_x, horizontal_y, to_horizontal, to_turn);

    let (sy, ey) = ordered_range(horizontal_y + 1, to_y);
    for y in sy..ey {
        grid.merge_v(to_x, y, v);
    }

    let arrow = if to_y > from_y { '↓' } else { '↑' };
    let arrow_y = if to_y > from_y { to_y - 1 } else { to_y + 1 };
    grid.set(to_x, arrow_y, arrow);
}

fn draw_side_channel_edge(grid: &mut Grid, p: EdgeDraw, nodes: &[LaidOutNode]) {
    let EdgeDraw {
        from_x,
        from_y,
        to_x,
        to_y,
        bend: channel_x,
        h,
        v,
    } = p;
    let gap_y = find_gap_row(from_y, to_y, from_x, to_x, channel_x, nodes)
        .unwrap_or_else(|| (from_y + to_y) / 2);

    let (sy, ey) = ordered_range(from_y + 1, gap_y);
    for y in sy..ey {
        grid.merge_v(from_x, y, v);
    }
    grid.place_turn(
        from_x,
        gap_y,
        if from_y < gap_y { Dir::Down } else { Dir::Up },
        if channel_x > from_x {
            Dir::Right
        } else {
            Dir::Left
        },
    );

    let (x0, x1) = if channel_x > from_x {
        (from_x + 1, channel_x)
    } else {
        (channel_x + 1, from_x)
    };
    for x in x0..x1 {
        grid.merge_h(x, gap_y, h);
    }

    grid.place_turn(
        channel_x,
        gap_y,
        if channel_x > from_x {
            Dir::Right
        } else {
            Dir::Left
        },
        if to_y > gap_y { Dir::Down } else { Dir::Up },
    );
    let (cy0, cy1) = ordered_range(gap_y + 1, to_y);
    for y in cy0..cy1 {
        grid.merge_v(channel_x, y, v);
    }

    grid.place_turn(
        channel_x,
        to_y,
        if to_y > gap_y { Dir::Down } else { Dir::Up },
        if to_x > channel_x {
            Dir::Right
        } else {
            Dir::Left
        },
    );
    let (tx0, tx1) = if to_x > channel_x {
        (channel_x + 1, to_x)
    } else {
        (to_x + 1, channel_x)
    };
    for x in tx0..tx1 {
        grid.merge_h(x, to_y, h);
    }

    let arrow = if to_y > from_y { '↓' } else { '↑' };
    let arrow_y = if to_y > from_y { to_y - 1 } else { to_y + 1 };
    grid.set(to_x, arrow_y, arrow);
}

/// Find a row between `from_y` and `to_y` where a horizontal line from
/// `from_x` to `channel_x` to `to_x` won't cross any node interior.
fn find_gap_row(
    from_y: usize,
    to_y: usize,
    from_x: usize,
    to_x: usize,
    channel_x: usize,
    nodes: &[LaidOutNode],
) -> Option<usize> {
    let (lo, hi) = ordered_range(from_y, to_y);
    for y in (lo + 1)..hi {
        let blocked = nodes.iter().any(|n| {
            n.top() <= y
                && y <= n.bottom()
                && (horizontal_segments_overlap(from_x, channel_x, n.left(), n.right())
                    || horizontal_segments_overlap(channel_x, to_x, n.left(), n.right()))
        });
        if !blocked {
            return Some(y);
        }
    }
    None
}

fn ordered_range(a: usize, b: usize) -> (usize, usize) {
    if a <= b { (a, b) } else { (b, a) }
}

fn horizontal_segments_overlap(ax1: usize, ax2: usize, bx1: usize, bx2: usize) -> bool {
    let (a_lo, a_hi) = if ax1 <= ax2 { (ax1, ax2) } else { (ax2, ax1) };
    let (b_lo, b_hi) = if bx1 <= bx2 { (bx1, bx2) } else { (bx2, bx1) };
    a_lo <= b_hi && b_lo <= a_hi
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
        let result = layout(&fc);
        assert!(!result.nodes.is_empty());
        let w0 = result.nodes[0].w;
        for (i, n) in result.nodes.iter().enumerate() {
            assert_eq!(n.w, w0, "node {} box width {} != {}", i, n.w, w0);
        }
    }

    #[test]
    fn layout_box_width_matches_max_label_display_width() {
        let fc = parse_flowchart(source());
        let result = layout(&fc);
        let max_label_w = fc
            .nodes
            .iter()
            .map(|n| crate::width::width(n.label.as_str()))
            .max()
            .unwrap_or(0);
        let expected_w = max_label_w + 2;
        for (i, n) in result.nodes.iter().enumerate() {
            assert_eq!(
                n.w, expected_w,
                "node {} box width {} != expected {}",
                i, n.w, expected_w
            );
        }
    }

    #[test]
    fn layout_no_node_overlap() {
        let fc = parse_flowchart(source());
        let result = layout(&fc);
        for i in 0..result.nodes.len() {
            for j in (i + 1)..result.nodes.len() {
                let a = &result.nodes[i];
                let b = &result.nodes[j];
                let x_overlap = a.left() < b.right() && b.left() < a.right();
                let y_overlap = a.top() < b.bottom() && b.top() < a.bottom();
                assert!(!(x_overlap && y_overlap), "nodes {} and {} overlap", i, j);
            }
        }
    }

    #[test]
    fn layout_nodes_fit_grid_bounds() {
        let fc = parse_flowchart(source());
        let result = layout(&fc);
        for n in &result.nodes {
            assert!(
                n.right() < result.grid_width,
                "node {} right {} >= grid_width {}",
                n.id,
                n.right(),
                result.grid_width
            );
            assert!(
                n.bottom() < result.grid_height,
                "node {} bottom {} >= grid_height {}",
                n.id,
                n.bottom(),
                result.grid_height
            );
        }
    }

    #[test]
    fn layout_cyclic_graph_no_infinite_loop() {
        let fc = parse_flowchart("flowchart TB\nA --> B\nB --> A");
        let result = layout(&fc);
        assert_eq!(result.nodes.len(), 2);
    }

    #[test]
    fn layout_layer_count_matches_dag_depth() {
        let fc = parse_flowchart("flowchart TB\nA --> B\nB --> C");
        let result = layout(&fc);
        let ys: std::collections::HashSet<usize> = result.nodes.iter().map(|n| n.y).collect();
        assert_eq!(ys.len(), 3, "expected 3 distinct layers, got {:?}", ys);
    }

    #[test]
    fn layout_corner_horizontal_y_in_gap_between_layers() {
        let fc = parse_flowchart("flowchart TB\nA --> B\nA --> C");
        let result = layout(&fc);
        for edge in &result.edges {
            if let EdgePath::Corner {
                from_y,
                to_y,
                horizontal_y,
                ..
            } = &edge.path
            {
                let (lo, hi) = if from_y < to_y {
                    (*from_y, *to_y)
                } else {
                    (*to_y, *from_y)
                };
                assert!(
                    lo < *horizontal_y && *horizontal_y < hi,
                    "horizontal_y {} not between from_y {} and to_y {}",
                    horizontal_y,
                    from_y,
                    to_y
                );
                for n in &result.nodes {
                    assert!(
                        !(n.top() <= *horizontal_y && *horizontal_y <= n.bottom()),
                        "horizontal_y {} is inside node {} ({}..{})",
                        horizontal_y,
                        n.id,
                        n.top(),
                        n.bottom()
                    );
                }
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
        let result = layout(&fc);
        let lines = render_flowchart(source(), 120);
        for (i, l) in lines.iter().enumerate() {
            let dw = crate::width::width(l);
            assert!(
                dw <= result.grid_width,
                "line {} display width {} > grid width {}",
                i,
                dw,
                result.grid_width
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
        let result = layout(&fc);
        let lines = render_flowchart(source(), 120);

        for node in &result.nodes {
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

    #[test]
    fn render_direct_edge_uses_arrow() {
        let lines = render_flowchart("flowchart TB\nA --> B", 80);
        let all: String = lines.join("\n");
        assert!(
            all.contains('↓') || all.contains('↑'),
            "direct edge should have an arrow, got:\n{all}"
        );
    }

    #[test]
    fn render_cjk_edge_label_aligned() {
        let src = "flowchart TB\nA -->|分支二| C[右边]";
        let lines = render_flowchart(src, 80);
        let all: String = lines.join("\n");
        assert!(all.contains("分支二"), "label should appear:\n{all}");

        let fc = parse_flowchart(src);
        let result = layout(&fc);
        for (i, l) in lines.iter().enumerate() {
            let dw = crate::width::width(l);
            assert!(
                dw <= result.grid_width,
                "line {} display width {} > grid_width {} — CJK label misaligned\n|{}|",
                i,
                dw,
                result.grid_width,
                l
            );
        }
    }

    #[test]
    fn render_cjk_node_label_in_box() {
        let src = "flowchart TB\nA[开始结束]";
        let lines = render_flowchart(src, 80);
        let all: String = lines.join("\n");
        assert!(all.contains("开始结束"), "node label should appear:\n{all}");

        let label_row = lines
            .iter()
            .find(|l| l.contains("开始结束"))
            .expect("label row should exist");
        assert!(
            label_row.contains('│'),
            "label row should have border chars:\n{}",
            label_row
        );
    }
}
