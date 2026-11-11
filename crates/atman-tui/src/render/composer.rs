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
    pub(crate) fn hide(app: &mut crate::app::AppState) {
        app.input_rect = None;
        app.composer_rect = None;
        app.quote_rect = None;
        app.quote_close_hovered = false;
        app.submission_queue_rect = None;
        app.submission_queue_hitmap = Default::default();
        app.approval_hitmap = Default::default();
        app.input_controls.clear();
        app.input_hover = None;
        app.reasoning_meter.hide();
        app.hovered_submission = None;
        app.hovered_submission_action = None;
    }
    pub fn for_view(app: &crate::app::AppState, editor: &InputEditor, bounds: Rect) -> Self {
        let bottom = Self::bottom(app, editor, bounds);
        let input = if let Some(card) = app.startup_card.as_ref() {
            crate::output::compute_startup_overlay(bounds, &card.recent).input_slot
        } else if let Some(intro) = app.startup_intro.as_ref() {
            let origin = crate::output::compute_startup_overlay(bounds, &intro.recent).input_slot;
            let progress = (intro.started_at.elapsed().as_millis().min(STARTUP_SLIDE_MS) as f32)
                / STARTUP_SLIDE_MS as f32;
            let mix = |a: u16, b: u16| {
                (f32::from(a) + (f32::from(b) - f32::from(a)) * spring_progress(progress)).round()
                    as u16
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
        let approvals_rows =
            approval_bar::height(app, layout::stacked_rect_width(input_rect.width));
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
            crate::input::wrapped_cursor_position(editor.buf(), editor.cursor(), content_w).0
                as u32;
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

    /// Reflow the animated editor and controls while retaining the reading anchor.
    pub fn project_visual(
        &self,
        app: &crate::app::AppState,
        editor: &InputEditor,
        bounds: Rect,
        input_rect: Rect,
    ) -> Self {
        let mut projected = Self::at(app, editor, bounds, input_rect);
        projected.occlusion = self.occlusion;
        projected
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
    if !ui.composer_visible() {
        ComposerLayout::hide(&mut ui.app);
        return;
    }
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
        app.approval_hitmap = approval_bar::render(f, area, app);
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
    } else if editor.has_preparing_images() {
        format!(" {} Image ", crate::loading::glyph(app.animation_frame))
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
            0,
            scroll_row.min(u16::MAX as u32) as u16,
            "",
            "",
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
    let (raw_row, raw_col) =
        crate::input::wrapped_cursor_position(editor.buf(), editor.cursor(), content_w);
    let raw_row = raw_row as u16;
    let raw_col = raw_col as u16;
    if app.submission_focus && ui.wm.focused_id().is_none() {
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
            // Fall back to the editor origin only when the editor is empty
            // (e.g. just after submit).  When the user is typing the
            // primary cursor-position logic works and must win.
            if editor.buf().is_empty() {
                f.set_cursor_position((input_rect.x + layout::INPUT_LEFT, input_rect.y + 1));
            }
        }
    }
    if app.popup.is_open() {
        completion::render_popup(f, input_rect, &app.popup, app.animation_frame);
    }
    render_pulse_bar(
        f,
        input_rect,
        app.tick,
        app.has_running_workflow(),
        border_color,
    );
    render_controls(f, ui, editor, input_rect, &title, &status);
}

/// Labels and hitboxes come from one projection, including CJK and themed names.
fn render_controls(
    f: &mut ratatui::Frame,
    ui: &mut UiState,
    editor: &InputEditor,
    input: Rect,
    title: &str,
    status: &str,
) {
    use crate::input::InputAction;
    let app = &mut ui.app;
    app.input_controls.clear();
    let inset = layout::INPUT_BORDER + 1;
    let area = Rect::new(
        input.x.saturating_add(inset),
        input.bottom().saturating_sub(1),
        input.width.saturating_sub(inset * 2),
        u16::from(input.height >= layout::INPUT_V_OVERHEAD),
    );
    if area.height == 0 {
        return;
    }
    let theme = crate::theme::theme();
    let top = Rect::new(area.x, input.y, area.width, 1);
    let mut heading_right = top.right();
    let fallback_quote = app.pending_quote.is_some() && app.quote_rect.is_none();
    if fallback_quote {
        heading_right = heading_right.saturating_sub(4).max(top.x);
    }
    let history = app.items.is_archive();
    let model = app.input_model().map(|(models, name)| {
        let hint = models.model_entry(&app.input_model_key()).map_or_else(
            || name.clone(),
            |entry| {
                format!(
                    "{name} · {} / {}",
                    entry.provider.as_deref().unwrap_or(""),
                    entry.model
                )
            },
        );
        let short = name
            .rsplit('/')
            .find(|part| !part.is_empty())
            .unwrap_or(&name)
            .to_owned();
        (short, hint)
    });
    let mode = app.trust.display();
    let escalation = app.trust.theme.escalation_display(app.trust.escalation);
    let model_label = model
        .as_ref()
        .map(|(short, _)| short.clone())
        .unwrap_or_else(|| {
            if history {
                "on continue"
            } else if app.settings.is_waiting() {
                "Loading"
            } else if app.settings.failure().is_some() {
                "Unavailable"
            } else {
                "set model"
            }
            .into()
        });
    let meter =
        (area.width >= 28).then(|| Rect::new(area.x, area.y, crate::input_reasoning::WIDTH, 1));
    let reserved = meter.map_or(0, |_| crate::input_reasoning::WIDTH + 1);
    let model_width = (crate::width::width(&model_label).min(26) as u16)
        .min(area.width.saturating_sub(10 + reserved));
    let send_enabled = app.pending_submission.is_none()
        && app.question.as_ref().is_none_or(|question| {
            question.pending().is_none() && !question.retention_pending() && !question.retrying()
        })
        && !editor.has_preparing_images()
        && !app.settings.is_waiting()
        && app.settings.failure().is_none()
        && (!editor.buf().trim().is_empty() || !editor.pending_images().is_empty());
    let pending = app.pending_below_rows();
    let console_open = ui.wm.modals.input_console.open;
    if let Some(rect) = meter {
        crate::input_reasoning::render_meter(f, app, rect);
    } else {
        app.reasoning_meter.hide();
    }
    let mut tooltip = None;
    let ask_label = if app.question_open.is_some() {
        format!(" {} ", crate::loading::glyph(app.animation_frame))
    } else {
        " ? ".into()
    };
    let mut policy_separator = None;
    let mut paint = |action, rect: Rect, label: &str, hint: String, enabled: bool| {
        if rect.is_empty() {
            return;
        }
        app.input_controls.push((action, rect));
        let hovered = app.input_hover == Some(action);
        let foreground = if action == InputAction::Send && enabled {
            theme.accent.into()
        } else if action == InputAction::Trust && !history {
            theme.permission_color(mode.color)
        } else if action == InputAction::Escalation && !history {
            theme.permission_color(escalation.color)
        } else if action == InputAction::Model && enabled {
            theme.work_action_fg.into()
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
    if top.width >= 20 {
        let attach = Rect::new(heading_right - 3, top.y, 3, 1);
        paint(
            InputAction::Attach,
            attach,
            if console_open { " × " } else { " + " },
            if console_open {
                "Return to draft · Esc".into()
            } else {
                "Paste image · Ctrl+V · use :attach <path> for a file".into()
            },
            console_open || app.session.is_some(),
        );
        heading_right = attach.x.saturating_sub(1);
        if heading_right >= top.x + 12 {
            let ask = Rect::new(heading_right - 3, top.y, 3, 1);
            paint(
                InputAction::Ask,
                ask,
                &ask_label,
                "Quick Ask · ask beside this conversation".into(),
                !history && (app.items.flow().is_some() || app.question.is_some()),
            );
            heading_right = ask.x.saturating_sub(1);
        }
        if app.question.is_some() && heading_right >= top.x + 16 {
            let rect = Rect::new(heading_right - 5, top.y, 5, 1);
            paint(
                InputAction::Trust,
                rect,
                " ▰ R ",
                "Read-only · project retrieval; no writes or process execution".into(),
                false,
            );
            heading_right = rect.x.saturating_sub(1);
        }
        if !history && app.question.is_none() {
            let available = heading_right.saturating_sub(top.x + 8);
            let mode_width = (crate::width::width(mode.name) as u16 + 4).min(available);
            let policy_width = if app.trust.mode == atman_runtime::trust::TrustMode::Eager {
                (crate::width::width(escalation.name) as u16 + 4)
                    .min(available.saturating_sub(mode_width + 1))
            } else {
                0
            };
            if policy_width >= 5 {
                let label = format!(
                    " ━ {} ",
                    crate::width::truncate(
                        escalation.name,
                        policy_width.saturating_sub(4) as usize
                    )
                );
                let rect = Rect::new(heading_right - policy_width, top.y, policy_width, 1);
                paint(
                    InputAction::Escalation,
                    rect,
                    &label,
                    format!(
                        "Eager escalation: {} · select policy · Tab to cycle",
                        escalation.name
                    ),
                    true,
                );
                heading_right = rect.x.saturating_sub(1);
                policy_separator = Some(Rect::new(heading_right, top.y, 1, 1));
            }
            let label = format!(
                " ▰ {} ",
                crate::width::truncate(mode.name, mode_width.saturating_sub(4) as usize)
            );
            let rect = Rect::new(
                heading_right.saturating_sub(mode_width),
                top.y,
                mode_width,
                1,
            );
            paint(
                InputAction::Trust,
                rect,
                &label,
                format!("Permissions: {} · {}", mode.name, mode.description),
                true,
            );
            heading_right = rect.x.saturating_sub(1);
        }
    }
    if area.width >= 20 {
        let label = format!(
            " {} ",
            crate::width::middle_truncate(&model_label, model_width as usize)
        );
        paint(
            InputAction::Model,
            Rect::new(area.x + reserved, area.y, model_width + 2, 1),
            &label,
            format!(
                "Model: {} · select in this view",
                model
                    .as_ref()
                    .map_or(model_label.as_str(), |(_, full)| full)
            ),
            !history,
        );
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
    }
    // Reserve status and the tail action before truncating the recipient.
    let status_projection = if !status.is_empty() {
        let available = heading_right.saturating_sub(top.x) / 2;
        let label = crate::width::truncate(status, available as usize);
        let width = crate::width::width(&label) as u16;
        let rect = Rect::new(heading_right - width, top.y, width, 1);
        heading_right = rect.x.saturating_sub(1);
        Some((label, rect))
    } else {
        None
    };
    let heading_width = heading_right.saturating_sub(top.x);
    let latest = format!(" ↓ {pending} new ");
    let latest = if pending == 0 || heading_width < 4 {
        ""
    } else if crate::width::width(&latest) + 3 <= heading_width as usize {
        &latest
    } else {
        " ↓ "
    };
    let latest_width = crate::width::width(latest) as u16;
    let label = crate::width::truncate(
        &format!(" {title} "),
        heading_width.saturating_sub(1 + latest_width) as usize,
    );
    let heading_width = heading_width.min(1 + crate::width::width(&label) as u16);
    paint(
        InputAction::Latest,
        Rect::new(top.x + heading_width, top.y, latest_width, 1),
        latest,
        "Scroll to latest · Ctrl+End · resume following output".into(),
        true,
    );
    if let Some(rect) = policy_separator {
        f.render_widget(
            Paragraph::new("╱").style(Style::default().fg(theme.subtle_fg.into()).bg(Color::Reset)),
            rect,
        );
    }
    if let Some((label, rect)) = status_projection {
        f.render_widget(
            Paragraph::new(label)
                .style(Style::default().fg(theme.subtle_fg.into()).bg(Color::Reset)),
            rect,
        );
    }
    let mut spans = vec![ratatui::text::Span::styled(
        ">",
        Style::default()
            .fg(theme.accent.into())
            .add_modifier(if app.streaming {
                ratatui::style::Modifier::DIM
            } else {
                ratatui::style::Modifier::BOLD
            }),
    )];
    spans.push(ratatui::text::Span::styled(
        label,
        Style::default().fg(theme.work_action_fg.into()),
    ));
    f.render_widget(
        Paragraph::new(ratatui::text::Line::from(spans)).style(Style::default().bg(Color::Reset)),
        Rect::new(top.x, top.y, heading_width, 1),
    );
    if let Some(tooltip) = tooltip.filter(|_| !ui.wm.modals.any_open()) {
        let area = Rect::new(input.x, input.bottom(), input.width, 1).intersection(f.area());
        f.render_widget(ratatui::widgets::Clear, area);
        f.render_widget(
            Paragraph::new(crate::width::truncate(&tooltip, area.width as usize))
                .style(Style::default().fg(theme.subtle_fg.into()).bg(Color::Reset)),
            area,
        );
    }
    if !ui.wm.modals.any_open()
        && let Some(rect) = meter
    {
        crate::input_reasoning::render_hint(f, app, rect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_cursor_yields_to_a_focused_utility_editor() {
        let mut ui = UiState::new(crate::app::AppState::default());
        let queued =
            crate::submission_queue::QueueItem::Session(atman_runtime::QueuedSubmissionView {
                id: atman_runtime::SubmissionId::now(),
                revision: 0,
                text: "queued draft".into(),
                created_at: chrono::Utc::now(),
                presentation: None,
                insert_block_reason: None,
            });
        let mut editor = InputEditor::default();
        editor.insert_str("queued draft");
        ui.app.queued_submissions = vec![queued.clone()];
        ui.app.queued_submission_edit = Some(crate::app::QueuedSubmissionEdit {
            id: uuid::Uuid::now_v7(),
            source: queued,
            editor,
            error: None,
        });
        ui.app.submission_focus = true;
        let main = InputEditor::default();
        let bounds = Rect::new(0, 0, 100, 40);
        ui.wm.open(
            "editor",
            crate::wm::ContentKey::Knowledge,
            crate::wm::WindowContent::Knowledge,
            "Editor",
            bounds,
        );
        let geometry = ComposerLayout::bottom(&ui.app, &main, bounds);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| {
                frame.set_cursor_position((1, 1));
                render_composer(frame, &mut ui, &main, &geometry);
            })
            .unwrap();
        assert!(ui.app.submission_queue_hitmap.edit_origin.is_some());
        assert_eq!(
            terminal.backend().cursor_position(),
            ratatui::layout::Position::new(1, 1)
        );
        ui.wm.blur();
        terminal
            .draw(|frame| {
                frame.set_cursor_position((1, 1));
                render_composer(frame, &mut ui, &main, &geometry);
            })
            .unwrap();
        assert_eq!(
            Some(terminal.backend().cursor_position()),
            ui.app.submission_queue_hitmap.edit_origin.map(Into::into)
        );
    }

    #[tokio::test]
    async fn model_labels_follow_the_bound_flow_without_requiring_usage_metadata() {
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
            ui.app.settings.ready().await;
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
        for width in [32, 40, 64, 100, 160] {
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
                    ui.app.session_name = Some("Long recipient · 中文".repeat(6));
                    ui.app.resolve_scroll(500, 30, 5);
                    ui.app.scroll_up(100);
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
                    assert_eq!(editing.height, 4);
                    assert_eq!(geometry.outer, geometry.input_rect);
                    let controls = &ui.app.input_controls;
                    assert!(
                        controls
                            .iter()
                            .any(|(action, _)| *action == crate::input::InputAction::Send)
                    );
                    assert!(
                        controls
                            .iter()
                            .any(|(action, _)| *action == crate::input::InputAction::Latest)
                    );
                    for (index, (action, rect)) in controls.iter().enumerate() {
                        assert_eq!(rect.intersection(geometry.outer), *rect);
                        assert!(rect.intersection(editing).is_empty());
                        assert_eq!(
                            rect.y,
                            if matches!(
                                action,
                                crate::input::InputAction::Attach
                                    | crate::input::InputAction::Ask
                                    | crate::input::InputAction::Trust
                                    | crate::input::InputAction::Escalation
                                    | crate::input::InputAction::Latest
                            ) {
                                geometry.input_rect.y
                            } else {
                                geometry.input_rect.bottom() - 1
                            }
                        );
                        for (_, other) in &controls[index + 1..] {
                            assert!(rect.intersection(*other).is_empty());
                        }
                    }
                    let buffer = terminal.backend().buffer();
                    let hit = |action| {
                        controls
                            .iter()
                            .find(|(candidate, _)| *candidate == action)
                            .map(|(_, rect)| *rect)
                    };
                    if let (Some(trust), Some(attach)) = (
                        hit(crate::input::InputAction::Trust),
                        hit(crate::input::InputAction::Attach),
                    ) {
                        let adjacent = hit(crate::input::InputAction::Ask).unwrap_or(attach);
                        if adjacent != attach {
                            assert_eq!(adjacent.right() + 1, attach.x);
                        }
                        if mode != TrustMode::Eager {
                            assert!(hit(crate::input::InputAction::Escalation).is_none());
                            assert_eq!(
                                trust.right() + 1,
                                adjacent.x,
                                "inactive escalation must not reserve a gap"
                            );
                        }
                        assert_eq!(
                            buffer[(trust.x, trust.y)].fg,
                            crate::theme::theme().permission_color(ui.app.trust.display().color)
                        );
                    }
                    assert_eq!(
                        buffer[(geometry.input_rect.x + 2, geometry.input_rect.y)].symbol(),
                        ">"
                    );
                    for y in editing.y..editing.bottom() {
                        for x in editing.x..editing.right() {
                            assert_eq!(buffer[(x, y)].symbol(), " ");
                        }
                    }
                    assert_eq!(
                        buffer[(geometry.input_rect.x, geometry.input_rect.y)].symbol(),
                        "┌"
                    );
                    assert_eq!(
                        buffer[(geometry.input_rect.x, geometry.input_rect.y)].fg,
                        ui.app.trust.display().color.ratatui()
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
    fn border_color_preserves_streaming_fade_and_returns_to_the_mode_color() {
        for mode in atman_runtime::trust::TrustMode::all() {
            let mut ui = UiState::new(crate::app::AppState::default());
            ui.app.trust.mode = mode;
            let editor = InputEditor::default();
            let geometry = ComposerLayout::bottom(&ui.app, &editor, Rect::new(0, 0, 100, 30));
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 30)).unwrap();
            for streaming in [false, true, false] {
                ui.app.streaming = streaming;
                ui.app.was_streaming = streaming;
                ui.app.border_fade_at =
                    Some(std::time::Instant::now() - std::time::Duration::from_millis(400));
                terminal
                    .draw(|frame| render_composer(frame, &mut ui, &editor, &geometry))
                    .unwrap();
                assert_eq!(
                    terminal.backend().buffer()[(geometry.input_rect.x, geometry.input_rect.y)].fg,
                    if streaming {
                        crate::theme::theme().subtle_fg.into()
                    } else {
                        ui.app.trust.display().color.ratatui()
                    }
                );
                assert!(ui.app.border_fade_at.is_none());
            }
        }
    }

    #[test]
    fn border_controls_keep_compact_quote_dismissal_and_corners_clear() {
        for width in 6..100 {
            let mut ui = UiState::new(crate::app::AppState::default());
            ui.app.pending_quote = Some("Quoted context".into());
            ui.app.session_name = Some("A long recipient title · 中文".repeat(6));
            ui.app.trust.mode = atman_runtime::trust::TrustMode::Eager;
            ui.app.resolve_scroll(500, 30, 5);
            ui.app.scroll_up(100);
            let editor = InputEditor::default();
            let area = Rect::new(3, 2, width, 4);
            let geometry = ComposerLayout::at(&ui.app, &editor, area, area);
            assert!(geometry.quote_rect.is_none());
            let close = crate::selection_menu::quote_close_rect(area).unwrap();
            let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(
                area.right(),
                area.bottom(),
            ))
            .unwrap();
            terminal
                .draw(|frame| render_composer(frame, &mut ui, &editor, &geometry))
                .unwrap();
            for (_, rect) in &ui.app.input_controls {
                assert!(rect.intersection(close).is_empty());
            }
            let buffer = terminal.backend().buffer();
            assert_eq!(buffer[(close.x, close.y)].symbol(), "[");
            assert_eq!(buffer[(close.x + 1, close.y)].symbol(), "x");
            for (x, y, glyph) in [
                (area.x, area.y, "┌"),
                (area.right() - 1, area.y, "┐"),
                (area.x, area.bottom() - 1, "└"),
                (area.right() - 1, area.bottom() - 1, "┘"),
            ] {
                assert_eq!(buffer[(x, y)].symbol(), glyph);
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

    #[test]
    fn animated_size_reflows_cursor_and_controls_without_moving_the_reading_anchor() {
        let mut ui = UiState::new(crate::app::AppState::default());
        let mut editor = InputEditor::default();
        editor.insert_str(&"中文 editor text ".repeat(12));
        let bounds = Rect::new(0, 0, 120, 50);
        let target = ComposerLayout::bottom(&ui.app, &editor, bounds);
        for width in [40, 64, 80, 96] {
            let input = Rect::new(5, 20, width, 7);
            let visual = target.project_visual(&ui.app, &editor, bounds, input);
            assert_eq!(visual.input_rect, input);
            assert_eq!(visual.occlusion, target.occlusion);
            let mut terminal =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 50)).unwrap();
            terminal
                .draw(|frame| render_composer(frame, &mut ui, &editor, &visual))
                .unwrap();
            let editing = layout::input_editor_rect(input);
            let cursor = terminal.backend().cursor_position();
            assert!(
                editing.contains(cursor),
                "width={width}, cursor={cursor:?}, editor={editing:?}, row={}, col={}, scroll={}",
                crate::input::wrapped_cursor_row(
                    editor.buf(),
                    editor.cursor(),
                    editing.width as usize
                ),
                crate::input::wrapped_cursor_col(
                    editor.buf(),
                    editor.cursor(),
                    editing.width as usize
                ),
                visual.scroll_row,
            );
            let actual = crate::input::cursor_from_wrapped(
                editor.buf(),
                visual.scroll_row as usize + usize::from(cursor.y - editing.y),
                usize::from(cursor.x - editing.x),
                usize::from(editing.width),
            );
            assert_eq!(actual, editor.cursor());
            for (_, rect) in &ui.app.input_controls {
                assert_eq!(rect.intersection(input), *rect);
                assert!(rect.intersection(editing).is_empty());
            }
        }
    }
}
