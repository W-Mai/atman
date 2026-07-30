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
        for (i, c) in s.chars().enumerate() {
            self.set(x + i, y, c);
        }
    }

    pub fn write_line(&mut self, x: usize, y: usize, s: &str) {
        self.write_str(x, y, s);
    }

    pub fn box_top(&mut self, x: usize, y: usize, w: usize) {
        self.set(x, y, '╭');
        self.set(x + w + 1, y, '╮');
        for i in 1..=w {
            self.set(x + i, y, '─');
        }
    }

    pub fn box_bottom(&mut self, x: usize, y: usize, w: usize) {
        self.set(x, y, '╰');
        self.set(x + w + 1, y, '╯');
        for i in 1..=w {
            self.set(x + i, y, '─');
        }
    }

    pub fn box_sides(&mut self, x: usize, y: usize, w: usize, h: usize) {
        for row in 0..h {
            self.set(x, y + row, '│');
            self.set(x + w + 1, y + row, '│');
        }
    }

    pub fn draw_box(&mut self, x: usize, y: usize, w: usize, h: usize, label: &str) {
        self.box_top(x, y, w);
        self.box_bottom(x, y + h + 1, w);
        self.box_sides(x, y + 1, w, h);
        let label_chars: Vec<char> = label.chars().collect();
        let max_label = w.saturating_sub(0);
        let truncated: String = label_chars.iter().take(max_label).collect();
        let label_w = truncated.chars().count();
        let pad = w.saturating_sub(label_w) / 2;
        self.write_str(x + 1 + pad, y + 1, &truncated);
    }

    pub fn draw_diamond(&mut self, x: usize, y: usize, w: usize, label: &str) {
        let cx = x + w / 2 + 1;
        self.set(cx, y, '◇');
        self.set(x, y + 1, '◇');
        self.set(x + w + 1, y + 1, '◇');
        self.set(cx, y + 2, '◇');
        let label_chars: Vec<char> = label.chars().collect();
        let truncated: String = label_chars.iter().take(w).collect();
        let lw = truncated.chars().count();
        let pad = w.saturating_sub(lw) / 2;
        self.write_str(x + 1 + pad, y + 1, &truncated);
    }

    pub fn draw_circle(&mut self, x: usize, y: usize, w: usize, label: &str) {
        self.set(x + w / 2 + 1, y, '○');
        self.set(x, y + 1, '(');
        self.set(x + w + 1, y + 1, ')');
        self.set(x + w / 2 + 1, y + 2, '○');
        let label_chars: Vec<char> = label.chars().collect();
        let truncated: String = label_chars.iter().take(w).collect();
        let lw = truncated.chars().count();
        let pad = w.saturating_sub(lw) / 2;
        self.write_str(x + 1 + pad, y + 1, &truncated);
    }

    pub fn draw_h_line(&mut self, x1: usize, x2: usize, y: usize, style: &str) {
        let c = match style {
            "dotted" => '┄',
            "thick" => '═',
            _ => '─',
        };
        for x in x1..x2 {
            if self.get(x, y) == ' ' {
                self.set(x, y, c);
            }
        }
    }

    pub fn draw_v_line(&mut self, x: usize, y1: usize, y2: usize, style: &str) {
        let c = match style {
            "dotted" => '┆',
            "thick" => '║',
            _ => '│',
        };
        for y in y1..y2 {
            if self.get(x, y) == ' ' {
                self.set(x, y, c);
            }
        }
    }

    pub fn to_lines(&self) -> Vec<String> {
        self.cells
            .iter()
            .map(|row| {
                let s: String = row.iter().collect();
                s.trim_end().to_string()
            })
            .collect()
    }
}

pub struct PositionedNode {
    pub id: String,
    pub label: String,
    pub shape: super::parser::NodeShape,
    pub x: usize,
    pub y: usize,
    pub w: usize,
    pub h: usize,
}

pub fn layout(fc: &super::parser::Flowchart) -> (Vec<PositionedNode>, usize, usize) {
    if fc.nodes.is_empty() {
        return (Vec::new(), 0, 0);
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

    let mut nodes = Vec::new();
    let gap = 4usize;

    if is_horizontal {
        let max_layer_height = layers.iter().map(|l| l.len()).max().unwrap_or(1);
        let total_w = layers.len() * (box_total_w + gap);
        let total_h = max_layer_height * (box_total_h + gap);

        for (layer_idx, layer) in layers.iter().enumerate() {
            for (pos_in_layer, node_id) in layer.iter().enumerate() {
                let node_idx = fc.node_map.get(node_id).copied().unwrap_or(0);
                let node = &fc.nodes[node_idx];
                let x = layer_idx * (box_total_w + gap);
                let y = pos_in_layer * (box_total_h + gap);
                nodes.push(PositionedNode {
                    id: node_id.clone(),
                    label: node.label.clone(),
                    shape: node.shape.clone(),
                    x: x + 1,
                    y: y + 1,
                    w: box_w,
                    h: box_h,
                });
            }
        }
        (nodes, total_w + 2, total_h + 2)
    } else {
        let max_layer_width = layers.iter().map(|l| l.len()).max().unwrap_or(1);
        let total_w = max_layer_width * (box_total_w + gap);
        let total_h = layers.len() * (box_total_h + gap);

        for (layer_idx, layer) in layers.iter().enumerate() {
            for (pos_in_layer, node_id) in layer.iter().enumerate() {
                let node_idx = fc.node_map.get(node_id).copied().unwrap_or(0);
                let node = &fc.nodes[node_idx];
                let x = pos_in_layer * (box_total_w + gap);
                let y = layer_idx * (box_total_h + gap);
                nodes.push(PositionedNode {
                    id: node_id.clone(),
                    label: node.label.clone(),
                    shape: node.shape.clone(),
                    x: x + 1,
                    y: y + 1,
                    w: box_w,
                    h: box_h,
                });
            }
        }
        (nodes, total_w + 2, total_h + 2)
    }
}

fn assign_layers(fc: &super::parser::Flowchart) -> Vec<Vec<String>> {
    let mut node_layer: HashMap<String, usize> = HashMap::new();
    let mut changed = true;

    for node in &fc.nodes {
        node_layer.insert(node.id.clone(), 0);
    }

    while changed {
        changed = false;
        for edge in &fc.edges {
            let from_layer = *node_layer.get(&edge.from).unwrap_or(&0);
            let to_layer = node_layer.get(&edge.to).copied();
            let new_layer = from_layer + 1;
            if new_layer > fc.nodes.len() {
                continue;
            }
            match to_layer {
                Some(tl) if tl < new_layer => {
                    node_layer.insert(edge.to.clone(), new_layer);
                    changed = true;
                }
                None => {
                    node_layer.insert(edge.to.clone(), new_layer);
                    changed = true;
                }
                _ => {}
            }
        }
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
