//! Batch execution through the same persistent instance host used by peer flows.

use std::{
    num::NonZeroUsize,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, bail};
use atman_runtime::flow_authority::{EffectiveAuthority, contract_allows_shell};
use atman_runtime::flow_runtime::{host::RuntimeHost, launch::EntryExecution};
use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, RuntimeStore, SessionConfiguration,
    SessionDescriptor,
    completion::InvocationResult,
    inbox::{SenderIdentity, UserContent},
    launch::{EntryBinding, LaunchInput},
};
use atman_runtime::source_program::LinkedProgram;
use atman_runtime::{AtmanRuntime, InvocationEnv, Value, ValueJson};
use uuid::Uuid;

pub(crate) struct Options {
    pub flow_name: Option<String>,
    pub mock: bool,
    pub ephemeral: bool,
    pub reasoning: Option<String>,
    pub images: Vec<PathBuf>,
    pub raw_args: Vec<String>,
    pub snapshot_live: bool,
}

pub(crate) async fn run(file: &Path, program: &LinkedProgram, options: Options) -> Result<()> {
    let parsed = program.entry_file();
    let name = match options.flow_name {
        Some(name) => name,
        None if parsed.flows.len() == 1 => parsed.flows[0].name.name.clone(),
        None => bail!(
            "{} has {} flows; pass --flow=<name> to disambiguate",
            file.display(),
            parsed.flows.len()
        ),
    };
    let entry = parsed
        .flows
        .iter()
        .find(|flow| flow.name.name == name)
        .with_context(|| format!("no flow named `{name}` in {}", file.display()))?;
    let args = crate::parse_args(&options.raw_args, entry)?;
    let env = match options.reasoning {
        Some(value) => {
            let selection: atman_runtime::provider::ReasoningSelection = value
                .parse()
                .map_err(|error: String| anyhow::anyhow!("invalid --reasoning value: {error}"))?;
            InvocationEnv::single("effort", Value::Str(selection.to_string()))
        }
        None => InvocationEnv::default(),
    };
    let root = crate::data_dir()?;
    let config = atman_runtime::config_hub::ConfigHub::global()?;
    let project = atman_runtime::storage::ProjectContext::resolve(
        &config,
        &std::env::current_dir().context("reading current project root")?,
        &root,
    )?;
    let trust = config.trust_config()?;
    let authority = EffectiveAuthority::root(
        &trust,
        contract_allows_shell(entry.contract.as_ref()),
        Some(project.root().to_path_buf()),
    );
    let descriptor = SessionDescriptor {
        id: Uuid::now_v7(),
        title: name.clone(),
        project: Some(project.clone()),
        configuration: SessionConfiguration::Atman(config.source()),
    };
    let redactor = atman_daemon::bootstrap::build_redactor(Some(config.config_dir()));
    let mut events = atman_runtime::event::EventSink::new();
    if let Some(redactor) = &redactor {
        events = events.with_redactor(redactor.clone());
    }
    crate::load_model_config_from_disk();
    let AtmanRuntime {
        mut executor,
        provider_catalog_refresh_plan,
        ..
    } = AtmanRuntime::build(crate::bootstrap_opts_for(
        events,
        options.mock,
        config,
        project,
    ))
    .await?;
    if !options.mock
        && let Some(lifecycle) = executor.provider_lifecycle()
    {
        crate::consume_provider_catalog_refresh_plan(
            provider_catalog_refresh_plan,
            move |provider_id| {
                let lifecycle = lifecycle.clone();
                async move { lifecycle.refresh_models_if_stale(&provider_id).await }
            },
        )
        .await;
    }
    executor.source_dir = file.parent().map(Path::to_path_buf);
    executor.tool_ctx.prompt_resolver =
        Some(Arc::new(atman_runtime::rendezvous::AutoResolveResolver {
            default: serde_json::json!({"hunks": []}),
        }));
    if let Err(errors) = atman_runtime::validate::validate(entry, &executor.tools) {
        for error in &errors {
            atman_runtime::notify!(warn, "validation: {error}");
        }
        bail!("flow validation failed with {} error(s)", errors.len());
    }
    if options.snapshot_live && crate::load_auto_snapshot() {
        crate::auto_snapshot_flows(file, program);
    }
    let text = if args.is_empty() {
        name.clone()
    } else {
        args.iter()
            .map(|(key, value)| format!("{key}={}", crate::render_value(value)))
            .collect::<Vec<_>>()
            .join(" ")
    };
    // Image bytes belong to the accepted invocation, not to a temporary caller path.
    let attachments = atman_runtime::attachment_store::AttachmentStore::at(Path::new(""));
    let images = options
        .images
        .iter()
        .map(|path| attachments.import_path(path))
        .collect::<Result<Vec<_>, _>>()?;
    let arguments = args
        .iter()
        .map(|(name, value)| Ok((name.clone(), value.to_json()?)))
        .collect::<Result<_, atman_runtime::RuntimeError>>()?;
    let invocation = env
        .values()
        .map(|(name, value)| Ok((name.to_owned(), value.to_json()?)))
        .collect::<Result<_, atman_runtime::RuntimeError>>()?;
    // Ephemeral runs use identical storage semantics, with ownership drained before deletion.
    let scratch = options
        .ephemeral
        .then(|| tempfile::Builder::new().prefix("atman-run-").tempdir())
        .transpose()?;
    let data = scratch.as_ref().map_or(root.as_path(), |dir| dir.path());
    let store = RuntimeStore::open(data)?;
    let host = match redactor {
        Some(redactor) => RuntimeHost::with_redactor(redactor),
        None => RuntimeHost::default(),
    };
    let session = host.attach_session(store.create_session(descriptor.clone())?)?;
    let outcome: Result<Value> = async {
        let request = Uuid::now_v7();
        let address = FlowRef {
            session_id: descriptor.id.into(),
            instance_id: FlowInstanceId::now(),
        };
        let launched = host
            .launch_entry(
                FlowDescriptor {
                    address: address.clone(),
                    creation_request: request,
                    created_by: None,
                    content: EntrySnapshot {
                        name,
                        path: file.to_path_buf(),
                        bundle: program.revision_bundle(),
                    }
                    .into(),
                },
                LaunchInput {
                    display_label: None,
                    scope: None,
                    workspace: None,
                    initial_goal: None,
                    execution: Some(EntryBinding {
                        user_routes: false,
                        session_start: true,
                        first_turn: atman_rt::TurnId(request.into()),
                        first_run: atman_rt::RunId(request.into()),
                        authority: authority.clone(),
                    }),
                    request: atman_runtime::flow_store::launch::EntryRequest {
                        request_id: request,
                        sender: SenderIdentity::User(request),
                        recipient: address,
                        arguments,
                        invocation,
                        start: atman_runtime::flow_store::launch::EntryStart::Invoke {
                            user: Some(UserContent {
                                text,
                                images,
                                presentation: None,
                            }),
                        },
                    },
                    inherited_outputs: vec![],
                    inherited_context: vec![],
                    result_route: None,
                },
                EntryExecution {
                    executor,
                    authority,
                    trust,
                    yield_interval: NonZeroUsize::new(1024).unwrap(),
                    catalog: session.snapshot().await?.cursor,
                },
                |_, _| Ok(()),
            )
            .await?;
        atman_runtime::notify!(
            info,
            location = Log,
            "session={} flow={}",
            descriptor.id,
            launched.worker.address().instance_id
        );
        host.wait_quiescent().await?;
        let complete = launched
            .worker
            .completion(None)
            .await?
            .context("entry has no recorded completion")?;
        match complete.completion.result {
            InvocationResult::Returned(value)
                if complete.completion.outcome == atman_rt::TurnOutcome::Completed =>
            {
                Ok(value.restore()?)
            }
            InvocationResult::Returned(_) => {
                bail!("entry interrupted: {:?}", complete.completion.outcome)
            }
            InvocationResult::Failed(error) => Err(error.into()),
            InvocationResult::Unobserved(outcome) => {
                bail!("entry ended without a result: {outcome:?}")
            }
        }
    }
    .await;
    let shutdown = host.shutdown().await;
    let result = outcome?;
    shutdown?;
    println!("{}", crate::render_value(&result));
    Ok(())
}
