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

#[tokio::test]
async fn language_confirmation_accepts_the_same_submission_from_both_transports() {
    let file =
        atman_rt::parse_file(r#"flow main() -> bool { return user_confirm("Proceed?") }"#).unwrap();
    for via_resolver in [true, false] {
        for answer in [Some(true), Some(false), None] {
            let session = Arc::new(Session::open_ephemeral());
            let forms = session.forms();
            let mut updates = forms.subscribe();
            let mut executor = atman_runtime::Executor::new();
            if via_resolver {
                executor.tool_ctx.prompt_resolver =
                    Some(Arc::new(TuiPromptResolver::new(forms.clone())));
            }
            let submission = match answer {
                Some(value) => FormSubmission::Submitted {
                    answers: vec![FormAnswer::Confirmed { value }],
                },
                None => FormSubmission::Rejected,
            };
            let (_, result) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
                tokio::join!(
                    async {
                        loop {
                            if let Some(form) = forms.list_pending().first() {
                                assert!(forms.submit(&form.form_id, submission));
                                break;
                            }
                            updates.changed().await.unwrap();
                        }
                    },
                    executor.run_in_turn(&file, "main", vec![], None, Some(session)),
                )
            })
            .await
            .unwrap();
            assert!(
                matches!(result.unwrap(), atman_rt::Value::Bool(value) if value == answer.unwrap_or(false)),
                "transport={via_resolver}, answer={answer:?}"
            );
            assert!(forms.list_pending().is_empty());
        }
    }
}
