use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use atman_rt::Value as CoreValue;
use atman_runtime::{Executor, tools};

type Value = CoreValue<atman_runtime::AtmanPayload, atman_runtime::RuntimeError>;

use atman_runtime::config_hub::RedactConfig;

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

pub(crate) fn resolve_config_hub(
    config_dir: Option<&Path>,
) -> Result<atman_runtime::config_hub::ConfigHub> {
    match config_dir {
        Some(dir) => Ok(atman_runtime::config_hub::ConfigHub::from_config_dir(dir)),
        None => atman_runtime::config_hub::ConfigHub::global()
            .map_err(|error| anyhow::anyhow!("resolve config hub: {error}")),
    }
}

fn connected_mcp_status(
    name: String,
    transport: atman_runtime::mcp::TransportKind,
    snapshot: &atman_runtime::mcp::McpToolSnapshot,
) -> atman_runtime::mcp::McpServerStatus {
    atman_runtime::mcp::McpServerStatus {
        name,
        transport,
        state: atman_runtime::mcp::McpServerState::Connected {
            tool_count: snapshot.tools.len(),
            tools: snapshot
                .tools
                .iter()
                .map(|tool| atman_runtime::mcp::McpToolInfo {
                    name: tool.name.clone(),
                    description: tool.description.clone(),
                })
                .collect(),
        },
    }
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
    let mut retained_namespaces = configs
        .iter()
        .filter(|config| !config.disabled)
        .map(|config| atman_runtime::mcp::mcp_tool_namespace(&config.name))
        .collect::<Vec<_>>();
    retained_namespaces.extend(
        atman_runtime::tools::mcp::CONTROL_TOOL_NAMES
            .iter()
            .map(|name| (*name).to_string()),
    );
    executor
        .tools
        .retain_namespaces("mcp.", &retained_namespaces);
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
    for cfg in configs.iter().filter(|cfg| !cfg.disabled) {
        executor.tools.replace_namespace(
            &atman_runtime::mcp::mcp_tool_namespace(&cfg.name),
            Vec::new(),
        );
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
                                            input: Value::Unit,
                                            schema: None,
                                            cache_prompt: false,
                                            prompt_cache_key: None,
                                            tools: Vec::new(),
                                            reasoning: atman_runtime::provider::ReasoningSelection::ProviderDefault,
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
                            let mut notifications = arc_client.subscribe_notifications();
                            let snapshot = arc_client.tool_snapshot();
                            atman_runtime::mcp::publish_tool_snapshot(
                                &executor.tools,
                                arc_client.clone(),
                                tier,
                                &snapshot,
                            );
                            session.update_mcp_server(connected_mcp_status(
                                name.clone(),
                                transport,
                                &snapshot,
                            ));
                            let refresh_tools = executor.tools.clone();
                            let refresh_session = session.clone();
                            tokio::task::spawn_local(async move {
                                loop {
                                    let should_refresh = match notifications.recv().await {
                                        Ok(atman_runtime::mcp::McpNotification::ToolsListChanged) => {
                                            true
                                        }
                                        Ok(_) => false,
                                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                                            true
                                        }
                                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                            break;
                                        }
                                    };
                                    if !should_refresh {
                                        continue;
                                    }
                                    match arc_client.refresh_tool_snapshot().await {
                                        Ok(Some(snapshot)) => {
                                            atman_runtime::mcp::publish_tool_snapshot(
                                                &refresh_tools,
                                                arc_client.clone(),
                                                tier,
                                                &snapshot,
                                            );
                                            refresh_session.update_mcp_server(
                                                connected_mcp_status(
                                                    name.clone(),
                                                    transport,
                                                    &snapshot,
                                                ),
                                            );
                                        }
                                        Ok(None) => {}
                                        Err(error) => {
                                            atman_runtime::notify!(
                                                warn,
                                                "MCP `{}` tool refresh failed; keeping the previous snapshot: {error}",
                                                name
                                            );
                                        }
                                    }
                                }
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

pub fn default_config_dir() -> Result<PathBuf> {
    atman_runtime::storage::config_dir()
}

pub fn default_data_dir() -> Result<PathBuf> {
    atman_runtime::storage::data_dir()
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::atman_runtime::{
        load_preview_config, load_sandbox_config, load_trust_config, load_web_config,
    };
    use atman_runtime::config_hub::SandboxConfig;
    use atman_runtime::event::EventSink;

    #[test]
    fn runtime_build_injects_tool_output_budget() {
        let _registry_lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let config = tempfile::tempdir().unwrap();
                let project = tempfile::tempdir().unwrap();
                let home = tempfile::tempdir().unwrap();
                std::fs::write(
                    config.path().join("config.toml"),
                    "[trust]\nmode = \"eager\"\n[tool_output]\nmax_lines = 7\nmax_bytes = 777\nmax_line_bytes = 111\n",
                )
                .unwrap();

                let outcome = atman_runtime::AtmanRuntime::build(atman_runtime::AtmanRuntimeOptions {
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
            });
    }

    #[test]
    fn runtime_build_loads_config_providers_from_the_selected_config_dir() {
        struct ConfigReset;

        impl Drop for ConfigReset {
            fn drop(&mut self) {
                atman_runtime::model_registry::set_provider_config(Default::default());
            }
        }

        let _registry_lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _reset = ConfigReset;
        let mut stale = atman_runtime::model_registry::ProviderConfig::default();
        stale.providers.insert(
            "stale".into(),
            atman_runtime::model_registry::ProviderEntry {
                kind: "openai-compat".into(),
                api_key: Some("stale-key".into()),
                enabled: Some(true),
                ..Default::default()
            },
        );
        atman_runtime::model_registry::set_provider_config(stale);

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let config = tempfile::tempdir().unwrap();
                let project = tempfile::tempdir().unwrap();
                let home = tempfile::tempdir().unwrap();
                std::fs::write(
                    config.path().join("config.toml"),
                    r#"[providers.selected]
kind = "openai-compat"
api_key = "test-key"
base_url = "https://gateway.example/v1"
enabled = true
"#,
                )
                .unwrap();

                let outcome =
                    atman_runtime::AtmanRuntime::build(atman_runtime::AtmanRuntimeOptions {
                        events: EventSink::new(),
                        mock: false,
                        config_dir: Some(config.path().to_path_buf()),
                        project_root: project.path().to_path_buf(),
                        home_dir: Some(home.path().to_path_buf()),
                        workspace_generation: "selected-config-provider-test".into(),
                    })
                    .await
                    .unwrap();

                assert!(outcome.executor.providers.contains("config:selected"));
                assert!(!outcome.executor.providers.contains("config:stale"));
                let provider_names = atman_runtime::model_registry::all_provider_entries()
                    .into_iter()
                    .map(|(name, _)| name)
                    .collect::<Vec<_>>();
                assert_eq!(provider_names, vec!["selected"]);
            });
    }

    #[test]
    fn runtime_build_restores_oauth_provider_from_selected_config_dir() {
        const PROVIDER_ID: &str = "selected-config-oauth";

        struct CatalogCleanup;

        impl Drop for CatalogCleanup {
            fn drop(&mut self) {
                atman_runtime::model_registry::remove_provider_catalog(PROVIDER_ID);
            }
        }

        let _registry_lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        atman_runtime::model_registry::remove_provider_catalog(PROVIDER_ID);
        let _catalog_cleanup = CatalogCleanup;

        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let config = tempfile::tempdir().unwrap();
                let project = tempfile::tempdir().unwrap();
                let home = tempfile::tempdir().unwrap();
                let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(config.path());
                hub.add_auth_provider(atman_runtime::auth_store::StoredProvider {
                    id: PROVIDER_ID.into(),
                    name: "Selected OAuth".into(),
                    kind: atman_runtime::auth_store::ProviderKind::Codex,
                    access_token: "expired-access".into(),
                    refresh_token: None,
                    expires_at: chrono::Utc::now().timestamp() - 3_600,
                    account: Some("legacy-account-id".into()),
                    enabled: true,
                    model_cache: Some(atman_runtime::auth_store::ModelCache {
                        fetched_at: 1,
                        models: vec![atman_runtime::auth_store::CachedModel {
                            slug: "gpt-selected".into(),
                            context_budget: Some(128_000),
                            thinking: true,
                        }],
                    }),
                })
                .unwrap();
                hub.ensure_auth_model_namespace(PROVIDER_ID, "selected@OAuth")
                    .unwrap();

                let outcome =
                    atman_runtime::AtmanRuntime::build(atman_runtime::AtmanRuntimeOptions {
                        events: EventSink::new(),
                        mock: false,
                        config_dir: Some(config.path().to_path_buf()),
                        project_root: project.path().to_path_buf(),
                        home_dir: Some(home.path().to_path_buf()),
                        workspace_generation: "oauth-config-test-generation".into(),
                    })
                    .await
                    .unwrap();

                assert!(outcome.executor.providers.contains(PROVIDER_ID));
                assert_eq!(
                    outcome
                        .executor
                        .provider_lifecycle()
                        .unwrap()
                        .config_hub()
                        .config_dir(),
                    config.path()
                );
                let model = atman_runtime::model_registry::all_model_entries()
                    .into_iter()
                    .find_map(|(name, entry)| {
                        (entry.provider.as_deref() == Some(PROVIDER_ID)).then_some(name)
                    })
                    .expect("selected config catalog should be restored");
                assert_eq!(model, "selected@OAuth:gpt-selected");
                let provider = outcome
                    .executor
                    .providers
                    .resolve(&model)
                    .expect("selected config model should resolve to its live provider");
                assert_eq!(provider.name(), PROVIDER_ID);
                assert_eq!(outcome.provider_catalog_refresh_plan.len(), 1);
                assert_eq!(outcome.provider_catalog_refresh_plan[0], PROVIDER_ID);
            });
    }

    #[test]
    fn runtime_build_rejects_malformed_auth_state() {
        let _registry_lock = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let config = tempfile::tempdir().unwrap();
                let project = tempfile::tempdir().unwrap();
                std::fs::write(config.path().join("auth.json"), b"{not-json").unwrap();

                let result =
                    atman_runtime::AtmanRuntime::build(atman_runtime::AtmanRuntimeOptions {
                        events: EventSink::new(),
                        mock: false,
                        config_dir: Some(config.path().to_path_buf()),
                        project_root: project.path().to_path_buf(),
                        home_dir: None,
                        workspace_generation: "malformed-auth-test".into(),
                    })
                    .await;

                let error = result.err().expect("malformed auth must fail bootstrap");
                let rendered = format!("{error:#}");
                assert!(rendered.contains("auth.json"), "{rendered}");
            });
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
