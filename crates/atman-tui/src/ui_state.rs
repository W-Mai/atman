use crate::app::{AppState, NoteLevel, OutputItem, ToastPosition};
use crate::wm::WindowManager;

pub struct UiState {
    pub app: AppState,
    pub wm: WindowManager,
}

impl std::ops::Deref for UiState {
    type Target = AppState;

    fn deref(&self) -> &Self::Target {
        &self.app
    }
}

impl std::ops::DerefMut for UiState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.app
    }
}

impl UiState {
    pub fn new(app: AppState) -> Self {
        Self {
            app,
            wm: WindowManager::default(),
        }
    }

    pub fn open_task_panel(&mut self, handle: &str, canvas: ratatui::layout::Rect) {
        self.open_task_panel_impl(handle, canvas, false, false);
    }

    pub fn open_task_panel_maximized(&mut self, handle: &str) {
        let canvas = self.app.maximized_canvas();
        self.open_task_panel_impl(handle, canvas, true, false);
    }

    pub fn open_task_panel_background(&mut self, handle: &str) {
        let canvas = self.app.maximized_canvas();
        let focused_before = self.wm.focused_id();
        let opened = self.open_task_panel_impl(handle, canvas, false, true);
        if focused_before.is_some() && opened {
            self.app.push_toast(
                format!("background task ready — see panel {handle}"),
                NoteLevel::Info,
                std::time::Duration::from_secs(4),
                ToastPosition::TopRight,
            );
        }
    }

    fn open_task_panel_impl(
        &mut self,
        handle: &str,
        canvas: ratatui::layout::Rect,
        maximized: bool,
        background: bool,
    ) -> bool {
        let item = self
            .app
            .items
            .iter()
            .rev()
            .find(|it| it.handle() == Some(handle))
            .cloned();
        let snap = self
            .app
            .task_snapshots
            .iter()
            .find(|s| s.source_handle == handle)
            .cloned();

        // A completed bash task may have left the in-memory item list (e.g.
        // after a history restore) while its snapshot and session log file
        // survive. Reconstruct the output so the panel shows real content
        // instead of falling through to the empty placeholder.
        let item = self.app.reconstruct_bash_item(&snap).or(item);

        let (kind, label, pw, ph) = if let Some(item) = item {
            match item {
                OutputItem::Terminal {
                    handle: h, screen, ..
                } => {
                    let (pw, ph) = (screen.cols + 8, screen.rows + 5);
                    let label = snap
                        .as_ref()
                        .map(|s| s.label.clone())
                        .unwrap_or_else(|| h.clone());
                    (atman_runtime::TaskKind::Terminal, label, pw, ph)
                }
                OutputItem::Bash { handle: h, .. } => {
                    let (pw, ph) = self.app.panel_sizes.get(&h).copied().unwrap_or((0, 0));
                    let label = snap
                        .as_ref()
                        .map(|s| s.label.clone())
                        .unwrap_or_else(|| h.clone());
                    (atman_runtime::TaskKind::Bash, label, pw, ph)
                }
                OutputItem::SubAgentActivity { handle: h, .. } => {
                    let label = snap
                        .as_ref()
                        .map(|s| s.label.clone())
                        .unwrap_or_else(|| h.clone());
                    (atman_runtime::TaskKind::Flow, label, 0, 0)
                }
                OutputItem::WorkflowPanel { .. } => {
                    let kind = snap
                        .as_ref()
                        .map(|s| s.kind)
                        .unwrap_or(atman_runtime::TaskKind::Flow);
                    let label = snap
                        .as_ref()
                        .map(|s| s.label.clone())
                        .unwrap_or_else(|| handle.to_string());
                    (kind, label, 0, 0)
                }
                _ => unreachable!("handle() only returns Some for Terminal/Bash"),
            }
        } else {
            let kind = snap
                .as_ref()
                .map(|s| s.kind)
                .unwrap_or(atman_runtime::TaskKind::Flow);
            let label = snap
                .as_ref()
                .map(|s| s.label.clone())
                .unwrap_or_else(|| handle.to_string());
            (kind, label, 0, 0)
        };

        let content: Box<dyn crate::wm::WindowComponent> = match kind {
            atman_runtime::TaskKind::Bash => {
                Box::new(crate::window::bash_panel::BashPanelContent {
                    handle: handle.to_string(),
                    scroll: 0,
                })
            }
            atman_runtime::TaskKind::Terminal => {
                Box::new(crate::window::terminal_panel::TerminalPanelContent {
                    handle: handle.to_string(),
                    scroll: 0,
                })
            }
            atman_runtime::TaskKind::Flow => {
                Box::new(crate::window::flow_panel::FlowPanelContent {
                    handle: handle.to_string(),
                    scroll: 0,
                    render_cache: None,
                })
            }
        };

        let existing_ids: std::collections::HashSet<crate::wm::WindowId> =
            self.wm.panels.iter().map(|p| p.id).collect();
        let window_id = if background {
            self.wm.open_background_with_size(
                handle,
                crate::wm::ContentKey::Task(handle.to_string()),
                crate::wm::OpenPolicy::ReuseExisting,
                crate::wm::WindowContent::Task {
                    handle: handle.to_string(),
                    kind,
                },
                &label,
                canvas,
                pw,
                ph,
                maximized,
            )
        } else {
            self.wm.open_with_size(
                handle,
                crate::wm::ContentKey::Task(handle.to_string()),
                crate::wm::OpenPolicy::ReuseExisting,
                crate::wm::WindowContent::Task {
                    handle: handle.to_string(),
                    kind,
                },
                &label,
                canvas,
                pw,
                ph,
                maximized,
            )
        };
        let is_new = !existing_ids.contains(&window_id);
        if let Some(panel) = self
            .wm
            .panels
            .iter_mut()
            .find(|panel| panel.id == window_id)
        {
            panel.content = Some(content);
        }
        is_new
    }

    pub fn open_mermaid_panel(&mut self, item_idx: usize, canvas: ratatui::layout::Rect) {
        let id = format!("mermaid:{item_idx}");
        let source = self.app.mermaid_item_source(item_idx).unwrap_or_default();
        let lines = crate::mermaid::render_mermaid(&source, canvas.width.saturating_sub(8));
        let pw = lines
            .iter()
            .map(|l| crate::width::spans_width(&l.spans))
            .max()
            .unwrap_or(80)
            .max(80) as u16;
        let ph = lines.len() as u16 + 5;
        self.wm.open_with_size(
            &id,
            crate::wm::ContentKey::Mermaid(id.clone()),
            crate::wm::OpenPolicy::ReuseExisting,
            crate::wm::WindowContent::Mermaid {
                item_id: id.clone(),
            },
            "Mermaid Diagram",
            canvas,
            pw,
            ph,
            false,
        );
        if let Some(p) = self
            .wm
            .panels
            .iter_mut()
            .find(|p| p.content_key == crate::wm::ContentKey::Mermaid(id.clone()))
        {
            p.content = Some(Box::new(
                crate::window::mermaid_panel::MermaidPanelContent {
                    item_id: id.clone(),
                    scroll: 0,
                    h_scroll: 0,
                    split: false,
                },
            ));
        }
    }
}
