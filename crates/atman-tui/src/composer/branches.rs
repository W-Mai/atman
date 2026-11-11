//! View-local drafts follow installed branches; accepted work retains its original receipt owner.

use std::{collections::HashMap, sync::Arc};

use atman_runtime::{
    flow_store::{FlowRef, source_capture::ConversationBranchId},
    provider::ReasoningSelection,
};

use super::PendingSubmission;
use crate::{app::AppState, input::InputEditor};

type BranchKey = (FlowRef, ConversationBranchId);

#[derive(Default)]
pub(crate) struct HistoryDrafts {
    current: Option<BranchKey>,
    saved: HashMap<BranchKey, Draft>,
}

#[derive(Default)]
pub(super) struct Draft {
    pub(super) editor: InputEditor,
    quote: Option<Arc<str>>,
    pub(super) pending: Option<PendingSubmission>,
    reasoning: Option<ReasoningSelection>,
}

impl HistoryDrafts {
    pub(crate) fn retains_submission(&self) -> bool {
        self.saved.values().any(|draft| draft.pending.is_some())
    }

    pub(super) fn editors(&self) -> impl Iterator<Item = &InputEditor> {
        self.saved.values().map(|draft| &draft.editor)
    }
}

impl Draft {
    fn swap(&mut self, app: &mut AppState, editor: &mut InputEditor) {
        editor.swap_draft(&mut self.editor);
        std::mem::swap(&mut app.pending_quote, &mut self.quote);
        std::mem::swap(&mut app.pending_submission, &mut self.pending);
        std::mem::swap(&mut app.input_reasoning, &mut self.reasoning);
    }
}

impl AppState {
    /// A prepared replacement can change drafts only after its complete document was installed.
    pub(crate) fn sync_history_draft(&mut self, editor: &mut InputEditor) {
        let Some(snapshot) = self.items.snapshot() else {
            return;
        };
        let Some(head) = &snapshot.status.history_head else {
            return;
        };
        let key = (snapshot.status.execution.address().clone(), head.branch);
        self.select_history_draft(editor, key);
    }

    fn select_history_draft(&mut self, editor: &mut InputEditor, key: BranchKey) {
        if self.history_drafts.current.as_ref() == Some(&key) {
            return;
        }
        let previous = self.history_drafts.current.replace(key.clone());
        let Some(previous) = previous else { return };
        let mut old = Draft {
            reasoning: self.input_reasoning.clone(),
            ..Draft::default()
        };
        old.swap(self, editor);
        if let Some(mut selected) = self.history_drafts.saved.remove(&key) {
            selected.swap(self, editor);
        }
        self.history_drafts.saved.insert(previous, old);
        self.sync_composer_images(editor);
        self.input = editor.buf().to_owned();
        self.quote_rect = None;
        self.quote_close_hovered = false;
        self.quote_expanded = false;
        self.quote_scroll = 0;
        self.input_controls.clear();
        self.input_hover = None;
        self.refresh_popup(editor.buf());
        self.mark_visual_dirty();
    }

    /// Route a late result through the original composer handlers without changing visible focus.
    pub(super) fn with_saved_draft(
        &mut self,
        editor: &mut InputEditor,
        matches: impl Fn(&Draft) -> bool,
        apply: impl FnOnce(&mut Self, &mut InputEditor),
    ) -> bool {
        let Some(key) = self
            .history_drafts
            .saved
            .iter()
            .find_map(|(key, draft)| matches(draft).then(|| key.clone()))
        else {
            return false;
        };
        let mut saved = self
            .history_drafts
            .saved
            .remove(&key)
            .expect("located draft");
        saved.swap(self, editor);
        apply(self, editor);
        saved.swap(self, editor);
        self.history_drafts.saved.insert(key, saved);
        self.sync_composer_images(editor);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::control::{ControlTicket, ViewBinding};
    use atman_runtime::{
        flow_store::{FlowInstanceId, branches::HistoryHead},
        message::{ImageData, ImageSource},
        provider::ImageDetail,
    };
    use uuid::Uuid;

    #[tokio::test]
    async fn saved_branch_drafts_receive_their_own_acknowledgements_and_images() {
        let flow = FlowRef {
            session_id: Uuid::now_v7().into(),
            instance_id: FlowInstanceId::now(),
        };
        let first = (flow.clone(), ConversationBranchId::random());
        let second = (flow.clone(), ConversationBranchId::random());
        let mut app = AppState::new("drafts".into(), None);
        let mut editor = InputEditor::default();
        app.select_history_draft(&mut editor, first.clone());
        editor.insert_str("accepted on first branch");
        app.pending_quote = Some("first quote".into());
        app.input_reasoning = Some(ReasoningSelection::Disabled);
        let ticket = ControlTicket {
            binding: ViewBinding {
                view_id: Uuid::now_v7(),
                revision: 1,
                flow: flow.clone().into(),
            },
            request_id: Uuid::now_v7(),
            history_head: Some(HistoryHead {
                branch: first.1,
                version: 0.into(),
            }),
        };
        app.pending_submission = Some(PendingSubmission {
            ticket: ticket.clone(),
            draft: editor.prepare_submission(),
            quote: app.pending_quote.clone(),
            consumes_quote: true,
            images: vec![],
        });
        app.select_history_draft(&mut editor, second.clone());
        assert!(editor.buf().is_empty());
        assert!(app.pending_submission.is_none());
        assert!(app.pending_quote.is_none());
        assert!(app.history_drafts.retains_submission());
        editor.insert_str("second branch draft");
        let second_quote: Arc<str> = "second quote".into();
        app.pending_quote = Some(second_quote.clone());
        app.input_reasoning = None;
        app.submission_result(&mut editor, Some(&ticket), Ok(()));
        assert!(!app.history_drafts.retains_submission());
        assert_eq!(editor.buf(), "second branch draft");
        assert!(Arc::ptr_eq(
            app.pending_quote.as_ref().unwrap(),
            &second_quote
        ));
        app.select_history_draft(&mut editor, first.clone());
        assert!(editor.buf().is_empty());
        assert!(app.pending_submission.is_none());
        assert!(app.pending_quote.is_none());
        assert_eq!(app.input_reasoning, Some(ReasoningSelection::Disabled));
        editor.ingest_paste(&"原始正文\n".repeat(20));
        let (image_request, _) = editor.reserve_image();
        app.sync_composer_images(&mut editor);
        assert!(app.composer_images_preparing);
        let visible = editor.buf().to_owned();
        let folded = editor.prepare_submission().content().text.clone();
        let first_quote: Arc<str> = "retained first quote".into();
        app.pending_quote = Some(first_quote.clone());
        app.select_history_draft(&mut editor, second.clone());
        assert!(!app.composer_images_preparing);
        app.prune_composer_images(&mut editor);
        let image = ImageSource {
            media_type: "image/png".into(),
            detail: ImageDetail::Auto,
            data: ImageData::Base64 {
                data: "aW1hZ2U=".into(),
            },
        };
        app.clipboard_image_ready(&mut editor, image_request, Ok(Some(image.clone())));
        assert_eq!(editor.buf(), "second branch draft");
        assert_eq!(app.attach_count, 0);
        assert_eq!(app.input_reasoning, None);
        app.select_history_draft(&mut editor, first.clone());
        assert_eq!(editor.buf(), visible);
        assert_eq!(editor.prepare_submission().content().text, folded);
        assert_eq!(editor.pending_images()[0].source(), Some(&image));
        assert!(Arc::ptr_eq(
            app.pending_quote.as_ref().unwrap(),
            &first_quote
        ));
        let rejected = ControlTicket {
            request_id: Uuid::now_v7(),
            ..ticket
        };
        app.pending_submission = Some(PendingSubmission {
            ticket: rejected.clone(),
            draft: editor.prepare_submission(),
            quote: app.pending_quote.clone(),
            consumes_quote: true,
            images: vec![image],
        });
        app.select_history_draft(&mut editor, second.clone());
        app.submission_result(&mut editor, Some(&rejected), Err("history changed".into()));
        app.select_history_draft(&mut editor, first);
        assert_eq!(editor.buf(), visible);
        assert!(app.pending_submission.is_none());
        assert_eq!(editor.pending_images().len(), 1);
        // A duplicate old receipt cannot clear this recovered draft.
        app.submission_result(&mut editor, Some(&rejected), Ok(()));
        assert_eq!(editor.buf(), visible);
        app.select_history_draft(&mut editor, second);
        assert_eq!(editor.buf(), "second branch draft");
    }
}
