use std::sync::Arc;

use atman_runtime::event::FlowRunId;
use atman_runtime::form::{FormAnswer, FormKind, PendingForm};
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
        self.forms.submit(&form_id, FormAnswer::Cancelled);
    }

    fn register_with_payload(
        &self,
        id: PromptId,
        kind: &str,
        payload: serde_json::Value,
    ) -> oneshot::Receiver<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        let form_id = format!("prompt_{}", id);
        let form_kind = build_form_kind(kind, &payload);
        let form = PendingForm {
            form_id,
            run_id: FlowRunId::now(),
            tool_use_id: format!("prompt_{}", id),
            kind: form_kind,
            emitted_at: chrono::Utc::now(),
        };
        let answer_rx = self.forms.request(form);
        let payload_clone = payload;
        let kind_str = kind.to_string();
        tokio::spawn(async move {
            let answer = answer_rx.await.ok();
            let value = answer_to_value(answer, &kind_str, &payload_clone);
            let _ = tx.send(value);
        });
        rx
    }
}

fn build_form_kind(kind: &str, payload: &serde_json::Value) -> FormKind {
    match kind {
        "form_ask" => {
            serde_json::from_value::<FormKind>(payload.clone()).unwrap_or(FormKind::Confirm {
                prompt: "Approve form_ask?".into(),
            })
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
