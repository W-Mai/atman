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

mod question;
mod question_actions;

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

struct ViewDelivery {
    key: ViewKey,
    ticket: Option<crate::control::ControlTicket>,
    command: TuiCommand,
}

struct PreviewObserver {
    output: atman_runtime::flow_runtime::live::LiveOutputReader,
    title: Option<tokio::sync::watch::Receiver<Option<String>>>,
}

type Observation<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<ViewDelivery>> + Send + 'a>>;

fn refresh_view(ui: &mut UiState, editor: &mut InputEditor, handle: &mut TuiHandle) -> Result<()> {
    if let Some(source) = handle.flow_view.take() {
        let mut source = source.prepare_initial(None, None, false);
        source.set_pending_offset(ui.app.pending_page_offset);
        handle.flow_view = Some(source);
        ui.app.flow_view_error = None;
    }
    refresh_cached_metadata(ui, editor, handle)
}

fn refresh_cached_metadata(
    ui: &mut UiState,
    editor: &mut InputEditor,
    handle: &mut TuiHandle,
) -> Result<()> {
    if let Some(source) = handle.flow_view.as_ref()
        && let Some(binding) = handle
            .control_tx
            .as_ref()
            .and_then(|sender| sender.binding())
        && source.address() != binding.flow
    {
        let error = std::io::Error::other("view source does not match destination");
        disconnect_view(ui, &error);
        return Err(error.into());
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

fn replace_attachment(
    ui: &mut UiState,
    editor: &mut InputEditor,
    handle: &mut TuiHandle,
    mut replacement: Box<TuiHandle>,
) -> Result<()> {
    // An unresolved submission still owns its original reply channel.
    anyhow::ensure!(
        attachment_can_replace(ui, None),
        "Waiting for pending input, preparation, settings, panel operations or recovery; retry opening this session afterward"
    );
    refresh_view(ui, editor, &mut replacement)?;
    ui.app.session = replacement.session.clone();
    ui.app.settings = replacement.settings.clone();
    ui.wm.modals.bind_settings(replacement.settings.clone());
    ui.wm.bind_controls(replacement.control_tx.as_ref());
    ui.app.task_registry = replacement.task_registry.take();
    ui.app.continued_session = replacement.continued_session.clone();
    *handle = *replacement;
    refresh_instance_live(ui, editor, handle);
    Ok(())
}

fn attachment_can_replace(ui: &UiState, accepted: Option<&crate::control::ControlTicket>) -> bool {
    ui.app
        .pending_submission
        .as_ref()
        .is_none_or(|pending| accepted == Some(&pending.ticket))
        && ui.app.queue_recovery_ticket.is_none()
        && !ui.app.approval_decisions.is_waiting()
        && !ui.app.image_paste.is_waiting()
        && !ui.app.clipboard_copies.is_waiting()
        && !ui.wm.modals.retains_settings_operation()
        && ui.app.mcp_pending_mutation.is_none()
        && !ui
            .app
            .knowledge_state
            .lock()
            .unwrap()
            .retains_control_operation()
        && !ui.wm.retains_control_operation()
        && ui
            .suspended_navigation
            .as_ref()
            .is_none_or(|hub| hub.pending.is_none())
        && ui.navigation_reply.is_none()
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
    minimum_size: (u16, u16),
    preferred_size: Option<(u16, u16)>,
    restore: Option<Rect>,
    hover: Option<crate::wm::PanelBtn>,
    last_titlebar_click: Option<std::time::Instant>,
}

impl Window {
    fn place_initial(&mut self, canvas: Rect, available: Rect, ordinal: usize) {
        if self.rect.width == 0 {
            self.rect = self.preferred_size.map_or_else(
                || initial_window_rect(canvas, available, ordinal),
                |preferred| {
                    bounded_with_minimum(
                        crate::wm::initial_panel_rect(available, preferred, ordinal),
                        available,
                        self.minimum_size,
                    )
                },
            );
        }
    }

    fn area(&self, canvas: Rect) -> Rect {
        if compact_window(canvas) || self.restore.is_some() {
            canvas
        } else {
            bounded_with_minimum(self.rect, canvas, self.minimum_size)
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
        icon: &str,
    ) {
        let rect = self.area(canvas);
        let t = crate::theme::theme();
        crate::wm::shell::render_surface(
            frame,
            crate::wm::shell::Shell {
                rect,
                title,
                icon,
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
    target: Option<Rect>,
    position: [f64; 4],
    velocity: [f64; 4],
    updated: Option<std::time::Instant>,
    moving: bool,
}

impl DockMotion {
    fn place(&mut self, target: Rect, immediate: bool, now: std::time::Instant) -> Rect {
        let components = |rect: Rect| [rect.x, rect.y, rect.right(), rect.bottom()].map(f64::from);
        if immediate || self.target.is_none() {
            self.position = components(target);
            self.velocity = [0.0; 4];
            self.moving = false;
        } else if let (Some(previous), Some(updated)) = (self.target, self.updated) {
            let seconds = now.saturating_duration_since(updated).as_secs_f64();
            for (index, destination) in components(previous).into_iter().enumerate() {
                (self.position[index], self.velocity[index]) = crate::render::spring_step(
                    self.position[index],
                    self.velocity[index],
                    destination,
                    seconds,
                );
            }
            self.moving = components(target)
                .into_iter()
                .enumerate()
                .any(|(index, destination)| {
                    (self.position[index] - destination).abs() > 0.05
                        || self.velocity[index].abs() > 0.5
                });
            if !self.moving {
                self.position = components(target);
                self.velocity = [0.0; 4];
            }
        }
        self.target = Some(target);
        self.updated = Some(now);
        // Quantize size once: independent edge rounding can make a monotonic
        // spring alternate between adjacent widths. Keep stationary anchors exact.
        let axis = |start: usize, end: usize| {
            let destination = components(target);
            let at_rest = |position: f64, velocity: f64, target: f64| {
                (position - target).abs() < 1e-6 && velocity.abs() < 1e-6
            };
            let mut size = (self.position[end] - self.position[start]).round();
            let origin = if at_rest(self.position[end], self.velocity[end], destination[end]) {
                destination[end] - size
            } else if at_rest(
                self.position[start] + self.position[end],
                self.velocity[start] + self.velocity[end],
                destination[start] + destination[end],
            ) {
                let center = destination[start] + destination[end];
                let parity = center % 2.0;
                size = ((self.position[end] - self.position[start] - parity) / 2.0).round() * 2.0
                    + parity;
                (center - size) / 2.0
            } else {
                self.position[start].round()
            };
            (origin.max(0.0) as u16, size.max(0.0) as u16)
        };
        let (x, width) = axis(0, 2);
        let (y, height) = axis(1, 3);
        Rect::new(x, y, width, height)
    }
}

pub(super) struct Workspace {
    active: Option<ViewKey>,
    active_binding: Option<ViewBinding>,
    main: Option<ViewKey>,
    pub views: HashMap<ViewKey, AttachedView>,
    windows: Vec<Window>,
    closed_windows: HashMap<ViewKey, Window>,
    main_history: crate::flow_hub::History,
    surfaces: HashMap<ViewKey, Surface>,
    canvas: Rect,
    dock_canvas: Rect,
    composer_rect: Option<Rect>,
    motion: DockMotion,
    sidebar: std::sync::Arc<std::sync::Mutex<crate::project_sidebar::ProjectSidebar>>,
    previews: HashMap<uuid::Uuid, PreviewObserver>,
    focus_history: Vec<ViewKey>,
    geometry: Option<Geometry>,
    background_start: usize,
    questions: question::Handoffs,
    control_work: crate::control::ControlWork,
    pub cursor_visible: bool,
}

impl Workspace {
    pub fn observe_focus(&mut self, ui: &mut UiState, handle: &TuiHandle) {
        if ui.app.conversation_closed {
            ui.wm.close_navigation_page(&mut ui.app);
        }
        ui.app.conversation_closed = false;
        if let Some(key) = self.active.clone() {
            self.focus_history.retain(|previous| previous != &key);
            self.focus_history.push(key);
        }
        self.sidebar = ui.app.project_sidebar.clone();
        if handle
            .quick_ask
            .as_ref()
            .is_some_and(|question| !matches!(question.phase(), crate::quick_ask::Phase::Kept(_)))
        {
            return;
        }
        let mut output = handle.live_output.as_ref().map(|reader| reader.subscribe());
        let snapshot = output.as_mut().map(|reader| reader.preview_snapshot());
        self.sidebar.lock().unwrap().focused(handle, snapshot);
        if let Some(binding) = handle.control_tx.as_ref().and_then(|tx| tx.binding()) {
            if let Some(output) = output {
                self.previews.insert(
                    binding.flow.session_id(),
                    PreviewObserver {
                        output,
                        title: handle.session_name_rx.clone(),
                    },
                );
            } else {
                self.previews.remove(&binding.flow.session_id());
            }
        }
    }

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
            active_binding: handle
                .control_tx
                .as_ref()
                .and_then(|tx| tx.binding())
                .cloned(),
            main: key,
            views: HashMap::new(),
            windows: vec![],
            closed_windows: HashMap::new(),
            main_history: crate::flow_hub::History::default(),
            surfaces: HashMap::new(),
            canvas: Rect::default(),
            dock_canvas: Rect::default(),
            composer_rect: None,
            motion: DockMotion::default(),
            sidebar: Default::default(),
            previews: HashMap::new(),
            focus_history: vec![],
            geometry: None,
            background_start: 0,
            questions: Default::default(),
            control_work: handle
                .control_tx
                .as_ref()
                .and_then(crate::ControlSender::control_work)
                .unwrap_or_default(),
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
        self.motion.moving
            || self
                .main
                .iter()
                .chain(self.windows.iter().map(|window| &window.key))
                .filter_map(|key| self.views.get(key))
                .any(|view| {
                    view.ui.app.tail_scroll_needs_tick() || view.ui.has_geometry_animation()
                })
    }

    pub(super) fn has_active_animation(&self) -> bool {
        self.main
            .iter()
            .chain(self.windows.iter().map(|window| &window.key))
            .filter_map(|key| self.views.get(key))
            .any(|view| view.ui.has_active_animation())
    }

    pub(super) fn tick_animation(&mut self) {
        for key in self
            .main
            .iter()
            .chain(self.windows.iter().map(|window| &window.key))
        {
            if let Some(view) = self.views.get_mut(key) {
                view.ui.app.refresh_preferences();
                view.ui.app.refresh_settings_status();
                refresh_flow_progress(&mut view.ui.app, view.handle.flow_view.as_ref());
                refresh_navigation_progress(
                    &mut view.ui.app,
                    view.ui.suspended_navigation.as_ref(),
                    view.ui.navigation_reply.is_some(),
                );
                view.ui.app.animation_frame = view.ui.app.animation_frame.wrapping_add(1);
                view.ui.app.refresh_clipboard_status(false);
            }
        }
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
                    .or_else(|| self.closed_windows.get_mut(key))
                    .expect("floating directory belongs to a retained view")
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
        mut attachment: Option<Box<TuiHandle>>,
    ) -> Result<()> {
        if let Some(source) = &mut attachment
            && let Some(sender) = &mut source.control_tx
        {
            sender.bind_work(self.control_work.clone());
        }
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
                    )?;
                } else if selected.ui.app.flow_view_error.is_some() {
                    refresh_view(&mut selected.ui, &mut selected.editor, &mut selected.handle)?;
                } else {
                    // The ordinary reader catches up independently after focus changes.
                    refresh_cached_metadata(
                        &mut selected.ui,
                        &mut selected.editor,
                        &mut selected.handle,
                    )?;
                }
            } else {
                let mut handle =
                    attachment.ok_or_else(|| anyhow::anyhow!("view attachment is missing"))?;
                let preferences = ui
                    .app
                    .preferences
                    .as_ref()
                    .map(|binding| binding.store.clone())
                    .unwrap_or_else(crate::states::PreferenceStore::start);
                let (mut next_ui, editor) = attach_view(&mut handle, &preferences)?;
                next_ui.app.image_paste.work = ui.app.image_paste.work.clone();
                next_ui.app.clipboard_copies.service = ui.app.clipboard_copies.service.clone();
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
            replace_attachment(ui, editor, handle, replacement)?;
        } else if ui.app.flow_view_error.is_some() {
            refresh_view(ui, editor, handle)?;
        }
        match key.placement {
            FlowPlacement::Main => self.main = Some(key),
            FlowPlacement::Floating => self.open_window(key),
        }
        if !self.canvas.is_empty() {
            self.constrain_panels(ui, self.canvas, handle.control_tx.as_ref());
        }
        self.active_binding = handle
            .control_tx
            .as_ref()
            .and_then(|tx| tx.binding())
            .cloned();
        self.observe_focus(ui, handle);
        Ok(())
    }

    pub async fn focus_with_navigation(
        &mut self,
        navigation: &mut Option<crate::flow_hub::FlowHub>,
        ui: &mut UiState,
        editor: &mut InputEditor,
        handle: &mut TuiHandle,
        key: ViewKey,
        attachment: Option<Box<TuiHandle>>,
    ) -> Result<()> {
        ui.suspended_navigation = navigation.take();
        let result = self.focus(ui, editor, handle, key, attachment).await;
        *navigation = ui.suspended_navigation.take();
        result
    }

    pub fn close_with_navigation(
        &mut self,
        navigation: &mut Option<crate::flow_hub::FlowHub>,
        ui: &mut UiState,
        editor: &mut InputEditor,
        handle: &mut TuiHandle,
        key: &ViewKey,
    ) -> Result<()> {
        ui.suspended_navigation = navigation.take();
        let result = self.close(ui, editor, handle, key);
        *navigation = ui.suspended_navigation.take();
        result
    }

    pub async fn complete_navigation(
        &mut self,
        navigation: &mut Option<crate::flow_hub::FlowHub>,
        ui: &mut UiState,
        editor: &mut InputEditor,
        handle: &mut TuiHandle,
        key: ViewKey,
        attachment: Option<Box<TuiHandle>>,
    ) -> Result<()> {
        let owner = self.directory_owner();
        let completed = navigation.take();
        refresh_navigation_progress(&mut ui.app, None, false);
        match self
            .focus_with_navigation(navigation, ui, editor, handle, key, attachment)
            .await
        {
            Ok(()) => {
                if let Some(completed) = completed {
                    self.directory_history_for(owner.as_ref()).save(completed);
                }
                Ok(())
            }
            Err(error) => {
                *navigation = completed;
                Err(error)
            }
        }
    }

    pub fn constrain_panels(
        &self,
        ui: &mut UiState,
        canvas: Rect,
        control: Option<&crate::ControlSender>,
    ) {
        let terminal = ui
            .wm
            .focused_id()
            .and_then(|id| ui.wm.panels.iter().find(|panel| panel.id == id))
            .and_then(|panel| match &panel.content_kind {
                crate::wm::WindowContent::Task {
                    handle,
                    kind: atman_runtime::TaskKind::Terminal,
                } => Some((panel.id, panel.rect, handle.clone())),
                _ => None,
            });
        let canvas = self
            .windows
            .iter()
            .find(|window| Some(&window.key) == self.active.as_ref())
            .map(|window| {
                let dock = Rect {
                    height: ui.app.maximized_canvas_for(canvas).height,
                    ..canvas
                };
                window.content(dock)
            })
            .unwrap_or(canvas);
        ui.wm.clamp_to_canvas(ui.app.maximized_canvas_for(canvas));
        if let Some((id, previous, handle)) = terminal
            && let Some(panel) = ui.wm.panels.iter().find(|panel| panel.id == id)
            && panel.rect != previous
        {
            let cols = panel.rect.width.saturating_sub(8);
            let rows = panel.rect.height.saturating_sub(5);
            if cols > 0
                && rows > 0
                && let Some(control) = control
            {
                let _ = control.send(TuiControl::TermResize { handle, rows, cols });
            }
        }
    }

    #[cfg(test)]
    pub async fn focus_ready(
        &mut self,
        ui: &mut UiState,
        editor: &mut InputEditor,
        handle: &mut TuiHandle,
        key: ViewKey,
        attachment: Option<Box<TuiHandle>>,
    ) -> Result<()> {
        self.focus(ui, editor, handle, key, attachment).await?;
        finish_test_attachment(ui, editor, handle).await
    }

    fn open_window(&mut self, key: ViewKey) {
        let question = matches!(&key.flow, ViewAddress::QuestionDraft { .. });
        let window = self
            .windows
            .iter()
            .position(|window| window.key == key)
            .map(|index| self.windows.remove(index))
            .or_else(|| self.closed_windows.remove(&key))
            .unwrap_or_else(|| Window {
                key,
                history: crate::flow_hub::History::default(),
                rect: Rect::default(),
                minimum_size: if question { (64, 18) } else { (64, 20) },
                preferred_size: question.then_some((72, 18)),
                restore: None,
                hover: None,
                last_titlebar_click: None,
            });
        self.windows.push(window);
    }

    fn remember_window(&mut self, key: &ViewKey) {
        if let Some(index) = self.windows.iter().position(|window| &window.key == key) {
            let mut window = self.windows.remove(index);
            window.hover = None;
            window.last_titlebar_click = None;
            self.closed_windows.insert(key.clone(), window);
        }
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
        ui.app.question_pointer = None;
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
        ui.app.question_pointer = None;
        self.active_binding = handle
            .control_tx
            .as_ref()
            .and_then(|tx| tx.binding())
            .cloned();
        self.observe_focus(ui, handle);
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
            let fallback = self.main.clone().or_else(|| {
                self.windows
                    .iter()
                    .rev()
                    .find(|window| &window.key != key)
                    .map(|window| window.key.clone())
            });
            if let Some(fallback) = fallback {
                self.select_cached(ui, editor, handle, &fallback)?;
            } else {
                self.show_navigation(ui, handle);
            }
        }
        self.remember_window(key);
        self.geometry = None;
        Ok(())
    }

    fn navigation_only(&self) -> bool {
        self.main.is_none() && self.windows.is_empty() && self.active.is_some()
    }

    fn show_navigation(&mut self, ui: &mut UiState, handle: &TuiHandle) {
        ui.app.conversation_closed = true;
        ui.app.flow_loading = None;
        ui.app.flow_refreshing = false;
        ui.app.status_notes.remove("view-read");
        ui.app.selection = None;
        ui.app.transcript_selection_pointer = None;
        ui.selection_menu = None;
        ui.app.popup.close();
        crate::render::ComposerLayout::hide(&mut ui.app);
        self.composer_rect = None;
        self.motion = DockMotion::default();
        self.cursor_visible = false;
        let canvas = if self.canvas.is_empty() {
            ui.app.maximized_canvas()
        } else {
            self.canvas
        };
        ui.wm.open_project_hub(
            canvas,
            self.sidebar.clone(),
            None,
            handle.control_tx.as_ref(),
        );
    }

    pub async fn close_session(
        &mut self,
        navigation: &mut Option<crate::flow_hub::FlowHub>,
        ui: &mut UiState,
        editor: &mut InputEditor,
        handle: &mut TuiHandle,
        target: &crate::project_hub::SessionTarget,
    ) -> Result<()> {
        let Some(session_id) = self
            .sidebar
            .lock()
            .unwrap()
            .open_flow(target)
            .map(ViewAddress::session_id)
        else {
            return Ok(());
        };
        let belongs = |key: &ViewKey| key.flow.session_target() == *target;
        let active_closes = self.active.as_ref().is_some_and(belongs);
        let fallback = self
            .focus_history
            .iter()
            .rev()
            .find(|key| {
                !belongs(key)
                    && self
                        .sidebar
                        .lock()
                        .unwrap()
                        .open_flow(&key.flow.session_target())
                        .is_some()
            })
            .cloned();
        if active_closes && let Some(key) = fallback {
            self.focus_with_navigation(navigation, ui, editor, handle, key, None)
                .await?;
        }
        let closing: Vec<_> = self
            .windows
            .iter()
            .filter(|window| belongs(&window.key))
            .map(|window| window.key.clone())
            .collect();
        for key in closing {
            self.remember_window(&key);
        }
        if self.main.as_ref().is_some_and(belongs) {
            self.main = None;
        }
        self.focus_history.retain(|key| !belongs(key));
        self.previews.remove(&session_id);
        self.sidebar.lock().unwrap().close(target);
        self.geometry = None;
        if self.navigation_only() {
            self.show_navigation(ui, handle);
        }
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
        self.remember_window(&old);
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
        if self.navigation_only() {
            self.cursor_visible = false;
            self.composer_rect = None;
            crate::render::ComposerLayout::hide(&mut ui.app);
            frame.render_widget(ratatui::widgets::Clear, self.canvas);
            frame.render_widget(
                Paragraph::new("Projects · Ctrl+L")
                    .style(Style::default().fg(crate::theme::theme().project_meta_fg.into())),
                Rect::new(
                    self.canvas.x + 2,
                    self.canvas.y + 2,
                    self.canvas.width.saturating_sub(4),
                    1,
                ),
            );
            self.sidebar
                .lock()
                .unwrap()
                .render(frame, None, ui.app.animation_frame);
            crate::render::render_window_layers(frame, ui, |_, _| {});
            self.cursor_visible = ui.app.modal_notification.is_none()
                && if let Some(kind) = ui.wm.top_kind() {
                    ui.wm.modals.cursor_visible(kind)
                } else {
                    ui.wm.focused_content_cursor_visible()
                };
            return Ok(());
        }
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
        } else if let Some((index, window)) = self
            .windows
            .iter_mut()
            .enumerate()
            .find(|(_, w)| Some(&w.key) == self.active.as_ref())
        {
            window.place_initial(self.canvas, self.dock_canvas, index);
            let rect = window.area(self.dock_canvas);
            let x = (i32::from(rect.x)
                + (i32::from(rect.width) - i32::from(composer.outer.width)) / 2)
                .clamp(
                    i32::from(self.canvas.x),
                    i32::from(self.canvas.right().saturating_sub(composer.outer.width)),
                ) as u16;
            composer.move_to(x, rect.bottom() + 1);
        }
        let visual = self.motion.place(
            composer.input_rect,
            self.geometry.is_some() || ui.app.startup_intro.is_some(),
            std::time::Instant::now(),
        );
        let width = visual.width.min(self.canvas.width).max(1);
        let height = visual.height.min(self.canvas.height).max(1);
        let visual = Rect::new(
            visual
                .x
                .clamp(self.canvas.x, self.canvas.right().saturating_sub(width)),
            visual
                .y
                .clamp(self.canvas.y, self.canvas.bottom().saturating_sub(height)),
            width,
            height,
        );
        if visual != composer.input_rect {
            composer = composer.project_visual(
                &ui.app,
                editor,
                if main_focus { main_area } else { self.canvas },
                visual,
            );
        }
        self.composer_rect = Some(composer.outer);
        let mut layers: Vec<_> = self
            .main
            .clone()
            .map(|key| (key, self.canvas))
            .into_iter()
            .collect();
        if self.main.is_none() {
            frame.render_widget(ratatui::widgets::Clear, self.canvas);
        }
        for (index, window) in self.windows.iter_mut().enumerate() {
            window.place_initial(self.canvas, self.dock_canvas, index);
            let canvas = if Some(&window.key) == self.active.as_ref() {
                self.dock_canvas
            } else {
                self.canvas
            };
            layers.push((window.key.clone(), window.content(canvas)));
        }
        for (key, mut area) in layers {
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
                    if app.is_some_and(|app| app.question.is_some()) {
                        "?"
                    } else {
                        "⬡"
                    },
                );
            }
            if focused {
                if let Some(binding) = &self.active_binding {
                    area = question_actions::render_chrome(frame, area, ui, binding);
                }
            } else if let Some(view) = self.views.get_mut(&key)
                && let Some(binding) = view
                    .handle
                    .control_tx
                    .as_ref()
                    .and_then(|sender| sender.binding())
            {
                area = question_actions::render_chrome(frame, area, &mut view.ui, binding);
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
        if !ui.composer_visible() {
            self.composer_rect = None;
        }
        self.sidebar.lock().unwrap().render(
            frame,
            self.active
                .as_ref()
                .map(|key| key.flow.session_target())
                .as_ref(),
            ui.app.animation_frame,
        );
        crate::render::render_window_layers(frame, ui, |frame, ui| {
            crate::render::render_composer(frame, ui, editor, &composer);
        });
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
                ui.composer_visible()
                    && (!ui.app.submission_focus || ui.app.queued_submission_edit.is_some())
                    && (ui.app.startup_card.is_none()
                        || ui.app.startup_focus == crate::app::StartupFocus::Input)
            };
        Ok(())
    }

    /// Observe every retained reply mailbox; only eligible views project source updates.
    pub async fn next_background(&mut self) {
        let delivery = {
            let mut pending: Vec<Observation<'_>> = self
            .views
            .iter_mut()
            .map(|(key, view)| {
                let observe_source = self.main.as_ref() == Some(key)
                    || self.windows.iter().any(|window| &window.key == key)
                    || view.ui.app.flow_loading.is_some()
                    || view.handle.flow_view.as_ref().is_some_and(|source| source.read_started_at().is_some())
                    || view.ui.app.popup.is_waiting()
                    || view.ui.app.image_paste.is_waiting()
                    || view.ui.app.clipboard_copies.is_waiting()
                    || view.ui.wm.modals.session_switcher.has_legacy_load()
                    || view.ui.wm.modals.form_modal.is_loading_preference()
                    || view.ui.app.settings.is_waiting()
                    || view.ui.wm.modals.retains_settings_operation()
                    || view.ui.app.preferences.as_ref().is_some_and(|binding| binding.needs_initial_load())
                    || view.ui.app.project_sidebar.lock().unwrap().has_pending();
                let key = key.clone();
                Box::pin(async move {
                    if !observe_source {
                        return recv_cmd(
                            &mut view.handle.cmd_rx,
                            &mut view.handle.control_reply_rx,
                            view.handle.control_tx.as_ref().and_then(|tx| tx.binding()),
                        ).await.map(|(ticket, command)| ViewDelivery { key, ticket, command });
                    }
                    if let Some(reader) = view.handle.flow_view.as_mut() {
                        reader.set_pending_offset(view.ui.app.pending_page_offset);
                    }
                    let mut read_progress = view.handle.flow_view.as_ref().and_then(FlowViewSource::subscribe_read_progress);
                    let update = tokio::select! {
                        _ = crate::instance_view::read_progress_changed(&mut read_progress) => {
                            refresh_flow_progress(&mut view.ui.app, view.handle.flow_view.as_ref());
                            return None;
                        }
                        _ = view.ui.wm.modals.form_modal.next_preference_update() => {
                            view.ui.app.mark_visual_dirty();
                            return None;
                        }
                        _ = view.ui.wm.modals.session_switcher.next_legacy_update() => {
                            view.ui.app.mark_visual_dirty();
                            return None;
                        }
                        (request, result) = view.ui.app.image_paste.next() => {
                            view.ui.app.clipboard_image_ready(&mut view.editor, request, result);
                            return None;
                        }
                        result = view.ui.app.clipboard_copies.next() => {
                            view.ui.app.clipboard_copy_ready(result);
                            return None;
                        }
                        _ = crate::states::preference_change(&mut view.ui.app.preferences) => { view.ui.app.refresh_preferences(); return None; }
                        _ = view.ui.app.popup.next_update() => { view.ui.app.mark_visual_dirty(); return None; }
                        _ = activity_changed(&mut view.handle.activity) => {
                            refresh_activity(&mut view.ui, &mut view.handle);
                            return None;
                        }
                        _ = changed(view.handle.session_name_rx.as_mut()) => {
                            refresh_session_name(&mut view.ui, &mut view.handle);
                            return None;
                        }
                        _ = view.handle.settings.changed() => {
                            apply_provider_catalog_changed(&mut view.ui, None);
                            return None;
                        }
                        _ = crate::settings::optional_changed(&mut view.ui.app.presentation_settings) => {
                            view.ui.app.mark_visual_dirty();
                            return None;
                        }
                        _ = live_output_change(&mut view.handle.live_output) => None,
                        command = recv_cmd(&mut view.handle.cmd_rx, &mut view.handle.control_reply_rx, view.handle.control_tx.as_ref().and_then(|tx| tx.binding())) => {
                            return command.map(|(ticket, command)| ViewDelivery { key, ticket, command });
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
                        return None;
                    };
                    let result = update.and_then(|update| view.ui.app.sync_flow_view(update));
                    update_flow_view(&mut view.ui, &mut view.handle, result);
                    if let Some(history) = view.ui.app.prepared_input_history.take() { view.editor.seed_history(history); }
                    None
                }) as Observation<'_>
            })
            .collect();
            for observer in self.previews.values_mut() {
                let sidebar = self.sidebar.clone();
                pending.push(Box::pin(async move {
                    tokio::select! {
                        _ = observer.output.changed() => {},
                        _ = changed(observer.title.as_mut()) => {},
                    }
                    let title = observer
                        .title
                        .as_mut()
                        .map(|title| title.borrow_and_update().clone());
                    sidebar
                        .lock()
                        .unwrap()
                        .observe(observer.output.preview_snapshot(), title);
                    None
                }));
            }
            self.questions.observe(&mut pending);
            if pending.is_empty() {
                std::future::pending().await
            } else {
                let count = pending.len();
                let start = self.background_start % count;
                pending.rotate_left(start);
                let (delivery, selected, remaining) = futures::future::select_all(pending).await;
                drop(remaining);
                self.background_start = (start + selected + 1) % count;
                delivery
            }
        };
        if let Some(delivery) = delivery {
            self.apply_background_delivery(delivery);
        }
    }

    fn apply_background_delivery(&mut self, delivery: ViewDelivery) {
        let Some(mut view) = self.views.remove(&delivery.key) else {
            return;
        };
        let mut navigation = view.ui.suspended_navigation.take();
        let owner = (delivery.key.placement == FlowPlacement::Floating).then_some(&delivery.key);
        let action = match delivery.command {
            TuiCommand::QuestionOpened {
                request_id,
                attachment,
            } => {
                if let Err(error) = self.question_opened(
                    &mut view.ui,
                    &mut view.editor,
                    &mut view.handle,
                    delivery.ticket.as_ref(),
                    request_id,
                    attachment,
                    false,
                ) {
                    view.ui.app.question_open = None;
                    view.ui
                        .app
                        .push_note(error.to_string(), crate::app::NoteLevel::Error);
                }
                None
            }
            TuiCommand::QuestionHandoff {
                request_id,
                kind,
                attachment,
            } => {
                self.accept_question_handoff(
                    &mut view.ui,
                    &view.handle,
                    delivery.ticket.as_ref(),
                    request_id,
                    kind,
                    attachment,
                );
                None
            }
            TuiCommand::FlowCatalog { result } => {
                match receive_catalog_reply(
                    &mut navigation,
                    self.directory_history_for(owner),
                    delivery.ticket.as_ref(),
                    result,
                    &mut view.ui.app,
                    view.handle.control_tx.as_ref(),
                ) {
                    CatalogReply::Focus(flow) => Some(crate::flow_hub::Action::Focus(flow)),
                    CatalogReply::SwitchSessions => {
                        view.ui
                            .wm
                            .modals
                            .session_switcher
                            .open_for(&view.ui.app, view.handle.control_tx.as_ref());
                        None
                    }
                    CatalogReply::Failed(error) => {
                        if let Some(hub) = navigation.as_mut() {
                            hub.failed(error);
                        }
                        None
                    }
                    CatalogReply::Applied => None,
                }
            }
            TuiCommand::FlowResolved { flow }
                if navigation
                    .as_ref()
                    .is_some_and(|hub| hub.owns_reply(delivery.ticket.as_ref())) =>
            {
                navigation.as_ref().map(|hub| hub.resolved(flow))
            }
            command @ (TuiCommand::FocusFlow { .. }
            | TuiCommand::SessionCreated { .. }
            | TuiCommand::NewSessionLocations(_))
                if navigation
                    .as_ref()
                    .is_some_and(|hub| hub.owns_reply(delivery.ticket.as_ref())) =>
            {
                defer_navigation_reply(&mut view.ui, delivery.ticket, command);
                None
            }
            command => {
                if let TuiCommand::Toast { message, .. } = &command
                    && let Some(hub) = navigation
                        .as_mut()
                        .filter(|hub| hub.owns_reply(delivery.ticket.as_ref()))
                {
                    hub.failed(message.clone());
                }
                apply_view_command(
                    &mut view.ui,
                    &mut view.editor,
                    delivery.ticket.as_ref(),
                    command,
                    view.handle.control_tx.as_ref(),
                );
                None
            }
        };
        if let Some(action) = action {
            let reuse = hub_view_key(&action).and_then(|key| {
                if key == delivery.key {
                    view.handle
                        .control_tx
                        .as_ref()
                        .and_then(|tx| tx.binding())
                        .cloned()
                } else if self.active.as_ref() == Some(&key) {
                    self.active_binding.clone()
                } else {
                    self.views
                        .get(&key)
                        .and_then(|view| view.handle.control_tx.as_ref())
                        .and_then(|tx| tx.binding())
                        .cloned()
                }
            });
            if let Some(hub) = navigation.as_mut()
                && let Some(control) = view.handle.control_tx.as_ref()
            {
                dispatch_hub_action(hub, action, control, reuse);
            }
        }
        view.ui.suspended_navigation = navigation;
        refresh_navigation_progress(
            &mut view.ui.app,
            view.ui.suspended_navigation.as_ref(),
            view.ui.navigation_reply.is_some(),
        );
        self.views.insert(delivery.key, view);
    }

    pub fn event(&mut self, event: &Event, shortcuts: bool) -> Action {
        if !shortcuts && self.geometry.is_none() && matches!(event, Event::Mouse(_)) {
            return Action::Pass;
        }
        if self.navigation_only() {
            return if matches!(event, Event::Mouse(mouse) if self.canvas.contains((mouse.column, mouse.row).into()))
            {
                Action::Consume
            } else {
                Action::Pass
            };
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
                            window.minimum_size,
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
                    window.rect = adjusted(
                        window.rect,
                        dx,
                        dy,
                        geometry.resize,
                        dock_canvas,
                        window.minimum_size,
                    );
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

fn initial_window_rect(canvas: Rect, available: Rect, ordinal: usize) -> Rect {
    let width = (canvas.width * 4 / 5).max(64).min(canvas.width);
    let height = (canvas.height * 4 / 5).max(20).min(canvas.height);
    bounded(
        crate::wm::initial_panel_rect(available, (width, height), ordinal),
        available,
    )
}

fn bounded(rect: Rect, canvas: Rect) -> Rect {
    bounded_with_minimum(rect, canvas, (64, 20))
}

fn bounded_with_minimum(rect: Rect, canvas: Rect, minimum: (u16, u16)) -> Rect {
    let width = rect.width.max(minimum.0).min(canvas.width);
    let height = rect.height.max(minimum.1).min(canvas.height);
    Rect::new(
        rect.x.clamp(canvas.x, canvas.right().saturating_sub(width)),
        rect.y
            .clamp(canvas.y, canvas.bottom().saturating_sub(height)),
        width,
        height,
    )
}

fn adjusted(rect: Rect, dx: i32, dy: i32, resize: bool, canvas: Rect, minimum: (u16, u16)) -> Rect {
    let adjust =
        |value: u16, delta: i32| (i32::from(value) + delta).clamp(0, i32::from(u16::MAX)) as u16;
    bounded_with_minimum(
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
        minimum,
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
    async fn session_close_preserves_view_owners_drafts_and_empty_navigation_can_reopen() {
        let directory = tempfile::tempdir().unwrap();
        let (commands, _incoming) = mpsc::unbounded_channel();
        let mut handle = history_handle(directory.path(), "First", &commands);
        let (mut ui, mut editor) = initialize_view(&mut handle).await.unwrap();
        let mut workspace = Workspace::new(&handle);
        workspace.observe_focus(&mut ui, &handle);
        let first = workspace.active.clone().unwrap();
        editor.insert_str("First retained draft");
        let second_handle = history_handle(directory.path(), "Second", &commands);
        let second = ViewKey {
            flow: second_handle
                .control_tx
                .as_ref()
                .unwrap()
                .binding()
                .unwrap()
                .flow
                .clone(),
            placement: FlowPlacement::Floating,
        };
        workspace
            .focus_ready(
                &mut ui,
                &mut editor,
                &mut handle,
                second.clone(),
                Some(Box::new(second_handle)),
            )
            .await
            .unwrap();
        editor.insert_str("Second retained draft");
        let binding = handle
            .control_tx
            .as_ref()
            .unwrap()
            .binding()
            .unwrap()
            .clone();
        let mut navigation = None;
        workspace
            .close_session(
                &mut navigation,
                &mut ui,
                &mut editor,
                &mut handle,
                &first.flow.session_target(),
            )
            .await
            .unwrap();
        assert!(workspace.main.is_none());
        assert_eq!(workspace.active.as_ref(), Some(&second));
        assert!(!workspace.navigation_only());
        let mut floating_only = ratatui::Terminal::new(TestBackend::new(140, 45)).unwrap();
        floating_only
            .draw(|frame| workspace.render(frame, &mut ui, &editor).unwrap())
            .unwrap();
        workspace
            .focus_ready(&mut ui, &mut editor, &mut handle, first.clone(), None)
            .await
            .unwrap();
        workspace
            .focus_ready(&mut ui, &mut editor, &mut handle, second.clone(), None)
            .await
            .unwrap();
        workspace
            .close_session(
                &mut navigation,
                &mut ui,
                &mut editor,
                &mut handle,
                &second.flow.session_target(),
            )
            .await
            .unwrap();
        assert_eq!(workspace.active.as_ref(), Some(&first));
        assert_eq!(editor.buf(), "First retained draft");
        assert_eq!(
            workspace.views[&second].editor.buf(),
            "Second retained draft"
        );
        assert_eq!(
            workspace.views[&second]
                .handle
                .control_tx
                .as_ref()
                .unwrap()
                .binding(),
            Some(&binding)
        );
        assert!(workspace.windows.is_empty());
        assert!(workspace.closed_windows.contains_key(&second));
        workspace
            .close_session(
                &mut navigation,
                &mut ui,
                &mut editor,
                &mut handle,
                &first.flow.session_target(),
            )
            .await
            .unwrap();
        assert!(workspace.navigation_only());
        assert!(!ui.composer_visible());
        assert!(ui.wm.has_navigation_page());
        assert!(
            ui.app
                .project_sidebar
                .lock()
                .unwrap()
                .open_flow(&first.flow.session_target())
                .is_none()
        );
        let mut terminal = ratatui::Terminal::new(TestBackend::new(140, 45)).unwrap();
        terminal
            .draw(|frame| workspace.render(frame, &mut ui, &editor).unwrap())
            .unwrap();
        assert!(ui.app.input_rect.is_none());
        workspace
            .focus_ready(&mut ui, &mut editor, &mut handle, first.clone(), None)
            .await
            .unwrap();
        assert!(!workspace.navigation_only());
        assert!(ui.composer_visible());
        assert!(!ui.wm.has_navigation_page());
        assert_eq!(editor.buf(), "First retained draft");
        workspace
            .close_session(
                &mut navigation,
                &mut ui,
                &mut editor,
                &mut handle,
                &first.flow.session_target(),
            )
            .await
            .unwrap();
        assert!(workspace.navigation_only());
        workspace
            .focus_ready(&mut ui, &mut editor, &mut handle, second.clone(), None)
            .await
            .unwrap();
        assert!(!workspace.navigation_only());
        assert_eq!(editor.buf(), "Second retained draft");
        assert_eq!(workspace.windows.len(), 1);
        workspace
            .close(&mut ui, &mut editor, &mut handle, &second)
            .unwrap();
        assert!(workspace.navigation_only());
        assert!(
            ui.app
                .project_sidebar
                .lock()
                .unwrap()
                .open_flow(&second.flow.session_target())
                .is_some(),
            "closing one floating window retains Session membership"
        );
    }

    #[tokio::test]
    async fn hidden_resolution_reuses_the_active_destination_binding_through_its_original_sender() {
        let directory = tempfile::tempdir().unwrap();
        let (commands, mut incoming) = mpsc::unbounded_channel();
        let mut handle = history_handle(directory.path(), "Owner", &commands);
        let (mut ui, mut editor) = initialize_view(&mut handle).await.unwrap();
        let mut workspace = Workspace::new(&handle);
        let owner = handle
            .control_tx
            .as_ref()
            .unwrap()
            .binding()
            .unwrap()
            .clone();
        let destination = history_handle(directory.path(), "Destination", &commands);
        let binding = destination
            .control_tx
            .as_ref()
            .unwrap()
            .binding()
            .unwrap()
            .clone();
        let key = ViewKey {
            flow: binding.flow.clone(),
            placement: FlowPlacement::Main,
        };
        let mut hub = crate::flow_hub::FlowHub::default();
        dispatch_hub_action(
            &mut hub,
            crate::flow_hub::Action::OpenSession {
                session: binding.flow.session_id(),
                placement: crate::control::FlowPlacement::Main,
            },
            handle.control_tx.as_ref().unwrap(),
            None,
        );
        let request = incoming.try_recv().unwrap();
        let mut navigation = Some(hub);
        workspace
            .focus_with_navigation(
                &mut navigation,
                &mut ui,
                &mut editor,
                &mut handle,
                key.clone(),
                Some(Box::new(destination)),
            )
            .await
            .unwrap();
        finish_test_attachment(&mut ui, &mut editor, &mut handle)
            .await
            .unwrap();
        request
            .replies
            .send(TuiCommand::FlowResolved {
                flow: binding.flow.clone(),
            })
            .unwrap();
        let request = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                tokio::select! {
                    request = incoming.recv() => break request.unwrap(),
                    _ = workspace.next_background() => {},
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(request.ticket.binding, owner);
        assert!(
            matches!(request.command, TuiControl::FocusFlow { flow, reuse: Some(reuse), .. } if flow == binding.flow && reuse == binding)
        );
        assert_eq!(workspace.active.as_ref(), Some(&key));
        assert!(navigation.is_none());
    }

    #[tokio::test]
    async fn pending_navigation_keeps_its_owner_and_prepares_hidden_destination_without_focus() {
        let directory = tempfile::tempdir().unwrap();
        let (commands, mut incoming) = mpsc::unbounded_channel();
        let mut handle = history_handle(directory.path(), "Owner A", &commands);
        let (mut ui, mut editor) = initialize_view(&mut handle).await.unwrap();
        let mut workspace = Workspace::new(&handle);
        let a = workspace.active.clone().unwrap();
        let other = history_handle(directory.path(), "Window B", &commands);
        let b = ViewKey {
            flow: other
                .control_tx
                .as_ref()
                .unwrap()
                .binding()
                .unwrap()
                .flow
                .clone(),
            placement: FlowPlacement::Main,
        };
        workspace
            .focus_ready(
                &mut ui,
                &mut editor,
                &mut handle,
                b.clone(),
                Some(Box::new(other)),
            )
            .await
            .unwrap();
        editor.insert_str("B retained draft");
        workspace
            .focus_ready(&mut ui, &mut editor, &mut handle, a.clone(), None)
            .await
            .unwrap();
        editor.insert_str("A retained draft");
        let mut destination = history_handle(directory.path(), "Destination C", &commands);
        let c = ViewKey {
            flow: destination
                .control_tx
                .as_ref()
                .unwrap()
                .binding()
                .unwrap()
                .flow
                .clone(),
            placement: FlowPlacement::Main,
        };
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        destination.flow_view = Some(destination.flow_view.take().unwrap().prepare_with_limit(
            None,
            None,
            true,
            gate.clone(),
        ));
        let mut hub = crate::flow_hub::FlowHub::default();
        dispatch_hub_action(
            &mut hub,
            crate::flow_hub::Action::OpenSession {
                session: c.flow.session_id(),
                placement: crate::control::FlowPlacement::Main,
            },
            handle.control_tx.as_ref().unwrap(),
            None,
        );
        let open = incoming.try_recv().unwrap();
        let original_binding = open.ticket.binding.clone();
        let started = hub.opening_started_at();
        let mut navigation = Some(hub);
        workspace
            .focus_with_navigation(
                &mut navigation,
                &mut ui,
                &mut editor,
                &mut handle,
                b.clone(),
                None,
            )
            .await
            .unwrap();
        assert!(navigation.is_none());
        assert_eq!(editor.buf(), "B retained draft");
        assert_eq!(
            workspace.views[&a]
                .ui
                .suspended_navigation
                .as_ref()
                .unwrap()
                .pending
                .as_ref(),
            Some(&open.ticket)
        );
        open.replies
            .send(TuiCommand::FlowResolved {
                flow: c.flow.clone(),
            })
            .unwrap();
        let focus = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                tokio::select! {
                    request = incoming.recv() => break request.unwrap(),
                    _ = workspace.next_background() => {},
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(focus.ticket.binding, original_binding);
        assert!(
            matches!(&focus.command, TuiControl::FocusFlow { flow, reuse: None, .. } if *flow == c.flow)
        );
        assert_eq!(
            workspace.views[&a]
                .ui
                .suspended_navigation
                .as_ref()
                .unwrap()
                .opening_started_at(),
            started
        );
        focus
            .replies
            .send(TuiCommand::FocusFlow {
                flow: c.flow.clone(),
                placement: FlowPlacement::Main,
                attachment: Some(Box::new(destination)),
            })
            .unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while workspace.views[&a].ui.navigation_reply.is_none() {
                workspace.next_background().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(workspace.active.as_ref(), Some(&b));
        assert_eq!(editor.buf(), "B retained draft");
        assert!(
            !workspace.views[&a]
                .ui
                .navigation_animates(workspace.views[&a].ui.suspended_navigation.as_ref())
        );
        gate.add_permits(1);
        workspace
            .focus_with_navigation(
                &mut navigation,
                &mut ui,
                &mut editor,
                &mut handle,
                a.clone(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(editor.buf(), "A retained draft");
        ui.wm.modals.palette.open_for(&ui.app);
        assert!(!ui.navigation_can_open(navigation.as_ref()));
        assert!(
            tokio::time::timeout(
                Duration::from_millis(20),
                recv_navigation_command(
                    &mut ui.navigation_reply,
                    &mut handle.cmd_rx,
                    &mut handle.control_reply_rx,
                    handle.control_tx.as_ref().and_then(|tx| tx.binding()),
                    false,
                )
            )
            .await
            .is_err()
        );
        assert!(ui.navigation_reply.is_some());
        ui.wm.modals.palette.close();
        assert!(ui.navigation_can_open(navigation.as_ref()));
        let (ticket, command) = recv_navigation_command(
            &mut ui.navigation_reply,
            &mut handle.cmd_rx,
            &mut handle.control_reply_rx,
            handle.control_tx.as_ref().and_then(|tx| tx.binding()),
            true,
        )
        .await
        .unwrap();
        assert_eq!(ticket.as_ref(), Some(&focus.ticket));
        let TuiCommand::FocusFlow {
            flow,
            placement,
            attachment,
        } = command
        else {
            panic!("prepared destination");
        };
        ui.app.navigation_loading = Some(crate::loading::ViewLoading {
            phase: crate::loading::ViewPhase::Waiting,
            started_at: std::time::Instant::now(),
        });
        workspace
            .complete_navigation(
                &mut navigation,
                &mut ui,
                &mut editor,
                &mut handle,
                ViewKey { flow, placement },
                attachment,
            )
            .await
            .unwrap();
        finish_test_attachment(&mut ui, &mut editor, &mut handle)
            .await
            .unwrap();
        assert_eq!(workspace.active.as_ref(), Some(&c));
        assert!(navigation.is_none());
        assert!(workspace.views[&a].ui.suspended_navigation.is_none());
        assert!(workspace.views[&a].ui.navigation_reply.is_none());
        assert!(workspace.views[&a].ui.app.navigation_loading.is_none());
        assert_eq!(workspace.views[&a].editor.buf(), "A retained draft");
        assert_eq!(workspace.views[&b].editor.buf(), "B retained draft");
    }

    #[tokio::test]
    async fn attachment_replacement_preserves_original_receipts_and_rebinds_panel_controls() {
        use atman_runtime::{event_log::legacy::LegacyFlowKey, projection::flow::LegacyHistory};
        let directory = tempfile::tempdir().unwrap();
        let (commands, mut incoming) = mpsc::unbounded_channel();
        let mut handle = history_handle(directory.path(), "Reconnected", &commands);
        let (mut ui, mut editor) = initialize_view(&mut handle).await.unwrap();
        editor.insert_str("original draft");
        let original = handle
            .control_tx
            .as_ref()
            .unwrap()
            .binding()
            .unwrap()
            .clone();
        let mut next = original.clone();
        next.revision += 1;
        next.view_id = uuid::Uuid::now_v7();
        let replacement = || {
            let session = original.flow.session_id();
            let view =
                LegacyHistory::inspect(&directory.path().join(format!("{session}.jsonl")), session)
                    .unwrap()
                    .read_flow(&LegacyFlowKey::Default)
                    .unwrap();
            Box::new(
                TuiHandle::from_legacy(
                    view,
                    next.clone(),
                    commands.clone(),
                    Some("Reconnected".into()),
                    None,
                )
                .unwrap(),
            )
        };
        let panel = ui.wm.open(
            "memory",
            crate::wm::ContentKey::Knowledge,
            crate::wm::WindowContent::Knowledge,
            "Memory & Rules",
            Rect::new(0, 0, 120, 40),
        );
        ui.wm
            .panels
            .iter_mut()
            .find(|window| window.id == panel)
            .unwrap()
            .content = Some(Box::new(
            crate::window::knowledge_panel::KnowledgePanelContent::new(
                ui.app.knowledge_state.clone(),
                handle.control_tx.clone(),
            ),
        ));
        ui.app
            .knowledge_state
            .lock()
            .unwrap()
            .request_read(handle.control_tx.as_ref(), false);
        let request = incoming
            .try_recv()
            .expect("memory action sends original read");
        let TuiControl::ListKnowledge { request_id } = request.command else {
            panic!("original read");
        };
        assert_eq!(ui.wm.focused_id(), Some(panel));
        assert!(replace_attachment(&mut ui, &mut editor, &mut handle, replacement()).is_err());
        assert_eq!(
            handle.control_tx.as_ref().unwrap().binding(),
            Some(&original)
        );
        assert_eq!(
            ui.app.knowledge_state.lock().unwrap().read_request,
            Some(request_id)
        );
        request
            .replies
            .send(TuiCommand::KnowledgeResult {
                request_id: Some(request_id),
                result: Ok((Vec::new(), Vec::new())),
            })
            .unwrap();
        let (ticket, command) = tokio::time::timeout(
            Duration::from_secs(2),
            recv_cmd(
                &mut handle.cmd_rx,
                &mut handle.control_reply_rx,
                Some(&original),
            ),
        )
        .await
        .expect("original read reply")
        .unwrap();
        apply_view_command(
            &mut ui,
            &mut editor,
            ticket.as_ref(),
            command,
            handle.control_tx.as_ref(),
        );
        replace_attachment(&mut ui, &mut editor, &mut handle, replacement()).unwrap();
        assert_eq!(editor.buf(), "original draft");
        assert_eq!(ui.wm.focused_id(), Some(panel));
        assert_eq!(handle.control_tx.as_ref().unwrap().binding(), Some(&next));
        let (consumed, actions) = ui.wm.dispatch_key(
            &KeyAction::Char('r'),
            &mut ui.app,
            handle.control_tx.as_ref(),
        );
        assert!(consumed, "retained panel handles refresh");
        assert!(actions.is_empty());
        let refreshed = incoming
            .try_recv()
            .expect("refresh reaches replacement source");
        assert_eq!(refreshed.ticket.binding, next);
        assert!(matches!(
            refreshed.command,
            TuiControl::ListKnowledge { .. }
        ));
        assert_eq!(request.ticket.binding, original);
        assert!(
            incoming.try_recv().is_err(),
            "rebind does not automatically resubmit"
        );
    }

    #[tokio::test]
    async fn hidden_mailboxes_receive_bound_reads_and_writes_without_projecting_sources() {
        let directory = tempfile::tempdir().unwrap();
        let (commands, mut incoming) = mpsc::unbounded_channel();
        let mut handle = history_handle(directory.path(), "Hidden A", &commands);
        let (mut ui, mut editor) = initialize_view(&mut handle).await.unwrap();
        let mut workspace = Workspace::new(&handle);
        let a = workspace.active.clone().unwrap();
        editor.insert_str("A original draft");
        let control = handle.control_tx.as_ref().unwrap();
        ui.app
            .knowledge_state
            .lock()
            .unwrap()
            .request_read(Some(control), false);
        let read = incoming.recv().await.unwrap();
        let TuiControl::ListKnowledge { request_id } = read.command else {
            panic!("read request");
        };
        let id = atman_runtime::memory::MemoryId::now();
        ui.app.knowledge_state.lock().unwrap().archiving = true;
        control
            .send(TuiControl::ArchiveConfession {
                id: id.clone(),
                base_revision: 0,
            })
            .unwrap();
        let write = incoming.recv().await.unwrap();
        let other = history_handle(directory.path(), "Visible B", &commands);
        let b = ViewKey {
            flow: other
                .control_tx
                .as_ref()
                .unwrap()
                .binding()
                .unwrap()
                .flow
                .clone(),
            placement: FlowPlacement::Main,
        };
        workspace
            .focus_ready(
                &mut ui,
                &mut editor,
                &mut handle,
                b.clone(),
                Some(Box::new(other)),
            )
            .await
            .unwrap();
        editor.insert_str("B original draft");
        let hidden = workspace.views.get_mut(&a).unwrap();
        assert!(hidden.ui.app.flow_loading.is_none());
        assert!(!hidden.ui.app.settings.is_waiting());
        let original_revision = hidden.ui.app.items.revision_clock();
        let FlowViewSource::Legacy { delivered, .. } = hidden.handle.flow_view.as_mut().unwrap()
        else {
            panic!("retained legacy source");
        };
        *delivered = false;
        read.replies
            .send(TuiCommand::KnowledgeResult {
                request_id: Some(request_id),
                result: Err("original read error".into()),
            })
            .unwrap();
        write.replies.send(TuiCommand::ConfessionArchived).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let complete = {
                    let knowledge = workspace.views[&a].ui.app.knowledge_state.lock().unwrap();
                    knowledge.read_request.is_none() && !knowledge.archiving
                };
                if complete {
                    break;
                }
                workspace.next_background().await;
            }
        })
        .await
        .unwrap();
        let hidden = &workspace.views[&a];
        assert_eq!(
            hidden
                .ui
                .app
                .knowledge_state
                .lock()
                .unwrap()
                .read_error
                .as_deref(),
            Some("original read error")
        );
        assert_eq!(hidden.ui.app.items.revision_clock(), original_revision);
        assert!(matches!(
            hidden.handle.flow_view.as_ref(),
            Some(FlowViewSource::Legacy {
                delivered: false,
                ..
            })
        ));
        assert_eq!(hidden.editor.buf(), "A original draft");
        assert_eq!(editor.buf(), "B original draft");
        assert_eq!(workspace.active.as_ref(), Some(&b));
        assert!(!workspace.has_active_animation());
        drop(read.replies);
        drop(write.replies);
        // No ready loop after mailbox consumption, including closed unbound channels.
        let (closed, rx) = mpsc::unbounded_channel();
        workspace.views.get_mut(&a).unwrap().handle.cmd_rx = Some(rx);
        drop(closed);
        assert!(
            tokio::time::timeout(Duration::from_millis(30), workspace.next_background())
                .await
                .is_err()
        );
        assert!(workspace.views[&a].handle.cmd_rx.is_none());
    }

    #[tokio::test]
    async fn constrained_terminal_panels_resize_their_original_control_destination() {
        let directory = tempfile::tempdir().unwrap();
        let (commands, mut incoming) = mpsc::unbounded_channel();
        let mut handle = history_handle(directory.path(), "Terminal owner", &commands);
        let (mut ui, editor) = initialize_view(&mut handle).await.unwrap();
        let mut workspace = Workspace::new(&handle);
        let mut terminal = ratatui::Terminal::new(TestBackend::new(200, 65)).unwrap();
        terminal
            .draw(|frame| workspace.render(frame, &mut ui, &editor).unwrap())
            .unwrap();
        let window = ui.wm.open(
            "terminal",
            crate::wm::ContentKey::Task("terminal".into()),
            crate::wm::WindowContent::Task {
                handle: "terminal".into(),
                kind: atman_runtime::TaskKind::Terminal,
            },
            "Terminal",
            ui.app.maximized_canvas(),
        );
        let canvas = Rect::new(32, 0, 78, 35);
        workspace.constrain_panels(&mut ui, canvas, handle.control_tx.as_ref());
        let request = incoming.try_recv().unwrap();
        assert_eq!(
            &request.ticket.binding,
            handle.control_tx.as_ref().unwrap().binding().unwrap()
        );
        let panel = ui
            .wm
            .panels
            .iter()
            .find(|panel| panel.id == window)
            .unwrap();
        assert!(
            matches!(request.command, TuiControl::TermResize { handle, rows, cols } if handle == "terminal" && rows == panel.rect.height - 5 && cols == panel.rect.width - 8)
        );
        workspace.constrain_panels(&mut ui, canvas, handle.control_tx.as_ref());
        assert!(
            incoming.try_recv().is_err(),
            "unchanged geometry does not issue another resize"
        );
    }

    #[tokio::test]
    async fn cached_panels_fit_the_new_viewport_without_covering_the_composer() {
        let directory = tempfile::tempdir().unwrap();
        let (commands, _incoming) = mpsc::unbounded_channel();
        let mut handle = history_handle(directory.path(), "Warm A", &commands);
        let (mut ui, mut editor) = initialize_view(&mut handle).await.unwrap();
        let mut workspace = Workspace::new(&handle);
        let a = workspace.active.clone().unwrap();
        editor.insert_str("A visible draft");
        let mut terminal = ratatui::Terminal::new(TestBackend::new(200, 65)).unwrap();
        terminal
            .draw(|frame| workspace.render(frame, &mut ui, &editor).unwrap())
            .unwrap();
        let canvas = ui.app.maximized_canvas();
        let window = ui.wm.open_with_size(
            "knowledge",
            crate::wm::ContentKey::Knowledge,
            crate::wm::OpenPolicy::ReuseExisting,
            crate::wm::WindowContent::Knowledge,
            "Memory & Rules",
            canvas,
            100,
            36,
            false,
        );
        ui.app.knowledge_state.lock().unwrap().message = "Knowledge snapshot".into();
        ui.wm
            .panels
            .iter_mut()
            .find(|panel| panel.id == window)
            .unwrap()
            .content = Some(Box::new(
            crate::window::knowledge_panel::KnowledgePanelContent::new(
                ui.app.knowledge_state.clone(),
                None,
            ),
        ));
        terminal
            .draw(|frame| workspace.render(frame, &mut ui, &editor).unwrap())
            .unwrap();
        let cold = history_handle(directory.path(), "Cold B", &commands);
        let b = ViewKey {
            flow: cold
                .control_tx
                .as_ref()
                .unwrap()
                .binding()
                .unwrap()
                .flow
                .clone(),
            placement: FlowPlacement::Main,
        };
        workspace
            .focus_ready(&mut ui, &mut editor, &mut handle, b, Some(Box::new(cold)))
            .await
            .unwrap();
        editor.insert_str("B retained draft");
        let mut terminal = ratatui::Terminal::new(TestBackend::new(110, 35)).unwrap();
        terminal
            .draw(|frame| workspace.render(frame, &mut ui, &editor).unwrap())
            .unwrap();
        workspace
            .focus_ready(&mut ui, &mut editor, &mut handle, a, None)
            .await
            .unwrap();
        assert_eq!(editor.buf(), "A visible draft");
        terminal
            .draw(|frame| workspace.render(frame, &mut ui, &editor).unwrap())
            .unwrap();
        let panel = ui
            .wm
            .panels
            .iter()
            .find(|panel| panel.id == window)
            .unwrap();
        let bounds = ui.app.maximized_canvas();
        assert!(bounds.contains((panel.rect.x, panel.rect.y).into()));
        assert!(panel.rect.right() <= bounds.right() && panel.rect.bottom() <= bounds.bottom());
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("A visible draft"));
        assert!(text.contains("Knowledge snapshot"));
        assert!(
            text.contains("[R]"),
            "cached panel controls stay above the input"
        );
    }

    fn history_handle(
        directory: &std::path::Path,
        title: &str,
        commands: &mpsc::UnboundedSender<crate::control::BoundControl>,
    ) -> TuiHandle {
        use atman_runtime::{
            event::{Event, EventEnvelope},
            event_log::legacy::LegacyFlowKey,
            message::Message,
            projection::flow::LegacyHistory,
        };
        let session = uuid::Uuid::now_v7();
        let turn = atman_rt::TurnId::now();
        let path = directory.join(format!("{session}.jsonl"));
        let event = EventEnvelope::new(
            1,
            Event::UserMsg {
                turn_id: turn.clone(),
                flow_run_id: None,
                message: Message::user_text(turn, format!("{title} history ready")),
                presentation: None,
                injection_id: None,
            },
        );
        std::fs::write(
            &path,
            format!("{}\n", serde_json::to_string(&event).unwrap()),
        )
        .unwrap();
        TuiHandle::from_legacy(
            LegacyHistory::inspect(&path, session)
                .unwrap()
                .read_flow(&LegacyFlowKey::Default)
                .unwrap(),
            ViewBinding {
                view_id: uuid::Uuid::now_v7(),
                revision: 1,
                flow: ViewAddress::Legacy {
                    session,
                    owner: LegacyFlowKey::Default,
                },
            },
            commands.clone(),
            Some(title.into()),
            None,
        )
        .unwrap()
    }

    #[tokio::test]
    async fn cold_view_preparation_survives_focus_and_cancelled_poll_without_stealing_focus() {
        let directory = tempfile::tempdir().unwrap();
        let (commands, _incoming) = mpsc::unbounded_channel();
        let mut handle = history_handle(directory.path(), "Warm A", &commands);
        let (mut ui, mut editor) = initialize_view(&mut handle).await.unwrap();
        let mut workspace = Workspace::new(&handle);
        let a = workspace.active.clone().unwrap();
        editor.insert_str("A draft");
        let mut cold = history_handle(directory.path(), "Cold B", &commands);
        let b = ViewKey {
            flow: cold
                .control_tx
                .as_ref()
                .unwrap()
                .binding()
                .unwrap()
                .flow
                .clone(),
            placement: FlowPlacement::Main,
        };
        let gate = std::sync::Arc::new(tokio::sync::Semaphore::new(0));
        cold.flow_view = Some(cold.flow_view.take().unwrap().prepare_with_limit(
            None,
            None,
            true,
            gate.clone(),
        ));
        tokio::time::timeout(
            Duration::from_millis(100),
            workspace.focus(
                &mut ui,
                &mut editor,
                &mut handle,
                b.clone(),
                Some(Box::new(cold)),
            ),
        )
        .await
        .unwrap()
        .unwrap();
        let started = ui.app.flow_loading.unwrap().started_at;
        editor.insert_str("B draft");
        ui.app.pending_quote = Some("B quote".into());
        ui.app.follow_tail = false;
        ui.app.scroll_offset = 13;
        assert!(
            tokio::time::timeout(
                Duration::from_millis(10),
                handle.flow_view.as_mut().unwrap().next()
            )
            .await
            .is_err()
        );
        workspace
            .focus(&mut ui, &mut editor, &mut handle, a.clone(), None)
            .await
            .unwrap();
        assert_eq!(editor.buf(), "A draft");
        workspace
            .focus(&mut ui, &mut editor, &mut handle, b.clone(), None)
            .await
            .unwrap();
        assert_eq!(ui.app.flow_loading.unwrap().started_at, started);
        assert_eq!(editor.buf(), "B draft");
        workspace
            .focus(&mut ui, &mut editor, &mut handle, a.clone(), None)
            .await
            .unwrap();
        gate.add_permits(1);
        tokio::time::timeout(Duration::from_secs(2), async {
            while workspace.views[&b].ui.app.flow_loading.is_some() {
                workspace.next_background().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(workspace.active.as_ref(), Some(&a));
        assert_eq!(editor.buf(), "A draft");
        workspace
            .focus(&mut ui, &mut editor, &mut handle, b, None)
            .await
            .unwrap();
        assert!(ui.app.flow_loading.is_none());
        assert!(!ui.app.items.is_empty());
        assert_eq!(editor.buf(), "B draft");
        assert_eq!(ui.app.pending_quote.as_deref(), Some("B quote"));
        assert_eq!(ui.app.scroll_offset, 13);
        assert!(!ui.app.follow_tail);
    }

    #[test]
    fn docking_springs_position_and_size_without_resetting_velocity() {
        let now = std::time::Instant::now();
        let left = Rect::new(10, 20, 64, 6);
        let right = Rect::new(90, 40, 96, 12);
        let mut motion = DockMotion::default();
        assert_eq!(motion.place(left, false, now), left);
        assert!(!motion.moving);
        assert_eq!(motion.place(right, false, now), left);
        let middle = motion.place(right, false, now + std::time::Duration::from_millis(80));
        assert!(middle.x > left.x && middle.x < right.x);
        assert!(middle.y > left.y && middle.y < right.y);
        assert!(middle.width > left.width && middle.width < right.width);
        assert!(middle.height > left.height && middle.height < right.height);
        let velocity = motion.velocity;
        assert_eq!(
            motion.place(left, false, now + std::time::Duration::from_millis(80)),
            middle
        );
        assert_eq!(motion.velocity, velocity);
        assert_eq!(
            motion.place(left, false, now + std::time::Duration::from_millis(500)),
            left
        );
        assert!(!motion.moving);
        assert_eq!(
            motion.place(right, true, now + std::time::Duration::from_millis(510)),
            right
        );
        assert!(!motion.moving);
        assert_eq!(motion.velocity, [0.0; 4]);
        assert_eq!(
            motion.place(right, false, now + std::time::Duration::from_millis(700)),
            right
        );
    }

    #[test]
    fn docking_is_frame_rate_independent_and_animates_same_owner_resize() {
        let now = std::time::Instant::now();
        let origin = Rect::new(10, 20, 64, 6);
        let target = Rect::new(0, 14, 84, 12);
        let mut frequent = DockMotion::default();
        let mut sparse = DockMotion::default();
        for motion in [&mut frequent, &mut sparse] {
            motion.place(origin, false, now);
            assert_eq!(motion.place(target, false, now), origin);
        }
        for ms in [16, 32, 48, 64, 80] {
            frequent.place(target, false, now + std::time::Duration::from_millis(ms));
        }
        let drawn = sparse.place(target, false, now + std::time::Duration::from_millis(80));
        for index in 0..4 {
            assert!((frequent.position[index] - sparse.position[index]).abs() < 1e-9);
            assert!((frequent.velocity[index] - sparse.velocity[index]).abs() < 1e-9);
        }
        assert!(drawn.width > origin.width && drawn.width < target.width);
        // A stationary center and bottom remain anchored through a size change.
        assert_eq!(drawn.x * 2 + drawn.width, origin.x * 2 + origin.width);
        assert_eq!(drawn.bottom(), origin.bottom());
        assert_eq!(
            sparse.place(target, false, now + std::time::Duration::from_secs(1)),
            target
        );
        assert!(!sparse.moving);
    }

    #[test]
    fn spring_size_quantization_never_reverses_during_a_single_transition() {
        let now = std::time::Instant::now();
        for (origin, target) in [
            (Rect::new(32, 58, 83, 5), Rect::new(47, 59, 96, 5)),
            (Rect::new(47, 57, 96, 8), Rect::new(32, 58, 83, 5)),
        ] {
            let mut motion = DockMotion::default();
            motion.place(origin, false, now);
            motion.place(target, false, now);
            let mut previous = origin;
            for ms in (10..=1000).step_by(10) {
                let drawn = motion.place(target, false, now + std::time::Duration::from_millis(ms));
                for (before, after, from, to) in [
                    (previous.width, drawn.width, origin.width, target.width),
                    (previous.height, drawn.height, origin.height, target.height),
                ] {
                    assert!(if from <= to {
                        after >= before
                    } else {
                        after <= before
                    });
                }
                previous = drawn;
            }
            assert_eq!(previous, target);
            assert!(!motion.moving);
        }
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
            minimum_size: (64, 20),
            preferred_size: None,
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
            active_binding: None,
            main: Some(main.clone()),
            views: HashMap::new(),
            windows: vec![first, second],
            closed_windows: HashMap::new(),
            main_history: crate::flow_hub::History::default(),
            surfaces: HashMap::new(),
            canvas: Rect::default(),
            dock_canvas: Rect::default(),
            composer_rect: None,
            motion: DockMotion::default(),
            sidebar: Default::default(),
            previews: HashMap::new(),
            focus_history: vec![],
            geometry: None,
            background_start: 0,
            cursor_visible: false,
            questions: Default::default(),
            control_work: Default::default(),
        };
        let ids = [
            uuid::Uuid::now_v7(),
            uuid::Uuid::now_v7(),
            uuid::Uuid::now_v7(),
        ];
        for (key, id) in [&main, &first_key, &second_key].into_iter().zip(ids) {
            workspace.active = Some(key.clone());
            let ticket = ControlTicket {
                history_head: None,
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
        workspace.remember_window(&first_key);
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
    fn window_preferences_survive_close_resize_and_maximized_reopen() {
        let mut window = window();
        let key = window.key.clone();
        let preferred = window.rect;
        window.hover = Some(crate::wm::PanelBtn::Resize);
        window.last_titlebar_click = Some(std::time::Instant::now());
        let mut main = key.clone();
        main.placement = FlowPlacement::Main;
        let mut workspace = Workspace {
            active: Some(key.clone()),
            main: Some(main),
            active_binding: None,
            views: HashMap::new(),
            windows: vec![window],
            closed_windows: HashMap::new(),
            main_history: Default::default(),
            surfaces: HashMap::new(),
            canvas: Rect::default(),
            dock_canvas: Rect::default(),
            composer_rect: None,
            motion: Default::default(),
            sidebar: Default::default(),
            previews: HashMap::new(),
            focus_history: vec![],
            geometry: None,
            background_start: 0,
            cursor_visible: false,
            questions: Default::default(),
            control_work: Default::default(),
        };
        let mut ui = UiState::new(AppState::default());
        let editor = InputEditor::default();
        for (width, height) in [(70, 30), (200, 65)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal
                .draw(|frame| workspace.render(frame, &mut ui, &editor).unwrap())
                .unwrap();
            assert_eq!(workspace.windows[0].rect, preferred);
        }
        workspace.remember_window(&key);
        assert!(workspace.windows.is_empty());
        assert_eq!(workspace.closed_windows[&key].rect, preferred);
        let other = window_key();
        workspace.open_window(other.clone());
        assert!(workspace.windows[0].rect.is_empty());
        workspace.open_window(key.clone());
        workspace.open_window(key.clone());
        assert_eq!(workspace.windows.len(), 2);
        let restored = workspace.windows.last().unwrap();
        assert_eq!(restored.key, key);
        assert_eq!(restored.rect, preferred);
        assert!(restored.hover.is_none());
        assert!(restored.last_titlebar_click.is_none());
        assert!(!workspace.closed_windows.contains_key(&key));
        workspace.maximize(&key);
        workspace.remember_window(&key);
        workspace.open_window(key.clone());
        assert_eq!(workspace.windows.last().unwrap().restore, Some(preferred));
        workspace.maximize(&key);
        assert_eq!(workspace.windows.last().unwrap().rect, preferred);
    }

    fn window_key() -> ViewKey {
        window().key
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
                .draw(|frame| {
                    window.draw(frame, frame.area(), true, None, "Review scheduling", "⬡")
                })
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
            active_binding: None,
            views: HashMap::new(),
            windows: vec![window],
            closed_windows: HashMap::new(),
            main_history: crate::flow_hub::History::default(),
            surfaces: HashMap::new(),
            canvas: Rect::new(0, 0, 40, 24),
            dock_canvas: Rect::new(0, 0, 40, 24),
            composer_rect: None,
            motion: DockMotion::default(),
            sidebar: Default::default(),
            previews: HashMap::new(),
            focus_history: vec![],
            geometry: None,
            background_start: 0,
            cursor_visible: false,
            questions: Default::default(),
            control_work: Default::default(),
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
                active_binding: None,
                views: HashMap::new(),
                windows: vec![],
                closed_windows: HashMap::new(),
                main_history: Default::default(),
                surfaces: HashMap::new(),
                canvas: Rect::default(),
                dock_canvas: Rect::default(),
                composer_rect: None,
                motion: Default::default(),
                sidebar: Default::default(),
                previews: HashMap::new(),
                focus_history: vec![],
                geometry: None,
                background_start: 0,
                cursor_visible: false,
                questions: Default::default(),
                control_work: Default::default(),
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

    #[test]
    fn project_navigation_owns_the_surface_without_exposing_the_composer() {
        for native in [false, true] {
            let mut key = window().key;
            key.placement = FlowPlacement::Main;
            let mut workspace = Workspace {
                active: Some(key.clone()),
                main: Some(key),
                active_binding: None,
                views: HashMap::new(),
                windows: vec![],
                closed_windows: HashMap::new(),
                main_history: Default::default(),
                surfaces: HashMap::new(),
                canvas: Rect::default(),
                dock_canvas: Rect::default(),
                composer_rect: None,
                motion: Default::default(),
                sidebar: Default::default(),
                previews: HashMap::new(),
                focus_history: vec![],
                geometry: None,
                background_start: 0,
                cursor_visible: false,
                questions: Default::default(),
                control_work: Default::default(),
            };
            let mut ui = UiState::new(AppState::new("surface".into(), None));
            let mut editor = InputEditor::default();
            editor.insert_str("PRESERVED-COMPOSER-DRAFT");
            ui.app.pending_quote = Some("PRESERVED-QUOTE".into());
            let mut terminal = Terminal::new(TestBackend::new(160, 45)).unwrap();
            let mut paint = |ui: &mut UiState, editor: &InputEditor| {
                terminal
                    .draw(|frame| {
                        if native {
                            workspace.render(frame, ui, editor).unwrap();
                        } else {
                            crate::render::render_frame(frame, ui, editor);
                        }
                    })
                    .unwrap();
                terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect::<String>()
            };
            assert!(paint(&mut ui, &editor).contains("PRESERVED-COMPOSER-DRAFT"));
            let old_input = ui.app.input_rect.unwrap();
            ui.wm.open_project_hub(
                Rect::new(0, 0, 160, 45),
                ui.app.project_sidebar.clone(),
                None,
                None,
            );
            let project = ui.wm.focused_id().unwrap();
            let text = paint(&mut ui, &editor);
            assert!(text.contains("Projects"));
            assert!(
                !text.contains("PRESERVED-COMPOSER-DRAFT"),
                "navigation owns the complete surface"
            );
            assert!(!text.contains("PRESERVED-QUOTE"));
            assert!(
                ui.app.composer_rect.is_none()
                    && ui.app.input_rect.is_none()
                    && ui.app.quote_rect.is_none()
            );
            assert!(ui.app.input_controls.is_empty());
            let click = crossterm::event::MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: old_input.x + 2,
                row: old_input.y + 1,
                modifiers: KeyModifiers::NONE,
            };
            assert!(!super::super::handle_composer_mouse(
                &mut ui,
                &mut editor,
                &click,
                None,
                None
            ));
            assert_eq!(ui.wm.focused_id(), Some(project));
            crate::key_handler::handle_key(
                crate::keys::KeyAction::Char('z'),
                &mut ui,
                &mut editor,
                &mut None,
                None,
                None,
            );
            assert_eq!(editor.buf(), "PRESERVED-COMPOSER-DRAFT");
            let (controls, mut incoming) = crate::ControlSender::channel();
            crate::key_handler::handle_key(
                crate::keys::KeyAction::HardStop,
                &mut ui,
                &mut editor,
                &mut None,
                None,
                Some(&controls),
            );
            assert!(matches!(
                incoming.try_recv(),
                Ok(crate::TuiControl::HardStop)
            ));
            assert_eq!(editor.buf(), "PRESERVED-COMPOSER-DRAFT");
            assert!(ui.wm.dispatch_paste("hidden paste", &mut ui.app, None));
            let (_, commands) =
                ui.wm
                    .dispatch_key(&crate::keys::KeyAction::Escape, &mut ui.app, None);
            ui.wm.apply_commands(&mut ui.app, commands, None);
            assert!(paint(&mut ui, &editor).contains("PRESERVED-COMPOSER-DRAFT"));
            assert_eq!(ui.app.pending_quote.as_deref(), Some("PRESERVED-QUOTE"));
            assert!(ui.app.input_rect.is_some());
        }
    }
}
