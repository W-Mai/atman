//! Placement and focus for complete flow views. Execution belongs to the host.

use std::collections::HashMap;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    Frame, TerminalOptions, Viewport,
    backend::TestBackend,
    layout::{Position, Rect},
    style::Style,
    widgets::Paragraph,
};

use super::*;
use crate::control::{FlowPlacement, ViewAddress, ViewBinding};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(super) struct ViewKey {
    pub flow: ViewAddress,
    pub placement: FlowPlacement,
}

pub(super) struct AttachedView {
    pub ui: UiState,
    pub editor: InputEditor,
    pub handle: TuiHandle,
}

async fn refresh_view(
    ui: &mut UiState,
    editor: &mut InputEditor,
    handle: &mut TuiHandle,
) -> Result<()> {
    if let Some(reader) = handle.flow_view.as_mut() {
        reader.set_pending_offset(ui.app.pending_page_offset);
        let result = reader
            .refresh()
            .await
            .and_then(|view| ui.app.sync_flow_view(view));
        if let Err(error) = result {
            disconnect_view(ui, &error);
            return Err(error.into());
        }
        if let Some(snapshot) = ui.app.items.snapshot() {
            ui.wm.flow_snapshot(snapshot.clone());
        }
    }
    refresh_instance_live(ui, editor, handle);
    refresh_session_name(ui, handle);
    refresh_live_output(ui, handle)?;
    if handle.settings.take_changes() {
        apply_provider_catalog_changed(ui, None);
    }
    Ok(())
}

/// A frame surface isolates clipping and the cursor while using the ordinary renderer.
struct Surface {
    area: Rect,
    terminal: Terminal<TestBackend>,
}

impl Surface {
    fn new(area: Rect) -> Result<Self> {
        Ok(Self {
            area,
            terminal: Terminal::with_options(
                TestBackend::new(area.right(), area.bottom()),
                TerminalOptions {
                    viewport: Viewport::Fixed(area),
                },
            )?,
        })
    }

    fn draw(
        &mut self,
        frame: &mut Frame,
        ui: &mut UiState,
        editor: &InputEditor,
    ) -> Result<Option<Position>> {
        let completed = self.terminal.draw(|f| render_frame(f, ui, editor))?;
        let area = self.area.intersection(frame.area());
        crate::sanitize_widget_edges(frame, area);
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                frame.buffer_mut()[(x, y)] = completed.buffer[(x, y)].clone();
            }
        }
        let backend = self.terminal.backend();
        Ok(backend
            .cursor_visible()
            .then(|| backend.cursor_position())
            .filter(|point| area.contains(*point)))
    }
}

struct Window {
    key: ViewKey,
    history: crate::flow_hub::History,
    rect: Rect,
    restore: Option<Rect>,
    hover: Option<usize>,
}

impl Window {
    fn area(&self, canvas: Rect) -> Rect {
        if compact_window(canvas) || self.restore.is_some() {
            canvas
        } else {
            bounded(self.rect, canvas)
        }
    }

    fn content(&self, canvas: Rect) -> Rect {
        let rect = self.area(canvas);
        let (padding, header) = if compact_window(canvas) {
            (0, 9)
        } else {
            (2, 6)
        };
        Rect::new(
            rect.x.saturating_add(padding),
            rect.y.saturating_add(header),
            rect.width.saturating_sub(padding * 2),
            rect.height.saturating_sub(header + 2),
        )
    }

    fn buttons(&self, canvas: Rect) -> [Rect; 5] {
        let rect = self.area(canvas);
        std::array::from_fn(|index| {
            let (padding, row, column, columns) = if compact_window(canvas) {
                if index < 3 {
                    (1, 0, index, 3)
                } else {
                    (1, 1, index - 3, 2)
                }
            } else {
                (2, 0, index, 5)
            };
            let width = rect.width.saturating_sub(padding * 2) / columns;
            Rect::new(
                rect.x + padding + column as u16 * width,
                rect.y + 2 + row * 3,
                width.saturating_sub(1),
                3,
            )
        })
    }

    fn draw(&self, frame: &mut Frame, active: bool, geometry: Option<bool>) {
        let canvas = frame.area();
        let rect = self.area(canvas);
        let t = crate::theme::theme();
        crate::wm::shadow::render_shadow(frame, rect, &t);
        let bg = crate::wm::shell::surface_bg(&t);
        frame.render_widget(ratatui::widgets::Clear, rect);
        frame.render_widget(Paragraph::new("").style(Style::default().bg(bg)), rect);
        frame.render_widget(
            Paragraph::new(format!("  {}", self.key.flow.label())).style(
                Style::default()
                    .fg(if active {
                        t.heading.into()
                    } else {
                        t.meta_fg.into()
                    })
                    .bg(bg),
            ),
            Rect::new(rect.x, rect.y, rect.width, 1),
        );
        for (index, (rect, label)) in self
            .buttons(canvas)
            .into_iter()
            .zip(["Close Esc", "Max F10", "Main ^Enter", "Move F6", "Size F7"])
            .enumerate()
        {
            let disabled = compact_window(canvas) && matches!(index, 1 | 3 | 4);
            let fill = if !disabled
                && (self.hover == Some(index)
                    || geometry.is_some_and(|resize| index == if resize { 4 } else { 3 }))
            {
                t.work_bg.lerp(t.work_hover_bg, 0.5)
            } else {
                t.work_bg.into_inner()
            };
            frame.render_widget(
                Paragraph::new(format!("\n{label}"))
                    .alignment(ratatui::layout::Alignment::Center)
                    .style(
                        Style::default()
                            .fg(if disabled {
                                t.meta_fg.into()
                            } else {
                                t.tinted_fg.into()
                            })
                            .bg(fill),
                    ),
                rect,
            );
        }
        frame.render_widget(
            Paragraph::new(match geometry {
                _ if compact_window(canvas) => " Full page · widen to move/resize",
                Some(false) => " Move: arrows · Enter apply · Esc cancel",
                Some(true) => " Size: arrows · Enter apply · Esc cancel",
                None => " Ctrl+Tab switch view · drag title / corner",
            })
            .style(Style::default().fg(t.meta_fg.into()).bg(bg)),
            Rect::new(
                rect.x + 1,
                rect.bottom().saturating_sub(1),
                rect.width.saturating_sub(4),
                1,
            ),
        );
        if !compact_window(canvas) {
            frame.render_widget(
                Paragraph::new("⇲").style(Style::default().fg(t.meta_fg.into()).bg(bg)),
                Rect::new(
                    rect.right().saturating_sub(2),
                    rect.bottom().saturating_sub(1),
                    2,
                    1,
                ),
            );
        }
    }
}

struct Geometry {
    key: ViewKey,
    resize: bool,
    pointer: Option<Position>,
    original: Rect,
}

pub(super) enum Action {
    Pass,
    Consume,
    Focus { key: ViewKey, forward: bool },
    Close(ViewKey),
    Promote(ViewKey),
}

pub(super) struct Workspace {
    active: Option<ViewKey>,
    main: Option<ViewKey>,
    pub views: HashMap<ViewKey, AttachedView>,
    windows: Vec<Window>,
    main_history: crate::flow_hub::History,
    surfaces: HashMap<ViewKey, Surface>,
    canvas: Rect,
    geometry: Option<Geometry>,
    background_start: usize,
    pub cursor_visible: bool,
}

impl Workspace {
    pub fn editing_geometry(&self) -> bool {
        self.geometry.is_some()
    }

    pub fn new(handle: &TuiHandle) -> Self {
        let key = handle
            .control_tx
            .as_ref()
            .and_then(|tx| tx.binding())
            .map(|binding| ViewKey {
                flow: binding.flow.clone(),
                placement: FlowPlacement::Main,
            });
        Self {
            active: key.clone(),
            main: key,
            views: HashMap::new(),
            windows: vec![],
            main_history: crate::flow_hub::History::default(),
            surfaces: HashMap::new(),
            canvas: Rect::default(),
            geometry: None,
            background_start: 0,
            cursor_visible: false,
        }
    }

    pub fn binding(&self, key: &ViewKey, handle: &TuiHandle) -> Option<ViewBinding> {
        let source = if self.active.as_ref() == Some(key) {
            handle
        } else {
            &self.views.get(key)?.handle
        };
        source.control_tx.as_ref()?.binding().cloned()
    }

    pub fn directory_owner(&self) -> Option<ViewKey> {
        self.active
            .as_ref()
            .filter(|key| key.placement == FlowPlacement::Floating)
            .cloned()
    }

    pub fn directory_history(&mut self) -> &mut crate::flow_hub::History {
        self.directory_history_for(self.directory_owner().as_ref())
    }

    pub fn directory_history_for(
        &mut self,
        owner: Option<&ViewKey>,
    ) -> &mut crate::flow_hub::History {
        match owner {
            None => &mut self.main_history,
            Some(key) => {
                &mut self
                    .windows
                    .iter_mut()
                    .find(|window| &window.key == key)
                    .expect("floating directory belongs to an open window")
                    .history
            }
        }
    }

    pub async fn focus(
        &mut self,
        ui: &mut UiState,
        editor: &mut InputEditor,
        handle: &mut TuiHandle,
        key: ViewKey,
        attachment: Option<Box<TuiHandle>>,
    ) -> Result<()> {
        let old = self
            .active
            .clone()
            .ok_or_else(|| anyhow::anyhow!("view is not bound"))?;
        if old != key {
            if !self.views.contains_key(&key) {
                let mut handle =
                    attachment.ok_or_else(|| anyhow::anyhow!("view attachment is missing"))?;
                anyhow::ensure!(
                    handle
                        .control_tx
                        .as_ref()
                        .and_then(|tx| tx.binding())
                        .is_some_and(|binding| binding.flow == key.flow),
                    "view source does not match destination"
                );
                let (ui, editor) = initialize_view(&mut handle).await?;
                self.views.insert(
                    key.clone(),
                    AttachedView {
                        ui,
                        editor,
                        handle: *handle,
                    },
                );
            }
            // Retain ownership until every fallible refresh has succeeded.
            let selected = self.views.get_mut(&key).unwrap();
            refresh_view(&mut selected.ui, &mut selected.editor, &mut selected.handle).await?;
            self.select_cached(ui, editor, handle, &key)?;
        } else if ui.app.flow_view_error.is_some() {
            refresh_view(ui, editor, handle).await?;
        }
        match key.placement {
            FlowPlacement::Main => self.main = Some(key),
            FlowPlacement::Floating => {
                let window = self
                    .windows
                    .iter()
                    .position(|window| window.key == key)
                    .map(|index| self.windows.remove(index))
                    .unwrap_or_else(|| Window {
                        key,
                        history: crate::flow_hub::History::default(),
                        rect: centered(self.canvas),
                        restore: None,
                        hover: None,
                    });
                self.windows.push(window);
            }
        }
        Ok(())
    }

    /// Transfer view ownership without reading a source or changing execution.
    fn select_cached(
        &mut self,
        ui: &mut UiState,
        editor: &mut InputEditor,
        handle: &mut TuiHandle,
        key: &ViewKey,
    ) -> Result<()> {
        let old = self
            .active
            .clone()
            .ok_or_else(|| anyhow::anyhow!("view is not bound"))?;
        if old == *key {
            return Ok(());
        }
        let selected = self
            .views
            .remove(key)
            .ok_or_else(|| anyhow::anyhow!("cached view is missing"))?;
        ui.app.transcript_selection_pointer = None;
        ui.app.thinking_click_origin = None;
        ui.selection_menu = None;
        ui.wm.interaction.drag_target = None;
        ui.wm.interaction.resize_target = None;
        self.views.insert(
            old,
            AttachedView {
                ui: std::mem::replace(ui, selected.ui),
                editor: std::mem::replace(editor, selected.editor),
                handle: std::mem::replace(handle, selected.handle),
            },
        );
        self.active = Some(key.clone());
        Ok(())
    }

    pub fn close(
        &mut self,
        ui: &mut UiState,
        editor: &mut InputEditor,
        handle: &mut TuiHandle,
        key: &ViewKey,
    ) -> Result<()> {
        if self.active.as_ref() == Some(key) {
            let main = self
                .main
                .clone()
                .ok_or_else(|| anyhow::anyhow!("main view is missing"))?;
            self.select_cached(ui, editor, handle, &main)?;
        }
        self.windows.retain(|window| &window.key != key);
        self.geometry = None;
        Ok(())
    }

    /// Move the selected view, including its draft and reply mailbox, into the main slot.
    pub fn promote(&mut self) {
        let Some(old) = self
            .active
            .clone()
            .filter(|key| key.placement == FlowPlacement::Floating)
        else {
            return;
        };
        let key = ViewKey {
            flow: old.flow.clone(),
            placement: FlowPlacement::Main,
        };
        if let Some(displaced) = self.views.remove(&key) {
            self.views.insert(old.clone(), displaced);
        }
        self.surfaces.remove(&old);
        self.surfaces.remove(&key);
        self.windows.retain(|window| window.key != old);
        self.active = Some(key.clone());
        self.main = Some(key);
        self.geometry = None;
    }

    pub fn render(
        &mut self,
        frame: &mut Frame,
        ui: &mut UiState,
        editor: &InputEditor,
    ) -> Result<()> {
        self.canvas = frame.area();
        if self.active.is_none() {
            render_frame(frame, ui, editor);
            return Ok(());
        }
        if self.canvas.width < 40 || self.canvas.height < 16 {
            self.cursor_visible = false;
            frame.render_widget(Paragraph::new("Terminal too small · Esc back"), self.canvas);
            return Ok(());
        }
        let mut layers = vec![(self.main.clone().unwrap(), self.canvas)];
        for window in &mut self.windows {
            if window.rect.width == 0 {
                window.rect = centered(self.canvas);
            }
            layers.push((window.key.clone(), window.content(self.canvas)));
        }
        let mut cursor = None;
        for (key, area) in layers {
            let focused = self.active.as_ref() == Some(&key);
            if let Some(window) = self.windows.iter().find(|window| window.key == key) {
                window.draw(
                    frame,
                    focused,
                    self.geometry
                        .as_ref()
                        .filter(|geometry| geometry.key == key)
                        .map(|geometry| geometry.resize),
                );
            }
            if area.is_empty() {
                continue;
            }
            let surface = match self.surfaces.entry(key.clone()) {
                std::collections::hash_map::Entry::Occupied(mut entry) => {
                    if entry.get().area != area {
                        entry.insert(Surface::new(area)?);
                    }
                    entry.into_mut()
                }
                std::collections::hash_map::Entry::Vacant(entry) => {
                    entry.insert(Surface::new(area)?)
                }
            };
            let position = if focused {
                surface.draw(frame, ui, editor)?
            } else if let Some(view) = self.views.get_mut(&key) {
                surface.draw(frame, &mut view.ui, &view.editor)?
            } else {
                None
            };
            if focused {
                cursor = position;
            } else if let Some(window) = self.windows.iter().find(|window| window.key == key)
                && cursor.is_some_and(|point| window.area(self.canvas).contains(point))
            {
                cursor = None;
            }
        }
        self.cursor_visible = cursor.is_some() && self.geometry.is_none();
        if self.cursor_visible {
            frame.set_cursor_position(cursor.unwrap());
        }
        Ok(())
    }

    /// Wait only on visible background readers. Dropping this future retains each reader's cursor.
    pub async fn next_background(&mut self) {
        let mut pending: Vec<_> = self
            .views
            .iter_mut()
            .filter(|(key, view)| {
                (self.main.as_ref() == Some(key)
                    || self.windows.iter().any(|window| &window.key == *key))
                    && view.handle.flow_view.is_some()
            })
            .map(|(_, view)| {
                Box::pin(async move {
                    view.handle.flow_view.as_mut().unwrap().set_pending_offset(view.ui.app.pending_page_offset);
                    let update = tokio::select! {
                        _ = changed(view.handle.session_name_rx.as_mut()) => {
                            refresh_session_name(&mut view.ui, &mut view.handle);
                            return;
                        }
                        _ = view.handle.settings.changed() => {
                            apply_provider_catalog_changed(&mut view.ui, None);
                            return;
                        }
                        _ = live_output_change(&mut view.handle.live_output) => None,
                        command = recv_cmd(&mut view.handle.cmd_rx, &mut view.handle.control_reply_rx, view.handle.control_tx.as_ref().and_then(|tx| tx.binding())) => {
                            if let Some((ticket, command)) = command { apply_view_command(&mut view.ui, &mut view.editor, ticket.as_ref(), command, view.handle.control_tx.as_ref()); }
                            return;
                        }
                        update = view.handle.flow_view.as_mut().unwrap().next(), if view.ui.app.flow_view_error.is_none() => Some(update),
                        _ = changed(view.handle.context_rx.as_mut()) => None,
                        _ = changed(view.handle.trust_rx.as_mut()) => None,
                        _ = changed(view.handle.attach_rx.as_mut()) => None,
                        _ = changed(view.handle.form_rx.as_mut()) => None,
                        _ = changed(view.handle.compact_review_rx.as_mut()) => None,
                        _ = permission_change(&mut view.handle.stream_rx) => None,
                    };
                    let Some(update) = update else {
                        refresh_instance_live(&mut view.ui, &mut view.editor, &view.handle);
                        update_live_output(&mut view.ui, &mut view.handle);
                        return;
                    };
                    let result = update.and_then(|update| view.ui.app.sync_flow_view(update));
                    update_flow_view(&mut view.ui, &mut view.handle, result);
                })
            })
            .collect();
        if pending.is_empty() {
            std::future::pending::<()>().await;
        } else {
            let count = pending.len();
            let start = self.background_start % count;
            pending.rotate_left(start);
            let (_, selected, _) = futures::future::select_all(pending).await;
            self.background_start = (start + selected + 1) % count;
        }
    }

    pub fn event(&mut self, event: &Event, shortcuts: bool) -> Action {
        if self.active.is_none() {
            return Action::Pass;
        }
        if matches!(event, Event::Key(key) if map_key(*key) == KeyAction::Quit) {
            return Action::Pass;
        }
        if compact_window(self.canvas)
            && let Some(geometry) = self.geometry.take()
        {
            if let Some(window) = self
                .windows
                .iter_mut()
                .find(|window| window.key == geometry.key)
            {
                window.rect = geometry.original;
            }
        }
        if let Some(geometry) = self.geometry.as_mut() {
            let Some(window) = self
                .windows
                .iter_mut()
                .find(|window| window.key == geometry.key)
            else {
                self.geometry = None;
                return Action::Consume;
            };
            match event {
                Event::Mouse(mouse) if geometry.pointer.is_some() => {
                    if matches!(mouse.kind, MouseEventKind::Drag(MouseButton::Left)) {
                        let start = geometry.pointer.unwrap();
                        window.rect = adjusted(
                            geometry.original,
                            i32::from(mouse.column) - i32::from(start.x),
                            i32::from(mouse.row) - i32::from(start.y),
                            geometry.resize,
                            self.canvas,
                        );
                    } else if matches!(mouse.kind, MouseEventKind::Up(_)) {
                        self.geometry = None;
                    }
                }
                Event::Key(key) if key.kind != KeyEventKind::Release => {
                    let (dx, dy) = match key.code {
                        KeyCode::Left => (-2, 0),
                        KeyCode::Right => (2, 0),
                        KeyCode::Up => (0, -1),
                        KeyCode::Down => (0, 1),
                        KeyCode::Esc => {
                            window.rect = geometry.original;
                            self.geometry = None;
                            return Action::Consume;
                        }
                        KeyCode::Enter => {
                            self.geometry = None;
                            return Action::Consume;
                        }
                        _ => return Action::Consume,
                    };
                    window.rect = adjusted(window.rect, dx, dy, geometry.resize, self.canvas);
                }
                _ => {}
            }
            return Action::Consume;
        }
        match event {
            Event::Key(key) if shortcuts && key.kind != KeyEventKind::Release => {
                let active = self.active.clone().unwrap();
                if key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Tab | KeyCode::BackTab)
                {
                    let mut floating: Vec<_> = self
                        .windows
                        .iter()
                        .map(|window| window.key.clone())
                        .collect();
                    floating.sort_by_key(|key| (key.flow.session_id(), key.flow.label()));
                    let keys: Vec<_> = std::iter::once(self.main.clone().unwrap())
                        .chain(floating)
                        .collect();
                    let current = keys.iter().position(|key| *key == active).unwrap_or(0);
                    let reverse =
                        key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
                    let index = if reverse {
                        (current + keys.len() - 1) % keys.len()
                    } else {
                        (current + 1) % keys.len()
                    };
                    return Action::Focus {
                        key: keys[index].clone(),
                        forward: false,
                    };
                }
                if active.placement == FlowPlacement::Floating {
                    match key.code {
                        KeyCode::Esc => return Action::Close(active),
                        KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Action::Promote(active);
                        }
                        KeyCode::F(10) => {
                            self.maximize(&active);
                            return Action::Consume;
                        }
                        KeyCode::F(6 | 7) => {
                            self.begin_geometry(&active, key.code == KeyCode::F(7), None);
                            return Action::Consume;
                        }
                        _ => {}
                    }
                }
                Action::Pass
            }
            Event::Mouse(mouse) => {
                let point = Position::new(mouse.column, mouse.row);
                let top = self
                    .windows
                    .iter()
                    .rposition(|window| window.area(self.canvas).contains(point));
                for (index, window) in self.windows.iter_mut().enumerate() {
                    window.hover = if Some(index) == top {
                        window
                            .buttons(self.canvas)
                            .iter()
                            .position(|rect| rect.contains(point))
                    } else {
                        None
                    };
                }
                let key = top
                    .map(|index| self.windows[index].key.clone())
                    .unwrap_or_else(|| self.main.clone().unwrap());
                let down = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
                if let Some(index) = top {
                    let window = &self.windows[index];
                    if down {
                        match window.hover {
                            Some(0) => return Action::Close(key),
                            Some(1) => {
                                self.maximize(&key);
                                return Action::Focus {
                                    key,
                                    forward: false,
                                };
                            }
                            Some(2) => return Action::Promote(key),
                            Some(3 | 4) => {
                                self.begin_geometry(&key, window.hover == Some(4), None);
                                return Action::Focus {
                                    key,
                                    forward: false,
                                };
                            }
                            _ => {}
                        }
                        let area = window.area(self.canvas);
                        let resize = point.y == area.bottom().saturating_sub(1)
                            && point.x >= area.right().saturating_sub(3);
                        if !compact_window(self.canvas) && (point.y == area.y || resize) {
                            self.begin_geometry(&key, resize, Some(point));
                            return Action::Focus {
                                key,
                                forward: false,
                            };
                        }
                    }
                    if !window.content(self.canvas).contains(point) {
                        return Action::Consume;
                    }
                }
                if self.active.as_ref() != Some(&key) {
                    if down
                        || matches!(
                            mouse.kind,
                            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                        )
                    {
                        return Action::Focus { key, forward: true };
                    }
                    return Action::Consume;
                }
                Action::Pass
            }
            _ => Action::Pass,
        }
    }

    fn maximize(&mut self, key: &ViewKey) {
        if compact_window(self.canvas) {
            return;
        }
        if let Some(window) = self.windows.iter_mut().find(|window| &window.key == key) {
            if let Some(rect) = window.restore.take() {
                window.rect = rect;
            } else {
                window.restore = Some(window.rect);
                window.rect = self.canvas;
            }
        }
    }

    fn begin_geometry(&mut self, key: &ViewKey, resize: bool, pointer: Option<Position>) {
        if compact_window(self.canvas) {
            return;
        }
        if let Some(window) = self.windows.iter_mut().find(|window| &window.key == key) {
            let area = window.area(self.canvas);
            window.restore = None;
            window.rect = area;
            self.geometry = Some(Geometry {
                key: key.clone(),
                resize,
                pointer,
                original: window.rect,
            });
        }
    }
}

fn compact_window(canvas: Rect) -> bool {
    canvas.width < 64
}

fn centered(canvas: Rect) -> Rect {
    let width = (canvas.width * 4 / 5).max(64).min(canvas.width);
    let height = (canvas.height * 4 / 5).max(20).min(canvas.height);
    Rect::new(
        canvas.x + (canvas.width - width) / 2,
        canvas.y + (canvas.height - height) / 2,
        width,
        height,
    )
}

fn bounded(rect: Rect, canvas: Rect) -> Rect {
    let width = rect.width.max(64).min(canvas.width);
    let height = rect.height.max(20).min(canvas.height);
    Rect::new(
        rect.x.clamp(canvas.x, canvas.right().saturating_sub(width)),
        rect.y
            .clamp(canvas.y, canvas.bottom().saturating_sub(height)),
        width,
        height,
    )
}

fn adjusted(rect: Rect, dx: i32, dy: i32, resize: bool, canvas: Rect) -> Rect {
    let adjust =
        |value: u16, delta: i32| (i32::from(value) + delta).clamp(0, i32::from(u16::MAX)) as u16;
    bounded(
        if resize {
            Rect::new(
                rect.x,
                rect.y,
                adjust(rect.width, dx),
                adjust(rect.height, dy),
            )
        } else {
            Rect::new(
                adjust(rect.x, dx),
                adjust(rect.y, dy),
                rect.width,
                rect.height,
            )
        },
        canvas,
    )
}

pub(super) async fn changed<T>(rx: Option<&mut tokio::sync::watch::Receiver<T>>) {
    if let Some(rx) = rx
        && rx.changed().await.is_ok()
    {
        return;
    }
    std::future::pending::<()>().await;
}

pub(super) async fn permission_change(
    rx: &mut Option<tokio::sync::broadcast::Receiver<StreamFrame>>,
) {
    loop {
        let Some(receiver) = rx.as_mut() else {
            return std::future::pending().await;
        };
        match receiver.recv().await {
            Ok(frame) if is_live_instance_frame(&frame) => return,
            // A missed permission event is recovered from the broker snapshot.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => return,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                *rx = None;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn permission_subscription_resynchronizes_lag_and_quiesces_after_close() {
        let (sender, receiver) = tokio::sync::broadcast::channel(1);
        let mut receiver = Some(receiver);
        // Any dropped frame may contain a permission update; reread the registry.
        for _ in 0..4 {
            sender.send(StreamFrame::Note("unrelated".into())).unwrap();
        }
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                permission_change(&mut receiver)
            )
            .await
            .is_ok()
        );
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(20),
                permission_change(&mut receiver)
            )
            .await
            .is_err()
        );
        drop(sender);
        for _ in 0..2 {
            assert!(
                tokio::time::timeout(
                    std::time::Duration::from_millis(20),
                    permission_change(&mut receiver)
                )
                .await
                .is_err()
            );
            assert!(receiver.is_none());
        }
    }

    fn window() -> Window {
        Window {
            history: crate::flow_hub::History::default(),
            key: ViewKey {
                flow: ViewAddress::Instance(atman_runtime::flow_store::FlowRef {
                    session_id: uuid::Uuid::new_v4().into(),
                    instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
                }),
                placement: FlowPlacement::Floating,
            },
            rect: Rect::new(16, 5, 108, 35),
            restore: None,
            hover: None,
        }
    }

    #[test]
    fn directory_history_follows_the_main_surface_and_each_floating_window() {
        use crate::control::ControlTicket;
        use crate::flow_hub::{Action as HubAction, FlowHub, FlowHubPage, HubRequest};
        use crate::project_hub::SessionTarget;

        let first = window();
        let second = window();
        let first_key = first.key.clone();
        let second_key = second.key.clone();
        let mut main = first_key.clone();
        main.placement = FlowPlacement::Main;
        let mut workspace = Workspace {
            active: Some(main.clone()),
            main: Some(main.clone()),
            views: HashMap::new(),
            windows: vec![first, second],
            main_history: crate::flow_hub::History::default(),
            surfaces: HashMap::new(),
            canvas: Rect::default(),
            geometry: None,
            background_start: 0,
            cursor_visible: false,
        };
        let ids = [
            uuid::Uuid::now_v7(),
            uuid::Uuid::now_v7(),
            uuid::Uuid::now_v7(),
        ];
        for (key, id) in [&main, &first_key, &second_key].into_iter().zip(ids) {
            workspace.active = Some(key.clone());
            let ticket = ControlTicket {
                binding: ViewBinding {
                    view_id: uuid::Uuid::now_v7(),
                    revision: 1,
                    flow: key.flow.clone(),
                },
                request_id: uuid::Uuid::now_v7(),
            };
            let mut hub = FlowHub::default();
            hub.pending = Some(ticket.clone());
            hub.receive(
                Some(&ticket),
                Ok(Box::new(FlowHubPage::Sessions(
                    crate::flow_hub::SessionDirectory {
                        sessions: vec![crate::flow_hub::SessionRow {
                            cleanup_pending: false,
                            target: SessionTarget::Persistent(id),
                            title: id.to_string(),
                            detail: String::new(),
                        }],
                        next: None,
                        total: 1,
                    },
                ))),
            );
            workspace.directory_history().save(hub);
        }
        // Focusing another conversation in the main surface retains its return directory.
        main.flow = second_key.flow.clone();
        for (key, id) in [&main, &first_key, &second_key].into_iter().zip(ids) {
            workspace.active = Some(key.clone());
            let owner = workspace.directory_owner();
            workspace.active = Some(second_key.clone());
            let (mut hub, action) = workspace
                .directory_history_for(owner.as_ref())
                .open(HubAction::Return, HubRequest::Sessions { before: None });
            assert!(matches!(action, HubAction::None));
            assert!(
                matches!(hub.key(crate::keys::KeyAction::Submit), HubAction::Load(HubRequest::Flows { session, .. }) if session == id)
            );
        }
    }

    #[test]
    fn window_layout_keeps_controls_readable_and_content_usable_across_canvas_sizes() {
        let window = window();
        let preferred = window.rect;
        for width in 40..=160 {
            let canvas = Rect::new(0, 0, width, 24);
            let content = window.content(canvas);
            assert!(content.width >= 40 && content.height >= 8);
            let buttons = window.buttons(canvas);
            for (index, button) in buttons.iter().enumerate() {
                assert_eq!(button.height, 3);
                assert_eq!(button.intersection(canvas), *button);
                assert!(button.intersection(content).is_empty());
                assert!(
                    buttons[..index]
                        .iter()
                        .all(|other| other.intersection(*button).is_empty())
                );
            }
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal
                .draw(|frame| window.draw(frame, true, None))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let lines: Vec<String> = (0..24)
                .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();
            for label in ["Close Esc", "Max F10", "Main ^Enter", "Move F6", "Size F7"] {
                assert!(
                    lines.iter().any(|line| line.contains(label)),
                    "{width}: {label}"
                );
            }
        }
        assert_eq!(window.rect, preferred);
        assert_eq!(window.area(Rect::new(0, 0, 140, 45)), preferred);
    }

    #[test]
    fn compact_window_controls_preserve_geometry_and_escape_only_closes_the_view() {
        let window = window();
        let key = window.key.clone();
        let preferred = window.rect;
        let mut workspace = Workspace {
            active: Some(key.clone()),
            main: Some(key.clone()),
            views: HashMap::new(),
            windows: vec![window],
            main_history: crate::flow_hub::History::default(),
            surfaces: HashMap::new(),
            canvas: Rect::new(0, 0, 40, 24),
            geometry: None,
            background_start: 0,
            cursor_visible: false,
        };
        for code in [KeyCode::F(6), KeyCode::F(7), KeyCode::F(10)] {
            assert!(matches!(
                workspace.event(
                    &Event::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE)),
                    true
                ),
                Action::Consume
            ));
            assert!(workspace.geometry.is_none());
            assert_eq!(workspace.windows[0].rect, preferred);
        }
        let close = workspace.windows[0].buttons(workspace.canvas)[0];
        let click = Event::Mouse(crossterm::event::MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: close.x,
            row: close.y,
            modifiers: KeyModifiers::NONE,
        });
        assert!(matches!(workspace.event(&click, true), Action::Close(target) if target == key));
        assert!(
            matches!(workspace.event(&Event::Key(crossterm::event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)), true), Action::Close(target) if target == key)
        );
        workspace.canvas = Rect::new(0, 0, 140, 45);
        workspace.maximize(&key);
        assert_eq!(
            workspace.windows[0].area(workspace.canvas),
            workspace.canvas
        );
        workspace.canvas = Rect::new(0, 0, 40, 24);
        assert_eq!(
            workspace.windows[0].area(workspace.canvas),
            workspace.canvas
        );
        workspace.canvas = Rect::new(0, 0, 140, 45);
        workspace.maximize(&key);
        assert_eq!(workspace.windows[0].area(workspace.canvas), preferred);
    }

    #[test]
    fn complete_view_rendering_is_independent_of_surface_origin() {
        let render = |x, y| {
            let area = Rect::new(x, y, 108, 28);
            let mut surface = Surface::new(area).unwrap();
            let mut ui = UiState::new(AppState::new("surface".into(), None));
            ui.app.items =
                crate::app::FlowViewModel::from(vec![crate::app::OutputItem::AssistantMd {
                    md: "answer from child\n\n中文 **bold**\n\n```rust\nlet value = 7;\n```".into(),
                    streaming: false,
                    retried: false,
                }]);
            let mut editor = InputEditor::default();
            editor.insert_str("draft child");
            let mut target = Terminal::new(TestBackend::new(area.right(), area.bottom())).unwrap();
            target
                .draw(|frame| {
                    surface.draw(frame, &mut ui, &editor).unwrap();
                })
                .unwrap();
            let buffer = target.backend().buffer();
            (area.y..area.bottom())
                .flat_map(|y| (area.x..area.right()).map(move |x| buffer[(x, y)].clone()))
                .collect::<Vec<_>>()
        };
        let main = render(0, 0);
        let floating = render(16, 10);
        for (index, (main, floating)) in main.iter().zip(&floating).enumerate() {
            assert_eq!(
                main,
                floating,
                "surface cell at ({}, {})",
                index % 108,
                index / 108
            );
        }
    }
}
