//! Submission ownership retained by the originating view until host acknowledgement.

use std::{sync::Arc, time::Duration};

use crate::{
    app::{AppState, NoteLevel, ToastPosition},
    control::ControlTicket,
    input::{InputEditor, PreparedSubmission},
};

pub(crate) struct PendingSubmission {
    pub ticket: ControlTicket,
    pub draft: PreparedSubmission,
    pub quote: Option<Arc<str>>,
    pub images: Vec<atman_runtime::message::ImageSource>,
}

impl AppState {
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
                if editor.acknowledge_submission_if(pending.draft, quote_matches) {
                    self.pending_quote = None;
                    if let Some(session) = self.session.as_ref() {
                        for image in &pending.images {
                            session.remove_pending_image(image);
                        }
                        editor.reconcile_images(&session.pending_images());
                        self.attach_count = session.pending_image_count();
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
                },
            },
            request_id: Uuid::now_v7(),
        };
        let pending = |editor: &InputEditor, app: &AppState| PendingSubmission {
            ticket: ticket.clone(),
            draft: editor.prepare_submission(),
            quote: app.pending_quote.clone(),
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
