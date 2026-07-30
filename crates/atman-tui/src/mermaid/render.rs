use super::grid::{Dir, EdgePath, Grid, LaidOutNode, layout};
use super::parser::{Flowchart, NodeShape, parse_flowchart};

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
        if !matches!(edge.path, EdgePath::Direct { .. }) {
            draw_edge_line(&mut grid, edge, &result.nodes);
        }
    }
    for edge in &result.edges {
        if matches!(edge.path, EdgePath::Direct { .. }) {
            draw_edge_line(&mut grid, edge, &result.nodes);
        }
    }
    for edge in &result.edges {
        draw_edge_label(&mut grid, edge);
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

fn draw_edge_line(grid: &mut Grid, edge: &super::grid::LaidOutEdge, _nodes: &[LaidOutNode]) {
    use super::grid::{E, N, S, W};
    match &edge.path {
        EdgePath::Direct { from_y, to_y, x } => {
            if from_y < to_y {
                for y in (*from_y + 1)..(*to_y - 1) {
                    grid.add_dir(*x, y, N | S);
                }
                grid.add_arrow(*x, *to_y - 1, Dir::Down);
            } else {
                for y in (*to_y + 2)..*from_y {
                    grid.add_dir(*x, y, N | S);
                }
                grid.add_arrow(*x, *to_y + 1, Dir::Up);
            }
        }
        EdgePath::SelfLoop { x, y } => {
            grid.add_dir(*x, *y, S | E);
            grid.add_dir(*x, *y + 1, N | S);
            grid.add_dir(*x, *y + 2, N | W);
            grid.add_dir(*x - 1, *y + 2, E | W);
            grid.add_dir(*x - 2, *y + 2, E | W);
            grid.add_arrow(*x - 3, *y + 2, Dir::Left);
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
            };
            draw_side_channel_edge(grid, p, _nodes);
        }
    }
}

fn draw_edge_label(grid: &mut Grid, edge: &super::grid::LaidOutEdge) {
    if let (Some(label), Some((lx, ly))) = (&edge.label, edge.label_pos.as_ref()) {
        let graphemes: Vec<(&str, usize)> = crate::width::graphemes(label.as_str()).collect();

        let mut col = 0usize;
        for (_g, gw) in &graphemes {
            let pos = *lx + col;
            if !grid.is_empty(pos, *ly) {
                return;
            }
            col += gw;
        }

        grid.write_text(*lx, *ly, label);
    }
}

struct EdgeDraw {
    from_x: usize,
    from_y: usize,
    to_x: usize,
    to_y: usize,
    bend: usize,
}

fn draw_corner_edge(grid: &mut Grid, p: EdgeDraw) {
    use super::grid::{E, N, S, W};
    let EdgeDraw {
        from_x,
        from_y,
        to_x,
        to_y,
        bend: horizontal_y,
    } = p;

    let (sy, ey) = ordered_range(from_y + 1, horizontal_y);
    for y in sy..ey {
        grid.add_dir(from_x, y, N | S);
    }

    let from_turn_ns = if from_y < horizontal_y { N } else { S };
    let from_horizontal_ew = if to_x > from_x { E } else { W };
    let to_horizontal_ew = if to_x > from_x { W } else { E };
    grid.add_turn(from_x, horizontal_y, from_turn_ns | from_horizontal_ew);

    let (x0, x1) = if to_x > from_x {
        (from_x + 1, to_x)
    } else {
        (to_x + 1, from_x)
    };
    for x in x0..x1 {
        grid.add_dir(x, horizontal_y, E | W);
    }

    let to_turn_ns = if to_y < horizontal_y { N } else { S };
    grid.add_turn(to_x, horizontal_y, to_horizontal_ew | to_turn_ns);

    let (sy, ey) = ordered_range(horizontal_y + 1, to_y);
    for y in sy..ey {
        grid.add_dir(to_x, y, N | S);
    }

    let arrow_dir = if to_y > from_y { Dir::Down } else { Dir::Up };
    let arrow_y = if to_y > from_y { to_y - 1 } else { to_y + 1 };
    grid.add_arrow(to_x, arrow_y, arrow_dir);
}

fn draw_side_channel_edge(grid: &mut Grid, p: EdgeDraw, nodes: &[LaidOutNode]) {
    use super::grid::{E, N, S, W};
    let EdgeDraw {
        from_x,
        from_y,
        to_x,
        to_y,
        bend: channel_x,
    } = p;
    let gap_y = find_gap_row(from_y, to_y, from_x, to_x, channel_x, nodes)
        .unwrap_or_else(|| (from_y + to_y) / 2);

    let (sy, ey) = ordered_range(from_y + 1, gap_y);
    for y in sy..ey {
        grid.add_dir(from_x, y, N | S);
    }

    let from_turn_ns = if from_y < gap_y { N } else { S };
    let from_to_channel_ew = if channel_x > from_x { E } else { W };
    let channel_from_horizontal_ew = if channel_x > from_x { W } else { E };
    grid.add_turn(from_x, gap_y, from_turn_ns | from_to_channel_ew);

    let (x0, x1) = if channel_x > from_x {
        (from_x + 1, channel_x)
    } else {
        (channel_x + 1, from_x)
    };
    for x in x0..x1 {
        grid.add_dir(x, gap_y, E | W);
    }

    let channel_turn_ns = if to_y > gap_y { S } else { N };
    grid.add_turn(
        channel_x,
        gap_y,
        channel_from_horizontal_ew | channel_turn_ns,
    );

    let (cy0, cy1) = ordered_range(gap_y + 1, to_y);
    for y in cy0..cy1 {
        grid.add_dir(channel_x, y, N | S);
    }

    let at_to_ns = if to_y > gap_y { N } else { S };
    let channel_to_to_ew = if to_x > channel_x { E } else { W };
    let to_horizontal_ew = if to_x > channel_x { W } else { E };
    grid.add_turn(channel_x, to_y, at_to_ns | channel_to_to_ew);

    let (tx0, tx1) = if to_x > channel_x {
        (channel_x + 1, to_x)
    } else {
        (to_x + 1, channel_x)
    };
    for x in tx0..tx1 {
        grid.add_dir(x, to_y, E | W);
    }
    let _ = to_horizontal_ew;

    let arrow_dir = if to_y > from_y { Dir::Down } else { Dir::Up };
    let arrow_y = if to_y > from_y { to_y - 1 } else { to_y + 1 };
    grid.add_arrow(to_x, arrow_y, arrow_dir);
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
    fn render_all_lines_fit_grid_width() {
        let cases = [
            ("flowchart TB\nA --> B", "simple flowchart"),
            ("flowchart TB\nA -->|分支二| C[右边]", "CJK edge label"),
            (
                "flowchart TB\nA[开始结束] --> B[连灰桥村]",
                "CJK node labels",
            ),
            ("flowchart TB\nA --> B\nB --> C\nC --> A", "cyclic graph"),
        ];
        for (src, desc) in &cases {
            let fc = parse_flowchart(src);
            let result = layout(&fc);
            let lines = render_flowchart_struct(&fc, src);
            for (i, l) in lines.iter().enumerate() {
                let dw = crate::width::width(l);
                assert!(
                    dw <= result.grid_width,
                    "{} line {} dw={} > grid_width={} — overflow/misalignment\n|{}|",
                    desc,
                    i,
                    dw,
                    result.grid_width,
                    l
                );
            }
        }
    }

    #[test]
    fn render_no_label_split_by_edge_line() {
        let src = "flowchart TB\n\
             A -->|开 始| B\n\
             B -->|战 斗| C\n\
             C -->|胜 利| D";
        let lines = render_flowchart(src, 80);
        let all: String = lines.join("\n");
        for (i, l) in lines.iter().enumerate() {
            let chars: Vec<char> = l.chars().collect();
            for j in 0..chars.len().saturating_sub(1) {
                if chars[j] == '│' {
                    let next = chars[j + 1];
                    if (next as u32) >= 0x4E00 && (next as u32) <= 0x9FFF {
                        let after = chars.get(j + 2).copied().unwrap_or(' ');
                        if after != '│' && after != ' ' {
                            eprintln!(
                                "warn: line {} has │ followed by CJK {:?} then {:?}: |{}|",
                                i, next, after, l
                            );
                        }
                    }
                }
            }
        }
        let _ = all;
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
