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
    refresh_activity(ui, handle);
    refresh_session_name(ui, handle);
    refresh_live_output(ui, handle)?;
    if handle.settings.take_changes() {
        apply_provider_catalog_changed(ui, None);
    }
    Ok(())
}

async fn replace_attachment(
    ui: &mut UiState,
    editor: &mut InputEditor,
    handle: &mut TuiHandle,
    mut replacement: Box<TuiHandle>,
) -> Result<()> {
    // An unresolved submission still owns its original reply channel.
    anyhow::ensure!(
        ui.app.pending_submission.is_none() && ui.app.queue_recovery_ticket.is_none(),
        "Waiting for the pending input or recovery acknowledgement; retry opening this session afterward"
    );
    refresh_view(ui, editor, &mut replacement).await?;
    ui.app.session = replacement.session.clone();
    ui.app.settings = replacement.settings.clone();
    ui.wm.modals.bind_settings(replacement.settings.clone());
    ui.app.task_registry = replacement.task_registry.take();
    ui.app.continued_session = replacement.continued_session.clone();
    *handle = *replacement;
    refresh_instance_live(ui, editor, handle);
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
        composer: Option<&crate::render::ComposerLayout>,
    ) -> Result<()> {
        let completed = self.terminal.draw(|f| {
            crate::render::render_flow(f, ui, editor, crate::render::InputLayer::Detached(composer))
        })?;
        let area = self.area.intersection(frame.area());
        crate::sanitize_widget_edges(frame, area);
        for y in area.y..area.bottom() {
            for x in area.x..area.right() {
                frame.buffer_mut()[(x, y)] = completed.buffer[(x, y)].clone();
            }
        }
        Ok(())
    }
}

struct Window {
    key: ViewKey,
    history: crate::flow_hub::History,
    rect: Rect,
    restore: Option<Rect>,
    hover: Option<crate::wm::PanelBtn>,
    last_titlebar_click: Option<std::time::Instant>,
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
        crate::wm::shell::content_area(self.area(canvas))
    }

    fn draw(
        &self,
        frame: &mut Frame,
        canvas: Rect,
        active: bool,
        geometry: Option<bool>,
        title: &str,
    ) {
        let rect = self.area(canvas);
        let t = crate::theme::theme();
        crate::wm::shell::render_surface(
            frame,
            crate::wm::shell::Shell {
                rect,
                title,
                icon: "⬡",
                maximized: self.restore.is_some(),
                close: Some(false),
            },
            active,
            self.hover,
            &t,
        );
        if let Some(resize) = geometry {
            frame.render_widget(
                Paragraph::new(if resize {
                    " Size: arrows · Enter apply · Esc cancel"
                } else {
                    " Move: arrows · Enter apply · Esc cancel"
                })
                .style(
                    Style::default()
                        .fg(t.meta_fg.into())
                        .bg(crate::wm::shell::surface_bg(&t)),
                ),
                Rect::new(
                    rect.x + 3,
                    rect.bottom().saturating_sub(1),
                    rect.width.saturating_sub(7),
                    1,
                ),
            );
        }
        crate::wm::shadow::render_shadow(frame, rect, &t);
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

#[derive(Default)]
struct DockMotion {
    owner: Option<ViewKey>,
    drawn: Option<Rect>,
    from: Rect,
    started: Option<std::time::Instant>,
}

impl DockMotion {
    fn place(
        &mut self,
        owner: &ViewKey,
        target: Rect,
        immediate: bool,
        now: std::time::Instant,
    ) -> Rect {
        if self.owner.as_ref() != Some(owner) {
            self.from = self.drawn.unwrap_or(target);
            self.started = self.drawn.map(|_| now);
            self.owner = Some(owner.clone());
        }
        if immediate {
            self.started = None;
        }
        let progress = self.started.map_or(1.0, |start| {
            now.saturating_duration_since(start).as_secs_f32() / 0.21
        });
        let drawn = if progress >= 1.0 {
            self.started = None;
            target
        } else {
            let t = crate::render::ease_out(progress);
            let mix =
                |a: u16, b: u16| (f32::from(a) + (f32::from(b) - f32::from(a)) * t).round() as u16;
            Rect {
                x: mix(self.from.x, target.x),
                y: mix(self.from.y, target.y),
                ..target
            }
        };
        self.drawn = Some(drawn);
        drawn
    }
}

pub(super) struct Workspace {
    active: Option<ViewKey>,
    main: Option<ViewKey>,
    pub views: HashMap<ViewKey, AttachedView>,
    windows: Vec<Window>,
    main_history: crate::flow_hub::History,
    surfaces: HashMap<ViewKey, Surface>,
    canvas: Rect,
    dock_canvas: Rect,
    composer_rect: Option<Rect>,
    motion: DockMotion,
    sidebar: std::sync::Arc<std::sync::Mutex<crate::project_sidebar::ProjectSidebar>>,
    geometry: Option<Geometry>,
    background_start: usize,
    pub cursor_visible: bool,
}

impl Workspace {
    pub fn active_placement(&self) -> FlowPlacement {
        self.active
            .as_ref()
            .map(|key| key.placement)
            .unwrap_or_default()
    }

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
            dock_canvas: Rect::default(),
            composer_rect: None,
            motion: DockMotion::default(),
            sidebar: Default::default(),
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

    pub fn animating(&self) -> bool {
        self.motion.started.is_some()
    }

    fn window_canvas(&self, key: &ViewKey) -> Rect {
        if self.active.as_ref() == Some(key) {
            self.dock_canvas
        } else {
            self.canvas
        }
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
        if let Some(source) = &attachment {
            anyhow::ensure!(
                source
                    .control_tx
                    .as_ref()
                    .and_then(|tx| tx.binding())
                    .is_some_and(|binding| binding.flow == key.flow),
                "view source does not match destination"
            );
        }
        if old != key {
            if let Some(selected) = self.views.get_mut(&key) {
                if let Some(replacement) = attachment {
                    replace_attachment(
                        &mut selected.ui,
                        &mut selected.editor,
                        &mut selected.handle,
                        replacement,
                    )
                    .await?;
                } else {
                    refresh_view(&mut selected.ui, &mut selected.editor, &mut selected.handle)
                        .await?;
                }
            } else {
                let mut handle =
                    attachment.ok_or_else(|| anyhow::anyhow!("view attachment is missing"))?;
                let (mut next_ui, editor) = initialize_view(&mut handle).await?;
                next_ui.app.project_sidebar = ui.app.project_sidebar.clone();
                self.views.insert(
                    key.clone(),
                    AttachedView {
                        ui: next_ui,
                        editor,
                        handle: *handle,
                    },
                );
            }
            // Retain ownership until every fallible refresh has succeeded.
            self.select_cached(ui, editor, handle, &key)?;
        } else if let Some(replacement) = attachment {
            replace_attachment(ui, editor, handle, replacement).await?;
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
                        last_titlebar_click: None,
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
        self.sidebar = ui.app.project_sidebar.clone();
        self.canvas = self.sidebar.lock().unwrap().layout(frame.area());
        if self.active.is_none() {
            render_frame(frame, ui, editor);
            return Ok(());
        }
        if self.canvas.width < 40 || self.canvas.height < 16 {
            self.cursor_visible = false;
            frame.render_widget(Paragraph::new("Terminal too small · Esc back"), self.canvas);
            return Ok(());
        }
        let main_focus = self.active_placement() == FlowPlacement::Main;
        let main_area = crate::layout::FocusLayout::new(
            crate::layout::compute(self.canvas).transcript,
            ui.app.startup_card.is_none() && ui.app.startup_intro.is_none(),
            ui.app.focus_sidebar_expanded(),
        )
        .conversation;
        let mut composer = crate::render::ComposerLayout::bottom(
            &ui.app,
            editor,
            if main_focus { main_area } else { self.canvas },
        );
        self.dock_canvas = Rect {
            height: self.canvas.height.saturating_sub(composer.outer.height + 1),
            ..self.canvas
        };
        if !main_focus && self.dock_canvas.height < 12 {
            self.cursor_visible = false;
            frame.render_widget(
                Paragraph::new("Terminal too small for panel and input · Esc back"),
                self.canvas,
            );
            return Ok(());
        }
        if main_focus {
            composer = crate::render::ComposerLayout::for_view(&ui.app, editor, main_area);
        } else if let Some(window) = self
            .windows
            .iter_mut()
            .find(|w| Some(&w.key) == self.active.as_ref())
        {
            if window.rect.width == 0 {
                window.rect = centered(self.dock_canvas);
            }
            window.rect = bounded(window.rect, self.dock_canvas);
            let rect = window.area(self.dock_canvas);
            let x = (i32::from(rect.x)
                + (i32::from(rect.width) - i32::from(composer.outer.width)) / 2)
                .clamp(
                    i32::from(self.canvas.x),
                    i32::from(self.canvas.right().saturating_sub(composer.outer.width)),
                ) as u16;
            composer.move_to(x, rect.bottom() + 1);
        }
        let position = self.motion.place(
            self.active.as_ref().unwrap(),
            composer.outer,
            self.geometry.is_some(),
            std::time::Instant::now(),
        );
        composer.move_visual_to(
            position.x.clamp(
                self.canvas.x,
                self.canvas.right().saturating_sub(composer.outer.width),
            ),
            position.y.clamp(
                self.canvas.y,
                self.canvas.bottom().saturating_sub(composer.outer.height),
            ),
        );
        self.composer_rect = Some(composer.outer);
        let mut layers = vec![(self.main.clone().unwrap(), self.canvas)];
        for window in &mut self.windows {
            if window.rect.width == 0 {
                window.rect = centered(self.canvas);
            }
            let canvas = if Some(&window.key) == self.active.as_ref() {
                self.dock_canvas
            } else {
                self.canvas
            };
            layers.push((window.key.clone(), window.content(canvas)));
        }
        for (key, area) in layers {
            let focused = self.active.as_ref() == Some(&key);
            if let Some(window) = self.windows.iter().find(|window| window.key == key) {
                let app = if focused {
                    Some(&ui.app)
                } else {
                    self.views.get(&key).map(|view| &view.ui.app)
                };
                let title = app
                    .and_then(|app| app.items.snapshot())
                    .map(|snapshot| snapshot.display_title.clone())
                    .unwrap_or_else(|| key.flow.label());
                window.draw(
                    frame,
                    self.window_canvas(&key),
                    focused,
                    self.geometry
                        .as_ref()
                        .filter(|geometry| geometry.key == key)
                        .map(|geometry| geometry.resize),
                    &title,
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
            if focused {
                surface.draw(frame, ui, editor, Some(&composer))?;
            } else if let Some(view) = self.views.get_mut(&key) {
                surface.draw(frame, &mut view.ui, &view.editor, None)?;
            }
        }
        if ui.app.focus_rail.page_open() {
            self.composer_rect = None;
        } else {
            crate::render::render_composer(frame, ui, editor, &composer);
        }
        self.sidebar.lock().unwrap().render(
            frame,
            self.active
                .as_ref()
                .map(|key| key.flow.session_target())
                .as_ref(),
        );
        crate::render::render_windows_and_selection(frame, ui);
        self.cursor_visible = self.geometry.is_none()
            && (!self.sidebar.lock().unwrap().focused
                || ui.wm.focused_id().is_some()
                || ui.wm.modals.any_open())
            && ui.app.modal_notification.is_none()
            && if let Some(kind) = ui.wm.top_kind() {
                ui.wm.modals.cursor_visible(kind)
            } else if ui.wm.focused_id().is_some() {
                ui.wm.focused_content_cursor_visible()
            } else {
                !ui.app.focus_rail.page_open()
                    && (!ui.app.submission_focus || ui.app.queued_submission_edit.is_some())
                    && (ui.app.startup_card.is_none()
                        || ui.app.startup_focus == crate::app::StartupFocus::Input)
            };
        Ok(())
    }

    /// Wait on visible background views, including historical views with reply mailboxes but no live reader. Dropping this future retains each reader's cursor.
    pub async fn next_background(&mut self) {
        let mut pending: Vec<_> = self
            .views
            .iter_mut()
            .filter(|(key, view)| {
                self.main.as_ref() == Some(key)
                    || self.windows.iter().any(|window| &window.key == *key)
                    || view.ui.app.project_sidebar.lock().unwrap().has_pending()
            })
            .map(|(_, view)| {
                Box::pin(async move {
                    if let Some(reader) = view.handle.flow_view.as_mut() {
                        reader.set_pending_offset(view.ui.app.pending_page_offset);
                    }
                    let update = tokio::select! {
                        _ = activity_changed(&mut view.handle.activity) => {
                            refresh_activity(&mut view.ui, &mut view.handle);
                            return;
                        }
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
                        update = async {
                            match view.handle.flow_view.as_mut() {
                                Some(reader) => reader.next().await,
                                None => std::future::pending().await,
                            }
                        }, if view.ui.app.flow_view_error.is_none() => Some(update),
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
        if !shortcuts && self.geometry.is_none() && matches!(event, Event::Mouse(_)) {
            return Action::Pass;
        }
        let active = self.active.clone();
        let canvas = self.canvas;
        let dock_canvas = self.dock_canvas;
        let bounds = |key: &ViewKey| {
            if active.as_ref() == Some(key) {
                dock_canvas
            } else {
                canvas
            }
        };
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
                            dock_canvas,
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
                    window.rect = adjusted(window.rect, dx, dy, geometry.resize, dock_canvas);
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
                    let mut sidebar = self.sidebar.lock().unwrap();
                    let count = keys.len() + usize::from(sidebar.available());
                    let current = if sidebar.focused {
                        keys.len()
                    } else {
                        keys.iter().position(|key| *key == active).unwrap_or(0)
                    };
                    let reverse =
                        key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT);
                    let index = if reverse {
                        (current + count - 1) % count
                    } else {
                        (current + 1) % count
                    };
                    sidebar.focused = index == keys.len();
                    if sidebar.focused {
                        return Action::Consume;
                    }
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
                if self.composer_rect.is_some_and(|rect| rect.contains(point)) {
                    return Action::Pass;
                }
                let top = self.windows.iter().rposition(|window| {
                    window.area(bounds(&window.key)).contains(point)
                        || crate::wm::shell::hit_button(window.area(bounds(&window.key)), point)
                            .is_some()
                        || crate::wm::shell::hit_titlebar(window.area(bounds(&window.key)), point)
                });
                for (index, window) in self.windows.iter_mut().enumerate() {
                    window.hover = if Some(index) == top {
                        crate::wm::shell::hit_button(window.area(bounds(&window.key)), point)
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
                            Some(crate::wm::PanelBtn::Minimize | crate::wm::PanelBtn::Close) => {
                                return Action::Close(key);
                            }
                            Some(crate::wm::PanelBtn::Maximize) => {
                                self.maximize(&key);
                                return Action::Focus {
                                    key,
                                    forward: false,
                                };
                            }
                            Some(crate::wm::PanelBtn::Resize) => {
                                self.begin_geometry(&key, true, Some(point));
                                return Action::Focus {
                                    key,
                                    forward: false,
                                };
                            }
                            _ => {}
                        }
                        let area = window.area(bounds(&window.key));
                        if !compact_window(self.canvas)
                            && crate::wm::shell::hit_titlebar(area, point)
                        {
                            let now = std::time::Instant::now();
                            let last = &mut self.windows[index].last_titlebar_click;
                            if last.is_some_and(|previous| {
                                now.duration_since(previous)
                                    < crate::wm::shell::DOUBLE_CLICK_INTERVAL
                            }) {
                                *last = None;
                                self.maximize(&key);
                            } else {
                                *last = Some(now);
                                self.begin_geometry(&key, false, Some(point));
                            }
                            return Action::Focus {
                                key,
                                forward: false,
                            };
                        }
                    }
                    if !window.content(bounds(&window.key)).contains(point) {
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
        let canvas = self.window_canvas(key);
        if compact_window(canvas) {
            return;
        }
        if let Some(window) = self.windows.iter_mut().find(|window| &window.key == key) {
            let area = window.area(canvas);
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

    #[test]
    fn docking_tracks_focus_without_resetting_or_animating_panel_movement() {
        let a = window().key;
        let b = window().key;
        let now = std::time::Instant::now();
        let left = Rect::new(10, 20, 64, 6);
        let right = Rect::new(90, 40, 64, 6);
        let mut motion = DockMotion::default();
        assert_eq!(motion.place(&a, left, false, now), left);
        assert!(motion.started.is_none());
        assert_eq!(motion.place(&b, right, false, now), left);
        let middle = motion.place(&b, right, false, now + std::time::Duration::from_millis(80));
        assert!(middle.x > left.x && middle.x < right.x);
        assert_eq!(
            motion.place(&a, left, false, now + std::time::Duration::from_millis(80)),
            middle
        );
        assert_eq!(
            motion.place(&a, left, false, now + std::time::Duration::from_millis(300)),
            left
        );
        assert!(motion.started.is_none());
        assert_eq!(
            motion.place(&a, right, true, now + std::time::Duration::from_millis(310)),
            right
        );
        assert!(motion.started.is_none());
        assert_eq!(
            motion.place(
                &a,
                right,
                false,
                now + std::time::Duration::from_millis(500)
            ),
            right
        );
    }

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
            last_titlebar_click: None,
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
            dock_canvas: Rect::default(),
            composer_rect: None,
            motion: DockMotion::default(),
            sidebar: Default::default(),
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
            assert!(content.width >= 34 && content.height >= 8);
            let buttons = [
                crate::wm::PanelBtn::Minimize,
                crate::wm::PanelBtn::Maximize,
                crate::wm::PanelBtn::Close,
                crate::wm::PanelBtn::Resize,
            ]
            .map(|button| crate::wm::shell::button_area(window.area(canvas), button));
            for (index, button) in buttons.iter().enumerate() {
                assert_eq!(button.height, 1);
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
                .draw(|frame| window.draw(frame, frame.area(), true, None, "Review scheduling"))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let lines: Vec<String> = (0..24)
                .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
                .collect();
            for label in ["Review scheduling", "—", "▢", "✕", "⇲"] {
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
            dock_canvas: Rect::new(0, 0, 40, 24),
            composer_rect: None,
            motion: DockMotion::default(),
            sidebar: Default::default(),
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
        let close = crate::wm::shell::button_area(
            workspace.windows[0].area(workspace.canvas),
            crate::wm::PanelBtn::Close,
        );
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
        workspace.dock_canvas = workspace.canvas;
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

        let pointer = |kind, x, y| {
            Event::Mouse(crossterm::event::MouseEvent {
                kind,
                column: x,
                row: y,
                modifiers: KeyModifiers::NONE,
            })
        };
        let down = MouseEventKind::Down(MouseButton::Left);
        let up = MouseEventKind::Up(MouseButton::Left);
        let title = (preferred.x + 8, preferred.y);
        workspace.event(&pointer(down, title.0, title.1), true);
        assert!(workspace.geometry.is_some());
        workspace.event(&pointer(up, title.0, title.1), true);
        workspace.event(&pointer(down, title.0, title.1), true);
        assert_eq!(
            workspace.windows[0].area(workspace.canvas),
            workspace.canvas
        );
        assert!(workspace.geometry.is_none());
        workspace.maximize(&key);

        let grip = crate::wm::shell::button_area(preferred, crate::wm::PanelBtn::Resize);
        workspace.event(&pointer(down, grip.x + 1, grip.y), true);
        assert!(workspace.geometry.as_ref().unwrap().resize);
        workspace.event(
            &pointer(
                MouseEventKind::Drag(MouseButton::Left),
                grip.x + 3,
                grip.y + 1,
            ),
            true,
        );
        workspace.event(&pointer(up, grip.x + 3, grip.y + 1), true);
        assert_eq!(workspace.windows[0].rect.width, preferred.width + 2);
        assert_eq!(workspace.windows[0].rect.height, preferred.height + 1);
        for button in [crate::wm::PanelBtn::Minimize, crate::wm::PanelBtn::Close] {
            let area = crate::wm::shell::button_area(workspace.windows[0].rect, button);
            assert!(
                matches!(workspace.event(&pointer(down, area.x + 1, area.y), true), Action::Close(target) if target == key)
            );
        }
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
                    surface.draw(frame, &mut ui, &editor, None).unwrap();
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

    #[test]
    fn compact_activity_controls_occlude_the_composer_without_losing_its_draft() {
        for width in [65, 95] {
            let mut key = window().key;
            key.placement = FlowPlacement::Main;
            let mut workspace = Workspace {
                active: Some(key.clone()),
                main: Some(key),
                views: HashMap::new(),
                windows: vec![],
                main_history: Default::default(),
                surfaces: HashMap::new(),
                canvas: Rect::default(),
                dock_canvas: Rect::default(),
                composer_rect: None,
                motion: Default::default(),
                sidebar: Default::default(),
                geometry: None,
                background_start: 0,
                cursor_visible: false,
            };
            let mut ui = UiState::new(AppState::new("surface".into(), None));
            let mut editor = InputEditor::default();
            editor.insert_str("Preserved composer draft");
            let mut terminal = Terminal::new(TestBackend::new(width, 26)).unwrap();
            terminal
                .draw(|frame| workspace.render(frame, &mut ui, &editor).unwrap())
                .unwrap();
            ui.app.select_focus_rail(true);
            let key = |code| Event::Key(crossterm::event::KeyEvent::new(code, KeyModifiers::NONE));
            assert!(ui.app.activity_key(&key(KeyCode::Tab)));
            terminal
                .draw(|frame| workspace.render(frame, &mut ui, &editor).unwrap())
                .unwrap();
            let text = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();
            assert!(text.contains("Sort"));
            assert!(!text.contains("Preserved composer draft"));
            assert!(workspace.composer_rect.is_none());
            assert!(ui.app.composer_rect.is_none());
            assert!(!workspace.cursor_visible);
            assert_eq!(editor.buf(), "Preserved composer draft");
            assert!(!ui.app.activity_key(&key(KeyCode::Esc)));
            assert!(ui.app.focus_page_event(&key(KeyCode::Esc)));
            terminal
                .draw(|frame| workspace.render(frame, &mut ui, &editor).unwrap())
                .unwrap();
            assert!(workspace.composer_rect.is_some());
            assert_eq!(editor.buf(), "Preserved composer draft");
        }
    }
}
