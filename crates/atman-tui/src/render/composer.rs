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
        let submission_base_rows: u16 = if app.queued_submission_edit.is_some() {
            11
        } else if app.queued_submissions.is_empty() {
            0
        } else {
            (app.queued_submissions.len() as u16).min(3) * 3
                + 2
                + if app.queue_page().is_some_and(|page| page.paged()) {
                    3
                } else {
                    0
                }
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
        let visible_rows = u32::from(input_rect.height.saturating_sub(2).max(1));
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
        let submission_rows = if submission_base_rows == 0 {
            0
        } else {
            let reason_rows = submission_queue::unavailable_lines(
                &app.queued_submissions,
                app.selected_submission
                    .min(app.queued_submissions.len().saturating_sub(1)),
                app.submission_focus,
                app.queue_active_turn(),
                layout::stacked_rect_width(input_rect.width),
            )
            .len() as u16;
            let footer_rows = if app.submission_focus
                && layout::stacked_rect_width(input_rect.width) < 80
                && app.queued_submission_edit.is_none()
            {
                3
            } else {
                0
            };
            let expanded = submission_base_rows
                .saturating_add(reason_rows)
                .saturating_add(footer_rows);
            let base_y = approvals_rect
                .or(quote_rect)
                .or(attachments_rect)
                .map_or(input_rect.y, |rect| rect.y);
            if expanded < base_y.saturating_sub(bounds.y) {
                expanded
            } else {
                submission_base_rows
            }
        };
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
        let top = top.saturating_sub(1).max(bounds.y);
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
    let theme = crate::theme::theme();
    f.render_widget(
        ratatui::widgets::Block::default().style(Style::default().bg(theme.panel_bg.into())),
        geometry.outer,
    );
    let title = app
        .items
        .snapshot()
        .map(|s| s.display_title.as_str())
        .or(app.session_name.as_deref())
        .unwrap_or("Main");
    f.render_widget(
        Paragraph::new(format!(
            "  To: {}",
            crate::width::truncate(title, geometry.outer.width.saturating_sub(7) as usize)
        ))
        .style(
            Style::default()
                .fg(theme.accent.into())
                .bg(theme.work_detail_bg.into()),
        ),
        Rect::new(geometry.outer.x, geometry.outer.y, geometry.outer.width, 1),
    );
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
        let block = ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(ratatui::style::Style::default().fg(theme.accent.into()))
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
                    format!("│ {line}"),
                    ratatui::style::Style::default().fg(theme.subtle_fg.into()),
                ))
            })
            .collect::<Vec<_>>();
        if disclosure > 0 {
            lines.push(ratatui::text::Line::from(ratatui::text::Span::styled(
                if app.quote_expanded {
                    format!("│ {}–{} / {total} · scroll", start + 1, start + text_rows)
                } else {
                    format!("│ +{} more · click or F4 to expand", total - text_rows)
                },
                ratatui::style::Style::default().fg(theme.subtle_fg.into()),
            )));
        }
        let block = ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Plain)
            .border_style(ratatui::style::Style::default().fg(theme.subtle_fg.into()))
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
        let block = ratatui::widgets::Block::default()
            .borders(ratatui::widgets::Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(ratatui::style::Style::default().fg(crate::theme::theme().warn.into()));
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
    let reasoning_badge = app.effective_input_reasoning_badge();
    f.render_widget(
        input_paragraph(
            editor.buf(),
            border_color,
            app.pending_below_rows().min(u16::MAX as u32) as u16,
            scroll_row.min(u16::MAX as u32) as u16,
            &app.trust,
            reasoning_badge.as_deref(),
            InputFooter {
                read_only: app.items.is_archive(),
                sending: app.pending_submission.is_some(),
                queued_count: app
                    .queue_page()
                    .map_or(app.queued_submissions.len(), |page| page.total),
                pending_quote_lines: app
                    .pending_quote
                    .as_ref()
                    .filter(|_| quote_rect.is_none())
                    .map(|quote| quote.lines().count()),
            },
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
            x: input_rect.x,
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
        let inner_x = input_rect.x.saturating_add(layout::INPUT_LEFT);
        let inner_y = input_rect.y.saturating_add(1);
        let mut placed = false;
        if raw_row as u32 >= scroll_row {
            let cy = inner_y + (raw_row as u32 - scroll_row) as u16;
            let cx = inner_x + raw_col;
            if cy < input_rect.y + input_rect.height.saturating_sub(1)
                && cx < input_rect.x + input_rect.width.saturating_sub(1)
            {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
