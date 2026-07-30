use std::collections::HashMap;

// ── gitGraph ──

pub struct Commit {
    pub id: String,
    pub message: String,
    pub branch: String,
    pub is_merge: bool,
}

pub struct GitGraph {
    pub branches: Vec<String>,
    pub commits: Vec<Commit>,
}

pub fn parse_git_graph(source: &str) -> GitGraph {
    let mut gg = GitGraph {
        branches: Vec::new(),
        commits: Vec::new(),
    };
    let mut current_branch = "main".to_string();
    let mut commit_counter = 0u32;

    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line == "gitGraph" {
            continue;
        }
        if let Some(rest) = line.strip_prefix("branch ") {
            let name = rest.trim().to_string();
            if !gg.branches.contains(&name) {
                gg.branches.push(name.clone());
            }
            current_branch = name;
            continue;
        }
        if line == "end" {
            current_branch = "main".to_string();
            continue;
        }
        if line.starts_with("commit") {
            let id = format!("c{}", commit_counter);
            commit_counter += 1;
            let msg = line
                .strip_prefix("commit")
                .unwrap_or("")
                .trim()
                .trim_start_matches("id:")
                .trim()
                .to_string();
            let is_merge = line.contains("type: REVERSE");
            gg.commits.push(Commit {
                id,
                message: msg,
                branch: current_branch.clone(),
                is_merge,
            });
        }
        if line.starts_with("merge ") {
            let target = line.strip_prefix("merge ").unwrap_or("").trim().to_string();
            let id = format!("c{}", commit_counter);
            commit_counter += 1;
            gg.commits.push(Commit {
                id,
                message: format!("merge {target}"),
                branch: current_branch.clone(),
                is_merge: true,
            });
        }
    }
    if gg.branches.is_empty() {
        gg.branches.push("main".to_string());
    }
    gg
}

pub fn render_git_graph(gg: &GitGraph) -> Vec<String> {
    let mut lines = Vec::new();
    let _branch_count = gg.branches.len();
    let col_width = 4usize;

    for (bi, branch) in gg.branches.iter().enumerate() {
        let prefix = " ".repeat(bi * col_width);
        lines.push(format!("{prefix}●─ {branch}"));
    }
    lines.push(String::new());

    let mut branch_commits: HashMap<String, Vec<&Commit>> = HashMap::new();
    for c in &gg.commits {
        branch_commits.entry(c.branch.clone()).or_default().push(c);
    }

    for commit in &gg.commits {
        let bi = gg
            .branches
            .iter()
            .position(|b| b == &commit.branch)
            .unwrap_or(0);
        let prefix = " ".repeat(bi * col_width);
        let glyph = if commit.is_merge { "◇" } else { "●" };
        lines.push(format!("{prefix}{glyph} {}", commit.message));
    }
    lines
}

// ── journey ──

pub struct JourneyTask {
    pub label: String,
    pub score: u8,
}

pub struct JourneySection {
    pub label: String,
    pub tasks: Vec<JourneyTask>,
}

pub struct Journey {
    pub title: Option<String>,
    pub sections: Vec<JourneySection>,
}

pub fn parse_journey(source: &str) -> Journey {
    let mut j = Journey {
        title: None,
        sections: Vec::new(),
    };
    let mut current: Option<JourneySection> = None;

    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line == "journey" {
            continue;
        }
        if let Some(rest) = line.strip_prefix("title ") {
            j.title = Some(rest.trim().to_string());
            continue;
        }
        if line.starts_with("section ") {
            if let Some(s) = current.take() {
                j.sections.push(s);
            }
            current = Some(JourneySection {
                label: line
                    .strip_prefix("section ")
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                tasks: Vec::new(),
            });
            continue;
        }
        if let Some(pos) = line.rfind(':') {
            let label = line[..pos].trim().to_string();
            let rest = line[pos + 1..].trim();
            let score: u8 = rest.parse().unwrap_or(0);
            current
                .get_or_insert(JourneySection {
                    label: String::new(),
                    tasks: Vec::new(),
                })
                .tasks
                .push(JourneyTask { label, score });
        }
    }
    if let Some(s) = current {
        j.sections.push(s);
    }
    j
}

pub fn render_journey(j: &Journey) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(title) = &j.title {
        lines.push(format!("  {title}"));
        lines.push(String::new());
    }
    for section in &j.sections {
        if !section.label.is_empty() {
            lines.push(format!("  ▸ {}", section.label));
        }
        for task in &section.tasks {
            let bar_len = (task.score as usize).min(5);
            let bar = "★".repeat(bar_len) + &"☆".repeat(5 - bar_len);
            lines.push(format!("    {bar}  {}", task.label));
        }
        lines.push(String::new());
    }
    lines
}

// ── quadrantChart ──

pub struct QuadrantPoint {
    pub label: String,
    pub x: f64,
    pub y: f64,
}

pub struct QuadrantChart {
    pub title: Option<String>,
    pub points: Vec<QuadrantPoint>,
}

pub fn parse_quadrant(source: &str) -> QuadrantChart {
    let mut qc = QuadrantChart {
        title: None,
        points: Vec::new(),
    };
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line == "quadrantChart" {
            continue;
        }
        if let Some(rest) = line.strip_prefix("title ") {
            qc.title = Some(rest.trim().to_string());
            continue;
        }
        if line.starts_with("x-axis") || line.starts_with("y-axis") || line.starts_with("quadrant-")
        {
            continue;
        }
        if let Some(pos) = line.find(':') {
            let label = line[..pos]
                .trim()
                .trim_matches('[')
                .trim_matches(']')
                .to_string();
            let rest = line[pos + 1..].trim();
            let coords: Vec<&str> = rest.split(',').collect();
            if coords.len() >= 2 {
                let x: f64 = coords[0].trim().parse().unwrap_or(0.0);
                let y: f64 = coords[1].trim().parse().unwrap_or(0.0);
                qc.points.push(QuadrantPoint { label, x, y });
            }
        }
    }
    qc
}

pub fn render_quadrant(qc: &QuadrantChart) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(title) = &qc.title {
        lines.push(format!("  {title}"));
        lines.push(String::new());
    }
    lines.push("  ┌─────────┬─────────┐".to_string());
    lines.push("  │  High   │  High   │".to_string());
    lines.push("  │ Impact  │ Impact  │".to_string());
    lines.push("  ├─────────┼─────────┤".to_string());
    lines.push("  │  Low    │  Low    │".to_string());
    lines.push("  │ Impact  │ Impact  │".to_string());
    lines.push("  └─────────┴─────────┘".to_string());
    if !qc.points.is_empty() {
        lines.push(String::new());
        for p in &qc.points {
            let quadrant = if p.y >= 0.5 {
                if p.x >= 0.5 { "Q1" } else { "Q2" }
            } else if p.x >= 0.5 {
                "Q4"
            } else {
                "Q3"
            };
            lines.push(format!(
                "  {quadrant}: {} ({:.1}, {:.1})",
                p.label, p.x, p.y
            ));
        }
    }
    lines
}

// ── xychart ──

pub struct XyPoint {
    pub x: f64,
    pub y: f64,
}

pub struct XyChart {
    pub title: Option<String>,
    pub points: Vec<XyPoint>,
}

pub fn parse_xychart(source: &str) -> XyChart {
    let mut chart = XyChart {
        title: None,
        points: Vec::new(),
    };
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line.starts_with("xychart") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("title ") {
            chart.title = Some(rest.trim().to_string());
            continue;
        }
        if line.starts_with("x-axis") || line.starts_with("y-axis") {
            continue;
        }
        if line.starts_with("bar") || line.starts_with("line") {
            if let Some(rest) = line
                .strip_prefix("bar")
                .or_else(|| line.strip_prefix("line"))
            {
                let nums: Vec<f64> = rest
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .filter_map(|s| s.trim().parse().ok())
                    .collect();
                for (i, y) in nums.iter().enumerate() {
                    chart.points.push(XyPoint {
                        x: i as f64 + 1.0,
                        y: *y,
                    });
                }
            }
        }
    }
    chart
}

pub fn render_xychart(chart: &XyChart) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(title) = &chart.title {
        lines.push(format!("  {title}"));
        lines.push(String::new());
    }
    if chart.points.is_empty() {
        return lines;
    }
    let max_y = chart
        .points
        .iter()
        .map(|p| p.y)
        .fold(0.0f64, f64::max)
        .max(1.0);
    let bar_max = 15usize;
    for p in &chart.points {
        let bar_len = (p.y / max_y * bar_max as f64).round() as usize;
        let bar = "█".repeat(bar_len);
        lines.push(format!("  {:>3.0} │ {}", p.x, bar));
    }
    lines.push(format!("     └{}", "─".repeat(bar_max + 2)));
    lines
}

// ── block ──

pub struct Block {
    pub id: String,
    pub label: String,
}

pub struct BlockDiagram {
    pub blocks: Vec<Block>,
}

pub fn parse_block(source: &str) -> BlockDiagram {
    let mut bd = BlockDiagram { blocks: Vec::new() };
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line.starts_with("block") {
            continue;
        }
        if let Some(pos) = line.find(':') {
            let id = line[..pos].trim().to_string();
            let label = line[pos + 1..].trim().trim_matches('"').to_string();
            bd.blocks.push(Block { id, label });
        } else if !line.contains("--") {
            let label = line.trim_matches('"').to_string();
            if !label.is_empty() {
                bd.blocks.push(Block {
                    id: label.clone(),
                    label,
                });
            }
        }
    }
    bd
}

pub fn render_block(bd: &BlockDiagram) -> Vec<String> {
    let mut lines = Vec::new();
    for block in &bd.blocks {
        let w = crate::width::width(block.label.as_str()) + 2;
        lines.push(format!("  ┌─{}─┐", "─".repeat(w)));
        lines.push(format!("  │ {} │", block.label));
        lines.push(format!("  └─{}─┘", "─".repeat(w)));
    }
    lines
}

// ── packet ──

pub struct PacketField {
    pub name: String,
    pub bits: u32,
}

pub struct Packet {
    pub fields: Vec<PacketField>,
}

pub fn parse_packet(source: &str) -> Packet {
    let mut pkt = Packet { fields: Vec::new() };
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line.starts_with("packet") {
            continue;
        }
        if let Some(pos) = line.find(':') {
            let name = line[..pos].trim().to_string();
            let bits: u32 = line[pos + 1..].trim().parse().unwrap_or(0);
            pkt.fields.push(PacketField { name, bits });
        }
    }
    pkt
}

pub fn render_packet(pkt: &Packet) -> Vec<String> {
    let mut lines = Vec::new();
    if pkt.fields.is_empty() {
        return lines;
    }
    let total_bits: u32 = pkt.fields.iter().map(|f| f.bits).sum();
    let total_bytes = total_bits.div_ceil(8);
    lines.push(format!("  Packet: {total_bits} bits ({total_bytes} bytes)"));
    lines.push(String::new());
    for field in &pkt.fields {
        let name_w = crate::width::width(field.name.as_str());
        lines.push(format!("  ┌─{}─┐", "─".repeat(name_w + 2)));
        lines.push(format!("  │ {} │ {} bits", field.name, field.bits));
        lines.push(format!("  └─{}─┘", "─".repeat(name_w + 2)));
    }
    lines
}

// ── sankey ──

pub struct SankeyFlow {
    pub from: String,
    pub to: String,
    pub value: f64,
}

pub struct Sankey {
    pub flows: Vec<SankeyFlow>,
}

pub fn parse_sankey(source: &str) -> Sankey {
    let mut sk = Sankey { flows: Vec::new() };
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line.starts_with("sankey") {
            continue;
        }
        if let Some(pos) = line.find('[') {
            let parts: Vec<&str> = line[..pos].trim().split(',').collect();
            if parts.len() >= 2 {
                let from = parts[0].trim().to_string();
                let to = parts[1].trim().to_string();
                let value: f64 = line[pos..]
                    .trim_matches('[')
                    .trim_matches(']')
                    .parse()
                    .unwrap_or(0.0);
                sk.flows.push(SankeyFlow { from, to, value });
            }
        }
    }
    sk
}

pub fn render_sankey(sk: &Sankey) -> Vec<String> {
    let mut lines = Vec::new();
    let max_val = sk
        .flows
        .iter()
        .map(|f| f.value)
        .fold(0.0f64, f64::max)
        .max(1.0);
    let bar_max = 20usize;
    for flow in &sk.flows {
        let bar_len = (flow.value / max_val * bar_max as f64).round() as usize;
        let bar = "█".repeat(bar_len);
        lines.push(format!("  {} ──{}──→ {}", flow.from, bar, flow.to));
    }
    lines
}

// ── requirement ──

pub struct Requirement {
    pub id: String,
    pub label: String,
    pub kind: String,
}

pub struct RequirementDiagram {
    pub requirements: Vec<Requirement>,
}

pub fn parse_requirement(source: &str) -> RequirementDiagram {
    let mut rd = RequirementDiagram {
        requirements: Vec::new(),
    };
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line == "requirementDiagram" {
            continue;
        }
        if let Some(rest) = line.strip_prefix("requirement ") {
            let parts: Vec<&str> = rest.splitn(2, ':').collect();
            let id = parts[0].trim().to_string();
            let label = if parts.len() > 1 {
                parts[1].trim().to_string()
            } else {
                id.clone()
            };
            rd.requirements.push(Requirement {
                id,
                label,
                kind: "requirement".to_string(),
            });
        }
    }
    rd
}

pub fn render_requirement(rd: &RequirementDiagram) -> Vec<String> {
    let mut lines = Vec::new();
    for req in &rd.requirements {
        let label_w = crate::width::width(req.label.as_str());
        lines.push(format!("  ┌─{}─┐", "─".repeat(label_w + 2)));
        lines.push(format!("  │ {} │", req.label));
        lines.push(format!("  │ {} │", " ".repeat(label_w + 2)));
        lines.push(format!("  └─{}─┘ [{}]", "─".repeat(label_w + 2), req.id));
        lines.push(String::new());
    }
    lines
}

// ── architecture ──

pub struct ArchService {
    pub id: String,
    pub label: String,
}

pub struct Architecture {
    pub services: Vec<ArchService>,
}

pub fn parse_architecture(source: &str) -> Architecture {
    let mut arch = Architecture {
        services: Vec::new(),
    };
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line.starts_with("architecture") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("service ") {
            let parts: Vec<&str> = rest.splitn(2, '(').collect();
            let id = parts[0].trim().to_string();
            let label = if parts.len() > 1 {
                parts[1].trim_end_matches(')').trim().to_string()
            } else {
                id.clone()
            };
            arch.services.push(ArchService { id, label });
        }
    }
    arch
}

pub fn render_architecture(arch: &Architecture) -> Vec<String> {
    let mut lines = Vec::new();
    for svc in &arch.services {
        let w = crate::width::width(svc.label.as_str()) + 2;
        lines.push(format!("  ╭─{}─╮", "─".repeat(w)));
        lines.push(format!("  │ {} │", svc.label));
        lines.push(format!("  ╰─{}─╯", "─".repeat(w)));
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_git_graph_basic() {
        let gg = parse_git_graph("gitGraph\ncommit\ncommit");
        assert_eq!(gg.commits.len(), 2);
    }

    #[test]
    fn parse_journey_basic() {
        let j = parse_journey("journey\ntitle T\nsection S\nTask1: 5");
        assert_eq!(j.title.as_deref(), Some("T"));
        assert_eq!(j.sections[0].tasks[0].score, 5);
    }

    #[test]
    fn parse_sankey_basic() {
        let sk = parse_sankey("sankey-beta\nA, B [10]");
        assert_eq!(sk.flows.len(), 1);
        assert_eq!(sk.flows[0].from, "A");
        assert_eq!(sk.flows[0].value, 10.0);
    }
}
