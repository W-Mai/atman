use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use crossterm::event::{MouseButton, MouseEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::{Block, Paragraph, Wrap};
use tokio::sync::mpsc;

use atman_runtime::memory::MemoryId;
use atman_runtime::memory::confession::{ConfessionChange, ConfessionFields, ConfessionView};

use crate::input::{InputEditor, cursor_from_wrapped, visual_line_count, wrapped_cursor_position};
use crate::keys::KeyAction;
use crate::settings_form::{Action, render_action, render_record};
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
    pub archiving: bool,
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
    Details(usize),
    Search,
    Field(usize),
}

struct ArchiveConfirm {
    id: MemoryId,
    base_revision: u64,
    trigger: String,
}

pub struct KnowledgePanelContent {
    state: Arc<Mutex<KnowledgeState>>,
    control_tx: Option<mpsc::UnboundedSender<TuiControl>>,
    tab: usize,
    selected: usize,
    scroll: u32,
    detail_scroll: u16,
    list_rect: Rect,
    detail_rect: Rect,
    body_height: u16,
    visible_rows: usize,
    compact: bool,
    detail_open: bool,
    search: InputEditor,
    search_focused: bool,
    search_scroll: u16,
    edit: Option<EditState>,
    archive_confirm: Option<ArchiveConfirm>,
    selected_proposals: HashSet<MemoryId>,
    show_history: bool,
    tab_rects: [Rect; 3],
    search_rect: Rect,
    search_input_rect: Rect,
    row_rects: Vec<(usize, Rect)>,
    detail_toggle_rects: Vec<(usize, Rect)>,
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
            list_rect: Rect::default(),
            detail_rect: Rect::default(),
            body_height: 0,
            visible_rows: 1,
            compact: false,
            detail_open: false,
            search: InputEditor::default(),
            search_focused: false,
            search_scroll: 0,
            edit: None,
            archive_confirm: None,
            selected_proposals: HashSet::new(),
            show_history: false,
            tab_rects: [Rect::default(); 3],
            search_rect: Rect::default(),
            search_input_rect: Rect::default(),
            row_rects: Vec::new(),
            detail_toggle_rects: Vec::new(),
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
            "Archive" => "[X] Archive",
            "Confirm Archive" => "[Y] Archive",
            "Cancel Archive" => "[N] Cancel",
            "Archiving…" => "Archiving…",
            "History" => "[H]istory",
            "Organize" => "[O]rganize",
            "Refresh" => "[R]efresh",
            "Reload" => "[⇧R]eload",
            "Stop" => "[C]ancel",
            "Select All" => "[S]elect all",
            "Select" => "[Space] Select",
            "Apply" => "[A]pply",
            "Applying…" => "Applying…",
            _ => "",
        }
    }

    fn compact_action_label(name: &str) -> &'static str {
        match name {
            "Edit" => "[E]dit",
            "Archive" => "[X]arc",
            "History" => "[H]ist",
            "Organize" => "[O]rg",
            "Refresh" => "[R]ef",
            "Select All" => "[S] All",
            _ => Self::action_label(name),
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
        if let Some((index, _)) = self
            .detail_toggle_rects
            .iter()
            .find(|(_, rect)| rect.contains(point))
        {
            return Some(HoverTarget::Details(*index));
        }
        self.row_rects
            .iter()
            .find(|(_, rect)| rect.contains(point))
            .map(|(index, _)| HoverTarget::Row(*index))
    }

    fn action(&mut self, name: &str) {
        self.search_focused = false;
        self.clear_geometry();
        if let Some(confirm) = self.archive_confirm.take() {
            match name {
                "Confirm Archive" => {
                    let mut state = self.state.lock().unwrap();
                    state.archiving = true;
                    state.message = "Archiving confession…".into();
                    drop(state);
                    self.send(TuiControl::ArchiveConfession {
                        id: confirm.id,
                        base_revision: confirm.base_revision,
                    });
                }
                "Cancel Archive" => {
                    self.state.lock().unwrap().message = "Archive cancelled".into();
                }
                _ => self.archive_confirm = Some(confirm),
            }
            return;
        }
        let mut state = self.state.lock().unwrap();
        let indices = self.filtered_indices(&state);
        match name {
            "Refresh" => {
                state.message = "Refreshing memory & rules…".into();
                drop(state);
                self.send(TuiControl::ListKnowledge);
            }
            "Reload" => {
                state.message = "Reloading rules…".into();
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
            "Archive" => {
                if self.tab == 0
                    && !state.archiving
                    && let Some(&index) = indices.get(self.selected)
                    && let Some(view) = state.confessions.get(index)
                {
                    self.archive_confirm = Some(ArchiveConfirm {
                        id: view.confession.id.clone(),
                        base_revision: view.revision,
                        trigger: view.confession.trigger.clone(),
                    });
                    self.detail_open = self.compact;
                    self.detail_scroll = 0;
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
                    self.detail_open = self.compact;
                    self.detail_scroll = 0;
                    if self.show_history {
                        self.send(TuiControl::GetConfessionHistory(id));
                    }
                }
            }
            "Organize" => {
                if state.loading || state.applying || state.archiving {
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
                self.detail_open = false;
                self.selected = 0;
                self.scroll = 0;
                self.detail_scroll = 0;
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
            "Select" if self.tab == 2 => {
                if let Some(&index) = indices.get(self.selected) {
                    let id = state.proposals[index].id.clone();
                    if !self.selected_proposals.remove(&id) {
                        self.selected_proposals.insert(id);
                    }
                }
            }
            "Cancel" if self.edit.as_ref().is_some_and(|edit| !edit.saving) => self.edit = None,
            _ => {}
        }
    }

    fn clear_geometry(&mut self) {
        self.tab_rects = [Rect::default(); 3];
        self.search_rect = Rect::default();
        self.search_input_rect = Rect::default();
        self.list_rect = Rect::default();
        self.detail_rect = Rect::default();
        self.row_rects.clear();
        self.detail_toggle_rects.clear();
        self.action_rects.clear();
        self.edit_segment_rects = [Rect::default(); 5];
        self.edit_text_rects = [Rect::default(); 5];
        self.hovered = None;
        self.cursor_on_screen = false;
    }

    fn render_editor(&mut self, area: Rect, frame: &mut Frame) {
        self.edit_segment_rects = [Rect::default(); 5];
        self.edit_text_rects = [Rect::default(); 5];
        self.action_rects.clear();
        self.cursor_on_screen = false;
        let Some(edit) = &mut self.edit else {
            return;
        };
        let t = crate::theme::theme();
        let surface = crate::wm::shell::surface_bg(&t);
        frame.render_widget(Block::default().style(Style::default().bg(surface)), area);
        if area.width < 5 || area.height < 10 {
            frame.render_widget(Paragraph::new("Enlarge window to edit memory"), area);
            return;
        }
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
        let title = if area.width < 60 {
            format!(
                " Edit · field {}/5 · revision {}",
                edit.focused + 1,
                edit.revision + 1
            )
        } else {
            format!(
                " Edit confession · revision {} · field {}/5",
                edit.revision + 1,
                edit.focused + 1
            )
        };
        frame.render_widget(
            Paragraph::new(title).style(Style::default().fg(t.subtle_fg.into()).bg(surface)),
            Rect::new(area.x, area.y, area.width, 1),
        );
        let body_rows = area.height.saturating_sub(7);
        let visible = (body_rows / 5).clamp(1, 5) as usize;
        let start = edit.focused.saturating_sub(visible - 1);
        let base_rows = body_rows / visible as u16;
        let extra_rows = body_rows % visible as u16;
        let mut y = area.y + 2;
        for (row, index) in (start..start + visible).enumerate() {
            let rows = base_rows + u16::from(row < extra_rows as usize);
            let segment = Rect::new(area.x, y, area.width, rows);
            let text_rect = Rect::new(
                area.x + 2,
                y + 2,
                area.width.saturating_sub(4),
                rows.saturating_sub(3),
            );
            self.edit_segment_rects[index] = segment;
            self.edit_text_rects[index] = text_rect;
            let focused = edit.focused == index;
            let bg = render_record(
                frame,
                segment,
                focused,
                self.hovered == Some(HoverTarget::Field(index)) && !edit.saving,
                &t,
            );
            frame.render_widget(
                Paragraph::new(labels[index]).style(Style::default().fg(t.subtle_fg.into()).bg(bg)),
                Rect::new(text_rect.x, y + 1, text_rect.width, 1),
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
            if let Some((cursor_row, _)) = cursor_position
                && edit.follow_cursor
            {
                let scroll = edit.scroll[index] as usize;
                if cursor_row < scroll {
                    edit.scroll[index] = cursor_row as u16;
                } else if cursor_row >= scroll + text_rect.height as usize {
                    edit.scroll[index] = cursor_row
                        .saturating_sub(text_rect.height as usize)
                        .saturating_add(1) as u16;
                }
            }
            edit.scroll[index] = edit.scroll[index].min(max_scroll as u16);
            frame.render_widget(
                Paragraph::new(edit.fields[index].buf())
                    .wrap(Wrap { trim: false })
                    .scroll((edit.scroll[index], 0))
                    .style(Style::default().fg(t.tinted_fg.into()).bg(bg)),
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
        let status = Rect::new(
            area.x + 1,
            area.bottom() - 5,
            area.width.saturating_sub(2),
            1,
        );
        let message = if edit.saving {
            "Saving…".to_string()
        } else {
            let state_message = self.state.lock().unwrap().message.clone();
            if state_message.is_empty() || state_message.contains(" confessions · ") {
                format!(
                    "Changed: {}",
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
        frame.render_widget(
            Paragraph::new(crate::width::truncate(&message, status.width as usize))
                .style(Style::default().fg(t.subtle_fg.into()).bg(surface)),
            status,
        );
        let button_width = area.width.saturating_sub(2) / 2;
        for (name, shortcut, rect) in [
            (
                "Save",
                "Enter",
                Rect::new(area.x, status.bottom(), button_width, 3),
            ),
            (
                "Cancel",
                "Esc",
                Rect::new(
                    area.x + button_width + 2,
                    status.bottom(),
                    area.width - button_width - 2,
                    3,
                ),
            ),
        ] {
            render_action(
                frame,
                rect,
                &Action {
                    label: name,
                    shortcut,
                    hovered: self.hovered == Some(HoverTarget::Action(name)),
                    disabled: edit.saving,
                    ..Default::default()
                },
                &t,
            );
            if !edit.saving {
                self.action_rects.push((name, rect));
            }
        }
        let help = if area.width < 29 {
            "Tab fields · ⇧↵ newline"
        } else if area.width < 48 {
            "Tab field · Shift+↵ newline"
        } else {
            "Tab/Shift+Tab fields · Shift+Enter newline"
        };
        frame.render_widget(
            Paragraph::new(help).style(Style::default().fg(t.subtle_fg.into()).bg(surface)),
            Rect::new(
                area.x + 1,
                area.bottom() - 1,
                area.width.saturating_sub(2),
                1,
            ),
        );
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
        let width = area.width.saturating_sub(4).min(112);
        let top = u16::from(area.height >= 22);
        let area = Rect::new(
            area.x + area.width.saturating_sub(width) / 2,
            area.y + top,
            width,
            area.height.saturating_sub(top * 2),
        );
        let hovered = self.hovered;
        self.clear_geometry();
        self.hovered = hovered;
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
        let surface = crate::wm::shell::surface_bg(&t);
        frame.render_widget(Block::default().style(Style::default().bg(surface)), area);
        self.compact = area.width < 84;
        if !self.compact {
            self.detail_open = false;
        }
        let mut tab_names = if area.width < 44 {
            ["Memory", "Rules", "Suggest"]
        } else {
            ["Confessions", "Rules", "Suggestions"]
        };
        if self.compact && self.detail_open {
            tab_names[self.tab] = "‹ List";
        }
        let tab_width = area.width.saturating_sub(2) / 3;
        for (index, name) in tab_names.into_iter().enumerate() {
            let x = area.x + index as u16 * (tab_width + 1);
            let rect = Rect::new(
                x,
                area.y,
                if index == 2 {
                    area.right() - x
                } else {
                    tab_width
                },
                3,
            );
            self.tab_rects[index] = rect;
            render_action(
                frame,
                rect,
                &Action {
                    label: name,
                    focused: self.tab == index,
                    hovered: self.hovered == Some(HoverTarget::Tab(index)),
                    ..Default::default()
                },
                &t,
            );
        }
        let search_rows = u16::from(!(self.compact && self.detail_open));
        self.search_rect = Rect::new(area.x, area.y + 3, area.width, search_rows);
        self.search_input_rect = Rect::new(
            area.x + 12,
            area.y + 3,
            area.width.saturating_sub(14),
            search_rows,
        );
        let search_bg = if self.search_focused {
            crate::theme::ThemeColor::new(surface).lerp(t.accent, 0.12)
        } else if self.hovered == Some(HoverTarget::Search) {
            crate::theme::ThemeColor::new(surface).lerp(t.work_hover_bg, 0.45)
        } else {
            surface
        };
        frame.render_widget(
            Block::default().style(Style::default().bg(search_bg)),
            self.search_rect,
        );
        frame.render_widget(
            Paragraph::new("  Search /")
                .style(Style::default().fg(t.subtle_fg.into()).bg(search_bg)),
            Rect::new(area.x, self.search_rect.y, 12, search_rows),
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
            Paragraph::new(crate::width::trim_display_offset(
                self.search.buf(),
                self.search_scroll as usize,
                search_width,
            ))
            .style(Style::default().fg(t.tinted_fg.into()).bg(search_bg)),
            self.search_input_rect,
        );
        let hint_rows = u16::from(area.height >= 20);
        let hint = if self.compact && self.detail_open {
            " Esc list · ↑/↓ scroll · PgUp/PgDn page"
        } else if self.tab == 2 {
            " ←/→ tabs · ↑/↓ rows · PgUp/PgDn page · Space select"
        } else {
            " ←/→ tabs · ↑/↓ rows · PgUp/PgDn page · u/d detail"
        };
        frame.render_widget(
            Paragraph::new(hint).style(Style::default().fg(t.subtle_fg.into()).bg(surface)),
            Rect::new(area.x, area.y + 3 + search_rows, area.width, hint_rows),
        );
        let actions: &[&str] = if self.archive_confirm.is_some() {
            &["Confirm Archive", "Cancel Archive"]
        } else if self.compact && self.detail_open {
            match self.tab {
                0 if state.archiving => &["Archiving…", "History"],
                0 => &["Edit", "Archive", "History"],
                1 => &["Reload", "Refresh"],
                _ if state.loading => &["Stop"],
                _ if state.applying => &["Applying…"],
                _ => &["Select", "Apply"],
            }
        } else if state.archiving && self.tab == 0 {
            &["Archiving…", "History", "Organize", "Refresh"]
        } else {
            match self.tab {
                0 => &["Edit", "Archive", "History", "Organize", "Refresh"],
                1 => &["Reload", "Refresh"],
                _ if state.loading => &["Stop"],
                _ if state.applying => &["Applying…"],
                _ => &["Select All", "Apply", "Organize", "Refresh"],
            }
        };
        let mut buttons = Vec::new();
        let mut button_x = 0;
        let mut button_y = 0;
        for &name in actions {
            let label = if area.width < 65 {
                Self::compact_action_label(name)
            } else {
                Self::action_label(name)
            };
            let width = (crate::width::width(label) as u16 + 2).min(area.width);
            if button_x > 0 && button_x + width > area.width {
                button_x = 0;
                button_y += 4;
            }
            buttons.push((
                name,
                label,
                Rect::new(area.x + button_x, button_y, width, 3),
            ));
            button_x += width + 1;
        }
        let footer_height = button_y + 4;
        let body_y = area.y + 3 + search_rows + hint_rows;
        let body_height = area
            .height
            .saturating_sub(3 + search_rows + hint_rows + footer_height);
        let stacked = self.compact;
        let list_width = if stacked {
            area.width
        } else {
            area.width * 2 / 5
        };
        let list_height = if self.compact && self.detail_open {
            0
        } else if stacked {
            (body_height / 2 / 3 * 3).max(3).min(body_height)
        } else {
            body_height
        };
        let list_area = Rect::new(area.x, body_y, list_width, list_height);
        self.list_rect = list_area;
        let detail_area = if list_height == 0 {
            Rect::new(
                area.x + 2,
                body_y,
                area.width.saturating_sub(4),
                body_height,
            )
        } else if stacked {
            Rect::new(
                area.x + 2,
                list_area.bottom() + 1,
                area.width.saturating_sub(4),
                body_height.saturating_sub(list_height + 1),
            )
        } else {
            Rect::new(
                list_area.right() + 3,
                body_y + 1,
                area.width.saturating_sub(list_width + 5),
                body_height.saturating_sub(2),
            )
        };
        self.detail_rect = detail_area;
        self.body_height = detail_area.height;
        let row_height = if stacked { 3 } else { 4 };
        self.visible_rows = (list_area.height / row_height).max(1) as usize;
        let indices = self.filtered_indices(&state);
        self.selected = self.selected.min(indices.len().saturating_sub(1));
        self.scroll = self
            .scroll
            .min(indices.len().saturating_sub(self.visible_rows) as u32);
        if self.selected < self.scroll as usize {
            self.scroll = self.selected as u32;
        } else if self.selected >= self.scroll as usize + self.visible_rows {
            self.scroll = self.selected.saturating_sub(self.visible_rows - 1) as u32;
        }
        for (row, &index) in indices
            .iter()
            .enumerate()
            .skip(self.scroll as usize)
            .take((list_area.height / row_height) as usize)
        {
            let (label, metadata) = match self.tab {
                0 => {
                    let view = &state.confessions[index];
                    (
                        view.confession.trigger.clone(),
                        format!(
                            "{} · {}",
                            view.category.as_deref().unwrap_or("Uncategorized"),
                            view.confession.created_at.format("%Y-%m-%d")
                        ),
                    )
                }
                1 => {
                    let rule = &state.rules[index];
                    (
                        rule.name.clone(),
                        format!("{} · {}", rule.scope, rule.description),
                    )
                }
                _ => {
                    let proposal = &state.proposals[index];
                    let trigger = state
                        .confessions
                        .iter()
                        .find(|view| view.confession.id == proposal.id)
                        .map(|view| view.confession.trigger.as_str())
                        .unwrap_or("Unknown confession");
                    (
                        format!(
                            "{} {}",
                            if self.selected_proposals.contains(&proposal.id) {
                                "[x]"
                            } else {
                                "[ ]"
                            },
                            trigger
                        ),
                        proposal.category.clone(),
                    )
                }
            };
            let rect = Rect::new(
                list_area.x,
                list_area.y + (row - self.scroll as usize) as u16 * row_height,
                list_area.width,
                row_height.min(list_area.height),
            );
            let bg = render_record(
                frame,
                rect,
                row == self.selected,
                self.hovered == Some(HoverTarget::Row(row)),
                &t,
            );
            let text = Rect::new(
                rect.x + 2,
                rect.y + 1,
                rect.width.saturating_sub(if stacked { 7 } else { 4 }),
                1,
            );
            frame.render_widget(
                Paragraph::new(crate::width::truncate(
                    &label.replace('\n', " "),
                    text.width as usize,
                ))
                .style(Style::default().fg(t.tinted_fg.into()).bg(bg)),
                text,
            );
            if row_height == 4 {
                frame.render_widget(
                    Paragraph::new(crate::width::truncate(
                        &metadata.replace('\n', " "),
                        text.width as usize,
                    ))
                    .style(Style::default().fg(t.subtle_fg.into()).bg(bg)),
                    Rect::new(text.x, text.y + 1, text.width, 1),
                );
            }
            if stacked {
                let toggle = Rect::new(
                    rect.right().saturating_sub(4),
                    rect.y,
                    4.min(rect.width),
                    rect.height,
                );
                render_action(
                    frame,
                    toggle,
                    &Action {
                        label: "›",
                        hovered: self.hovered == Some(HoverTarget::Details(row)),
                        ..Default::default()
                    },
                    &t,
                );
                self.detail_toggle_rects.push((row, toggle));
            }
            self.row_rects.push((row, rect));
        }
        let mut detail = match indices.get(self.selected).copied() {
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
                if self.tab == 2 && state.loading {
                    format!("{}\n\n[C] Cancel", state.message)
                } else if self.tab == 2 && state.message.starts_with("Organization failed:") {
                    format!("{}\n\n[O] Retry", state.message)
                } else if self.tab == 2 && state.proposals.is_empty() {
                    format!("{}\n\n[O] Organize to generate suggestions", state.message)
                } else {
                    "No matching entries".into()
                }
            }
        };
        if let Some(confirm) = &self.archive_confirm {
            detail = format!(
                "Archive this confession?\n\n{}\n\nIt will disappear from active memory and search. Its revision history remains.\n\n[Y] Archive · [N] Cancel",
                confirm.trigger
            );
        }
        let detail_lines = crate::width::word_wrap(&detail, detail_area.width as usize);
        let max_detail_scroll = detail_lines
            .len()
            .saturating_sub(detail_area.height as usize) as u16;
        self.detail_scroll = self.detail_scroll.min(max_detail_scroll);
        frame.render_widget(
            Paragraph::new(detail_lines.join("\n"))
                .scroll((self.detail_scroll, 0))
                .style(Style::default().fg(t.tinted_fg.into()).bg(surface)),
            detail_area,
        );
        let status = Rect::new(
            area.x + 1,
            area.bottom() - footer_height,
            area.width.saturating_sub(2),
            1,
        );
        let status_text = format!(
            "{} / {} · {}",
            if indices.is_empty() {
                0
            } else {
                self.selected + 1
            },
            indices.len(),
            state.message
        );
        frame.render_widget(
            Paragraph::new(crate::width::truncate(&status_text, status.width as usize))
                .style(Style::default().fg(t.subtle_fg.into()).bg(surface)),
            status,
        );
        for (name, label, mut rect) in buttons {
            rect.y += status.bottom();
            let disabled = matches!(name, "Applying…" | "Archiving…")
                || (indices.is_empty() && matches!(name, "Edit" | "Archive" | "History"));
            render_action(
                frame,
                rect,
                &Action {
                    label,
                    hovered: self.hovered == Some(HoverTarget::Action(name)),
                    disabled,
                    ..Default::default()
                },
                &t,
            );
            if !disabled {
                self.action_rects.push((name, rect));
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
                self.scroll = 0;
                self.detail_scroll = 0;
                self.clear_geometry();
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
            } else if self.list_rect.contains((mouse.column, mouse.row).into())
                && self.archive_confirm.is_none()
            {
                let total = {
                    let state = self.state.lock().unwrap();
                    self.filtered_indices(&state).len()
                };
                self.scroll = if down {
                    self.scroll
                        .saturating_add(3)
                        .min(total.saturating_sub(self.visible_rows) as u32)
                } else {
                    self.scroll.saturating_sub(3)
                };
                self.selected = self.scroll as usize;
                self.show_history = false;
                self.detail_scroll = 0;
                self.row_rects.clear();
                self.detail_toggle_rects.clear();
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
                if self.archive_confirm.is_some() {
                    return WmEventResult::Consumed(Vec::new());
                }
                if let Some(index) = self
                    .tab_rects
                    .iter()
                    .position(|rect| rect.contains((mouse.column, mouse.row).into()))
                {
                    self.tab = index;
                    self.detail_open = false;
                    self.selected = 0;
                    self.scroll = 0;
                    self.search_focused = false;
                    self.show_history = false;
                    self.detail_scroll = 0;
                    self.clear_geometry();
                    return WmEventResult::Consumed(Vec::new());
                }
                if self.search_rect.contains((mouse.column, mouse.row).into()) {
                    self.search_focused = true;
                    if self
                        .search_input_rect
                        .contains((mouse.column, mouse.row).into())
                    {
                        let col = mouse.column.saturating_sub(self.search_input_rect.x) as usize
                            + self.search_scroll as usize;
                        self.search.set_cursor_by_display(0, col as u16);
                    }
                    return WmEventResult::Consumed(Vec::new());
                }
                if let Some((row, _)) = self
                    .detail_toggle_rects
                    .iter()
                    .find(|(_, rect)| rect.contains((mouse.column, mouse.row).into()))
                {
                    self.selected = *row;
                    self.detail_open = true;
                    self.detail_scroll = 0;
                    self.show_history = false;
                    self.search_focused = false;
                    self.clear_geometry();
                    return WmEventResult::Consumed(Vec::new());
                }
                if let Some((row, _)) = self
                    .row_rects
                    .iter()
                    .find(|(_, rect)| rect.contains((mouse.column, mouse.row).into()))
                {
                    if self.selected != *row || self.show_history {
                        self.detail_scroll = 0;
                    }
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
                KeyAction::Escape => {
                    self.edit = None;
                    self.clear_geometry();
                }
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
                KeyAction::PageUp | KeyAction::PageDown => {
                    let rect = self.edit_text_rects[edit.focused];
                    for _ in 0..rect.height.max(1) {
                        if *action == KeyAction::PageUp {
                            edit.fields[edit.focused]
                                .move_line_up_visual(rect.width.max(1) as usize);
                        } else {
                            edit.fields[edit.focused]
                                .move_line_down_visual(rect.width.max(1) as usize);
                        }
                    }
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
                        self.scroll = 0;
                        self.detail_scroll = 0;
                        self.row_rects.clear();
                        self.detail_toggle_rects.clear();
                    }
                }
            }
            return WmEventResult::Consumed(Vec::new());
        }
        if self.archive_confirm.is_some() {
            match action {
                KeyAction::Char('y') | KeyAction::Submit => self.action("Confirm Archive"),
                KeyAction::Char('n') | KeyAction::Escape => self.action("Cancel Archive"),
                KeyAction::HistoryUp => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
                KeyAction::HistoryDown => {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                }
                KeyAction::PageUp | KeyAction::Char('u') => {
                    self.detail_scroll = self.detail_scroll.saturating_sub(self.body_height.max(1));
                }
                KeyAction::PageDown | KeyAction::Char('d') => {
                    self.detail_scroll = self.detail_scroll.saturating_add(self.body_height.max(1));
                }
                _ => {}
            }
            return WmEventResult::Consumed(Vec::new());
        }
        let total = {
            let state = self.state.lock().unwrap();
            self.filtered_indices(&state).len()
        };
        let old_selected = self.selected;
        match action {
            KeyAction::Submit if self.compact => {
                self.detail_open = !self.detail_open;
                self.detail_scroll = 0;
                self.clear_geometry();
            }
            KeyAction::Escape if self.detail_open => {
                self.detail_open = false;
                self.clear_geometry();
            }
            KeyAction::HistoryUp | KeyAction::Char('k') if self.compact && self.detail_open => {
                self.detail_scroll = self.detail_scroll.saturating_sub(1);
            }
            KeyAction::HistoryDown | KeyAction::Char('j') if self.compact && self.detail_open => {
                self.detail_scroll = self.detail_scroll.saturating_add(1);
            }
            KeyAction::PageUp if self.compact && self.detail_open => {
                self.detail_scroll = self.detail_scroll.saturating_sub(self.body_height.max(1));
            }
            KeyAction::PageDown if self.compact && self.detail_open => {
                self.detail_scroll = self.detail_scroll.saturating_add(self.body_height.max(1));
            }
            KeyAction::HistoryUp | KeyAction::Char('k') => {
                self.selected = self.selected.saturating_sub(1);
                self.show_history = false;
            }
            KeyAction::HistoryDown | KeyAction::Char('j') => {
                self.selected = (self.selected + 1).min(total.saturating_sub(1));
                self.show_history = false;
            }
            KeyAction::PageUp => {
                self.selected = self.selected.saturating_sub(self.visible_rows);
                self.show_history = false;
            }
            KeyAction::PageDown => {
                self.selected = self
                    .selected
                    .saturating_add(self.visible_rows)
                    .min(total.saturating_sub(1));
                self.show_history = false;
            }
            KeyAction::Char('u') => {
                self.detail_scroll = self.detail_scroll.saturating_sub(self.body_height)
            }
            KeyAction::Char('d') => {
                self.detail_scroll = self.detail_scroll.saturating_add(self.body_height)
            }
            KeyAction::Char('/') => {
                self.search_focused = true;
                self.detail_open = false;
                self.clear_geometry();
            }
            KeyAction::Char('e') if self.tab == 0 => self.action("Edit"),
            KeyAction::Char('x') if self.tab == 0 => self.action("Archive"),
            KeyAction::Char('h') if self.tab == 0 => self.action("History"),
            KeyAction::Char('o') => self.action("Organize"),
            KeyAction::Char('c') if self.tab == 2 => self.action("Stop"),
            KeyAction::Char('r') => self.action("Refresh"),
            KeyAction::Char('R') if self.tab == 1 => self.action("Reload"),
            KeyAction::Char('a') if self.tab == 2 => self.action("Apply"),
            KeyAction::Char('s') if self.tab == 2 => self.action("Select All"),
            KeyAction::Char(' ') if self.tab == 2 => self.action("Select"),
            KeyAction::CursorLeft | KeyAction::BackTab => {
                self.tab = self.tab.saturating_sub(1);
                self.selected = 0;
                self.detail_open = false;
                self.scroll = 0;
                self.show_history = false;
                self.detail_scroll = 0;
                self.clear_geometry();
            }
            KeyAction::CursorRight | KeyAction::Tab => {
                self.tab = (self.tab + 1).min(2);
                self.selected = 0;
                self.detail_open = false;
                self.scroll = 0;
                self.show_history = false;
                self.detail_scroll = 0;
                self.clear_geometry();
            }
            _ => return WmEventResult::Ignored,
        }
        if self.selected < self.scroll as usize {
            self.scroll = self.selected as u32;
        }
        if self.selected >= self.scroll as usize + self.visible_rows {
            self.scroll = (self.selected + 1).saturating_sub(self.visible_rows) as u32;
        }
        if self.selected != old_selected {
            self.detail_scroll = 0;
            self.row_rects.clear();
            self.detail_toggle_rects.clear();
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

    fn render_panel(panel: &mut KnowledgePanelContent, terminal: &mut Terminal<TestBackend>) {
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
            hovered_action: None,
            remove_armed: None,
        };
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
    }

    #[test]
    fn record_geometry_drives_padding_paging_and_click_selection() {
        let state = Arc::new(Mutex::new(KnowledgeState {
            confessions: (0..20).map(|_| sample_view()).collect(),
            ..Default::default()
        }));
        for (width, height) in [(100, 36), (55, 16), (40, 18), (34, 15)] {
            let mut panel = KnowledgePanelContent::new(state.clone(), None);
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            render_panel(&mut panel, &mut terminal);
            let visible = panel.visible_rows;
            let mut scroll = 0;
            let mut h_scroll = 0;
            let mut ctx = EventCtx {
                scroll: &mut scroll,
                h_scroll: &mut h_scroll,
            };
            panel.handle_event(&WmEvent::Key(KeyAction::PageDown), &mut ctx);
            render_panel(&mut panel, &mut terminal);
            assert_eq!(panel.selected, visible);
            assert!(panel.scroll > 0);
            let row_rects = panel.row_rects.clone();
            assert!(!row_rects.is_empty());
            let selected = panel.selected;
            for (row, rect) in row_rects {
                assert!(rect.height >= 3);
                assert!(panel.list_rect.contains((rect.x, rect.bottom() - 1).into()));
                if row == selected {
                    for y in rect.y..rect.bottom() {
                        assert_eq!(terminal.backend().buffer()[(rect.x, y)].symbol(), "▌");
                    }
                }
                panel.handle_event(
                    &WmEvent::Mouse(MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: rect.x + 1,
                        row: rect.bottom() - 1,
                        modifiers: KeyModifiers::NONE,
                    }),
                    &mut ctx,
                );
                assert_eq!(panel.selected, row);
            }
            for (_, rect) in &panel.action_rects {
                assert_eq!(rect.height, 3);
                assert!(rect.right() <= width && rect.bottom() <= height);
            }
            assert!(panel.detail_rect.width >= if panel.compact { width - 8 } else { 30 });
            if panel.compact {
                let (_, toggle) = panel.detail_toggle_rects[0];
                panel.handle_event(
                    &WmEvent::Mouse(MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: toggle.x,
                        row: toggle.bottom() - 1,
                        modifiers: KeyModifiers::NONE,
                    }),
                    &mut ctx,
                );
                assert!(panel.detail_open);
                assert!(panel.row_rects.is_empty());
                render_panel(&mut panel, &mut terminal);
                assert!(panel.row_rects.is_empty());
                assert!(panel.search_rect.is_empty());
                assert!(panel.detail_rect.height >= 8);
                assert_eq!(panel.action_rects.len(), 3);
                assert!(
                    panel
                        .action_rects
                        .iter()
                        .all(|(_, rect)| rect.y == panel.action_rects[0].1.y)
                );
                let selected = panel.selected;
                let page = panel.detail_rect.height;
                panel.handle_event(&WmEvent::Key(KeyAction::PageDown), &mut ctx);
                assert_eq!(panel.selected, selected);
                assert_eq!(panel.detail_scroll, page);
                panel.handle_event(&WmEvent::Key(KeyAction::HistoryUp), &mut ctx);
                assert_eq!(panel.detail_scroll, page - 1);
                panel.handle_event(&WmEvent::Key(KeyAction::Escape), &mut ctx);
                assert!(!panel.detail_open);
                render_panel(&mut panel, &mut terminal);
                panel.handle_event(&WmEvent::Key(KeyAction::Submit), &mut ctx);
                assert!(panel.detail_open);
            }
        }
    }

    #[test]
    fn editor_focus_keeps_fields_and_actions_visible_without_stale_targets() {
        for (width, height) in [(100, 36), (55, 16), (40, 18)] {
            let mut view = sample_view();
            view.confession.trigger = "one\ntwo\nthree\nfour\nfive\nsix\nseven".into();
            let state = Arc::new(Mutex::new(KnowledgeState {
                confessions: vec![view],
                ..Default::default()
            }));
            let mut panel = KnowledgePanelContent::new(state, None);
            panel.action("Edit");
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            for focused in 0..5 {
                let edit = panel.edit.as_mut().unwrap();
                edit.focused = focused;
                edit.follow_cursor = true;
                render_panel(&mut panel, &mut terminal);
                assert!(panel.cursor_visible());
                let segment = panel.edit_segment_rects[focused];
                let text = panel.edit_text_rects[focused];
                assert!(segment.height >= 5);
                assert!(text.height >= 2);
                assert!(text.bottom() < segment.bottom());
                assert_eq!(panel.action_rects.len(), 2);
                for (_, rect) in &panel.action_rects {
                    assert_eq!(rect.height, 3);
                    assert!(rect.right() <= width && rect.bottom() <= height);
                }
            }
            panel.action("Cancel");
            assert!(panel.edit.is_none());
            assert!(panel.action_rects.is_empty());
            assert!(panel.edit_segment_rects.iter().all(|rect| rect.is_empty()));
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
        panel.action("Select");
        assert!(panel.selected_proposals.is_empty());
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
        for (width, height) in [(100, 36), (55, 16)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            for tab in 0..3 {
                panel.tab = tab;
                if tab == 2 {
                    panel.state.lock().unwrap().message =
                        "Organization failed: model unavailable".into();
                }
                render_panel(&mut panel, &mut terminal);
                let content = terminal
                    .backend()
                    .buffer()
                    .content()
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>();
                assert!(content.contains(match tab {
                    0 => "initial",
                    1 => "AGENTS.md",
                    _ => "Organization failed",
                }));
                if tab == 0 {
                    assert!(content.contains("[E]dit"));
                    assert!(content.contains(if width < 65 { "[X]arc" } else { "[X] Archive" }));
                    assert!(content.contains(if width < 65 { "[H]ist" } else { "[H]istory" }));
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
            crate::theme::ThemeColor::new(crate::wm::shell::surface_bg(&crate::theme::theme()))
                .lerp(crate::theme::theme().accent, 0.12)
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
            .draw(|frame| panel.render_editor(Rect::new(0, 0, 10, 16), frame))
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
        let cursor = panel.search.cursor();
        panel.handle_event(
            &WmEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: panel.search_rect.x + 1,
                row: panel.search_rect.y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut ctx,
        );
        assert_eq!(panel.search.cursor(), cursor);
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

    #[test]
    fn archive_requires_confirmation_and_accepts_mouse_or_keyboard() {
        let view = sample_view();
        let id = view.confession.id.clone();
        let state = Arc::new(Mutex::new(KnowledgeState {
            confessions: vec![view],
            ..Default::default()
        }));
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut panel = KnowledgePanelContent::new(state.clone(), Some(tx));
        panel.action("Archive");
        assert!(panel.archive_confirm.is_some());
        assert!(rx.try_recv().is_err());
        panel.action("Cancel Archive");
        assert!(panel.archive_confirm.is_none());
        assert!(rx.try_recv().is_err());

        panel.action("Archive");
        let rect = Rect::new(2, 4, 16, 1);
        panel.action_rects = vec![("Confirm Archive", rect)];
        let mut scroll = 0;
        let mut h_scroll = 0;
        let mut ctx = EventCtx {
            scroll: &mut scroll,
            h_scroll: &mut h_scroll,
        };
        panel.body_height = 6;
        panel.handle_event(&WmEvent::Key(KeyAction::PageDown), &mut ctx);
        assert_eq!(panel.detail_scroll, 6);
        panel.handle_event(&WmEvent::Key(KeyAction::HistoryUp), &mut ctx);
        assert_eq!(panel.detail_scroll, 5);
        panel.handle_event(&WmEvent::Key(KeyAction::PageUp), &mut ctx);
        assert_eq!(panel.detail_scroll, 0);
        assert_eq!(panel.archive_confirm.as_ref().unwrap().id, id);
        assert!(rx.try_recv().is_err());
        panel.handle_event(
            &WmEvent::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: rect.x + 1,
                row: rect.y,
                modifiers: KeyModifiers::NONE,
            }),
            &mut ctx,
        );
        assert!(
            matches!(rx.try_recv(), Ok(TuiControl::ArchiveConfession { id: archived_id, base_revision: 0 }) if archived_id == id)
        );
        assert!(state.lock().unwrap().archiving);

        state.lock().unwrap().archiving = false;
        panel.action("Archive");
        panel.handle_event(&WmEvent::Key(KeyAction::Char('n')), &mut ctx);
        assert!(panel.archive_confirm.is_none());
        assert!(rx.try_recv().is_err());
    }
}
