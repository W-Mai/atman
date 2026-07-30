use chrono::{Datelike, NaiveDate};

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
        if line.is_empty() || line.starts_with("%%") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("pie ") {
            if let Some(title) = rest.strip_prefix("title ") {
                chart.title = Some(title.trim().to_string());
            }
            continue;
        }
        if line == "pie" {
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
    pub id: Option<String>,
    pub label: String,
    pub start_day: i64,
    pub duration_days: i64,
    pub is_crit: bool,
    pub is_done: bool,
    pub is_active: bool,
    pub is_milestone: bool,
    pub dep_ids: Vec<String>,
}

pub struct GanttSection {
    pub label: String,
    pub tasks: Vec<GanttTask>,
}

pub struct GanttChart {
    pub title: Option<String>,
    pub sections: Vec<GanttSection>,
    pub start_date: Option<NaiveDate>,
}

fn parse_duration_days(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let split_pos = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num_str, unit) = s.split_at(split_pos);
    let num: f64 = num_str.parse().ok()?;
    let days = match unit {
        "d" | "day" | "days" => num,
        "w" | "week" | "weeks" => num * 7.0,
        "M" | "month" | "months" => num * 30.0,
        "h" | "hour" | "hours" => num / 24.0,
        "m" | "min" | "minute" | "minutes" => num / 1440.0,
        "" => num,
        _ => return None,
    };
    Some(days.round() as i64)
}

fn parse_date(s: &str, fmt: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if fmt.contains("YYYY-MM-DD") || fmt.is_empty() {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
    } else if fmt.contains("DD-MM-YYYY") {
        NaiveDate::parse_from_str(s, "%d-%m-%Y").ok()
    } else {
        NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()
    }
}

struct RawGanttTask {
    label: String,
    id: Option<String>,
    is_crit: bool,
    is_done: bool,
    is_active: bool,
    is_milestone: bool,
    deps: Vec<String>,
    start_date: Option<NaiveDate>,
    duration_days: Option<i64>,
    end_date: Option<NaiveDate>,
}

pub fn parse_gantt(source: &str) -> GanttChart {
    let mut chart = GanttChart {
        title: None,
        sections: Vec::new(),
        start_date: None,
    };
    let mut current_section: Option<GanttSection> = None;
    let mut date_format = String::new();
    let mut raw_tasks: Vec<RawGanttTask> = Vec::new();

    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("%%") || line == "gantt" {
            continue;
        }
        if let Some(rest) = line.strip_prefix("title ") {
            chart.title = Some(rest.trim().to_string());
            continue;
        }
        if let Some(rest) = line.strip_prefix("dateFormat ") {
            date_format = rest.trim().to_string();
            continue;
        }
        if line.starts_with("axisFormat")
            || line.starts_with("tickInterval")
            || line.starts_with("excludes")
            || line.starts_with("weekend")
            || line.starts_with("todayMarker")
        {
            continue;
        }
        if let Some(rest) = line.strip_prefix("section ") {
            if let Some(s) = current_section.take() {
                chart.sections.push(s);
            }
            current_section = Some(GanttSection {
                label: rest.trim().to_string(),
                tasks: Vec::new(),
            });
            continue;
        }
        if let Some(pos) = line.find(':') {
            let label = line[..pos].trim().to_string();
            let rest = line[pos + 1..].trim();
            let parts: Vec<&str> = rest.split(',').map(|p| p.trim()).collect();
            if parts.is_empty() || parts[0].is_empty() {
                continue;
            }
            let mut is_milestone = label == "milestone";
            let mut idx = 0;
            let mut is_crit = false;
            let mut is_done = false;
            let mut is_active = false;
            while idx < parts.len() {
                match parts[idx] {
                    "crit" => is_crit = true,
                    "done" => is_done = true,
                    "active" => is_active = true,
                    "milestone" => is_milestone = true,
                    _ => break,
                }
                idx += 1;
            }
            let mut id = None;
            let mut deps = Vec::new();
            let mut start_date = None;
            let mut duration_days = None;
            let mut end_date = None;
            let remaining: Vec<&str> = parts[idx..].to_vec();
            if !remaining.is_empty() {
                let first = remaining[0];
                let is_date = parse_date(first, &date_format).is_some();
                let is_dur = parse_duration_days(first).is_some();
                let is_after = first.starts_with("after ");
                if !is_date && !is_dur && !is_after {
                    id = Some(first.to_string());
                }
            }
            for part in remaining {
                if let Some(dep) = part.strip_prefix("after ") {
                    deps.push(dep.trim().to_string());
                } else if let Some(d) = parse_date(part, &date_format) {
                    if start_date.is_none() {
                        start_date = Some(d);
                    } else {
                        end_date = Some(d);
                    }
                } else if let Some(dur) = parse_duration_days(part) {
                    duration_days = Some(dur);
                }
            }
            raw_tasks.push(RawGanttTask {
                label,
                id,
                is_crit,
                is_done,
                is_active,
                is_milestone,
                deps,
                start_date,
                duration_days,
                end_date,
            });
        }
    }

    let mut min_date: Option<NaiveDate> = None;
    for rt in &raw_tasks {
        if let Some(sd) = rt.start_date {
            min_date = Some(min_date.map_or(sd, |m| m.min(sd)));
        }
    }
    chart.start_date = min_date;
    let chart_start = min_date.unwrap_or_else(|| NaiveDate::from_ymd_opt(2024, 1, 1).unwrap());

    let mut resolved: std::collections::HashMap<String, (i64, i64)> =
        std::collections::HashMap::new();
    for _ in 0..raw_tasks.len() + 5 {
        let mut changed = false;
        for (i, rt) in raw_tasks.iter().enumerate() {
            let id_key = rt.id.clone().unwrap_or_else(|| {
                if !rt.label.is_empty() {
                    rt.label.clone()
                } else {
                    format!("__task_{i}")
                }
            });
            if resolved.contains_key(&id_key) {
                continue;
            }
            let start = if let Some(sd) = rt.start_date {
                Some(sd)
            } else if !rt.deps.is_empty() {
                let mut dep_end: Option<NaiveDate> = None;
                let mut all_resolved = true;
                for dep in &rt.deps {
                    if let Some((_, end_day)) = resolved.get(dep) {
                        let dep_end_date = chart_start + chrono::Duration::days(*end_day);
                        dep_end = Some(dep_end.map_or(dep_end_date, |d| d.max(dep_end_date)));
                    } else {
                        all_resolved = false;
                        break;
                    }
                }
                if all_resolved { dep_end } else { None }
            } else {
                None
            };
            if start.is_none() {
                continue;
            }
            let start_date = start.unwrap();
            let start_day = (start_date - chart_start).num_days();
            let end_day = if let Some(ed) = rt.end_date {
                (ed - chart_start).num_days()
            } else if let Some(dur) = rt.duration_days {
                start_day + dur
            } else {
                start_day + 1
            };
            resolved.insert(id_key, (start_day, end_day));
            changed = true;
        }
        if !changed {
            break;
        }
    }

    for (i, rt) in raw_tasks.iter().enumerate() {
        let id_key = rt.id.clone().unwrap_or_else(|| {
            if !rt.label.is_empty() {
                rt.label.clone()
            } else {
                format!("__task_{i}")
            }
        });
        let (start_day, end_day) = resolved.get(&id_key).copied().unwrap_or((0, 1));
        let task = GanttTask {
            id: rt.id.clone(),
            label: rt.label.clone(),
            start_day,
            duration_days: if rt.is_milestone {
                0
            } else {
                (end_day - start_day).max(1)
            },
            is_crit: rt.is_crit,
            is_done: rt.is_done,
            is_active: rt.is_active,
            is_milestone: rt.is_milestone,
            dep_ids: rt.deps.clone(),
        };
        let section = current_section.get_or_insert(GanttSection {
            label: String::new(),
            tasks: Vec::new(),
        });
        section.tasks.push(task);
    }
    if let Some(s) = current_section {
        chart.sections.push(s);
    }
    chart
}

pub fn render_gantt(chart: &GanttChart) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(title) = &chart.title {
        lines.push(format!("  {title}"));
        lines.push(String::new());
    }

    let all_tasks: Vec<&GanttTask> = chart.sections.iter().flat_map(|s| s.tasks.iter()).collect();
    if all_tasks.is_empty() {
        return lines;
    }

    let max_end_day = all_tasks
        .iter()
        .map(|t| t.start_day + t.duration_days)
        .max()
        .unwrap_or(1)
        .max(1) as usize;

    let label_col_w = all_tasks
        .iter()
        .map(|t| crate::width::width(t.label.as_str()))
        .max()
        .unwrap_or(5)
        .max(8);
    let bar_max = 40usize;
    let chart_start = chart
        .start_date
        .unwrap_or_else(|| NaiveDate::from_ymd_opt(2024, 1, 1).unwrap());

    let tick_interval = if max_end_day <= 7 {
        1
    } else if max_end_day <= 14 {
        2
    } else if max_end_day <= 30 {
        5
    } else if max_end_day <= 60 {
        7
    } else {
        30
    };

    let mut axis_chars: Vec<char> = vec![' '; bar_max];
    let mut tick_cols: Vec<usize> = Vec::new();
    for tick in (0..=max_end_day).step_by(tick_interval) {
        let col = tick * bar_max / max_end_day;
        tick_cols.push(col);
    }
    for (i, &col) in tick_cols.iter().enumerate() {
        if col >= bar_max {
            continue;
        }
        let day = i * tick_interval;
        let date = chart_start + chrono::Duration::days(day as i64);
        let label = if tick_interval >= 30 {
            date.format("%m/%d").to_string()
        } else {
            date.format("%m-%d").to_string()
        };
        for (j, c) in label.chars().enumerate() {
            if col + j < bar_max {
                axis_chars[col + j] = c;
            }
        }
    }
    lines.push(format!(
        "{:width$}  {}",
        "",
        axis_chars.iter().collect::<String>(),
        width = label_col_w
    ));
    lines.push(format!(
        "{:width$}  {}",
        "",
        "─".repeat(bar_max),
        width = label_col_w
    ));

    let is_weekend = |day: i64| {
        let date = chart_start + chrono::Duration::days(day);
        date.weekday().num_days_from_monday() >= 5
    };

    for section in &chart.sections {
        if !section.label.is_empty() {
            let header = format!("▸ {}", section.label);
            let header_w = crate::width::width(header.as_str());
            let pad = (label_col_w + 2 + bar_max).saturating_sub(header_w);
            lines.push(format!("{}{}", header, " ".repeat(pad)));
        }
        for task in &section.tasks {
            let start_col = (task.start_day as usize) * bar_max / max_end_day;
            let mut bar_row: Vec<char> = vec![' '; bar_max];

            if task.is_milestone {
                if start_col < bar_max {
                    bar_row[start_col] = '◆';
                }
            } else {
                let end_col =
                    ((task.start_day + task.duration_days) as usize) * bar_max / max_end_day;
                let end_col = end_col.max(start_col + 1).min(bar_max);
                for (col, cell) in bar_row.iter_mut().enumerate() {
                    let day = (col * max_end_day / bar_max) as i64;
                    if is_weekend(day) && col >= start_col && col < end_col {
                        *cell = '░';
                    }
                }
                let bar_char = if task.is_done {
                    '█'
                } else if task.is_active {
                    '▓'
                } else if task.is_crit {
                    '▒'
                } else {
                    '█'
                };
                for cell in &mut bar_row[start_col..end_col] {
                    *cell = bar_char;
                }
            }

            let label_w = crate::width::width(task.label.as_str());
            let label_pad = label_col_w.saturating_sub(label_w);
            let mut row = String::new();
            row.push_str(&task.label);
            row.push_str(&" ".repeat(label_pad));
            row.push_str("  ");
            row.push_str(&bar_row.iter().collect::<String>());
            lines.push(row);

            for dep in &task.dep_ids {
                lines.push(format!(
                    "{:width$}  {}← {}",
                    "",
                    " ".repeat(start_col),
                    dep,
                    width = label_col_w
                ));
            }
        }
        lines.push(String::new());
    }

    let legend_text = "Legend: █ done  ▓ active  ▒ crit  ◆ milestone  ░ weekend";
    let legend_w = crate::width::width(legend_text);
    let legend = if legend_w > bar_max {
        crate::width::truncate_plain(legend_text, bar_max)
    } else {
        legend_text.to_string()
    };
    lines.push(format!("{:width$}  {}", "", legend, width = label_col_w));
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
    fn parse_pie_title_inline() {
        let chart = parse_pie("pie title 嘿嘿大师\n\"A\" : 30");
        assert_eq!(chart.title.as_deref(), Some("嘿嘿大师"));
        assert_eq!(chart.slices.len(), 1);
    }

    #[test]
    fn parse_pie_title_separate_line() {
        let chart = parse_pie("pie\ntitle 嘿嘿大师\n\"A\" : 30");
        assert_eq!(chart.title.as_deref(), Some("嘿嘿大师"));
    }

    #[test]
    fn parse_simple_gantt() {
        let chart = parse_gantt("gantt\nsection S1\nTask1 : 0, 5d");
        assert_eq!(chart.sections.len(), 1);
        assert_eq!(chart.sections[0].tasks.len(), 1);
    }

    #[test]
    fn parse_gantt_date_duration() {
        let chart =
            parse_gantt("gantt\ndateFormat YYYY-MM-DD\nsection S1\nTask1 :a1, 2024-01-01, 3d");
        assert_eq!(chart.sections[0].tasks.len(), 1);
        let t = &chart.sections[0].tasks[0];
        assert_eq!(t.id.as_deref(), Some("a1"));
        assert_eq!(t.start_day, 0);
        assert_eq!(t.duration_days, 3);
    }

    #[test]
    fn parse_gantt_after_dep() {
        let chart = parse_gantt(
            "gantt\ndateFormat YYYY-MM-DD\nsection S1\nTask1 :a1, 2024-01-01, 3d\nTask2 :after a1, 2d",
        );
        assert_eq!(chart.sections[0].tasks.len(), 2);
        let t2 = &chart.sections[0].tasks[1];
        assert_eq!(t2.start_day, 3);
        assert_eq!(t2.duration_days, 2);
    }

    #[test]
    fn parse_gantt_crit() {
        let chart = parse_gantt("gantt\nsection S1\nTask1 :crit, 2024-01-01, 1d");
        assert!(chart.sections[0].tasks[0].is_crit);
    }

    #[test]
    fn render_gantt_cjk_label() {
        let chart =
            parse_gantt("gantt\ndateFormat YYYY-MM-DD\nsection 阶段\n任务一 :a1, 2024-01-01, 3d");
        let lines = render_gantt(&chart);
        let all: String = lines.join("\n");
        assert!(all.contains("任务一"), "task label should appear:\n{all}");
        assert!(all.contains("阶段"), "section label should appear");
    }

    #[test]
    fn render_gantt_has_date_axis() {
        let chart = parse_gantt("gantt\ndateFormat YYYY-MM-DD\nsection S\nTask1 : 2024-01-01, 5d");
        let lines = render_gantt(&chart);
        let all: String = lines.join("\n");
        assert!(all.contains("01-01"), "date axis should show start date");
    }

    #[test]
    fn render_gantt_crit_bar() {
        let chart = parse_gantt("gantt\ndateFormat YYYY-MM-DD\nsection S\nT :crit, 2024-01-01, 2d");
        let lines = render_gantt(&chart);
        let all: String = lines.join("\n");
        assert!(all.contains('▒'), "crit task should use ▒");
    }

    #[test]
    fn parse_simple_timeline() {
        let tl = parse_timeline("timeline\ntitle T\nsection S\n2024 : Event A, Event B");
        assert_eq!(tl.title.as_deref(), Some("T"));
        assert_eq!(tl.sections[0].entries[0].events.len(), 2);
    }
}
