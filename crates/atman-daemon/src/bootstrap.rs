use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use atman_runtime::event::EventSink;
use atman_runtime::providers::anthropic::AnthropicProvider;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::providers::openai::OpenAiProvider;
use atman_runtime::sandbox::Sandbox;
use atman_runtime::{Executor, Value, tools};

pub use atman_runtime::config_hub::{RedactConfig, SandboxConfig};

pub struct BootstrapOptions {
    pub events: EventSink,
    pub mock: bool,
    pub config_dir: Option<PathBuf>,
    pub project_root: PathBuf,
    pub home_dir: Option<PathBuf>,
    pub workspace_generation: String,
}

pub fn load_redact_config(config_dir: Option<&Path>) -> RedactConfig {
    let Some(dir) = config_dir else {
        return RedactConfig::default();
    };
    atman_runtime::config_hub::ConfigHub::from_config_dir(dir)
        .redact_config()
        .unwrap_or_default()
}

pub fn build_redactor(config_dir: Option<&Path>) -> Option<Arc<atman_runtime::redact::Redactor>> {
    let cfg = load_redact_config(config_dir);
    if !cfg.enabled {
        return None;
    }
    let mut pairs: Vec<(&str, &str)> = atman_runtime::redact::BUILTIN_PATTERNS.to_vec();
    for (k, r) in &cfg.custom_patterns {
        pairs.push((k.as_str(), r.as_str()));
    }
    let mode = if cfg.partial {
        atman_runtime::redact::RedactMode::Partial
    } else {
        atman_runtime::redact::RedactMode::Full
    };
    let redactor =
        atman_runtime::redact::Redactor::from_pairs(&pairs, mode).with_allowlist(cfg.allowlist);
    Some(Arc::new(redactor))
}

pub struct BootstrapOutcome {
    pub executor: Executor,
}

/// Spawn background MCP connections on a single thread using cooperative
/// concurrency (tokio::task::spawn_local).  Each enabled server connects
/// independently — fast servers don't wait for slow ones.  Tools are
/// registered as each server connects and become visible on the next
/// LLM call via `"mcp.*"`.
pub fn spawn_mcp_boot(
    executor: atman_runtime::Executor,
    session: std::sync::Arc<atman_runtime::Session>,
    config_dir: Option<&std::path::Path>,
) -> Option<tokio::sync::oneshot::Sender<()>> {
    let configs = match config_dir {
        Some(dir) => atman_runtime::config_hub::ConfigHub::from_config_dir(dir).load_mcp(),
        None => atman_runtime::config_hub::ConfigHub::global()
            .map(|hub| hub.load_mcp())
            .unwrap_or_default(),
    };
    if configs.is_empty() {
        return None;
    }
    // Initialise all servers: disabled → Disabled, rest → Pending.
    for cfg in &configs {
        let state = if cfg.disabled {
            atman_runtime::mcp::McpServerState::Disabled
        } else {
            atman_runtime::mcp::McpServerState::Pending
        };
        session.update_mcp_server(atman_runtime::mcp::McpServerStatus {
            name: cfg.name.clone(),
            transport: cfg.transport,
            state,
        });
    }
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("mcp runtime");
        let local = tokio::task::LocalSet::new();
        local.block_on(&rt, async move {
            let mut tasks = Vec::new();
            for cfg in &configs {
                if cfg.disabled {
                    continue;
                }
                let executor = executor.clone();
                let session = session.clone();
                let name = cfg.name.clone();
                let command = cfg.command.clone();
                let args = cfg.args.clone();
                let env = cfg.env.clone();
                let url = cfg.url.clone();
                let auth_token = cfg.auth_token.clone();
                let timeout_ms = cfg.timeout_ms;
                let tier = cfg.tier;
                let transport = cfg.transport;
                tasks.push(tokio::task::spawn_local(async move {
                    // Connecting
                    session.update_mcp_server(atman_runtime::mcp::McpServerStatus {
                        name: name.clone(),
                        transport,
                        state: atman_runtime::mcp::McpServerState::Connecting,
                    });
                    let outcome = match transport {
                        atman_runtime::mcp::TransportKind::Stdio => {
                            atman_runtime::mcp::McpClient::connect_stdio(
                                &name, &command, &args, &env, timeout_ms,
                            )
                            .await
                        }
                        atman_runtime::mcp::TransportKind::Http => match url.as_deref() {
                            Some(url) => {
                                atman_runtime::mcp::McpClient::connect_http(
                                    &name, url, auth_token, timeout_ms,
                                )
                                .await
                            }
                            None => Err(atman_runtime::mcp::McpError::Protocol(
                                "http transport requires `url`".into(),
                            )),
                        },
                        atman_runtime::mcp::TransportKind::Sse => match url.as_deref() {
                            Some(url) => {
                                atman_runtime::mcp::McpClient::connect_http(
                                    &name, url, auth_token, timeout_ms,
                                )
                                .await
                            }
                            None => Err(atman_runtime::mcp::McpError::Protocol(
                                "sse transport requires `url`".into(),
                            )),
                        },
                    };
                    match outcome {
                        Ok(client) => {
                            let tool_count = client.tools.len();
                            let providers = executor.providers.clone();
                            client.set_sampling_handler(std::sync::Arc::new(
                                move |req: atman_runtime::mcp::SamplingRequest| {
                                    let providers = providers.clone();
                                    Box::pin(async move {
                                        let model = req.model.unwrap_or_default();
                                        let provider =
                                            providers.resolve(&model).ok_or_else(|| {
                                                atman_runtime::RuntimeError::ToolFailed(format!(
                                                    "no provider for model `{model}`"
                                                ))
                                            })?;
                                        let messages: Vec<atman_runtime::Message> = req
                                            .messages
                                            .into_iter()
                                            .map(|m| {
                                                let text = match &m.content {
                                                    serde_json::Value::String(s) => s.clone(),
                                                    other => other.to_string(),
                                                };
                                                let role = match m.role.as_str() {
                                                    "assistant" => {
                                                        atman_runtime::MessageRole::Assistant
                                                    }
                                                    "system" => atman_runtime::MessageRole::System,
                                                    _ => atman_runtime::MessageRole::User,
                                                };
                                                atman_runtime::Message {
                                                    role,
                                                    parts: vec![atman_runtime::MessagePart::Text {
                                                        text,
                                                    }],
                                                    turn_id: atman_runtime::TurnId::now(),
                                                    origin:
                                                        atman_runtime::message::MessageOrigin::User,
                                                }
                                            })
                                            .collect();
                                        let llm_req = atman_runtime::provider::LlmRequest {
                                            model: model.clone(),
                                            messages,
                                            system: req.system_prompt,
                                            input: atman_runtime::Value::Unit,
                                            schema: None,
                                            cache_prompt: false,
                                            tools: Vec::new(),
                                            thinking_enabled: false,
                                            stall_timeout_secs: 0,
                                        };
                                        let am = provider.call(llm_req).await?;
                                        Ok(atman_runtime::mcp::SamplingResponse {
                                            role: "assistant".into(),
                                            content: serde_json::json!(am.text_concat()),
                                            model: Some(model),
                                        })
                                    })
                                },
                            ));
                            let arc_client = std::sync::Arc::new(client);
                            for tool in &arc_client.tools {
                                let adapter = atman_runtime::mcp::McpToolAdapter::new(
                                    arc_client.clone(),
                                    &tool.name,
                                    tier,
                                    tool.input_schema.as_ref(),
                                    tool.description.as_deref(),
                                );
                                executor.tools.register(std::sync::Arc::new(adapter));
                            }
                            session.update_mcp_server(atman_runtime::mcp::McpServerStatus {
                                name,
                                transport,
                                state: atman_runtime::mcp::McpServerState::Connected {
                                    tool_count,
                                    tools: arc_client
                                        .tools
                                        .iter()
                                        .map(|t| atman_runtime::mcp::McpToolInfo {
                                            name: t.name.clone(),
                                            description: t.description.clone(),
                                        })
                                        .collect(),
                                },
                            });
                        }
                        Err(e) => {
                            atman_runtime::notify!(error, "MCP `{}` failed: {e}", name);
                            session.update_mcp_server(atman_runtime::mcp::McpServerStatus {
                                name,
                                transport,
                                state: atman_runtime::mcp::McpServerState::Error {
                                    message: e.to_string(),
                                },
                            });
                        }
                    }
                }));
            }
            for t in tasks {
                let _ = t.await;
            }
            // Keep runtime alive until shutdown signal (for hot-reload).
            let _ = shutdown_rx.await;
        });
    });
    Some(shutdown_tx)
}

pub async fn build_executor(opts: BootstrapOptions) -> Result<BootstrapOutcome> {
    let events = opts.events.clone();
    let mut executor = Executor::with_events(events);
    let workspace_service = atman_runtime::flow_workspace::FlowWorkspaceService::new(
        &opts.project_root,
        None,
        &opts.workspace_generation,
    )?;
    executor.tool_ctx = executor
        .tool_ctx
        .clone()
        .with_flow_workspace_service(Arc::new(workspace_service));

    let rule_fetch = build_rule_fetch(&opts.project_root, opts.home_dir.as_deref()).await;
    tools::register_tier_zero_with_rules(&executor.tools, rule_fetch);
    tools::register_git_ops(&executor.tools);
    tools::register_watch(&executor.tools);
    let task_registry = atman_runtime::TaskRegistry::new();
    let bg_registry =
        tools::register_bash_bg_with_task_registry(&executor.tools, task_registry.clone());
    let term_registry =
        tools::register_terminal_with_task_registry(&executor.tools, task_registry.clone());
    executor.tools.register(std::sync::Arc::new(
        atman_runtime::tools::task_ops::TaskList,
    ));
    executor.tools.register(std::sync::Arc::new(
        atman_runtime::tools::task_ops::TaskKill,
    ));
    let trust_config = load_trust_config(opts.config_dir.as_deref());
    let tool_output_budget = opts
        .config_dir
        .as_deref()
        .map(atman_runtime::config_hub::ConfigHub::from_config_dir)
        .and_then(|hub| hub.tool_output_budget().ok())
        .unwrap_or_default();
    executor.tool_ctx = executor
        .tool_ctx
        .clone()
        .with_bg_registry(bg_registry)
        .with_term_registry(term_registry)
        .with_task_registry(task_registry)
        .with_trust(trust_config);
    executor.tool_ctx.tool_output_budget = tool_output_budget;
    tools::register_preview(
        &executor.tools,
        load_preview_config(opts.config_dir.as_deref()),
    );
    let web_config = load_web_config(opts.config_dir.as_deref());
    tools::register_web(&executor.tools, web_config.fetch);
    tools::register_web_search(&executor.tools, &web_config.search);
    register_providers_from_env(&mut executor).await;
    if let Some(sandbox) =
        build_sandbox(&opts.project_root, opts.config_dir.as_deref()).context("sandbox init")?
    {
        executor.tool_ctx = executor.tool_ctx.clone().with_sandbox(sandbox);
    }
    if opts.mock {
        executor.providers.register(Arc::new(
            MockProvider::new("mock").with_fallback(Value::Str("[mock response]".into())),
        ));
        use atman_runtime::model_registry::{ModelConfig, ModelEntry};
        let mut models = std::collections::HashMap::new();
        models.insert(
            "mock".into(),
            ModelEntry {
                model: "mock".into(),
                context_budget: Some(200_000),
                ..Default::default()
            },
        );
        atman_runtime::model_registry::set_model_config(ModelConfig {
            models,
            providers: std::collections::HashMap::new(),
            aliases: std::collections::HashMap::new(),
        });
    }
    Ok(BootstrapOutcome { executor })
}

fn build_sandbox(
    project_root: &Path,
    config_dir: Option<&Path>,
) -> Result<Option<Arc<dyn atman_runtime::sandbox::Sandbox>>> {
    let cfg = load_sandbox_config(config_dir);
    if !cfg.enabled {
        return Ok(None);
    }
    let template = match &cfg.template_path {
        Some(p) => std::fs::read_to_string(p)
            .with_context(|| format!("read sandbox template {}", p.display()))?,
        None => atman_runtime::sandbox::DEFAULT_PROFILE.to_string(),
    };
    let sandbox = atman_runtime::sandbox::SandboxExec::new(project_root)
        .with_extra_read(cfg.extra_read.clone())
        .with_extra_write(cfg.extra_write.clone())
        .with_allow_network(cfg.allow_network)
        .with_template(template);
    if !sandbox.is_available() {
        if cfg.strict {
            anyhow::bail!("sandbox enabled + strict, but sandbox-exec not available on this host");
        }
        atman_runtime::notify!(
            warn,
            "sandbox enabled but sandbox-exec not available; falling back to no-sandbox path"
        );
        return Ok(None);
    }
    Ok(Some(Arc::new(sandbox)))
}

pub fn load_sandbox_config(config_dir: Option<&Path>) -> SandboxConfig {
    let Some(dir) = config_dir else {
        return SandboxConfig::default();
    };
    atman_runtime::config_hub::ConfigHub::from_config_dir(dir)
        .sandbox_config()
        .unwrap_or_default()
}

pub fn attach_memory_stores(
    executor: &mut Executor,
    session: &atman_runtime::Session,
    project_scope_root: &Path,
) {
    attach_memory_stores_with_redactor(
        executor,
        session.dir(),
        project_scope_root,
        None,
        None,
        session.goal_watch().clone(),
        session.todos_watch().clone(),
        session.plans_watch().clone(),
    );
}

#[allow(clippy::too_many_arguments)]
pub fn attach_memory_stores_with_redactor(
    executor: &mut Executor,
    session_dir: &Path,
    project_scope_root: &Path,
    redactor: Option<Arc<atman_runtime::redact::Redactor>>,
    project_index: Option<Arc<atman_runtime::index::AnchorIndex>>,
    goal_watch: tokio::sync::watch::Sender<Option<String>>,
    todos_watch: tokio::sync::watch::Sender<Vec<atman_runtime::memory::todo::Todo>>,
    plans_watch: tokio::sync::watch::Sender<Vec<atman_runtime::memory::plan::Plan>>,
) {
    let confession_root = project_scope_root.join("confessions");
    let spec_root = project_scope_root.join("specs");
    let _ = std::fs::create_dir_all(&confession_root);
    let _ = std::fs::create_dir_all(&spec_root);
    let todo_store =
        Arc::new(atman_runtime::memory::todo::TodoStore::at(session_dir).with_notify(todos_watch));
    let goal_store =
        Arc::new(atman_runtime::memory::goal::GoalStore::at(session_dir).with_notify(goal_watch));
    let plan_store =
        Arc::new(atman_runtime::memory::plan::PlanStore::at(session_dir).with_notify(plans_watch));
    let mut confession_store =
        atman_runtime::memory::confession::ConfessionStore::at(&confession_root);
    let mut spec_store = atman_runtime::memory::spec::SpecStore::new(spec_root);
    if let Some(idx) = &project_index {
        confession_store = confession_store.with_index(idx.clone());
        spec_store = spec_store.with_index(idx.clone());
    }
    if let Some(r) = &redactor {
        confession_store = confession_store.with_redactor(r.clone());
    }
    let confession_store = Arc::new(confession_store);
    let spec_store = Arc::new(spec_store);
    tools::register_memory(
        &executor.tools,
        todo_store,
        confession_store,
        goal_store,
        plan_store,
    );
    tools::register_spec_memory(&executor.tools, spec_store);
}

async fn build_rule_fetch(
    project_root: &Path,
    home: Option<&Path>,
) -> atman_runtime::tools::memory_stubs::RuleFetch {
    let rule_fetch = atman_runtime::tools::memory_stubs::RuleFetch::new();
    if std::env::var("ATMAN_DISABLE_MIGRATION").is_ok() {
        return rule_fetch;
    }
    let Some(home) = home else {
        return rule_fetch;
    };
    let rules = atman_runtime::migration::scan_migrated_rules(project_root, home);
    rule_fetch.set_migrated(rules).await;
    rule_fetch
}

async fn register_providers_from_env(executor: &mut Executor) {
    register_providers_from_config(executor);
    atman_runtime::model_registry::register_all_preset_models();
    register_providers_from_auth_store(executor).await;
}

async fn register_providers_from_auth_store(executor: &mut Executor) {
    use atman_runtime::auth_store::ProviderKind;
    use atman_runtime::auth_store::cached_to_discovered;
    let Ok(store) = atman_runtime::auth_store::AuthStore::load() else {
        return;
    };
    for p in &store.providers {
        if !p.enabled {
            continue;
        }
        if p.kind == ProviderKind::Codex {
            // Hydrate cached models immediately so the UI has them from frame 0.
            if let Some(cache) = &p.model_cache {
                let cached = cached_to_discovered(cache);
                atman_runtime::model_registry::register_discovered_for_provider(
                    &p.id, &p.name, &cached,
                );
            }

            // Create provider (token refresh if needed) without model discovery.
            match atman_runtime::oauth::create_oauth_provider_no_discover::<
                atman_runtime::providers::codex::CodexProvider,
            >(p)
            .await
            {
                Ok(provider) => {
                    executor.providers.register(provider);
                }
                Err(e) => {
                    atman_runtime::notify!(
                        warn,
                        "codex provider {} ({}) failed to init: {e:#}",
                        p.name,
                        p.id
                    );
                }
            }
        }
    }
}

fn register_providers_from_config(executor: &mut Executor) {
    for (name, entry) in atman_runtime::model_registry::all_provider_entries() {
        if entry.enabled == Some(false) {
            continue;
        }
        // Resolve API key: api_key_env -> api_key -> kind-based env fallback
        let key = entry
            .api_key_env
            .as_deref()
            .and_then(|env| std::env::var(env).ok().filter(|v| !v.trim().is_empty()))
            .or_else(|| entry.api_key.clone().filter(|k| !k.is_empty()))
            .or_else(|| match entry.kind.as_str() {
                "openai" | "openai-compat" => std::env::var("OPENAI_API_KEY")
                    .ok()
                    .filter(|v| !v.is_empty()),
                "anthropic" => std::env::var("ANTHROPIC_API_KEY")
                    .ok()
                    .filter(|v| !v.is_empty()),
                _ => None,
            })
            .unwrap_or_default();

        let needs_key = matches!(
            entry.kind.as_str(),
            "openai" | "openai-compat" | "anthropic"
        );
        if needs_key && key.is_empty() {
            continue;
        }

        // Resolve base_url: config -> kind-based env override
        let base_url = entry
            .base_url
            .clone()
            .or_else(|| match entry.kind.as_str() {
                "openai" | "openai-compat" => std::env::var("OPENAI_BASE_URL").ok(),
                "anthropic" => std::env::var("ANTHROPIC_BASE_URL").ok(),
                _ => None,
            });

        let provider_name = format!("config:{name}");
        match entry.kind.as_str() {
            "anthropic" => {
                let mut p = AnthropicProvider::new(&provider_name, &key);
                if let Some(url) = &base_url {
                    p = p.with_base_url(url);
                }
                if let Some(mt) = entry.max_tokens {
                    p = p.with_max_tokens(mt);
                }
                executor.providers.register(Arc::new(p));
            }
            "openai" | "openai-compat" => {
                let mut p = OpenAiProvider::new(&provider_name, &key);
                if let Some(url) = &base_url {
                    p = p.with_base_url(url);
                }
                if let Some(mt) = entry.max_tokens {
                    p = p.with_max_tokens(mt);
                }
                executor.providers.register(Arc::new(p));
            }
            _ => {}
        }
    }
}

pub fn load_preview_config(
    config_dir: Option<&Path>,
) -> atman_runtime::tools::preview::PreviewConfig {
    let Some(dir) = config_dir else {
        return atman_runtime::tools::preview::PreviewConfig::default();
    };
    atman_runtime::config_hub::ConfigHub::from_config_dir(dir)
        .preview_config()
        .unwrap_or_default()
}

pub fn default_config_dir() -> Result<PathBuf> {
    atman_runtime::storage::config_dir()
}

#[derive(Debug, Clone, Default)]
pub struct WebConfig {
    pub fetch: atman_runtime::tools::web::WebConfig,
    pub search: atman_runtime::tools::web::SearchConfig,
}

pub fn load_web_config(config_dir: Option<&Path>) -> WebConfig {
    let Some(dir) = config_dir else {
        return WebConfig::default();
    };
    let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(dir);
    WebConfig {
        fetch: hub.web_fetch_config().unwrap_or_default(),
        search: hub.web_search_config().unwrap_or_default(),
    }
}

pub fn load_trust_config(config_dir: Option<&Path>) -> atman_runtime::trust::TrustConfig {
    let Some(dir) = config_dir else {
        return atman_runtime::trust::TrustConfig::default();
    };
    atman_runtime::config_hub::ConfigHub::from_config_dir(dir)
        .trust_config()
        .unwrap_or_default()
}

pub fn default_data_dir() -> Result<PathBuf> {
    atman_runtime::storage::data_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn build_executor_injects_tool_output_budget() {
        let config = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            config.path().join("config.toml"),
            "[trust]\nmode = \"eager\"\n[tool_output]\nmax_lines = 7\nmax_bytes = 777\nmax_line_bytes = 111\n",
        )
        .unwrap();

        let outcome = build_executor(BootstrapOptions {
            events: EventSink::new(),
            mock: true,
            config_dir: Some(config.path().to_path_buf()),
            project_root: project.path().to_path_buf(),
            home_dir: Some(home.path().to_path_buf()),
            workspace_generation: "bootstrap-test-generation".into(),
        })
        .await
        .unwrap();

        assert_eq!(
            outcome.executor.tool_ctx.tool_output_budget,
            atman_runtime::tools::tool_output::ToolOutputBudget {
                max_lines: 7,
                max_bytes: 777,
                max_line_bytes: 111,
            }
        );
    }

    #[test]
    fn load_web_config_preserves_subdomain_error_isolation() {
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(
            dir.path().join("config.toml"),
            "[web]\nmax_bytes = \"large\"\n[web.search]\nprovider = \"none\"\n",
        )
        .unwrap();
        let invalid_fetch = load_web_config(Some(dir.path()));
        assert_eq!(invalid_fetch.fetch.max_bytes, 1_000_000);
        assert_eq!(invalid_fetch.search.provider_name(), "none");

        std::fs::write(
            dir.path().join("config.toml"),
            "[web]\nmax_bytes = 2048\n[web.search]\nprovider = \"unknown\"\n",
        )
        .unwrap();
        let invalid_search = load_web_config(Some(dir.path()));
        assert_eq!(invalid_search.fetch.max_bytes, 2048);
        assert_eq!(invalid_search.search.provider_name(), "tavily");
    }

    #[test]
    fn load_trust_config_uses_all_hub_fields_and_defaults_on_error() {
        use atman_runtime::trust::{EscalationPolicy, Theme, TrustMode};

        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[trust]\nmode = \"eager\"\ntheme = \"weather\"\nescalation = \"deny\"\n",
        )
        .unwrap();

        let configured = load_trust_config(Some(dir.path()));
        assert_eq!(configured.mode, TrustMode::Eager);
        assert_eq!(configured.theme, Theme::Weather);
        assert_eq!(configured.escalation, EscalationPolicy::Deny);

        std::fs::write(
            dir.path().join("config.toml"),
            "[trust]\nmode = \"invalid\"\n",
        )
        .unwrap();
        let invalid = load_trust_config(Some(dir.path()));
        assert_eq!(invalid.mode, TrustMode::Steady);
        assert_eq!(invalid.theme, Theme::Default);
        assert_eq!(invalid.escalation, EscalationPolicy::Ask);
    }

    #[test]
    fn load_preview_config_uses_hub_projection_and_defaults_on_error() {
        let dir = tempfile::tempdir().unwrap();
        let default = atman_runtime::tools::preview::PreviewConfig::default();

        let missing = load_preview_config(Some(dir.path()));
        assert_eq!(missing.base_url, default.base_url);
        assert_eq!(missing.timeout_ms, default.timeout_ms);

        std::fs::write(
            dir.path().join("config.toml"),
            "[preview]\nbase_url = \"http://127.0.0.1:9000\"\ntimeout_ms = 250\nmax_body_bytes = 4096\n",
        )
        .unwrap();
        let configured = load_preview_config(Some(dir.path()));
        assert_eq!(configured.base_url, "http://127.0.0.1:9000");
        assert_eq!(configured.timeout_ms, 250);
        assert_eq!(configured.max_body_bytes, 4096);

        std::fs::write(dir.path().join("config.toml"), "[preview\n").unwrap();
        let invalid = load_preview_config(Some(dir.path()));
        assert_eq!(invalid.base_url, default.base_url);
        assert_eq!(invalid.timeout_ms, default.timeout_ms);
        assert_eq!(invalid.max_body_bytes, default.max_body_bytes);
    }

    #[test]
    fn sandbox_config_defaults_to_enabled() {
        let cfg = SandboxConfig::default();
        assert!(cfg.enabled);
        assert!(!cfg.strict);
    }

    #[test]
    fn load_sandbox_config_defaults_missing_file_to_enabled() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_sandbox_config(Some(dir.path()));

        assert!(cfg.enabled);
        assert!(!cfg.strict);
        assert_eq!(cfg.extra_read, Vec::<PathBuf>::new());
    }

    #[test]
    fn load_sandbox_config_preserves_paths_and_explicit_values() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            r#"
[sandbox]
enabled = false
strict = true
extra_read = ["../read"]
extra_write = ["/tmp/write"]
template_path = "profiles/custom.sb"
allow_network = true
"#,
        )
        .unwrap();

        let cfg = load_sandbox_config(Some(dir.path()));
        assert!(!cfg.enabled);
        assert!(cfg.strict);
        assert_eq!(cfg.extra_read, vec![PathBuf::from("../read")]);
        assert_eq!(cfg.extra_write, vec![PathBuf::from("/tmp/write")]);
        assert_eq!(cfg.template_path, Some(PathBuf::from("profiles/custom.sb")));
        assert!(cfg.allow_network);
    }

    #[test]
    fn load_sandbox_config_keeps_invalid_toml_at_default() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[sandbox\n").unwrap();

        assert_eq!(
            load_sandbox_config(Some(dir.path())),
            SandboxConfig::default()
        );
    }

    #[test]
    fn load_redact_config_preserves_missing_file_default() {
        let dir = tempfile::tempdir().unwrap();

        assert_eq!(
            load_redact_config(Some(dir.path())),
            RedactConfig::default()
        );
        assert_eq!(load_redact_config(None), RedactConfig::default());
    }

    #[test]
    fn load_redact_config_and_build_redactor_use_hub_projection() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            r#"
[redact]
enabled = true
mode = "partial"
allowlist = ["safe@example.com"]
custom_patterns = [{ kind = "ticket", regex = "T-[0-9]+" }]
"#,
        )
        .unwrap();

        let config = load_redact_config(Some(dir.path()));
        assert!(config.enabled);
        assert!(config.partial);
        assert_eq!(config.custom_patterns.len(), 1);

        let redactor = build_redactor(Some(dir.path())).unwrap();
        assert!(
            redactor
                .scan("ticket T-42")
                .iter()
                .any(|hit| hit.kind == "ticket")
        );
        assert!(redactor.scan("safe@example.com").is_empty());
    }

    #[test]
    fn load_redact_config_keeps_invalid_toml_disabled() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.toml"), "[redact\n").unwrap();

        assert_eq!(
            load_redact_config(Some(dir.path())),
            RedactConfig::default()
        );
        assert!(build_redactor(Some(dir.path())).is_none());
    }
}
