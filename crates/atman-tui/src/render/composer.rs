//! Geometry shared by the outer compositor, painting and input hit testing.
use super::*;
use ratatui::layout::Rect;

#[derive(Clone, Debug)]
pub(crate) struct ComposerLayout {
    pub input_rect: Rect,
    pub outer: Rect,
    occlusion: Rect,
    attachments_rect: Option<Rect>,
    quote_rect: Option<Rect>,
    approvals_rect: Option<Rect>,
    submission_queue_rect: Option<Rect>,
    injections_rect: Option<Rect>,
    quote_lines: Option<Vec<String>>,
    scroll_row: u32,
}

impl ComposerLayout {
    pub fn for_view(app: &crate::app::AppState, editor: &InputEditor, bounds: Rect) -> Self {
        let bottom = Self::bottom(app, editor, bounds);
        let input = if let Some(card) = app.startup_card.as_ref() {
            crate::output::compute_startup_overlay(bounds, &card.recent).input_slot
        } else if let Some(intro) = app.startup_intro.as_ref() {
            let origin = crate::output::compute_startup_overlay(bounds, &intro.recent).input_slot;
            let progress = (intro.started_at.elapsed().as_millis().min(STARTUP_SLIDE_MS) as f32)
                / STARTUP_SLIDE_MS as f32;
            let mix = |a: u16, b: u16| {
                (f32::from(a) + (f32::from(b) - f32::from(a)) * ease_out(progress)).round() as u16
            };
            Rect::new(
                mix(origin.x, bottom.input_rect.x),
                mix(origin.y, bottom.input_rect.y),
                mix(origin.width, bottom.input_rect.width),
                mix(origin.height, bottom.input_rect.height),
            )
        } else {
            return bottom;
        };
        Self::at(app, editor, bounds, input)
    }

    pub fn bottom(app: &crate::app::AppState, editor: &InputEditor, bounds: Rect) -> Self {
        let width = layout::input_content_width(bounds.width);
        let rows = crate::input::visual_line_count(editor.buf(), width).min(12) as u16;
        Self::at(
            app,
            editor,
            bounds,
            layout::compute_input_rect(bounds, rows),
        )
    }

    pub fn at(
        app: &crate::app::AppState,
        editor: &InputEditor,
        bounds: Rect,
        input_rect: Rect,
    ) -> Self {
        let standalone_canonical = app
            .pending_permissions
            .keys()
            .filter(|id| !app.grouped_permission_request_ids.contains(*id))
            .count();
        let pending_count = app.pending_permissions.len();
        let visible_rows = standalone_canonical;
        let approvals_rows: u16 = if pending_count == 0 {
            0
        } else {
            let items = visible_rows.min(9) as u16;
            let overflow = if visible_rows > 9 { 1 } else { 0 };
            let groups = app.pending_permission_groups.len() as u16;
            let expanded_members = app
                .pending_permission_groups
                .values()
                .filter(|group| group.expanded)
                .map(|group| group.payload.request_ids.len() as u16)
                .sum::<u16>();
            items + overflow + groups + expanded_members + 2
        };
        let injection_rows: u16 = if app.pending_injections.is_empty() {
            0
        } else {
            // title + N items + 2 for block borders
            (app.pending_injections.len() as u16).min(5) + 3
        };
        let attachment_rows: u16 = if editor.pending_images().is_empty() {
            0
        } else {
            let visible = editor.pending_images().len().min(4) as u16;
            let overflow = u16::from(editor.pending_images().len() > 4);
            visible + overflow + 2
        };
        let quote_lines = app
            .pending_quote
            .as_deref()
            .map(|text| quote_visual_lines(text, layout::stacked_rect_width(input_rect.width)));
        let quote_rows: u16 = quote_lines.as_ref().map_or(0, |lines| {
            let total = lines.len();
            let visible = total.min(if app.quote_expanded { 8 } else { 3 });
            visible as u16 + 2 + u16::from(total > visible)
        });
        let content_w = (input_rect.width.saturating_sub(layout::INPUT_H_OVERHEAD)) as usize;
        let cursor_row =
            crate::input::wrapped_cursor_row(editor.buf(), editor.cursor(), content_w) as u32;
        let visible_rows = u32::from(layout::input_editor_rect(input_rect).height.max(1));
        let scroll_row = cursor_row.saturating_sub(visible_rows.saturating_sub(1));
        let attachments_rect =
            layout::compute_stacked_rect(bounds, input_rect, None, attachment_rows);
        let quote_rect =
            layout::compute_stacked_rect(bounds, input_rect, attachments_rect, quote_rows);
        let approvals_rect = layout::compute_stacked_rect(
            bounds,
            input_rect,
            quote_rect.or(attachments_rect),
            approvals_rows,
        );
        let submission_rows = submission_queue::height(
            &app.queued_submissions,
            app.queued_submission_edit.is_some(),
            app.queue_page().is_some_and(|page| page.paged()),
            layout::stacked_rect_width(input_rect.width),
        );
        let submission_queue_rect = layout::compute_stacked_rect(
            bounds,
            input_rect,
            approvals_rect.or(quote_rect).or(attachments_rect),
            submission_rows,
        );
        let injections_rect = (injection_rows > 0)
            .then(|| {
                layout::compute_injection_rect(
                    bounds,
                    input_rect,
                    submission_queue_rect
                        .or(approvals_rect)
                        .or(quote_rect)
                        .or(attachments_rect),
                    injection_rows,
                )
            })
            .flatten();

        let top = [
            attachments_rect,
            quote_rect,
            approvals_rect,
            submission_queue_rect,
            injections_rect,
        ]
        .into_iter()
        .flatten()
        .fold(input_rect.y, |top, rect| top.min(rect.y));
        let outer = Rect::new(
            input_rect.x,
            top,
            input_rect.width,
            input_rect.bottom().saturating_sub(top),
        );
        Self {
            input_rect,
            outer,
            occlusion: outer,
            attachments_rect,
            quote_rect,
            approvals_rect,
            submission_queue_rect,
            injections_rect,
            quote_lines,
            scroll_row,
        }
    }

    pub fn move_to(&mut self, x: u16, y: u16) {
        let dx = i32::from(x) - i32::from(self.outer.x);
        let dy = i32::from(y) - i32::from(self.outer.y);
        let shift = |rect: &mut Rect| {
            rect.x = (i32::from(rect.x) + dx) as u16;
            rect.y = (i32::from(rect.y) + dy) as u16;
        };
        shift(&mut self.outer);
        self.occlusion = self.outer;
        shift(&mut self.input_rect);
        for rect in [
            &mut self.attachments_rect,
            &mut self.quote_rect,
            &mut self.approvals_rect,
            &mut self.submission_queue_rect,
            &mut self.injections_rect,
        ]
        .into_iter()
        .flatten()
        {
            shift(rect);
        }
    }

    pub fn move_visual_to(&mut self, x: u16, y: u16) {
        let occlusion = self.occlusion;
        self.move_to(x, y);
        self.occlusion = occlusion;
    }

    pub fn apply(&self, app: &mut crate::app::AppState) {
        app.composer_rect = Some(self.outer);
        app.input_rect = Some(self.input_rect);
        app.input_scroll_row = self.scroll_row;
        app.quote_rect = self.quote_rect;
        app.submission_queue_rect = self.submission_queue_rect;
        if app.pending_quote.is_none() {
            app.quote_close_hovered = false;
        }
    }

    pub fn occluded_rows(&self, transcript: Rect) -> u32 {
        let overlap = self.occlusion.intersection(transcript);
        if overlap.is_empty() {
            0
        } else {
            u32::from(transcript.bottom().saturating_sub(overlap.y))
        }
    }
}

pub(crate) fn render_composer(
    f: &mut ratatui::Frame,
    ui: &mut UiState,
    editor: &InputEditor,
    geometry: &ComposerLayout,
) {
    let app = &mut ui.app;
    geometry.apply(app);
    let ComposerLayout {
        input_rect,
        attachments_rect,
        quote_rect,
        approvals_rect,
        submission_queue_rect,
        injections_rect,
        scroll_row,
        ..
    } = *geometry;
    let quote_lines = &geometry.quote_lines;
    let content_w = input_rect.width.saturating_sub(layout::INPUT_H_OVERHEAD) as usize;
    sanitize_widget_edges(f, geometry.outer);
    f.render_widget(ratatui::widgets::Clear, geometry.outer);
    let title = app
        .items
        .snapshot()
        .map(|s| s.display_title.as_str())
        .or(app.session_name.as_deref())
        .unwrap_or("Main");
    let title = crate::width::truncate(title, input_rect.width.saturating_sub(6) as usize);
    if let Some(area) = attachments_rect {
        sanitize_widget_edges(f, area);
        f.render_widget(ratatui::widgets::Clear, area);
        let theme = crate::theme::theme();
        let mut lines: Vec<ratatui::text::Line<'_>> = editor
            .pending_images()
            .iter()
            .take(4)
            .map(|image| {
                ratatui::text::Line::from(vec![
                    ratatui::text::Span::styled(
                        format!(" {} ", image.marker),
                        ratatui::style::Style::default().fg(theme.accent.into()),
                    ),
                    ratatui::text::Span::raw(crate::width::truncate(
                        &image.name,
                        area.width.saturating_sub(16) as usize,
                    )),
                ])
            })
            .collect();
        if editor.pending_images().len() > 4 {
            lines.push(ratatui::text::Line::from(ratatui::text::Span::styled(
                format!(" +{} more", editor.pending_images().len() - 4),
                ratatui::style::Style::default().fg(theme.subtle_fg.into()),
            )));
        }
        let block = crate::surface::block(crate::surface::Role::Context)
            .padding(ratatui::widgets::Padding::new(1, 1, 0, 1))
            .title(format!(" images · {} ", editor.pending_images().len()));
        f.render_widget(ratatui::widgets::Paragraph::new(lines).block(block), area);
    }
    if let (Some(area), Some(wrapped)) = (quote_rect, quote_lines.as_ref()) {
        sanitize_widget_edges(f, area);
        f.render_widget(ratatui::widgets::Clear, area);
        let theme = crate::theme::theme();
        let total = wrapped.len();
        let content_rows = area.height.saturating_sub(2) as usize;
        let visible = quote_card_text_rows(total, area.height, app.quote_expanded);
        let disclosure = usize::from(total > visible && content_rows > visible);
        let text_rows = visible;
        let max_scroll = total.saturating_sub(text_rows);
        app.quote_scroll = app.quote_scroll.min(max_scroll);
        let start = if app.quote_expanded {
            app.quote_scroll
        } else {
            0
        };
        let mut lines = wrapped
            .iter()
            .skip(start)
            .take(text_rows)
            .map(|line| {
                ratatui::text::Line::from(ratatui::text::Span::styled(
                    format!("  {line}"),
                    ratatui::style::Style::default().fg(theme.subtle_fg.into()),
                ))
            })
            .collect::<Vec<_>>();
        if disclosure > 0 {
            lines.push(ratatui::text::Line::from(ratatui::text::Span::styled(
                if app.quote_expanded {
                    format!("  {}–{} / {total} · scroll", start + 1, start + text_rows)
                } else {
                    format!("  +{} more · click or F4 to expand", total - text_rows)
                },
                ratatui::style::Style::default().fg(theme.subtle_fg.into()),
            )));
        }
        let block = crate::surface::block(crate::surface::Role::Context)
            .padding(ratatui::widgets::Padding::new(1, 1, 0, 1))
            .title(format!(
                " QUOTE · {total} rows · F4 {}{} · Ctrl+U remove ",
                if app.quote_expanded {
                    "collapse"
                } else {
                    "expand"
                },
                if app.quote_expanded {
                    " · PgUp/PgDn scroll"
                } else {
                    ""
                }
            ));
        f.render_widget(ratatui::widgets::Paragraph::new(lines).block(block), area);
    }
    if let Some(area) = approvals_rect {
        sanitize_widget_edges(f, area);
        f.render_widget(ratatui::widgets::Clear, area);
        app.approval_hitmap = approval_bar::render(
            f,
            area,
            &app.pending_permissions,
            &app.pending_permission_groups,
            &app.grouped_permission_request_ids,
            app.selected_permission_group.as_ref(),
        );
    } else {
        app.approval_hitmap = approval_bar::ApprovalHitMap::default();
    }
    if let Some(area) = submission_queue_rect {
        sanitize_widget_edges(f, area);
        f.render_widget(ratatui::widgets::Clear, area);
        app.submission_queue_hitmap = submission_queue::render(
            f,
            area,
            &app.queued_submissions,
            submission_queue::QueueRenderState {
                recovery: app.queue_recovery_view(),
                page: app.queue_page(),
                selected: app.selected_submission,
                focused: app.submission_focus,
                hovered: app.hovered_submission,
                hovered_action: app.hovered_submission_action,
                edit: app.queued_submission_edit.as_ref(),
                active_turn: app.queue_active_turn(),
                saving: app.queue_saving(),
            },
        );
    } else {
        app.submission_queue_hitmap = submission_queue::QueueHitMap::default();
    }
    // Render injection queue above approvals bar / input box.
    if let Some(area) = injections_rect {
        sanitize_widget_edges(f, area);
        f.render_widget(ratatui::widgets::Clear, area);
        let lines = output::render_injection_queue(&app.pending_injections, area.width);
        let block = crate::surface::block(crate::surface::Role::Context)
            .padding(ratatui::widgets::Padding::uniform(1));
        let para = ratatui::widgets::Paragraph::new(lines).block(block);
        f.render_widget(para, area);
    }
    sanitize_widget_edges(f, input_rect);
    f.render_widget(ratatui::widgets::Clear, input_rect);
    let target_border = if app.streaming {
        crate::theme::theme().subtle_fg.into()
    } else {
        app.trust.display().color.ratatui()
    };
    if app.streaming != app.was_streaming {
        app.was_streaming = app.streaming;
        app.border_fade_at = Some(std::time::Instant::now());
    }
    let border_color = if let Some(start) = app.border_fade_at {
        let elapsed = start.elapsed().as_secs_f64();
        let prev = if app.streaming {
            app.trust.display().color.ratatui()
        } else {
            crate::theme::theme().subtle_fg.into()
        };
        if elapsed >= 0.35 {
            app.border_fade_at = None;
            target_border
        } else {
            lerp_rgb(prev, target_border, (elapsed / 0.35).clamp(0.0, 1.0))
        }
    } else {
        target_border
    };
    let status = if app.pending_submission.is_some() {
        " Sending · draft retained ".to_owned()
    } else if app.items.is_archive() {
        " History · send to continue ".to_owned()
    } else if let Some(quote) = app.pending_quote.as_ref().filter(|_| quote_rect.is_none()) {
        format!(" quote · {} lines ", quote.lines().count())
    } else {
        String::new()
    };
    f.render_widget(
        input_paragraph(
            editor.buf(),
            border_color,
            app.pending_below_rows().min(u16::MAX as u32) as u16,
            scroll_row.min(u16::MAX as u32) as u16,
            &title,
            &status,
        ),
        input_rect,
    );
    if app.pending_quote.is_some()
        && let Some(close) =
            crate::selection_menu::quote_close_rect(quote_rect.unwrap_or(input_rect))
    {
        f.render_widget(
            ratatui::widgets::Paragraph::new("[x]").style(
                ratatui::style::Style::default()
                    .fg(crate::theme::theme().subtle_fg.into())
                    .bg(if app.quote_close_hovered {
                        crate::theme::theme().work_hover_bg.into()
                    } else {
                        ratatui::style::Color::Reset
                    }),
            ),
            close,
        );
    }
    f.render_widget(
        ratatui::widgets::Paragraph::new(ratatui::text::Line::from(ratatui::text::Span::styled(
            "❯",
            if app.streaming {
                ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::DIM)
            } else {
                ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::BOLD)
            },
        ))),
        ratatui::layout::Rect {
            x: input_rect.x + layout::INPUT_BORDER + layout::INPUT_PAD,
            y: input_rect.y + 1,
            width: 2,
            height: 1,
        },
    );
    let raw_row = crate::input::wrapped_cursor_row(editor.buf(), editor.cursor(), content_w) as u16;
    let raw_col = crate::input::wrapped_cursor_col(editor.buf(), editor.cursor(), content_w) as u16;
    if app.submission_focus {
        if let (Some(_), Some((x, y))) = (
            app.queued_submission_edit.as_ref(),
            app.submission_queue_hitmap.edit_origin,
        ) {
            f.set_cursor_position((x, y));
        }
    } else if app.startup_intro.is_none()
        && (app.startup_card.is_none() || app.startup_focus == crate::app::StartupFocus::Input)
        && !ui.wm.modals.onboarding_open
        && !ui.wm.modals.provider_manager.open
        && ui.wm.focused_id().is_none()
    {
        let editing = layout::input_editor_rect(input_rect);
        let inner_x = editing.x;
        let inner_y = editing.y;
        let mut placed = false;
        if raw_row as u32 >= scroll_row {
            let cy = inner_y + (raw_row as u32 - scroll_row) as u16;
            let cx = inner_x + raw_col;
            if cy < editing.bottom() && cx < editing.right() {
                f.set_cursor_position((cx, cy));
                placed = true;
            }
        }
        if !placed {
            // Fall back to the ❯ prompt only when the editor is empty
            // (e.g. just after submit).  When the user is typing the
            // primary cursor-position logic works and must win.
            if editor.buf().is_empty() {
                f.set_cursor_position((input_rect.x + layout::INPUT_LEFT, input_rect.y + 1));
            }
        }
    }
    if app.popup.is_open() {
        completion::render_popup(f, input_rect, &app.popup);
    }
    render_pulse_bar(
        f,
        input_rect,
        app.tick,
        app.has_running_workflow(),
        border_color,
    );
    render_controls(f, ui, editor, input_rect, &status);
}

/// Labels and hitboxes come from one projection, including CJK and themed names.
fn render_controls(
    f: &mut ratatui::Frame,
    ui: &mut UiState,
    editor: &InputEditor,
    input: Rect,
    status: &str,
) {
    use crate::input::InputAction;
    let app = &mut ui.app;
    app.input_controls.clear();
    let inset = layout::INPUT_BORDER + layout::INPUT_PAD;
    let area = Rect::new(
        input.x.saturating_add(inset),
        input.bottom().saturating_sub(2),
        input.width.saturating_sub(inset * 2),
        u16::from(input.height >= layout::INPUT_V_OVERHEAD),
    );
    if area.width < 20 || area.height == 0 {
        return;
    }
    let theme = crate::theme::theme();
    let history = app.items.is_archive();
    let models = if history { None } else { app.model_registry() };
    let model = models.and_then(|models| {
        models
            .model_entry("smart")
            .filter(|entry| {
                entry.enabled != Some(false)
                    && entry
                        .provider
                        .as_deref()
                        .is_some_and(|provider| !provider.is_empty())
            })
            .map(|_| models.resolve_alias("smart"))
    });
    let reasoning = if history {
        None
    } else {
        app.effective_input_reasoning_badge()
    };
    let mode = app.trust.display();
    let labels = [
        model
            .clone()
            .unwrap_or_else(|| if history { "on continue" } else { "set model" }.into()),
        reasoning
            .clone()
            .unwrap_or_else(|| if model.is_some() { "default" } else { "—" }.into()),
        if history {
            "on continue".into()
        } else {
            format!("{} {}", mode.emoji, mode.name)
        },
    ];
    let mut widths = labels
        .each_ref()
        .map(|label| crate::width::width(label).min(26) as u16);
    let budget = area.width.saturating_sub(17);
    while widths.iter().sum::<u16>() > budget {
        let index = (0..3).max_by_key(|&index| widths[index]).unwrap();
        widths[index] = widths[index].saturating_sub(1);
    }
    let send_enabled = app.pending_submission.is_none()
        && (!editor.buf().trim().is_empty() || !editor.pending_images().is_empty());
    let mut tooltip = None;
    let mut paint = |action, rect: Rect, label: &str, hint: String, enabled: bool| {
        app.input_controls.push((action, rect));
        let hovered = app.input_hover == Some(action);
        let foreground = if action == InputAction::Send && enabled {
            theme.accent.into()
        } else if action == InputAction::Trust && !history {
            mode.color.ratatui()
        } else {
            theme.subtle_fg.into()
        };
        let mut style = Style::default().fg(foreground).bg(if hovered && enabled {
            theme.work_hover_bg.into()
        } else {
            Color::Reset
        });
        if !enabled {
            style = style.add_modifier(ratatui::style::Modifier::DIM);
        }
        f.render_widget(Paragraph::new(label.to_owned()).style(style), rect);
        if hovered {
            tooltip = Some(hint);
        }
    };
    paint(
        InputAction::Attach,
        Rect::new(area.x, area.y, 3, 1),
        " + ",
        "Paste image · Ctrl+V · use :attach <path> for a file".into(),
        app.session.is_some(),
    );
    let mut x = area.x + 3;
    for (index, (action, hint, enabled)) in [
        (
            InputAction::Model,
            format!("Model: {} · select model in this view", labels[0]),
            !history,
        ),
        (
            InputAction::Reasoning,
            format!(
                "Reasoning: {} · Ctrl+T to cycle · this submission only",
                reasoning.as_deref().unwrap_or("model default")
            ),
            model.is_some() && !history,
        ),
        (
            InputAction::Trust,
            format!("Permission: {} · {}", mode.name, mode.description),
            !history,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let label = format!(
            " {} ",
            crate::width::truncate(&labels[index], widths[index] as usize)
        );
        let rect = Rect::new(x, area.y, widths[index] + 2, 1);
        paint(action, rect, &label, hint, enabled);
        x = rect.right();
    }
    paint(
        InputAction::Newline,
        Rect::new(area.right() - 7, area.y, 3, 1),
        " ↵ ",
        "Insert newline · Shift+Enter / Ctrl+J".into(),
        true,
    );
    paint(
        InputAction::Send,
        Rect::new(area.right() - 3, area.y, 3, 1),
        " ↑ ",
        if send_enabled {
            "Send · Enter"
        } else {
            "Enter a message or attach an image to send"
        }
        .into(),
        send_enabled,
    );
    if !history && app.trust.mode == atman_runtime::trust::TrustMode::Eager {
        let escalation = app.trust.theme.escalation_display(app.trust.escalation);
        let available = input
            .width
            .saturating_sub(6 + crate::width::width(status) as u16);
        let label = crate::width::truncate(
            &format!(" {} {} ", escalation.emoji, escalation.name),
            available as usize,
        );
        let rect = Rect::new(
            input.x + 3,
            input.bottom() - 1,
            crate::width::width(&label) as u16,
            1,
        );
        paint(
            InputAction::Escalation,
            rect,
            &label,
            format!("Escalation: {} · Tab to cycle", escalation.name),
            true,
        );
    }
    if let Some(tooltip) = tooltip.filter(|_| !ui.wm.modals.any_open()) {
        let area = Rect::new(input.x, input.bottom(), input.width, 1).intersection(f.area());
        f.render_widget(ratatui::widgets::Clear, area);
        f.render_widget(
            Paragraph::new(crate::width::truncate(&tooltip, area.width as usize))
                .style(Style::default().fg(theme.subtle_fg.into()).bg(Color::Reset)),
            area,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_labels_follow_the_bound_flow_without_requiring_usage_metadata() {
        use atman_runtime::flow_store::{FlowInstanceId, FlowRef};
        use std::sync::Arc;
        for name in ["first-model", "second-model"] {
            let models = Arc::new(atman_runtime::model_registry::ModelRegistry::new());
            models.replace_config_text(&format!(
                "[providers.fixture]\nkind = 'openai'\n[models.{name}]\nprovider = 'fixture'\nmodel = 'vendor/model'\n[alias.smart]\nmodel = '{name}'\n"
            )).unwrap();
            let session = Arc::new(atman_runtime::Session::open_instance_context(FlowRef {
                session_id: uuid::Uuid::now_v7().into(),
                instance_id: FlowInstanceId::now(),
            }));
            session.bind_model_registry(models).unwrap();
            let mut ui = UiState::new(crate::app::AppState::default().with_session(Some(session)));
            let editor = InputEditor::default();
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 30)).unwrap();
            terminal
                .draw(|frame| {
                    let geometry = ComposerLayout::bottom(&ui.app, &editor, frame.area());
                    render_composer(frame, &mut ui, &editor, &geometry);
                })
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains(name));
            assert!(!text.contains("set model"));
        }
    }

    #[test]
    fn outline_and_controls_share_non_overlapping_geometry_in_every_mode_theme() {
        use atman_runtime::trust::{Theme, TrustMode};
        for width in [40, 64, 100, 160] {
            for theme in [
                Theme::Default,
                Theme::Wuxia,
                Theme::Animal,
                Theme::Weather,
                Theme::Drink,
            ] {
                for mode in TrustMode::all() {
                    let mut ui = UiState::new(crate::app::AppState::default());
                    ui.app.trust.theme = theme;
                    ui.app.trust.mode = mode;
                    let editor = InputEditor::default();
                    let geometry =
                        ComposerLayout::bottom(&ui.app, &editor, Rect::new(0, 0, width, 30));
                    let mut terminal =
                        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 30))
                            .unwrap();
                    terminal
                        .draw(|frame| {
                            frame.render_widget(
                                Paragraph::new("X".repeat(width as usize * 30))
                                    .style(Style::default().bg(Color::Red)),
                                frame.area(),
                            );
                            render_composer(frame, &mut ui, &editor, &geometry);
                        })
                        .unwrap();
                    let editing = layout::input_editor_rect(geometry.input_rect);
                    assert_eq!(editing.height, 2);
                    assert_eq!(geometry.outer, geometry.input_rect);
                    let controls = &ui.app.input_controls;
                    assert!(
                        controls
                            .iter()
                            .any(|(action, _)| *action == crate::input::InputAction::Send)
                    );
                    for (index, (_, rect)) in controls.iter().enumerate() {
                        assert_eq!(rect.intersection(geometry.outer), *rect);
                        assert!(rect.intersection(editing).is_empty());
                        for (_, other) in &controls[index + 1..] {
                            assert!(rect.intersection(*other).is_empty());
                        }
                    }
                    let buffer = terminal.backend().buffer();
                    assert_eq!(
                        buffer[(geometry.input_rect.x, geometry.input_rect.y)].symbol(),
                        "┌"
                    );
                    assert_eq!(
                        buffer[(geometry.input_rect.right() - 1, geometry.input_rect.y)].symbol(),
                        "┐"
                    );
                    for y in geometry.outer.y..geometry.outer.bottom() {
                        for x in geometry.outer.x..geometry.outer.right() {
                            assert_eq!(buffer[(x, y)].bg, Color::Reset);
                            assert_ne!(buffer[(x, y)].symbol(), "X");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn translated_composer_keeps_cursor_and_context_in_the_same_surface() {
        for width in [40, 64, 100, 160] {
            let mut ui = UiState::new(crate::app::AppState::default());
            ui.app.pending_quote = Some("Quoted context\n第二行".into());
            let mut editor = InputEditor::default();
            editor.insert_str("中文 editing\nlast line");
            let bounds = Rect::new(0, 0, width, 50);
            let mut geometry = ComposerLayout::bottom(&ui.app, &editor, bounds);
            geometry.move_to(1, 3);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, 50)).unwrap();
            terminal
                .draw(|frame| render_composer(frame, &mut ui, &editor, &geometry))
                .unwrap();
            let cursor = terminal.backend().cursor_position();
            assert!(geometry.input_rect.contains(cursor));
            assert_eq!(cursor.y, geometry.input_rect.y + 2);
            assert_eq!(cursor.x, geometry.input_rect.x + layout::INPUT_LEFT + 9);
            assert_eq!(ui.app.input_rect, Some(geometry.input_rect));
            assert_eq!(ui.app.quote_rect, geometry.quote_rect);
            assert_eq!(ui.app.composer_rect, Some(geometry.outer));
            assert_eq!(geometry.occluded_rows(Rect::new(0, 0, width, 3)), 0);
            for rect in [geometry.quote_rect, Some(geometry.input_rect)]
                .into_iter()
                .flatten()
            {
                assert_eq!(geometry.outer.intersection(rect), rect);
            }
        }
    }
}
