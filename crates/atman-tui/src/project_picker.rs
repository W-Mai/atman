use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
};

use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{Frame, layout::Rect, style::Style, widgets::Paragraph};
use uuid::Uuid;

use crate::{
    ControlSender, TuiControl, control::ControlTicket, input::InputEditor, keys::KeyAction,
    theme::Theme,
};

#[derive(Debug)]
pub struct DirectoryListing {
    pub directory: PathBuf,
    pub folders: Vec<PathBuf>,
    pub roots: Vec<PathBuf>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Focus {
    #[default]
    Tree,
    Path,
    Search,
}

#[derive(Clone)]
enum Action {
    Parent,
    Close,
    Edit(Focus),
    Select(PathBuf),
    Toggle(PathBuf),
    Open,
    Retry(Option<PathBuf>, ReadKind),
}

#[derive(Clone, Copy)]
enum ReadKind {
    Navigate,
    Expand,
}

struct PendingRead {
    ticket: ControlTicket,
    path: Option<PathBuf>,
    kind: ReadKind,
}

#[derive(Default)]
pub struct Picker {
    pub(crate) open: bool,
    id: Uuid,
    current: Option<PathBuf>,
    entries: HashMap<PathBuf, DirectoryListing>,
    expanded: HashSet<PathBuf>,
    selected: Option<PathBuf>,
    roots: Vec<PathBuf>,
    read: Option<PendingRead>,
    create: Option<ControlTicket>,
    intent: Option<(PathBuf, Uuid)>,
    error: Option<String>,
    retry: Option<(Option<PathBuf>, ReadKind)>,
    focus: Focus,
    path: InputEditor,
    query: InputEditor,
    offset: usize,
    rows_on_screen: usize,
    follow_selection: bool,
    cursor: Option<(u16, u16)>,
    hover: Option<(u16, u16)>,
    hits: Vec<(Rect, Action)>,
}

impl Picker {
    pub(crate) fn open(&mut self, tx: Option<&ControlSender>) {
        // Keep an accepted create in its original picker, including its retry identity.
        if self.create.is_some() || self.intent.is_some() && self.error.is_some() {
            self.open = true;
            return;
        }
        *self = Self {
            open: true,
            id: Uuid::now_v7(),
            ..Self::default()
        };
        self.read(None, ReadKind::Navigate, tx);
    }

    fn hide(&mut self, tx: Option<&ControlSender>) {
        self.open = false;
        self.hits.clear();
        self.cursor = None;
        self.hover = None;
        if self.read.take().is_some()
            && let Some(tx) = tx
        {
            let _ = tx.send(TuiControl::CancelProjectFolderRead { picker: self.id });
        }
    }

    pub(crate) fn waiting(&self) -> bool {
        self.read.is_some() || self.create.is_some()
    }

    pub(crate) fn cursor(&self) -> Option<(u16, u16)> {
        self.cursor
    }

    fn read(&mut self, path: Option<PathBuf>, kind: ReadKind, tx: Option<&ControlSender>) {
        let request = TuiControl::BrowseProjectFolders {
            picker: self.id,
            path: path.clone(),
        };
        self.read = tracked(tx, request).map(|ticket| PendingRead { ticket, path, kind });
        self.hits.clear();
        self.retry = None;
        self.error = self
            .read
            .is_none()
            .then(|| "Directory browsing is unavailable".to_owned());
    }

    pub(crate) fn loaded(
        &mut self,
        ticket: Option<&ControlTicket>,
        picker: Uuid,
        request: Uuid,
        result: Result<DirectoryListing, String>,
    ) -> bool {
        if picker != self.id
            || !self.read.as_ref().is_some_and(|read| {
                ticket == Some(&read.ticket) && request == read.ticket.request_id
            })
        {
            return false;
        }
        let Some(read) = self.read.take() else {
            return false;
        };
        self.hits.clear();
        let listing = match result {
            Ok(listing) => listing,
            Err(error) => {
                self.error = Some(error);
                self.retry = Some((read.path, read.kind));
                return true;
            }
        };
        self.roots.clone_from(&listing.roots);
        let node = match read.kind {
            ReadKind::Navigate => {
                let node = listing.directory.clone();
                self.current = Some(node.clone());
                self.select(node.clone());
                self.path.replace_with(&node.to_string_lossy());
                self.query.replace_with("");
                self.focus = Focus::Tree;
                self.offset = 0;
                node
            }
            ReadKind::Expand => read.path.unwrap_or_else(|| listing.directory.clone()),
        };
        self.expanded.insert(node.clone());
        self.entries.insert(node, listing);
        true
    }

    pub(crate) fn owns_create(&self, ticket: Option<&ControlTicket>) -> bool {
        self.create
            .as_ref()
            .is_some_and(|pending| Some(pending) == ticket)
    }

    pub(crate) fn created(&mut self, ticket: Option<&ControlTicket>) {
        if self.owns_create(ticket) {
            *self = Self::default();
        }
    }

    pub(crate) fn failed(
        &mut self,
        ticket: Option<&ControlTicket>,
        request: Uuid,
        error: &str,
    ) -> bool {
        if !self.owns_create(ticket) || ticket.is_none_or(|ticket| ticket.request_id != request) {
            return false;
        }
        self.create = None;
        self.error = Some(error.to_owned());
        self.hits.clear();
        true
    }

    fn select(&mut self, path: PathBuf) {
        self.selected = Some(path);
        self.follow_selection = true;
    }

    fn rows(&self) -> Vec<(usize, &PathBuf)> {
        let mut rows = Vec::new();
        let query = self.query.buf().to_lowercase();
        if let Some(current) = &self.current {
            let mut stack: Vec<_> = self
                .roots
                .iter()
                .rev()
                .filter(|root| *root != current && !current.starts_with(root))
                .map(|root| (0, root))
                .collect();
            stack.push((0, current));
            let mut visited = HashSet::new();
            while let Some((depth, path)) = stack.pop() {
                if !visited.insert(path) {
                    continue;
                }
                if depth == 0 || query.is_empty() || label(path).to_lowercase().contains(&query) {
                    rows.push((depth, path));
                }
                if self.expanded.contains(path)
                    && let Some(listing) = self.entries.get(path)
                {
                    stack.extend(listing.folders.iter().rev().map(|child| (depth + 1, child)));
                }
            }
        }
        rows
    }

    fn toggle(&mut self, path: PathBuf, tx: Option<&ControlSender>) {
        if self.expanded.remove(&path) {
            if self
                .read
                .as_ref()
                .is_some_and(|read| read.path.as_ref() == Some(&path))
                && let Some(tx) = tx
            {
                let _ = tx.send(TuiControl::CancelProjectFolderRead { picker: self.id });
                self.read = None;
            }
        } else if self.entries.contains_key(&path) {
            self.expanded.insert(path);
        } else {
            self.read(Some(path), ReadKind::Expand, tx);
        }
    }

    fn submit(&mut self, tx: Option<&ControlSender>) {
        if self.read.is_some() {
            return;
        }
        if self.focus == Focus::Path {
            if !self.path.buf().trim().is_empty() {
                self.read(Some(PathBuf::from(self.path.buf())), ReadKind::Navigate, tx);
            }
            return;
        }
        let Some(path) = self
            .selected
            .as_ref()
            .filter(|selected| self.rows().iter().any(|(_, path)| *path == *selected))
        else {
            return;
        };
        let path = self
            .entries
            .get(path)
            .map_or(path, |listing| &listing.directory);
        let creation = self
            .intent
            .as_ref()
            .filter(|(target, _)| target == path)
            .map_or_else(Uuid::now_v7, |(_, id)| *id);
        let path = path.clone();
        self.intent = Some((path.clone(), creation));
        self.create = tracked(tx, TuiControl::OpenProjectDirectory { creation, path });
        self.error = self
            .create
            .is_none()
            .then(|| "Project creation is unavailable".into());
    }

    fn action(&mut self, action: Action, tx: Option<&ControlSender>) {
        if matches!(action, Action::Close) {
            self.hide(tx);
        } else if self.create.is_none() {
            match action {
                Action::Close => {}
                Action::Parent => {
                    if let Some(parent) = self.current.as_ref().and_then(|path| path.parent()) {
                        self.read(Some(parent.to_owned()), ReadKind::Navigate, tx);
                    }
                }
                Action::Edit(focus) => self.focus = focus,
                Action::Select(path) => {
                    self.focus = Focus::Tree;
                    self.select(path);
                }
                Action::Toggle(path) => {
                    self.focus = Focus::Tree;
                    self.select(path.clone());
                    self.toggle(path, tx);
                }
                Action::Open => self.submit(tx),
                Action::Retry(path, kind) => self.read(path, kind, tx),
            }
        }
        self.hits.clear();
        self.cursor = None;
    }

    pub(crate) fn paste(&mut self, text: &str) {
        if self.create.is_none() {
            if self.focus == Focus::Tree {
                self.path.replace_with("");
                self.focus = Focus::Path;
            }
            self.edit_text(|editor| editor.paste_single_line(text));
        }
    }

    fn edit_text(&mut self, edit: impl FnOnce(&mut InputEditor)) {
        match self.focus {
            Focus::Search => {
                edit(&mut self.query);
                self.offset = 0;
                let searching = !self.query.buf().is_empty();
                let rows = self.rows();
                let mut candidates = rows
                    .iter()
                    .filter(|(depth, _)| !searching || *depth > 0)
                    .map(|(_, path)| *path);
                let first = candidates.next();
                let selected = self.selected.as_ref();
                if first != selected && !candidates.any(|path| Some(path) == selected) {
                    self.selected = first.cloned();
                }
                self.follow_selection = true;
            }
            Focus::Path => edit(&mut self.path),
            Focus::Tree => return,
        }
        self.hits.clear();
    }

    pub(crate) fn key(&mut self, key: &KeyAction, tx: Option<&ControlSender>) {
        use KeyAction::*;
        if *key == Escape {
            self.hide(tx);
            return;
        }
        if self.create.is_some() {
            return;
        }
        match (self.focus, key) {
            (Focus::Tree, Tab) | (Focus::Search, BackTab) => self.focus = Focus::Path,
            (Focus::Path, Tab) | (Focus::Tree, BackTab) => self.focus = Focus::Search,
            (Focus::Search, Tab | Submit) | (Focus::Path, BackTab) => self.focus = Focus::Tree,
            (_, Save) | (Focus::Tree | Focus::Path, Submit) => self.submit(tx),
            (Focus::Tree, Char('/')) => self.focus = Focus::Search,
            (Focus::Tree, Backspace) => self.action(Action::Parent, tx),
            (Focus::Tree, CursorRight | CursorLeft | Char(' ')) => {
                if let Some(path) = self.selected.clone() {
                    let expanded = self.expanded.contains(&path);
                    if *key == Char(' ')
                        || (*key == CursorRight && !expanded)
                        || (*key == CursorLeft && expanded)
                    {
                        self.toggle(path, tx);
                    } else if *key == CursorLeft {
                        let parent = self
                            .entries
                            .iter()
                            .find(|(_, listing)| listing.folders.contains(&path))
                            .map(|(node, _)| node.clone());
                        if let Some(parent) = parent {
                            self.select(parent);
                        }
                    }
                }
            }
            (Focus::Tree, HistoryUp | HistoryDown | PageUp | PageDown | Home | End) => {
                let rows = self.rows();
                let index = rows
                    .iter()
                    .position(|(_, path)| Some(*path) == self.selected.as_ref())
                    .unwrap_or_default();
                let target = match key {
                    HistoryUp => index.saturating_sub(1),
                    HistoryDown => index + 1,
                    PageUp => index.saturating_sub(self.rows_on_screen),
                    PageDown => index + self.rows_on_screen,
                    Home => 0,
                    _ => rows.len().saturating_sub(1),
                };
                if let Some((_, path)) = rows.get(target.min(rows.len().saturating_sub(1))) {
                    self.select((*path).clone());
                }
            }
            (Focus::Tree, Char('r' | 'R')) => {
                if let Some((path, kind)) = self.retry.clone() {
                    self.read(path, kind, tx);
                }
            }
            (Focus::Tree, _) => {}
            (_, RemoveAttachment) => self.edit_text(|editor| editor.replace_with("")),
            (
                _,
                Char(_) | Backspace | Delete | DeleteWordBackward | CursorLeft | CursorRight
                | CursorHome | CursorEnd | Home | End,
            ) => self.edit_text(|editor| {
                editor.handle_key(match key {
                    Home => &CursorHome,
                    End => &CursorEnd,
                    key => key,
                });
            }),
            _ => {}
        }
        self.hits.clear();
        self.cursor = None;
    }

    pub(crate) fn mouse(&mut self, event: MouseEvent, tx: Option<&ControlSender>) {
        let point = (event.column, event.row);
        self.hover = Some(point);
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(action) = self
                    .hits
                    .iter()
                    .find(|(rect, _)| contains(*rect, point))
                    .map(|(_, action)| action.clone())
                {
                    self.action(action, tx);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown if self.create.is_none() => {
                self.follow_selection = false;
                if event.kind == MouseEventKind::ScrollUp {
                    self.offset = self.offset.saturating_sub(3);
                } else {
                    self.offset = (self.offset + 3)
                        .min(self.rows().len().saturating_sub(self.rows_on_screen));
                }
                self.hits.clear();
            }
            _ => {}
        }
    }

    pub(crate) fn render(&mut self, frame: &mut Frame, area: Rect, theme: &Theme, animation: u32) {
        self.hits.clear();
        self.cursor = None;
        if area.height < 4 || area.width < 12 {
            return;
        }
        let active = self.create.is_none();
        let x = area.x + 1;
        let width = area.width - 2;
        let top = area.y;
        let close = Rect::new(x + width - 3, top, 3, 1);
        self.control(frame, theme, close, " × ", Action::Close, false);
        if active
            && self
                .current
                .as_ref()
                .is_some_and(|path| path.parent().is_some())
        {
            self.control(
                frame,
                theme,
                Rect::new(x, top, 3, 1),
                " ↑ ",
                Action::Parent,
                false,
            );
        }
        let path_rect = Rect::new(x + 4, top, width - 8, 1);
        let search_rect = Rect::new(x, top + 1, width, 1);
        self.text_field(frame, theme, path_rect, Focus::Path, "Path ", active);
        self.text_field(frame, theme, search_rect, Focus::Search, "⌕ ", active);
        let footer = area.bottom() - 1;
        self.rows_on_screen = usize::from(footer.saturating_sub(top + 2));
        let rows = self.rows();
        let selected = rows
            .iter()
            .position(|(_, path)| Some(*path) == self.selected.as_ref());
        let mut offset = self
            .offset
            .min(rows.len().saturating_sub(self.rows_on_screen));
        if self.follow_selection
            && let Some(index) = selected
        {
            if index < offset {
                offset = index;
            } else if index >= offset + self.rows_on_screen {
                offset = index.saturating_add(1).saturating_sub(self.rows_on_screen);
            }
        }
        let visible_rows: Vec<_> = rows
            .into_iter()
            .skip(offset)
            .take(self.rows_on_screen)
            .map(|(depth, path)| (depth, path.clone()))
            .collect();
        self.offset = offset;
        self.follow_selection = false;
        for (line, (depth, path)) in visible_rows.into_iter().enumerate() {
            let y = top + 2 + line as u16;
            let indent = depth
                .saturating_mul(2)
                .min(usize::from(width.saturating_sub(8))) as u16;
            let toggle = Rect::new(x + indent, y, 3, 1);
            let name = Rect::new(toggle.right(), y, width - indent - 3, 1);
            let focused = self.selected.as_ref() == Some(&path);
            let glyph = if self.expanded.contains(&path) {
                " ▾ "
            } else {
                " ▸ "
            };
            if active {
                self.control(
                    frame,
                    theme,
                    toggle,
                    glyph,
                    Action::Toggle(path.clone()),
                    focused,
                );
                self.control(
                    frame,
                    theme,
                    name,
                    &format!("{} ", label(&path)),
                    Action::Select(path),
                    focused,
                );
            } else {
                frame.render_widget(
                    Paragraph::new(glyph).style(Style::default().fg(theme.project_meta_fg.into())),
                    toggle,
                );
                frame.render_widget(
                    Paragraph::new(label(&path))
                        .style(Style::default().fg(theme.project_session_fg.into())),
                    name,
                );
            }
        }
        let editing_path = self.focus == Focus::Path;
        let can_confirm = active
            && self.read.is_none()
            && if editing_path {
                !self.path.buf().trim().is_empty()
            } else {
                selected.is_some()
            };
        let footer_text = Rect::new(
            x,
            footer,
            if can_confirm {
                width.saturating_sub(8)
            } else {
                width
            },
            1,
        );
        if self.create.is_some() || self.read.is_some() {
            let label = if self.create.is_some() {
                "Opening"
            } else {
                "Reading folders"
            };
            frame.render_widget(
                Paragraph::new(crate::loading::line(label, animation, footer_text.width)),
                footer_text,
            );
        } else if let Some(error) = self.error.as_ref() {
            frame.render_widget(
                Paragraph::new(error.as_str()).style(Style::default().fg(theme.error.into())),
                footer_text,
            );
            if let Some((path, kind)) = &self.retry {
                self.hits
                    .push((footer_text, Action::Retry(path.clone(), *kind)));
            }
        } else {
            frame.render_widget(
                Paragraph::new(if self.current.is_some() {
                    "← → Browse  ·  Tab"
                } else {
                    "Enter a path"
                })
                .style(Style::default().fg(theme.project_meta_fg.into())),
                footer_text,
            );
        }
        if can_confirm {
            self.control(
                frame,
                theme,
                Rect::new(x + width - 6, footer, 6, 1),
                if editing_path { " Go " } else { " Open " },
                Action::Open,
                false,
            );
        }
    }

    fn text_field(
        &mut self,
        frame: &mut Frame,
        theme: &Theme,
        rect: Rect,
        focus: Focus,
        prefix: &str,
        active: bool,
    ) {
        let editor = if focus == Focus::Path {
            &self.path
        } else {
            &self.query
        };
        let focused = active && self.focus == focus;
        let prefix_width = unicode_width::UnicodeWidthStr::width(prefix);
        let available = usize::from(rect.width).saturating_sub(prefix_width);
        let skip = if focused {
            editor
                .cursor_display_col()
                .saturating_sub(available.saturating_sub(1))
        } else {
            0
        };
        let text = visible_text(editor.buf(), skip);
        frame.render_widget(
            Paragraph::new(format!("{prefix}{text}")).style(
                Style::default().fg(theme.project_title_fg.into()).bg(
                    crate::settings_form::record_background(
                        theme,
                        focused,
                        self.hover.is_some_and(|point| contains(rect, point)),
                    ),
                ),
            ),
            rect,
        );
        if active {
            self.hits.push((rect, Action::Edit(focus)));
        }
        if focused && available != 0 {
            self.cursor = Some((
                rect.x
                    + (prefix_width
                        + editor
                            .cursor_display_col()
                            .saturating_sub(skip)
                            .min(available - 1)) as u16,
                rect.y,
            ));
        }
    }

    fn control(
        &mut self,
        frame: &mut Frame,
        theme: &Theme,
        rect: Rect,
        label: &str,
        action: Action,
        focused: bool,
    ) {
        let hovered = self.hover.is_some_and(|point| contains(rect, point));
        let fg = if focused || hovered {
            theme.project_title_fg
        } else {
            theme.project_session_fg
        };
        frame.render_widget(
            Paragraph::new(label).style(Style::default().fg(fg.into()).bg(
                crate::settings_form::record_background(theme, focused, hovered),
            )),
            rect,
        );
        self.hits.push((rect, action));
    }
}

fn tracked(tx: Option<&ControlSender>, control: TuiControl) -> Option<ControlTicket> {
    let tx = tx?;
    tx.binding()?;
    tx.send_tracked(control).ok().flatten()
}

fn label(path: &std::path::Path) -> String {
    path.file_name()
        .map_or_else(|| path.to_string_lossy(), |name| name.to_string_lossy())
        .chars()
        .map(|ch| if ch.is_control() { '�' } else { ch })
        .collect()
}

fn contains(rect: Rect, (x, y): (u16, u16)) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

fn visible_text(text: &str, skip: usize) -> &str {
    use unicode_width::UnicodeWidthChar;
    let mut width = 0;
    for (index, ch) in text.char_indices() {
        if width >= skip {
            return &text[index..];
        }
        width += ch.width().unwrap_or_default();
    }
    ""
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confirming_edited_path_navigates_before_opening_a_project() {
        let (commands, mut received) = tokio::sync::mpsc::unbounded_channel();
        let (sender, _) = ControlSender::for_flow(
            crate::control::ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: atman_runtime::flow_store::FlowRef {
                    session_id: Uuid::now_v7().into(),
                    instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
                }
                .into(),
            },
            commands,
        );
        let mut picker = Picker {
            current: Some(PathBuf::from("/previous")),
            selected: Some(PathBuf::from("/previous")),
            focus: Focus::Path,
            ..Picker::default()
        };
        picker.path.replace_with("/chosen");
        picker.action(Action::Open, Some(&sender));
        let request = received.try_recv().unwrap();
        assert!(
            matches!(request.command, TuiControl::BrowseProjectFolders { path: Some(path), .. }
            if path == std::path::Path::new("/chosen"))
        );
        picker.key(&KeyAction::Submit, Some(&sender));
        assert!(received.try_recv().is_err());
        assert!(picker.loaded(
            Some(&request.ticket),
            picker.id,
            request.ticket.request_id,
            Ok(DirectoryListing {
                directory: PathBuf::from("/chosen"),
                folders: Vec::new(),
                roots: Vec::new(),
            }),
        ));
        picker.key(&KeyAction::Submit, Some(&sender));
        assert!(
            matches!(received.try_recv().unwrap().command, TuiControl::OpenProjectDirectory { path, .. }
            if path == std::path::Path::new("/chosen"))
        );
    }
}
