//! Model selection validates the retained question window before changing client state.

use super::*;

/// Check the current model window using the recorded entry and ordinary input-budget estimates.
/// This does not change the model, source reference, history, or execution owner.
pub async fn validate_question_model_switch(
    worker: &FlowWorker,
    executor: &crate::Executor,
    model: &str,
    effort: Option<&ReasoningSelection>,
) -> io::Result<QuestionModelBudget> {
    let (position, window, configuration, project, entry) = worker
        .request(|store| {
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
