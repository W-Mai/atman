use super::*;
use crate::settings_form::{self, Action, Field, FieldHit, FormLayout};
use crate::theme::Theme;
use ratatui::Frame;
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Paragraph};

pub(super) fn render(f: &mut Frame, area: Rect, manager: &mut ProviderManager, t: &Theme) {
    f.render_widget(
        Block::default().style(Style::default().bg(crate::wm::shell::surface_bg(t))),
        area,
    );
    if area.height < 12 || area.width < 24 {
        label(
            f,
            area,
            "More space is needed to manage providers.",
            t.subtle_fg.into(),
        );
        return;
    }
    manager.compact = area.width < 80 || area.height < 24;
    if manager.show_confirm || manager.confirm_discard || manager.show_result {
        render_confirmation(f, area, manager, t);
    } else if manager.show_add {
        if manager.in_form {
            render_form(f, area, manager, t);
        } else if manager.name_focused {
            render_oauth(f, area, manager, t);
        } else {
            render_add(f, area, manager, t);
        }
    } else if manager.more {
        render_more(f, area, manager, t);
    } else {
        render_browse(f, area, manager, t);
    }
}

fn label(f: &mut Frame, area: Rect, text: impl Into<String>, color: Color) {
    if area.height > 0 && area.width > 0 {
        f.render_widget(
            Paragraph::new(crate::width::truncate(&text.into(), area.width as usize))
                .style(Style::default().fg(color)),
            area,
        );
    }
}

fn action_label(
    action: ProviderAction,
    manager: &ProviderManager,
    width: u16,
) -> (&'static str, &'static str) {
    match action {
        ProviderAction::Add => ("Add", "n"),
        ProviderAction::Edit => ("Edit", "e"),
        ProviderAction::Models => ("Models", "m"),
        ProviderAction::Test => (
            if width >= 25 {
                "Test connection"
            } else {
                "Test"
            },
            if manager.in_form { "" } else { "t" },
        ),
        ProviderAction::Toggle => (
            if manager
                .providers
                .get(manager.selected)
                .is_some_and(|provider| provider.status == ProviderStatus::Disabled)
            {
                "Enable"
            } else {
                "Disable"
            },
            "d",
        ),
        ProviderAction::Refresh => ("Refresh", "r"),
        ProviderAction::Remove => ("Remove", "Del"),
        ProviderAction::More => ("More", "…"),
        ProviderAction::Back => ("Back", "Esc"),
        ProviderAction::Previous => ("Previous", "PgUp"),
        ProviderAction::Next => ("Next", "PgDn"),
        ProviderAction::Choose => ("Select", "Enter"),
        ProviderAction::Save => ("Save", if width < 15 { "^S" } else { "Ctrl+S" }),
        ProviderAction::Cancel => ("Cancel", if width < 15 { "" } else { "Esc" }),
        ProviderAction::ConfirmRemove => ("Remove", ""),
        ProviderAction::Discard => ("Discard", ""),
        ProviderAction::KeepEditing => (if width < 17 { "Keep" } else { "Keep editing" }, "Esc"),
        ProviderAction::Login => ("Login", ""),
        ProviderAction::Result => ("Details", ""),
        ProviderAction::Hide => ("Hide", "Esc"),
    }
}

fn button(
    f: &mut Frame,
    rect: Rect,
    action: ProviderAction,
    manager: &mut ProviderManager,
    t: &Theme,
) {
    let (text, shortcut) = action_label(action, manager, rect.width);
    let form_focus = manager.in_form
        && !manager.confirm_discard
        && match action {
            ProviderAction::Test => manager.form_field == 7,
            ProviderAction::Save => manager.form_field == 9,
            ProviderAction::Cancel => manager.form_field == 10,
            ProviderAction::Result => manager.form_field == 11,
            _ => false,
        };
    settings_form::render_action(
        f,
        rect,
        &Action {
            label: text,
            shortcut,
            focused: manager.focus == ProviderFocus::Action(action) || form_focus,
            hovered: manager.hovered_action == Some(action),
            disabled: manager.has_pending_mutation() && action != ProviderAction::Hide,
        },
        t,
    );
    manager.action_hits.push((action, rect));
    if manager.in_form {
        if action == ProviderAction::Test {
            manager.test_btn_rect = Some(rect);
        }
        if action == ProviderAction::Save {
            manager.save_rect = Some(rect);
        }
    }
}

fn actions(
    f: &mut Frame,
    rect: Rect,
    available: &[ProviderAction],
    manager: &mut ProviderManager,
    t: &Theme,
) {
    if rect.height < 3 || available.is_empty() {
        return;
    }
    let columns = available.len().min(3);
    let gap = if rect.width < 48 { 1 } else { 2 };
    let width = rect.width.saturating_sub((columns as u16 - 1) * gap) / columns as u16;
    for (index, action) in available.iter().enumerate() {
        let row = index / columns;
        let col = index % columns;
        let y = rect.y + row as u16 * 4;
        if y + 3 > rect.bottom() {
            break;
        }
        let x = rect.x + col as u16 * (width + gap);
        let width = if col + 1 == columns {
            rect.right().saturating_sub(x)
        } else {
            width
        };
        button(f, Rect::new(x, y, width, 3), *action, manager, t);
    }
}

fn footer(f: &mut Frame, rect: Rect, text: &str, manager: &ProviderManager, t: &Theme) {
    let text = manager.pending_help().unwrap_or(text);
    label(
        f,
        rect,
        text,
        if manager.has_pending_mutation() {
            t.accent.into()
        } else {
            t.subtle_fg.into()
        },
    );
}

fn render_browse(f: &mut Frame, area: Rect, manager: &mut ProviderManager, t: &Theme) {
    manager.visible_records = (area
        .height
        .saturating_sub(if manager.compact { 6 } else { 10 })
        / 4)
    .max(1) as usize;
    let available = manager.available_actions();
    let buttons = if manager.has_pending_mutation() {
        vec![ProviderAction::Hide]
    } else if manager.compact {
        let main = available
            .iter()
            .copied()
            .find(|action| matches!(action, ProviderAction::Edit | ProviderAction::Models))
            .unwrap_or(ProviderAction::Add);
        if !manager.details && main == ProviderAction::Add {
            vec![ProviderAction::Add, ProviderAction::More]
        } else {
            vec![
                if manager.details {
                    ProviderAction::Back
                } else {
                    ProviderAction::Add
                },
                main,
                ProviderAction::More,
            ]
        }
    } else {
        let mut visible: Vec<_> = available.iter().copied().take(5).collect();
        if available.len() > 5 {
            visible.push(ProviderAction::More);
        }
        visible
    };
    let button_height = if buttons.len() > 3 { 7 } else { 3 };
    let head = Rect::new(area.x, area.y, area.width, 2);
    let body = Rect::new(
        area.x,
        head.bottom(),
        area.width,
        area.height.saturating_sub(2 + button_height + 1),
    );
    let controls = Rect::new(area.x, body.bottom(), area.width, button_height);
    let help = Rect::new(area.x, controls.bottom(), area.width, 1);
    label(
        f,
        Rect::new(head.x + 1, head.y, head.width.saturating_sub(2), 1),
        format!(
            "Providers · {} / {}",
            manager.filtered_indices().len(),
            manager.providers.len()
        ),
        t.subtle_fg.into(),
    );
    render_search(f, Rect::new(head.x, head.y + 1, head.width, 1), manager, t);
    if manager.compact {
        if manager.details {
            render_details(f, body, manager, t);
        } else {
            render_list(f, body, manager, t);
        }
    } else {
        let list_width = body.width.saturating_sub(2) * 2 / 5;
        render_list(
            f,
            Rect::new(body.x, body.y, list_width, body.height),
            manager,
            t,
        );
        render_details(
            f,
            Rect::new(
                body.x + list_width + 2,
                body.y,
                body.width.saturating_sub(list_width + 2),
                body.height,
            ),
            manager,
            t,
        );
    }
    actions(f, controls, &buttons, manager, t);
    let count = manager.filtered_indices().len();
    let per_page = manager.visible_records.max(1);
    let first = manager.list_offset;
    let (page, pages) = manager.list_page();
    let text = if manager.compact && manager.details {
        "↑↓ scroll · Tab actions · Esc back".to_owned()
    } else if count > 0 {
        format!(
            "{}–{} / {} · Pg {}/{} · PgUp/PgDn",
            first + 1,
            (first + per_page).min(count),
            count,
            page,
            pages
        )
    } else {
        "Search local providers · Add n".into()
    };
    footer(f, help, &text, manager, t);
}

fn render_search(f: &mut Frame, rect: Rect, manager: &mut ProviderManager, t: &Theme) {
    let focused = manager.focus == ProviderFocus::Search;
    let bg = settings_form::render_record(f, rect, focused, false, t);
    let prefix = "Search / ";
    let value = Rect::new(
        rect.x + 1 + prefix.len() as u16,
        rect.y,
        rect.width.saturating_sub(prefix.len() as u16 + 2),
        1,
    );
    let offset = if focused {
        manager
            .search
            .cursor_display_col()
            .saturating_sub(value.width.saturating_sub(1) as usize)
    } else {
        0
    };
    label(
        f,
        Rect::new(rect.x + 1, rect.y, prefix.len() as u16, 1),
        prefix,
        t.subtle_fg.into(),
    );
    let empty = manager.search.buf().is_empty();
    f.render_widget(
        Paragraph::new(crate::width::trim_display_offset(
            if empty {
                "All local providers"
            } else {
                manager.search.buf()
            },
            offset,
            value.width as usize,
        ))
        .style(Style::default().bg(bg).fg(if empty {
            t.subtle_fg.into()
        } else {
            t.tinted_fg.into()
        })),
        value,
    );
    manager.search_hit = Some(FieldHit {
        record: rect,
        value,
        offset,
        arrows: None,
    });
    if focused && !manager.has_pending_mutation() && value.width > 0 {
        f.set_cursor_position((
            value.x
                + manager
                    .search
                    .cursor_display_col()
                    .saturating_sub(offset)
                    .min(value.width as usize - 1) as u16,
            value.y,
        ));
    }
}

fn record(
    f: &mut Frame,
    rect: Rect,
    title: &str,
    detail: &str,
    selected: bool,
    hovered: bool,
    t: &Theme,
) {
    let bg = settings_form::render_record(f, rect, selected, hovered, t);
    let text = Rect::new(rect.x + 2, rect.y + 1, rect.width.saturating_sub(4), 1);
    f.render_widget(
        Paragraph::new(crate::width::truncate(title, text.width as usize))
            .style(Style::default().fg(t.tinted_fg.into()).bg(bg)),
        text,
    );
    f.render_widget(
        Paragraph::new(crate::width::truncate(detail, text.width as usize))
            .style(Style::default().fg(t.subtle_fg.into()).bg(bg)),
        Rect::new(text.x, text.y + 1, text.width, 1),
    );
}

fn keep_visible(offset: &mut usize, selected: usize, count: usize, visible: usize) {
    let visible = visible.max(1);
    *offset = (*offset).min(count.saturating_sub(visible));
    if selected < *offset {
        *offset = selected;
    }
    if selected >= *offset + visible {
        *offset = selected + 1 - visible;
    }
}

fn render_list(f: &mut Frame, rect: Rect, manager: &mut ProviderManager, t: &Theme) {
    manager.list_rect = Some(rect);
    manager.visible_records = (rect.height / 4).max(1) as usize;
    let visible = manager.filtered_indices();
    let selected = visible
        .iter()
        .position(|index| *index == manager.selected)
        .unwrap_or(0);
    keep_visible(
        &mut manager.list_offset,
        selected,
        visible.len(),
        manager.visible_records,
    );
    if visible.is_empty() {
        render_text(
            f,
            rect,
            &[
                "No matching providers".into(),
                "".into(),
                "Search filters local records. Clear Search or add a provider.".into(),
            ],
            manager,
            t,
        );
        return;
    }
    for (row, index) in visible
        .iter()
        .skip(manager.list_offset)
        .take(manager.visible_records)
        .enumerate()
    {
        let provider = &manager.providers[*index];
        let rect = Rect::new(rect.x, rect.y + row as u16 * 4, rect.width, 4);
        let key = provider.key();
        let status = provider.status_label();
        let name_width = (rect.width as usize).saturating_sub(6 + crate::width::width(status));
        let title = format!(
            "{}  {status}",
            crate::width::pad_right(
                &crate::width::truncate(&provider.name, name_width),
                name_width
            )
        );
        record(
            f,
            rect,
            &title,
            &format!("{} · {}", provider.source_label(), provider.kind),
            *index == manager.selected,
            manager.hovered_record.as_ref() == Some(&key),
            t,
        );
        manager.record_hits.push((key, rect));
    }
}

fn render_details(f: &mut Frame, rect: Rect, manager: &mut ProviderManager, t: &Theme) {
    let mut lines = Vec::new();
    if let Some(provider) = manager
        .providers
        .get(manager.selected)
        .filter(|_| !manager.filtered_indices().is_empty())
    {
        lines.extend([
            provider.name.clone(),
            format!(
                "{} · {} · {}",
                provider.source_label(),
                provider.kind,
                provider.status_label()
            ),
            String::new(),
        ]);
        if !provider.detail.is_empty() {
            lines.push(provider.detail.clone());
            lines.push(String::new());
        }
        if matches!(provider.source, ProviderSource::Config)
            && let Some((_, entry)) = atman_runtime::model_registry::all_provider_entries()
                .into_iter()
                .find(|(name, _)| name == &provider.name)
        {
            let reason = match atman_runtime::config_provider::config_provider_availability(&entry)
            {
                atman_runtime::config_provider::ConfigProviderAvailability::Available => {
                    "Credentials and configuration available"
                }
                atman_runtime::config_provider::ConfigProviderAvailability::Disabled => {
                    "Disabled · editing and connection tests remain available"
                }
                atman_runtime::config_provider::ConfigProviderAvailability::MissingCredential => {
                    "Credential unavailable · set an API key or environment variable"
                }
                atman_runtime::config_provider::ConfigProviderAvailability::UnsupportedKind => {
                    "Provider type is unavailable"
                }
                _ => "Configuration requires attention",
            };
            lines.push(reason.into());
            if let Some(api) = entry.effective_api(None) {
                lines.push(format!("API  {api}"));
            }
            if let Some(endpoint) = entry.api_endpoint(None) {
                lines.push(format!("Endpoint  {endpoint}"));
            }
            lines.push(String::new());
        }
        let provider_key = match &provider.source {
            ProviderSource::Config => Some(provider.name.as_str()),
            ProviderSource::AuthStore { id } => Some(id.as_str()),
            ProviderSource::Env => None,
        };
        if let Some(key) = provider_key {
            let count = manager
                .groups
                .iter()
                .filter(|group| group.provider_name == key)
                .map(|group| group.models.len())
                .sum::<usize>();
            let count = count.max(
                manager
                    .model_counts
                    .iter()
                    .find(|(key, _)| *key == provider.key())
                    .map_or(0, |(_, count)| *count),
            );
            lines.push(format!("Models  {count} · open Models to manage"));
        } else {
            lines.push("Read-only environment connection".into());
            lines.push("Change credentials in the launching environment.".into());
        }
        if let Some(result) = manager.current_result() {
            lines.push(String::new());
            lines.push(format!(
                "{} · {}",
                result.operation,
                if result.result.is_ok() {
                    "Passed"
                } else {
                    "Failed"
                }
            ));
            lines.push(match &result.result {
                Ok(text) | Err(text) => text.clone(),
            });
            if result.operation == "Test connection" {
                lines.push(
                    "Directory and credentials only; model requests and tools are not tested."
                        .into(),
                );
            }
        }
    } else {
        lines.push("Select a provider to view details.".into());
    }
    render_text(f, rect, &lines, manager, t);
}

fn render_text(
    f: &mut Frame,
    rect: Rect,
    lines: &[String],
    manager: &mut ProviderManager,
    t: &Theme,
) {
    let inset = 2.min(rect.width / 2);
    let inner = Rect::new(
        rect.x + inset,
        rect.y,
        rect.width.saturating_sub(inset * 2),
        rect.height,
    );
    let wrapped: Vec<_> = lines
        .iter()
        .flat_map(|line| {
            if line.is_empty() {
                vec![String::new()]
            } else {
                crate::width::word_wrap(line, inner.width.max(1) as usize)
            }
        })
        .collect();
    manager.details_max_scroll = wrapped.len().saturating_sub(inner.height as usize);
    manager.details_scroll = manager.details_scroll.min(manager.details_max_scroll);
    manager.details_rect = Some(rect);
    f.render_widget(
        Paragraph::new(
            wrapped
                .into_iter()
                .skip(manager.details_scroll)
                .take(inner.height as usize)
                .map(Line::from)
                .collect::<Vec<_>>(),
        )
        .style(Style::default().fg(t.tinted_fg.into())),
        inner,
    );
}

fn render_more(f: &mut Frame, area: Rect, manager: &mut ProviderManager, t: &Theme) {
    label(
        f,
        Rect::new(area.x + 1, area.y, area.width - 2, 1),
        "Provider actions",
        t.subtle_fg.into(),
    );
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 5);
    let available = manager.available_actions();
    let visible = (body.height / 3).max(1) as usize;
    manager.action_selected = manager
        .action_selected
        .min(available.len().saturating_sub(1));
    keep_visible(
        &mut manager.action_offset,
        manager.action_selected,
        available.len(),
        visible,
    );
    manager.list_rect = Some(body);
    for (row, (index, action)) in available
        .iter()
        .enumerate()
        .skip(manager.action_offset)
        .take(visible)
        .enumerate()
    {
        let rect = Rect::new(body.x, body.y + row as u16 * 3, body.width, 3);
        let (label, shortcut) = action_label(*action, manager, rect.width);
        settings_form::render_action(
            f,
            rect,
            &Action {
                label,
                shortcut,
                focused: index == manager.action_selected
                    && manager.focus == ProviderFocus::ProviderList,
                hovered: manager.hovered_action == Some(*action),
                disabled: false,
            },
            t,
        );
        manager.action_hits.push((*action, rect));
    }
    actions(
        f,
        Rect::new(area.x, area.bottom() - 4, area.width, 3),
        &[ProviderAction::Back],
        manager,
        t,
    );
    footer(
        f,
        Rect::new(area.x, area.bottom() - 1, area.width, 1),
        "↑↓ select · Enter run · Esc back",
        manager,
        t,
    );
}

fn render_add(f: &mut Frame, area: Rect, manager: &mut ProviderManager, t: &Theme) {
    label(
        f,
        Rect::new(area.x + 1, area.y, area.width - 2, 1),
        "Add provider · presets and Custom",
        t.subtle_fg.into(),
    );
    let body = Rect::new(area.x, area.y + 1, area.width, area.height - 5);
    manager.list_rect = Some(body);
    manager.visible_records = (body.height / 4).max(1) as usize;
    keep_visible(
        &mut manager.option_offset,
        manager.kind_selected,
        manager.add_options.len(),
        manager.visible_records,
    );
    for (row, (index, option)) in manager
        .add_options
        .iter()
        .enumerate()
        .skip(manager.option_offset)
        .take(manager.visible_records)
        .enumerate()
    {
        let rect = Rect::new(body.x, body.y + row as u16 * 4, body.width, 4);
        record(
            f,
            rect,
            option.label,
            option.description,
            index == manager.kind_selected,
            manager.hovered_option == Some(index),
            t,
        );
        manager.option_hits.push((index, rect));
    }
    actions(
        f,
        Rect::new(area.x, area.bottom() - 4, area.width, 3),
        &[ProviderAction::Choose, ProviderAction::Back],
        manager,
        t,
    );
    footer(
        f,
        Rect::new(area.x, area.bottom() - 1, area.width, 1),
        &format!(
            "{} / {} · PgUp/PgDn · Enter select",
            manager.kind_selected + 1,
            manager.add_options.len()
        ),
        manager,
        t,
    );
}

fn render_oauth(f: &mut Frame, area: Rect, manager: &mut ProviderManager, t: &Theme) {
    label(
        f,
        Rect::new(area.x + 1, area.y, area.width - 2, 1),
        "Codex OAuth · connection name",
        t.subtle_fg.into(),
    );
    let field_rect = Rect::new(
        area.x,
        area.y + 2,
        area.width,
        if area.width >= 68 { 3 } else { 4 },
    );
    let field = settings_form::render_field(
        f,
        field_rect,
        &Field {
            label: "Name",
            value: manager.name_editor.buf(),
            placeholder: "Connection name",
            focused: manager.focus == ProviderFocus::ProviderList,
            hovered: manager.hovered_field == Some(0),
            cursor: Some(manager.name_editor.cursor_display_col()),
            locked: manager.has_pending_mutation(),
            ..Default::default()
        },
        t,
    );
    manager.form_hits.push((0, field));
    if manager.current_result().is_some() {
        button(
            f,
            Rect::new(area.x, field_rect.bottom(), area.width, 3),
            ProviderAction::Result,
            manager,
            t,
        );
    } else {
        label(
            f,
            Rect::new(area.x + 2, field_rect.bottom() + 1, area.width - 4, 1),
            "Login opens the supported OAuth flow.",
            t.subtle_fg.into(),
        );
    }
    let buttons = if manager.has_pending_mutation() {
        vec![ProviderAction::Hide]
    } else {
        vec![ProviderAction::Login, ProviderAction::Cancel]
    };
    actions(
        f,
        Rect::new(area.x, area.bottom() - 4, area.width, 3),
        &buttons,
        manager,
        t,
    );
    footer(
        f,
        Rect::new(area.x, area.bottom() - 1, area.width, 1),
        "Tab action · Enter run · Esc back",
        manager,
        t,
    );
}

fn render_form(f: &mut Frame, area: Rect, manager: &mut ProviderManager, t: &Theme) {
    label(
        f,
        Rect::new(area.x + 1, area.y, area.width - 2, 1),
        format!(
            "{} · {}",
            if manager.editing_provider.is_some() {
                "Edit provider"
            } else {
                "New provider"
            },
            manager.name_editor.buf()
        ),
        t.subtle_fg.into(),
    );
    let fields: Vec<_> = manager
        .form_fields()
        .into_iter()
        .filter(|field| *field != 7)
        .collect();
    if !fields.contains(&manager.form_field) && !matches!(manager.form_field, 7 | 9 | 10 | 11) {
        manager.form_field = fields[0];
    }
    let selected = fields
        .iter()
        .position(|field| *field == manager.form_field)
        .unwrap_or(fields.len().saturating_sub(1));
    let layout = FormLayout::with_actions(
        Rect::new(area.x, area.y + 1, area.width, area.height - 1),
        fields.len(),
        selected,
        3,
    );
    let locked = manager.has_pending_mutation();
    for &(index, rect) in &layout.fields {
        let field = fields[index];
        let active = field == manager.form_field;
        let (label, value, cursor) = match field {
            0 => (
                "Name",
                manager.name_editor.buf().to_owned(),
                Some(manager.name_editor.cursor_display_col()),
            ),
            1 => ("Type", manager.provider_type_editor.buf().to_owned(), None),
            2 => (
                "API Key",
                if active {
                    manager.api_key_editor.buf().to_owned()
                } else {
                    "•".repeat(manager.api_key_editor.buf().chars().count().min(20))
                },
                Some(manager.api_key_editor.cursor_display_col()),
            ),
            3 => (
                "API Key Env",
                manager.api_key_env_editor.buf().to_owned(),
                Some(manager.api_key_env_editor.cursor_display_col()),
            ),
            4 => (
                "Base URL",
                manager.base_url_editor.buf().to_owned(),
                Some(manager.base_url_editor.cursor_display_col()),
            ),
            5 => (
                "Chat reasoning format",
                manager.reasoning_format_editor.buf().to_owned(),
                None,
            ),
            6 => ("Enabled", manager.enabled_editor.buf().to_owned(), None),
            8 => (
                "API",
                manager.form_api.unwrap_or_default().to_string(),
                None,
            ),
            _ => continue,
        };
        let hit = settings_form::render_field(
            f,
            rect,
            &Field {
                label,
                value: &value,
                placeholder: "Not set",
                focused: active,
                hovered: manager.hovered_field == Some(field),
                cursor,
                selector: matches!(field, 1 | 5 | 6 | 8),
                locked,
            },
            t,
        );
        manager.form_hits.push((field, hit));
        if let Some((left, right)) = hit.arrows {
            manager.selector_hits.push((field, left, right));
        }
    }
    if let Some((first, _)) = layout.fields.first()
        && layout.fields.len() < fields.len()
    {
        label(
            f,
            layout.scroll,
            format!(
                "Fields {}–{} / {} · ↑↓",
                first + 1,
                first + layout.fields.len(),
                fields.len()
            ),
            t.subtle_fg.into(),
        );
    }
    if let Some(result) = manager.current_result() {
        let info = format!(
            "{} · {} · Details",
            result.operation,
            if result.result.is_ok() {
                "Passed"
            } else {
                "Failed"
            }
        );
        if layout.info.height >= 3 {
            let rect = Rect::new(layout.info.x, layout.info.y, layout.info.width, 3);
            settings_form::render_action(
                f,
                rect,
                &Action {
                    label: &info,
                    focused: manager.form_field == 11,
                    hovered: manager.hovered_action == Some(ProviderAction::Result),
                    ..Default::default()
                },
                t,
            );
            manager.action_hits.push((ProviderAction::Result, rect));
        } else {
            label(f, layout.info, info, t.subtle_fg.into());
        }
    } else {
        label(
            f,
            layout.info,
            format!("Endpoint: {}", manager.form_endpoint()),
            t.subtle_fg.into(),
        );
    }
    for (rect, action) in layout.actions.iter().zip([
        ProviderAction::Cancel,
        ProviderAction::Test,
        ProviderAction::Save,
    ]) {
        button(
            f,
            *rect,
            if locked && action == ProviderAction::Cancel {
                ProviderAction::Hide
            } else {
                action
            },
            manager,
            t,
        );
    }
    footer(
        f,
        layout.help,
        if layout.help.width < 48 {
            "Tab field · Ctrl+S save · Esc back"
        } else {
            "Tab field/action · ←→ select · Ctrl+S save"
        },
        manager,
        t,
    );
}

fn render_confirmation(f: &mut Frame, area: Rect, manager: &mut ProviderManager, t: &Theme) {
    let mut lines = Vec::new();
    let (title, controls) = if manager.show_result {
        if let Some(result) = manager.current_result() {
            lines.extend([result.name.clone(), result.operation.into(), String::new()]);
            lines.push(match &result.result {
                Ok(text) | Err(text) => text.clone(),
            });
            if result.operation == "Test connection" {
                lines.extend([String::new(), "Checks directory access and credentials only; it does not test model inference or tools.".into()]);
            }
        }
        ("Operation details", vec![ProviderAction::Back])
    } else if manager.confirm_discard {
        lines.extend([
            manager.name_editor.buf().into(),
            String::new(),
            "This draft has unsaved changes.".into(),
            "Save the changes, discard them, or keep editing.".into(),
        ]);
        (
            "Unsaved changes",
            vec![
                ProviderAction::Save,
                ProviderAction::Discard,
                ProviderAction::KeepEditing,
            ],
        )
    } else {
        lines.extend([manager.confirm_provider_name.clone(), String::new()]);
        lines.insert(
            1,
            if manager.confirm_config_provider {
                "Source: Config"
            } else {
                "Source: OAuth"
            }
            .into(),
        );
        if manager.remove_blockers.is_empty() {
            lines.push(
                if manager.confirm_config_provider {
                    "Remove the saved provider configuration?"
                } else {
                    "Remove the saved login credentials and cached model catalog?"
                }
                .into(),
            );
            (
                "Remove provider",
                vec![ProviderAction::ConfirmRemove, ProviderAction::Cancel],
            )
        } else {
            lines.push("Removal is blocked by configured models:".into());
            lines.extend(
                manager
                    .remove_blockers
                    .iter()
                    .map(|name| format!("• {name}")),
            );
            lines.extend([
                String::new(),
                "Update these models before removing this provider.".into(),
            ]);
            (
                "Provider in use",
                vec![ProviderAction::Models, ProviderAction::Cancel],
            )
        }
    };
    label(
        f,
        Rect::new(area.x + 1, area.y, area.width - 2, 1),
        title,
        t.subtle_fg.into(),
    );
    render_text(
        f,
        Rect::new(area.x, area.y + 2, area.width, area.height - 6),
        &lines,
        manager,
        t,
    );
    actions(
        f,
        Rect::new(area.x, area.bottom() - 4, area.width, 3),
        &controls,
        manager,
        t,
    );
    footer(
        f,
        Rect::new(area.x, area.bottom() - 1, area.width, 1),
        "Tab action · Enter run · ↑↓ scroll",
        manager,
        t,
    );
}
