use crate::input::{InputEditor, visual_line_count, wrapped_cursor_col, wrapped_cursor_position};
use crate::keys::KeyAction;
use crate::wm::modal::ModalAction;
use atman_runtime::form::{FormAnswer, FormKind, FormQuestion, FormSubmission, PendingForm};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Wrap};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FormPhase {
    #[default]
    Editing,
    FinalConfirm,
}

#[derive(Debug, Clone)]
pub enum SubmitOutcome {
    Submit {
        request_id: u64,
        form_id: String,
        submission: FormSubmission,
    },
    None,
}

#[derive(Default)]
pub struct FormModal {
    pub open: bool,
    form_id: Option<String>,
    questions: Vec<FormQuestion>,
    preference: Option<crate::settings::Preference>,
    setting_request: Option<uuid::Uuid>,
    new_session: Option<SessionForm>,
    queued_runtime_form: Option<PendingForm>,
    pub current_index: usize,
    pub draft_answers: Vec<Option<FormAnswer>>,
    pub phase: FormPhase,
    pub confirm_focus: usize,
    pub multi_selected: Vec<bool>,
    pub text_editor: InputEditor,
    pub error: Option<String>,
    pending_submission: Option<u64>,
    next_submission_id: u64,
    pub last_input_rect: Option<Rect>,
    pub yes_rect: Option<Rect>,
    pub no_rect: Option<Rect>,
    pub scroll: u16,
    follow_cursor: bool,
    option_hits: Vec<(usize, Rect)>,
    hovered_option: Option<usize>,
    session_actions: Vec<(usize, Rect)>,
    hovered_action: Option<usize>,
}

struct SessionForm {
    locations: crate::new_session::Locations,
    directories: Vec<crate::new_session::Location>,
    directory_chosen: bool,
    pending: Option<crate::control::ControlTicket>,
}

impl FormModal {
    pub fn attach(&mut self, form: PendingForm) {
        let questions = questions(&form);
        self.attach_content(form.form_id, questions);
    }

    fn attach_content(&mut self, id: String, questions: Vec<FormQuestion>) {
        self.form_id = Some(id);
        self.preference = None;
        self.setting_request = None;
        self.new_session = None;
        self.session_actions.clear();
        self.hovered_action = None;
        self.open = true;
        self.current_index = 0;
        self.draft_answers = vec![None; questions.len()];
        self.questions = questions;
        self.phase = FormPhase::Editing;
        self.confirm_focus = 0;
        self.error = None;
        self.pending_submission = None;
        self.last_input_rect = None;
        self.yes_rect = None;
        self.no_rect = None;
        self.scroll = 0;
        self.follow_cursor = true;
        self.option_hits.clear();
        self.hovered_option = None;
        self.reset_question_state();
    }

    pub fn close(&mut self) {
        let queued = self.queued_runtime_form.take();
        self.preference = None;
        self.setting_request = None;
        self.new_session = None;
        self.session_actions.clear();
        self.hovered_action = None;
        self.pending_submission = None;
        self.open = false;
        self.form_id = None;
        self.questions.clear();
        self.current_index = 0;
        self.draft_answers.clear();
        self.phase = FormPhase::Editing;
        self.confirm_focus = 0;
        self.multi_selected.clear();
        self.text_editor = InputEditor::default();
        self.error = None;
        self.last_input_rect = None;
        self.yes_rect = None;
        self.no_rect = None;
        self.scroll = 0;
        self.follow_cursor = false;
        self.option_hits.clear();
        self.hovered_option = None;
        if let Some(form) = queued {
            self.attach(form);
        }
    }

    pub fn active_form_id(&self) -> Option<&str> {
        self.form_id.as_deref()
    }

    fn questions(&self) -> &[FormQuestion] {
        &self.questions
    }

    pub(crate) fn current_kind(&self) -> Option<&FormKind> {
        self.questions().get(self.current_index).map(|q| &q.kind)
    }

    pub(crate) fn sync_runtime(&mut self, form: Option<&PendingForm>) {
        if self.preference.is_some() || self.new_session.is_some() {
            self.queued_runtime_form = form.cloned();
        } else if let Some(form) = form {
            if self.active_form_id() != Some(&form.form_id) {
                self.attach(form.clone());
            }
        } else {
            self.close();
        }
    }

    pub(crate) fn open_new_session(
        &mut self,
        locations: crate::new_session::Locations,
    ) -> Result<(), String> {
        if self.open {
            return Err("Finish or close the current form first".into());
        }
        let projects = locations.projects.iter().map(|p| p.label.clone()).collect();
        let directories = locations
            .directories(0)
            .into_iter()
            .map(|p| p.label)
            .collect();
        self.attach_content(
            locations.id.to_string(),
            vec![
                FormQuestion {
                    id: "project".into(),
                    kind: FormKind::SingleSelect {
                        prompt: "Project".into(),
                        options: projects,
                    },
                },
                FormQuestion {
                    id: "directory".into(),
                    kind: FormKind::SingleSelect {
                        prompt: "Working directory".into(),
                        options: directories,
                    },
                },
            ],
        );
        self.new_session = Some(SessionForm {
            directories: locations.directories(0),
            locations,
            directory_chosen: false,
            pending: None,
        });
        self.fill_single_choices();
        if self.single_choice(0) {
            self.current_index = 1;
        }
        if self.single_choice(self.current_index) {
            self.phase = FormPhase::FinalConfirm;
        }
        Ok(())
    }

    fn single_choice(&self, index: usize) -> bool {
        matches!(self.questions.get(index).map(|q| &q.kind), Some(FormKind::SingleSelect { options, .. }) if options.len() == 1)
    }

    fn fill_single_choices(&mut self) {
        for (index, question) in self.questions.iter().enumerate() {
            if let FormKind::SingleSelect { options, .. } = &question.kind
                && options.len() == 1
            {
                self.draft_answers[index] = Some(FormAnswer::Selected {
                    index: 0,
                    label: options[0].clone(),
                });
            }
        }
    }

    fn session_selection(&self) -> Option<crate::new_session::Selection> {
        let session = self.new_session.as_ref()?;
        let project = self.draft_focus(0);
        let directory = session.directories.get(self.draft_focus(1))?.path.clone()?;
        Some(crate::new_session::Selection { project, directory })
    }

    pub(crate) fn owns_session_reply(
        &self,
        ticket: Option<&crate::control::ControlTicket>,
    ) -> bool {
        self.new_session
            .as_ref()
            .and_then(|s| s.pending.as_ref())
            .is_some_and(|pending| Some(pending) == ticket)
    }

    pub(crate) fn session_failed(
        &mut self,
        ticket: Option<&crate::control::ControlTicket>,
        id: uuid::Uuid,
        error: String,
    ) {
        if self.owns_session_reply(ticket)
            && self
                .new_session
                .as_ref()
                .is_some_and(|s| s.locations.id == id)
        {
            self.new_session.as_mut().unwrap().pending = None;
            self.pending_submission = None;
            self.error = Some(error);
        }
    }

    pub(crate) fn open_preference(
        &mut self,
        preference: crate::settings::Preference,
        app: &mut crate::app::AppState,
    ) {
        let result = if self.open {
            Err("Finish or close the current form first".into())
        } else {
            preference.selection(app)
        };
        match result {
            Ok((kind, selected)) => {
                self.attach_content(
                    uuid::Uuid::now_v7().to_string(),
                    vec![FormQuestion {
                        id: "setting".into(),
                        kind,
                    }],
                );
                self.preference = Some(preference);
                self.confirm_focus = selected;
            }
            Err(error) => app.push_note(error, crate::app::NoteLevel::Error),
        }
    }

    pub(crate) fn settings_result(&mut self, id: uuid::Uuid, result: &Result<String, String>) {
        if self.setting_request != Some(id) {
            return;
        }
        self.setting_request = None;
        self.pending_submission = None;
        match result {
            Ok(_) => self.close(),
            Err(error) => self.error = Some(error.clone()),
        }
    }

    fn clear_geometry(&mut self) {
        self.session_actions.clear();
        self.hovered_action = None;
        self.last_input_rect = None;
        self.yes_rect = None;
        self.no_rect = None;
        self.option_hits.clear();
        self.hovered_option = None;
    }

    fn reset_question_state(&mut self) {
        self.multi_selected = match self.current_kind() {
            Some(FormKind::MultiSelect { options, .. }) => {
                let mut selected = vec![false; options.len()];
                if let Some(Some(FormAnswer::MultiSelected { indices, .. })) =
                    self.draft_answers.get(self.current_index)
                {
                    for &index in indices {
                        if let Some(value) = selected.get_mut(index) {
                            *value = true;
                        }
                    }
                }
                selected
            }
            _ => Vec::new(),
        };
        self.text_editor = InputEditor::default();
        if let Some(Some(FormAnswer::TextEntered { text })) =
            self.draft_answers.get(self.current_index)
        {
            self.text_editor.insert_str(text);
        }
    }

    fn answer_current(&self) -> Option<FormAnswer> {
        match self.current_kind()? {
            FormKind::Confirm { .. } => Some(FormAnswer::Confirmed {
                value: self.confirm_focus == 0,
            }),
            FormKind::SingleSelect { options, .. } => {
                options
                    .get(self.confirm_focus)
                    .cloned()
                    .map(|label| FormAnswer::Selected {
                        index: self.confirm_focus,
                        label,
                    })
            }
            FormKind::MultiSelect {
                options, min, max, ..
            } => {
                let indices: Vec<_> = self
                    .multi_selected
                    .iter()
                    .enumerate()
                    .filter_map(|(i, b)| b.then_some(i))
                    .collect();
                if min.is_some_and(|m| indices.len() < m) {
                    return None;
                }
                if max.is_some_and(|m| indices.len() > m) {
                    return None;
                }
                let labels = indices
                    .iter()
                    .filter_map(|&i| options.get(i).cloned())
                    .collect();
                Some(FormAnswer::MultiSelected { indices, labels })
            }
            FormKind::Text { .. } => Some(FormAnswer::TextEntered {
                text: self.text_editor.buf().to_string(),
            }),
        }
    }

    fn commit_current(&mut self) -> bool {
        let previous = self.session_selection();
        let Some(answer) = self.answer_current() else {
            if let Some(FormKind::MultiSelect { min, max, .. }) = self.current_kind() {
                let count = self.multi_selected.iter().filter(|b| **b).count();
                self.error = if min.is_some_and(|m| count < m) {
                    min.map(|m| format!("Select at least {m}"))
                } else {
                    max.map(|m| format!("Select at most {m}"))
                };
            }
            return false;
        };
        if self.current_index >= self.draft_answers.len() {
            self.draft_answers.resize(self.current_index + 1, None);
        }
        self.draft_answers[self.current_index] = Some(answer);
        if let Some(session) = &mut self.new_session {
            if self.current_index == 0 {
                let project = self.confirm_focus;
                let mut directories = session.locations.directories(project);
                let desired = if session.directory_chosen {
                    previous.map(|selection| selection.directory)
                } else if previous
                    .as_ref()
                    .is_some_and(|selection| selection.project != project)
                {
                    session.locations.projects[project].path.clone()
                } else {
                    session.locations.current_directory.clone()
                };
                if session.directory_chosen
                    && let Some(path) = &desired
                    && !directories
                        .iter()
                        .any(|location| location.path.as_ref() == Some(path))
                {
                    directories.push(crate::new_session::Location {
                        path: Some(path.clone()),
                        label: format!("Selected directory · {}", path.display()),
                    });
                }
                let index = directories
                    .iter()
                    .position(|p| p.path == desired)
                    .unwrap_or(0);
                self.draft_answers[1] = directories.get(index).map(|p| FormAnswer::Selected {
                    index,
                    label: p.label.clone(),
                });
                self.questions[1].kind = FormKind::SingleSelect {
                    prompt: "Working directory".into(),
                    options: directories.iter().map(|p| p.label.clone()).collect(),
                };
                session.directories = directories;
            } else {
                session.directory_chosen = true;
            }
        }
        self.error = None;
        true
    }

    fn draft_focus(&self, index: usize) -> usize {
        match self.draft_answers.get(index).and_then(Option::as_ref) {
            Some(FormAnswer::Selected { index, .. }) => *index,
            Some(FormAnswer::Confirmed { value }) => usize::from(!value),
            _ => 0,
        }
    }

    pub fn move_question(&mut self, delta: isize) {
        if self.phase != FormPhase::Editing || self.questions().is_empty() {
            return;
        }
        if !self.commit_current() {
            return;
        }
        let len = self.questions().len() as isize;
        self.current_index = (self.current_index as isize + delta).rem_euclid(len) as usize;
        if self.new_session.is_some() && self.single_choice(self.current_index) {
            self.current_index = (self.current_index as isize + delta).rem_euclid(len) as usize;
        }
        self.confirm_focus = self.draft_focus(self.current_index);
        self.reset_question_state();
        self.scroll = 0;
        self.follow_cursor = true;
        self.clear_geometry();
    }

    pub fn move_cursor(&mut self, delta: isize) {
        let Some(kind) = self.current_kind() else {
            return;
        };
        let len = match kind {
            FormKind::SingleSelect { options, .. } | FormKind::MultiSelect { options, .. } => {
                options.len()
            }
            FormKind::Confirm { .. } => 2,
            FormKind::Text { .. } => return,
        };
        if len > 0 {
            self.confirm_focus =
                (self.confirm_focus as isize + delta).rem_euclid(len as isize) as usize;
            self.error = None;
            self.follow_cursor = true;
        }
    }

    pub fn toggle_current(&mut self) {
        if matches!(self.current_kind(), Some(FormKind::MultiSelect { .. })) {
            if let Some(selected) = self.multi_selected.get_mut(self.confirm_focus) {
                *selected = !*selected;
                self.error = None;
            }
        }
    }

    pub fn submit(&mut self) -> SubmitOutcome {
        if self.is_submitting() {
            return SubmitOutcome::None;
        }
        if self.phase == FormPhase::Editing {
            if self.current_index + 1 < self.questions().len() {
                self.move_question(1);
                if self.new_session.is_none() || !self.single_choice(1) {
                    return SubmitOutcome::None;
                }
                self.current_index = 1;
            }
            if !self.commit_current() {
                return SubmitOutcome::None;
            }
            self.phase = FormPhase::FinalConfirm;
            self.confirm_focus = 0;
            self.clear_geometry();
            self.scroll = 0;
            self.follow_cursor = true;
            return SubmitOutcome::None;
        }
        if self.confirm_focus == 1 {
            return self.reject();
        }
        let Some(form_id) = self.active_form_id().map(str::to_owned) else {
            return SubmitOutcome::None;
        };
        let answers = self.draft_answers.iter().filter_map(Clone::clone).collect();
        let request_id = self.begin_submission();
        SubmitOutcome::Submit {
            request_id,
            form_id,
            submission: FormSubmission::Submitted { answers },
        }
    }

    pub fn reject(&mut self) -> SubmitOutcome {
        if self.is_submitting() {
            return SubmitOutcome::None;
        }
        let Some(form_id) = self.active_form_id().map(str::to_owned) else {
            return SubmitOutcome::None;
        };
        let request_id = self.begin_submission();
        SubmitOutcome::Submit {
            request_id,
            form_id,
            submission: FormSubmission::Rejected,
        }
    }

    pub fn cancel(&mut self) -> SubmitOutcome {
        if !self.open {
            SubmitOutcome::None
        } else {
            self.reject()
        }
    }

    pub fn is_submitting(&self) -> bool {
        self.pending_submission.is_some()
    }

    fn begin_submission(&mut self) -> u64 {
        self.next_submission_id += 1;
        self.pending_submission = Some(self.next_submission_id);
        self.error = None;
        self.next_submission_id
    }

    pub fn submission_result(
        &mut self,
        form_id: &str,
        request_id: u64,
        result: Result<bool, String>,
    ) {
        if self.preference.is_some()
            || self.new_session.is_some()
            || self.active_form_id() != Some(form_id)
            || self.pending_submission != Some(request_id)
        {
            return;
        }
        self.pending_submission = None;
        match result {
            Ok(true) => self.close(),
            Ok(false) => {
                self.error = Some("Form is no longer available or the answer is invalid".into())
            }
            Err(error) => self.error = Some(error),
        }
    }

    fn dispatch_submission(&mut self, outcome: SubmitOutcome, tx: Option<&crate::ControlSender>) {
        if let SubmitOutcome::Submit {
            request_id,
            form_id,
            submission,
        } = outcome
        {
            if self.new_session.is_some() {
                let request = self.new_session.as_ref().unwrap().locations.id;
                let selection = matches!(submission, FormSubmission::Submitted { .. })
                    .then(|| self.session_selection())
                    .flatten();
                let cancelled = matches!(submission, FormSubmission::Rejected);
                let sent = tx
                    .and_then(|tx| {
                        tx.send_tracked(crate::TuiControl::CreateSession { request, selection })
                            .ok()
                    })
                    .flatten();
                if cancelled {
                    self.close();
                } else if let Some(ticket) = sent {
                    self.new_session.as_mut().unwrap().pending = Some(ticket);
                } else {
                    self.pending_submission = None;
                    self.error = Some("Session creation is unavailable".into());
                }
                return;
            }
            if let Some(preference) = self.preference {
                let FormSubmission::Submitted { answers } = submission else {
                    self.close();
                    return;
                };
                let result = match answers.first() {
                    Some(FormAnswer::Selected { index, .. }) => preference.mutation(*index),
                    _ => Err("Setting selection is missing".into()),
                }
                .and_then(|action| crate::settings::SettingsMutationRequest::send(action, tx));
                match result {
                    Ok(request) => self.setting_request = Some(request.id),
                    Err(error) => {
                        self.pending_submission = None;
                        self.error = Some(error);
                    }
                }
                return;
            }
            let sent = tx.is_some_and(|tx| {
                tx.send(crate::TuiControl::FormSubmit {
                    request_id,
                    form_id: form_id.clone(),
                    submission,
                })
                .is_ok()
            });
            if !sent {
                self.submission_result(
                    &form_id,
                    request_id,
                    Err("Form submission is unavailable".into()),
                );
            }
        }
    }

    pub fn handle_mouse(&mut self, event: &MouseEvent, tx: Option<&crate::ControlSender>) {
        if !self.open || self.is_submitting() {
            return;
        }
        let point = (event.column, event.row).into();
        let action = self
            .session_actions
            .iter()
            .find_map(|(index, rect)| rect.contains(point).then_some(*index));
        if event.kind == MouseEventKind::Moved {
            self.hovered_action = action;
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left)
            && let Some(action) = action
        {
            let outcome = match action {
                0 => {
                    self.back_question();
                    SubmitOutcome::None
                }
                1 => self.submit(),
                _ => self.cancel(),
            };
            self.dispatch_submission(outcome, tx);
            return;
        }
        let option = self
            .option_hits
            .iter()
            .find_map(|(index, rect)| rect.contains(point).then_some(*index));
        match event.kind {
            MouseEventKind::Moved => {
                self.hovered_option = option;
                return;
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.scroll = if event.kind == MouseEventKind::ScrollUp {
                    self.scroll.saturating_sub(3)
                } else {
                    self.scroll.saturating_add(3)
                };
                self.follow_cursor = false;
                return;
            }
            MouseEventKind::Down(MouseButton::Left) => {}
            _ => return,
        }
        if let Some(index) = option
            && matches!(
                self.current_kind(),
                Some(FormKind::SingleSelect { .. } | FormKind::MultiSelect { .. })
            )
            && self.phase == FormPhase::Editing
        {
            self.confirm_focus = index;
            self.toggle_current();
            self.error = None;
            return;
        }
        if let Some(rect) = self.last_input_rect
            && rect.contains(point)
            && let Some(kind) = self.current_kind()
        {
            let prompt_rows = visual_line_count(kind.prompt(), rect.width as usize) + 1;
            let row = (event.row - rect.y) as usize + self.scroll as usize;
            if let Some(input_row) = row.checked_sub(prompt_rows) {
                let cursor = crate::input::cursor_from_wrapped(
                    self.text_editor.buf(),
                    input_row,
                    (event.column - rect.x) as usize,
                    rect.width as usize,
                );
                self.text_editor.set_cursor(cursor);
                self.follow_cursor = true;
            }
            return;
        }
        let choice = if self
            .yes_rect
            .is_some_and(|r| r.contains((event.column, event.row).into()))
        {
            Some(0)
        } else if self
            .no_rect
            .is_some_and(|r| r.contains((event.column, event.row).into()))
        {
            Some(1)
        } else {
            None
        };
        if let Some(choice) = choice {
            self.confirm_focus = choice;
            let outcome = self.submit();
            self.dispatch_submission(outcome, tx);
        }
    }

    #[cfg(test)]
    fn attach_test(&mut self, form: PendingForm) {
        self.attach(form);
    }

    fn back_question(&mut self) {
        if self.phase == FormPhase::FinalConfirm {
            self.phase = FormPhase::Editing;
            self.current_index = self.questions.len().saturating_sub(1);
            if self.new_session.is_some()
                && self.single_choice(self.current_index)
                && !self.single_choice(0)
            {
                self.current_index = 0;
            }
            self.confirm_focus = self.draft_focus(self.current_index);
            self.reset_question_state();
            self.clear_geometry();
            self.scroll = 0;
            self.follow_cursor = true;
        } else {
            self.move_question(-1);
        }
    }

    pub(crate) fn content_height(&self, width: u16) -> u16 {
        if let Some(selection) = self.session_selection()
            && self.phase == FormPhase::FinalConfirm
        {
            let session = self.new_session.as_ref().unwrap();
            let project = format!(
                "Project: {}",
                session.locations.projects[selection.project].label
            );
            let directory = format!("Working directory: {}", selection.directory.display());
            return (visual_line_count(&project, width as usize)
                + visual_line_count(&directory, width as usize)
                + 16) as u16;
        }
        self.current_kind()
            .map_or(6, |kind| estimate_height(kind, width))
            + if self.new_session.is_some() { 4 } else { 0 }
    }
}

impl crate::wm::modal::ModalOverlay for FormModal {
    fn render_content(
        &mut self,
        f: &mut ratatui::Frame,
        area: Rect,
        _app: &crate::app::AppState,
        t: &crate::theme::Theme,
    ) {
        let Some(kind) = self.current_kind().cloned() else {
            return;
        };
        let gutter = u16::from(
            !matches!(kind, FormKind::Text { .. }) || self.phase == FormPhase::FinalConfirm,
        )
        .min(area.width / 2);
        self.session_actions.clear();
        let footer = if self.new_session.is_some() {
            4.min(area.height.saturating_sub(3))
        } else {
            0
        };
        let inner = Rect {
            x: area.x + gutter,
            y: area.y.saturating_add(2),
            width: area.width.saturating_sub(gutter * 2),
            height: area.height.saturating_sub(3 + footer),
        };
        self.yes_rect = None;
        self.no_rect = None;
        self.last_input_rect = None;
        self.option_hits.clear();
        let total = self.questions().len().max(1);
        let filled = (area.width as usize * (self.current_index + 1).min(total)) / total;
        let progress_line = Line::from(
            (0..area.width as usize)
                .map(|i| {
                    Span::styled(
                        if i < filled { "━" } else { "·" },
                        Style::default().fg(if i < filled {
                            t.accent.into()
                        } else {
                            t.panel_bg.into()
                        }),
                    )
                })
                .collect::<Vec<_>>(),
        );
        f.render_widget(
            Paragraph::new(progress_line),
            Rect {
                x: area.x,
                y: area.y,
                width: area.width,
                height: 1,
            },
        );
        let hint = if area.width < 60 {
            if self.phase == FormPhase::FinalConfirm {
                "↑↓ choose · Enter · Esc reject"
            } else if matches!(kind, FormKind::MultiSelect { .. }) {
                "↑↓ · Space toggle · Tab · Enter"
            } else {
                "↑↓ · Tab question · Enter next"
            }
        } else if self.phase == FormPhase::FinalConfirm {
            " ↑↓ · choose  enter/y · confirm  n/esc · reject  shift+tab · back "
        } else {
            hint_for(&kind)
        };
        f.render_widget(
            Paragraph::new(hint).alignment(Alignment::Right),
            Rect {
                x: area.x,
                y: area.y + area.height.saturating_sub(1),
                width: area.width,
                height: 1,
            },
        );
        let mut lines = if self.phase == FormPhase::FinalConfirm
            && let Some(session) = &self.new_session
        {
            let selection = self.session_selection().unwrap();
            vec![
                Line::from("New Session"),
                Line::default(),
                Line::from(format!(
                    "Project: {}",
                    session.locations.projects[selection.project].label
                )),
                Line::from(format!(
                    "Working directory: {}",
                    selection.directory.display()
                )),
                Line::default(),
            ]
        } else if self.phase == FormPhase::FinalConfirm {
            vec![Line::from(Span::styled(
                "  Submit all answers?",
                Style::default()
                    .fg(t.tinted_fg.into())
                    .add_modifier(Modifier::BOLD),
            ))]
        } else {
            let mut prompt_lines = kind
                .prompt()
                .split('\n')
                .map(|line| {
                    Line::from(Span::styled(
                        line.to_owned(),
                        Style::default()
                            .fg(t.tinted_fg.into())
                            .add_modifier(Modifier::BOLD),
                    ))
                })
                .collect::<Vec<_>>();
            prompt_lines.push(Line::from(""));
            prompt_lines
        };
        let mut text_height = None;
        let mut option_rows = Vec::new();
        let row_count = |lines: &[Line<'_>]| -> usize {
            lines
                .iter()
                .map(|line| visual_line_count(&line.to_string(), inner.width as usize))
                .sum()
        };
        let mut next_option_row = row_count(&lines);
        let mut push_option = |lines: &mut Vec<Line<'static>>, index, line: Line<'static>| {
            let start = next_option_row;
            next_option_row += visual_line_count(&line.to_string(), inner.width as usize) + 2;
            lines.push(Line::default());
            lines.push(line);
            lines.push(Line::default());
            option_rows.push((index, start, next_option_row));
        };
        if self.phase == FormPhase::FinalConfirm {
            let (yes_label, no_label) = if self.new_session.is_some() {
                ("Create", "Cancel")
            } else {
                ("Yes", "No")
            };
            let yes = if self.confirm_focus == 0 {
                format!("[ {yes_label} ]")
            } else {
                format!("  {yes_label}  ")
            };
            let no = if self.confirm_focus == 1 {
                format!("[ {no_label} ]")
            } else {
                format!("  {no_label}  ")
            };
            for (index, text) in [yes, no].into_iter().enumerate() {
                push_option(&mut lines, index, Line::from(format!("  {text}")));
            }
        } else {
            match &kind {
                FormKind::Confirm { .. } => {
                    let yes = if self.confirm_focus == 0 {
                        "[ Yes ]"
                    } else {
                        "  Yes  "
                    };
                    let no = if self.confirm_focus == 1 {
                        "[ No ]"
                    } else {
                        "  No  "
                    };
                    for (index, text) in [yes, no].into_iter().enumerate() {
                        push_option(&mut lines, index, Line::from(format!("  {text}")));
                    }
                }
                FormKind::SingleSelect { options, .. } | FormKind::MultiSelect { options, .. } => {
                    let is_multi =
                        matches!(self.current_kind(), Some(FormKind::MultiSelect { .. }));
                    for (i, label) in options.iter().enumerate() {
                        push_option(
                            &mut lines,
                            i,
                            Line::from(Span::styled(
                                format!(
                                    "  {}{}",
                                    if is_multi
                                        && self.multi_selected.get(i).copied().unwrap_or(false)
                                    {
                                        "[✓] "
                                    } else if is_multi {
                                        "[ ] "
                                    } else {
                                        ""
                                    },
                                    label
                                ),
                                Style::default().fg(if i == self.confirm_focus {
                                    t.accent.into()
                                } else {
                                    t.tinted_fg.into()
                                }),
                            )),
                        );
                    }
                }
                FormKind::Text { placeholder, .. } => {
                    let mut text = if self.text_editor.buf().is_empty() {
                        placeholder.as_deref().unwrap_or_default().to_owned()
                    } else {
                        self.text_editor.buf().to_owned()
                    };
                    if !self.text_editor.buf().is_empty()
                        && self.text_editor.cursor() == self.text_editor.buf().len()
                        && inner.width > 0
                        && wrapped_cursor_col(
                            self.text_editor.buf(),
                            self.text_editor.cursor(),
                            inner.width as usize,
                        ) >= inner.width as usize
                    {
                        text.push(' ');
                    }
                    text_height = Some(visual_line_count(&text, inner.width as usize));
                    for line in text.split('\n') {
                        lines.push(Line::from(Span::styled(
                            line.to_owned(),
                            Style::default().fg(t.tinted_fg.into()),
                        )));
                    }
                    self.last_input_rect = Some(inner);
                }
            }
        }
        if self.is_submitting() {
            lines.push(Line::from(Span::styled(
                "Submitting…",
                Style::default().fg(t.subtle_fg.into()),
            )));
        }
        if let Some(error) = &self.error {
            lines.push(Line::from(Span::styled(
                format!("! {error}"),
                Style::default().fg(t.error.into()),
            )));
        }
        let height = if let Some(text_height) = text_height {
            visual_line_count(kind.prompt(), inner.width as usize)
                + 1
                + text_height
                + usize::from(self.error.is_some())
                + usize::from(self.is_submitting())
        } else {
            row_count(&lines)
        } as u16;
        self.scroll = self.scroll.min(height.saturating_sub(inner.height));
        if self.follow_cursor && inner.height > 0 && !option_rows.is_empty() {
            if let Some((_, start, end)) = option_rows
                .iter()
                .find(|(index, _, _)| *index == self.confirm_focus)
            {
                if *start < self.scroll as usize {
                    self.scroll = *start as u16;
                } else if *end > self.scroll as usize + inner.height as usize {
                    self.scroll = end.saturating_sub(inner.height as usize) as u16;
                }
            }
            self.follow_cursor = false;
        }
        if self.follow_cursor && self.last_input_rect.is_some() && inner.height > 0 {
            let (input_row, _) = wrapped_cursor_position(
                self.text_editor.buf(),
                self.text_editor.cursor(),
                inner.width as usize,
            );
            let row = visual_line_count(kind.prompt(), inner.width as usize) as u16
                + 1
                + input_row as u16;
            if row < self.scroll {
                self.scroll = row;
            } else if row >= self.scroll.saturating_add(inner.height) {
                self.scroll = row.saturating_sub(inner.height - 1);
            }
            self.follow_cursor = false;
        }
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.scroll, 0)),
            inner,
        );
        for (index, start, end) in option_rows {
            let visible_start = start.max(self.scroll as usize);
            let visible_end = end.min(self.scroll as usize + inner.height as usize);
            if visible_start >= visible_end {
                continue;
            }
            let rect = Rect::new(
                inner.x - gutter,
                inner.y + (visible_start - self.scroll as usize) as u16,
                inner.width + gutter * 2,
                (visible_end - visible_start) as u16,
            );
            let bg = crate::theme::ThemeColor::new(crate::wm::shell::surface_bg(t));
            let bg = if index == self.confirm_focus {
                bg.lerp(t.accent, 0.12)
            } else if self.hovered_option == Some(index) {
                bg.lerp(t.work_hover_bg, 0.45)
            } else {
                crate::wm::shell::surface_bg(t)
            };
            for y in rect.y..rect.bottom() {
                for x in rect.x..rect.right() {
                    f.buffer_mut()[(x, y)].set_bg(bg);
                }
                if index == self.confirm_focus && rect.width > 0 {
                    f.buffer_mut()[(rect.x, y)]
                        .set_symbol("▌")
                        .set_fg(t.accent.into());
                }
            }
            self.option_hits.push((index, rect));
            if self.phase == FormPhase::FinalConfirm || matches!(kind, FormKind::Confirm { .. }) {
                if index == 0 {
                    self.yes_rect = Some(rect);
                } else {
                    self.no_rect = Some(rect);
                }
            }
        }
        if footer > 0 {
            let actions: &[(usize, &str, &str)] = if self.phase == FormPhase::FinalConfirm {
                &[(0, "Back", "Shift+Tab")]
            } else {
                &[
                    (0, "Back", "Shift+Tab"),
                    (1, "Next", "Enter"),
                    (2, "Cancel", "Esc"),
                ]
            };
            let width =
                area.width.saturating_sub((actions.len() as u16 - 1) * 2) / actions.len() as u16;
            for (column, &(index, label, shortcut)) in actions.iter().enumerate() {
                let rect = Rect::new(
                    area.x + column as u16 * (width + 2),
                    inner.bottom() + 1,
                    width,
                    footer.saturating_sub(1),
                );
                crate::settings_form::render_action(
                    f,
                    rect,
                    &crate::settings_form::Action {
                        label,
                        shortcut,
                        hovered: self.hovered_action == Some(index),
                        ..Default::default()
                    },
                    t,
                );
                self.session_actions.push((index, rect));
            }
        }
    }

    fn handle_key(
        &mut self,
        action: &KeyAction,
        _app: &mut crate::app::AppState,
        tx: Option<&crate::ControlSender>,
    ) -> Option<ModalAction> {
        if !self.open {
            return None;
        }
        if self.is_submitting() {
            return Some(ModalAction::Consumed);
        }
        let outcome = match action {
            KeyAction::Escape => Some(self.cancel()),
            KeyAction::Submit => Some(self.submit()),
            KeyAction::Char('y') | KeyAction::Char('Y')
                if self.phase == FormPhase::FinalConfirm =>
            {
                Some(self.submit())
            }
            KeyAction::Char('n') | KeyAction::Char('N')
                if self.phase == FormPhase::FinalConfirm =>
            {
                Some(self.reject())
            }
            KeyAction::Tab if self.phase == FormPhase::Editing => {
                self.move_question(1);
                None
            }
            KeyAction::BackTab if self.phase == FormPhase::FinalConfirm => {
                self.back_question();
                None
            }
            KeyAction::BackTab if self.phase == FormPhase::Editing => {
                self.move_question(-1);
                None
            }
            KeyAction::HistoryUp | KeyAction::Char('k')
                if self.phase == FormPhase::FinalConfirm =>
            {
                self.confirm_focus = 0;
                self.follow_cursor = true;
                None
            }
            KeyAction::HistoryDown | KeyAction::Char('j')
                if self.phase == FormPhase::FinalConfirm =>
            {
                self.confirm_focus = 1;
                self.follow_cursor = true;
                None
            }
            KeyAction::HistoryUp
                if self.phase == FormPhase::Editing
                    && matches!(
                        self.current_kind(),
                        Some(FormKind::Text {
                            multiline: true,
                            ..
                        })
                    ) =>
            {
                self.text_editor.move_line_up_visual(
                    self.last_input_rect.map_or(80, |rect| rect.width as usize),
                );
                self.follow_cursor = true;
                None
            }
            KeyAction::HistoryDown
                if self.phase == FormPhase::Editing
                    && matches!(
                        self.current_kind(),
                        Some(FormKind::Text {
                            multiline: true,
                            ..
                        })
                    ) =>
            {
                self.text_editor.move_line_down_visual(
                    self.last_input_rect.map_or(80, |rect| rect.width as usize),
                );
                self.follow_cursor = true;
                None
            }
            KeyAction::HistoryUp
                if self.phase == FormPhase::Editing
                    && !matches!(self.current_kind(), Some(FormKind::Text { .. })) =>
            {
                self.move_cursor(-1);
                None
            }
            KeyAction::HistoryDown
                if self.phase == FormPhase::Editing
                    && !matches!(self.current_kind(), Some(FormKind::Text { .. })) =>
            {
                self.move_cursor(1);
                None
            }
            KeyAction::Char('k') if !matches!(self.current_kind(), Some(FormKind::Text { .. })) => {
                self.move_cursor(-1);
                None
            }
            KeyAction::Char('j') if !matches!(self.current_kind(), Some(FormKind::Text { .. })) => {
                self.move_cursor(1);
                None
            }
            KeyAction::Char(' ') if !matches!(self.current_kind(), Some(FormKind::Text { .. })) => {
                self.toggle_current();
                None
            }
            KeyAction::PageUp | KeyAction::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(3);
                self.follow_cursor = false;
                None
            }
            KeyAction::PageDown | KeyAction::ScrollDown => {
                self.scroll = self.scroll.saturating_add(3);
                self.follow_cursor = false;
                None
            }
            KeyAction::Newline
                if matches!(
                    self.current_kind(),
                    Some(FormKind::Text {
                        multiline: false,
                        ..
                    })
                ) =>
            {
                None
            }
            KeyAction::Backspace
            | KeyAction::Delete
            | KeyAction::DeleteWordBackward
            | KeyAction::CursorLeft
            | KeyAction::CursorRight
            | KeyAction::CursorHome
            | KeyAction::CursorEnd
            | KeyAction::Char(_)
            | KeyAction::Newline
                if matches!(self.current_kind(), Some(FormKind::Text { .. })) =>
            {
                self.text_editor.handle_key(action);
                self.follow_cursor = true;
                None
            }
            _ => None,
        };
        if let Some(outcome) = outcome {
            self.dispatch_submission(outcome, tx);
        }
        Some(ModalAction::Consumed)
    }

    fn handle_paste(&mut self, text: &str) {
        if !self.open || self.is_submitting() || self.phase != FormPhase::Editing {
            return;
        }
        match self.current_kind() {
            Some(FormKind::Text {
                multiline: true, ..
            }) => self.text_editor.paste_multiline(text),
            Some(FormKind::Text {
                multiline: false, ..
            }) => self.text_editor.paste_single_line(text),
            _ => {}
        }
        self.follow_cursor = true;
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        let rect = self.last_input_rect?;
        let kind = self.current_kind()?;
        let (input_row, col) = wrapped_cursor_position(
            self.text_editor.buf(),
            self.text_editor.cursor(),
            rect.width as usize,
        );
        let row = visual_line_count(kind.prompt(), rect.width as usize) + 1 + input_row;
        let visible_row = row.checked_sub(self.scroll as usize)?;
        (visible_row < rect.height as usize && col < rect.width as usize)
            .then_some((rect.x + col as u16, rect.y + visible_row as u16))
    }
    fn title(&self) -> Line<'static> {
        Line::from(if self.new_session.is_some() {
            " New Session "
        } else {
            " form "
        })
    }
    fn icon(&self) -> &str {
        "📋"
    }
    fn accent(&self, t: &crate::theme::Theme) -> ratatui::style::Color {
        t.accent.into()
    }
}

fn questions(form: &PendingForm) -> Vec<FormQuestion> {
    if form.form.questions.is_empty() {
        vec![FormQuestion {
            id: "question".into(),
            kind: form.kind.clone(),
        }]
    } else {
        form.form.questions.clone()
    }
}

pub fn estimate_height(kind: &FormKind, width: u16) -> u16 {
    let width = width.max(1) as usize;
    let prompt = visual_line_count(kind.prompt(), width) as u16;
    prompt
        + match kind {
            FormKind::Confirm { .. } => 7,
            FormKind::SingleSelect { options, .. } | FormKind::MultiSelect { options, .. } => {
                options
                    .iter()
                    .map(|option| visual_line_count(&format!("  [ ] {option}"), width) + 2)
                    .sum::<usize>() as u16
                    + 2
            }
            FormKind::Text { multiline, .. } => {
                if *multiline {
                    4
                } else {
                    2
                }
            }
        }
}

fn hint_for(kind: &FormKind) -> &'static str {
    match kind {
        FormKind::Confirm { .. } => " ↑↓ · choose  tab/shift+tab · question  enter · next ",
        FormKind::SingleSelect { .. } => " ↑↓ · choose  tab/shift+tab · question  enter · next ",
        FormKind::MultiSelect { .. } => {
            " ↑↓ · choose  space · toggle  tab/shift+tab · question  enter · next "
        }
        FormKind::Text {
            multiline: true, ..
        } => " ←→↑↓ · edit  tab/shift+tab · question  enter · next ",
        FormKind::Text { .. } => " ←→ · edit  tab/shift+tab · question  enter · next ",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wm::modal::ModalOverlay;
    type FlowRunId = atman_rt::RunId<atman_runtime::event::AtmanUuid>;
    #[test]
    fn preferences_keep_configuration_receipts_and_runtime_forms_separate() {
        use crate::settings::{Preference, SettingsMutation, SettingsSource};
        let temp = tempfile::tempdir().unwrap();
        let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(temp.path().join("config"));
        hub.set_math_rendering_enabled(false).unwrap();
        let mut app = crate::app::AppState::default();
        app.settings = SettingsSource::from(hub);
        let mut modal = FormModal::default();
        modal.open_preference(Preference::MathRendering, &mut app);
        assert_eq!(modal.confirm_focus, 1);
        let local_id = modal.active_form_id().unwrap().to_owned();
        let runtime = mk_questions(vec![FormKind::Confirm {
            prompt: "Runtime question".into(),
        }]);
        modal.sync_runtime(Some(&runtime));
        assert_eq!(modal.active_form_id(), Some(local_id.as_str()));
        let (tx, mut rx) = crate::ControlSender::channel();
        modal.handle_key(&KeyAction::Submit, &mut app, Some(&tx));
        // Mouse and keyboard share the same submission and acknowledgement path.
        modal.yes_rect = Some(Rect::new(1, 1, 8, 3));
        modal.handle_mouse(
            &MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 2,
                row: 2,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            Some(&tx),
        );
        let crate::TuiControl::MutateSettings(first) = rx.try_recv().unwrap() else {
            panic!()
        };
        assert!(matches!(
            first.action,
            SettingsMutation::MathRendering { enabled: false }
        ));
        modal.settings_result(uuid::Uuid::now_v7(), &Ok("stale".into()));
        modal.submission_result(&local_id, modal.pending_submission.unwrap(), Ok(true));
        assert!(modal.open && modal.is_submitting());
        modal.settings_result(first.id, &Err("write failed".into()));
        assert_eq!(modal.error.as_deref(), Some("write failed"));
        assert!(!modal.is_submitting());
        assert_eq!(modal.draft_answers.len(), 1);
        modal.handle_key(&KeyAction::Submit, &mut app, Some(&tx));
        let crate::TuiControl::MutateSettings(second) = rx.try_recv().unwrap() else {
            panic!()
        };
        modal.settings_result(first.id, &Ok("old success".into()));
        assert!(modal.is_submitting());
        modal.settings_result(second.id, &Ok("saved".into()));
        assert_eq!(modal.active_form_id(), Some(runtime.form_id.as_str()));
        assert!(modal.preference.is_none());
        modal.sync_runtime(None);
        modal.open_preference(Preference::MathRendering, &mut app);
        modal.sync_runtime(Some(&runtime));
        modal.sync_runtime(None);
        modal.handle_key(&KeyAction::Escape, &mut app, Some(&tx));
        assert!(!modal.open);
        assert!(
            rx.try_recv().is_err(),
            "cancelling a setting must not submit a runtime answer"
        );
    }
    #[test]
    fn new_session_project_changes_keep_an_explicit_directory() {
        use crate::new_session::{Location, Locations};
        let mut modal = FormModal::default();
        modal
            .open_new_session(Locations {
                id: uuid::Uuid::now_v7(),
                projects: vec![
                    Location {
                        path: Some("/a".into()),
                        label: "A".into(),
                    },
                    Location {
                        path: Some("/b".into()),
                        label: "B".into(),
                    },
                ],
                current_directory: Some("/a/sub".into()),
                startup_directory: "/b".into(),
            })
            .unwrap();
        modal.submit();
        modal.move_cursor(2);
        modal.submit();
        assert_eq!(
            modal.session_selection().unwrap().directory,
            std::path::Path::new("/a")
        );
        modal.back_question();
        modal.move_question(-1);
        modal.move_cursor(1);
        modal.submit();
        let selected = modal.session_selection().unwrap();
        assert_eq!(selected.project, 1);
        assert_eq!(selected.directory, std::path::Path::new("/a"));
    }

    #[test]
    fn new_session_form_preserves_choices_and_routes_only_matching_receipts() {
        use crate::control::{ControlSender, ViewAddress, ViewBinding};
        use crate::new_session::{Location, Locations};
        use atman_runtime::event_log::legacy::LegacyFlowKey;
        let id = uuid::Uuid::now_v7();
        let locations = Locations {
            id,
            projects: vec![
                Location {
                    path: Some("/a".into()),
                    label: "Session project · /a".into(),
                },
                Location {
                    path: Some("/b".into()),
                    label: "Startup project · /b".into(),
                },
            ],
            current_directory: Some("/a/sub".into()),
            startup_directory: "/b".into(),
        };
        let mut modal = FormModal::default();
        modal.open_new_session(locations.clone()).unwrap();
        let mut app = crate::app::AppState::default();
        let (host, mut incoming) = tokio::sync::mpsc::unbounded_channel();
        let (tx, _replies) = ControlSender::for_flow(
            ViewBinding {
                view_id: uuid::Uuid::now_v7(),
                revision: 1,
                flow: ViewAddress::Legacy {
                    session: uuid::Uuid::now_v7(),
                    owner: LegacyFlowKey::Default,
                },
            },
            host,
        );
        let runtime = mk_questions(vec![FormKind::Confirm {
            prompt: "Runtime question".into(),
        }]);
        modal.sync_runtime(Some(&runtime));
        assert_eq!(modal.active_form_id(), Some(id.to_string().as_str()));
        modal.move_cursor(1);
        modal.submit();
        assert_eq!(modal.current_index, 1);
        assert_eq!(
            modal.confirm_focus, 1,
            "changing project recommends its directory"
        );
        modal.move_cursor(-1);
        modal.submit();
        assert_eq!(
            modal.session_selection().unwrap().directory,
            std::path::Path::new("/a/sub")
        );
        modal.back_question();
        modal.move_question(-1);
        modal.move_cursor(-1);
        modal.submit();
        assert_eq!(
            modal.session_selection().unwrap().directory,
            std::path::Path::new("/a/sub"),
            "explicit cwd survives a project change"
        );
        modal.submit();
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 35)).unwrap();
        terminal
            .draw(|frame| modal.render_content(frame, frame.area(), &app, &crate::theme::theme()))
            .unwrap();
        let yes = modal.yes_rect.unwrap();
        modal.handle_mouse(
            &MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: yes.x + 2,
                row: yes.y + 1,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            Some(&tx),
        );
        let first = incoming.try_recv().unwrap();
        assert!(
            matches!(first.command, crate::TuiControl::CreateSession { request, selection: Some(_) } if request == id)
        );
        modal.handle_key(&KeyAction::Submit, &mut app, Some(&tx));
        assert!(incoming.try_recv().is_err());
        modal.session_failed(None, id, "stale".into());
        assert!(modal.is_submitting());
        modal.session_failed(Some(&first.ticket), id, "directory unavailable".into());
        assert!(!modal.is_submitting());
        assert_eq!(modal.phase, FormPhase::FinalConfirm);
        assert_eq!(
            modal.session_selection().unwrap().directory,
            std::path::Path::new("/a/sub")
        );
        modal.handle_key(&KeyAction::Submit, &mut app, Some(&tx));
        let retry = incoming.try_recv().unwrap();
        assert!(
            matches!(retry.command, crate::TuiControl::CreateSession { request, .. } if request == id)
        );
        assert!(!modal.owns_session_reply(Some(&first.ticket)));
        assert!(modal.owns_session_reply(Some(&retry.ticket)));
        modal.close();
        assert_eq!(modal.active_form_id(), Some(runtime.form_id.as_str()));
        modal.close();
        let mut one_project = locations;
        one_project.projects.truncate(1);
        modal.open_new_session(one_project).unwrap();
        assert_eq!(modal.current_index, 1);
        modal.handle_key(&KeyAction::Escape, &mut app, Some(&tx));
        assert!(!modal.open);
        assert!(
            matches!(incoming.try_recv().unwrap().command, crate::TuiControl::CreateSession { request, selection: None } if request == id)
        );
    }

    fn mk_questions(kinds: Vec<FormKind>) -> PendingForm {
        PendingForm {
            form_id: "f".into(),
            run_id: FlowRunId::now(),
            tool_use_id: "t".into(),
            kind: kinds[0].clone(),
            form: atman_runtime::form::CompositeForm {
                questions: kinds
                    .into_iter()
                    .enumerate()
                    .map(|(i, kind)| FormQuestion {
                        id: i.to_string(),
                        kind,
                    })
                    .collect(),
            },
            emitted_at: chrono::Utc::now(),
        }
    }
    #[test]
    fn attach_preserves_composite_questions() {
        let mut m = FormModal::default();
        m.attach_test(mk_questions(vec![
            FormKind::Text {
                prompt: "a".into(),
                placeholder: None,
                multiline: false,
            },
            FormKind::Confirm { prompt: "b".into() },
        ]));
        assert_eq!(m.draft_answers.len(), 2);
    }
    #[test]
    fn submissions_keep_drafts_until_the_matching_acknowledgement() {
        let mut modal = FormModal::default();
        let form = mk_questions(vec![FormKind::Confirm {
            prompt: "Proceed?".into(),
        }]);
        modal.attach_test(form.clone());
        modal.submit();
        let SubmitOutcome::Submit {
            request_id: first, ..
        } = modal.submit()
        else {
            panic!("expected submission");
        };
        let draft = modal.draft_answers.clone();
        assert!(modal.open && modal.is_submitting());
        assert!(matches!(modal.submit(), SubmitOutcome::None));
        modal.submission_result("another form", first, Ok(true));
        assert!(modal.is_submitting());
        modal.submission_result(&form.form_id, first, Err("storage unavailable".into()));
        assert!(modal.open && !modal.is_submitting());
        assert_eq!(modal.draft_answers, draft);
        assert_eq!(modal.error.as_deref(), Some("storage unavailable"));

        let SubmitOutcome::Submit {
            request_id: retry, ..
        } = modal.submit()
        else {
            panic!("expected retry");
        };
        modal.submission_result(&form.form_id, first, Ok(true));
        assert!(modal.open && modal.is_submitting());
        modal.submission_result(&form.form_id, retry, Ok(true));
        assert!(!modal.open);

        modal.attach_test(form.clone());
        modal.submit();
        let SubmitOutcome::Submit {
            request_id: next, ..
        } = modal.submit()
        else {
            panic!("expected another interaction");
        };
        modal.submission_result(&form.form_id, retry, Ok(true));
        assert!(modal.open && modal.is_submitting());
        modal.submission_result(&form.form_id, next, Ok(true));
        assert!(!modal.open);
    }

    #[test]
    fn tab_navigates_without_registry_order() {
        let mut m = FormModal::default();
        m.attach_test(mk_questions(vec![
            FormKind::SingleSelect {
                prompt: "a".into(),
                options: vec!["x".into()],
            },
            FormKind::Confirm { prompt: "b".into() },
        ]));
        m.handle_key(&KeyAction::Tab, &mut crate::app::AppState::default(), None);
        assert_eq!(m.current_index, 1);
    }
    #[test]
    fn choices_use_vertical_arrows_and_questions_use_tab() {
        let mut m = FormModal::default();
        m.attach_test(mk_questions(vec![
            FormKind::SingleSelect {
                prompt: "choice".into(),
                options: vec!["x".into(), "y".into()],
            },
            FormKind::Text {
                prompt: "text".into(),
                placeholder: None,
                multiline: false,
            },
        ]));
        let mut app = crate::app::AppState::default();

        m.handle_key(&KeyAction::CursorRight, &mut app, None);
        assert_eq!(m.current_index, 0);
        assert_eq!(m.confirm_focus, 0);
        m.handle_key(&KeyAction::HistoryDown, &mut app, None);
        assert_eq!(m.confirm_focus, 1);
        m.handle_key(&KeyAction::Tab, &mut app, None);
        assert_eq!(m.current_index, 1);
        assert!(matches!(
            &m.draft_answers[0],
            Some(FormAnswer::Selected { index: 1, .. })
        ));

        m.text_editor.replace_with("ab");
        m.handle_key(&KeyAction::CursorLeft, &mut app, None);
        assert_eq!(m.current_index, 1);
        assert_eq!(m.text_editor.cursor(), 1);
        m.handle_key(&KeyAction::HistoryUp, &mut app, None);
        assert_eq!(m.current_index, 1);
        assert_eq!(m.text_editor.cursor(), 1);
        m.handle_key(&KeyAction::BackTab, &mut app, None);
        assert_eq!(m.current_index, 0);
        assert_eq!(m.confirm_focus, 1);
    }

    #[test]
    fn final_yes_submits_all_drafts_once() {
        let mut m = FormModal::default();
        m.attach_test(mk_questions(vec![
            FormKind::SingleSelect {
                prompt: "a".into(),
                options: vec!["x".into()],
            },
            FormKind::Confirm { prompt: "b".into() },
        ]));
        m.submit();
        m.submit();
        assert!(
            matches!(m.submit(), SubmitOutcome::Submit { submission: FormSubmission::Submitted { answers }, .. } if answers.len() == 2)
        );
    }
    #[test]
    fn final_confirmation_keys_control_focus_and_submission() {
        let mut m = FormModal::default();
        m.attach_test(mk_questions(vec![FormKind::Text {
            prompt: "last question".into(),
            placeholder: None,
            multiline: false,
        }]));
        m.submit();
        assert_eq!(m.phase, FormPhase::FinalConfirm);
        m.handle_key(
            &KeyAction::CursorRight,
            &mut crate::app::AppState::default(),
            None,
        );
        assert_eq!(m.confirm_focus, 0);
        m.handle_key(
            &KeyAction::HistoryDown,
            &mut crate::app::AppState::default(),
            None,
        );
        assert_eq!(m.confirm_focus, 1);
        let out = m.handle_key(
            &KeyAction::Submit,
            &mut crate::app::AppState::default(),
            None,
        );
        assert!(matches!(out, Some(ModalAction::Consumed)));
        assert!(m.open);
        assert_eq!(m.error.as_deref(), Some("Form submission is unavailable"));

        let mut m = FormModal::default();
        m.attach_test(mk_questions(vec![FormKind::Text {
            prompt: "last question".into(),
            placeholder: None,
            multiline: false,
        }]));
        m.submit();
        m.handle_key(
            &KeyAction::HistoryDown,
            &mut crate::app::AppState::default(),
            None,
        );
        m.handle_key(
            &KeyAction::HistoryUp,
            &mut crate::app::AppState::default(),
            None,
        );
        assert_eq!(m.confirm_focus, 0);
        assert!(matches!(
            m.submit(),
            SubmitOutcome::Submit {
                submission: FormSubmission::Submitted { .. },
                ..
            }
        ));
    }

    #[test]
    fn backtab_from_final_confirmation_returns_to_last_question() {
        let mut m = FormModal::default();
        m.attach_test(mk_questions(vec![
            FormKind::Text {
                prompt: "first".into(),
                placeholder: None,
                multiline: false,
            },
            FormKind::Text {
                prompt: "last".into(),
                placeholder: None,
                multiline: false,
            },
        ]));
        m.submit();
        m.submit();
        assert_eq!(m.phase, FormPhase::FinalConfirm);
        m.handle_key(
            &KeyAction::BackTab,
            &mut crate::app::AppState::default(),
            None,
        );
        assert_eq!(m.phase, FormPhase::Editing);
        assert_eq!(m.current_index, 1);
        assert_eq!(m.current_kind().unwrap().prompt(), "last");
    }

    #[test]
    fn final_no_and_escape_reject_one_request() {
        for action in [KeyAction::Char('n'), KeyAction::Escape] {
            let mut m = FormModal::default();
            m.attach_test(mk_questions(vec![FormKind::Confirm { prompt: "a".into() }]));
            m.submit();
            let out = if matches!(action, KeyAction::Escape) {
                m.cancel()
            } else {
                m.reject()
            };
            assert!(matches!(
                out,
                SubmitOutcome::Submit {
                    submission: FormSubmission::Rejected,
                    ..
                }
            ));
        }
    }

    #[test]
    fn final_confirmation_backtab_returns_to_last_question() {
        let mut m = FormModal::default();
        m.attach_test(mk_questions(vec![
            FormKind::Confirm {
                prompt: "first".into(),
            },
            FormKind::Confirm {
                prompt: "last".into(),
            },
        ]));
        m.submit();
        m.handle_key(
            &KeyAction::HistoryDown,
            &mut crate::app::AppState::default(),
            None,
        );
        m.submit();
        assert_eq!(m.phase, FormPhase::FinalConfirm);
        assert_eq!(m.current_index, 1);

        m.handle_key(
            &KeyAction::BackTab,
            &mut crate::app::AppState::default(),
            None,
        );
        assert_eq!(m.phase, FormPhase::Editing);
        assert_eq!(m.current_index, 1);
        assert_eq!(m.current_kind().unwrap().prompt(), "last");
        assert_eq!(m.confirm_focus, 1);
    }

    #[test]
    fn final_confirmation_uses_vertical_arrows() {
        for (arrow, expected) in [
            (
                KeyAction::HistoryUp,
                FormSubmission::Submitted {
                    answers: vec![
                        FormAnswer::Confirmed { value: true },
                        FormAnswer::Confirmed { value: true },
                    ],
                },
            ),
            (KeyAction::HistoryDown, FormSubmission::Rejected),
        ] {
            let mut m = FormModal::default();
            m.attach_test(mk_questions(vec![
                FormKind::Confirm {
                    prompt: "first".into(),
                },
                FormKind::Confirm {
                    prompt: "last".into(),
                },
            ]));
            m.submit();
            m.submit();
            let mut app = crate::app::AppState::default();
            m.handle_key(&arrow, &mut app, None);
            let out = m.submit();
            assert!(
                matches!(out, SubmitOutcome::Submit { submission, .. } if submission == expected)
            );
        }
    }

    #[test]
    fn paste_respects_text_field_mode_and_confirmation_phase() {
        use crate::wm::modal::ModalOverlay;

        for (multiline, expected) in [(false, "a b c"), (true, "a\nb\nc")] {
            let mut modal = FormModal::default();
            modal.attach_test(mk_questions(vec![FormKind::Text {
                prompt: "path".into(),
                placeholder: None,
                multiline,
            }]));
            modal.handle_paste("a\r\nb\rc");
            assert_eq!(modal.text_editor.buf(), expected);
            modal.submit();
            modal.handle_paste(" ignored");
            assert_eq!(
                modal.draft_answers,
                vec![Some(FormAnswer::TextEntered {
                    text: expected.into()
                })]
            );
        }
    }

    #[test]
    fn wrapped_options_and_mouse_hits_share_the_visible_rows() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut terminal = Terminal::new(TestBackend::new(40, 24)).unwrap();
        let area = Rect::new(2, 1, 18, 14);
        let app = crate::app::AppState::default();
        let draw = |terminal: &mut Terminal<TestBackend>, modal: &mut FormModal| {
            terminal
                .draw(|frame| modal.render_content(frame, area, &app, &crate::theme::theme()))
                .unwrap();
        };
        for prompt in [
            "line one\nline two",
            "A long prompt that wraps across several rows in the form",
        ] {
            let mut modal = FormModal::default();
            modal.attach_test(mk_questions(vec![FormKind::Confirm {
                prompt: prompt.into(),
            }]));
            draw(&mut terminal, &mut modal);
            let yes = modal.yes_rect.unwrap();
            let row = (yes.y..yes.bottom())
                .find(|&y| {
                    (yes.x..yes.right())
                        .map(|x| terminal.backend().buffer()[(x, y)].symbol())
                        .collect::<String>()
                        .contains("Yes")
                })
                .expect("the Yes hit region must contain its rendered label");
            assert!(
                !modal
                    .no_rect
                    .is_some_and(|no| no.contains((yes.x + 3, row).into()))
            );
            modal.handle_mouse(
                &MouseEvent {
                    kind: MouseEventKind::Down(MouseButton::Left),
                    column: yes.x + 3,
                    row,
                    modifiers: crossterm::event::KeyModifiers::NONE,
                },
                None,
            );
            assert_eq!(
                modal.draft_answers[0],
                Some(FormAnswer::Confirmed { value: true })
            );
        }
        let mut modal = FormModal::default();
        modal.attach_test(mk_questions(vec![FormKind::MultiSelect {
            prompt: "Choose".into(),
            options: (0..8)
                .map(|i| format!("Choice {i} wraps into multiple lines"))
                .collect(),
            min: None,
            max: None,
        }]));
        for _ in 0..6 {
            modal.move_cursor(1);
        }
        draw(&mut terminal, &mut modal);
        assert!(modal.scroll > 0);
        let hit = modal
            .option_hits
            .iter()
            .find(|(index, _)| *index == 6)
            .unwrap()
            .1;
        modal.handle_mouse(
            &MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: hit.x + 1,
                row: hit.y,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            None,
        );
        assert!(modal.multi_selected[6]);
        assert_eq!(modal.multi_selected.iter().filter(|&&v| v).count(), 1);
    }

    #[test]
    fn mouse_yes_submits_confirm_form() {
        let mut modal = FormModal::default();
        modal.attach_test(mk_questions(vec![FormKind::Confirm {
            prompt: "Implement?".into(),
        }]));
        let (tx, mut rx) = crate::ControlSender::channel();
        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 4,
            row: 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        modal.yes_rect = Some(Rect::new(2, 3, 7, 1));
        modal.handle_mouse(&click, Some(&tx));
        assert_eq!(modal.phase, FormPhase::FinalConfirm);
        assert!(rx.try_recv().is_err());
        modal.handle_mouse(&click, Some(&tx));
        assert!(
            modal.open,
            "a stale click must not submit an unseen confirmation"
        );
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(40, 20)).unwrap();
        terminal
            .draw(|frame| {
                modal.render_content(
                    frame,
                    frame.area(),
                    &crate::app::AppState::default(),
                    &crate::theme::theme(),
                )
            })
            .unwrap();
        let yes = modal.yes_rect.unwrap();
        let click = MouseEvent {
            column: yes.x + 2,
            row: yes.y + 1,
            ..click
        };
        modal.handle_mouse(&click, Some(&tx));
        assert!(modal.open && modal.is_submitting());
        assert!(matches!(
            rx.try_recv(),
            Ok(crate::TuiControl::FormSubmit {
                submission: FormSubmission::Submitted { answers },
                ..
            }) if answers == vec![FormAnswer::Confirmed { value: true }]
        ));
    }

    #[test]
    fn text_cursor_tracks_newlines_wrapping_and_scroll() {
        use ratatui::{Terminal, backend::TestBackend};

        let mut modal = FormModal::default();
        modal.attach_test(mk_questions(vec![FormKind::Text {
            prompt: "path".into(),
            placeholder: None,
            multiline: true,
        }]));
        let mut terminal = Terminal::new(TestBackend::new(20, 10)).unwrap();
        let app = crate::app::AppState::default();
        let area = Rect::new(2, 1, 8, 6);
        let mut draw = |modal: &mut FormModal| {
            terminal
                .draw(|frame| modal.render_content(frame, area, &app, &crate::theme::theme()))
                .unwrap();
        };

        modal.text_editor.replace_with("one\ntwo");
        draw(&mut modal);
        assert_eq!(modal.cursor_position(), Some((5, 5)));
        modal.handle_key(
            &KeyAction::HistoryUp,
            &mut crate::app::AppState::default(),
            None,
        );
        draw(&mut modal);
        assert_eq!(modal.cursor_position(), Some((5, 4)));
        modal.handle_key(
            &KeyAction::HistoryDown,
            &mut crate::app::AppState::default(),
            None,
        );
        draw(&mut modal);
        assert_eq!(modal.cursor_position(), Some((5, 5)));

        modal.text_editor.replace_with("hello world");
        modal.follow_cursor = true;
        draw(&mut modal);
        assert_eq!(modal.cursor_position(), Some((7, 5)));

        modal.text_editor.replace_with("abcdefgh");
        modal.follow_cursor = true;
        draw(&mut modal);
        assert_eq!(modal.cursor_position(), Some((2, 5)));

        modal.text_editor.replace_with("one\ntwo\nthree\nfour");
        modal.follow_cursor = true;
        draw(&mut modal);
        assert_eq!(modal.scroll, 3);
        assert_eq!(modal.cursor_position(), Some((6, 5)));
    }

    #[test]
    fn single_line_form_rejects_keyboard_newline() {
        let mut modal = FormModal::default();
        modal.attach_test(mk_questions(vec![FormKind::Text {
            prompt: "path".into(),
            placeholder: None,
            multiline: false,
        }]));
        modal.text_editor.replace_with("/tmp/project");
        modal.handle_key(
            &KeyAction::Newline,
            &mut crate::app::AppState::default(),
            None,
        );
        assert_eq!(modal.text_editor.buf(), "/tmp/project");
        modal.handle_key(
            &KeyAction::Char(' '),
            &mut crate::app::AppState::default(),
            None,
        );
        assert_eq!(modal.text_editor.buf(), "/tmp/project ");
    }
}
