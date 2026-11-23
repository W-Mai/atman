//! Model selection validates the retained question window before changing client state.

use super::*;
use crate::flow_store::{local_operator::LocalOperator, position::TransientPosition};
use crate::user_input::controls::{InputControlSource, InputControlsSnapshot};

impl RuntimeHost {
    pub async fn question_model_options(
        &self,
        target: &QuestionTarget,
        operator: LocalOperator,
        model: String,
        reasoning: Option<ReasoningSelection>,
        query: Option<String>,
    ) -> anyhow::Result<InputControlsSnapshot> {
        let (session, worker) = self
            .question_owner(target, operator.id())
            .await?
            .ok()
            .ok_or_else(|| invalid("Quick Ask has no accepted temporary owner"))?;
        let source = InputControlSource::for_flow(&session, &worker).await?;
        let projected = source.question_models(model, reasoning, query).await?;
        let context = worker.context();
        anyhow::ensure!(
            session
                .loaded_flow(&worker.address().instance_id)
                .is_some_and(|current| {
                    current.is_transient() && std::sync::Arc::ptr_eq(&context, &current.context())
                }),
            "Quick Ask owner retired during model observation"
        );
        Ok(projected)
    }

    pub async fn check_question_model(
        &self,
        target: &QuestionTarget,
        operator: LocalOperator,
        expected: TransientPosition,
        model: &str,
        effort: Option<&ReasoningSelection>,
    ) -> anyhow::Result<QuestionModelBudget> {
        let (session, worker) = self
            .question_owner(target, operator.id())
            .await?
            .ok()
            .ok_or_else(|| invalid("Quick Ask has no accepted temporary owner"))?;
        let _capacity = session.question_starts.reserve_preparation()?;
        let executor = session.executor_for(worker.project().await?).await?;
        let expected = FlowPosition::Transient {
            transient: expected,
        };
        Ok(check_model_switch(&worker, &executor, model, effort, Some(expected)).await?)
    }
}

/// Check the current model window using the recorded entry and ordinary input-budget estimates.
/// This does not change the model, source reference, history, or execution owner.
pub async fn validate_question_model_switch(
    worker: &FlowWorker,
    executor: &crate::Executor,
    model: &str,
    effort: Option<&ReasoningSelection>,
) -> io::Result<QuestionModelBudget> {
    check_model_switch(worker, executor, model, effort, None).await
}

async fn check_model_switch(
    worker: &FlowWorker,
    executor: &crate::Executor,
    model: &str,
    effort: Option<&ReasoningSelection>,
    expected: Option<FlowPosition>,
) -> io::Result<QuestionModelBudget> {
    let (position, window, configuration, project, entry) = worker
        .request(move |store| {
            if expected
                .as_ref()
                .is_some_and(|expected| store.cursor() != expected)
                || store.transient_frozen()
            {
                return Err(invalid("Quick Ask changed before checking the model"));
            }
            let window = store.context_messages()?;
            if reference_origin(&window).is_none() {
                return Err(invalid("Flow has no retained question reference"));
            }
            Ok((
                store.cursor().clone(),
                window,
                store.session_descriptor().configuration.clone(),
                store.project().cloned(),
                store.descriptor().entry()?.clone(),
            ))
        })
        .await?;
    executor
        .validate_session_binding(&configuration, project.as_ref())
        .map_err(invalid)?;
    let trust = worker.context().trust_config();
    let current = executor
        .tool_ctx
        .flow_identity
        .as_ref()
        .map(|identity| identity.effective_authority.clone())
        .unwrap_or_else(|| EffectiveAuthority::root(&trust, false, None));
    let yield_interval = NonZeroUsize::new(16).expect("nonzero question preparation interval");
    let authority = question_authority(worker, &current, yield_interval).await?;
    let context = worker.context();
    let mut runtime = crate::eval::session_context_record_specs(&context).await;
    runtime.extend(crate::eval::llm_dispatch::runtime_context_record_specs(
        None,
    ));
    let budget = budget::prepare(
        &QuestionPreparation {
            entry,
            executor: executor.clone(),
            authority,
            trust,
            yield_interval,
        },
        &configuration,
        project.as_ref(),
        worker.address(),
        model,
        effort,
        budget::ConversationBudget::Window {
            messages: window,
            runtime,
        },
    )
    .await?;
    budget.validate_current(executor)?;
    worker
        .request(move |store| {
            if store.cursor() != &position
                || store.session_descriptor().configuration != configuration
                || store.project() != project.as_ref()
                || store.transient_frozen()
            {
                return Err(invalid(
                    "Question changed while checking the model; retry the selection",
                ));
            }
            Ok(())
        })
        .await?;
    Ok(budget)
}
