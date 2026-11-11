//! Use the actual entry policy and model's existing compaction reserves before source capture.

use std::{io::Read, path::Path};

use atman_rt::ast::{Arg, Expr, Literal, Node, Stmt};
use serde::{Deserialize, Serialize};

use super::*;

/// Estimates retain their source; these are not provider-reported token counts.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct QuestionModelBudget {
    pub model: String,
    pub context_tokens: u64,
    pub usable_tokens: u64,
    pub fixed_tokens: u64,
    pub conversation_tokens: u64,
    pub reference_tokens: u64,
    provider: Option<String>,
    api_model: String,
    api: Option<crate::provider::OpenAiApi>,
    max_output_tokens: Option<u32>,
}

pub(super) enum ConversationBudget {
    Estimate(u64),
    Window {
        messages: Vec<Message>,
        runtime: Vec<crate::context_plan::ContextRecordSpec>,
    },
}

impl From<u64> for ConversationBudget {
    fn from(tokens: u64) -> Self {
        Self::Estimate(tokens)
    }
}

impl QuestionModelBudget {
    pub(super) fn capture_limits(&self) -> ReferenceBudget {
        let defaults = ReferenceBudget::default();
        ReferenceBudget {
            max_context_bytes: defaults
                .max_context_bytes
                .min(
                    usize::try_from(self.reference_tokens.saturating_mul(7) / 2)
                        .unwrap_or(usize::MAX),
                )
                .max(1),
            ..defaults
        }
    }

    pub(super) fn validate_current(&self, executor: &crate::Executor) -> io::Result<()> {
        let current = resolve_model(executor, &self.model, None)?;
        if current.info.name != self.model
            || current.info.context_budget != self.context_tokens
            || current.info.compaction_trigger_threshold() != self.usable_tokens
            || current.info.max_output_tokens != self.max_output_tokens
            || current.provider() != self.provider.as_deref()
            || current.api_model != self.api_model
            || current.api != self.api
        {
            return Err(invalid(
                "question model configuration changed after preparation",
            ));
        }
        Ok(())
    }
}

pub(super) fn resolve_model(
    executor: &crate::Executor,
    requested: &str,
    effort: Option<&ReasoningSelection>,
) -> io::Result<crate::model_registry::ModelCallConfig> {
    let snapshot = executor.providers.models().snapshot();
    let concrete = snapshot.resolve_alias(requested);
    if concrete.is_empty() || snapshot.model_entry(&concrete).is_none() {
        return Err(invalid("question model is not configured"));
    }
    let model = snapshot.model_call_config(&concrete);
    if !model.provider_enabled() || model.info.context_budget == 0 {
        return Err(invalid(
            "question model is disabled or has no context budget",
        ));
    }
    if let Some(effort) = effort {
        model.resolve_reasoning(effort).map_err(invalid)?;
    }
    if executor
        .providers
        .resolve_call(&model)
        .map_err(invalid)?
        .is_none()
    {
        return Err(invalid("question model has no configured provider"));
    }
    Ok(model)
}

pub(super) fn content_tokens(content: &UserContent, turn: &TurnId) -> u64 {
    let mut message = Message::user_text(turn.clone(), content.text.clone());
    message.parts.extend(
        content
            .images
            .iter()
            .cloned()
            .map(|source| MessagePart::Image { source }),
    );
    crate::compaction::estimate_tokens_for_messages(&[message])
}

pub(super) async fn prepare(
    preparation: &QuestionPreparation,
    configuration: &crate::flow_store::SessionConfiguration,
    project: Option<&crate::storage::ProjectContext>,
    address: &FlowRef,
    requested_model: &str,
    effort: Option<&ReasoningSelection>,
    conversation: impl Into<ConversationBudget>,
) -> io::Result<QuestionModelBudget> {
    let conversation = conversation.into();
    let checking_window = matches!(&conversation, ConversationBudget::Window { .. });
    let model = resolve_model(&preparation.executor, requested_model, effort)?;
    let entry = preparation.entry.clone();
    let tools = preparation.executor.tools.clone();
    let roots = crate::flow_store::SessionDescriptor {
        id: address.session_id.0,
        configuration: configuration.clone(),
        project: project.cloned(),
        title: String::new(),
    }
    .source_roots();
    let (system, specs) = crate::read_work::capture(move || entry_inputs(&entry, &tools, &roots))
        .await
        .map_err(io::Error::other)??;
    let mut system = Some(format!(
        "{system}\n\n{}",
        crate::context_plan::CONTEXT_RECORD_INSTRUCTIONS
    ));
    let cwd = preparation
        .authority
        .workspace_root
        .as_deref()
        .map(crate::fs_access::canonicalize_stable);
    crate::eval::llm_dispatch::normalize_working_directory_context(&mut system, cwd.as_deref());
    let fixed_tokens = crate::context_plan::estimate_fixed_input_tokens(&system, &specs);
    let (fixed_tokens, conversation_tokens, minimum_reference) = match conversation {
        ConversationBudget::Estimate(tokens) => {
            let context = crate::session::Session::restore_instance_context(
                crate::event::EventSink::new(),
                address.clone(),
                configuration.clone(),
                project.cloned(),
                vec![],
                vec![],
                None,
                None,
                None,
            );
            context
                .bind_provider_registry(&preparation.executor.providers)
                .map_err(invalid)?;
            let mut runtime = crate::eval::session_context_record_specs(&context).await;
            runtime.extend(crate::eval::llm_dispatch::runtime_context_record_specs(
                None,
            ));
            let runtime_tokens = compile_context_records(&[], runtime)
                .iter()
                .map(|record| crate::provider::estimate_tokens(&record.render_for_model()))
                .sum::<u64>();
            (fixed_tokens.saturating_add(runtime_tokens), tokens, 512)
        }
        ConversationBudget::Window {
            mut messages,
            runtime,
        } => {
            let turn = TurnId::now();
            let records = compile_context_records(&messages, runtime);
            messages.extend(
                records
                    .into_iter()
                    .map(|record| Message::context_record(turn.clone(), record)),
            );
            let arguments = crate::eval::llm_args::parse_llm_args_from_toolargs(
                &crate::tool::ToolArgs {
                    named: vec![(
                        "context_latest".into(),
                        crate::Value::List(vec![crate::Value::Str("quickask.source".into())]),
                    )],
                    ..Default::default()
                },
                &preparation.executor.tools,
            )
            .map_err(invalid)?;
            let handle = Arc::new(std::sync::Mutex::new(messages));
            let projection = crate::eval::llm_context::build_llm_context(
                &arguments,
                crate::eval::ContextMode::Session,
                None,
                Some(&handle),
                &turn,
                None,
                None,
            )
            .map_err(|error| match error {
                crate::Value::Err(error) => invalid(error),
                _ => invalid("question model context projection failed"),
            })?;
            let total = crate::compaction::estimate_tokens_for_messages(&projection.messages);
            let retained = without_reference(projection.messages);
            let tokens = crate::compaction::estimate_tokens_for_messages(&retained);
            (fixed_tokens, tokens, total.saturating_sub(tokens))
        }
    };
    let usable_tokens = model.info.compaction_trigger_threshold();
    let reference_tokens = usable_tokens
        .saturating_sub(fixed_tokens)
        .saturating_sub(conversation_tokens);
    if fixed_tokens
        .saturating_add(conversation_tokens)
        .saturating_add(minimum_reference)
        > usable_tokens
    {
        if !checking_window {
            return Err(invalid(
                "question and retained conversation leave no room for a source reference",
            ));
        }
        return Err(invalid(format!(
            "Model {} cannot fit the retained question context (estimated {} tokens, usable budget {}). Refresh the source reference or start a new Quick Ask with this model; the current model is unchanged",
            model.info.name,
            fixed_tokens
                .saturating_add(conversation_tokens)
                .saturating_add(minimum_reference),
            usable_tokens,
        )));
    }
    Ok(QuestionModelBudget {
        model: model.info.name.clone(),
        context_tokens: model.info.context_budget,
        usable_tokens,
        fixed_tokens,
        conversation_tokens,
        reference_tokens,
        provider: model.provider().map(str::to_owned),
        api_model: model.api_model,
        api: model.api,
        max_output_tokens: model.info.max_output_tokens,
    })
}

fn entry_inputs(
    entry: &EntrySnapshot,
    tools: &crate::tool::ToolRegistry,
    roots: &crate::source_program::SourceRoots,
) -> io::Result<(String, Vec<crate::tool::ToolSpec>)> {
    let program =
        crate::source_program::load_program_from_bundle(&entry.bundle, &entry.path, roots)
            .map_err(invalid)?;
    let flow_id = program
        .entry_flow(&entry.name)
        .ok_or_else(|| invalid("question entry is absent from its captured revision"))?;
    let flow = program
        .flow(&flow_id)
        .ok_or_else(|| invalid("question entry is absent from its captured revision"))?;
    if crate::validate::invocation_message_parameter(flow).map_err(invalid)? != Some("question")
        || crate::flow_authority::contract_allows_shell(flow.contract.as_ref())
        || crate::goal::is_managed(flow)
    {
        return Err(invalid(
            "question entry must declare a question message and deny shell access and Goal continuation",
        ));
    }
    let arguments = llm_arguments(&flow.body)
        .ok_or_else(|| invalid("question entry has no explicit model call to prepare"))?;
    let argument = |key| {
        arguments.iter().find_map(|argument| match argument {
            Arg::Named { name, value } if name.name == key => Some(value),
            _ => None,
        })
    };
    if !matches!(argument("model"), Some(Expr::Ident(name)) if name.name == "model")
        || !matches!(argument("cache"), Some(Expr::Literal(Literal::Bool(true))))
        || !matches!(argument("context"), Some(Expr::Literal(Literal::Str(mode))) if mode == "session")
        || !matches!(argument("context_latest"), Some(Expr::List(keys)) if matches!(keys.as_slice(), [Expr::Literal(Literal::Str(key))] if key == "quickask.source"))
    {
        return Err(invalid(
            "question entry must use its model argument, cache, session context and latest source reference",
        ));
    }
    let system = match argument("system") {
        Some(Expr::Literal(Literal::Str(text))) => text.clone(),
        Some(Expr::FileRef(reference)) => {
            let path = Path::new(&reference.path);
            let path = if path.is_absolute() {
                path.to_owned()
            } else {
                program
                    .source_dir(&flow_id)
                    .unwrap_or_else(|| Path::new("."))
                    .join(path)
            };
            const MAX_PROMPT_BYTES: u64 = 1024 * 1024;
            let mut text = String::new();
            std::fs::File::open(path)?
                .take(MAX_PROMPT_BYTES + 1)
                .read_to_string(&mut text)?;
            if text.len() as u64 > MAX_PROMPT_BYTES {
                return Err(invalid("question policy exceeds the bounded prompt size"));
            }
            text
        }
        _ => {
            return Err(invalid(
                "question policy must be an explicit string or captured file reference",
            ));
        }
    };
    let Some(Expr::List(selectors)) = argument("tools") else {
        return Err(invalid("question tools must be an explicit list"));
    };
    let values = selectors
        .iter()
        .map(|selector| match selector {
            Expr::Literal(Literal::Str(name))
                if matches!(
                    name.as_str(),
                    "fs.read"
                        | "fs.list"
                        | "fs.grep"
                        | "image.read"
                        | "output.read"
                        | "final.answer"
                ) =>
            {
                Ok(crate::Value::Str(name.clone()))
            }
            _ => Err(invalid(
                "question entry contains an unsupported model tool selector",
            )),
        })
        .collect::<io::Result<Vec<_>>>()?;
    let specs =
        crate::eval::llm_args::resolve_tool_specs_from_values(&values, tools).map_err(invalid)?;
    Ok((system, specs))
}

fn llm_arguments(body: &[Stmt]) -> Option<&[Arg]> {
    body.iter().find_map(|statement| match statement {
        Stmt::Bind {
            value: Expr::Node(Node::ToolCall { path, args }),
            ..
        }
        | Stmt::Expr(Expr::Node(Node::ToolCall { path, args }))
            if path
                .iter()
                .map(|part| part.name.as_str())
                .eq(["llm", "call"]) =>
        {
            Some(args.as_slice())
        }
        Stmt::Loop { body } | Stmt::When { body, .. } => llm_arguments(body),
        _ => None,
    })
}
