use std::collections::HashMap;

#[derive(Clone, Default)]
pub struct Grid {
    pub cells: Vec<Vec<char>>,
    pub width: usize,
    pub height: usize,
}

impl Grid {
    pub fn new(width: usize, height: usize) -> Self {
        Grid {
            cells: vec![vec![' '; width]; height],
            width,
            height,
        }
    }

    pub fn get(&self, x: usize, y: usize) -> char {
        if y < self.height && x < self.width {
            self.cells[y][x]
        } else {
            ' '
        }
    }

    pub fn set(&mut self, x: usize, y: usize, c: char) {
        if y < self.height && x < self.width {
            self.cells[y][x] = c;
        }
    }

    pub fn write_str(&mut self, x: usize, y: usize, s: &str) {
        let mut i = 0usize;
        for (g, gw) in crate::width::graphemes(s) {
            let first_char = g.chars().next().unwrap_or(' ');
            self.set(x + i, y, first_char);
            for extra in 1..gw {
                self.set(x + i + extra, y, '\u{0}');
            }
            i += gw;
        }
    }

    pub fn is_empty(&self, x: usize, y: usize) -> bool {
        let c = self.get(x, y);
        c == ' ' || c == '\u{0}'
    }

    pub fn draw_h_segment(&mut self, x1: usize, x2: usize, y: usize, style: &str) {
        let c = match style {
            "dotted" => '┄',
            "thick" => '═',
            _ => '─',
        };
        for x in x1..x2 {
            if self.is_empty(x, y) {
                self.set(x, y, c);
            }
        }
    }

    pub fn draw_v_segment(&mut self, x: usize, y1: usize, y2: usize, style: &str) {
        let c = match style {
            "dotted" => '┆',
            "thick" => '║',
            _ => '│',
        };
        for y in y1..y2 {
            if self.is_empty(x, y) {
                self.set(x, y, c);
            }
        }
    }

    #[allow(dead_code)]
    fn place_corner(&mut self, x: usize, y: usize, style: &str) {
        if !self.is_empty(x, y) {
            return;
        }
        let c = match style {
            "dotted" => '┄',
            "thick" => '═',
            _ => '─',
        };
        self.set(x, y, c);
    }

    pub fn place_turn(&mut self, x: usize, y: usize, from_dir: Dir, to_dir: Dir) {
        let existing = self.get(x, y);
        let corner = turn_char(from_dir, to_dir, existing);
        self.set(x, y, corner);
    }

    /// Draw a vertical line segment, merging with any existing char.
    pub fn merge_v(&mut self, x: usize, y: usize, v: char) {
        let existing = self.get(x, y);
        self.set(x, y, merge_chars(existing, v));
    }

    /// Draw a horizontal line segment, merging with any existing char.
    pub fn merge_h(&mut self, x: usize, y: usize, h: char) {
        let existing = self.get(x, y);
        self.set(x, y, merge_chars(existing, h));
    }

    #[allow(dead_code)]
    pub fn place_t_junction(&mut self, x: usize, y: usize, stem: Dir) {
        let existing = self.get(x, y);
        let c = t_junction_char(stem, existing);
        self.set(x, y, c);
    }

    #[allow(dead_code)]
    pub fn place_cross(&mut self, x: usize, y: usize) {
        let existing = self.get(x, y);
        let c = match existing {
            '│' | '─' => '┼',
            '├' | '┤' | '┬' | '┴' => '┼',
            _ => existing,
        };
        self.set(x, y, c);
    }

    pub fn draw_box(&mut self, x: usize, y: usize, w: usize, h: usize, label: &str) {
        self.set(x, y, '╭');
        self.set(x + w + 1, y, '╮');
        self.set(x, y + h + 1, '╰');
        self.set(x + w + 1, y + h + 1, '╯');
        for i in 1..=w {
            self.set(x + i, y, '─');
            self.set(x + i, y + h + 1, '─');
        }
        for row in 0..h {
            self.set(x, y + 1 + row, '│');
            self.set(x + w + 1, y + 1 + row, '│');
        }
        let truncated = crate::width::truncate_plain(label, w);
        let label_w = crate::width::width(&truncated);
        let pad = w.saturating_sub(label_w) / 2;
        self.write_str(x + 1 + pad, y + 1, &truncated);
    }

    pub fn draw_diamond(&mut self, x: usize, y: usize, w: usize, label: &str) {
        let cx = x + w / 2 + 1;
        self.set(cx, y, '◇');
        self.set(x, y + 1, '◇');
        self.set(x + w + 1, y + 1, '◇');
        self.set(cx, y + 2, '◇');
        let truncated = crate::width::truncate_plain(label, w);
        let lw = crate::width::width(&truncated);
        let pad = w.saturating_sub(lw) / 2;
        self.write_str(x + 1 + pad, y + 1, &truncated);
    }

    pub fn draw_circle(&mut self, x: usize, y: usize, w: usize, label: &str) {
        self.set(x + w / 2 + 1, y, '○');
        self.set(x, y + 1, '(');
        self.set(x + w + 1, y + 1, ')');
        self.set(x + w / 2 + 1, y + 2, '○');
        let truncated = crate::width::truncate_plain(label, w);
        let lw = crate::width::width(&truncated);
        let pad = w.saturating_sub(lw) / 2;
        self.write_str(x + 1 + pad, y + 1, &truncated);
    }

    pub fn draw_h_line(&mut self, x1: usize, x2: usize, y: usize, style: &str) {
        self.draw_h_segment(x1, x2, y, style);
    }

    pub fn draw_v_line(&mut self, x: usize, y1: usize, y2: usize, style: &str) {
        self.draw_v_segment(x, y1, y2, style);
    }

    pub fn to_lines(&self) -> Vec<String> {
        self.cells
            .iter()
            .map(|row| {
                let s: String = row.iter().filter(|c| **c != '\u{0}').collect();
                s.trim_end().to_string()
            })
            .collect()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

fn turn_char(from: Dir, to: Dir, existing: char) -> char {
    let pair = (from, to);
    let corner = match pair {
        (Dir::Up, Dir::Right) | (Dir::Left, Dir::Down) => '┌',
        (Dir::Up, Dir::Left) | (Dir::Right, Dir::Down) => '┐',
        (Dir::Down, Dir::Right) | (Dir::Left, Dir::Up) => '└',
        (Dir::Down, Dir::Left) | (Dir::Right, Dir::Up) => '┘',
        _ => '─',
    };
    merge_chars(existing, corner)
}

#[allow(dead_code)]
fn t_junction_char(stem: Dir, existing: char) -> char {
    let c = match stem {
        Dir::Down => '┬',
        Dir::Up => '┴',
        Dir::Right => '├',
        Dir::Left => '┤',
    };
    merge_chars(existing, c)
}

const MERGE_MASKS: [(char, u8); 12] = [
    (' ', 0b0000),
    ('│', 0b0011),
    ('─', 0b1100),
    ('┌', 0b1010),
    ('┐', 0b0110),
    ('└', 0b1001),
    ('┘', 0b0101),
    ('├', 0b1011),
    ('┤', 0b0111),
    ('┬', 0b1110),
    ('┴', 0b1101),
    ('┼', 0b1111),
];

fn merge_chars(existing: char, new: char) -> char {
    let em = MERGE_MASKS
        .iter()
        .find(|&&(c, _)| c == existing)
        .map(|&(_, m)| m)
        .unwrap_or(0);
    let nm = MERGE_MASKS
        .iter()
        .find(|&&(c, _)| c == new)
        .map(|&(_, m)| m)
        .unwrap_or(0);
    let u = em | nm;
    match u {
        0b0000 => ' ',
        0b0011 => '│',
        0b0101 => '┘',
        0b0110 => '┐',
        0b0111 => '┤',
        0b1001 => '└',
        0b1010 => '┌',
        0b1011 => '├',
        0b1100 => '─',
        0b1101 => '┴',
        0b1110 => '┬',
        0b1111 => '┼',
        _ => new,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn turn_char_connects_correct_directions() {
        assert_eq!(turn_char(Dir::Up, Dir::Right, ' '), '┌');
        assert_eq!(turn_char(Dir::Left, Dir::Down, ' '), '┌');
        assert_eq!(turn_char(Dir::Up, Dir::Left, ' '), '┐');
        assert_eq!(turn_char(Dir::Right, Dir::Down, ' '), '┐');
        assert_eq!(turn_char(Dir::Down, Dir::Right, ' '), '└');
        assert_eq!(turn_char(Dir::Left, Dir::Up, ' '), '└');
        assert_eq!(turn_char(Dir::Down, Dir::Left, ' '), '┘');
        assert_eq!(turn_char(Dir::Right, Dir::Up, ' '), '┘');
    }

    #[test]
    fn merge_chars_preserves_direction_union() {
        let all = [' ', '│', '─', '┌', '┐', '└', '┘', '├', '┤', '┬', '┴', '┼'];
        for &e in &all {
            for &n in &all {
                let got = merge_chars(e, n);
                let em = MERGE_MASKS
                    .iter()
                    .find(|&&(c, _)| c == e)
                    .map(|&(_, m)| m)
                    .unwrap_or(0);
                let nm = MERGE_MASKS
                    .iter()
                    .find(|&&(c, _)| c == n)
                    .map(|&(_, m)| m)
                    .unwrap_or(0);
                let u = em | nm;
                let exp = MERGE_MASKS.iter().find(|&&(_, m)| m == u).map(|&(c, _)| c);
                if let Some(expected) = exp {
                    assert_eq!(
                        got, expected,
                        "merge_chars('{e}', '{n}'): got '{got}', expected '{expected}'"
                    );
                }
            }
        }
    }
}

#[derive(Debug)]
pub struct LayoutResult {
    pub nodes: Vec<LaidOutNode>,
    pub edges: Vec<LaidOutEdge>,
    pub grid_width: usize,
    pub grid_height: usize,
}

#[derive(Debug)]
pub struct LaidOutNode {
    pub id: String,
    pub label: String,
    pub shape: super::parser::NodeShape,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

impl LaidOutNode {
    pub fn left(&self) -> usize {
        self.x
    }
    pub fn right(&self) -> usize {
        self.x + self.w + 1
    }
    pub fn top(&self) -> usize {
        self.y
    }
    pub fn bottom(&self) -> usize {
        self.y + self.h + 1
    }
    pub fn center_x(&self) -> usize {
        self.x + self.w / 2 + 1
    }
    pub fn center_y(&self) -> usize {
        self.y + 1
    }
    pub fn contains(&self, x: usize, y: usize) -> bool {
        x >= self.left() && x <= self.right() && y >= self.top() && y <= self.bottom()
    }
}

#[derive(Debug)]
pub struct LaidOutEdge {
    pub from: String,
    pub to: String,
    pub style: super::parser::EdgeStyle,
    pub label: Option<String>,
    pub label_pos: Option<(usize, usize)>,
    pub path: EdgePath,
}

#[derive(Debug)]
pub enum EdgePath {
    Direct {
        from_y: usize,
        to_y: usize,
        x: usize,
    },
    Corner {
        from_x: usize,
        from_y: usize,
        to_x: usize,
        to_y: usize,
        horizontal_y: usize,
    },
    SideChannel {
        from_x: usize,
        from_y: usize,
        to_x: usize,
        to_y: usize,
        channel_x: usize,
    },
}

pub fn layout(fc: &super::parser::Flowchart) -> LayoutResult {
    if fc.nodes.is_empty() {
        return LayoutResult {
            nodes: Vec::new(),
            edges: Vec::new(),
            grid_width: 0,
            grid_height: 0,
        };
    }

    let max_label_w = fc
        .nodes
        .iter()
        .map(|n| crate::width::width(n.label.as_str()))
        .max()
        .unwrap_or(3)
        .max(3);
    let box_w = max_label_w + 2;
    let box_h = 1;
    let box_total_w = box_w + 2;
    let box_total_h = box_h + 2;

    let is_horizontal = matches!(
        fc.direction,
        super::parser::Direction::LR | super::parser::Direction::RL
    );

    let layers = assign_layers(fc);
    let layers = reduce_crossings(fc, &layers);

    const LAYER_GAP: usize = 4;
    const NODE_GAP: usize = 2;

    let mut nodes = Vec::new();

    if is_horizontal {
        let max_layer_height = layers.iter().map(|l| l.len()).max().unwrap_or(1);
        let total_w = layers.len() * (box_total_w + LAYER_GAP) + NODE_GAP;
        let total_h = max_layer_height * (box_total_h + NODE_GAP);

        for (layer_idx, layer) in layers.iter().enumerate() {
            for (pos_in_layer, node_id) in layer.iter().enumerate() {
                let node_idx = fc.node_map.get(node_id).copied().unwrap_or(0);
                let node = &fc.nodes[node_idx];
                let x = layer_idx * (box_total_w + LAYER_GAP) + 1;
                let y = pos_in_layer * (box_total_h + NODE_GAP) + 1;
                nodes.push(LaidOutNode {
                    id: node_id.clone(),
                    label: node.label.clone(),
                    shape: node.shape.clone(),
                    x,
                    y,
                    w: box_w,
                    h: box_h,
                });
            }
        }
        build_layout_result(fc, nodes, total_w, total_h, true)
    } else {
        let max_layer_width = layers.iter().map(|l| l.len()).max().unwrap_or(1);
        let total_w = max_layer_width * (box_total_w + NODE_GAP) + NODE_GAP;
        let total_h = layers.len() * (box_total_h + LAYER_GAP);

        for (layer_idx, layer) in layers.iter().enumerate() {
            for (pos_in_layer, node_id) in layer.iter().enumerate() {
                let node_idx = fc.node_map.get(node_id).copied().unwrap_or(0);
                let node = &fc.nodes[node_idx];
                let x = pos_in_layer * (box_total_w + NODE_GAP) + 1;
                let y = layer_idx * (box_total_h + LAYER_GAP) + 1;
                nodes.push(LaidOutNode {
                    id: node_id.clone(),
                    label: node.label.clone(),
                    shape: node.shape.clone(),
                    x,
                    y,
                    w: box_w,
                    h: box_h,
                });
            }
        }
        build_layout_result(fc, nodes, total_w, total_h, false)
    }
}

fn build_layout_result(
    fc: &super::parser::Flowchart,
    nodes: Vec<LaidOutNode>,
    grid_w: usize,
    grid_h: usize,
    is_horizontal: bool,
) -> LayoutResult {
    let node_by_id: HashMap<&str, &LaidOutNode> =
        nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    let mut edges = Vec::new();
    for edge in &fc.edges {
        let (Some(from), Some(to)) = (
            node_by_id.get(edge.from.as_str()),
            node_by_id.get(edge.to.as_str()),
        ) else {
            continue;
        };

        let (path, label_pos) = if is_horizontal {
            route_horizontal(from, to, &nodes)
        } else {
            route_vertical(from, to, &nodes)
        };

        edges.push(LaidOutEdge {
            from: edge.from.clone(),
            to: edge.to.clone(),
            style: edge.style.clone(),
            label: edge.label.clone(),
            label_pos,
            path,
        });
    }

    let (grid_w, grid_h) = grow_grid_for_channels(&edges, grid_w, grid_h);

    LayoutResult {
        nodes,
        edges,
        grid_width: grid_w,
        grid_height: grid_h,
    }
}

fn grow_grid_for_channels(edges: &[LaidOutEdge], grid_w: usize, grid_h: usize) -> (usize, usize) {
    let mut w = grid_w;
    let h = grid_h;
    for e in edges {
        if let EdgePath::SideChannel { channel_x, .. } = &e.path {
            if *channel_x >= w {
                w = *channel_x + 2;
            }
        }
    }
    (w, h)
}

fn route_vertical(
    from: &LaidOutNode,
    to: &LaidOutNode,
    all_nodes: &[LaidOutNode],
) -> (EdgePath, Option<(usize, usize)>) {
    let from_cx = from.center_x();
    let to_cx = to.center_x();

    if from_cx == to_cx {
        let mid_y = (from.bottom() + to.top()) / 2;
        return (
            EdgePath::Direct {
                from_y: from.bottom(),
                to_y: to.top(),
                x: from_cx,
            },
            Some((from_cx + 1, mid_y)),
        );
    }

    let from_y = from.bottom();
    let to_y = to.top();
    let (lo, hi) = if from_y <= to_y {
        (from_y, to_y)
    } else {
        (to_y, from_y)
    };

    let is_clear = |y: usize| {
        !all_nodes.iter().any(|n| {
            n.id != from.id
                && n.id != to.id
                && n.top() <= y
                && y <= n.bottom()
                && horizontal_segments_overlap(from_cx, to_cx, n.left(), n.right())
        })
    };

    let mid_y = (from_y + to_y) / 2;
    let mut horizontal_y = mid_y;
    if !is_clear(mid_y) {
        let max_offset = (hi - lo).max(1);
        for offset in 1..=max_offset {
            let above = mid_y.saturating_sub(offset);
            let below = mid_y + offset;
            if above > lo && is_clear(above) {
                horizontal_y = above;
                break;
            }
            if below < hi && is_clear(below) {
                horizontal_y = below;
                break;
            }
        }
    }

    let mid_blocked = !is_clear(horizontal_y);

    if mid_blocked {
        let channel_x = side_channel_x(all_nodes, from, to);
        return (
            EdgePath::SideChannel {
                from_x: from_cx,
                from_y,
                to_x: to_cx,
                to_y,
                channel_x,
            },
            None,
        );
    }

    let label_pos = label_at_horizontal_bend(from_cx, to_cx, horizontal_y);

    (
        EdgePath::Corner {
            from_x: from_cx,
            from_y,
            to_x: to_cx,
            to_y,
            horizontal_y,
        },
        label_pos,
    )
}

fn route_horizontal(
    from: &LaidOutNode,
    to: &LaidOutNode,
    _all_nodes: &[LaidOutNode],
) -> (EdgePath, Option<(usize, usize)>) {
    let from_x = from.right();
    let to_x = to.left();
    let from_cy = from.center_y();
    let to_cy = to.center_y();

    if from_cy == to_cy {
        let mid_x = (from_x + to_x) / 2;
        return (
            EdgePath::Direct {
                from_y: from_cy,
                to_y: to_cy,
                x: from_x,
            },
            Some((mid_x, from_cy + 1)),
        );
    }

    let vertical_x = (from_x + to_x) / 2;
    let label_pos = label_at_vertical_bend(from_cy, to_cy, vertical_x);

    (
        EdgePath::Corner {
            from_x,
            from_y: from_cy,
            to_x,
            to_y: to_cy,
            horizontal_y: vertical_x,
        },
        label_pos,
    )
}

fn horizontal_segments_overlap(ax1: usize, ax2: usize, bx1: usize, bx2: usize) -> bool {
    let (a_lo, a_hi) = if ax1 <= ax2 { (ax1, ax2) } else { (ax2, ax1) };
    let (b_lo, b_hi) = if bx1 <= bx2 { (bx1, bx2) } else { (bx2, bx1) };
    a_lo <= b_hi && b_lo <= a_hi
}

fn side_channel_x(all_nodes: &[LaidOutNode], from: &LaidOutNode, to: &LaidOutNode) -> usize {
    let min_x = all_nodes.iter().map(|n| n.left()).min().unwrap_or(0);
    let max_x = all_nodes.iter().map(|n| n.right()).max().unwrap_or(0);
    let mid_x = (from.center_x() + to.center_x()) / 2;
    let left_channel = min_x.saturating_sub(2).max(1);
    let right_channel = max_x + 2;
    if mid_x.saturating_sub(left_channel) <= right_channel.saturating_sub(mid_x) {
        left_channel
    } else {
        right_channel
    }
}

fn label_at_horizontal_bend(from_x: usize, to_x: usize, y: usize) -> Option<(usize, usize)> {
    let mid_x = (from_x + to_x) / 2;
    Some((mid_x, y.saturating_sub(1)))
}

fn label_at_vertical_bend(from_y: usize, to_y: usize, x: usize) -> Option<(usize, usize)> {
    let mid_y = (from_y + to_y) / 2;
    Some((x + 1, mid_y))
}

fn assign_layers(fc: &super::parser::Flowchart) -> Vec<Vec<String>> {
    use std::collections::HashSet;

    let mut preds: HashMap<&str, Vec<&str>> = HashMap::new();
    for edge in &fc.edges {
        preds
            .entry(edge.to.as_str())
            .or_default()
            .push(edge.from.as_str());
    }

    let mut node_layer: HashMap<String, usize> = HashMap::new();
    for node in &fc.nodes {
        node_layer.insert(node.id.clone(), 0);
    }

    let mut visited: HashSet<String> = HashSet::new();
    let mut on_stack: HashSet<String> = HashSet::new();

    fn dfs(
        node_id: &str,
        preds: &HashMap<&str, Vec<&str>>,
        node_layer: &mut HashMap<String, usize>,
        visited: &mut HashSet<String>,
        on_stack: &mut HashSet<String>,
    ) {
        if visited.contains(node_id) {
            return;
        }
        visited.insert(node_id.to_string());
        on_stack.insert(node_id.to_string());

        if let Some(predecessors) = preds.get(node_id) {
            for &prev in predecessors {
                if on_stack.contains(prev) {
                    continue;
                }
                dfs(prev, preds, node_layer, visited, on_stack);
                let prev_layer = *node_layer.get(prev).unwrap_or(&0);
                let cur = *node_layer.get(node_id).unwrap_or(&0);
                if prev_layer + 1 > cur {
                    node_layer.insert(node_id.to_string(), prev_layer + 1);
                }
            }
        }

        on_stack.remove(node_id);
    }

    for node in &fc.nodes {
        dfs(
            &node.id,
            &preds,
            &mut node_layer,
            &mut visited,
            &mut on_stack,
        );
    }

    let max_layer = *node_layer.values().max().unwrap_or(&0);
    let mut layers = vec![Vec::new(); max_layer + 1];
    for node in &fc.nodes {
        let layer = *node_layer.get(&node.id).unwrap_or(&0);
        layers[layer].push(node.id.clone());
    }
    layers
}

fn reduce_crossings(fc: &super::parser::Flowchart, layers: &[Vec<String>]) -> Vec<Vec<String>> {
    if layers.len() < 2 {
        return layers.to_vec();
    }

    let mut result: Vec<Vec<String>> = layers.to_vec();

    for i in 1..result.len() {
        let prev = &result[i - 1];
        let cur = &result[i];
        let mut barycenters: Vec<(String, f64)> = cur
            .iter()
            .map(|node_id| {
                let mut sum = 0.0;
                let mut count = 0.0;
                for (idx, prev_id) in prev.iter().enumerate() {
                    for edge in &fc.edges {
                        if edge.to == *node_id && edge.from == *prev_id {
                            sum += idx as f64;
                            count += 1.0;
                        }
                        if edge.from == *node_id && edge.to == *prev_id {
                            sum += idx as f64;
                            count += 1.0;
                        }
                    }
                }
                let bary = if count > 0.0 { sum / count } else { 0.0 };
                (node_id.clone(), bary)
            })
            .collect();

        barycenters.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
        result[i] = barycenters.into_iter().map(|(id, _)| id).collect();
    }

    result
}
