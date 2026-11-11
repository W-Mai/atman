//! View-bound reasoning choices and the composer's fixed-width meter.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use atman_runtime::{
    model_registry::ModelRegistrySnapshot,
    provider::{ReasoningEffort, ReasoningSelection},
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Clear, Paragraph},
};

use crate::{app::AppState, flow_view::DocumentIdentity, input::InputAction};

pub(crate) const WIDTH: u16 = 12;
const CELLS: u16 = 8;
const MOTION: Duration = Duration::from_millis(180);
const FEEDBACK: Duration = Duration::from_millis(1100);

#[derive(Clone)]
struct Binding {
    document: DocumentIdentity,
    model: String,
    models: Arc<ModelRegistrySnapshot>,
}

impl Binding {
    fn matches(&self, app: &AppState) -> bool {
        !app.items.is_archive()
            && self.document == app.items.identity()
            && self.model == app.input_model_key()
            && app
                .model_registry()
                .is_some_and(|models| Arc::ptr_eq(&models, &self.models))
    }

    fn same_source(&self, other: &Self) -> bool {
        self.document == other.document
            && self.model == other.model
            && Arc::ptr_eq(&self.models, &other.models)
    }
}

#[derive(Clone)]
pub(crate) struct Projection {
    binding: Option<Binding>,
    choices: Vec<Option<ReasoningSelection>>,
    stops: Vec<usize>,
    requested: Option<ReasoningSelection>,
    label: String,
    marker: Option<&'static str>,
    fraction: Option<f32>,
}

fn rank(effort: &ReasoningEffort) -> Option<u32> {
    use ReasoningEffort::*;
    match effort {
        Minimal => Some(0),
        Low => Some(1),
        Medium => Some(2),
        High => Some(3),
        XHigh => Some(4),
        Max => Some(5),
        Ultra => Some(6),
        Persistent => Some(7),
        None | Custom(_) => Option::None,
    }
}

fn label(selection: &Option<ReasoningSelection>) -> String {
    match selection {
        None => "default".into(),
        Some(ReasoningSelection::Disabled) => "none".into(),
        Some(selection) => selection.to_string(),
    }
}

impl Projection {
    pub(crate) fn capture(app: &AppState) -> Self {
        let binding = app
            .input_model()
            .filter(|(models, _)| models.model_info(&app.input_model_key()).context_budget > 0)
            .map(|(models, _)| Binding {
                document: app.items.identity(),
                model: app.input_model_key(),
                models,
            });
        let choices = app.input_reasoning_choices();
        let effective = app.input_reasoning_for_submission();
        let domain = app.input_reasoning.as_ref().or(effective.as_ref());
        let execution = domain.and_then(ReasoningSelection::execution_mode);
        let budgets = matches!(domain, Some(ReasoningSelection::BudgetTokens { .. }));
        let mut ordered: Vec<_> = choices
            .iter()
            .enumerate()
            .filter_map(|(index, selection)| {
                let order = match selection {
                    Some(ReasoningSelection::Effort {
                        effort,
                        execution_mode,
                    }) if !budgets && execution_mode.as_ref() == execution => rank(effort),
                    Some(ReasoningSelection::BudgetTokens { tokens }) if budgets => Some(*tokens),
                    _ => None,
                }?;
                Some((order, index))
            })
            .collect();
        ordered.sort_by_key(|(order, _)| *order);
        let stops: Vec<_> = ordered.into_iter().map(|(_, index)| index).collect();
        let selected = stops
            .iter()
            .position(|index| choices[*index].as_ref() == domain);
        let fraction = selected.map(|index| (index + 1) as f32 / stops.len() as f32);
        let mut title = label(&app.input_reasoning);
        if app.input_reasoning.is_none()
            && let Some(effective) = effective
        {
            let resolved = effective.to_string();
            if resolved != "default" {
                title.push_str(&format!(" · {resolved}"));
            }
        }
        let mut marker = match &app.input_reasoning {
            None if fraction.is_none() => Some("↳"),
            None => None,
            Some(
                ReasoningSelection::Disabled
                | ReasoningSelection::Effort {
                    effort: ReasoningEffort::None,
                    ..
                },
            ) => Some("⌀"),
            Some(ReasoningSelection::Auto { .. }) => Some("A"),
            _ if fraction.is_none() => Some("◆"),
            _ => None,
        };
        if let Some(source) = &binding
            && let Some(requested) = &app.input_reasoning
            && source
                .models
                .resolve_reasoning_for_model(&source.model, requested)
                .is_err()
        {
            title.push_str(" !");
            marker = Some("!");
        }
        if binding.is_none() {
            title = if app.items.is_archive() {
                "on continue"
            } else if app.settings.is_waiting() {
                "Loading reasoning"
            } else {
                "Reasoning unavailable"
            }
            .into();
        }
        Self {
            binding,
            choices,
            stops,
            requested: app.input_reasoning.clone(),
            label: title,
            marker,
            fraction,
        }
    }

    fn same_source(&self, other: &Self) -> bool {
        self.binding
            .as_ref()
            .zip(other.binding.as_ref())
            .is_some_and(|(a, b)| a.same_source(b))
    }

    fn available(&self) -> bool {
        self.binding.is_some()
    }

    fn matches(&self, app: &AppState) -> bool {
        self.binding
            .as_ref()
            .is_some_and(|source| source.matches(app))
    }

    fn stop_at(&self, cell: u16) -> Option<usize> {
        if self.stops.is_empty() {
            return None;
        }
        let stop = usize::from(cell.min(CELLS - 1)) * self.stops.len() / usize::from(CELLS);
        self.stops
            .get(if cell == CELLS - 1 {
                self.stops.len() - 1
            } else {
                stop
            })
            .copied()
    }
}

struct Motion {
    from: f32,
    to: f32,
    started: Instant,
}

impl Motion {
    fn value(&self, now: Instant) -> f32 {
        let progress = (now.saturating_duration_since(self.started).as_secs_f32()
            / MOTION.as_secs_f32())
        .clamp(0.0, 1.0);
        self.from + (self.to - self.from) * (1.0 - (1.0 - progress).powi(3))
    }
}

#[derive(Default)]
pub(crate) struct Meter {
    projection: Option<Projection>,
    motion: Option<Motion>,
    feedback: Option<Instant>,
}

impl Meter {
    pub(crate) fn hide(&mut self) {
        if let Some(projection) = self.projection.take() {
            crate::render_work::discard(projection);
        }
        self.motion = None;
        self.feedback = None;
    }
    pub(crate) fn motion_pending(&self) -> bool {
        self.motion.is_some()
    }
    pub(crate) fn feedback_pending(&self) -> bool {
        self.feedback.is_some()
    }
    pub(crate) fn show_feedback(&mut self) {
        self.feedback = Some(Instant::now());
    }

    fn observe(&mut self, next: Projection, now: Instant) -> bool {
        let unavailable = next.binding.is_none()
            && self
                .projection
                .as_ref()
                .is_none_or(|old| old.binding.is_none());
        let same = self
            .projection
            .as_ref()
            .is_some_and(|old| old.same_source(&next));
        if !same {
            self.motion = None;
            self.feedback = None;
        } else if let Some(previous) = &self.projection
            && previous.requested != next.requested
        {
            let from = self
                .motion
                .as_ref()
                .map(|motion| motion.value(now))
                .or(previous.fraction)
                .unwrap_or(0.0);
            self.motion = next
                .fraction
                .filter(|to| (*to - from).abs() > f32::EPSILON)
                .map(|to| Motion {
                    from,
                    to,
                    started: now,
                });
            self.feedback = Some(now);
        }
        if self
            .motion
            .as_ref()
            .is_some_and(|motion| now.saturating_duration_since(motion.started) >= MOTION)
        {
            self.motion = None;
        }
        if self
            .feedback
            .is_some_and(|started| now.saturating_duration_since(started) >= FEEDBACK)
        {
            self.feedback = None;
        }
        let old = self.projection.replace(next);
        if !same && let Some(old) = old {
            crate::render_work::discard(old);
        }
        !same && !unavailable
    }
}

fn select(app: &mut AppState, projection: &Projection, index: usize) -> bool {
    let Some(selection) = projection.choices.get(index) else {
        return false;
    };
    if !projection.matches(app) || !app.input_reasoning_choices().contains(selection) {
        return false;
    }
    app.input_reasoning = selection.clone();
    app.reasoning_meter.show_feedback();
    app.mark_visual_dirty();
    true
}

pub(crate) fn select_painted(app: &mut AppState, index: usize) -> bool {
    let Some(projection) = app.reasoning_meter.projection.clone() else {
        return false;
    };
    select(app, &projection, index)
}

pub(crate) fn render_meter(frame: &mut Frame, app: &mut AppState, area: Rect) {
    let projection = Projection::capture(app);
    let now = Instant::now();
    if app.reasoning_meter.observe(projection, now)
        && matches!(
            app.input_hover,
            Some(InputAction::Reasoning | InputAction::ReasoningLevel(_))
        )
    {
        app.input_hover = None;
    }
    let meter = &app.reasoning_meter;
    let projection = meter
        .projection
        .as_ref()
        .expect("installed reasoning projection");
    let theme = crate::theme::theme();
    let hovered = matches!(
        app.input_hover,
        Some(InputAction::Reasoning | InputAction::ReasoningLevel(_))
    );
    let bg = if hovered && projection.available() {
        theme.work_hover_bg.into()
    } else {
        Color::Reset
    };
    let base = Style::default().fg(theme.subtle_fg.into()).bg(bg);
    frame.render_widget(
        Paragraph::new(" ⋯ ").style(base),
        Rect::new(area.x, area.y, 3, 1),
    );
    app.input_controls
        .push((InputAction::Reasoning, Rect::new(area.x, area.y, 3, 1)));
    let preview = match app.input_hover {
        Some(InputAction::ReasoningLevel(index)) if projection.stops.contains(&index) => {
            Some(index)
        }
        _ => None,
    };
    let changed_preview =
        preview.is_some_and(|index| projection.choices[index] != projection.requested);
    let peak = projection.available()
        && if changed_preview {
            preview == projection.stops.last().copied()
        } else {
            projection.marker.is_none() && projection.fraction == Some(1.0)
        };
    let fraction = if changed_preview {
        preview
            .and_then(|index| projection.stops.iter().position(|stop| *stop == index))
            .map(|index| (index + 1) as f32 / projection.stops.len() as f32)
    } else {
        meter
            .motion
            .as_ref()
            .map(|motion| motion.value(now))
            .or(projection.fraction)
    };
    let amount = fraction.unwrap_or(0.0) * f32::from(CELLS);
    let color = if changed_preview {
        theme.accent.lerp(theme.subtle_fg, 0.45)
    } else {
        theme.accent.into()
    };
    let mut spans = Vec::with_capacity(CELLS as usize + 1);
    for cell in 0..CELLS {
        let symbol = if projection.available() {
            if amount >= f32::from(cell + 1) {
                "━"
            } else if amount >= f32::from(cell) + 0.5 {
                "╾"
            } else if matches!(projection.requested, Some(ReasoningSelection::Auto { .. }))
                && !changed_preview
            {
                "┄"
            } else {
                "─"
            }
        } else {
            "─"
        };
        spans.push(Span::styled(
            symbol,
            base.fg(if amount > f32::from(cell) {
                if peak {
                    let color = theme.reasoning_peak[usize::from(cell)];
                    if changed_preview {
                        color.lerp(theme.subtle_fg, 0.45)
                    } else {
                        color.into()
                    }
                } else {
                    color
                }
            } else {
                theme.subtle_fg.into()
            }),
        ));
        if projection.available()
            && let Some(index) = projection.stop_at(cell)
        {
            app.input_controls.push((
                InputAction::ReasoningLevel(index),
                Rect::new(area.x + 3 + cell, area.y, 1, 1),
            ));
        }
    }
    spans.push(Span::styled(" ", base));
    frame.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(area.x + 3, area.y, CELLS + 1, 1),
    );
    if !changed_preview
        && projection.available()
        && let Some(marker) = projection.marker
    {
        frame.render_widget(
            Paragraph::new(format!(" {marker} ")).style(base),
            Rect::new(area.x + 5, area.y, 3, 1),
        );
    }
}

pub(crate) fn floating_rect(canvas: Rect, anchor: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(canvas.width);
    let height = height.min(canvas.height);
    let above = anchor.y.saturating_sub(canvas.y);
    let y = if above >= height.saturating_add(1) {
        anchor.y - height - 1
    } else if canvas.bottom().saturating_sub(anchor.bottom()) >= height.saturating_add(1) {
        anchor.bottom() + 1
    } else {
        anchor
            .y
            .saturating_sub(height)
            .clamp(canvas.y, canvas.bottom().saturating_sub(height))
    };
    Rect::new(
        anchor
            .x
            .clamp(canvas.x, canvas.right().saturating_sub(width)),
        y.clamp(canvas.y, canvas.bottom().saturating_sub(height)),
        width,
        height,
    )
}

pub(crate) fn render_hint(frame: &mut Frame, app: &AppState, anchor: Rect) {
    let meter = &app.reasoning_meter;
    let Some(projection) = &meter.projection else {
        return;
    };
    let hovered = matches!(
        app.input_hover,
        Some(InputAction::Reasoning | InputAction::ReasoningLevel(_))
    );
    if !hovered && meter.feedback.is_none() {
        return;
    }
    let theme = crate::theme::theme();
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(
            projection.label.clone(),
            Style::default().fg(theme.tinted_fg.into()),
        ),
    ];
    if let Some(InputAction::ReasoningLevel(index)) = app.input_hover
        && let Some(choice) = projection.choices.get(index)
        && *choice != projection.requested
    {
        spans.push(Span::styled(
            " → ",
            Style::default().fg(theme.subtle_fg.into()),
        ));
        spans.push(Span::styled(
            label(choice),
            Style::default().fg(theme.tinted_fg.into()),
        ));
    }
    spans.push(Span::raw(" "));
    let line = Line::from(spans);
    let rect = floating_rect(
        frame.area(),
        anchor,
        line.width().min(u16::MAX as usize) as u16,
        1,
    );
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(crate::surface::Role::Overlay.background())),
        rect,
    );
}

#[derive(Default)]
pub(crate) struct Picker {
    pub(crate) open: bool,
    projection: Option<Projection>,
    selected: usize,
    hovered: Option<usize>,
    rect: Rect,
    hits: Vec<(usize, Rect)>,
}

impl Picker {
    pub(crate) fn bounds(&self, canvas: Rect) -> Rect {
        self.rect.intersection(canvas)
    }
    pub(crate) fn open(&mut self, app: &AppState) {
        let projection = app
            .reasoning_meter
            .projection
            .clone()
            .filter(|projection| projection.available() && projection.matches(app));
        self.close();
        let Some(projection) = projection else {
            return;
        };
        self.selected = projection
            .choices
            .iter()
            .position(|choice| *choice == app.input_reasoning)
            .unwrap_or(0);
        self.projection = Some(projection);
        self.hovered = None;
        self.hits.clear();
        self.open = true;
    }

    fn close(&mut self) {
        self.open = false;
        self.hovered = None;
        self.hits.clear();
        if let Some(projection) = self.projection.take() {
            crate::render_work::discard(projection);
        }
    }

    fn current(&mut self, app: &AppState) -> bool {
        if !self.open
            || !self
                .projection
                .as_ref()
                .is_some_and(|projection| projection.matches(app))
        {
            self.close();
            return false;
        }
        true
    }

    fn rows(&self, width: u16) -> Vec<Vec<usize>> {
        let Some(projection) = &self.projection else {
            return Vec::new();
        };
        let mut modes = Vec::new();
        for wanted in [
            None,
            Some(ReasoningSelection::Auto {
                execution_mode: None,
            }),
            Some(ReasoningSelection::Disabled),
        ] {
            if let Some(index) = projection
                .choices
                .iter()
                .position(|choice| *choice == wanted)
            {
                modes.push(index);
            }
        }
        let extra: Vec<_> = (0..projection.choices.len())
            .filter(|index| !modes.contains(index))
            .collect();
        let mode_columns = (width / 9).clamp(1, 3) as usize;
        let columns = if width >= 18 { 2 } else { 1 };
        let mut rows: Vec<_> = modes.chunks(mode_columns).map(<[usize]>::to_vec).collect();
        if !modes.is_empty() && !extra.is_empty() {
            rows.push(Vec::new());
        }
        rows.extend(extra.chunks(columns).map(<[usize]>::to_vec));
        rows
    }

    pub(crate) fn render(&mut self, frame: &mut Frame, canvas: Rect, app: &mut AppState) {
        if !self.current(app) {
            return;
        }
        let Some(anchor) = app
            .input_controls
            .iter()
            .find(|(action, _)| *action == InputAction::Reasoning)
            .map(|(_, rect)| *rect)
        else {
            self.close();
            return;
        };
        let width = canvas.width.min(31);
        let rows = self.rows(width.saturating_sub(2));
        let rect = floating_rect(
            canvas,
            anchor,
            width,
            rows.len().saturating_add(2).min(u16::MAX as usize) as u16,
        );
        self.rect = rect;
        self.hits.clear();
        if rect.is_empty() {
            return;
        }
        frame.render_widget(Clear, rect);
        frame.render_widget(crate::surface::block(crate::surface::Role::Overlay), rect);
        let inset = u16::from(rect.width >= 3 && rect.height >= 3);
        let content = Rect::new(
            rect.x + inset,
            rect.y + inset,
            rect.width.saturating_sub(inset * 2),
            rect.height.saturating_sub(inset * 2),
        );
        let visible = content.height as usize;
        let selected_row = rows
            .iter()
            .position(|row| row.contains(&self.selected))
            .unwrap_or(0);
        let start = selected_row
            .saturating_add(1)
            .saturating_sub(visible)
            .min(rows.len().saturating_sub(visible));
        let theme = crate::theme::theme();
        let projection = self.projection.as_ref().unwrap();
        for (offset, row) in rows.iter().skip(start).take(visible).enumerate() {
            for (column, index) in row.iter().enumerate() {
                let left = column as u16 * content.width / row.len() as u16;
                let right = (column + 1) as u16 * content.width / row.len() as u16;
                let hit = Rect::new(content.x + left, content.y + offset as u16, right - left, 1);
                let selected = projection.choices[*index] == app.input_reasoning;
                let caption = crate::width::truncate(
                    &label(&projection.choices[*index]),
                    hit.width.saturating_sub(2) as usize,
                );
                crate::settings_form::render_action(
                    frame,
                    hit,
                    &crate::settings_form::Action {
                        label: &caption,
                        focused: selected,
                        hovered: self.hovered == Some(*index) || self.selected == *index,
                        ..Default::default()
                    },
                    &theme,
                );
                self.hits.push((*index, hit));
            }
        }
    }

    fn apply(&mut self, app: &mut AppState, index: usize) {
        if let Some(projection) = &self.projection {
            select(app, projection, index);
        }
        self.close();
    }

    pub(crate) fn key(&mut self, action: &crate::keys::KeyAction, app: &mut AppState) {
        use crate::directional_selector::{SelectorDirection, move_wrapped};
        use crate::keys::KeyAction;
        if !self.current(app) {
            return;
        }
        let rows = self.rows(self.rect.width.saturating_sub(2).max(1));
        let order: Vec<_> = rows.iter().flatten().copied().collect();
        let mut position = order
            .iter()
            .position(|index| *index == self.selected)
            .unwrap_or(0);
        match action {
            KeyAction::Escape | KeyAction::Interrupt => self.close(),
            KeyAction::Submit => self.apply(app, self.hovered.unwrap_or(self.selected)),
            KeyAction::CycleReasoning => {
                app.cycle_input_reasoning();
                app.reasoning_meter.show_feedback();
                self.close();
            }
            KeyAction::HistoryUp
            | KeyAction::HistoryDown
            | KeyAction::PageUp
            | KeyAction::PageDown => {
                let rows: Vec<_> = rows.iter().filter(|row| !row.is_empty()).collect();
                let mut row = rows
                    .iter()
                    .position(|row| row.contains(&self.selected))
                    .unwrap_or(0);
                let column = rows
                    .get(row)
                    .and_then(|row| row.iter().position(|index| *index == self.selected))
                    .unwrap_or(0);
                let direction = if matches!(action, KeyAction::HistoryUp | KeyAction::PageUp) {
                    SelectorDirection::Previous
                } else {
                    SelectorDirection::Next
                };
                if move_wrapped(&mut row, rows.len(), direction) {
                    self.selected = rows[row][column.min(rows[row].len() - 1)];
                }
                self.hovered = None;
            }
            KeyAction::CursorLeft | KeyAction::BackTab => {
                move_wrapped(&mut position, order.len(), SelectorDirection::Previous);
                self.selected = order.get(position).copied().unwrap_or(0);
                self.hovered = None;
            }
            KeyAction::CursorRight | KeyAction::Tab => {
                move_wrapped(&mut position, order.len(), SelectorDirection::Next);
                self.selected = order.get(position).copied().unwrap_or(0);
                self.hovered = None;
            }
            KeyAction::CursorHome | KeyAction::Home => {
                self.selected = order.first().copied().unwrap_or(0);
                self.hovered = None;
            }
            KeyAction::CursorEnd | KeyAction::End => {
                self.selected = order.last().copied().unwrap_or(0);
                self.hovered = None;
            }
            _ => {}
        }
        app.mark_visual_dirty();
    }

    pub(crate) fn mouse(&mut self, event: &crossterm::event::MouseEvent, app: &mut AppState) {
        use crossterm::event::{MouseButton, MouseEventKind};
        if !self.current(app) {
            return;
        }
        let point = (event.column, event.row).into();
        let hit = self
            .hits
            .iter()
            .find(|(_, rect)| rect.contains(point))
            .map(|(index, _)| *index);
        match event.kind {
            MouseEventKind::Moved => {
                self.hovered = hit;
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = hit {
                    self.apply(app, index);
                } else if !self.rect.contains(point) {
                    self.close();
                }
            }
            MouseEventKind::ScrollUp => self.key(&crate::keys::KeyAction::HistoryUp, app),
            MouseEventKind::ScrollDown => self.key(&crate::keys::KeyAction::HistoryDown, app),
            _ => {}
        }
        app.mark_visual_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{UiState, input::InputEditor, keys::KeyAction};
    use atman_runtime::model_registry::{
        AliasEntry, ModelEntry, ModelRegistry, ProviderConfig, ProviderEntry,
    };
    use ratatui::{Terminal, backend::TestBackend};

    fn registry(efforts: Vec<ReasoningEffort>) -> Arc<ModelRegistry> {
        let models = Arc::new(ModelRegistry::new());
        models
            .replace_config(ProviderConfig {
                providers: [(
                    "gateway".into(),
                    ProviderEntry {
                        kind: "openai".into(),
                        ..Default::default()
                    },
                )]
                .into(),
                models: [(
                    "primary".into(),
                    ModelEntry {
                        model: "vendor/reasoning-model".into(),
                        provider: Some("gateway".into()),
                        context_budget: Some(128_000),
                        reasoning: Some("high".into()),
                        reasoning_efforts: efforts,
                        ..Default::default()
                    },
                )]
                .into(),
                aliases: [(
                    "smart".into(),
                    AliasEntry {
                        model: "primary".into(),
                    },
                )]
                .into(),
            })
            .unwrap();
        models
    }

    async fn ready(app: &mut AppState) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while app.settings.is_waiting() {
                app.settings.changed().await;
            }
        })
        .await
        .unwrap();
        assert!(app.settings.failure().is_none());
    }

    async fn app() -> AppState {
        let models = registry(vec![
            ReasoningEffort::Low,
            ReasoningEffort::Medium,
            ReasoningEffort::High,
            ReasoningEffort::XHigh,
        ]);
        let mut app = AppState::default();
        app.settings = crate::settings::SettingsSource::for_models(models);
        ready(&mut app).await;
        app
    }

    #[tokio::test]
    async fn choices_preserve_submission_semantics_and_reject_foreign_or_refreshed_sources() {
        let mut app = app().await;
        app.input = "draft stays here".into();
        let document = app.items.identity();
        let revision = app.items.revision_clock();
        let projection = Projection::capture(&app);
        assert_eq!(projection.stops.len(), 4);
        assert_eq!(projection.fraction, Some(0.75));
        assert_eq!(projection.label, "default · high");
        assert_eq!(projection.marker, None);
        assert!(projection.stops.iter().all(|index| matches!(
            projection.choices[*index],
            Some(ReasoningSelection::Effort { .. })
        )));
        for (index, choice) in projection.choices.iter().enumerate() {
            assert!(select(&mut app, &projection, index));
            assert_eq!(&app.input_reasoning, choice);
            if choice.is_some() {
                assert_eq!(app.input_reasoning_for_submission(), *choice);
            }
        }
        assert!(select(&mut app, &projection, 0));
        assert_eq!(app.input_reasoning, None);
        assert!(matches!(
            app.input_reasoning_for_submission(),
            Some(ReasoningSelection::Effort {
                effort: ReasoningEffort::High,
                ..
            })
        ));
        assert_eq!(app.input, "draft stays here");
        assert_eq!(app.items.identity(), document);
        assert_eq!(app.items.revision_clock(), revision);
        app.settings.invalidate();
        ready(&mut app).await;
        assert!(
            !select(&mut app, &projection, 1),
            "an old painted index cannot select a refreshed model"
        );
        let current = Projection::capture(&app);
        app.items = Default::default();
        assert!(
            !select(&mut app, &current, 1),
            "another document cannot inherit an open control"
        );
        assert_eq!(app.input_reasoning, None);
    }

    #[tokio::test]
    async fn meter_geometry_and_theme_are_independent_of_choice_and_hints() {
        let app = app().await;
        for theme in crate::theme::Theme::test_modes() {
            theme.with(|| {
                for width in [40, 48, 64, 100] {
                    let mut view = AppState::default();
                    view.settings = app.settings.clone();
                    let mut ui = UiState::new(view);
                    let editor = InputEditor::default();
                    let mut terminal = Terminal::new(TestBackend::new(width, 30)).unwrap();
                    let choices = ui.app.input_reasoning_choices();
                    let mut geometry = None;
                    for choice in choices {
                        ui.app.input_reasoning = choice;
                        ui.app.reasoning_meter.hide();
                        terminal
                            .draw(|frame| crate::render::render_frame(frame, &mut ui, &editor))
                            .unwrap();
                        let more = ui
                            .app
                            .input_controls
                            .iter()
                            .find(|(action, _)| *action == InputAction::Reasoning)
                            .map(|(_, rect)| *rect);
                        let Some(more) = more else {
                            assert!(ui.app.input_rect.unwrap().width.saturating_sub(4) < 28);
                            continue;
                        };
                        let model = ui
                            .app
                            .input_controls
                            .iter()
                            .find(|(action, _)| *action == InputAction::Model)
                            .unwrap()
                            .1;
                        assert_eq!(more.width, 3);
                        assert_eq!(model.x, more.x + WIDTH + 1);
                        assert!(more.y == ui.app.input_rect.unwrap().bottom() - 1);
                        assert_eq!(*geometry.get_or_insert((more, model)), (more, model));
                        let projection = Projection::capture(&ui.app);
                        if projection.fraction.is_some() {
                            assert_eq!(
                                terminal.backend().buffer()[(more.x + 3, more.y)].fg,
                                if projection.fraction == Some(1.0) && projection.marker.is_none() {
                                    theme.reasoning_peak[0].into()
                                } else {
                                    theme.accent.into()
                                }
                            );
                        }
                        for (index, (_, hit)) in ui.app.input_controls.iter().enumerate() {
                            for (_, other) in &ui.app.input_controls[index + 1..] {
                                assert!(hit.intersection(*other).is_empty());
                            }
                        }
                    }
                }
            });
        }
    }

    #[tokio::test]
    async fn picker_uses_modal_capture_and_keeps_every_choice_reachable_after_resize() {
        let mut ui = UiState::new(app().await);
        let mut editor = InputEditor::default();
        editor.insert_str("draft i");
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| crate::render::render_frame(frame, &mut ui, &editor))
            .unwrap();
        ui.wm.modals.reasoning_picker.open(&ui.app);
        ui.wm.sync_modals();
        assert_eq!(
            ui.wm.top_kind(),
            Some(crate::wm::ModalKind::ReasoningPicker)
        );
        assert!(ui.wm.dispatch_paste("unrelated paste", &mut ui.app, None));
        assert_eq!(editor.buf(), "draft i");
        let (consumed, commands) = ui.wm.dispatch_key(&KeyAction::Submit, &mut ui.app, None);
        assert!(consumed && commands.is_empty());
        assert_eq!(ui.app.input_reasoning, None);
        for (width, height) in [(1, 1), (12, 3), (30, 8)] {
            ui.wm.modals.reasoning_picker.open(&ui.app);
            let count = ui.app.input_reasoning_choices().len();
            let mut selected = std::collections::HashSet::new();
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            for _ in 0..count {
                let picker = &mut ui.wm.modals.reasoning_picker;
                terminal
                    .draw(|frame| picker.render(frame, frame.area(), &mut ui.app))
                    .unwrap();
                assert!(
                    picker
                        .hits
                        .iter()
                        .all(|(_, hit)| hit.intersection(terminal.backend().buffer().area) == *hit)
                );
                assert!(
                    picker
                        .hits
                        .iter()
                        .any(|(index, _)| *index == picker.selected)
                );
                selected.insert(picker.selected);
                picker.key(&KeyAction::Tab, &mut ui.app);
            }
            assert_eq!(selected.len(), count);
        }
        ui.wm.modals.reasoning_picker.open(&ui.app);
        ui.app.settings.invalidate();
        ready(&mut ui.app).await;
        ui.wm
            .modals
            .reasoning_picker
            .key(&KeyAction::Submit, &mut ui.app);
        assert!(!ui.wm.modals.reasoning_picker.open);
        assert_eq!(ui.app.input_reasoning, None);
    }

    #[tokio::test]
    async fn meter_motion_and_feedback_finish_on_their_installed_frame() {
        let mut app = app().await;
        app.input_reasoning = Some(ReasoningSelection::Auto {
            execution_mode: None,
        });
        let mut meter = Meter::default();
        let start = Instant::now();
        meter.observe(Projection::capture(&app), start);
        assert!(!meter.motion_pending() && !meter.feedback_pending());
        app.input_reasoning = Some(ReasoningSelection::Effort {
            effort: ReasoningEffort::High,
            execution_mode: None,
        });
        meter.observe(Projection::capture(&app), start);
        assert!(meter.motion_pending() && meter.feedback_pending());
        let motion = meter.motion.as_ref().unwrap();
        assert_eq!(motion.value(start), 0.0);
        let middle = motion.value(start + MOTION / 2);
        assert!(middle > 0.0 && middle < motion.to);
        meter.observe(Projection::capture(&app), start + FEEDBACK);
        assert!(!meter.motion_pending() && !meter.feedback_pending());
        app.input_reasoning = Some(ReasoningSelection::Disabled);
        meter.observe(Projection::capture(&app), start + FEEDBACK);
        assert!(!meter.motion_pending());
        assert_eq!(meter.projection.as_ref().unwrap().marker, Some("⌀"));
        meter.hide();
        assert!(!meter.feedback_pending());
        assert_eq!(app.input_reasoning, Some(ReasoningSelection::Disabled));
    }
}
