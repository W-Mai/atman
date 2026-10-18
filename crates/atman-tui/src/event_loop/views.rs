//! Placement and focus for complete flow views. Execution belongs to the host.

use std::collections::HashMap;

use atman_runtime::flow_store::FlowRef;
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind};
use ratatui::{
    Frame, TerminalOptions, Viewport,
    backend::TestBackend,
    layout::{Position, Rect},
    style::Style,
    widgets::Paragraph,
};

use super::*;
use crate::control::{FlowPlacement, ViewBinding};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(super) struct ViewKey {
    pub flow: FlowRef,
    pub placement: FlowPlacement,
}

pub(super) struct AttachedView {
    pub ui: UiState,
    pub editor: InputEditor,
    pub handle: TuiHandle,
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
    rect: Rect,
    restore: Option<Rect>,
    hover: Option<usize>,
}

impl Window {
    fn content(&self) -> Rect {
        Rect::new(
            self.rect.x.saturating_add(2),
            self.rect.y.saturating_add(6),
            self.rect.width.saturating_sub(4),
            self.rect.height.saturating_sub(8),
        )
    }

    fn buttons(&self) -> [Rect; 5] {
        let width = self.rect.width.saturating_sub(4) / 5;
        std::array::from_fn(|index| {
            Rect::new(
                self.rect.x + 2 + index as u16 * width,
                self.rect.y + 2,
                width.saturating_sub(1),
                3,
            )
        })
    }

    fn draw(&self, frame: &mut Frame, active: bool, geometry: Option<bool>) {
        let t = crate::theme::theme();
        crate::wm::shadow::render_shadow(frame, self.rect, &t);
        let bg = crate::wm::shell::surface_bg(&t);
        frame.render_widget(ratatui::widgets::Clear, self.rect);
        frame.render_widget(Paragraph::new("").style(Style::default().bg(bg)), self.rect);
        frame.render_widget(
            Paragraph::new(format!("  Flow {}", self.key.flow.instance_id)).style(
                Style::default()
                    .fg(if active {
                        t.heading.into()
                    } else {
                        t.meta_fg.into()
                    })
                    .bg(bg),
            ),
            Rect::new(self.rect.x, self.rect.y, self.rect.width, 1),
        );
        for (index, (rect, label)) in self
            .buttons()
            .into_iter()
            .zip(["Close Esc", "Max F10", "Main ^Enter", "Move F6", "Size F7"])
            .enumerate()
        {
            let fill = if self.hover == Some(index)
                || geometry.is_some_and(|resize| index == if resize { 4 } else { 3 })
            {
                t.work_bg.lerp(t.work_hover_bg, 0.5)
            } else {
                t.work_bg.into_inner()
            };
            frame.render_widget(
                Paragraph::new(format!("\n{label}"))
                    .alignment(ratatui::layout::Alignment::Center)
                    .style(Style::default().fg(t.tinted_fg.into()).bg(fill)),
                rect,
            );
        }
        frame.render_widget(
            Paragraph::new(match geometry {
                Some(false) => " Move: arrows · Enter apply · Esc cancel",
                Some(true) => " Size: arrows · Enter apply · Esc cancel",
                None => " Ctrl+Tab switch view · drag title / corner",
            })
            .style(Style::default().fg(t.meta_fg.into()).bg(bg)),
            Rect::new(
                self.rect.x + 1,
                self.rect.bottom().saturating_sub(1),
                self.rect.width.saturating_sub(4),
                1,
            ),
        );
        frame.render_widget(
            Paragraph::new("⇲").style(Style::default().fg(t.meta_fg.into()).bg(bg)),
            Rect::new(
                self.rect.right().saturating_sub(2),
                self.rect.bottom().saturating_sub(1),
                2,
                1,
            ),
        );
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
    surfaces: HashMap<ViewKey, Surface>,
    canvas: Rect,
    geometry: Option<Geometry>,
    background_start: usize,
    pub cursor_visible: bool,
}

impl Workspace {
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
            let mut selected = if let Some(view) = self.views.remove(&key) {
                view
            } else {
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
                AttachedView {
                    ui,
                    editor,
                    handle: *handle,
                }
            };
            if let Some(reader) = selected.handle.flow_view.as_mut() {
                if let Err(error) =
                    async { selected.ui.app.sync_flow_view(reader.refresh().await?) }.await
                {
                    self.views.insert(key, selected);
                    return Err(error.into());
                }
                if let Some(snapshot) = selected.ui.app.items.snapshot() {
                    selected.ui.wm.flow_snapshot(snapshot.clone());
                }
            }
            refresh_instance_live(&mut selected.ui, &mut selected.editor, &selected.handle);
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
                        rect: centered(self.canvas),
                        restore: None,
                        hover: None,
                    });
                self.windows.push(window);
            }
        }
        Ok(())
    }

    pub async fn close(
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
            self.focus(ui, editor, handle, main, None).await?;
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
            window.rect = if window.restore.is_some() {
                self.canvas
            } else {
                bounded(window.rect, self.canvas)
            };
            layers.push((window.key.clone(), window.content()));
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
                && cursor.is_some_and(|point| window.rect.contains(point))
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
                    let update = tokio::select! {
                        command = recv_cmd(&mut view.handle.cmd_rx, &mut view.handle.control_reply_rx, view.handle.control_tx.as_ref().and_then(|tx| tx.binding())) => {
                            if let Some((ticket, command)) = command { apply_view_command(&mut view.ui, &mut view.editor, ticket.as_ref(), command); }
                            return;
                        }
                        update = view.handle.flow_view.as_mut().unwrap().next() => Some(update),
                        _ = changed(view.handle.context_rx.as_mut()) => None,
                        _ = changed(view.handle.trust_rx.as_mut()) => None,
                        _ = changed(view.handle.attach_rx.as_mut()) => None,
                        _ = changed(view.handle.form_rx.as_mut()) => None,
                        _ = changed(view.handle.compact_review_rx.as_mut()) => None,
                        _ = permission_change(&mut view.handle.stream_rx) => None,
                    };
                    let Some(update) = update else {
                        refresh_instance_live(&mut view.ui, &mut view.editor, &view.handle);
                        return;
                    };
                    match update {
                        Ok(update) => {
                            if let Err(error) = view.ui.app.sync_flow_view(update) {
                                view.handle.flow_view = None;
                                view.ui.app.push_toast(
                                    format!("View disconnected: {error}"),
                                    crate::app::NoteLevel::Error,
                                    std::time::Duration::from_secs(8),
                                    crate::app::ToastPosition::TopRight,
                                );
                            } else if let Some(snapshot) = view.ui.app.items.snapshot() {
                                view.ui.wm.flow_snapshot(snapshot.clone());
                            }
                        }
                        Err(error) => {
                            view.handle.flow_view = None;
                            view.ui.app.push_toast(
                                format!("View disconnected: {error}"),
                                crate::app::NoteLevel::Error,
                                std::time::Duration::from_secs(8),
                                crate::app::ToastPosition::TopRight,
                            );
                        }
                    }
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
                    floating.sort_by_key(|key| key.flow.instance_id.to_string());
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
                    .rposition(|window| window.rect.contains(point));
                for (index, window) in self.windows.iter_mut().enumerate() {
                    window.hover = if Some(index) == top {
                        window
                            .buttons()
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
                        let resize = point.y == window.rect.bottom().saturating_sub(1)
                            && point.x >= window.rect.right().saturating_sub(3);
                        if point.y == window.rect.y || resize {
                            self.begin_geometry(&key, resize, Some(point));
                            return Action::Focus {
                                key,
                                forward: false,
                            };
                        }
                    }
                    if !window.content().contains(point) {
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
        if let Some(window) = self.windows.iter_mut().find(|window| &window.key == key) {
            if let Some(rect) = window.restore.take() {
                window.rect = bounded(rect, self.canvas);
            } else {
                window.restore = Some(window.rect);
                window.rect = self.canvas;
            }
        }
    }

    fn begin_geometry(&mut self, key: &ViewKey, resize: bool, pointer: Option<Position>) {
        if let Some(window) = self.windows.iter_mut().find(|window| &window.key == key) {
            window.restore = None;
            self.geometry = Some(Geometry {
                key: key.clone(),
                resize,
                pointer,
                original: window.rect,
            });
        }
    }
}

fn centered(canvas: Rect) -> Rect {
    let width = (canvas.width * 4 / 5).max(60).min(canvas.width);
    let height = (canvas.height * 4 / 5).max(20).min(canvas.height);
    Rect::new(
        canvas.x + (canvas.width - width) / 2,
        canvas.y + (canvas.height - height) / 2,
        width,
        height,
    )
}

fn bounded(rect: Rect, canvas: Rect) -> Rect {
    let width = rect.width.max(60).min(canvas.width);
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

async fn changed<T>(rx: Option<&mut tokio::sync::watch::Receiver<T>>) {
    if let Some(rx) = rx
        && rx.changed().await.is_ok()
    {
        return;
    }
    std::future::pending::<()>().await;
}

async fn permission_change(rx: &mut tokio::sync::broadcast::Receiver<StreamFrame>) {
    loop {
        match rx.recv().await {
            Ok(frame) if is_live_instance_frame(&frame) => return,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                std::future::pending::<()>().await
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
