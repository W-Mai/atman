//! Admit an operator reply at the original owner and retain its acknowledgement.

use std::{collections::VecDeque, io, sync::Mutex};

use atman_proto::{JsonRpcError, interactions::*};
use atman_runtime::{
    flow_runtime::{FlowWorker, catalog::SessionCatalog},
    form::{
        DeferredFormReceipt, FormAnswer, FormAnswerTiming, FormKind, FormReplyLookup,
        FormReplyOperation, FormSubmission, ObservedFormAnswer, PendingForm,
    },
    redact::Redactor,
    session::CompactReviewDecision,
};
use uuid::Uuid;

use super::{BUILTIN_REDACTOR, MAX_FORM_BYTES, MAX_OPTIONS, MAX_QUESTIONS, MAX_SUMMARY_BYTES};
use crate::DaemonState;

const MAX_RECEIPTS: usize = 128;
const MAX_HOST_GENERATION_BYTES: usize = 128;

#[derive(Default)]
pub(in crate::flow_service) struct ReplyReceipts(Mutex<VecDeque<Entry>>);

struct Entry {
    key: FlowInteractionOperation,
    fingerprint: blake3::Hash,
    outcome: FlowInteractionOutcome,
}

struct Attempt<'a> {
    receipts: &'a ReplyReceipts,
    id: Uuid,
    pending: bool,
}

impl Attempt<'_> {
    fn complete(mut self, outcome: FlowInteractionOutcome) {
        self.receipts.finish(self.id, outcome);
        self.pending = false;
    }
}

impl Drop for Attempt<'_> {
    fn drop(&mut self) {
        if self.pending {
            self.receipts
                .finish(self.id, FlowInteractionOutcome::Unknown);
        }
    }
}

impl ReplyReceipts {
    fn get(
        entries: &VecDeque<Entry>,
        key: &FlowInteractionOperation,
        fingerprint: Option<blake3::Hash>,
    ) -> Result<Option<FlowInteractionOutcome>, JsonRpcError> {
        let Some(entry) = entries
            .iter()
            .find(|entry| entry.key.request_id == key.request_id)
        else {
            return Ok(None);
        };
        if entry.key != *key
            || fingerprint.is_some_and(|fingerprint| fingerprint != entry.fingerprint)
        {
            return Err(conflict());
        }
        Ok(Some(entry.outcome.clone()))
    }

    fn lookup(
        &self,
        key: &FlowInteractionOperation,
        fingerprint: Option<blake3::Hash>,
    ) -> Result<Option<FlowInteractionOutcome>, JsonRpcError> {
        Self::get(&self.0.lock().unwrap(), key, fingerprint)
    }

    fn begin(
        &self,
        key: &FlowInteractionOperation,
        fingerprint: blake3::Hash,
    ) -> Result<Result<Attempt<'_>, FlowInteractionOutcome>, JsonRpcError> {
        let mut entries = self.0.lock().unwrap();
        if let Some(outcome) = Self::get(&entries, key, Some(fingerprint))? {
            return Ok(Err(outcome));
        }
        if entries.len() >= MAX_RECEIPTS {
            let index = entries
                .iter()
                .position(|entry| entry.outcome != FlowInteractionOutcome::Pending)
                .ok_or_else(|| JsonRpcError::application("Interaction replies are busy"))?;
            entries.remove(index);
        }
        entries.push_back(Entry {
            key: key.clone(),
            fingerprint,
            outcome: FlowInteractionOutcome::Pending,
        });
        Ok(Ok(Attempt {
            receipts: self,
            id: key.request_id,
            pending: true,
        }))
    }

    fn finish(&self, id: Uuid, outcome: FlowInteractionOutcome) {
        if let Some(entry) = self.0.lock().unwrap().iter_mut().find(|entry| {
            entry.key.request_id == id && entry.outcome == FlowInteractionOutcome::Pending
        }) {
            entry.outcome = outcome;
        }
    }
}

fn conflict() -> JsonRpcError {
    JsonRpcError::invalid_params("Interaction request ID was reused with different data")
}

pub(in crate::flow_service) fn validate_operation(
    operation: &FlowInteractionOperation,
) -> Result<(), JsonRpcError> {
    if [
        operation.flow.session_id.0,
        operation.flow.instance_id,
        operation.head.branch,
        operation.interaction_id,
        operation.request_id,
    ]
    .iter()
    .any(Uuid::is_nil)
        || operation.host_generation.is_empty()
        || operation.host_generation.len() > MAX_HOST_GENERATION_BYTES
    {
        return Err(JsonRpcError::invalid_params(
            "Interaction reply requires its original Flow, head, and owner",
        ));
    }
    Ok(())
}

pub(in crate::flow_service) fn validate_reply(
    request: &ReplyFlowInteractionRequest,
) -> Result<(), JsonRpcError> {
    use FlowInteractionKind::*;
    use FlowInteractionReply::*;

    validate_operation(&request.operation)?;
    let valid = match (request.operation.kind, &request.reply) {
        (WaitingForm | DeferredForm, RejectForm)
        | (CompactReview, AcceptSummary | RejectSummary) => true,
        (CompactReview, EditSummary { summary }) => summary.len() <= MAX_SUMMARY_BYTES,
        (WaitingForm | DeferredForm, AnswerForm { answers }) => {
            (1..=MAX_QUESTIONS).contains(&answers.len()) && answer_bytes(answers).is_some()
        }
        _ => false,
    };
    if !valid {
        return Err(JsonRpcError::invalid_params(
            "Reply does not match the interaction kind or size limit",
        ));
    }
    Ok(())
}

fn answer_bytes(answers: &[FlowFormAnswer]) -> Option<usize> {
    answers
        .iter()
        .try_fold(0_usize, |used, answer| match answer {
            FlowFormAnswer::Text { text } => used
                .checked_add(text.len())
                .filter(|bytes| *bytes <= MAX_FORM_BYTES),
            FlowFormAnswer::MultiSelect { indices } if indices.len() > MAX_OPTIONS => None,
            _ => Some(used),
        })
}

pub(in crate::flow_service) async fn status(
    state: &DaemonState,
    operation: FlowInteractionOperation,
) -> Result<serde_json::Value, JsonRpcError> {
    let known = status_outcome(state, &operation, None).await?;
    response(operation, known.unwrap_or(FlowInteractionOutcome::Unknown))
}

pub(in crate::flow_service) async fn submit(
    state: &DaemonState,
    request: ReplyFlowInteractionRequest,
) -> Result<serde_json::Value, JsonRpcError> {
    use FlowInteractionOutcome::*;

    let encoded = serde_json::to_vec(&request)
        .map_err(|_| JsonRpcError::internal("Interaction reply could not be encoded"))?;
    let fingerprint = blake3::hash(&encoded);
    let ReplyFlowInteractionRequest { operation, reply } = request;
    if let Some(outcome) = status_outcome(state, &operation, Some(fingerprint)).await? {
        return response(operation, outcome);
    }
    if operation.host_generation != state.daemon_generation() {
        return response(operation, Unknown);
    }
    let Some(service) = state.session_service.get().filter(|_| !state.is_stopping()) else {
        return response(operation, Unavailable);
    };
    let Some((session, worker)) = super::loaded_flow(service, &operation.flow) else {
        return response(operation, Unavailable);
    };
    let _host = service
        .host
        .retain_host_operation()
        .map_err(|_| JsonRpcError::application("The interaction owner is unavailable"))?;
    let attempt = match service.interaction_replies.begin(&operation, fingerprint)? {
        Ok(attempt) => attempt,
        Err(outcome) => return response(operation, outcome),
    };
    let outcome = match session.retain_operation().await {
        Ok(_owner) => apply(state, service, &worker, &operation, fingerprint, reply)
            .await
            .unwrap_or(Unknown),
        Err(_) => Unavailable,
    };
    attempt.complete(outcome.clone());
    response(operation, outcome)
}

async fn status_outcome(
    state: &DaemonState,
    operation: &FlowInteractionOperation,
    fingerprint: Option<blake3::Hash>,
) -> Result<Option<FlowInteractionOutcome>, JsonRpcError> {
    let recorded = state
        .session_service
        .get()
        .map(|service| service.interaction_replies.lookup(operation, fingerprint))
        .transpose()?
        .flatten();
    if recorded
        .as_ref()
        .is_some_and(|outcome| *outcome != FlowInteractionOutcome::Unknown)
        || operation.kind != FlowInteractionKind::DeferredForm
    {
        return Ok(recorded);
    }
    let address = super::core_address(operation.flow.clone());
    let catalog = SessionCatalog::new(
        state
            .session_service
            .get()
            .map(|service| service.host.clone()),
        Some(state.data_dir().to_path_buf()),
    );
    let lookup = FormReplyLookup {
        head: super::wire::core_head(operation.head.clone()),
        prompt_id: operation.interaction_id,
        request_id: operation.request_id,
        host_generation: operation.host_generation.clone(),
    };
    let acceptance = match catalog
        .observe_form_reply(&address, lookup, |_| Ok(()))
        .await
    {
        Ok(acceptance) => acceptance,
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(_) => {
            return Err(JsonRpcError::application(
                "The original form reply could not be checked",
            ));
        }
    };
    let Some(DeferredFormReceipt {
        operation: original,
        acceptance,
    }) = acceptance
    else {
        return Ok(recorded);
    };
    if fingerprint.is_some_and(|fingerprint| *fingerprint.as_bytes() != original.fingerprint) {
        return Err(conflict());
    }
    Ok(Some(deferred(acceptance).map_err(|_| {
        JsonRpcError::internal("Form reply has no durable receipt")
    })?))
}

async fn apply(
    state: &DaemonState,
    service: &super::SessionService,
    worker: &FlowWorker,
    operation: &FlowInteractionOperation,
    fingerprint: blake3::Hash,
    reply: FlowInteractionReply,
) -> io::Result<FlowInteractionOutcome> {
    use FlowInteractionOutcome::*;
    use FlowInteractionReply::*;

    let head = super::wire::core_head(operation.head.clone());
    let id = operation.interaction_id.to_string();
    let decision = match reply {
        AcceptSummary => CompactReviewDecision::AcceptAsIs,
        EditSummary { summary } => CompactReviewDecision::AcceptEdited { summary },
        RejectSummary => CompactReviewDecision::Reject,
        AnswerForm { .. } | RejectForm => {
            let context = worker.context();
            let inbox = context.deferred_form_inbox();
            let configured = context.sink().redactor();
            let redactor = configured
                .as_deref()
                .unwrap_or_else(|| BUILTIN_REDACTOR.get_or_init(Redactor::builtin));
            let timing = if operation.kind == FlowInteractionKind::WaitingForm {
                FormAnswerTiming::Waiting
            } else {
                FormAnswerTiming::Deferred
            };
            let mut matching = super::registries(state, service, context.forms())
                .into_iter()
                .filter_map(|registry| {
                    let submission =
                        registry.inspect_form_for_flow(&inbox, &head, &id, timing, |form| {
                            super::form(redactor, form, timing)?;
                            form_submission(form, &reply)
                        })?;
                    Some((registry, submission))
                });
            let Some((registry, submission)) = matching.next() else {
                return Ok(Unavailable);
            };
            if matching.next().is_some() {
                return Ok(Unavailable);
            }
            let Some(submission) = submission else {
                return Ok(InvalidAnswer);
            };
            let operation = FormReplyOperation {
                request_id: operation.request_id,
                host_generation: operation.host_generation.clone(),
                fingerprint: *fingerprint.as_bytes(),
            };
            return match registry
                .submit_for_flow_with_operation(
                    worker,
                    &id,
                    head,
                    timing,
                    submission,
                    Some(operation),
                )
                .await?
            {
                ObservedFormAnswer::Resumed => Ok(Resumed),
                ObservedFormAnswer::Dismissed => Ok(Dismissed),
                ObservedFormAnswer::Deferred(acceptance) => deferred(acceptance),
                ObservedFormAnswer::Unavailable => Ok(Unavailable),
            };
        }
    };
    if worker.decide_compact_review(id, head, decision).await? {
        Ok(Decided)
    } else {
        Ok(Unavailable)
    }
}

fn form_submission(form: &PendingForm, reply: &FlowInteractionReply) -> Option<FormSubmission> {
    let FlowInteractionReply::AnswerForm { answers } = reply else {
        return Some(FormSubmission::Rejected);
    };
    if answers.len() != form.form.questions.len() {
        return None;
    }
    let answers = form
        .form
        .questions
        .iter()
        .zip(answers)
        .map(|(question, answer)| match (&question.kind, answer) {
            (_, FlowFormAnswer::Skip) => Some(FormAnswer::Cancelled),
            (FormKind::Confirm { .. }, FlowFormAnswer::Confirm { value }) => {
                Some(FormAnswer::Confirmed { value: *value })
            }
            (FormKind::Text { .. }, FlowFormAnswer::Text { text }) => {
                Some(FormAnswer::TextEntered { text: text.clone() })
            }
            (FormKind::SingleSelect { options, .. }, FlowFormAnswer::Select { index }) => {
                Some(FormAnswer::Selected {
                    index: *index,
                    label: options.get(*index)?.clone(),
                })
            }
            (FormKind::MultiSelect { options, .. }, FlowFormAnswer::MultiSelect { indices }) => {
                let unique = indices
                    .iter()
                    .copied()
                    .collect::<std::collections::HashSet<_>>();
                if unique.len() != indices.len() {
                    return None;
                }
                let labels = indices
                    .iter()
                    .map(|index| options.get(*index).cloned())
                    .collect::<Option<Vec<_>>>()?;
                Some(FormAnswer::MultiSelected {
                    indices: indices.clone(),
                    labels,
                })
            }
            _ => None,
        })
        .collect::<Option<Vec<_>>>()?;
    let submission = FormSubmission::Submitted { answers };
    form.form.accepts(&submission).then_some(submission)
}

fn deferred(
    acceptance: atman_runtime::flow_store::inbox::Acceptance,
) -> io::Result<FlowInteractionOutcome> {
    Ok(FlowInteractionOutcome::Deferred {
        message_id: acceptance.message_id,
        accepted: super::wire::cursor(&acceptance.receipt)?,
    })
}

fn response(
    operation: FlowInteractionOperation,
    outcome: FlowInteractionOutcome,
) -> Result<serde_json::Value, JsonRpcError> {
    serde_json::to_value(FlowInteractionReplyResponse { operation, outcome })
        .map_err(|_| JsonRpcError::internal("Interaction receipt could not be encoded"))
}
