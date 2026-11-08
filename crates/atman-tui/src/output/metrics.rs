//! Source-scoped workflow measurements and view-owned presentation motion.

use std::{
    cell::Cell,
    hash::{DefaultHasher, Hash, Hasher},
    time::{Duration, Instant},
};

use ratatui::{
    style::Style,
    text::{Line, Span},
};

use super::{WorkflowProjection, workflow_llm_display};

pub(super) const DURATION: Duration = Duration::from_millis(320);
pub(super) const CLOCK_MARKER: char = '\u{e007}';

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Metric {
    Input,
    Output,
    Cache,
    Insertions,
    Deletions,
    Files,
}

impl Metric {
    const ALL: [Self; 6] = [
        Self::Input,
        Self::Output,
        Self::Cache,
        Self::Insertions,
        Self::Deletions,
        Self::Files,
    ];

    fn index(self) -> usize {
        self as usize
    }
    fn width(self) -> usize {
        match self {
            Self::Input => 6,
            Self::Output => 5,
            Self::Cache | Self::Insertions | Self::Deletions => 4,
            Self::Files => 3,
        }
    }
    fn marker(self) -> char {
        [
            '\u{e001}', '\u{e002}', '\u{e003}', '\u{e004}', '\u{e005}', '\u{e006}',
        ][self.index()]
    }
    fn glyph(self) -> &'static str {
        match self {
            Self::Input => "↑",
            Self::Output => "↓",
            Self::Cache => "↻",
            Self::Insertions => "+",
            Self::Deletions => "−",
            Self::Files => "▧",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Cache {
    Absent,
    Rate,
    Tokens,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MetricSnapshot {
    turn: atman_runtime::event::AtmanUuid,
    routes: u64,
    numbers: [u64; 6],
    usage: bool,
    cache: Cache,
    edits: bool,
}

impl MetricSnapshot {
    pub(crate) fn capture(graph: &WorkflowProjection) -> Option<Self> {
        let usage = workflow_llm_display(graph.summary().llm_routes()).filter(|display| {
            display.stats.total_in > 0
                || display.stats.total_out > 0
                || display.stats.cache_read > 0
        });
        let edits = graph.applied_edits();
        if usage.is_none() && edits.applied_edits == 0 {
            return None;
        }
        let mut value = Self {
            turn: graph.graph().turn_id.0,
            routes: 0,
            numbers: [0; 6],
            usage: usage.is_some(),
            cache: Cache::Absent,
            edits: edits.applied_edits > 0,
        };
        if let Some(display) = usage {
            // Order-independent presentation identity; route changes install exact values.
            for route in graph
                .summary()
                .llm_routes()
                .keys()
                .filter(|route| route.is_primary() || !display.primary)
            {
                let mut hash = DefaultHasher::new();
                route.hash(&mut hash);
                value.routes = value.routes.wrapping_add(hash.finish());
            }
            value.numbers[0] = display.stats.total_in;
            value.numbers[1] = display.stats.total_out;
            value.numbers[2] = display.stats.cache_read;
            if display.stats.cache_read > 0 {
                value.cache = if display.routes == 1 && display.stats.total_in > 0 {
                    Cache::Rate
                } else {
                    Cache::Tokens
                };
            }
        }
        value.numbers[3] = edits.insertions as u64;
        value.numbers[4] = edits.deletions as u64;
        value.numbers[5] = edits.files as u64;
        Some(value)
    }

    pub(crate) fn item(item: &crate::app::OutputItem) -> (Option<Self>, bool) {
        use crate::app::OutputItem;
        match item {
            OutputItem::WorkflowPanel {
                graph, ended_at, ..
            } => (
                Self::capture(graph),
                ended_at.is_none()
                    && graph.summary().status() == super::WorkflowAggregateStatus::Running,
            ),
            OutputItem::SubAgentActivity {
                workflow_graph,
                status,
                ..
            } => (Self::capture(workflow_graph), status.is_running()),
            _ => (None, false),
        }
    }

    fn same_policy(self, other: Self) -> bool {
        self.turn == other.turn
            && self.routes == other.routes
            && self.usage == other.usage
            && self.cache == other.cache
            && self.edits == other.edits
    }

    fn contains(self, metric: Metric) -> bool {
        match metric {
            Metric::Input | Metric::Output => self.usage,
            Metric::Cache => self.cache != Cache::Absent,
            _ => self.edits,
        }
    }

    fn format(self, metric: Metric) -> String {
        let raw = if metric == Metric::Cache && self.cache == Cache::Rate {
            format!(
                "{:.0}%",
                self.numbers[2] as f64 / self.numbers[0].max(1) as f64 * 100.0
            )
        } else {
            atman_runtime::humanize::format_count(self.numbers[metric.index()])
        };
        let width = metric.width();
        let text = if crate::width::width(&raw) > width {
            format!("{:.0e}", self.numbers[metric.index()] as f64)
        } else {
            raw
        };
        format!("{text:>width$}")
    }
}

pub(super) fn spans(
    value: Option<MetricSnapshot>,
    animated: bool,
    budget: usize,
) -> Vec<Span<'static>> {
    let Some(value) = value else {
        return Vec::new();
    };
    let theme = crate::theme::theme();
    let mut spans = Vec::new();
    let mut used = 0;
    for metric in Metric::ALL
        .into_iter()
        .filter(|metric| value.contains(*metric))
    {
        let gap = if spans.is_empty() { 0 } else { 2 };
        let width = 1 + metric.width();
        if used + gap + width > budget {
            break;
        }
        let color = match metric {
            Metric::Insertions => theme.success,
            Metric::Deletions => theme.error,
            _ => theme.work_meta_fg,
        };
        let style = Style::default().fg(color.into());
        if gap > 0 {
            spans.push(Span::styled("  ", style));
        }
        spans.push(Span::styled(metric.glyph(), style));
        spans.push(Span::styled(
            if animated {
                metric.marker().to_string().repeat(metric.width())
            } else {
                value.format(metric)
            },
            style,
        ));
        used += gap + width;
    }
    spans
}

#[derive(Clone, Debug)]
struct Field {
    line: usize,
    span: usize,
    metric: Metric,
}

#[derive(Clone, Debug)]
pub(super) struct MetricPaint {
    value: MetricSnapshot,
    fields: Vec<Field>,
}

impl MetricPaint {
    pub(super) fn capture(graph: &WorkflowProjection, lines: &[Line<'static>]) -> Option<Self> {
        let value = MetricSnapshot::capture(graph)?;
        let mut fields = Vec::new();
        for (line_index, line) in lines.iter().enumerate() {
            for (index, pair) in line.spans.windows(2).enumerate() {
                for metric in Metric::ALL {
                    if pair[0].content == metric.glyph()
                        && pair[1].content.chars().count() == metric.width()
                        && pair[1].content.chars().all(|ch| ch == metric.marker())
                    {
                        fields.push(Field {
                            line: line_index,
                            span: index + 1,
                            metric,
                        });
                    }
                }
            }
        }
        (!fields.is_empty()).then_some(Self { value, fields })
    }

    pub(super) fn shift(&mut self, rows: usize) {
        for field in &mut self.fields {
            field.line += rows;
        }
    }

    fn visible(&self, start: usize, end: usize) -> bool {
        self.fields
            .iter()
            .any(|field| (start..end).contains(&field.line))
    }

    pub(super) fn patch(
        &self,
        line: &mut Line<'static>,
        row: usize,
        value: Option<MetricSnapshot>,
    ) {
        let value = value
            .filter(|value| value.same_policy(self.value))
            .unwrap_or(self.value);
        for field in self.fields.iter().filter(|field| field.line == row) {
            if let Some(span) = line.spans.get_mut(field.span) {
                span.content = value.format(field.metric).into();
            }
        }
    }
}

pub(crate) fn patch(
    lines: &mut [Line<'static>],
    paint: &super::DynamicPaint,
    motion: &MetricMotion,
    offset: usize,
) {
    let Some(fields) = &paint.metrics else {
        return;
    };
    if !fields.visible(offset, offset.saturating_add(lines.len())) {
        return;
    }
    let value = motion.paint(Instant::now());
    for (row, line) in lines.iter_mut().enumerate() {
        fields.patch(line, offset + row, value);
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct MetricMotion {
    target: Option<MetricSnapshot>,
    from: Option<MetricSnapshot>,
    started: Option<Instant>,
    last_painted: Cell<Option<Instant>>,
}

impl MetricMotion {
    pub(crate) fn observe(
        &mut self,
        target: Option<MetricSnapshot>,
        running: bool,
        animate: bool,
        now: Instant,
    ) {
        let recent = self
            .last_painted
            .get()
            .is_some_and(|last| now.saturating_duration_since(last) <= DURATION);
        let compatible = self.target.zip(target).is_some_and(|(old, new)| {
            old.same_policy(new) && old.numbers.iter().zip(new.numbers).all(|(a, b)| *a <= b)
        });
        let interpolate = running && animate && recent && compatible;
        if target != self.target {
            self.from = if interpolate {
                self.sample(now)
            } else {
                target
            };
            self.started = interpolate.then_some(now);
            self.target = target;
        }
        if !interpolate {
            self.from = target;
            self.started = None;
        }
    }

    fn sample(&self, now: Instant) -> Option<MetricSnapshot> {
        let target = self.target?;
        let Some(start) = self.started else {
            return Some(target);
        };
        if self
            .last_painted
            .get()
            .is_none_or(|last| now.saturating_duration_since(last) > DURATION)
        {
            return Some(target);
        }
        let elapsed = now.saturating_duration_since(start).as_secs_f64() / DURATION.as_secs_f64();
        let eased = 1.0 - (1.0 - elapsed.clamp(0.0, 1.0)).powi(3);
        let Some(from) = self.from else {
            return Some(target);
        };
        let mut value = target;
        for (index, number) in value.numbers.iter_mut().enumerate() {
            *number = from.numbers[index]
                .saturating_add(
                    ((target.numbers[index] - from.numbers[index]) as f64 * eased).round() as u64,
                )
                .min(target.numbers[index]);
        }
        Some(value)
    }

    pub(crate) fn paint(&self, now: Instant) -> Option<MetricSnapshot> {
        let value = self.sample(now);
        self.last_painted.set(Some(now));
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(count: u64) -> MetricSnapshot {
        MetricSnapshot {
            turn: atman_runtime::event::AtmanUuid(uuid::Uuid::nil()),
            routes: 1,
            numbers: [count, count / 10, count * 82 / 100, 128, 37, 1],
            usage: true,
            cache: Cache::Rate,
            edits: true,
        }
    }

    #[test]
    fn motion_is_monotonic_retargets_visible_values_and_snaps_foreign_or_hidden_sources() {
        let now = Instant::now();
        let mut motion = MetricMotion::default();
        let first = snapshot(10000);
        motion.observe(Some(first), true, true, now);
        assert_eq!(motion.paint(now), Some(first));
        let target = snapshot(30000);
        motion.observe(Some(target), true, true, now);
        let midpoint = motion.paint(now + DURATION / 2).unwrap();
        assert_eq!(midpoint.numbers[0], 27500);
        motion.observe(Some(snapshot(60000)), true, true, now + DURATION / 2);
        assert_eq!(motion.paint(now + DURATION / 2), Some(midpoint));
        let mut last = midpoint.numbers[0];
        for ms in (160..=480).step_by(16) {
            let current = motion
                .paint(now + Duration::from_millis(ms))
                .unwrap()
                .numbers[0];
            assert!((last..=60000).contains(&current));
            last = current;
        }
        assert_eq!(last, 60000);
        // Completion, disabled motion, reductions and source changes never replay growth.
        for (mut value, running, animate) in [
            (snapshot(70000), false, true),
            (snapshot(80000), true, false),
            (snapshot(20000), true, true),
            (snapshot(90000), true, true),
        ] {
            if value.numbers[0] == 90000 {
                value.routes = 2;
            }
            motion.observe(Some(value), running, animate, now + DURATION * 2);
            assert_eq!(motion.paint(now + DURATION * 2), Some(value));
        }
        let mut foreign = snapshot(100000);
        foreign.routes = 2;
        foreign.turn = atman_rt::TurnId::now().0;
        motion.observe(Some(foreign), true, true, now + DURATION * 2);
        assert_eq!(motion.paint(now + DURATION * 2), Some(foreign));
        motion.observe(Some(snapshot(200000)), true, true, now + DURATION * 4);
        assert_eq!(motion.paint(now + DURATION * 4), Some(snapshot(200000)));
    }

    #[test]
    fn fixed_fields_patch_both_surfaces_without_changing_geometry_or_styles() {
        let initial = snapshot(10000);
        let fields = spans(Some(initial), true, 100);
        let mut paint = MetricPaint {
            value: initial,
            fields: vec![],
        };
        let mut x = 0;
        for (index, span) in fields.iter().enumerate() {
            if let Some(metric) = Metric::ALL
                .into_iter()
                .find(|m| span.content.chars().all(|ch| ch == m.marker()))
            {
                paint.fields.extend([0, 2].map(|line| Field {
                    line,
                    span: index,
                    metric,
                }));
            }
            x += span.width();
        }
        let mut header = Line::from(fields.clone());
        let mut footer = Line::from(fields);
        for count in [10000, 27500, 30000, u64::MAX / 100] {
            let value = snapshot(count);
            paint.patch(&mut header, 0, Some(value));
            paint.patch(&mut footer, 2, Some(value));
            assert_eq!(header, footer);
            assert_eq!(header.width(), x);
            assert!(
                header
                    .spans
                    .iter()
                    .all(|s| !s.content.contains(['\u{e001}', '\u{e002}']))
            );
        }
        for width in 0..100 {
            let visible = spans(Some(initial), false, width);
            assert!(crate::width::spans_width(visible.iter()) <= width);
        }
    }
}
