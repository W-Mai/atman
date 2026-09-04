use std::sync::Arc;

use atman_runtime::event::FlowRunId;
use atman_runtime::form::{
    CompositeForm, FormAnswer, FormKind, FormQuestion, FormSubmission, PendingForm,
};
use atman_runtime::rendezvous::{PromptId, PromptResolver};
use atman_runtime::session::FormRegistry;
use tokio::sync::oneshot;

pub struct TuiPromptResolver {
    forms: Arc<FormRegistry>,
}

impl TuiPromptResolver {
    pub fn new(forms: Arc<FormRegistry>) -> Self {
        Self { forms }
    }
}

impl PromptResolver for TuiPromptResolver {
    fn register(&self, id: PromptId) -> oneshot::Receiver<serde_json::Value> {
        self.register_with_payload(id, "confirm", serde_json::json!({}))
    }

    fn drop_pending(&self, id: &PromptId) {
        let form_id = format!("prompt_{}", id);
        self.forms.cancel(&form_id);
    }

    fn register_with_payload(
        &self,
        id: PromptId,
        kind: &str,
        payload: serde_json::Value,
    ) -> oneshot::Receiver<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        let form_id = format!("prompt_{}", id);
        let Some(form) = composite_pending_form(&form_id, &id, kind, &payload) else {
            return rx;
        };
        let answer_rx = self.forms.request(form);
        let payload_clone = payload;
        let kind_str = kind.to_string();
        tokio::spawn(async move {
            let submission = answer_rx.await.unwrap_or(FormSubmission::Rejected);
            let value = if kind_str == "form_ask" {
                serde_json::to_value(&submission).expect("form submissions serialize")
            } else {
                let answer = match submission {
                    FormSubmission::Submitted { mut answers } => answers.pop(),
                    FormSubmission::Rejected => Some(FormAnswer::Cancelled),
                };
                answer_to_value(answer, &payload_clone)
            };
            let _ = tx.send(value);
        });
        rx
    }
}

fn composite_pending_form(
    form_id: &str,
    id: &PromptId,
    kind: &str,
    payload: &serde_json::Value,
) -> Option<PendingForm> {
    let form = if kind == "form_ask" {
        serde_json::from_value::<CompositeForm>(payload.clone()).ok()?
    } else {
        CompositeForm {
            questions: vec![FormQuestion {
                id: "question".into(),
                kind: build_form_kind(kind, payload),
            }],
        }
    };
    let first_kind = form.questions.first()?.kind.clone();
    Some(PendingForm {
        form_id: form_id.into(),
        run_id: FlowRunId::now(),
        tool_use_id: format!("prompt_{id}"),
        form,
        kind: first_kind,
        emitted_at: chrono::Utc::now(),
    })
}

fn build_form_kind(kind: &str, payload: &serde_json::Value) -> FormKind {
    match kind {
        "hunk_selection" => {
            let hunks = payload["hunks"].as_array().cloned().unwrap_or_default();
            let options: Vec<String> = hunks
                .iter()
                .map(|h| {
                    let id = h["id"].as_u64().unwrap_or(0);
                    let diff = h["unified_diff"].as_str().unwrap_or("");
                    let first_line = diff.lines().next().unwrap_or("");
                    format!("hunk #{id}: {first_line}")
                })
                .collect();
            FormKind::MultiSelect {
                prompt: "Select hunks to apply".into(),
                options,
                min: Some(0),
                max: None,
            }
        }
        _ => FormKind::Confirm {
            prompt: format!("Approve {}?", kind),
        },
    }
}

fn answer_to_value(answer: Option<FormAnswer>, payload: &serde_json::Value) -> serde_json::Value {
    let hunks = payload["hunks"].as_array().cloned().unwrap_or_default();
    let all_ids: Vec<u64> = hunks.iter().filter_map(|h| h["id"].as_u64()).collect();
    match answer {
        Some(FormAnswer::MultiSelected { indices, .. }) => {
            let ids: Vec<serde_json::Value> = indices
                .iter()
                .filter_map(|&i| all_ids.get(i).copied())
                .map(serde_json::Value::from)
                .collect();
            serde_json::json!({ "hunks": ids })
        }
        Some(FormAnswer::Confirmed { value: true }) => {
            serde_json::json!({ "hunks": all_ids.into_iter().map(serde_json::Value::from).collect::<Vec<_>>() })
        }
        _ => serde_json::json!({ "hunks": [] }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_prompt_wait_removes_the_form_and_records_abandonment() {
        let session = Arc::new(atman_runtime::Session::open_ephemeral());
        let forms = session.forms();
        let subscriber = forms.subscribe();
        let resolver: Arc<dyn PromptResolver> = Arc::new(TuiPromptResolver::new(forms.clone()));
        let id = PromptId::now();
        let mut response = Box::pin(atman_runtime::rendezvous::await_prompt_with_payload(
            &resolver,
            id,
            "form_ask",
            serde_json::json!({"questions": [{"id": "confirm", "kind": "confirm", "prompt": "Continue?"}]}),
            std::time::Duration::from_secs(60),
        ));
        assert!(futures::poll!(&mut response).is_pending());
        assert_eq!(subscriber.borrow().len(), 1);
        drop(response);
        assert!(subscriber.borrow().is_empty());
        assert!(forms.list_pending().is_empty());
        assert!(matches!(
            session.sink().snapshot().last(),
            Some(atman_runtime::event::Event::FormResolved {
                abandoned: true,
                ..
            })
        ));
    }
    #[tokio::test]
    async fn form_tool_uses_one_submission_contract_for_single_and_composite_forms() {
        use atman_runtime::Value;
        use atman_runtime::tool::{Tool, ToolArgs, ToolCtx};

        for composite in [false, true] {
            let session = Arc::new(atman_runtime::Session::open_ephemeral());
            let forms = session.forms();
            let _subscriber = forms.subscribe();
            let mut ctx = ToolCtx::new();
            ctx.prompt_resolver = Some(Arc::new(TuiPromptResolver::new(forms.clone())));
            let question = serde_json::json!({
                "id": "confirm", "kind": "confirm", "prompt": "Continue?"
            });
            let args = if composite {
                serde_json::json!({"questions": [question.clone(), {
                    "id": "name", "kind": "text", "prompt": "Name?"
                }]})
            } else {
                question
            };
            let Value::Struct(named) = Value::from_json(args) else {
                unreachable!()
            };
            let mut response = atman_runtime::tools::form::FormAsk.call(
                ToolArgs {
                    positional: vec![],
                    named,
                },
                &ctx,
            );
            assert!(futures::poll!(&mut response).is_pending());
            let pending = forms.list_pending().pop().unwrap();
            assert_eq!(pending.form.questions.len(), if composite { 2 } else { 1 });
            let mut answers = vec![FormAnswer::Confirmed { value: true }];
            if composite {
                answers.push(FormAnswer::TextEntered {
                    text: "answer".into(),
                });
            }
            assert!(forms.submit(&pending.form_id, FormSubmission::Submitted { answers }));
            let result = response.await.unwrap().to_json();
            if composite {
                assert_eq!(result["kind"], "submitted");
                assert_eq!(result["answers"][0]["value"], true);
                assert_eq!(result["answers"][1]["text"], "answer");
            } else {
                assert_eq!(result["kind"], "confirmed");
                assert_eq!(result["value"], true);
            }
            assert!(forms.list_pending().is_empty());
        }
    }

    #[tokio::test]
    async fn invalid_form_payload_does_not_open_a_confirmation() {
        let forms = Arc::new(FormRegistry::new());
        let _subscriber = forms.subscribe();
        let resolver = TuiPromptResolver::new(forms.clone());
        for payload in [serde_json::json!({}), serde_json::json!({"questions": []})] {
            assert!(
                resolver
                    .register_with_payload(PromptId::now(), "form_ask", payload)
                    .await
                    .is_err()
            );
            assert!(forms.list_pending().is_empty());
        }
    }
}
