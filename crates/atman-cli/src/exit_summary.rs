//! Theme-aware terminal summary printed after workspace shutdown.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::Result;
use atman_runtime::{Session, flow_runtime::host::RuntimeHost};
use uuid::Uuid;

pub(super) struct Capture {
    summary: SessionSummary,
    contexts: Vec<Arc<Session>>,
}

impl Capture {
    pub(super) async fn from_host(host: &RuntimeHost, id: Uuid) -> Result<Option<Self>> {
        let Ok(session) = host.session(id) else {
            return Ok(None);
        };
        let catalog = session.snapshot().await?.catalog;
        let mut contexts = Vec::new();
        let mut title = catalog.title.clone();
        let mut registered: Vec<_> = catalog.flows.iter().collect();
        registered.sort_by_key(|row| Some(&row.id) != catalog.default_flow.as_ref());
        for row in registered {
            // Observe only the owners already loaded by this workspace.
            let Some(worker) = session.loaded_flow(&row.id) else {
                continue;
            };
            if contexts.is_empty() && title.is_none() {
                title = Some(worker.subscribe_name().borrow().clone());
            }
            contexts.push(worker.context());
        }
        let mut summary = SessionSummary::empty(id);
        summary.name = title.or(Some(catalog.descriptor.title));
        summary.project_root = catalog
            .project
            .map(|project| project.root().display().to_string());
        Ok(Some(Self { summary, contexts }))
    }

    fn finish(mut self, elapsed: Duration) -> SessionSummary {
        let mut files = std::collections::BTreeSet::new();
        for context in &self.contexts {
            let events = context.sink().snapshot_envelopes();
            let activity = atman_runtime::activity::summarize_events(&events);
            self.summary.activity.attempted_calls += activity.attempted_calls;
            self.summary.activity.completed_calls += activity.completed_calls;
            self.summary.activity.failed_calls += activity.failed_calls;
            self.summary.activity.applied_edits += activity.applied_edits;
            self.summary.activity.hunks += activity.hunks;
            self.summary.activity.insertions += activity.insertions;
            self.summary.activity.deletions += activity.deletions;
            let mut calls = Vec::new();
            for envelope in events {
                match envelope.event {
                    event @ atman_runtime::event::Event::LlmCall { .. } => calls.push(event),
                    atman_runtime::event::Event::FileEditApplied { path, .. } => {
                        files.insert(path);
                    }
                    _ => {}
                }
            }
            let cost = atman_runtime::cost::total(&calls);
            self.summary.cost.calls += cost.calls;
            self.summary.cost.failures += cost.failures;
            self.summary.cost.wallclock_ms += cost.wallclock_ms;
            self.summary.cost.usage.input += cost.usage.input;
            self.summary.cost.usage.output += cost.usage.output;
            self.summary.cost.usage.cached_input += cost.usage.cached_input;
            self.summary.cost.usage.cache_write += cost.usage.cache_write;
            self.summary.cost.usage.reasoning_tokens += cost.usage.reasoning_tokens;
        }
        self.summary.activity.files = files.len();
        if let Some(context) = self.contexts.first() {
            self.summary.msg_count = context.user_message_count();
            self.summary.goal = context.goal();
            self.summary.todos = context.subscribe_todos().borrow().clone();
            self.summary.plans = context.subscribe_plans().borrow().clone();
        }
        self.summary.elapsed = elapsed;
        self.summary
    }
}

pub(super) async fn show(
    root: &Path,
    id: Uuid,
    capture: Option<Capture>,
    elapsed: Duration,
) -> Result<()> {
    let root = root.to_owned();
    let printed = tokio::task::spawn_blocking(move || {
        let summary = match capture {
            Some(capture) => capture.finish(elapsed),
            None => historical_summary(&root, id, elapsed),
        };
        print_session_summary(&summary);
    })
    .await;
    let restored = atman_tui::terminal_guard::ensure_shell_mode();
    printed?;
    restored?;
    Ok(())
}

fn historical_summary(root: &Path, id: Uuid, elapsed: Duration) -> SessionSummary {
    let mut summary = SessionSummary::empty(id);
    let directory = root.join("sessions").join(id.to_string());
    if directory.is_dir() {
        let meta = atman_runtime::session_meta::SessionMeta::load(&directory).unwrap_or_default();
        summary.name = meta.title;
        summary.project_root = meta.project_root.map(|path| path.display().to_string());
        summary.goal = atman_runtime::memory::goal::GoalStore::at(&directory)
            .load()
            .ok()
            .flatten()
            .map(|goal| goal.objective);
        let stats = std::fs::read(directory.join("stats.json"))
            .ok()
            .and_then(|bytes| {
                serde_json::from_slice::<atman_runtime::session_meta::SessionStats>(&bytes).ok()
            });
        if let Some(stats) = stats.filter(|stats| {
            std::fs::metadata(directory.join("events.jsonl"))
                .is_ok_and(|metadata| metadata.len() == stats.event_bytes)
        }) {
            summary.msg_count = stats.user_message_count as usize;
        }
        let runtime = tokio::runtime::Handle::current();
        summary.todos = runtime
            .block_on(atman_runtime::memory::todo::TodoStore::at(&directory).list())
            .unwrap_or_default();
        summary.plans = runtime
            .block_on(atman_runtime::memory::plan::PlanStore::at(&directory).list())
            .unwrap_or_default();
    } else if let Ok(read) = atman_runtime::flow_store::RuntimeStore::inspect_session(root, id) {
        summary.name = read
            .projection
            .title
            .or(Some(read.projection.descriptor.title));
        summary.project_root = read
            .projection
            .project
            .map(|project| project.root().display().to_string());
    }
    summary.elapsed = elapsed;
    summary
}

struct SessionSummary {
    sid: String,
    name: Option<String>,
    project_root: Option<String>,
    msg_count: usize,
    goal: Option<String>,
    todos: Vec<atman_runtime::memory::todo::Todo>,
    plans: Vec<atman_runtime::memory::plan::Plan>,
    activity: atman_runtime::activity::ActivitySummary,
    cost: atman_runtime::cost::CostSummary,
    elapsed: std::time::Duration,
}

impl SessionSummary {
    fn empty(id: Uuid) -> Self {
        Self {
            sid: id.to_string(),
            name: None,
            project_root: None,
            msg_count: 0,
            goal: None,
            todos: Vec::new(),
            plans: Vec::new(),
            activity: Default::default(),
            cost: Default::default(),
            elapsed: Duration::ZERO,
        }
    }
}

fn safe_text(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect::<String>()
        .replace('\t', "  ")
}

fn print_session_summary(summary: &SessionSummary) {
    use std::io::{IsTerminal, Write};

    let is_tty = std::io::stdout().is_terminal();
    let color = is_tty
        && std::env::var_os("NO_COLOR").is_none()
        && std::env::var("TERM").is_ok_and(|term| term != "dumb");
    let terminal_width = if is_tty {
        crossterm::terminal::size()
            .map(|(columns, _)| usize::from(columns))
            .unwrap_or(92)
    } else {
        92
    };
    let layout = session_summary_layout(summary, terminal_width);
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();

    let _ = writeln!(stdout);
    for row in &layout.rows {
        let _ = write!(stdout, "{}", " ".repeat(layout.indent));
        if color {
            write_summary_row(&mut stdout, row, layout.width);
        } else {
            let _ = writeln!(stdout, "{}", row.plain(layout.width));
        }
    }
    let _ = writeln!(stdout);
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct SummaryColor(u8, u8, u8);

impl From<atman_tui::theme::ThemeColor> for SummaryColor {
    fn from(color: atman_tui::theme::ThemeColor) -> Self {
        let (r, g, b) = color.rgb();
        Self(r, g, b)
    }
}

#[derive(Clone)]
struct SummarySpan {
    text: String,
    fg: SummaryColor,
    bg: Option<SummaryColor>,
    bold: bool,
}

impl SummarySpan {
    fn new(text: impl Into<String>, fg: SummaryColor) -> Self {
        Self {
            text: text.into(),
            fg,
            bg: None,
            bold: false,
        }
    }

    fn bold(mut self) -> Self {
        self.bold = true;
        self
    }

    fn on(mut self, bg: SummaryColor) -> Self {
        self.bg = Some(bg);
        self
    }
}

struct SummaryRow {
    bg: SummaryColor,
    spans: Vec<SummarySpan>,
}

impl SummaryRow {
    fn blank(bg: SummaryColor) -> Self {
        Self {
            bg,
            spans: Vec::new(),
        }
    }

    fn plain(&self, width: usize) -> String {
        let mut text = self
            .spans
            .iter()
            .map(|span| span.text.as_str())
            .collect::<String>();
        let used = atman_tui::width::width(&text);
        text.push_str(&" ".repeat(width.saturating_sub(used)));
        text
    }
}

struct SessionSummaryLayout {
    width: usize,
    indent: usize,
    rows: Vec<SummaryRow>,
}

#[derive(Clone, Copy)]
struct SummaryPalette {
    outer_bg: SummaryColor,
    focus_bg: SummaryColor,
    primary: SummaryColor,
    title: SummaryColor,
    meta: SummaryColor,
    accent: SummaryColor,
    heading: SummaryColor,
    success: SummaryColor,
    add: SummaryColor,
    remove: SummaryColor,
}

impl SummaryPalette {
    fn current() -> Self {
        let theme = atman_tui::theme::theme();
        Self {
            outer_bg: theme.panel_bg.into(),
            focus_bg: theme.user_msg_bg.into(),
            primary: theme.work_action_fg.into(),
            title: theme.work_title_fg.into(),
            meta: theme.work_meta_fg.into(),
            accent: theme.accent.into(),
            heading: theme.heading.into(),
            success: theme.success.into(),
            add: theme.diff_add_fg.into(),
            remove: theme.diff_remove_fg.into(),
        }
    }
}

fn session_summary_layout(summary: &SessionSummary, terminal_width: usize) -> SessionSummaryLayout {
    let palette = SummaryPalette::current();
    let width = terminal_width.clamp(20, 84);
    let indent = 0;
    let content_width = width.saturating_sub(4).max(1);
    let mut rows = vec![SummaryRow::blank(palette.outer_bg)];

    let header_gap = content_width.saturating_sub(atman_tui::width::width("∴ ATMANsaved"));
    rows.push(outer_row(
        palette,
        vec![
            SummarySpan::new("∴ ATMAN", palette.accent).bold(),
            SummarySpan::new(" ".repeat(header_gap), palette.primary),
            SummarySpan::new("saved", palette.success),
        ],
    ));
    rows.push(SummaryRow::blank(palette.outer_bg));

    append_focus_block(
        &mut rows,
        width,
        palette,
        &[
            (
                summary.name.as_deref().unwrap_or("Untitled session"),
                palette.primary,
                true,
            ),
            (
                summary.project_root.as_deref().unwrap_or("-"),
                palette.meta,
                false,
            ),
        ],
    );
    rows.push(SummaryRow::blank(palette.outer_bg));

    let done = summary
        .todos
        .iter()
        .filter(|todo| matches!(todo.status, atman_runtime::memory::todo::TodoStatus::Done))
        .count();
    let pending = summary
        .todos
        .iter()
        .filter(|todo| {
            matches!(
                todo.status,
                atman_runtime::memory::todo::TodoStatus::Pending
            )
        })
        .count();
    let usage = [
        (
            format!(
                "{} tokens",
                atman_runtime::humanize::format_count(summary.cost.usage.total())
            ),
            format!(
                "{} cached",
                atman_runtime::humanize::format_count(summary.cost.usage.cached_input)
            ),
        ),
        (
            format!("{} turns", summary.msg_count),
            format_elapsed(summary.elapsed),
        ),
    ];
    let work = [
        (
            format!("{} tools", summary.activity.attempted_calls),
            format!("{} files", summary.activity.files),
            format!("+{} lines", summary.activity.insertions),
        ),
        (
            format!("{done} done"),
            format!("{pending} pending"),
            format!("−{} lines", summary.activity.deletions),
        ),
    ];
    append_stats(&mut rows, content_width, palette, &usage, &work);
    rows.push(SummaryRow::blank(palette.outer_bg));

    append_labeled_text(
        &mut rows,
        width,
        palette,
        "goal",
        summary.goal.as_deref().unwrap_or("(none)"),
    );
    let plan = summary
        .plans
        .iter()
        .max_by_key(|plan| plan.updated_at)
        .map(|plan| {
            let (done, total) = plan.progress();
            format!("{} {done}/{total}", plan.title)
        })
        .unwrap_or_else(|| "(none)".to_string());
    append_labeled_text(&mut rows, width, palette, "plan", &plan);
    rows.push(SummaryRow::blank(palette.outer_bg));

    append_focus_labeled_text(
        &mut rows,
        width,
        palette,
        "resume",
        &format!("atman --continue {}", summary.sid),
    );
    rows.push(SummaryRow::blank(palette.outer_bg));

    SessionSummaryLayout {
        width,
        indent,
        rows,
    }
}

fn outer_row(palette: SummaryPalette, spans: Vec<SummarySpan>) -> SummaryRow {
    let mut padded = vec![SummarySpan::new("  ", palette.primary)];
    padded.extend(spans);
    SummaryRow {
        bg: palette.outer_bg,
        spans: padded,
    }
}

fn append_focus_block(
    rows: &mut Vec<SummaryRow>,
    width: usize,
    palette: SummaryPalette,
    entries: &[(&str, SummaryColor, bool)],
) {
    let block_width = width.saturating_sub(4);
    rows.push(focus_row(width, palette, Vec::new()));
    let text_width = block_width.saturating_sub(4).max(1);
    for (text, color, bold) in entries {
        for line in atman_tui::width::word_wrap(&safe_text(text), text_width) {
            let mut span = SummarySpan::new(line, *color).on(palette.focus_bg);
            span.bold = *bold;
            rows.push(focus_row(width, palette, vec![span]));
        }
    }
    rows.push(focus_row(width, palette, Vec::new()));
}

fn focus_row(width: usize, palette: SummaryPalette, content: Vec<SummarySpan>) -> SummaryRow {
    let block_width = width.saturating_sub(4);
    let used = content
        .iter()
        .map(|span| atman_tui::width::width(&span.text))
        .sum::<usize>();
    let mut spans = vec![SummarySpan::new("  ", palette.primary)];
    spans.push(SummarySpan::new("  ", palette.primary).on(palette.focus_bg));
    spans.extend(content);
    spans.push(
        SummarySpan::new(
            " ".repeat(block_width.saturating_sub(2 + used)),
            palette.primary,
        )
        .on(palette.focus_bg),
    );
    SummaryRow {
        bg: palette.outer_bg,
        spans,
    }
}

fn append_stats(
    rows: &mut Vec<SummaryRow>,
    content_width: usize,
    palette: SummaryPalette,
    usage: &[(String, String); 2],
    work: &[(String, String, String); 2],
) {
    let stats_width = content_width.saturating_sub(4).max(1);
    if stats_width >= 68 {
        let gap = 6;
        let left_width = (stats_width - gap) / 2;
        let right_width = stats_width - gap - left_width;
        rows.push(inset_outer_row(
            palette,
            join_columns(
                vec![SummarySpan::new("USAGE", palette.title).bold()],
                left_width,
                vec![SummarySpan::new("WORK", palette.title).bold()],
                gap,
                palette,
            ),
        ));
        for row in 0..2 {
            rows.push(inset_outer_row(
                palette,
                join_columns(
                    usage_grid_row(usage, row, left_width, palette),
                    left_width,
                    work_grid_row(work, row, right_width, palette),
                    gap,
                    palette,
                ),
            ));
        }
    } else {
        rows.push(inset_outer_row(
            palette,
            vec![SummarySpan::new("USAGE", palette.title).bold()],
        ));
        for row in 0..2 {
            rows.push(inset_outer_row(
                palette,
                usage_grid_row(usage, row, stats_width, palette),
            ));
        }
        rows.push(SummaryRow::blank(palette.outer_bg));
        rows.push(inset_outer_row(
            palette,
            vec![SummarySpan::new("WORK", palette.title).bold()],
        ));
        for row in 0..2 {
            rows.push(inset_outer_row(
                palette,
                work_grid_row(work, row, stats_width, palette),
            ));
        }
    }
}

fn inset_outer_row(palette: SummaryPalette, mut spans: Vec<SummarySpan>) -> SummaryRow {
    spans.insert(0, SummarySpan::new("  ", palette.primary));
    outer_row(palette, spans)
}

fn usage_grid_row(
    usage: &[(String, String); 2],
    row: usize,
    width: usize,
    palette: SummaryPalette,
) -> Vec<SummarySpan> {
    let first_width = usage
        .iter()
        .map(|(first, _)| atman_tui::width::width(first))
        .max()
        .unwrap_or(0);
    let first = &usage[row].0;
    let second = &usage[row].1;
    if first_width + 3 + atman_tui::width::width(second) > width {
        return vec![SummarySpan::new(
            atman_tui::width::truncate(&format!("{first} · {second}"), width),
            palette.primary,
        )];
    }
    let mut spans = if row == 0 {
        let (count, unit) = first.split_once(' ').unwrap_or((first, ""));
        vec![
            SummarySpan::new(count, palette.heading).bold(),
            SummarySpan::new(format!(" {unit}"), palette.primary),
        ]
    } else {
        vec![SummarySpan::new(first, palette.meta)]
    };
    spans.extend([
        SummarySpan::new(
            " ".repeat(first_width.saturating_sub(atman_tui::width::width(first))),
            palette.primary,
        ),
        SummarySpan::new(" · ", palette.meta),
        SummarySpan::new(second, palette.meta),
    ]);
    pad_spans(&mut spans, width, palette.primary);
    spans
}

fn work_grid_row(
    work: &[(String, String, String); 2],
    row: usize,
    width: usize,
    palette: SummaryPalette,
) -> Vec<SummarySpan> {
    let first_width = work
        .iter()
        .map(|(first, _, _)| atman_tui::width::width(first))
        .max()
        .unwrap_or(0);
    let second_width = work
        .iter()
        .map(|(_, second, _)| atman_tui::width::width(second))
        .max()
        .unwrap_or(0);
    let (first, second, third) = &work[row];
    let fixed = first_width + 3 + second_width + 3;
    if fixed >= width || atman_tui::width::width(third) > width.saturating_sub(fixed) {
        return vec![SummarySpan::new(
            atman_tui::width::truncate(&format!("{first} · {second} · {third}"), width),
            palette.primary,
        )];
    }
    let third_width = width.saturating_sub(fixed);
    let base_color = if row == 0 {
        palette.primary
    } else {
        palette.meta
    };
    let (delta, unit) = third.split_once(' ').unwrap_or((third, ""));
    let mut spans = vec![
        SummarySpan::new(first, base_color),
        SummarySpan::new(
            " ".repeat(first_width.saturating_sub(atman_tui::width::width(first))),
            base_color,
        ),
        SummarySpan::new(" · ", palette.meta),
        SummarySpan::new(second, base_color),
        SummarySpan::new(
            " ".repeat(second_width.saturating_sub(atman_tui::width::width(second))),
            base_color,
        ),
        SummarySpan::new(" · ", palette.meta),
        SummarySpan::new(
            " ".repeat(third_width.saturating_sub(atman_tui::width::width(third))),
            palette.primary,
        ),
        SummarySpan::new(
            atman_tui::width::truncate(delta, third_width),
            if row == 0 {
                palette.add
            } else {
                palette.remove
            },
        ),
        SummarySpan::new(format!(" {unit}"), palette.meta),
    ];
    pad_spans(&mut spans, width, palette.primary);
    spans
}

fn join_columns(
    mut left: Vec<SummarySpan>,
    left_width: usize,
    right: Vec<SummarySpan>,
    gap: usize,
    palette: SummaryPalette,
) -> Vec<SummarySpan> {
    pad_spans(&mut left, left_width, palette.primary);
    left.push(SummarySpan::new(" ".repeat(gap), palette.primary));
    left.extend(right);
    left
}

fn pad_spans(spans: &mut Vec<SummarySpan>, width: usize, color: SummaryColor) {
    let used = spans
        .iter()
        .map(|span| atman_tui::width::width(&span.text))
        .sum::<usize>();
    spans.push(SummarySpan::new(
        " ".repeat(width.saturating_sub(used)),
        color,
    ));
}

fn append_labeled_text(
    rows: &mut Vec<SummaryRow>,
    width: usize,
    palette: SummaryPalette,
    label: &str,
    text: &str,
) {
    let content_width = width.saturating_sub(8).max(1);
    let label = format!("{label:<8}");
    let label_width = atman_tui::width::width(&label);
    let lines = atman_tui::width::word_wrap(
        &safe_text(text),
        content_width.saturating_sub(label_width).max(1),
    );
    for (index, line) in lines.into_iter().enumerate() {
        rows.push(inset_outer_row(
            palette,
            vec![
                SummarySpan::new(
                    if index == 0 {
                        label.clone()
                    } else {
                        " ".repeat(label_width)
                    },
                    palette.title,
                ),
                SummarySpan::new(line, palette.primary),
            ],
        ));
    }
}

fn append_focus_labeled_text(
    rows: &mut Vec<SummaryRow>,
    width: usize,
    palette: SummaryPalette,
    label: &str,
    text: &str,
) {
    let block_width = width.saturating_sub(4);
    let content_width = block_width.saturating_sub(4).max(1);
    let label = format!("{label:<10}");
    let label_width = atman_tui::width::width(&label);
    rows.push(focus_row(width, palette, Vec::new()));
    for (index, line) in atman_tui::width::word_wrap(
        &safe_text(text),
        content_width.saturating_sub(label_width).max(1),
    )
    .into_iter()
    .enumerate()
    {
        rows.push(focus_row(
            width,
            palette,
            vec![
                SummarySpan::new(
                    if index == 0 {
                        label.clone()
                    } else {
                        " ".repeat(label_width)
                    },
                    palette.accent,
                )
                .on(palette.focus_bg),
                SummarySpan::new(line, palette.primary).on(palette.focus_bg),
            ],
        ));
    }
    rows.push(focus_row(width, palette, Vec::new()));
}

fn format_elapsed(duration: std::time::Duration) -> String {
    let seconds = duration.as_secs();
    match seconds {
        0..=59 => format!("{seconds}s"),
        60..=3_599 => format!("{}m {}s", seconds / 60, seconds % 60),
        3_600..=86_399 => format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60),
        _ => format!("{}d {}h", seconds / 86_400, (seconds % 86_400) / 3_600),
    }
}

fn write_summary_row(writer: &mut impl std::io::Write, row: &SummaryRow, width: usize) {
    let SummaryColor(br, bg, bb) = row.bg;
    let _ = write!(writer, "\x1b[48;2;{br};{bg};{bb}m");
    let mut used = 0usize;
    for span in &row.spans {
        let SummaryColor(fr, fg, fb) = span.fg;
        let SummaryColor(sr, sg, sb) = span.bg.unwrap_or(row.bg);
        let weight = if span.bold { "1" } else { "22" };
        let _ = write!(
            writer,
            "\x1b[{weight};38;2;{fr};{fg};{fb};48;2;{sr};{sg};{sb}m{}",
            span.text
        );
        used += atman_tui::width::width(&span.text);
    }
    let _ = writeln!(
        writer,
        "\x1b[22;48;2;{br};{bg};{bb}m{}\x1b[0m",
        " ".repeat(width.saturating_sub(used))
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captured_session_statistics_merge_loaded_flows_without_history_reads() {
        let contexts: Vec<_> = (1..=2).map(|index| {
            let context = Arc::new(Session::open_ephemeral());
            for event in [
                serde_json::json!({"type":"llm_call", "model":"model", "provider":"gateway",
                    "usage":{"input":index * 10,"output":2,"cached_input":7,"cache_write":3,"reasoning_tokens":5}}),
                serde_json::json!({"type":"tool_node", "run_id":Uuid::now_v7(),
                    "parent_node_id":"node", "tool_use_id":"call", "tool_name":"fs.edit", "args_preview":""}),
                serde_json::json!({"type":"file_edit_applied", "tool_name":"fs.edit",
                    "path":"/workspace/file.rs", "metrics":{"hunks":1,"insertions":3,"deletions":1}}),
            ] {
                context.sink().emit(serde_json::from_value(event).unwrap());
            }
            context
        }).collect();
        let summary = Capture {
            summary: SessionSummary::empty(Uuid::now_v7()),
            contexts,
        }
        .finish(Duration::from_secs(12));
        assert_eq!(summary.cost.calls, 2);
        assert_eq!(summary.cost.usage.input, 30);
        assert_eq!(summary.cost.usage.output, 4);
        assert_eq!(summary.cost.usage.cached_input, 14);
        assert_eq!(summary.cost.usage.cache_write, 6);
        assert_eq!(summary.cost.usage.reasoning_tokens, 10);
        assert_eq!(summary.activity.attempted_calls, 2);
        assert_eq!(summary.activity.files, 1);
        assert_eq!(
            (summary.activity.insertions, summary.activity.deletions),
            (6, 2)
        );
        assert_eq!(summary.elapsed, Duration::from_secs(12));
        assert_eq!(safe_text("goal\u{1b}[2J\nplan\tend"), "goal[2J\nplan  end");
    }

    fn summary_fixture() -> SessionSummary {
        let mut plan = atman_runtime::memory::plan::Plan::new(
            "plan",
            "完成退出摘要布局并验证很长的计划内容可以完整换行",
            vec!["layout".into(), "verify".into()],
        );
        plan.steps[0].done = true;
        let todo = |status| atman_runtime::memory::todo::Todo {
            id: atman_runtime::memory::MemoryId::now(),
            where_: "summary".into(),
            why: "fixture".into(),
            how: "fixture".into(),
            expected_result: "fixture".into(),
            status,
        };
        SessionSummary {
            sid: "b3bacc33-f077-492a-96ad-1c3c9657f8e4".into(),
            name: Some("DSL闭包实现与意图字段复盘".into()),
            project_root: Some("/workspace/project".into()),
            msg_count: 107,
            goal: Some("实现一个完整显示并且在较窄终端中自然换行的退出摘要".into()),
            todos: vec![
                todo(atman_runtime::memory::todo::TodoStatus::Done),
                todo(atman_runtime::memory::todo::TodoStatus::Pending),
            ],
            plans: vec![plan],
            activity: atman_runtime::activity::ActivitySummary {
                attempted_calls: 69,
                files: 8,
                insertions: 214,
                deletions: 37,
                ..Default::default()
            },
            cost: atman_runtime::cost::CostSummary {
                usage: atman_runtime::provider::TokenUsage {
                    input: 410_000,
                    cached_input: 920_000,
                    output: 20_000,
                    ..Default::default()
                },
                ..Default::default()
            },
            elapsed: std::time::Duration::from_secs(18 * 60 + 42),
        }
    }

    #[test]
    fn session_summary_aligns_stat_separators_and_line_deltas() {
        let layout = session_summary_layout(&summary_fixture(), 104);
        assert_eq!((layout.width, layout.indent), (84, 0));
        let stat_rows = [
            layout.rows[9].plain(layout.width),
            layout.rows[10].plain(layout.width),
        ];
        let separator_columns = |line: &str| {
            line.char_indices()
                .filter_map(|(index, ch)| {
                    (ch == '·').then_some(atman_tui::width::width(&line[..index]))
                })
                .collect::<Vec<_>>()
        };

        assert_eq!(
            separator_columns(&stat_rows[0]),
            separator_columns(&stat_rows[1])
        );
        for line in &stat_rows {
            assert!(line.contains(" · "));
        }
        let add_start = stat_rows[0].find("+214 lines").unwrap();
        let add_end = atman_tui::width::width(&stat_rows[0][..add_start])
            + atman_tui::width::width("+214 lines");
        let remove_start = stat_rows[1].find("−37 lines").unwrap();
        let remove_end = atman_tui::width::width(&stat_rows[1][..remove_start])
            + atman_tui::width::width("−37 lines");
        assert_eq!(add_end, remove_end);
        assert_eq!(add_end, layout.width - 4);

        let title_row = layout.rows[4].plain(layout.width);
        let usage_header = layout.rows[8].plain(layout.width);
        let goal_row = layout.rows[12].plain(layout.width);
        let plan_row = layout.rows[13].plain(layout.width);
        for (line, text) in [
            (&title_row, "DSL闭包实现与意图字段复盘"),
            (&usage_header, "USAGE"),
            (&goal_row, "goal"),
            (&plan_row, "plan"),
        ] {
            let start = line.find(text).unwrap();
            assert_eq!(atman_tui::width::width(&line[..start]), 4);
        }
    }

    #[test]
    fn session_summary_wraps_content_without_exceeding_terminal_width() {
        let layout = session_summary_layout(&summary_fixture(), 46);

        assert_eq!(layout.width, 46);
        assert!(layout.rows.len() > 19);
        assert!(
            layout
                .rows
                .iter()
                .all(|row| atman_tui::width::width(&row.plain(layout.width)) == layout.width)
        );
        let text = layout
            .rows
            .iter()
            .map(|row| row.plain(layout.width))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("DSL闭包实现"));
        assert!(
            text.split_whitespace()
                .collect::<String>()
                .contains("b3bacc33-f077-492a-96ad-1c3c9657f8e4")
        );
    }

    #[test]
    fn focus_blocks_fill_the_inner_width_with_two_column_outer_padding() {
        let layout = session_summary_layout(&summary_fixture(), 104);
        let focus_bg = SummaryPalette::current().focus_bg;
        for row_index in [3, 4, 5, 6, 15, 16, 17] {
            let focus_width = layout.rows[row_index]
                .spans
                .iter()
                .filter(|span| span.bg == Some(focus_bg))
                .map(|span| atman_tui::width::width(&span.text))
                .sum::<usize>();
            assert_eq!(focus_width, layout.width - 4);
        }
    }
}
