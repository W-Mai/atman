use crate::app::{AppState, NoteLevel, ToastPosition};
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
        self.app
            .open_task_panel(&mut self.wm, handle, canvas, maximized, background)
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
