use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use atman_runtime::{Executor, tools};

pub use atman_runtime::atman_runtime::{build_redactor, load_redact_config};

pub(crate) fn resolve_config_hub(
    config_dir: Option<&Path>,
) -> Result<atman_runtime::config_hub::ConfigHub> {
    match config_dir {
        Some(dir) => Ok(atman_runtime::config_hub::ConfigHub::from_config_dir(dir)),
        None => atman_runtime::config_hub::ConfigHub::global()
            .map_err(|error| anyhow::anyhow!("resolve config hub: {error}")),
    }
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
    let todo_store =
        Arc::new(atman_runtime::memory::todo::TodoStore::at(session_dir).with_notify(todos_watch));
    let goal_store =
        Arc::new(atman_runtime::memory::goal::GoalStore::at(session_dir).with_notify(goal_watch));
    let plan_store =
        Arc::new(atman_runtime::memory::plan::PlanStore::at(session_dir).with_notify(plans_watch));
    tools::register_file_memory(&executor.tools, todo_store, goal_store, plan_store);
    tools::register_project_memory(&executor.tools, project_scope_root, redactor, project_index);
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
    use atman_runtime::config_hub::{RedactConfig, SandboxConfig};
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
                    config: atman_runtime::config_hub::ConfigHub::from_config_dir(config.path()),
                    project: Some(atman_runtime::storage::ProjectContext::resolve(&atman_runtime::config_hub::ConfigHub::from_config_dir(config.path()), project.path(), &config.path().join("data")).unwrap()),
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
                        config: atman_runtime::config_hub::ConfigHub::from_config_dir(
                            config.path(),
                        ),
                        project: Some(
                            atman_runtime::storage::ProjectContext::resolve(
                                &atman_runtime::config_hub::ConfigHub::from_config_dir(
                                    config.path(),
                                ),
                                project.path(),
                                &config.path().join("data"),
                            )
                            .unwrap(),
                        ),
                        home_dir: Some(home.path().to_path_buf()),
                        workspace_generation: "selected-config-provider-test".into(),
                    })
                    .await
                    .unwrap();

                assert!(outcome.executor.providers.contains("config:selected"));
                assert!(!outcome.executor.providers.contains("config:stale"));
                let provider_names = outcome
                    .executor
                    .providers
                    .models()
                    .all_provider_entries()
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
                        config: atman_runtime::config_hub::ConfigHub::from_config_dir(
                            config.path(),
                        ),
                        project: Some(
                            atman_runtime::storage::ProjectContext::resolve(
                                &atman_runtime::config_hub::ConfigHub::from_config_dir(
                                    config.path(),
                                ),
                                project.path(),
                                &config.path().join("data"),
                            )
                            .unwrap(),
                        ),
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
                        config: atman_runtime::config_hub::ConfigHub::from_config_dir(
                            config.path(),
                        ),
                        project: Some(
                            atman_runtime::storage::ProjectContext::resolve(
                                &atman_runtime::config_hub::ConfigHub::from_config_dir(
                                    config.path(),
                                ),
                                project.path(),
                                &config.path().join("data"),
                            )
                            .unwrap(),
                        ),
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
