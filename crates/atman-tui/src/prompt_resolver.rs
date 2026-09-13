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
        self.forms.submit(&form_id, FormSubmission::Rejected);
    }

    fn expire_pending(&self, id: &PromptId) -> bool {
        self.forms.expire(&format!("prompt_{}", id))
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
            let Ok(submission) = answer_rx.await else {
                return;
            };
            // FormAsk decodes FormSubmission for both single and composite forms.
            let value = if kind_str == "form_ask" {
                serde_json::to_value(&submission).unwrap_or(serde_json::Value::Null)
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
    use crate::wm::modal::ModalOverlay;

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
