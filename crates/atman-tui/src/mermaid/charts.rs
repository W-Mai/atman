pub struct PieSlice {
    pub label: String,
    pub value: f64,
}

pub struct PieChart {
    pub title: Option<String>,
    pub slices: Vec<PieSlice>,
}

pub fn parse_pie(source: &str) -> PieChart {
    let mut chart = PieChart {
        title: None,
        slices: Vec::new(),
    };
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line == "pie" {
            continue;
        }
        if let Some(rest) = line.strip_prefix("title ") {
            chart.title = Some(rest.trim().to_string());
            continue;
        }
        if let Some(pos) = line.find('"') {
            if let Some(end) = line[pos + 1..].find('"') {
                let label = line[pos + 1..pos + 1 + end].to_string();
                let rest = &line[pos + 1 + end + 1..];
                let value: f64 = rest
                    .trim()
                    .trim_start_matches(':')
                    .trim()
                    .parse()
                    .unwrap_or(0.0);
                chart.slices.push(PieSlice { label, value });
            }
        }
    }
    chart
}

pub fn render_pie(chart: &PieChart) -> Vec<String> {
    if chart.slices.is_empty() {
        return Vec::new();
    }
    let total: f64 = chart.slices.iter().map(|s| s.value).sum();
    if total <= 0.0 {
        return Vec::new();
    }

    let mut lines = Vec::new();
    if let Some(title) = &chart.title {
        lines.push(format!("  {title}"));
        lines.push(String::new());
    }

    let max_label = chart
        .slices
        .iter()
        .map(|s| crate::width::width(s.label.as_str()))
        .max()
        .unwrap_or(3)
        .max(3);
    let bar_max = 30usize;

    for slice in &chart.slices {
        let pct = slice.value / total * 100.0;
        let bar_len = (pct / 100.0 * bar_max as f64).round() as usize;
        let bar = "█".repeat(bar_len);
        let pad_bar = " ".repeat(bar_max - bar_len);
        let label_pad = max_label - crate::width::width(slice.label.as_str());
        let label_str = format!(
            "  {}{}  {}{} {:5.1}%",
            slice.label,
            " ".repeat(label_pad),
            bar,
            pad_bar,
            pct
        );
        lines.push(label_str);
    }
    lines
}

pub struct GanttTask {
    pub label: String,
    pub start: f64,
    pub duration: f64,
}

pub struct GanttSection {
    pub label: String,
    pub tasks: Vec<GanttTask>,
}

pub struct GanttChart {
    pub sections: Vec<GanttSection>,
}

pub fn parse_gantt(source: &str) -> GanttChart {
    let mut chart = GanttChart {
        sections: Vec::new(),
    };
    let mut current_section: Option<GanttSection> = None;

    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line == "gantt" {
            continue;
        }
        if line.starts_with("dateFormat") || line.starts_with("axisFormat") {
            continue;
        }
        if line.starts_with("section ") {
            if let Some(s) = current_section.take() {
                chart.sections.push(s);
            }
            current_section = Some(GanttSection {
                label: line
                    .strip_prefix("section ")
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                tasks: Vec::new(),
            });
            continue;
        }
        if let Some(pos) = line.find(':') {
            let label = line[..pos].trim().to_string();
            let rest = line[pos + 1..].trim();
            let parts: Vec<&str> = rest.split_whitespace().collect();
            if parts.len() >= 2 {
                let start: f64 = parts[0].parse().unwrap_or(0.0);
                let dur_str = parts[1].trim_start_matches(|c: char| !c.is_numeric());
                let duration: f64 = dur_str.parse().unwrap_or(0.0);
                let section = current_section.get_or_insert(GanttSection {
                    label: String::new(),
                    tasks: Vec::new(),
                });
                section.tasks.push(GanttTask {
                    label,
                    start,
                    duration,
                });
            }
        }
    }
    if let Some(s) = current_section {
        chart.sections.push(s);
    }
    chart
}

pub fn render_gantt(chart: &GanttChart) -> Vec<String> {
    let mut lines = Vec::new();
    let bar_max = 30usize;

    let max_end = chart
        .sections
        .iter()
        .flat_map(|s| s.tasks.iter())
        .map(|t| t.start + t.duration)
        .fold(0.0f64, f64::max)
        .max(1.0);

    for section in &chart.sections {
        if !section.label.is_empty() {
            lines.push(format!("  ▸ {}", section.label));
        }
        for task in &section.tasks {
            let start_col = (task.start / max_end * bar_max as f64).round() as usize;
            let bar_len = (task.duration / max_end * bar_max as f64).round() as usize;
            let bar_len = bar_len.max(1);
            let pre = " ".repeat(start_col);
            let bar = "█".repeat(bar_len);
            let post = " ".repeat(bar_max.saturating_sub(start_col + bar_len));
            lines.push(format!("    {}  {}{}{}", task.label, pre, bar, post));
        }
        lines.push(String::new());
    }
    lines
}

pub struct TimelineEntry {
    pub label: String,
    pub events: Vec<String>,
}

pub struct TimelineSection {
    pub label: String,
    pub entries: Vec<TimelineEntry>,
}

pub struct Timeline {
    pub title: Option<String>,
    pub sections: Vec<TimelineSection>,
}

pub fn parse_timeline(source: &str) -> Timeline {
    let mut tl = Timeline {
        title: None,
        sections: Vec::new(),
    };
    let mut current_section: Option<TimelineSection> = None;

    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line == "timeline" {
            continue;
        }
        if let Some(rest) = line.strip_prefix("title ") {
            tl.title = Some(rest.trim().to_string());
            continue;
        }
        if line.starts_with("section ") {
            if let Some(s) = current_section.take() {
                tl.sections.push(s);
            }
            current_section = Some(TimelineSection {
                label: line
                    .strip_prefix("section ")
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                entries: Vec::new(),
            });
            continue;
        }
        if let Some(pos) = line.find(':') {
            let label = line[..pos].trim().to_string();
            let events: Vec<String> = line[pos + 1..]
                .split(',')
                .map(|e| e.trim().to_string())
                .collect();
            let section = current_section.get_or_insert(TimelineSection {
                label: String::new(),
                entries: Vec::new(),
            });
            section.entries.push(TimelineEntry { label, events });
        }
    }
    if let Some(s) = current_section {
        tl.sections.push(s);
    }
    tl
}

pub fn render_timeline(tl: &Timeline) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(title) = &tl.title {
        lines.push(format!("  {title}"));
        lines.push(String::new());
    }
    for section in &tl.sections {
        if !section.label.is_empty() {
            lines.push(format!("  ▸ {}", section.label));
        }
        for entry in &section.entries {
            lines.push(format!(
                "    ● {} : {}",
                entry.label,
                entry.events.join(", ")
            ));
        }
        lines.push(String::new());
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_pie() {
        let chart = parse_pie("pie\n\"A\" : 30\n\"B\" : 70");
        assert_eq!(chart.slices.len(), 2);
        assert_eq!(chart.slices[0].label, "A");
        assert_eq!(chart.slices[0].value, 30.0);
    }

    #[test]
    fn parse_simple_gantt() {
        let chart = parse_gantt("gantt\nsection S1\nTask1 : 0, 5d");
        assert_eq!(chart.sections.len(), 1);
        assert_eq!(chart.sections[0].tasks.len(), 1);
    }

    #[test]
    fn parse_simple_timeline() {
        let tl = parse_timeline("timeline\ntitle T\nsection S\n2024 : Event A, Event B");
        assert_eq!(tl.title.as_deref(), Some("T"));
        assert_eq!(tl.sections[0].entries[0].events.len(), 2);
    }
}
