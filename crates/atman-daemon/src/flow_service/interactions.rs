//! Read the original owner's pending forms and compact review.

use std::sync::{Arc, OnceLock};

use atman_proto::{JsonRpcError, flows::FlowAddress, interactions::*};
use atman_runtime::{
    flow_runtime::{FlowWorker, host::SessionHandle},
    form::{FormAnswerTiming, FormKind, PendingForm},
    redact::Redactor,
    session::{FormRegistry, PendingCompactReview},
};
use uuid::Uuid;

use super::{SessionService, core_address, wire};
use crate::DaemonState;

mod reply;
pub(super) use reply::{ReplyReceipts, status, submit, validate_operation, validate_reply};

const MAX_FORMS: usize = 8;
const MAX_QUESTIONS: usize = 24;
const MAX_OPTIONS: usize = 48;
const MAX_FORM_BYTES: usize = 16 * 1024;
const MAX_PROMPT_BYTES: usize = 2048;
const MAX_OPTION_BYTES: usize = 512;
const MAX_SUMMARY_BYTES: usize = 64 * 1024;
const MAX_PREVIEW_BYTES: usize = 16 * 1024;
static BUILTIN_REDACTOR: OnceLock<Redactor> = OnceLock::new();

pub(super) fn snapshot(
    state: &DaemonState,
    request: ReadFlowInteractionsRequest,
) -> Result<serde_json::Value, JsonRpcError> {
    let reply = read(state, request);
    serde_json::to_value(reply)
        .map_err(|_| JsonRpcError::internal("Pending interactions could not be encoded"))
}

fn read(state: &DaemonState, request: ReadFlowInteractionsRequest) -> ReadFlowInteractionsResponse {
    use ReadFlowInteractionsResponse::*;

    let ReadFlowInteractionsRequest { flow, head } = request;
    let Some(service) = state.session_service.get().filter(|_| !state.is_stopping()) else {
        return Unavailable { flow, head };
    };
    let Some((_, worker)) = loaded_flow(service, &flow) else {
        return Unavailable { flow, head };
    };
    let context = worker.context();
    let inbox = context.deferred_form_inbox();
    let expected = wire::core_head(head.clone());
    if inbox.history_head().as_ref() != Some(&expected) {
        return SourceChanged { flow, head };
    }
    let configured = context.sink().redactor();
    let redactor = configured
        .as_deref()
        .unwrap_or_else(|| BUILTIN_REDACTOR.get_or_init(Redactor::builtin));
    let mut forms = Vec::new();
    let mut omitted = false;
    let mut remaining = MAX_FORMS;
    for registry in registries(state, service, context.forms()) {
        let observed =
            registry.inspect_for_flow(&inbox, &expected, remaining + 1, |form, timing| {
                self::form(redactor, form, timing)
            });
        let count = observed.len().min(remaining);
        omitted |= observed.len() > count;
        for item in observed.into_iter().take(count) {
            match item {
                Some(item) => forms.push(item),
                None => omitted = true,
            }
        }
        remaining -= count;
        if omitted && remaining == 0 {
            break;
        }
    }
    let compact = worker.inspect_compact_review(&expected, |review| self::review(redactor, review));
    omitted |= compact.as_ref().is_some_and(Option::is_none);
    if inbox.history_head().as_ref() != Some(&expected) {
        return SourceChanged { flow, head };
    }
    Ready {
        flow,
        head,
        host_generation: state.daemon_generation().to_owned(),
        forms,
        compact: compact.flatten(),
        omitted,
    }
}

fn loaded_flow(
    service: &SessionService,
    flow: &FlowAddress,
) -> Option<(SessionHandle, FlowWorker)> {
    let address = core_address(flow.clone());
    let session = service.host.session(address.session_id.0).ok()?;
    let worker = session.loaded_flow(&address.instance_id)?;
    (!worker.is_stopped() && !worker.is_transient()).then_some((session, worker))
}

fn registries(
    state: &DaemonState,
    service: &SessionService,
    local: Arc<FormRegistry>,
) -> Vec<Arc<FormRegistry>> {
    let mut registries = vec![local, state.forms.clone()];
    if let Some(shared) = &service.prompts {
        for registry in shared.form_registries() {
            if !registries
                .iter()
                .any(|current| Arc::ptr_eq(current, &registry))
            {
                registries.push(registry);
            }
        }
    }
    registries
}

fn form(redactor: &Redactor, pending: &PendingForm, timing: FormAnswerTiming) -> Option<FlowForm> {
    let id = Uuid::parse_str(&pending.form_id)
        .ok()
        .filter(|id| !id.is_nil())?;
    let questions = &pending.form.questions;
    if questions.is_empty() || questions.len() > MAX_QUESTIONS {
        return None;
    }
    let mut remaining = MAX_FORM_BYTES;
    let mut project = |text: &str, max: usize| {
        let limit = remaining.min(max);
        let text = bounded(redactor, text, limit)?;
        remaining -= text.len();
        Some(text)
    };
    let questions = questions
        .iter()
        .map(|question| {
            let prompt = project(question.kind.prompt(), MAX_PROMPT_BYTES)?;
            let mut options = |values: &[String]| {
                if values.len() > MAX_OPTIONS {
                    return None;
                }
                values
                    .iter()
                    .map(|value| project(value, MAX_OPTION_BYTES))
                    .collect::<Option<Vec<_>>>()
            };
            let input = match &question.kind {
                FormKind::Confirm { .. } => FlowFormInput::Confirm,
                FormKind::SingleSelect {
                    options: values, ..
                } => FlowFormInput::SingleSelect {
                    options: options(values)?,
                },
                FormKind::MultiSelect {
                    options: values,
                    min,
                    max,
                    ..
                } => FlowFormInput::MultiSelect {
                    options: options(values)?,
                    min: *min,
                    max: *max,
                },
                FormKind::Text {
                    placeholder,
                    multiline,
                    ..
                } => FlowFormInput::Text {
                    placeholder: match placeholder {
                        Some(value) => Some(project(value, MAX_PROMPT_BYTES)?),
                        None => None,
                    },
                    multiline: *multiline,
                },
            };
            Some(FlowFormQuestion { prompt, input })
        })
        .collect::<Option<Vec<_>>>()?;
    let timing = match timing {
        FormAnswerTiming::Waiting => FlowFormTiming::Waiting,
        FormAnswerTiming::Deferred => FlowFormTiming::Deferred,
    };
    Some(FlowForm {
        id,
        run_id: pending.run_id.0.0,
        timing,
        questions,
    })
}

fn review(redactor: &Redactor, pending: &PendingCompactReview) -> Option<FlowCompactReview> {
    Some(FlowCompactReview {
        id: Uuid::parse_str(&pending.review_id)
            .ok()
            .filter(|id| !id.is_nil())?,
        summary: bounded(redactor, &pending.summary, MAX_SUMMARY_BYTES)?,
        slice_preview: bounded(redactor, &pending.slice_preview, MAX_PREVIEW_BYTES),
        slice_count: pending.slice_count,
        range_start: pending.range_start,
        range_end: pending.range_end,
        tokens_before: pending.tokens_before,
    })
}

fn bounded(redactor: &Redactor, value: &str, max: usize) -> Option<String> {
    if value.len() > max {
        return None;
    }
    let redacted = redactor.redact(value).0;
    (redacted.len() <= max).then_some(redacted)
}
