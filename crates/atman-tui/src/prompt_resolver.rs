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
        let form = composite_pending_form(&form_id, &id, kind, &payload);
        let answer_rx = self.forms.request(form);
        let payload_clone = payload;
        let kind_str = kind.to_string();
        tokio::spawn(async move {
            let submission = answer_rx.await.unwrap_or(FormSubmission::Rejected);
            let value = if kind_str == "form_ask"
                && serde_json::from_value::<CompositeForm>(payload_clone.clone()).is_ok()
            {
                serde_json::to_value(&submission).unwrap_or(serde_json::json!({"kind":"rejected"}))
            } else {
                let answer = match submission {
                    FormSubmission::Submitted { mut answers } => answers.pop(),
                    FormSubmission::Rejected => Some(FormAnswer::Cancelled),
                };
                answer_to_value(answer, &kind_str, &payload_clone)
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
) -> PendingForm {
    let questions = serde_json::from_value::<CompositeForm>(payload.clone())
        .map(|form| form.questions)
        .unwrap_or_else(|_| {
            vec![FormQuestion {
                id: "question".into(),
                kind: build_form_kind(kind, payload),
            }]
        });
    let first_kind = questions
        .first()
        .map(|question| question.kind.clone())
        .unwrap_or(FormKind::Confirm {
            prompt: "Approve form_ask?".into(),
        });
    PendingForm {
        form_id: form_id.into(),
        run_id: FlowRunId::now(),
        tool_use_id: format!("prompt_{id}"),
        form: CompositeForm { questions },
        kind: first_kind,
        emitted_at: chrono::Utc::now(),
    }
}

fn build_form_kind(kind: &str, payload: &serde_json::Value) -> FormKind {
    match kind {
        "form_ask" => {
            if let Ok(form) = serde_json::from_value::<CompositeForm>(payload.clone()) {
                form.questions
                    .first()
                    .map(|question| question.kind.clone())
                    .unwrap_or(FormKind::Confirm {
                        prompt: "Approve form_ask?".into(),
                    })
            } else {
                serde_json::from_value::<FormKind>(payload.clone()).unwrap_or(FormKind::Confirm {
                    prompt: "Approve form_ask?".into(),
                })
            }
        }
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

fn answer_to_value(
    answer: Option<FormAnswer>,
    kind: &str,
    payload: &serde_json::Value,
) -> serde_json::Value {
    if kind == "form_ask" {
        return match answer {
            Some(a) => serde_json::to_value(&a).unwrap_or(serde_json::json!({})),
            None => serde_json::to_value(&FormAnswer::Cancelled).unwrap_or(serde_json::json!({})),
        };
    }
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
            serde_json::json!({"kind": "confirm", "prompt": "Continue?"}),
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
}
