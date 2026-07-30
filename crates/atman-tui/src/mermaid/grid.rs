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

    pub fn draw_stadium(&mut self, x: usize, y: usize, w: usize, h: usize, label: &str) {
        self.set(x, y, '(');
        self.set(x + w + 1, y, ')');
        self.set(x, y + h + 1, '(');
        self.set(x + w + 1, y + h + 1, ')');
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

    pub fn to_lines_offset(&self, offset: usize, visible_w: usize) -> Vec<String> {
        self.cells
            .iter()
            .map(|row| {
                let mut result = String::new();
                let end = (offset + visible_w).min(self.width);
                for c in &row[offset..end] {
                    if *c != '\u{0}' {
                        result.push(*c);
                    }
                }
                let dw = crate::width::width(&result);
                if dw < visible_w {
                    result.push_str(&" ".repeat(visible_w - dw));
                }
                result
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
    if existing == ' ' || existing == '\u{0}' {
        return new;
    }
    if new == ' ' || new == '\u{0}' {
        return existing;
    }
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
    if em == 0 && nm == 0 {
        return new;
    }
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

#[derive(Debug, Clone)]
pub enum EdgePath {
    Direct {
        from_y: usize,
        to_y: usize,
        x: usize,
    },
    SelfLoop {
        x: usize,
        y: usize,
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

/// Minimum visual gap (in display columns) between two labels on the same row.
const LABEL_GAP: usize = 3;

fn resolve_turn_vline_conflicts(edges: &mut [LaidOutEdge], nodes: &[LaidOutNode]) {
    let mut turn_cells: std::collections::HashSet<(usize, usize)> =
        std::collections::HashSet::new();
    for e in edges.iter() {
        if let EdgePath::Corner {
            from_x,
            to_x,
            horizontal_y,
            ..
        } = &e.path
        {
            turn_cells.insert((*from_x, *horizontal_y));
            turn_cells.insert((*to_x, *horizontal_y));
        }
    }

    for ei in 0..edges.len() {
        let path = edges[ei].path.clone();
        let EdgePath::Corner {
            from_x,
            from_y,
            to_x,
            to_y,
            horizontal_y: mut hy,
        } = path
        else {
            continue;
        };

        let v_conflict = vline_hits_any_turn(from_x, from_y, hy, &turn_cells, ei, edges)
            || vline_hits_any_turn(to_x, hy, to_y, &turn_cells, ei, edges);
        if !v_conflict {
            continue;
        }

        let (lo, hi) = if from_y <= to_y {
            (from_y, to_y)
        } else {
            (to_y, from_y)
        };
        let mut found = false;
        for offset in 1..=(hi - lo).max(1) {
            for &candidate in &[hy.saturating_sub(offset), hy + offset] {
                if candidate <= lo || candidate >= hi {
                    continue;
                }
                let h_blocked = nodes.iter().any(|n| {
                    n.top() <= candidate
                        && candidate <= n.bottom()
                        && horizontal_segments_overlap(from_x, to_x, n.left(), n.right())
                });
                if h_blocked {
                    continue;
                }
                let new_v_conflict =
                    vline_hits_any_turn(from_x, from_y, candidate, &turn_cells, ei, edges)
                        || vline_hits_any_turn(to_x, candidate, to_y, &turn_cells, ei, edges);
                if new_v_conflict {
                    continue;
                }
                hy = candidate;
                found = true;
                break;
            }
            if found {
                break;
            }
        }

        if found {
            turn_cells.remove(&(from_x, edges[ei].path_corner_hy()));
            turn_cells.remove(&(to_x, edges[ei].path_corner_hy()));
            turn_cells.insert((from_x, hy));
            turn_cells.insert((to_x, hy));
            let new_label = label_at_horizontal_bend(from_x, to_x, hy);
            edges[ei].path = EdgePath::Corner {
                from_x,
                from_y,
                to_x,
                to_y,
                horizontal_y: hy,
            };
            edges[ei].label_pos = new_label;
        }
    }
}

fn vline_hits_any_turn(
    x: usize,
    y1: usize,
    y2: usize,
    turn_cells: &std::collections::HashSet<(usize, usize)>,
    self_ei: usize,
    edges: &[LaidOutEdge],
) -> bool {
    let (lo, hi) = if y1 <= y2 { (y1, y2) } else { (y2, y1) };
    for y in lo..=hi {
        if !turn_cells.contains(&(x, y)) {
            continue;
        }
        if let EdgePath::Corner {
            from_x,
            to_x,
            horizontal_y,
            ..
        } = &edges[self_ei].path
        {
            if (*from_x == x || *to_x == x) && *horizontal_y == y {
                continue;
            }
        }
        return true;
    }
    false
}

trait CornerHy {
    fn path_corner_hy(&self) -> usize;
}

impl CornerHy for LaidOutEdge {
    fn path_corner_hy(&self) -> usize {
        match &self.path {
            EdgePath::Corner { horizontal_y, .. } => *horizontal_y,
            _ => 0,
        }
    }
}

fn resolve_label_positions(edges: &mut [LaidOutEdge], nodes: &[LaidOutNode], grid_w: usize) {
    let line_cells = compute_edge_line_cells(edges);

    let mut occupied: Vec<(usize, usize, usize)> = Vec::new();

    for edge in edges.iter_mut() {
        let (label_w, lx_orig, ly_orig) = match edge.label.as_ref().zip(edge.label_pos) {
            Some((label, pos)) => (crate::width::width(label), pos.0, pos.1),
            None => continue,
        };
        if label_w == 0 {
            continue;
        }

        let max_x = grid_w.saturating_sub(label_w);
        let lx_clamped = lx_orig.min(max_x);

        let max_y_delta = 4;
        let max_x_delta = 5;
        let mut placed = (lx_clamped, ly_orig);

        'outer: for y_delta in 0..=max_y_delta {
            for y in y_offsets(ly_orig, y_delta).into_iter() {
                for x_delta in 0..=max_x_delta {
                    for x in x_offsets(lx_clamped, x_delta, max_x).into_iter() {
                        if !collides_label(&occupied, x, y, label_w)
                            && !collides_node(nodes, x, y, label_w)
                            && !collides_edge_line(&line_cells, x, y, label_w)
                        {
                            placed = (x, y);
                            break 'outer;
                        }
                    }
                }
            }
        }

        occupied.push((placed.1, placed.0, placed.0 + label_w));
        edge.label_pos = Some(placed);
    }
}

/// Compute the set of (x, y) cells that will be occupied by edge lines.
fn compute_edge_line_cells(edges: &[LaidOutEdge]) -> std::collections::HashSet<(usize, usize)> {
    let mut cells = std::collections::HashSet::new();
    for edge in edges {
        match &edge.path {
            EdgePath::Direct { from_y, to_y, x } => {
                if from_y < to_y {
                    for y in (*from_y + 1)..(*to_y - 1) {
                        cells.insert((*x, y));
                    }
                    cells.insert((*x, *to_y - 1));
                } else {
                    for y in (*to_y + 2)..*from_y {
                        cells.insert((*x, y));
                    }
                    cells.insert((*x, *to_y + 1));
                }
            }
            EdgePath::SelfLoop { x, y } => {
                cells.insert((*x, *y));
                cells.insert((*x, *y + 1));
                cells.insert((*x, *y + 2));
                cells.insert((*x - 1, *y + 2));
                cells.insert((*x - 2, *y + 2));
                cells.insert((*x - 3, *y + 2));
            }
            EdgePath::Corner {
                from_x,
                from_y,
                to_x,
                to_y,
                horizontal_y,
            } => {
                let (sy, ey) = if *from_y < *horizontal_y {
                    (*from_y + 1, *horizontal_y)
                } else {
                    (*horizontal_y, *from_y)
                };
                for y in sy..ey {
                    cells.insert((*from_x, y));
                }
                let (x0, x1) = if to_x > from_x {
                    (*from_x + 1, *to_x)
                } else {
                    (*to_x + 1, *from_x)
                };
                for x in x0..x1 {
                    cells.insert((x, *horizontal_y));
                }
                let (sy2, ey2) = if *to_y < *horizontal_y {
                    (*to_y, *horizontal_y)
                } else {
                    (*horizontal_y + 1, *to_y)
                };
                for y in sy2..ey2 {
                    cells.insert((*to_x, y));
                }
            }
            EdgePath::SideChannel {
                from_x,
                from_y,
                to_x,
                to_y,
                channel_x,
            } => {
                let lo = (*from_x).min(*channel_x);
                let hi = (*from_x).max(*channel_x);
                for x in lo..=hi {
                    cells.insert((x, *from_y));
                }
                let lo2 = (*to_x).min(*channel_x);
                let hi2 = (*to_x).max(*channel_x);
                for x in lo2..=hi2 {
                    cells.insert((x, *to_y));
                }
                let (ylo, yhi) = if from_y < to_y {
                    (*from_y, *to_y)
                } else {
                    (*to_y, *from_y)
                };
                for y in ylo..=yhi {
                    cells.insert((*channel_x, y));
                }
            }
        }
    }
    cells
}

fn collides_edge_line(
    cells: &std::collections::HashSet<(usize, usize)>,
    x: usize,
    y: usize,
    lw: usize,
) -> bool {
    for dx in 0..lw {
        if cells.contains(&(x + dx, y)) {
            return true;
        }
    }
    false
}

fn y_offsets(center: usize, delta: usize) -> [usize; 2] {
    if delta == 0 {
        [center, center]
    } else {
        [center + delta, center.saturating_sub(delta)]
    }
}

fn x_offsets(center: usize, delta: usize, max_x: usize) -> [usize; 2] {
    if delta == 0 {
        [center.min(max_x), center.min(max_x)]
    } else {
        let hi = center + delta;
        let lo = center.saturating_sub(delta);
        [hi.min(max_x), lo]
    }
}

fn collides_label(occupied: &[(usize, usize, usize)], x: usize, y: usize, lw: usize) -> bool {
    let x_end = x + lw;
    occupied.iter().any(|&(oy, os, oe)| {
        oy == y && !(x_end + LABEL_GAP <= os || x.saturating_sub(LABEL_GAP) >= oe)
    })
}

fn collides_node(nodes: &[LaidOutNode], x: usize, y: usize, lw: usize) -> bool {
    let x_end = x + lw;
    nodes
        .iter()
        .any(|n| y >= n.top() && y <= n.bottom() && x_end > n.left() && x < n.right())
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
    let mut h_lane_used: HashMap<(usize, usize), std::collections::HashSet<usize>> = HashMap::new();
    for edge in &fc.edges {
        let (Some(from), Some(to)) = (
            node_by_id.get(edge.from.as_str()),
            node_by_id.get(edge.to.as_str()),
        ) else {
            continue;
        };

        let (path, label_pos) = if edge.from == edge.to {
            (
                EdgePath::SelfLoop {
                    x: from.right() + 1,
                    y: from.top(),
                },
                None,
            )
        } else if is_horizontal {
            route_horizontal(from, to, &nodes)
        } else {
            route_vertical_lane(from, to, &nodes, &mut h_lane_used)
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

    resolve_turn_vline_conflicts(&mut edges, &nodes);
    resolve_label_positions(&mut edges, &nodes, grid_w);

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
    let mut h = grid_h;
    for e in edges {
        match &e.path {
            EdgePath::SideChannel { channel_x, .. } => {
                if *channel_x >= w {
                    w = *channel_x + 2;
                }
            }
            EdgePath::SelfLoop { x, y } => {
                if *x + 1 >= w {
                    w = *x + 2;
                }
                if *y + 3 >= h {
                    h = *y + 4;
                }
            }
            _ => {}
        }
    }
    (w, h)
}

fn route_vertical_lane(
    from: &LaidOutNode,
    to: &LaidOutNode,
    all_nodes: &[LaidOutNode],
    h_lane_used: &mut HashMap<(usize, usize), std::collections::HashSet<usize>>,
) -> (EdgePath, Option<(usize, usize)>) {
    let (path, label_pos) = route_vertical(from, to, all_nodes);

    if let EdgePath::Corner {
        from_x,
        from_y,
        to_x,
        to_y,
        horizontal_y: mut hy,
    } = path.clone()
    {
        let gap_key = (from_y.min(to_y), from_y.max(to_y));
        let used = h_lane_used.entry(gap_key).or_default();

        if used.contains(&hy) {
            let (lo, hi) = gap_key;
            for offset in 1..=(hi - lo).max(1) {
                let above = hy.saturating_sub(offset);
                let below = hy + offset;
                if above > lo && !used.contains(&above) {
                    let clear = !all_nodes.iter().any(|n| {
                        n.id != from.id
                            && n.id != to.id
                            && n.top() <= above
                            && above <= n.bottom()
                            && horizontal_segments_overlap(from_x, to_x, n.left(), n.right())
                    });
                    if clear {
                        hy = above;
                        break;
                    }
                }
                if below < hi && !used.contains(&below) {
                    let clear = !all_nodes.iter().any(|n| {
                        n.id != from.id
                            && n.id != to.id
                            && n.top() <= below
                            && below <= n.bottom()
                            && horizontal_segments_overlap(from_x, to_x, n.left(), n.right())
                    });
                    if clear {
                        hy = below;
                        break;
                    }
                }
            }
            let new_path = EdgePath::Corner {
                from_x,
                from_y,
                to_x,
                to_y,
                horizontal_y: hy,
            };
            let new_label = label_at_horizontal_bend(from_x, to_x, hy);
            used.insert(hy);
            return (new_path, new_label);
        }
        used.insert(hy);
    }

    (path, label_pos)
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

fn label_at_vertical_bend(_from_y: usize, _to_y: usize, _x: usize) -> Option<(usize, usize)> {
    None
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
