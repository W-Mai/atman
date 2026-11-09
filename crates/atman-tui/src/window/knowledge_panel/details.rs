//! Captured Knowledge detail text and viewport-only panel painting.

use std::sync::Arc;

use ratatui::{Frame, layout::Rect, style::Style, text::Line, widgets::Paragraph};

use super::{ArchiveConfirm, ConfessionChange, KnowledgeState, MemoryId, projection::Snapshot};
use crate::{
    app::OutputRevision,
    window::output_panel::preparation::{Key, Preparation, Rendered, Source},
    wm::component::RenderCtx,
};

#[derive(Clone)]
pub(super) struct Request {
    snapshot: Snapshot,
    tab: usize,
    selected: Option<usize>,
    show_history: bool,
    history: Option<Arc<[ConfessionChange]>>,
    history_id: Option<MemoryId>,
    history_request: Option<MemoryId>,
    read_pending: bool,
    filter_pending: bool,
    read_error: Option<Arc<str>>,
    loading: bool,
    message: Arc<str>,
    archive: Option<ArchiveConfirm>,
}

impl Request {
    pub fn capture(
        state: &KnowledgeState,
        tab: usize,
        selected: Option<usize>,
        show_history: bool,
        archive: Option<&ArchiveConfirm>,
        filtering: bool,
    ) -> Self {
        let history = tab == 0 && show_history;
        Self {
            snapshot: Snapshot::capture(state),
            tab,
            selected,
            show_history,
            history: history.then(|| state.history.clone()),
            history_id: history.then(|| state.history_id.clone()).flatten(),
            history_request: history
                .then(|| state.history_request.as_ref().map(|(_, id)| id.clone()))
                .flatten(),
            read_pending: selected.is_none() && state.read_request.is_some(),
            filter_pending: selected.is_none() && filtering,
            read_error: if selected.is_none() {
                state.read_error.as_deref().map(Arc::from)
            } else {
                None
            },
            loading: selected.is_none() && state.loading,
            message: if selected.is_none() {
                state.message.as_str().into()
            } else {
                Arc::default()
            },
            archive: archive.cloned(),
        }
    }

    fn same_identity(&self, other: &Self) -> bool {
        if let Some(archive) = &self.archive {
            return other.archive.as_ref().is_some_and(|other| {
                archive.id == other.id && archive.base_revision == other.base_revision
            });
        }
        if other.archive.is_some()
            || self.tab != other.tab
            || self.show_history != other.show_history
        {
            return false;
        }
        match (self.selected, other.selected, self.tab) {
            (None, None, _) => true,
            (Some(a), Some(b), 0) => {
                self.snapshot.confessions[a].confession.id
                    == other.snapshot.confessions[b].confession.id
            }
            (Some(a), Some(b), 1) => {
                let a = &self.snapshot.rules[a];
                let b = &other.snapshot.rules[b];
                a.name == b.name
                    && a.scope == b.scope
                    && a.source == b.source
                    && a.source_path == b.source_path
            }
            (Some(a), Some(b), _) => {
                self.snapshot.proposals[a].id == other.snapshot.proposals[b].id
            }
            _ => false,
        }
    }

    fn same_version(&self, other: &Self) -> bool {
        self.same_identity(other)
            && self.selected == other.selected
            && self.snapshot.same_version(&other.snapshot)
            && match (&self.history, &other.history) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (None, None) => true,
                _ => false,
            }
            && match (&self.archive, &other.archive) {
                (Some(a), Some(b)) => Arc::ptr_eq(&a.trigger, &b.trigger),
                (None, None) => true,
                _ => false,
            }
            && self.history_id == other.history_id
            && self.history_request == other.history_request
            && self.read_pending == other.read_pending
            && self.filter_pending == other.filter_pending
            && self.read_error == other.read_error
            && self.loading == other.loading
            && self.message == other.message
    }

    fn small(&self) -> bool {
        let bytes = if let Some(archive) = &self.archive {
            archive.trigger.len().saturating_add(256)
        } else if let Some(index) = self.selected {
            match self.tab {
                0 if self.show_history
                    && self
                        .history
                        .as_ref()
                        .is_some_and(|history| !history.is_empty()) =>
                {
                    return false;
                }
                0 => {
                    let view = &self.snapshot.confessions[index];
                    let c = &view.confession;
                    c.trigger
                        .len()
                        .saturating_add(c.rule_violated.len())
                        .saturating_add(c.what_i_did.len())
                        .saturating_add(c.why.len())
                        .saturating_add(c.mitigation.len())
                        .saturating_add(view.category.as_deref().map_or(0, str::len))
                        .saturating_add(256)
                }
                1 => {
                    let rule = &self.snapshot.rules[index];
                    rule.name
                        .len()
                        .saturating_add(rule.scope.len())
                        .saturating_add(rule.source.len())
                        .saturating_add(rule.source_path.len())
                        .saturating_add(rule.content.len())
                        .saturating_add(64)
                }
                _ => return false,
            }
        } else {
            self.message
                .len()
                .saturating_add(self.read_error.as_deref().map_or(0, str::len))
                .saturating_add(128)
        };
        bytes <= 1024
    }

    fn text(&self) -> String {
        let mut detail = match self.selected {
            Some(index)
                if self.tab == 0
                    && self.show_history
                    && self.history_id.as_ref()
                        != Some(&self.snapshot.confessions[index].confession.id)
                    && self.history_request.as_ref().is_some_and(|id| {
                        *id == self.snapshot.confessions[index].confession.id
                    }) =>
            {
                "Loading history".into()
            }
            Some(index)
                if self.tab == 0
                    && self.show_history
                    && self.history_id.as_ref()
                        == Some(&self.snapshot.confessions[index].confession.id) =>
            {
                let view = &self.snapshot.confessions[index];
                let mut text = format!(
                    "{}\nCreated: {}\n\n",
                    view.confession.trigger, view.confession.created_at
                );
                for (index, change) in self.history.as_deref().unwrap_or(&[]).iter().enumerate() {
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
                let view = &self.snapshot.confessions[index];
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
                let rule = &self.snapshot.rules[index];
                format!(
                    "{}\n\n{} · {}\n{}\n\n{}",
                    rule.name, rule.scope, rule.source, rule.source_path, rule.content
                )
            }
            Some(index) => {
                let p = &self.snapshot.proposals[index];
                let confession = self
                    .snapshot
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
                if self.read_pending || self.filter_pending {
                    String::new()
                } else if let Some(error) = &self.read_error {
                    error.to_string()
                } else if self.tab == 2 && self.loading {
                    format!("{}\n\n[C] Cancel", self.message)
                } else if self.tab == 2 && self.message.starts_with("Organization failed:") {
                    format!("{}\n\n[O] Retry", self.message)
                } else if self.tab == 2 && self.snapshot.proposals.is_empty() {
                    format!("{}\n\n[O] Organize to generate suggestions", self.message)
                } else {
                    "No matching entries".into()
                }
            }
        };
        if let Some(confirm) = &self.archive {
            detail = format!(
                "Archive this confession?\n\n{}\n\nIt will disappear from active memory and search. Its revision history remains.\n\n[Y] Archive · [N] Cancel",
                confirm.trigger
            );
        }
        detail
    }
}

#[derive(Default)]
pub(super) struct Details {
    preparation: Preparation,
    request: Option<Request>,
    identity: u64,
    revision: u64,
}

impl Details {
    pub fn is_pending(&self) -> bool {
        self.preparation.is_pending()
    }
    pub fn error(&self) -> Option<&str> {
        self.preparation.error()
    }
    pub fn retry(&mut self) {
        self.preparation.retry();
    }

    pub fn render(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        ctx: &RenderCtx<'_>,
        request: Request,
        scroll: &mut u16,
        hovered: bool,
    ) -> Option<Rect> {
        if area.width == 0 || area.height == 0 {
            return None;
        }
        if self
            .request
            .as_ref()
            .is_none_or(|old| !old.same_version(&request))
        {
            if self
                .request
                .as_ref()
                .is_none_or(|old| !old.same_identity(&request))
            {
                self.identity = self.identity.wrapping_add(1);
            }
            self.revision = self.revision.wrapping_add(1);
            crate::render_work::discard(self.request.replace(request));
        } else {
            crate::render_work::discard(request);
        }
        let key = Key {
            window: ctx.window_id,
            width: area.width,
            theme: crate::theme::theme(),
            preferences: crate::render_preferences::RenderPreferences::current(),
            revision: OutputRevision {
                id: self.identity,
                layout: self.revision,
                source_generation: self.revision,
                ..Default::default()
            },
        };
        if self.request.as_ref().is_some_and(Request::small) {
            if self
                .preparation
                .ready
                .as_ref()
                .is_none_or(|ready| ready.key != key)
            {
                let text = self.request.as_ref().expect("detail request").text();
                let lines = crate::width::word_wrap(&text, usize::from(key.width))
                    .into_iter()
                    .map(Line::from)
                    .collect::<Vec<_>>()
                    .into();
                self.preparation.set_ready_lines(key, lines);
            }
        } else {
            self.preparation.update_retaining(key, || {
                let request = self.request.as_ref().expect("detail request").clone();
                Source::Render {
                    surface: None,
                    operation: Box::new(move || {
                        let text = request.text();
                        Some(Rendered {
                            lines: crate::width::word_wrap(&text, usize::from(key.width))
                                .into_iter()
                                .map(Line::from)
                                .collect::<Vec<_>>()
                                .into(),
                            paint: Default::default(),
                            regions: Arc::from([]),
                            inspector: None,
                            animated: false,
                        })
                    }),
                }
            });
        }
        if let Some(ready) = self.preparation.ready.as_ref().filter(|ready| {
            ready.key.window == key.window
                && ready.key.revision.id == key.revision.id
                && ready.key.width == key.width
                && ready.key.theme == key.theme
                && ready.key.preferences == key.preferences
        }) {
            let maximum = ready
                .lines
                .len()
                .saturating_sub(usize::from(area.height))
                .min(usize::from(u16::MAX)) as u16;
            *scroll = (*scroll).min(maximum);
            let start = usize::from(*scroll);
            let end = start
                .saturating_add(usize::from(area.height))
                .min(ready.lines.len());
            frame.render_widget(
                Paragraph::new(ready.lines[start..end].to_vec()).style(
                    Style::default()
                        .fg(key.theme.tinted_fg.into())
                        .bg(crate::wm::shell::surface_bg(&key.theme)),
                ),
                area,
            );
        } else if self.preparation.is_pending() {
            frame.render_widget(
                Paragraph::new(crate::loading::line("", ctx.animation_frame, area.width)),
                Rect { height: 1, ..area },
            );
        }
        if let Some(error) = self.error() {
            frame.render_widget(
                Paragraph::new(error.to_owned())
                    .style(Style::default().fg(key.theme.subtle_fg.into())),
                Rect { height: 1, ..area },
            );
            if area.height > 1 {
                let rect = Rect::new(
                    area.x,
                    area.y + 1,
                    area.width.min(16),
                    area.height.saturating_sub(1).min(3),
                );
                crate::settings_form::render_action(
                    frame,
                    rect,
                    &crate::settings_form::Action {
                        label: "Retry",
                        shortcut: "R",
                        hovered,
                        ..Default::default()
                    },
                    &key.theme,
                );
                return Some(rect);
            }
        }
        None
    }
}

impl Drop for Details {
    fn drop(&mut self) {
        crate::render_work::discard(self.request.take());
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        KnowledgePanelContent,
        tests::{render_once, render_panel},
    };
    use super::*;
    use crate::wm::component::WindowComponent;
    use ratatui::{Terminal, backend::TestBackend};
    use std::{
        sync::{Mutex, mpsc},
        time::{Duration, Instant},
    };

    fn rule(name: &str, content: String) -> crate::RuleView {
        crate::RuleView {
            name: name.into(),
            description: String::new(),
            scope: "project".into(),
            source: "file".into(),
            source_path: "rules.md".into(),
            content,
        }
    }

    #[test]
    fn detail_preparation_captures_source_and_keeps_events_independent_of_workers() {
        let pool = futures::executor::ThreadPool::builder()
            .pool_size(1)
            .create()
            .unwrap();
        let (started, entered) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        pool.spawn_ok(async move {
            started.send(()).unwrap();
            let _ = wait.recv();
        });
        entered.recv_timeout(Duration::from_secs(2)).unwrap();
        let state = Arc::new(Mutex::new(KnowledgeState {
            rules: vec![
                rule("Large", "row 汉 e\u{301}\n".repeat(10_000)),
                rule("Small", "immediate".into()),
            ]
            .into(),
            ..Default::default()
        }));
        let mut panel = KnowledgePanelContent::new(state.clone(), None);
        panel.details.preparation = Preparation::with_pool(pool);
        panel.tab = 1;
        let mut terminal = Terminal::new(TestBackend::new(100, 36)).unwrap();
        let start = Instant::now();
        render_once(&mut panel, &mut terminal);
        assert!(start.elapsed() < Duration::from_millis(100));
        assert!(panel.details.is_pending() && panel.has_active_animation());
        assert!(Arc::ptr_eq(
            &panel.details.request.as_ref().unwrap().snapshot.rules,
            &state.lock().unwrap().rules
        ));
        let revision = panel.details.revision;
        for _ in 0..10 {
            render_once(&mut panel, &mut terminal);
        }
        assert_eq!(
            panel.details.revision, revision,
            "unchanged capture must remain stable"
        );
        assert!(
            state.try_lock().is_ok(),
            "worker must never hold the panel mutex"
        );
        panel.selected = 1;
        render_once(&mut panel, &mut terminal);
        assert!(!panel.details.is_pending() && !panel.has_active_animation());
        let ready = panel.details.preparation.ready.as_ref().unwrap();
        assert!(ready.text.is_none(), "detail has no selectable copy source");
        assert!(
            ready
                .lines
                .iter()
                .any(|line| line.to_string().contains("immediate"))
        );
        let small_identity = ready.key.revision.id;
        release.send(()).unwrap();
        panel.selected = 0;
        render_panel(&mut panel, &mut terminal);
        let ready = panel.details.preparation.ready.as_ref().unwrap();
        assert_ne!(ready.key.revision.id, small_identity);
        let request = panel.details.request.as_ref().unwrap();
        let expected =
            crate::width::word_wrap(&request.text(), usize::from(panel.detail_rect.width));
        assert_eq!(
            ready
                .lines
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            expected
        );
        assert!(ready.text.is_none());
        let lines = ready.lines.clone();
        for _ in 0..10 {
            render_once(&mut panel, &mut terminal);
        }
        assert!(Arc::ptr_eq(
            &lines,
            &panel.details.preparation.ready.as_ref().unwrap().lines
        ));
        panel.detail_scroll = u16::MAX;
        render_once(&mut panel, &mut terminal);
        assert_eq!(
            usize::from(panel.detail_scroll),
            lines
                .len()
                .saturating_sub(usize::from(panel.detail_rect.height))
                .min(usize::from(u16::MAX))
        );
        assert!(!panel.has_active_animation());
    }

    #[test]
    fn obsolete_detail_source_never_renders_under_replacement_rows() {
        let state = Arc::new(Mutex::new(KnowledgeState {
            rules: vec![rule("Original", "old body\n".repeat(2000))].into(),
            ..Default::default()
        }));
        let mut panel = KnowledgePanelContent::new(state.clone(), None);
        panel.tab = 1;
        let mut terminal = Terminal::new(TestBackend::new(100, 36)).unwrap();
        render_panel(&mut panel, &mut terminal);
        let identity = panel.details.identity;
        state
            .lock()
            .unwrap()
            .replace_catalog(Vec::new(), vec![rule("Replacement", "new body".into())]);
        render_panel(&mut panel, &mut terminal);
        assert_ne!(panel.details.identity, identity);
        let ready = panel.details.preparation.ready.as_ref().unwrap();
        assert!(
            ready
                .lines
                .iter()
                .any(|line| line.to_string().contains("new body"))
        );
        assert!(
            !ready
                .lines
                .iter()
                .any(|line| line.to_string().contains("old body"))
        );
        terminal = Terminal::new(TestBackend::new(20, 10)).unwrap();
        render_once(&mut panel, &mut terminal);
        assert!(!panel.has_active_animation());
    }
}
