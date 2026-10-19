//! Submission ownership retained by the originating view until host acknowledgement.

use std::{sync::Arc, time::Duration};

use crate::{
    app::{AppState, NoteLevel, ToastPosition},
    control::ControlTicket,
    input::{InputEditor, PreparedSubmission},
};
use atman_runtime::{Session, message::ImageSource};

pub(crate) struct PendingSubmission {
    pub ticket: ControlTicket,
    pub draft: PreparedSubmission,
    pub quote: Option<Arc<str>>,
    pub consumes_quote: bool,
    pub images: Vec<atman_runtime::message::ImageSource>,
}

impl AppState {
    pub(crate) fn composer_attachment_command(&mut self, editor: &mut InputEditor) -> bool {
        use atman_runtime::meta_commands::AttachmentCommand;
        let draft = editor.prepare_submission();
        let Some(command) = AttachmentCommand::from_line(&draft.content().text) else {
            return false;
        };
        let (message, removed) = match command {
            Ok(AttachmentCommand::Add(_)) => return false,
            Err(error) => {
                self.push_note(error, NoteLevel::Error);
                return true;
            }
            Ok(AttachmentCommand::Clear) => (
                "Pending attachments cleared".into(),
                draft.content().images.clone(),
            ),
            Ok(AttachmentCommand::List) => {
                let names = editor
                    .pending_images()
                    .iter()
                    .map(|image| format!("{}: {}", image.number, image.name))
                    .collect::<Vec<_>>();
                (
                    if names.is_empty() {
                        "No pending attachments".into()
                    } else {
                        names.join("\n")
                    },
                    vec![],
                )
            }
        };
        self.acknowledge_composer(editor, draft, true, &removed);
        self.push_note(message, NoteLevel::Info);
        true
    }

    pub(crate) fn attachment_imported(
        &mut self,
        editor: &mut InputEditor,
        ticket: Option<&ControlTicket>,
        result: Result<ImageSource, String>,
    ) {
        if !self
            .pending_submission
            .as_ref()
            .is_some_and(|pending| Some(&pending.ticket) == ticket)
        {
            return;
        }
        match result {
            Err(error) => self.submission_result(editor, ticket, Err(error)),
            Ok(source) => {
                self.submission_result(editor, ticket, Ok(()));
                let number = self.attach_composer_image(editor, source);
                self.push_note(
                    format!("Attached image {number} ({} pending)", self.attach_count),
                    NoteLevel::Info,
                );
            }
        }
    }

    pub(crate) fn prepare_composer_submission(
        &self,
        draft: &PreparedSubmission,
    ) -> crate::TuiSubmission {
        let content = draft.content();
        let presentation = self.pending_quote.as_ref().map(|quote| {
            atman_runtime::user_input::UserInputPresentation {
                prompt: content.text.clone(),
                quote: Some(atman_runtime::user_input::QuoteSnapshot {
                    text: quote.to_string(),
                }),
            }
        });
        let mut submission = crate::TuiSubmission {
            text: presentation
                .as_ref()
                .map_or_else(|| content.text.clone(), |value| value.model_text()),
            images: content.images.clone(),
            reasoning: self.input_reasoning_for_submission(),
            presentation,
        };
        if let Some(command) = submission.control_command() {
            submission = crate::TuiSubmission {
                text: command.to_owned(),
                images: vec![],
                reasoning: None,
                presentation: None,
            };
        }
        submission
    }

    fn legacy_attachment_session(&self) -> Option<&Session> {
        self.session
            .as_deref()
            .filter(|session| session.flow_instance().is_none())
    }

    /// Persistent views own their drafts; legacy hosts still expose an attachment queue.
    pub(crate) fn sync_composer_images(&mut self, editor: &mut InputEditor) {
        if let Some(session) = self.legacy_attachment_session() {
            editor.reconcile_images(&session.pending_images());
        }
        self.attach_count = editor.pending_images().len();
    }

    pub(crate) fn attach_composer_image(
        &mut self,
        editor: &mut InputEditor,
        source: ImageSource,
    ) -> u32 {
        if let Some(session) = self.legacy_attachment_session() {
            session.queue_image_source(source.clone());
        }
        let number = editor.attach_image(source);
        self.sync_composer_images(editor);
        number
    }

    pub(crate) fn remove_composer_images(
        &mut self,
        editor: &mut InputEditor,
        images: &[ImageSource],
    ) {
        if let Some(session) = self.legacy_attachment_session() {
            for image in images {
                session.remove_pending_image(image);
            }
        }
        self.sync_composer_images(editor);
    }

    pub(crate) fn prune_composer_images(&mut self, editor: &mut InputEditor) {
        let removed = editor.prune_missing_image_references();
        self.remove_composer_images(editor, &removed);
    }

    pub(crate) fn acknowledge_composer(
        &mut self,
        editor: &mut InputEditor,
        draft: PreparedSubmission,
        clear: bool,
        submitted_images: &[ImageSource],
    ) -> bool {
        let mut retained = draft.content().images.clone();
        for image in submitted_images {
            if let Some(index) = retained.iter().position(|pending| pending == image) {
                retained.remove(index);
            }
        }
        if !editor.acknowledge_submission_if(draft, clear) {
            return false;
        }
        editor.reconcile_images(&retained);
        self.remove_composer_images(editor, submitted_images);
        true
    }

    pub(crate) fn submission_result(
        &mut self,
        editor: &mut InputEditor,
        ticket: Option<&ControlTicket>,
        result: Result<(), String>,
    ) {
        if !self
            .pending_submission
            .as_ref()
            .is_some_and(|pending| Some(&pending.ticket) == ticket)
        {
            return;
        }
        let pending = self.pending_submission.take().unwrap();
        match result {
            Err(error) => self.push_toast(
                format!("Input not accepted: {error}"),
                NoteLevel::Error,
                Duration::from_secs(8),
                ToastPosition::BottomRight,
            ),
            Ok(()) => {
                let quote_matches = match (&pending.quote, &self.pending_quote) {
                    (Some(old), Some(current)) => Arc::ptr_eq(old, current),
                    (None, None) => true,
                    _ => false,
                };
                if self.acknowledge_composer(editor, pending.draft, quote_matches, &pending.images)
                {
                    if pending.consumes_quote {
                        self.pending_quote = None;
                    }
                } else {
                    self.push_toast(
                        "Input accepted; newer draft retained",
                        NoteLevel::Info,
                        Duration::from_secs(4),
                        ToastPosition::BottomRight,
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::ViewBinding;
    use atman_runtime::flow_store::{FlowInstanceId, FlowRef};
    use uuid::Uuid;

    #[test]
    fn native_images_follow_draft_and_ack_ownership() {
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let session = Arc::new(Session::open_instance_context(flow.clone()));
        let mut app = AppState::new("session".into(), None).with_session(Some(session.clone()));
        let mut editor = InputEditor::default();
        let image = session
            .import_image_bytes(b"\x89PNG\r\n\x1a\n", None)
            .unwrap();
        editor.insert_str(":status");
        let quote: Arc<str> = "pending quote".into();
        app.pending_quote = Some(quote.clone());
        app.attach_composer_image(&mut editor, image.clone());
        // Control commands do not consume images that were not submitted.
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: flow.into(),
            },
            request_id: Uuid::now_v7(),
        };
        let pending = |editor: &InputEditor, images| PendingSubmission {
            ticket: ticket.clone(),
            draft: editor.prepare_submission(),
            quote: Some(quote.clone()),
            consumes_quote: false,
            images,
        };
        app.pending_submission = Some(pending(&editor, vec![]));
        app.submission_result(&mut editor, Some(&ticket), Ok(()));
        assert!(!editor.buf().contains(":status"));
        assert_eq!(editor.pending_images()[0].source, image);
        assert_eq!(app.attach_count, 1);
        assert_eq!(session.pending_image_count(), 0);
        assert!(Arc::ptr_eq(app.pending_quote.as_ref().unwrap(), &quote));
        app.pending_submission = Some(pending(&editor, vec![image.clone()]));
        app.submission_result(&mut editor, Some(&ticket), Err("disk full".into()));
        assert_eq!(editor.pending_images().len(), 1);
        assert!(Arc::ptr_eq(app.pending_quote.as_ref().unwrap(), &quote));
        app.pending_submission = Some(pending(&editor, vec![image.clone()]));
        editor.insert_str("new draft");
        app.submission_result(&mut editor, Some(&ticket), Ok(()));
        assert!(editor.buf().contains("new draft"));
        assert_eq!(editor.pending_images().len(), 1);
        app.pending_submission = Some(pending(&editor, vec![image.clone()]));
        app.submission_result(&mut editor, Some(&ticket), Ok(()));
        assert!(editor.buf().is_empty());
        assert!(editor.pending_images().is_empty());
        assert_eq!(app.attach_count, 0);

        editor.insert_str(":attach image.png");
        app.pending_submission = Some(pending(&editor, vec![]));
        let mut wrong_ticket = ticket.clone();
        wrong_ticket.request_id = Uuid::now_v7();
        app.attachment_imported(&mut editor, Some(&wrong_ticket), Ok(image.clone()));
        assert!(editor.pending_images().is_empty());
        app.attachment_imported(&mut editor, Some(&ticket), Err("cannot read image".into()));
        assert_eq!(editor.buf(), ":attach image.png");
        app.pending_submission = Some(pending(&editor, vec![]));
        editor.replace_with("newer draft");
        app.attachment_imported(&mut editor, Some(&ticket), Ok(image.clone()));
        assert!(editor.buf().contains("newer draft"));
        assert_eq!(editor.pending_images().len(), 1);
        app.attachment_imported(&mut editor, Some(&ticket), Ok(image.clone()));
        assert_eq!(editor.pending_images().len(), 1);
        assert!(Arc::ptr_eq(app.pending_quote.as_ref().unwrap(), &quote));
        editor.replace_with(":attach list");
        assert!(app.composer_attachment_command(&mut editor));
        assert_eq!(editor.pending_images().len(), 1);
        editor.insert_str(":attach clear");
        assert!(app.composer_attachment_command(&mut editor));
        assert!(editor.pending_images().is_empty());
        assert_eq!(app.attach_count, 0);
        assert_eq!(session.pending_image_count(), 0);
        assert!(Arc::ptr_eq(app.pending_quote.as_ref().unwrap(), &quote));
    }

    #[test]
    fn native_input_ack_preserves_ownership_and_newer_drafts() {
        let mut app = AppState::new("session".into(), None);
        let mut editor = InputEditor::default();
        editor.insert_str("original");
        app.pending_quote = Some("quote".into());
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: FlowRef {
                    session_id: Uuid::now_v7().into(),
                    instance_id: FlowInstanceId::now(),
                }
                .into(),
            },
            request_id: Uuid::now_v7(),
        };
        let pending = |editor: &InputEditor, app: &AppState| PendingSubmission {
            ticket: ticket.clone(),
            draft: editor.prepare_submission(),
            quote: app.pending_quote.clone(),
            consumes_quote: true,
            images: vec![],
        };
        app.pending_submission = Some(pending(&editor, &app));
        let mut stale = ticket.clone();
        stale.request_id = Uuid::now_v7();
        app.submission_result(&mut editor, Some(&stale), Ok(()));
        assert!(app.pending_submission.is_some());
        assert_eq!(editor.buf(), "original");
        app.submission_result(&mut editor, Some(&ticket), Err("rejected".into()));
        assert!(app.pending_submission.is_none());
        assert_eq!(editor.buf(), "original");
        assert_eq!(app.pending_quote.as_deref(), Some("quote"));

        app.pending_submission = Some(pending(&editor, &app));
        // Equal text is still a newly selected quote, not the original submission.
        app.pending_quote = Some("quote".into());
        app.submission_result(&mut editor, Some(&ticket), Ok(()));
        assert_eq!(editor.buf(), "original");
        assert!(app.pending_quote.is_some());
        app.pending_submission = Some(pending(&editor, &app));
        editor.insert_str(" edited");
        app.submission_result(&mut editor, Some(&ticket), Ok(()));
        assert_eq!(editor.buf(), "original edited");
        app.pending_submission = Some(pending(&editor, &app));
        app.submission_result(&mut editor, Some(&ticket), Ok(()));
        assert!(editor.buf().is_empty());
        assert!(app.pending_quote.is_none());
        editor.insert_str("next");
        app.submission_result(&mut editor, Some(&ticket), Ok(()));
        assert_eq!(editor.buf(), "next");
    }
}
