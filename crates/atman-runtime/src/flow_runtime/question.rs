//! Contextual questions use the ordinary entry, input, context and retention owners.

use std::{collections::BTreeMap, io, num::NonZeroUsize, sync::Arc};

use uuid::Uuid;

use super::{
    FlowWorker,
    host::RuntimeHost,
    idle_context::IdleContextUpdate,
    launch::{EntryExecution, LaunchedFlow},
    source_capture::{CapturedOrigin, ReferenceBudget, ReferenceRequest, SourceSelection},
};
use crate::{
    context_plan::{ReferenceOrigin, compile_context_records},
    event::TurnId,
    flow_authority::EffectiveAuthority,
    flow_store::{
        EntrySnapshot, FlowDescriptor, FlowRef,
        context::CommittedContextCheckpoint,
        inbox::{DeliveryMode, DeliveryRequest, InboxPayload, SenderIdentity, UserContent},
        invalid,
        launch::{EntryBinding, EntryRequest, EntryStart, LaunchInput},
        position::FlowPosition,
    },
    message::{Message, MessagePart},
    provider::ReasoningSelection,
    tools::tool_output::OutputStore,
};

mod budget;
mod model_switch;
pub use budget::QuestionModelBudget;
pub use model_switch::validate_question_model_switch;
#[cfg(test)]
mod tests;

/// The host authenticates `user`; source identity is an observation, never an access grant.
#[derive(Clone, Debug)]
pub struct QuestionRequest {
    pub id: Uuid,
    pub request_id: Uuid,
    pub source: SourceSelection,
    pub user: Uuid,
    pub content: UserContent,
    pub model: String,
    pub effort: Option<ReasoningSelection>,
}

/// Trusted configuration for the actual captured question entry, shared by every client.
pub struct QuestionPreparation {
    pub entry: EntrySnapshot,
    pub executor: crate::Executor,
    pub authority: EffectiveAuthority,
    pub trust: crate::trust::TrustConfig,
    pub yield_interval: NonZeroUsize,
}

/// An immutable attempt. Retry it rather than preparing another source snapshot for the same ID.
pub struct PreparedQuestion {
    descriptor: FlowDescriptor,
    input: LaunchInput,
    source: SourceSelection,
    origin: CapturedOrigin,
    outputs: Arc<OutputStore>,
    preparation: QuestionPreparation,
    budget: QuestionModelBudget,
}

pub struct LaunchedQuestion {
    pub flow: LaunchedFlow,
    pub source: SourceSelection,
    pub budget: QuestionModelBudget,
}

impl PreparedQuestion {
    pub fn address(&self) -> &FlowRef {
        &self.descriptor.address
    }

    pub fn request_id(&self) -> Uuid {
        self.descriptor.creation_request
    }

    pub fn source(&self) -> &SourceSelection {
        &self.source
    }

    pub fn budget(&self) -> &QuestionModelBudget {
        &self.budget
    }

    /// An accepted launch remains owned by the ordinary host if this waiter disappears.
    pub async fn launch(
        &self,
        host: &RuntimeHost,
        authorize_new: impl Fn(&FlowDescriptor, &LaunchInput) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<LaunchedQuestion> {
        let accepted = match host
            .session(self.descriptor.address.session_id.0)?
            .loaded_flow(&self.descriptor.address.instance_id)
        {
            Some(worker) => worker.launch_record().await?.is_some(),
            None => false,
        };
        if !accepted {
            self.budget.validate_current(&self.preparation.executor)?;
        }
        let flow = host
            .launch_transient_entry(
                self.descriptor.clone(),
                self.input.clone(),
                EntryExecution {
                    executor: self.preparation.executor.clone(),
                    authority: self.preparation.authority.clone(),
                    trust: self.preparation.trust.clone(),
                    yield_interval: self.preparation.yield_interval,
                    catalog: self.origin.catalog.clone(),
                },
                self.outputs.clone(),
                self.origin.flow_project.clone(),
                authorize_new,
            )
            .await?;
        Ok(LaunchedQuestion {
            flow,
            source: self.source.clone(),
            budget: self.budget.clone(),
        })
    }
}

impl RuntimeHost {
    /// Prepare one frozen source and exact invocation without creating an execution owner or writing history.
    pub async fn prepare_question(
        &self,
        request: QuestionRequest,
        mut preparation: QuestionPreparation,
        authorize_source: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<PreparedQuestion> {
        if request.id.is_nil() || request.request_id.is_nil() || request.user.is_nil() {
            return Err(invalid("question identities must be nonempty"));
        }
        authorize_source(&request.source.flow)?;
        let session = self.session(request.source.flow.session_id.0)?;
        let worker = session
            .loaded_flow(&request.source.flow.instance_id)
            .ok_or_else(|| invalid("open the source Flow before asking a question"))?;
        let catalog = session.snapshot().await?;
        let project = worker.project().await?;
        preparation
            .executor
            .validate_session_binding(&catalog.catalog.descriptor.configuration, project.as_ref())
            .map_err(invalid)?;
        preparation.authority =
            question_authority(&worker, &preparation.authority, preparation.yield_interval).await?;
        let address = FlowRef {
            session_id: request.source.flow.session_id,
            instance_id: atman_rt::FlowInstanceId(request.id.into()),
        };
        if address == request.source.flow {
            return Err(invalid("question identity cannot alias its source"));
        }
        let first_turn = atman_rt::TurnId(scoped_id(request.id, b"question.first-turn").into());
        let budget = budget::prepare(
            &preparation,
            &catalog.catalog.descriptor.configuration,
            project.as_ref(),
            &address,
            &request.model,
            request.effort.as_ref(),
            budget::content_tokens(&request.content, &first_turn),
        )
        .await?;
        let capture = self
            .capture_reference(
                ReferenceRequest {
                    flow: request.source.flow,
                    branch: Some(request.source.branch),
                    at: None,
                    budget: budget.capture_limits(),
                },
                authorize_source,
            )
            .await?;
        if capture.origin.configuration != catalog.catalog.descriptor.configuration
            || capture.origin.flow_project != project
        {
            return Err(invalid(
                "source binding changed while preparing the question",
            ));
        }
        let source_authority = capture
            .origin
            .execution
            .as_ref()
            .ok_or_else(|| invalid("source Flow has no recorded execution ceiling"))?
            .authority
            .read_only();
        let authority = source_authority
            .for_child(&preparation.authority, false, None)
            .map_err(invalid)?;
        if authority.workspace_root != preparation.authority.workspace_root {
            return Err(invalid(
                "source working directory changed during preparation",
            ));
        }
        preparation.authority = authority;
        let source = capture.source;
        let selected = source.clone();
        let reference_tokens = budget.reference_tokens;
        let prepared = crate::read_work::capture(move || {
            capture
                .product
                .into_source_record_at(selected, reference_tokens)
        })
        .await
        .map_err(io::Error::other)??;
        let inherited_context = compile_context_records(&[], [prepared.record])
            .into_iter()
            .map(|record| Message::context_record(first_turn.clone(), record))
            .collect();
        let descriptor = FlowDescriptor {
            address: address.clone(),
            creation_request: request.request_id,
            created_by: None,
            content: preparation.entry.clone().into(),
        };
        let input = LaunchInput {
            name: None,
            display_label: Some("Quick Ask".into()),
            scope: None,
            workspace: None,
            request: EntryRequest {
                request_id: request.request_id,
                sender: SenderIdentity::User(request.user),
                recipient: address,
                arguments: invocation_arguments(&request.content, &budget.model),
                invocation: invocation_environment(request.effort.as_ref()),
                start: EntryStart::Invoke {
                    user: Some(request.content),
                },
            },
            initial_goal: None,
            inherited_context,
            continued_from: None,
            inherited_outputs: vec![],
            result_route: None,
            execution: Some(EntryBinding {
                user_routes: false,
                session_start: false,
                first_turn,
                first_run: atman_rt::RunId(scoped_id(request.id, b"question.first-run").into()),
                authority: preparation.authority.clone(),
            }),
        };
        Ok(PreparedQuestion {
            descriptor,
            input,
            source,
            origin: capture.origin,
            outputs: Arc::new(prepared.outputs),
            preparation,
            budget,
        })
    }
}

/// A reference refresh is a context operation, not another authored turn.
#[derive(Clone, Debug)]
pub struct QuestionRefreshRequest {
    pub id: Uuid,
    pub question: FlowRef,
    pub expected: FlowPosition,
    pub source: SourceSelection,
    pub model: String,
    pub effort: Option<ReasoningSelection>,
}

pub struct PreparedQuestionRefresh {
    question: FlowRef,
    update: IdleContextUpdate,
    outputs: Arc<OutputStore>,
    pub source: SourceSelection,
    pub budget: QuestionModelBudget,
}

impl PreparedQuestionRefresh {
    pub fn question(&self) -> &FlowRef {
        &self.question
    }

    pub async fn apply(
        &self,
        worker: &FlowWorker,
        authorize: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<CommittedContextCheckpoint> {
        if worker.address() != &self.question {
            return Err(invalid("question refresh owner changed"));
        }
        worker
            .append_idle_context(self.update.clone(), self.outputs.clone(), authorize)
            .await
    }
}

impl RuntimeHost {
    pub async fn prepare_question_refresh(
        &self,
        request: QuestionRefreshRequest,
        mut preparation: QuestionPreparation,
        authorize: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<PreparedQuestionRefresh> {
        if request.id.is_nil() {
            return Err(invalid("question refresh identity must be nonempty"));
        }
        authorize(&request.question)?;
        authorize(&request.source.flow)?;
        let authorize = Arc::new(authorize);
        let worker = self
            .session(request.question.session_id.0)?
            .loaded_flow(&request.question.instance_id)
            .ok_or_else(|| invalid("open the question before refreshing its reference"))?;
        let expected = request.expected.clone();
        let selected = request.source.clone();
        let check = authorize.clone();
        let (window, configuration, project, turn, entry) = worker
            .request(move |store| {
                check(store.state().address())?;
                if store.cursor() != &expected
                    || !matches!(
                        store.state().state(),
                        atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Idle)
                    )
                    || store.pending_inputs().next().is_some()
                {
                    return Err(invalid(
                        "question must remain idle at the selected boundary",
                    ));
                }
                let window = store.context_messages()?;
                let source = reference_origin(&window)
                    .ok_or_else(|| invalid("Flow has no retained question reference"))?;
                if source.source.flow != selected.flow || source.source.branch != selected.branch {
                    return Err(invalid("question source branch changed"));
                }
                let turn = window
                    .iter()
                    .rev()
                    .find(|message| {
                        message.role == crate::message::MessageRole::User
                            && message.origin == crate::message::MessageOrigin::User
                    })
                    .map(|message| message.turn_id.clone())
                    .ok_or_else(|| invalid("question has no existing turn identity"))?;
                Ok((
                    window,
                    store.session_descriptor().configuration.clone(),
                    store.project().cloned(),
                    turn,
                    store.descriptor().entry()?.clone(),
                ))
            })
            .await?;
        preparation.entry = entry;
        preparation.authority =
            question_authority(&worker, &preparation.authority, preparation.yield_interval).await?;
        let retained = without_reference(window);
        let budget = budget::prepare(
            &preparation,
            &configuration,
            project.as_ref(),
            &request.question,
            &request.model,
            request.effort.as_ref(),
            crate::compaction::estimate_tokens_for_messages(&retained),
        )
        .await?;
        let capture = self
            .capture_reference(
                ReferenceRequest {
                    flow: request.source.flow,
                    branch: Some(request.source.branch),
                    at: None,
                    budget: budget.capture_limits(),
                },
                move |flow| authorize(flow),
            )
            .await?;
        let source = capture.source;
        let selected = source.clone();
        let reference_tokens = budget.reference_tokens;
        let prepared = crate::read_work::capture(move || {
            capture
                .product
                .into_source_record_at(selected, reference_tokens)
        })
        .await
        .map_err(io::Error::other)??;
        Ok(PreparedQuestionRefresh {
            question: request.question,
            update: IdleContextUpdate {
                id: request.id,
                expected: request.expected,
                turn,
                records: vec![prepared.record],
            },
            outputs: Arc::new(prepared.outputs),
            source,
            budget,
        })
    }
}

/// Supply the current question model as an explicit entry argument on every next-turn submission.
pub fn question_followup(
    recipient: FlowRef,
    request_id: Uuid,
    user: Uuid,
    content: UserContent,
    model: &str,
    effort: Option<&ReasoningSelection>,
    executor: &crate::Executor,
) -> io::Result<DeliveryRequest> {
    if request_id.is_nil() || user.is_nil() {
        return Err(invalid("question submission identities must be nonempty"));
    }
    let config = budget::resolve_model(executor, model, effort)?;
    let request = DeliveryRequest {
        request_id,
        sender: SenderIdentity::User(user),
        recipient,
        delivery: DeliveryMode::NextTurn,
        cause: None,
        correlation: None,
        scope: None,
        payload: InboxPayload::Invocation {
            arguments: invocation_arguments(&content, &config.info.name),
            invocation: invocation_environment(effort),
            user: Some(content),
        },
    };
    request.validate_source()?;
    Ok(request)
}

pub fn reference_origin(messages: &[Message]) -> Option<&ReferenceOrigin> {
    messages
        .iter()
        .rev()
        .flat_map(|message| message.parts.iter().rev())
        .find_map(|part| match part {
            MessagePart::ContextRecord(record) if record.key() == "quickask.source" => Some(record),
            _ => None,
        })
        .and_then(|record| record.body().reference_origin())
}

async fn question_authority(
    worker: &FlowWorker,
    current: &EffectiveAuthority,
    yield_interval: NonZeroUsize,
) -> io::Result<EffectiveAuthority> {
    let recorded = worker
        .request(|store| {
            store
                .launch_record()
                .and_then(|launch| launch.input.execution)
                .map(|binding| binding.authority.read_only())
                .ok_or_else(|| invalid("question source has no recorded execution ceiling"))
        })
        .await?;
    // The source owns its working directory; the host contributes ceilings, not a new cwd grant.
    let ceiling = recorded
        .for_child(&current.read_only(), false, None)
        .map_err(invalid)?;
    Ok(worker
        .entry_recipe(ceiling, yield_interval)
        .await?
        .authority
        .read_only())
}

fn without_reference(mut messages: Vec<Message>) -> Vec<Message> {
    for message in &mut messages {
        message.parts.retain(|part| !matches!(part, MessagePart::ContextRecord(record) if record.key() == "quickask.source"));
    }
    messages.retain(|message| !message.parts.is_empty());
    messages
}

fn invocation_arguments(content: &UserContent, model: &str) -> BTreeMap<String, serde_json::Value> {
    [
        ("question".into(), content.text.clone().into()),
        ("model".into(), model.into()),
    ]
    .into()
}

fn invocation_environment(
    effort: Option<&ReasoningSelection>,
) -> BTreeMap<String, serde_json::Value> {
    effort
        .map(|effort| [("effort".into(), effort.to_string().into())].into())
        .unwrap_or_default()
}

fn scoped_id(question: Uuid, purpose: &[u8]) -> Uuid {
    let mut digest = blake3::Hasher::new();
    digest.update(purpose);
    digest.update(question.as_bytes());
    Uuid::from_bytes(
        digest.finalize().as_bytes()[..16]
            .try_into()
            .expect("digest contains UUID bytes"),
    )
}
