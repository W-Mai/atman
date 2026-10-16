use std::sync::Arc;

type FlowRunId = atman_rt::RunId<crate::event::AtmanUuid>;
use super::{PromptContext, PromptId, PromptResolver};
use crate::form::{CompositeForm, FormAnswer, FormKind, FormQuestion, FormSubmission, PendingForm};
use crate::session::FormRegistry;
use tokio::sync::oneshot;

/// Present host prompts through a form registry while retaining invocation ownership.
pub struct FormPromptResolver {
    forms: Arc<FormRegistry>,
}

impl FormPromptResolver {
    pub fn new(forms: Arc<FormRegistry>) -> Self {
        Self { forms }
    }
}

impl PromptResolver for FormPromptResolver {
    fn register(&self, id: PromptId) -> oneshot::Receiver<serde_json::Value> {
        self.register_with_payload(id, "confirm", serde_json::json!({}))
    }

    fn drop_pending(&self, id: &PromptId) {
        let form_id = id.to_string();
        self.forms.cancel(&form_id);
    }

    fn expire_pending(&self, id: &PromptId) -> bool {
        self.forms.expire(&id.to_string())
    }

    fn register_with_payload(
        &self,
        id: PromptId,
        kind: &str,
        payload: serde_json::Value,
    ) -> oneshot::Receiver<serde_json::Value> {
        self.register_form(id, kind, payload, None)
    }

    fn register_in_context(
        &self,
        id: PromptId,
        kind: &str,
        payload: serde_json::Value,
        context: &PromptContext,
    ) -> oneshot::Receiver<serde_json::Value> {
        self.register_form(id, kind, payload, Some(context))
    }
}

impl FormPromptResolver {
    fn register_form(
        &self,
        id: PromptId,
        kind: &str,
        payload: serde_json::Value,
        context: Option<&PromptContext>,
    ) -> oneshot::Receiver<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        let form_id = id.to_string();
        let mut form = composite_pending_form(&form_id, &id, kind, &payload);
        if let Some(context) = context {
            if let Some(run_id) = &context.run_id {
                form.run_id = run_id.clone();
            }
            if let Some(tool_use_id) = &context.tool_use_id {
                form.tool_use_id = tool_use_id.clone();
            }
        }
        let answer_rx = match context.and_then(|context| context.session.as_ref()) {
            Some(session) => self
                .forms
                .request_with_inbox(form, session.deferred_form_inbox()),
            None => self.forms.request(form),
        };
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

    #[tokio::test]
    async fn only_expirable_waits_retain_a_late_answer_route() {
        let owner = Arc::new(crate::Session::open_ephemeral());
        let forms = owner.forms();
        let _subscriber = forms.subscribe();
        let ctx = crate::tool::ToolCtx::new()
            .with_session_runtime(owner.clone())
            .with_forms(forms.clone());
        let resolver = ctx.form_resolver().unwrap();
        for expirable in [false, true] {
            let id = PromptId::now();
            let payload = serde_json::to_value(FormKind::Confirm {
                prompt: "Proceed?".into(),
            })
            .unwrap();
            let cancel = tokio_util::sync::CancellationToken::new();
            if expirable {
                assert!(
                    crate::rendezvous::await_expirable_prompt_with_payload_cancel(
                        &resolver,
                        id,
                        "form_ask",
                        payload,
                        std::time::Duration::ZERO,
                        &cancel,
                    )
                    .await
                    .unwrap()
                    .is_none()
                );
            } else {
                assert!(
                    crate::rendezvous::await_prompt_with_payload_cancel(
                        &resolver,
                        id,
                        "form_ask",
                        payload,
                        std::time::Duration::ZERO,
                        &cancel,
                    )
                    .await
                    .is_err()
                );
            }
            assert_eq!(forms.list_pending().len(), usize::from(expirable));
            let form_id = id.to_string();
            let submitted = FormSubmission::Submitted {
                answers: vec![FormAnswer::Confirmed { value: true }],
            };
            assert_eq!(
                forms.submit(&form_id, submitted.clone()).await.unwrap(),
                expirable
            );
            assert!(!forms.submit(&form_id, submitted).await.unwrap());
        }
        assert_eq!(owner.deferred_form_inbox().pending_count(), 1);
        assert!(forms.list_pending().is_empty());
    }
}
