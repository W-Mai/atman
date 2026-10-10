use super::*;
use crate::settings_form::{Action as Button, render_action, render_record};

impl ModelManager {
    pub(super) fn render_manager(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        t: &crate::theme::Theme,
    ) {
        self.record_hits.clear();
        self.action_hits.clear();
        self.form_hits.clear();
        self.search_hit = None;
        self.list_rect = None;
        self.detail_rect = None;
        self.form_info_rect = None;
        self.test_rect = None;
        self.save_rect = None;
        self.confirm_yes_rect = None;
        self.confirm_no_rect = None;
        if area.height < 12 || area.width < 24 {
            text(f, area, "Enlarge terminal to view models", t);
            return;
        }
        self.narrow = area.width < 80 || area.height < 24;
        if self.confirm_discard || self.confirm_delete.is_some() {
            self.render_confirmation(f, area, t);
        } else if self.page == Page::Status {
            let info = self.form_status();
            let body = Rect::new(
                area.x + 1,
                area.y + 1,
                area.width.saturating_sub(2),
                area.height - 5,
            );
            self.render_details(f, body, &info, t);
            self.render_buttons(
                f,
                Rect::new(area.x + 1, area.bottom() - 3, area.width - 2, 3),
                &[Action::Back],
                t,
            );
        } else if self.show_form {
            self.render_form(f, area, t);
        } else if self.page == Page::Actions {
            self.render_more(f, area, t);
        } else {
            self.render_browser(f, area, t);
        }
    }

    fn render_browser(&mut self, f: &mut ratatui::Frame, area: Rect, t: &crate::theme::Theme) {
        let content = Rect::new(area.x + 1, area.y, area.width - 2, area.height);
        let total = self
            .groups
            .iter()
            .filter(|g| {
                self.locked_provider
                    .as_ref()
                    .is_none_or(|scope| scope == &g.provider_name)
            })
            .map(|g| g.models.len())
            .sum::<usize>();
        let scope = self.locked_provider.as_deref().unwrap_or("All providers");
        text(
            f,
            Rect::new(content.x, content.y, content.width, 1),
            &format!("{scope} · {} / {total}", self.browser.rows().len()),
            t,
        );
        let search_area = Rect::new(content.x, content.y + 1, content.width, 1);
        let search_value = Rect::new(
            search_area.x + 9.min(search_area.width),
            search_area.y,
            search_area.width.saturating_sub(9),
            1,
        );
        let focused = self.focus == Focus::Search;
        let offset = if focused {
            self.search
                .cursor_display_col()
                .saturating_sub(search_value.width.saturating_sub(1) as usize)
        } else {
            0
        };
        text(f, search_area, "Search /", t);
        text(
            f,
            search_value,
            &crate::width::trim_display_offset(
                if self.search.buf().is_empty() {
                    "local models"
                } else {
                    self.search.buf()
                },
                offset,
                search_value.width as usize,
            ),
            t,
        );
        self.search_hit = Some(crate::settings_form::FieldHit {
            record: search_area,
            value: search_value,
            offset,
            arrows: None,
        });
        if focused && search_value.width > 0 {
            f.set_cursor_position((
                search_value.x
                    + self
                        .search
                        .cursor_display_col()
                        .saturating_sub(offset)
                        .min(search_value.width as usize - 1) as u16,
                search_value.y,
            ));
        }

        let actions = self.actions();
        let compact = content.height < 20 || content.width < 60;
        let footer_h = if compact {
            3
        } else if actions.len() > 3 {
            7
        } else {
            3
        };
        let footer = Rect::new(
            content.x,
            content.bottom() - footer_h,
            content.width,
            footer_h,
        );
        let body = Rect::new(
            content.x,
            content.y + 2,
            content.width,
            content.height.saturating_sub(footer_h + 3),
        );
        if self.narrow && self.page == Page::Details {
            let detail = self.model_details();
            self.render_details(f, body, &detail, t);
        } else if self.narrow {
            self.render_records(f, body, t);
        } else {
            let left_width = body.width.saturating_sub(2) * 2 / 5;
            self.render_records(f, Rect::new(body.x, body.y, left_width, body.height), t);
            let detail = self.model_details();
            self.render_details(
                f,
                Rect::new(
                    body.x + left_width + 2,
                    body.y,
                    body.width - left_width - 2,
                    body.height,
                ),
                &detail,
                t,
            );
        }
        let visible = if compact {
            vec![
                if self.narrow && self.page == Page::Details {
                    Action::Back
                } else {
                    Action::Add
                },
                Action::Edit,
                Action::More,
            ]
        } else {
            actions
        };
        self.render_buttons(f, footer, &visible, t);
        let per_page = self
            .list_rect
            .map_or(1, |r| usize::from(r.height / 4).max(1));
        let count = self.browser.rows().len();
        let page = self.browser.selected_index() / per_page + 1;
        let pages = count.div_ceil(per_page).max(1);
        let hint = if self.page == Page::Details && self.narrow {
            "↑↓ scroll · Enter action".to_owned()
        } else {
            format!("Page {page}/{pages} · PgUp/PgDn")
        };
        text(
            f,
            Rect::new(content.x, footer.y - 1, content.width, 1),
            &hint,
            t,
        );
    }

    fn render_records(&mut self, f: &mut ratatui::Frame, area: Rect, t: &crate::theme::Theme) {
        self.list_rect = Some(area);
        let count = usize::from(area.height / 4).max(1);
        self.browser.set_visible_rows(count);
        if self.browser.rows().is_empty() {
            text(
                f,
                area,
                if self.search.buf().is_empty() {
                    "No models configured or cataloged.\nUse Add to create a configuration."
                } else {
                    "No local models match Search."
                },
                t,
            );
            return;
        }
        let start = self.browser.selected_index() / count * count;
        for (row, index) in (start..(start + count).min(self.browser.rows().len())).enumerate() {
            let record = &self.browser.rows()[index];
            let mut parts = record.value.splitn(3, '\0');
            let source = if parts.next() == Some("config") {
                "Configured"
            } else {
                "Catalog"
            };
            let provider = parts.next().unwrap_or_default();
            let rect = Rect::new(area.x, area.y + row as u16 * 4, area.width, 4);
            let bg = render_record(
                f,
                rect,
                index == self.browser.selected_index(),
                self.hovered_record.as_deref() == Some(&record.value),
                t,
            );
            let name = Rect::new(rect.x + 2, rect.y + 1, rect.width.saturating_sub(4), 1);
            f.render_widget(
                Paragraph::new(crate::width::truncate(&record.label, name.width as usize))
                    .style(Style::default().bg(bg).fg(t.tinted_fg.into())),
                name,
            );
            let meta = Rect::new(name.x, name.y + 1, name.width, 1);
            f.render_widget(
                Paragraph::new(crate::width::truncate(
                    &format!("{source} · {provider}"),
                    meta.width as usize,
                ))
                .style(Style::default().bg(bg).fg(t.subtle_fg.into())),
                meta,
            );
            self.record_hits.push((record.value.clone(), rect));
        }
    }

    fn model_details(&self) -> String {
        let Some(model) = self.current_model() else {
            return "Select a model to view details.".into();
        };
        let entry = atman_runtime::model_registry::model_entry(&model.slug).filter(|entry| {
            entry.provider.as_deref().unwrap_or("unknown") == self.current_provider()
        });
        let provider = atman_runtime::model_registry::all_provider_entries()
            .into_iter()
            .find(|(key, _)| key == self.current_provider())
            .map(|(_, entry)| entry);
        let aliases = Self::aliases_for(&model.slug);
        let mut lines = vec![
            model.slug.clone(),
            String::new(),
            if self.current_editable() {
                "Configured model · editable".into()
            } else {
                "Catalog model · read-only".into()
            },
            format!("Provider: {}", self.current_provider()),
        ];
        if !atman_runtime::model_registry::is_provider_enabled(self.current_provider()) {
            lines.push("Provider disabled · configuration remains available".into());
        }
        if let Some(entry) = &entry {
            lines.push(format!(
                "Model ID: {}",
                if entry.model.is_empty() {
                    &model.slug
                } else {
                    &entry.model
                }
            ));
            if let Some(provider) = provider {
                if let Some(api) = provider.effective_api(entry.api) {
                    lines.push(format!(
                        "API: {}",
                        if entry.api.is_some() {
                            api.to_string()
                        } else {
                            format!("Inherit ({api})")
                        }
                    ));
                }
                if let Some(endpoint) = provider.api_endpoint(entry.api) {
                    lines.push(format!("Endpoint: {endpoint}"));
                }
            } else {
                lines.push("API: Provider-managed".into());
            }
        }
        lines.extend([
            format!("Reasoning: {}", model.reasoning),
            format!("Context Budget: {}", model.context_budget),
            format!(
                "Max Tokens: {}",
                model
                    .max_output_tokens
                    .map_or("Inherit".into(), |n| n.to_string())
            ),
            String::new(),
            format!(
                "Aliases: {}",
                if aliases.is_empty() {
                    "None".into()
                } else {
                    aliases.join(", ")
                }
            ),
        ]);
        if let Some((message, _)) = &self.status {
            lines.extend([String::new(), message.clone()]);
        }
        lines.join("\n")
    }

    fn render_details(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        value: &str,
        t: &crate::theme::Theme,
    ) {
        self.detail_rect = Some(area);
        let lines =
            crate::width::word_wrap(value, usize::from(area.width.saturating_sub(2)).max(1));
        self.detail_max_scroll = lines.len().saturating_sub(area.height as usize);
        self.detail_scroll = self.detail_scroll.min(self.detail_max_scroll);
        f.render_widget(
            Paragraph::new(
                lines
                    .into_iter()
                    .skip(self.detail_scroll)
                    .take(area.height as usize)
                    .map(Line::from)
                    .collect::<Vec<_>>(),
            )
            .style(
                Style::default()
                    .fg(t.tinted_fg.into())
                    .bg(crate::wm::shell::surface_bg(t)),
            ),
            Rect::new(
                area.x + 1,
                area.y,
                area.width.saturating_sub(2),
                area.height,
            ),
        );
    }

    fn render_more(&mut self, f: &mut ratatui::Frame, area: Rect, t: &crate::theme::Theme) {
        text(
            f,
            Rect::new(area.x + 1, area.y, area.width - 2, 1),
            "Model actions · ↑↓ select · Enter",
            t,
        );
        let actions = self.actions();
        let count = usize::from(area.height.saturating_sub(5) / 3).max(1);
        let selected = actions
            .iter()
            .position(|action| self.focus == Focus::Action(*action))
            .unwrap_or(0);
        if selected < self.actions_offset {
            self.actions_offset = selected;
        }
        if selected >= self.actions_offset + count {
            self.actions_offset = selected + 1 - count;
        }
        for (row, action) in actions
            .iter()
            .skip(self.actions_offset)
            .take(count)
            .enumerate()
        {
            self.render_button(
                f,
                Rect::new(area.x + 1, area.y + 1 + row as u16 * 3, area.width - 2, 3),
                *action,
                t,
            );
        }
        self.render_button(
            f,
            Rect::new(area.x + 1, area.bottom() - 3, area.width - 2, 3),
            Action::Back,
            t,
        );
    }

    fn render_confirmation(&mut self, f: &mut ratatui::Frame, area: Rect, t: &crate::theme::Theme) {
        let (message, actions) = if self.confirm_discard {
            (
                format!(
                    "Unsaved model configuration\n\n{}\n\nSave the changes, discard this draft, or keep editing.",
                    self.name_editor.buf()
                ),
                vec![Action::Save, Action::Discard, Action::Keep],
            )
        } else {
            let name = self.confirm_delete.as_deref().unwrap_or_default();
            let message = if self.delete_aliases.is_empty() {
                format!(
                    "Delete model \"{name}\"?\n\nConfigured model\n\nOnly this saved model configuration is removed. This change cannot be undone."
                )
            } else {
                format!(
                    "Cannot delete \"{name}\"\n\nReferenced by aliases:\n{}\n\nEdit or remove these references in Aliases first.",
                    self.delete_aliases.join("\n")
                )
            };
            (
                message,
                if self.delete_aliases.is_empty() {
                    vec![Action::Cancel, Action::Confirm]
                } else {
                    vec![Action::Cancel, Action::Aliases]
                },
            )
        };
        let mut message = message;
        if let Some((error, false)) = &self.status {
            message.push_str(&format!("\n\n{error}"));
        }
        self.render_details(
            f,
            Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 5),
            &message,
            t,
        );
        self.focus = Focus::Action(actions[self.confirm_focus.min(actions.len() - 1)]);
        self.render_buttons(
            f,
            Rect::new(area.x + 1, area.bottom() - 3, area.width - 2, 3),
            &actions,
            t,
        );
    }

    fn render_buttons(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        actions: &[Action],
        t: &crate::theme::Theme,
    ) {
        let columns = if actions.len() > 3 {
            actions.len().div_ceil(2)
        } else {
            actions.len()
        }
        .max(1);
        let width = area.width.saturating_sub(columns as u16 - 1) / columns as u16;
        for (index, action) in actions.iter().enumerate() {
            let row = index / columns;
            let column = index % columns;
            let x = area.x + column as u16 * (width + 1);
            let y = area.y + row as u16 * 4;
            if y + 3 <= area.bottom() {
                self.render_button(
                    f,
                    Rect::new(
                        x,
                        y,
                        if column + 1 == columns {
                            area.right() - x
                        } else {
                            width
                        },
                        3,
                    ),
                    *action,
                    t,
                );
            }
        }
    }

    fn render_button(
        &mut self,
        f: &mut ratatui::Frame,
        rect: Rect,
        action: Action,
        t: &crate::theme::Theme,
    ) {
        let (label, shortcut) = match action {
            Action::Add => ("Add", "n"),
            Action::Edit => ("Edit", "e"),
            Action::Alias => (
                if rect.width < 19 {
                    "Alias"
                } else {
                    "Create alias"
                },
                "a",
            ),
            Action::Delete => ("Delete", "Del"),
            Action::All => ("All providers", ""),
            Action::More => ("More", ""),
            Action::Back => ("Back", "Esc"),
            Action::Save => ("Save", "^S"),
            Action::Cancel => (
                if self.inputs_locked() {
                    "Hide"
                } else {
                    "Cancel"
                },
                "Esc",
            ),
            Action::Test => ("Test", ""),
            Action::Stop => ("Stop test", ""),
            Action::Details => ("Details", ""),
            Action::Confirm => ("Delete", ""),
            Action::Discard => ("Discard", ""),
            Action::Keep => (
                if rect.width < 16 {
                    "Keep"
                } else {
                    "Keep editing"
                },
                "",
            ),
            Action::Aliases => ("Aliases", ""),
        };
        let disabled = (matches!(action, Action::Edit | Action::Delete)
            && !self.current_editable())
            || (action == Action::Alias && self.current_model().is_none())
            || (self.inputs_locked()
                && !matches!(
                    action,
                    Action::Cancel | Action::Back | Action::Stop | Action::Details
                ));
        render_action(
            f,
            rect,
            &Button {
                label,
                shortcut,
                focused: self.focus == Focus::Action(action),
                hovered: self.hovered_action == Some(action),
                disabled,
            },
            t,
        );
        self.action_hits.push((action, rect));
        if action == Action::Confirm {
            self.confirm_yes_rect = Some(rect);
        }
        if action == Action::Cancel {
            self.confirm_no_rect = Some(rect);
        }
    }

    pub(super) fn form_status(&self) -> String {
        let endpoint = self
            .provider_entry()
            .and_then(|entry| entry.api_endpoint(self.form_api))
            .unwrap_or_else(|| "Provider-managed endpoint".into());
        let target = format!(
            "{}{} · {}\n{}\nAPI: {} · Reasoning: {} · Max Tokens: {}",
            self.selected_provider,
            if self.locked_provider.is_some() {
                " (locked by scope)"
            } else {
                ""
            },
            self.model_editor.buf(),
            endpoint,
            self.api_label(),
            self.thinking_editor.buf(),
            if self.max_tokens_editor.buf().is_empty() {
                "Inherit"
            } else {
                self.max_tokens_editor.buf()
            }
        );
        match &self.test_result {
            Some((result, _)) => format!("{result}\n\n{target}"),
            None => {
                format!("{target}\n\nTest sends a model request; returned tools are not executed.")
            }
        }
    }
}

fn text(f: &mut ratatui::Frame, rect: Rect, value: &str, t: &crate::theme::Theme) {
    f.render_widget(
        Paragraph::new(value).style(
            Style::default()
                .fg(t.subtle_fg.into())
                .bg(crate::wm::shell::surface_bg(t)),
        ),
        rect,
    );
}
