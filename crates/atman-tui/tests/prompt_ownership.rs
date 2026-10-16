use std::sync::Arc;

use atman_runtime::form::{FormAnswer, FormKind, FormSubmission};
use atman_runtime::rendezvous::{PromptId, PromptResolver};
use atman_runtime::{Session, tool::ToolCtx};
use atman_tui::prompt_resolver::TuiPromptResolver;

#[tokio::test]
async fn shared_display_preserves_invocation_identity_and_late_answer_destination() {
    let display = Arc::new(Session::open_ephemeral());
    let first = Arc::new(Session::open_ephemeral());
    let second = Arc::new(Session::open_ephemeral());
    let forms = display.forms();
    let _watch = forms.subscribe();
    let resolver: Arc<dyn PromptResolver> = Arc::new(TuiPromptResolver::new(forms.clone()));
    let mut pending = Vec::new();
    for session in [&first, &second] {
        let mut ctx = ToolCtx::new().with_session_runtime(session.clone());
        ctx.prompt_resolver = Some(resolver.clone());
        ctx.flow_run_id = Some(atman_rt::RunId::now());
        ctx.current_node_id = Some("form-node".into());
        ctx.tool_use_id = Some("form-call".into());
        let id = PromptId::now();
        let scoped = ctx.prompt_resolver().unwrap();
        let rx = scoped.register_with_payload(
            id,
            "form_ask",
            serde_json::to_value(FormKind::Confirm {
                prompt: "Proceed?".into(),
            })
            .unwrap(),
        );
        let item = forms
            .list_pending()
            .into_iter()
            .find(|form| form.form_id == format!("prompt_{id}"))
            .unwrap();
        assert_eq!(Some(item.run_id), ctx.flow_run_id);
        assert_eq!(item.tool_use_id, "form-call");
        pending.push((id, scoped, rx));
    }
    let (first_id, first_resolver, first_rx) = pending.remove(0);
    assert!(first_resolver.expire_pending(&first_id));
    assert!(first_rx.await.is_err());
    let submitted = FormSubmission::Submitted {
        answers: vec![FormAnswer::Confirmed { value: true }],
    };
    let (second_id, _, second_rx) = pending.remove(0);
    assert!(forms.submit(&format!("prompt_{second_id}"), submitted.clone()));
    assert_eq!(
        serde_json::from_value::<FormSubmission>(second_rx.await.unwrap()).unwrap(),
        submitted
    );
    assert!(forms.submit(&format!("prompt_{first_id}"), submitted.clone()));
    assert!(!forms.submit(&format!("prompt_{first_id}"), submitted));
    assert_eq!(first.deferred_form_inbox().pending_count(), 1);
    assert_eq!(second.deferred_form_inbox().pending_count(), 0);
    assert_eq!(display.deferred_form_inbox().pending_count(), 0);
    assert!(forms.list_pending().is_empty());
}
