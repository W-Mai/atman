use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;
use ratatui::widgets::{Block, Paragraph, Wrap};
use tokio::sync::mpsc;

use atman_runtime::memory::MemoryId;
use atman_runtime::memory::confession::{ConfessionChange, ConfessionFields, ConfessionView};

use crate::input::{InputEditor, cursor_from_wrapped, visual_line_count, wrapped_cursor_position};
use crate::keys::KeyAction;
use crate::wm::component::{
    CloseOutcome, EventCtx, HitRegion, RenderCtx, SizeHint, WindowComponent, WmEvent, WmEventResult,
};
use crate::{OrganizationProposal, RuleView, TuiControl};

#[derive(Default)]
pub struct KnowledgeState {
    pub confessions: Vec<ConfessionView>,
    pub rules: Vec<RuleView>,
    pub proposals: Vec<OrganizationProposal>,
    pub message: String,
    pub loading: bool,
    pub applying: bool,
    pub organize_request: u64,
    pub history_id: Option<MemoryId>,
    pub history: Vec<ConfessionChange>,
    pub saved_id: Option<MemoryId>,
    pub failed_save_id: Option<MemoryId>,
}

struct EditState {
    id: MemoryId,
    revision: u64,
    original: [String; 5],
    fields: [InputEditor; 5],
    focused: usize,
    saving: bool,
    scroll: [u16; 5],
    follow_cursor: bool,
}

impl EditState {
    fn new(view: &ConfessionView) -> Self {
        let c = &view.confession;
        let text = [
            &c.trigger,
            &c.rule_violated,
            &c.what_i_did,
            &c.why,
            &c.mitigation,
        ];
        let fields = std::array::from_fn(|index| {
            let mut editor = InputEditor::default();
            editor.insert_str(text[index]);
            editor
        });
        Self {
            id: c.id.clone(),
            revision: view.revision,
            original: text.map(ToOwned::to_owned),
            fields,
            focused: 0,
            saving: false,
            scroll: [0; 5],
            follow_cursor: true,
        }
    }

    fn values(&self) -> ConfessionFields {
        ConfessionFields {
            trigger: self.fields[0].buf().to_owned(),
            rule_violated: self.fields[1].buf().to_owned(),
            what_i_did: self.fields[2].buf().to_owned(),
            why: self.fields[3].buf().to_owned(),
            mitigation: self.fields[4].buf().to_owned(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HoverTarget {
    Action(&'static str),
    Tab(usize),
    Row(usize),
    Search,
    Field(usize),
}

pub struct KnowledgePanelContent {
    state: Arc<Mutex<KnowledgeState>>,
    control_tx: Option<mpsc::UnboundedSender<TuiControl>>,
    tab: usize,
    selected: usize,
    scroll: u32,
    detail_scroll: u16,
    detail_rect: Rect,
    body_height: u16,
    search: InputEditor,
    search_focused: bool,
    search_scroll: u16,
    edit: Option<EditState>,
    selected_proposals: HashSet<MemoryId>,
    show_history: bool,
    tab_rects: [Rect; 3],
    search_rect: Rect,
    search_input_rect: Rect,
    row_rects: Vec<(usize, Rect)>,
    action_rects: Vec<(&'static str, Rect)>,
    edit_segment_rects: [Rect; 5],
    edit_text_rects: [Rect; 5],
    hovered: Option<HoverTarget>,
    cursor_on_screen: bool,
}

impl KnowledgePanelContent {
    pub fn new(
        state: Arc<Mutex<KnowledgeState>>,
        control_tx: Option<mpsc::UnboundedSender<TuiControl>>,
    ) -> Self {
        Self {
            state,
            control_tx,
            tab: 0,
            selected: 0,
            scroll: 0,
            detail_scroll: 0,
            detail_rect: Rect::default(),
            body_height: 0,
            search: InputEditor::default(),
            search_focused: false,
            search_scroll: 0,
            edit: None,
            selected_proposals: HashSet::new(),
            show_history: false,
            tab_rects: [Rect::default(); 3],
            search_rect: Rect::default(),
            search_input_rect: Rect::default(),
            row_rects: Vec::new(),
            action_rects: Vec::new(),
            edit_segment_rects: [Rect::default(); 5],
            edit_text_rects: [Rect::default(); 5],
            hovered: None,
            cursor_on_screen: false,
        }
    }

    fn filtered_indices(&self, state: &KnowledgeState) -> Vec<usize> {
        let needle = self.search.buf().to_lowercase();
        match self.tab {
            0 => state
                .confessions
                .iter()
                .enumerate()
                .filter_map(|(index, view)| {
                    let c = &view.confession;
                    (needle.is_empty()
                        || [
                            c.trigger.as_str(),
                            c.rule_violated.as_str(),
                            c.mitigation.as_str(),
                            view.category.as_deref().unwrap_or(""),
                        ]
                        .iter()
                        .any(|value| value.to_lowercase().contains(&needle)))
                    .then_some(index)
                    .or_else(|| {
                        c.created_at
                            .format("%Y-%m-%d")
                            .to_string()
                            .contains(&needle)
                            .then_some(index)
                    })
                })
                .collect(),
            1 => state
                .rules
                .iter()
                .enumerate()
                .filter_map(|(index, rule)| {
                    (needle.is_empty()
                        || [&rule.name, &rule.description, &rule.source_path]
                            .iter()
                            .any(|value| value.to_lowercase().contains(&needle)))
                    .then_some(index)
                })
                .collect(),
            _ => state
                .proposals
                .iter()
                .enumerate()
                .filter_map(|(index, proposal)| {
                    let trigger = state
                        .confessions
                        .iter()
                        .find(|view| view.confession.id == proposal.id)
                        .map(|view| view.confession.trigger.as_str())
                        .unwrap_or("");
                    (needle.is_empty()
                        || [
                            proposal.category.as_str(),
                            proposal.reason.as_str(),
                            trigger,
                        ]
                        .iter()
                        .any(|value| value.to_lowercase().contains(&needle)))
                    .then_some(index)
                })
                .collect(),
        }
    }

    fn send(&self, control: TuiControl) {
        if let Some(tx) = &self.control_tx {
            let _ = tx.send(control);
        }
    }

    fn action_label(name: &str) -> &'static str {
        match name {
            "Edit" => "[E]dit",
            "History" => "[H]istory",
            "Organize" => "[O]rganize",
            "Refresh" => "[R]efresh",
            "Reload" => "[⇧R]eload",
            "Stop" => "[C]ancel",
            "Select All" => "[S]elect all",
            "Apply" => "[A]pply",
            "Applying…" => "Applying…",
            _ => "",
        }
    }

    fn hover_at(&self, x: u16, y: u16) -> Option<HoverTarget> {
        let point = (x, y).into();
        if let Some((name, _)) = self
            .action_rects
            .iter()
            .find(|(_, rect)| rect.contains(point))
        {
            return Some(HoverTarget::Action(name));
        }
        if self.edit.is_some() {
            return self
                .edit_segment_rects
                .iter()
                .position(|rect| rect.contains(point))
                .map(HoverTarget::Field);
        }
        if let Some(index) = self.tab_rects.iter().position(|rect| rect.contains(point)) {
            return Some(HoverTarget::Tab(index));
        }
        if self.search_rect.contains(point) {
            return Some(HoverTarget::Search);
        }
        self.row_rects
            .iter()
            .find(|(_, rect)| rect.contains(point))
            .map(|(index, _)| HoverTarget::Row(*index))
    }

    fn action(&mut self, name: &str) {
        self.search_focused = false;
        let mut state = self.state.lock().unwrap();
        let indices = self.filtered_indices(&state);
        match name {
            "Refresh" => {
                drop(state);
                self.send(TuiControl::ListKnowledge);
            }
            "Reload" => {
                drop(state);
                self.send(TuiControl::ReloadRules);
            }
            "Edit" => {
                if self.tab == 0
                    && let Some(&index) = indices.get(self.selected)
                    && let Some(view) = state.confessions.get(index)
                {
                    self.edit = Some(EditState::new(view));
                }
            }
            "History" => {
                if self.tab == 0
                    && let Some(&index) = indices.get(self.selected)
                    && let Some(view) = state.confessions.get(index)
                {
                    let id = view.confession.id.clone();
                    drop(state);
                    self.show_history = !self.show_history;
                    if self.show_history {
                        self.send(TuiControl::GetConfessionHistory(id));
                    }
                }
            }
            "Organize" => {
                if state.loading || state.applying {
                    return;
                }
                state.loading = true;
                state.proposals.clear();
                state.message = "Generating suggestions…".into();
                state.organize_request = state.organize_request.wrapping_add(1);
                let request_id = state.organize_request;
                drop(state);
                self.selected_proposals.clear();
                self.tab = 2;
                self.selected = 0;
                self.send(TuiControl::SuggestOrganization { request_id });
            }
            "Stop" => {
                if !state.loading {
                    return;
                }
                state.loading = false;
                state.organize_request = state.organize_request.wrapping_add(1);
                state.message = "Organization cancelled".into();
                drop(state);
                self.send(TuiControl::CancelOrganization);
            }
            "Apply" => {
                if state.applying {
                    return;
                }
                let changes = state
                    .proposals
                    .iter()
                    .filter(|proposal| self.selected_proposals.contains(&proposal.id))
                    .map(|proposal| ConfessionChange::Organized {
                        id: proposal.id.clone(),
                        base_revision: proposal.base_revision,
                        category: proposal.category.clone(),
                        related_ids: proposal.related_ids.clone(),
                        changed_at: chrono::Utc::now(),
                    })
                    .collect::<Vec<_>>();
                if !changes.is_empty() {
                    state.applying = true;
                    state.message = "Applying organization…".into();
                    drop(state);
                    self.send(TuiControl::OrganizeConfessions(changes));
                } else {
                    state.message = "Select suggestions before applying".into();
                }
            }
            "Select All" => {
                let ids = state
                    .proposals
                    .iter()
                    .map(|proposal| proposal.id.clone())
                    .collect::<HashSet<_>>();
                if ids.iter().all(|id| self.selected_proposals.contains(id)) {
                    self.selected_proposals.clear();
                } else {
                    self.selected_proposals = ids;
                }
            }
            "Save" => {
                if let Some(edit) = &mut self.edit
                    && !edit.saving
                {
                    let fields = edit.values();
                    let id = edit.id.clone();
                    let base_revision = edit.revision;
                    edit.saving = true;
                    state.saved_id = None;
                    state.failed_save_id = None;
                    drop(state);
                    self.send(TuiControl::ReviseConfession {
                        id,
                        base_revision,
                        fields,
                    });
                }
            }
            "Cancel" if self.edit.as_ref().is_some_and(|edit| !edit.saving) => self.edit = None,
            _ => {}
        }
    }

    fn render_editor(&mut self, area: Rect, frame: &mut Frame) {
        let Some(edit) = &mut self.edit else {
            return;
        };
        let t = crate::theme::theme();
        let labels = [
            "Trigger",
            "Rule violated",
            "What I did",
            "Why",
            "Mitigation",
        ];
        let changed = labels
            .iter()
            .enumerate()
            .filter_map(|(index, label)| {
                (edit.fields[index].buf() != edit.original[index]).then_some(*label)
            })
            .collect::<Vec<_>>();
        frame.render_widget(
            Paragraph::new(format!(
                " Edit confession · revision {} · Tab/Shift+Tab fields · ↑↓ move ",
                edit.revision + 1
            ))
            .style(Style::default().fg(t.accent.into())),
            Rect::new(area.x, area.y, area.width, 1),
        );
        let body_rows = area.height.saturating_sub(3);
        let base_rows = body_rows / 5;
        let extra_rows = body_rows % 5;
        let mut y = area.y + 1;
        for (index, label) in labels.into_iter().enumerate() {
            let rows = base_rows + u16::from(index < extra_rows as usize);
            let segment = Rect::new(area.x, y, area.width, rows);
            let text_rect = Rect::new(
                area.x + 2,
                y + 1,
                area.width.saturating_sub(3),
                rows.saturating_sub(1),
            );
            self.edit_segment_rects[index] = segment;
            self.edit_text_rects[index] = text_rect;
            let focused = edit.focused == index;
            let hovered = self.hovered == Some(HoverTarget::Field(index));
            let bg = if hovered {
                t.highlight_bg.into()
            } else if focused {
                t.code_bg.into()
            } else {
                t.work_bg.into()
            };
            frame.render_widget(Block::default().style(Style::default().bg(bg)), segment);
            frame.render_widget(
                Paragraph::new(format!(" {} {label}", if focused { "▸" } else { " " })).style(
                    Style::default()
                        .fg(if focused || hovered {
                            t.accent.into()
                        } else {
                            t.subtle_fg.into()
                        })
                        .bg(bg)
                        .add_modifier(if focused || hovered {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                Rect::new(area.x, y, area.width, 1),
            );
            let content_width = text_rect.width.max(1) as usize;
            let total_rows = visual_line_count(edit.fields[index].buf(), content_width);
            let cursor_position = focused.then(|| {
                wrapped_cursor_position(
                    edit.fields[index].buf(),
                    edit.fields[index].cursor(),
                    content_width,
                )
            });
            let max_scroll = total_rows
                .max(cursor_position.map_or(0, |(row, _)| row + 1))
                .saturating_sub(text_rect.height as usize);
            if let Some((cursor_row, _)) = cursor_position {
                if edit.follow_cursor {
                    let scroll = edit.scroll[index] as usize;
                    if cursor_row < scroll {
                        edit.scroll[index] = cursor_row as u16;
                    } else if cursor_row >= scroll + text_rect.height as usize {
                        edit.scroll[index] = cursor_row
                            .saturating_sub(text_rect.height as usize)
                            .saturating_add(1) as u16;
                    }
                }
            }
            edit.scroll[index] = edit.scroll[index].min(max_scroll as u16);
            frame.render_widget(
                Paragraph::new(edit.fields[index].buf())
                    .wrap(Wrap { trim: false })
                    .scroll((edit.scroll[index], 0))
                    .style(Style::default().bg(bg)),
                text_rect,
            );
            if let Some((cursor_row, cursor_col)) = cursor_position.filter(|_| !edit.saving) {
                let visible_row = cursor_row.saturating_sub(edit.scroll[index] as usize);
                if cursor_row >= edit.scroll[index] as usize
                    && visible_row < text_rect.height as usize
                    && cursor_col < text_rect.width as usize
                {
                    frame.set_cursor_position((
                        text_rect.x + cursor_col as u16,
                        text_rect.y + visible_row as u16,
                    ));
                    self.cursor_on_screen = true;
                }
            }
            y += rows;
        }
        edit.follow_cursor = false;
        self.action_rects.clear();
        let status = Rect::new(
            area.x,
            area.y + area.height.saturating_sub(2),
            area.width,
            1,
        );
        let message = if edit.saving {
            "Saving…".to_string()
        } else {
            let state_message = self.state.lock().unwrap().message.clone();
            if state_message.is_empty() || state_message.contains(" confessions · ") {
                format!(
                    " Changed: {}",
                    if changed.is_empty() {
                        "none".into()
                    } else {
                        changed.join(", ")
                    }
                )
            } else {
                state_message
            }
        };
        frame.render_widget(Paragraph::new(message), status);
        let y = area.y + area.height.saturating_sub(1);
        let save = Rect::new(area.x, y, 14.min(area.width), 1);
        let cancel = Rect::new(
            area.x.saturating_add(15),
            y,
            14.min(area.width.saturating_sub(15)),
            1,
        );
        frame.render_widget(
            Paragraph::new("Save [Enter]").style(Style::default().fg(
                if self.hovered == Some(HoverTarget::Action("Save")) {
                    t.accent.into()
                } else {
                    t.subtle_fg.into()
                },
            )),
            save,
        );
        frame.render_widget(
            Paragraph::new("Cancel [Esc]").style(Style::default().fg(
                if self.hovered == Some(HoverTarget::Action("Cancel")) {
                    t.accent.into()
                } else {
                    t.subtle_fg.into()
                },
            )),
            cancel,
        );
        if area.width > 31 {
            frame.render_widget(
                Paragraph::new("Newline [Shift+Enter]")
                    .style(Style::default().fg(t.subtle_fg.into())),
                Rect::new(area.x + 30, y, area.width - 30, 1),
            );
        }
        self.action_rects
            .extend([("Save", save), ("Cancel", cancel)]);
    }

    fn sync_edit_result(&mut self) {
        let Some(id) = self.edit.as_ref().map(|edit| edit.id.clone()) else {
            return;
        };
        let mut state = self.state.lock().unwrap();
        if state.saved_id.as_ref() == Some(&id) {
            self.edit = None;
            state.saved_id = None;
        } else if state.failed_save_id.as_ref() == Some(&id) {
            if let Some(edit) = &mut self.edit {
                edit.saving = false;
            }
            state.failed_save_id = None;
        }
    }
}

impl WindowComponent for KnowledgePanelContent {
    fn render_content(
        &mut self,
        area: Rect,
        frame: &mut Frame,
        _ctx: &RenderCtx,
    ) -> Vec<HitRegion> {
        let area = Rect::new(
            area.x.saturating_add(1),
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        self.cursor_on_screen = false;
        if area.width < 25 || area.height < 13 {
            frame.render_widget(Paragraph::new("Enlarge window to edit memory"), area);
            return Vec::new();
        }
        self.sync_edit_result();
        if self.edit.is_some() {
            self.render_editor(area, frame);
            return Vec::new();
        }
        let state = self.state.lock().unwrap();
        let t = crate::theme::theme();
        let tab_names = ["Confessions", "Rules", "Suggestions"];
        let mut x = area.x;
        for (index, name) in tab_names.into_iter().enumerate() {
            let width = (name.len() as u16 + 4).min(area.x + area.width - x);
            let rect = Rect::new(x, area.y, width, 1);
            self.tab_rects[index] = rect;
            frame.render_widget(
                Paragraph::new(format!(" {name} ")).style(
                    Style::default()
                        .fg(
                            if self.tab == index || self.hovered == Some(HoverTarget::Tab(index)) {
                                t.accent.into()
                            } else {
                                t.subtle_fg.into()
                            },
                        )
                        .bg(if self.hovered == Some(HoverTarget::Tab(index)) {
                            t.highlight_bg.into()
                        } else {
                            t.work_bg.into()
                        })
                        .add_modifier(if self.tab == index {
                            Modifier::BOLD
                        } else {
                            Modifier::empty()
                        }),
                ),
                rect,
            );
            x = x.saturating_add(width);
        }
        self.search_rect = Rect::new(area.x, area.y + 1, area.width, 1);
        self.search_input_rect =
            Rect::new(area.x + 12, area.y + 1, area.width.saturating_sub(12), 1);
        let search_bg = if self.search_focused || self.hovered == Some(HoverTarget::Search) {
            t.highlight_bg.into()
        } else {
            t.work_bg.into()
        };
        frame.render_widget(
            Block::default().style(Style::default().bg(search_bg)),
            self.search_rect,
        );
        frame.render_widget(
            Paragraph::new(" Search [/]")
                .style(Style::default().fg(t.subtle_fg.into()).bg(search_bg)),
            Rect::new(area.x, area.y + 1, 12, 1),
        );
        let search_width = self.search_input_rect.width.max(1) as usize;
        if self.search_focused {
            let col = self.search.cursor_display_col();
            if col < self.search_scroll as usize {
                self.search_scroll = col as u16;
            } else if col >= self.search_scroll as usize + search_width {
                self.search_scroll = col.saturating_sub(search_width - 1) as u16;
            }
            frame.set_cursor_position((
                self.search_input_rect.x + (col - self.search_scroll as usize) as u16,
                self.search_input_rect.y,
            ));
            self.cursor_on_screen = true;
        } else {
            self.search_scroll = 0;
        }
        frame.render_widget(
            Paragraph::new(self.search.buf())
                .scroll((0, self.search_scroll))
                .style(Style::default().bg(search_bg)),
            self.search_input_rect,
        );
        let hint = if self.tab == 2 {
            " ←/→ tabs · ↑/↓ rows · Space select · / search "
        } else {
            " ←/→ tabs · ↑/↓ rows · u/d detail · / search "
        };
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().fg(t.subtle_fg.into())),
            Rect::new(area.x, area.y + 2, area.width, 1),
        );
        let indices = self.filtered_indices(&state);
        self.selected = self.selected.min(indices.len().saturating_sub(1));
        let body_y = area.y + 3;
        let body_height = area.height.saturating_sub(5);
        self.body_height = body_height;
        let list_width = (area.width * 2 / 5)
            .max(20)
            .min(area.width.saturating_sub(1));
        let list_area = Rect::new(area.x, body_y, list_width, body_height);
        let detail_area = Rect::new(
            area.x + list_width + 1,
            body_y,
            area.width.saturating_sub(list_width + 1),
            body_height,
        );
        self.detail_rect = detail_area;
        self.scroll = self
            .scroll
            .min(indices.len().saturating_sub(body_height as usize) as u32);
        self.row_rects.clear();
        let mut list_lines = Vec::new();
        for (row, &index) in indices
            .iter()
            .enumerate()
            .skip(self.scroll as usize)
            .take(body_height as usize)
        {
            let label = match self.tab {
                0 => format!(
                    "{}{}",
                    state.confessions[index]
                        .category
                        .as_deref()
                        .map(|c| format!("[{c}] "))
                        .unwrap_or_default(),
                    state.confessions[index].confession.trigger
                ),
                1 => state.rules[index].name.clone(),
                _ => {
                    let proposal = &state.proposals[index];
                    let trigger = state
                        .confessions
                        .iter()
                        .find(|view| view.confession.id == proposal.id)
                        .map(|view| view.confession.trigger.as_str())
                        .unwrap_or("Unknown confession");
                    format!(
                        "{} {} · {}",
                        if self.selected_proposals.contains(&proposal.id) {
                            "[x]"
                        } else {
                            "[ ]"
                        },
                        proposal.category,
                        trigger
                    )
                }
            };
            let marker = if row == self.selected { "▸" } else { " " };
            list_lines.push(Line::styled(
                format!(
                    "{marker} {}",
                    crate::width::truncate(&label, list_width.saturating_sub(3) as usize)
                ),
                Style::default()
                    .fg(
                        if row == self.selected || self.hovered == Some(HoverTarget::Row(row)) {
                            t.accent.into()
                        } else {
                            t.subtle_fg.into()
                        },
                    )
                    .bg(if self.hovered == Some(HoverTarget::Row(row)) {
                        t.highlight_bg.into()
                    } else {
                        t.work_bg.into()
                    }),
            ));
            self.row_rects.push((
                row,
                Rect::new(
                    list_area.x,
                    list_area.y + (row - self.scroll as usize) as u16,
                    list_area.width,
                    1,
                ),
            ));
        }
        frame.render_widget(Paragraph::new(list_lines), list_area);
        let detail = match indices.get(self.selected).copied() {
            Some(index)
                if self.tab == 0
                    && self.show_history
                    && state.history_id.as_ref()
                        == Some(&state.confessions[index].confession.id) =>
            {
                let view = &state.confessions[index];
                let mut text = format!(
                    "{}\nCreated: {}\n\n",
                    view.confession.trigger, view.confession.created_at
                );
                for (index, change) in state.history.iter().enumerate() {
                    let line = match change {
                        ConfessionChange::Revised {
                            changed_at, fields, ..
                        } => {
                            format!(
                                "{} · revised · {}\nTrigger: {}\nRule: {}\nWhat I did: {}\nWhy: {}\nMitigation: {}\n",
                                index + 1,
                                changed_at,
                                fields.trigger,
                                fields.rule_violated,
                                fields.what_i_did,
                                fields.why,
                                fields.mitigation
                            )
                        }
                        ConfessionChange::Organized {
                            changed_at,
                            category,
                            ..
                        } => format!("{} · organized as {} · {}", index + 1, category, changed_at),
                        ConfessionChange::Archived {
                            changed_at, reason, ..
                        } => format!("{} · archived: {} · {}", index + 1, reason, changed_at),
                    };
                    text.push_str(&line);
                    text.push('\n');
                }
                text
            }
            Some(index) if self.tab == 0 => {
                let view = &state.confessions[index];
                let c = &view.confession;
                format!(
                    "{}\n\nRule: {}\nCreated: {}\nRevision: {}\nCategory: {}\n\nWhat I did\n{}\n\nWhy\n{}\n\nMitigation\n{}",
                    c.trigger,
                    c.rule_violated,
                    c.created_at.format("%Y-%m-%d %H:%M"),
                    view.revision,
                    view.category.as_deref().unwrap_or("—"),
                    c.what_i_did,
                    c.why,
                    c.mitigation
                )
            }
            Some(index) if self.tab == 1 => {
                let rule = &state.rules[index];
                format!(
                    "{}\n\n{} · {}\n{}\n\n{}",
                    rule.name, rule.scope, rule.source, rule.source_path, rule.content
                )
            }
            Some(index) => {
                let p = &state.proposals[index];
                let confession = state
                    .confessions
                    .iter()
                    .find(|view| view.confession.id == p.id);
                format!(
                    "{}\nRule: {}\nID: {}\n\nCategory: {}\n\nReason: {}\n\nRelated: {}\n\nSpace: select/deselect",
                    confession
                        .map(|view| view.confession.trigger.as_str())
                        .unwrap_or("Unknown confession"),
                    confession
                        .map(|view| view.confession.rule_violated.as_str())
                        .unwrap_or(""),
                    p.id,
                    p.category,
                    p.reason,
                    p.related_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }
            None => {
                if state.loading {
                    "Generating suggestions…".into()
                } else {
                    "No matching entries".into()
                }
            }
        };
        let detail_lines = crate::width::word_wrap(&detail, detail_area.width as usize);
        let max_detail_scroll = detail_lines
            .len()
            .saturating_sub(detail_area.height as usize) as u16;
        self.detail_scroll = self.detail_scroll.min(max_detail_scroll);
        frame.render_widget(
            Paragraph::new(detail_lines.join("\n")).scroll((self.detail_scroll, 0)),
            detail_area,
        );
        let status = Rect::new(area.x, area.y + area.height - 2, area.width, 1);
        frame.render_widget(
            Paragraph::new(format!(" {} entries · {}", indices.len(), state.message))
                .style(Style::default().fg(t.subtle_fg.into())),
            status,
        );
        self.action_rects.clear();
        let actions: &[&str] = match self.tab {
            0 => &["Edit", "History", "Organize", "Refresh"],
            1 => &["Reload", "Refresh"],
            _ if state.loading => &["Stop"],
            _ if state.applying => &["Applying…"],
            _ => &["Select All", "Apply", "Organize", "Refresh"],
        };
        let mut x = area.x;
        for &name in actions {
            let label = Self::action_label(name);
            let width = crate::width::width(label) as u16 + 1;
            let rect = Rect::new(
                x,
                area.y + area.height - 1,
                width.min(area.x + area.width - x),
                1,
            );
            frame.render_widget(
                Paragraph::new(label).style(
                    Style::default()
                        .fg(if self.hovered == Some(HoverTarget::Action(name)) {
                            t.accent.into()
                        } else {
                            t.subtle_fg.into()
                        })
                        .bg(if self.hovered == Some(HoverTarget::Action(name)) {
                            t.highlight_bg.into()
                        } else {
                            t.work_bg.into()
                        }),
                ),
                rect,
            );
            if name != "Applying…" {
                self.action_rects.push((name, rect));
            }
            x = x.saturating_add(width + 1);
            if x >= area.x + area.width {
                break;
            }
        }
        Vec::new()
    }

    fn handle_event(&mut self, event: &WmEvent, _ctx: &mut EventCtx) -> WmEventResult {
        if let WmEvent::Paste(text) = event {
            if let Some(edit) = &mut self.edit {
                if !edit.saving {
                    edit.fields[edit.focused].paste_multiline(text);
                    edit.follow_cursor = true;
                }
                return WmEventResult::Consumed(Vec::new());
            }
            if self.search_focused {
                self.search.paste_single_line(text);
                self.selected = 0;
                return WmEventResult::Consumed(Vec::new());
            }
        }
        if let WmEvent::Mouse(mouse) = event
            && matches!(mouse.kind, MouseEventKind::Moved)
        {
            self.hovered = self.hover_at(mouse.column, mouse.row);
            return if self.hovered.is_some() {
                WmEventResult::Consumed(Vec::new())
            } else {
                WmEventResult::Ignored
            };
        }
        if let WmEvent::Mouse(mouse) = event
            && matches!(
                mouse.kind,
                MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            )
        {
            let down = matches!(mouse.kind, MouseEventKind::ScrollDown);
            if let Some(edit) = &mut self.edit {
                if let Some(index) = self
                    .edit_segment_rects
                    .iter()
                    .position(|rect| rect.contains((mouse.column, mouse.row).into()))
                {
                    let rect = self.edit_text_rects[index];
                    let max_scroll =
                        visual_line_count(edit.fields[index].buf(), rect.width.max(1) as usize)
                            .saturating_sub(rect.height as usize) as u16;
                    edit.scroll[index] = if down {
                        edit.scroll[index].saturating_add(2).min(max_scroll)
                    } else {
                        edit.scroll[index].saturating_sub(2)
                    };
                    edit.follow_cursor = false;
                }
                return WmEventResult::Consumed(Vec::new());
            }
            if self.detail_rect.contains((mouse.column, mouse.row).into()) {
                self.detail_scroll = if down {
                    self.detail_scroll.saturating_add(3)
                } else {
                    self.detail_scroll.saturating_sub(3)
                };
            } else {
                let total = {
                    let state = self.state.lock().unwrap();
                    self.filtered_indices(&state).len()
                };
                self.scroll = if down {
                    self.scroll
                        .saturating_add(3)
                        .min(total.saturating_sub(self.body_height as usize) as u32)
                } else {
                    self.scroll.saturating_sub(3)
                };
                self.selected = self.scroll as usize;
            }
            return WmEventResult::Consumed(Vec::new());
        }
        if let WmEvent::Mouse(mouse) = event
            && matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
        {
            if let Some((name, _)) = self
                .action_rects
                .iter()
                .find(|(_, rect)| rect.contains((mouse.column, mouse.row).into()))
            {
                let name = *name;
                self.action(name);
                return WmEventResult::Consumed(Vec::new());
            }
            if let Some(edit) = &mut self.edit {
                if edit.saving {
                    return WmEventResult::Consumed(Vec::new());
                }
                if let Some(index) = self
                    .edit_segment_rects
                    .iter()
                    .position(|rect| rect.contains((mouse.column, mouse.row).into()))
                {
                    edit.focused = index;
                    let rect = self.edit_text_rects[index];
                    if rect.contains((mouse.column, mouse.row).into()) {
                        let row =
                            mouse.row.saturating_sub(rect.y) as usize + edit.scroll[index] as usize;
                        let col = mouse.column.saturating_sub(rect.x) as usize;
                        let pos = cursor_from_wrapped(
                            edit.fields[index].buf(),
                            row,
                            col,
                            rect.width.max(1) as usize,
                        );
                        edit.fields[index].set_cursor(pos);
                    }
                    edit.follow_cursor = true;
                    return WmEventResult::Consumed(Vec::new());
                }
            } else {
                if let Some(index) = self
                    .tab_rects
                    .iter()
                    .position(|rect| rect.contains((mouse.column, mouse.row).into()))
                {
                    self.tab = index;
                    self.selected = 0;
                    self.scroll = 0;
                    self.search_focused = false;
                    return WmEventResult::Consumed(Vec::new());
                }
                if self.search_rect.contains((mouse.column, mouse.row).into()) {
                    self.search_focused = true;
                    let col = if mouse.column < self.search_input_rect.x {
                        0
                    } else {
                        mouse.column.saturating_sub(self.search_input_rect.x) as usize
                            + self.search_scroll as usize
                    };
                    self.search.set_cursor_by_display(0, col as u16);
                    return WmEventResult::Consumed(Vec::new());
                }
                if let Some((row, _)) = self
                    .row_rects
                    .iter()
                    .find(|(_, rect)| rect.contains((mouse.column, mouse.row).into()))
                {
                    self.selected = *row;
                    self.show_history = false;
                    self.search_focused = false;
                    if self.tab == 2 {
                        let state = self.state.lock().unwrap();
                        if let Some(&index) = self.filtered_indices(&state).get(self.selected) {
                            let id = state.proposals[index].id.clone();
                            if !self.selected_proposals.remove(&id) {
                                self.selected_proposals.insert(id);
                            }
                        }
                    }
                    return WmEventResult::Consumed(Vec::new());
                }
                self.search_focused = false;
            }
        }
        let WmEvent::Key(action) = event else {
            return WmEventResult::Ignored;
        };
        if let Some(edit) = &mut self.edit {
            if edit.saving {
                return WmEventResult::Consumed(Vec::new());
            }
            match action {
                KeyAction::Escape => self.edit = None,
                KeyAction::Tab => {
                    edit.focused = (edit.focused + 1) % 5;
                    edit.follow_cursor = true;
                }
                KeyAction::BackTab => {
                    edit.focused = (edit.focused + 4) % 5;
                    edit.follow_cursor = true;
                }
                KeyAction::Submit => self.action("Save"),
                KeyAction::HistoryUp => {
                    edit.fields[edit.focused].move_line_up_visual(
                        self.edit_text_rects[edit.focused].width.max(1) as usize,
                    );
                    edit.follow_cursor = true;
                }
                KeyAction::HistoryDown => {
                    edit.fields[edit.focused].move_line_down_visual(
                        self.edit_text_rects[edit.focused].width.max(1) as usize,
                    );
                    edit.follow_cursor = true;
                }
                _ => {
                    if edit.fields[edit.focused].handle_key(action) {
                        edit.follow_cursor = true;
                    }
                }
            }
            return WmEventResult::Consumed(Vec::new());
        }
        if self.search_focused {
            match action {
                KeyAction::Escape | KeyAction::Submit => self.search_focused = false,
                KeyAction::Newline => {}
                _ => {
                    if self.search.handle_key(action) {
                        self.selected = 0;
                    }
                }
            }
            return WmEventResult::Consumed(Vec::new());
        }
        let total = {
            let state = self.state.lock().unwrap();
            self.filtered_indices(&state).len()
        };
        match action {
            KeyAction::HistoryUp | KeyAction::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.show_history = false;
            }
            KeyAction::HistoryDown | KeyAction::Char('j') => {
                self.selected = (self.selected + 1).min(total.saturating_sub(1));
                self.show_history = false;
            }
            KeyAction::Char('u') => {
                self.detail_scroll = self.detail_scroll.saturating_sub(self.body_height)
            }
            KeyAction::Char('d') => {
                self.detail_scroll = self.detail_scroll.saturating_add(self.body_height)
            }
            KeyAction::Char('/') => self.search_focused = true,
            KeyAction::Char('e') if self.tab == 0 => self.action("Edit"),
            KeyAction::Char('h') if self.tab == 0 => self.action("History"),
            KeyAction::Char('o') => self.action("Organize"),
            KeyAction::Char('c') if self.tab == 2 => self.action("Stop"),
            KeyAction::Char('r') => self.action("Refresh"),
            KeyAction::Char('R') if self.tab == 1 => self.action("Reload"),
            KeyAction::Char('a') if self.tab == 2 => self.action("Apply"),
            KeyAction::Char('s') if self.tab == 2 => self.action("Select All"),
            KeyAction::Char(' ') if self.tab == 2 => {
                let state = self.state.lock().unwrap();
                if let Some(&index) = self.filtered_indices(&state).get(self.selected) {
                    let id = state.proposals[index].id.clone();
                    if !self.selected_proposals.remove(&id) {
                        self.selected_proposals.insert(id);
                    }
                }
            }
            KeyAction::CursorLeft => {
                self.tab = self.tab.saturating_sub(1);
                self.selected = 0;
                self.scroll = 0;
            }
            KeyAction::CursorRight => {
                self.tab = (self.tab + 1).min(2);
                self.selected = 0;
                self.scroll = 0;
            }
            _ => return WmEventResult::Ignored,
        }
        if self.selected < self.scroll as usize {
            self.scroll = self.selected as u32;
        }
        if self.selected >= self.scroll as usize + self.body_height as usize {
            self.scroll = (self.selected + 1).saturating_sub(self.body_height as usize) as u32;
        }
        WmEventResult::Consumed(Vec::new())
    }

    fn cursor_visible(&self) -> bool {
        self.cursor_on_screen
    }

    fn on_close(&mut self) -> CloseOutcome {
        if self.edit.is_some() {
            CloseOutcome::Block("Discard or save the confession edit first".into())
        } else {
            let mut state = self.state.lock().unwrap();
            if state.loading {
                state.loading = false;
                state.organize_request = state.organize_request.wrapping_add(1);
                drop(state);
                self.send(TuiControl::CancelOrganization);
            }
            CloseOutcome::Close
        }
    }

    fn preferred_size(&self, _viewport: Rect) -> SizeHint {
        SizeHint {
            min: (55, 16),
            max: None,
            preferred: (100, 36),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::memory::Confession;
    use crossterm::event::{KeyModifiers, MouseButton, MouseEvent};
    use ratatui::{Terminal, backend::TestBackend};

    fn sample_view() -> ConfessionView {
        ConfessionView {
            confession: Confession {
                id: MemoryId::now(),
                trigger: "initial".into(),
                rule_violated: "rule".into(),
                what_i_did: "did".into(),
                why: "reason".into(),
                mitigation: "fix".into(),
                anchors: Vec::new(),
                created_at: chrono::Utc::now(),
            },
            revision: 0,
            category: None,
            related_ids: Vec::new(),
            archived: false,
        }
    }

    #[test]
    fn edit_keeps_draft_until_save_ack() {
        let view = sample_view();
        let state = Arc::new(Mutex::new(KnowledgeState {
            confessions: vec![view.clone()],
            ..Default::default()
        }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut panel = KnowledgePanelContent::new(state.clone(), Some(tx));
        panel.action("Edit");
        let mut scroll = 0;
        let mut h_scroll = 0;
        let mut ctx = EventCtx {
            scroll: &mut scroll,
            h_scroll: &mut h_scroll,
        };
        panel.handle_event(&WmEvent::Key(KeyAction::Char('!')), &mut ctx);
        panel.action("Save");
        assert!(panel.edit.is_some());
        let control = rx.try_recv().unwrap();
        assert!(
            matches!(control, TuiControl::ReviseConfession { fields, .. } if fields.trigger == "initial!")
        );
        assert!(state.lock().unwrap().saved_id.is_none());
        panel.action("Save");
        assert!(rx.try_recv().is_err());
        state.lock().unwrap().failed_save_id = Some(view.confession.id.clone());
        panel.sync_edit_result();
        assert!(panel.edit.as_ref().is_some_and(|edit| !edit.saving));
        panel.action("Save");
        assert!(matches!(
            rx.try_recv(),
            Ok(TuiControl::ReviseConfession { .. })
        ));
        state.lock().unwrap().saved_id = Some(view.confession.id);
        panel.sync_edit_result();
        assert!(panel.edit.is_none());
    }

    #[test]
    fn proposal_row_mouse_click_selects_it() {
        let view = sample_view();
        let id = view.confession.id;
        let state = Arc::new(Mutex::new(KnowledgeState {
            proposals: vec![OrganizationProposal {
                id: id.clone(),
                base_revision: 0,
                category: "workflow".into(),
                related_ids: Vec::new(),
                reason: "similar mitigation".into(),
            }],
            ..Default::default()
        }));
        let mut panel = KnowledgePanelContent::new(state, None);
        panel.tab = 2;
        panel.row_rects = vec![(0, Rect::new(1, 2, 20, 1))];
        let event = WmEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row: 2,
            modifiers: KeyModifiers::NONE,
        });
        let mut scroll = 0;
        let mut h_scroll = 0;
        let mut ctx = EventCtx {
            scroll: &mut scroll,
            h_scroll: &mut h_scroll,
        };
        assert!(matches!(
            panel.handle_event(&event, &mut ctx),
            WmEventResult::Consumed(_)
        ));
        assert!(panel.selected_proposals.contains(&id));
        panel.action("Select All");
        assert!(panel.selected_proposals.is_empty());
        panel.action("Select All");
        assert!(panel.selected_proposals.contains(&id));
    }

    #[test]
    fn panel_renders_confessions_and_rules_at_small_and_large_sizes() {
        let state = Arc::new(Mutex::new(KnowledgeState {
            confessions: vec![sample_view()],
            rules: vec![RuleView {
                name: "AGENTS.md".into(),
                description: "project rules".into(),
                scope: "project".into(),
                source: "file".into(),
                source_path: "/project/AGENTS.md".into(),
                content: "Use keyboard and mouse".into(),
            }],
            ..Default::default()
        }));
        let mut panel = KnowledgePanelContent::new(state, None);
        let empty_index = std::collections::HashMap::new();
        let empty_details = std::collections::HashMap::new();
        let empty_set = std::collections::HashSet::new();
        let resources = std::collections::HashMap::new();
        let prompts = std::collections::HashMap::new();
        let browser = crate::mcp_manager::McpBrowserState {
            tab: crate::mcp_manager::McpBrowserTab::default(),
            content_revision: 0,
            resources: &resources,
            prompts: &prompts,
        };
        for (width, height) in [(100, 36), (55, 16)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            for tab in 0..2 {
                panel.tab = tab;
                terminal
                    .draw(|frame| {
                        panel.render_content(
                            frame.area(),
                            frame,
                            &RenderCtx {
                                window_id: crate::wm::WindowId(1),
                                snapshots: &[],
                                items: &[],
                                item_revisions: &[],
                                handle_index: &empty_index,
                                detached_task_details: &empty_details,
                                task_handle_index: &empty_index,
                                workflow_run_to_panel: &empty_index,
                                task_snapshots_revision: 0,
                                interaction_revision: 0,
                                animation_frame: 0,
                                expanded_tools: &empty_set,
                                activity_nodes: &[],
                                mcp_servers: &[],
                                expanded_mcp_servers: &empty_set,
                                mcp_selected: 0,
                                hovered_mcp_row: &None,
                                mcp_browser: &browser,
                                hovered_history_row: &None,
                            },
                        );
                    })
                    .unwrap();
                let content = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(content.contains(if tab == 0 { "initial" } else { "AGENTS.md" }));
                if tab == 0 {
                    assert!(content.contains("[E]dit"));
                    assert!(content.contains("[H]istory"));
                }
            }
        }
    }

    #[test]
    fn editor_uses_real_cursor_and_clicks_inside_cjk_text() {
        let mut view = sample_view();
        view.confession.trigger = "你好world".into();
        let state = Arc::new(Mutex::new(KnowledgeState {
            confessions: vec![view],
            ..Default::default()
        }));
        let mut panel = KnowledgePanelContent::new(state, None);
        panel.action("Edit");
        panel.edit.as_mut().unwrap().fields[0].set_cursor("你".len());
        let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
        terminal
            .draw(|frame| panel.render_editor(frame.area(), frame))
            .unwrap();
        let rect = panel.edit_text_rects[0];
        let cursor = terminal.backend().cursor_position();
        assert_eq!((cursor.x, cursor.y), (rect.x + 2, rect.y));
        assert!(panel.cursor_visible());
        assert!(
            terminal
                .backend()
                .buffer()
                .content()
                .iter()
                .all(|cell| cell.symbol() != "▏")
        );
        assert_eq!(panel.edit.as_ref().unwrap().fields[0].buf(), "你好world");

        let mut scroll = 0;
        let mut h_scroll = 0;
        let mut ctx = EventCtx {
            scroll: &mut scroll,
            h_scroll: &mut h_scroll,
        };
        let moved = WmEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: rect.x + 4,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        panel.handle_event(&moved, &mut ctx);
        assert_eq!(panel.hovered, Some(HoverTarget::Field(0)));
        terminal
            .draw(|frame| panel.render_editor(frame.area(), frame))
            .unwrap();
        let segment = panel.edit_segment_rects[0];
        assert_eq!(
            terminal.backend().buffer()[(segment.x, segment.y)].bg,
            crate::theme::theme().highlight_bg.into()
        );
        let outside = WmEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Moved,
            column: 59,
            row: 0,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(
            panel.handle_event(&outside, &mut ctx),
            WmEventResult::Ignored
        ));
        assert!(panel.hovered.is_none());
        let clicked = WmEvent::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x + 4,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        });
        panel.handle_event(&clicked, &mut ctx);
        assert_eq!(
            panel.edit.as_ref().unwrap().fields[0].cursor(),
            "你好".len()
        );
        panel.handle_event(&WmEvent::Key(KeyAction::Char('!')), &mut ctx);
        assert_eq!(panel.edit.as_ref().unwrap().fields[0].buf(), "你好!world");
    }

    #[test]
    fn editor_click_maps_wrapped_row_to_original_byte() {
        let mut view = sample_view();
        view.confession.trigger = "hello world".into();
        let state = Arc::new(Mutex::new(KnowledgeState {
            confessions: vec![view],
            ..Default::default()
        }));
        let mut panel = KnowledgePanelContent::new(state, None);
        panel.action("Edit");
        panel.edit_segment_rects[0] = Rect::new(1, 3, 20, 3);
        panel.edit_text_rects[0] = Rect::new(4, 4, 6, 2);
        let mut scroll = 0;
        let mut h_scroll = 0;
        let mut ctx = EventCtx {
            scroll: &mut scroll,
            h_scroll: &mut h_scroll,
        };
        panel.handle_event(
            &WmEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 6,
                row: 5,
                modifiers: KeyModifiers::NONE,
            }),
            &mut ctx,
        );
        assert_eq!(panel.edit.as_ref().unwrap().fields[0].cursor(), 8);
        panel.handle_event(&WmEvent::Key(KeyAction::Char('X')), &mut ctx);
        assert_eq!(panel.edit.as_ref().unwrap().fields[0].buf(), "hello woXrld");
    }

    #[test]
    fn editor_cursor_remains_visible_at_exact_wrap_edge() {
        let mut view = sample_view();
        view.confession.trigger = "abcdef".into();
        let state = Arc::new(Mutex::new(KnowledgeState {
            confessions: vec![view],
            ..Default::default()
        }));
        let mut panel = KnowledgePanelContent::new(state, None);
        panel.action("Edit");
        let mut terminal = Terminal::new(TestBackend::new(20, 16)).unwrap();
        terminal
            .draw(|frame| panel.render_editor(Rect::new(0, 0, 9, 16), frame))
            .unwrap();
        let rect = panel.edit_text_rects[0];
        assert_eq!(rect.width, 6);
        let cursor = terminal.backend().cursor_position();
        assert_eq!((cursor.x, cursor.y), (rect.x, rect.y + 1));
        assert!(panel.cursor_visible());
    }

    #[test]
    fn editor_scroll_and_click_keep_long_text_cursor_aligned() {
        let mut view = sample_view();
        view.confession.trigger = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj".into();
        let state = Arc::new(Mutex::new(KnowledgeState {
            confessions: vec![view],
            ..Default::default()
        }));
        let mut panel = KnowledgePanelContent::new(state, None);
        panel.action("Edit");
        let mut terminal = Terminal::new(TestBackend::new(55, 16)).unwrap();
        terminal
            .draw(|frame| panel.render_editor(frame.area(), frame))
            .unwrap();
        let rect = panel.edit_text_rects[0];
        assert!(panel.edit.as_ref().unwrap().scroll[0] > 0);
        assert!(panel.cursor_visible());
        let mut scroll = 0;
        let mut h_scroll = 0;
        let mut ctx = EventCtx {
            scroll: &mut scroll,
            h_scroll: &mut h_scroll,
        };
        panel.handle_event(
            &WmEvent::Mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: rect.x,
                row: rect.y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut ctx,
        );
        let visible_row = panel.edit.as_ref().unwrap().scroll[0] as usize;
        panel.handle_event(
            &WmEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x,
                row: rect.y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut ctx,
        );
        let edit = panel.edit.as_ref().unwrap();
        assert_eq!(edit.fields[0].cursor(), visible_row * 2);
        panel.cursor_on_screen = false;
        terminal
            .draw(|frame| panel.render_editor(frame.area(), frame))
            .unwrap();
        assert!(panel.cursor_visible());
    }

    #[test]
    fn search_click_moves_cursor_without_inserting_padding() {
        let state = Arc::new(Mutex::new(KnowledgeState::default()));
        let mut panel = KnowledgePanelContent::new(state, None);
        panel.search.insert_str("hello world");
        panel.search_rect = Rect::new(1, 1, 40, 1);
        panel.search_input_rect = Rect::new(13, 1, 28, 1);
        let mut scroll = 0;
        let mut h_scroll = 0;
        let mut ctx = EventCtx {
            scroll: &mut scroll,
            h_scroll: &mut h_scroll,
        };
        panel.handle_event(
            &WmEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 19,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
            &mut ctx,
        );
        assert!(panel.search_focused);
        panel.handle_event(&WmEvent::Key(KeyAction::Char('X')), &mut ctx);
        assert_eq!(panel.search.buf(), "hello Xworld");
    }

    #[test]
    fn organization_request_can_be_cancelled_without_reusing_its_id() {
        let state = Arc::new(Mutex::new(KnowledgeState::default()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut panel = KnowledgePanelContent::new(state.clone(), Some(tx));
        panel.action("Organize");
        assert!(matches!(
            rx.try_recv(),
            Ok(TuiControl::SuggestOrganization { request_id: 1 })
        ));
        panel.action("Organize");
        assert!(rx.try_recv().is_err());
        panel.action("Stop");
        assert!(matches!(rx.try_recv(), Ok(TuiControl::CancelOrganization)));
        assert_eq!(state.lock().unwrap().organize_request, 2);
    }

    #[test]
    fn organization_apply_sends_one_reviewed_batch() {
        let view = sample_view();
        let id = view.confession.id.clone();
        let state = Arc::new(Mutex::new(KnowledgeState {
            confessions: vec![view],
            proposals: vec![OrganizationProposal {
                id: id.clone(),
                base_revision: 0,
                category: "workflow".into(),
                related_ids: Vec::new(),
                reason: "similar mitigation".into(),
            }],
            ..Default::default()
        }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut panel = KnowledgePanelContent::new(state.clone(), Some(tx));
        panel.tab = 2;
        panel.search.insert_str("initial");
        assert_eq!(panel.filtered_indices(&state.lock().unwrap()), vec![0]);
        panel.selected_proposals.insert(id);
        panel.action("Apply");
        panel.action("Apply");
        assert!(matches!(
            rx.try_recv(),
            Ok(TuiControl::OrganizeConfessions(changes)) if changes.len() == 1
        ));
        assert!(rx.try_recv().is_err());
        assert!(state.lock().unwrap().applying);
    }
}
