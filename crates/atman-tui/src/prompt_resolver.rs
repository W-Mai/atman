pub use atman_runtime::rendezvous::FormPromptResolver as TuiPromptResolver;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wm::modal::ModalOverlay;
    use atman_runtime::form::{FormAnswer, FormKind, FormSubmission};
    use atman_runtime::rendezvous::{PromptId, PromptResolver};
    use atman_runtime::session::FormRegistry;
    use std::sync::Arc;

    #[tokio::test]
    async fn single_confirm_returns_submission_not_bare_answer() {
        let forms = Arc::new(FormRegistry::new());
        let _watch = forms.subscribe();
        let resolver = TuiPromptResolver::new(Arc::clone(&forms));
        let id = PromptId::now();
        let rx = resolver.register_with_payload(
            id,
            "form_ask",
            serde_json::to_value(FormKind::Confirm {
                prompt: "Proceed?".into(),
            })
            .unwrap(),
        );
        let mut modal = crate::form_modal::FormModal::default();
        modal.attach(forms.list_pending().remove(0));
        let mut app = crate::app::AppState::default();
        let (control_tx, mut control_rx) = tokio::sync::mpsc::unbounded_channel();
        modal.handle_key(&crate::keys::KeyAction::Submit, &mut app, Some(&control_tx));
        modal.handle_key(&crate::keys::KeyAction::Submit, &mut app, Some(&control_tx));
        let crate::TuiControl::FormSubmit {
            form_id,
            submission,
        } = control_rx.try_recv().unwrap()
        else {
            panic!("expected form submission");
        };
        assert!(forms.submit(&form_id, submission));
        let response: FormSubmission = serde_json::from_value(rx.await.unwrap()).unwrap();
        assert_eq!(
            response,
            FormSubmission::Submitted {
                answers: vec![FormAnswer::Confirmed { value: true }],
            }
        );
    }
}
