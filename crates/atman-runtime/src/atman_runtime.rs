use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config_hub::{ConfigHub, RedactConfig, SandboxConfig};
use crate::event::EventSink;
use crate::executor::Executor;
use crate::providers::mock::MockProvider;
use crate::sandbox::{Sandbox, SandboxExec};
use crate::tools;
use crate::value::Value;

pub struct AtmanRuntimeOptions {
    pub events: EventSink,
    pub mock: bool,
    pub config: ConfigHub,
    pub project: Option<crate::storage::ProjectContext>,
    pub home_dir: Option<PathBuf>,
    pub workspace_generation: String,
}

/// Atman-specific tools, providers, and flow executor for a project.
#[non_exhaustive]
pub struct AtmanRuntime {
    pub executor: Executor,
    /// Provider ids whose restored model catalogs need a conditional refresh.
    pub provider_catalog_refresh_plan: Vec<String>,
}

impl AtmanRuntime {
    pub async fn build(opts: AtmanRuntimeOptions) -> Result<Self> {
        let auth_hub = opts.config;
        let config_dir = Some(auth_hub.config_dir());
        if let Some(project) = &opts.project {
            project.validate_locations()?;
        }
        let project_root = opts.project.as_ref().map(|project| project.root());
        let mut executor = Executor::with_events(opts.events);
        executor.providers =
            crate::provider::ProviderRegistry::with_models(auth_hub.models().clone());
        executor.tool_ctx = crate::tool::ToolCtx::for_project(opts.project.clone());
        if let Some(root) = project_root {
            let workspace_service = crate::flow_workspace::FlowWorkspaceService::new(
                root,
                None,
                &opts.workspace_generation,
            )?;
            executor.tool_ctx = executor
                .tool_ctx
                .clone()
                .with_flow_workspace_service(Arc::new(workspace_service));
        }
        let rule_fetch = build_rule_fetch(project_root, opts.home_dir.as_deref()).await;
        tools::register_tier_zero_with_rules(&executor.tools, rule_fetch);
        tools::register_flow_memory(&executor.tools);
        if let Some(project) = &opts.project {
            tools::register_project_memory(
                &executor.tools,
                project.storage_root(),
                build_redactor(config_dir),
                None,
            );
        }
        tools::register_git_ops(&executor.tools);
        tools::register_watch(&executor.tools);
        let task_registry = crate::TaskRegistry::new();
        let bg_registry =
            tools::register_bash_bg_with_task_registry(&executor.tools, task_registry.clone());
        let term_registry =
            tools::register_terminal_with_task_registry(&executor.tools, task_registry.clone());
        executor.tools.register(Arc::new(tools::task_ops::TaskList));
        executor.tools.register(Arc::new(tools::task_ops::TaskKill));
        let trust_config = load_trust_config(config_dir);
        let tool_output_budget = auth_hub.tool_output_budget().unwrap_or_default();
        executor.tool_ctx = executor
            .tool_ctx
            .clone()
            .with_bg_registry(bg_registry)
            .with_term_registry(term_registry)
            .with_task_registry(task_registry)
            .with_trust(trust_config);
        executor.tool_ctx.tool_output_budget = tool_output_budget;
        tools::register_preview(&executor.tools, load_preview_config(config_dir));
        let web_config = load_web_config(config_dir);
        tools::register_web(&executor.tools, web_config.fetch);
        tools::register_web_search(&executor.tools, &web_config.search);
        let lifecycle = executor.attach_provider_lifecycle(auth_hub.clone())?;
        let provider_catalog_refresh_plan = lifecycle.prepare_auth_runtime().await?;
        if let Some(sandbox) = build_sandbox(project_root, config_dir).context("sandbox init")? {
            executor.tool_ctx = executor.tool_ctx.clone().with_sandbox(sandbox);
        }
        if opts.mock {
            executor.providers.register(Arc::new(
                MockProvider::new("mock").with_fallback(Value::Str("[mock response]".into())),
            ));
            executor.providers.models().register_model_entries(vec![(
                "mock".into(),
                crate::model_registry::ModelEntry {
                    model: "mock".into(),
                    context_budget: Some(200_000),
                    ..Default::default()
                },
            )]);
        }
        executor.tool_ctx.mcp_service = Some(Arc::new(crate::mcp_service::McpService::start(
            auth_hub.clone(),
            project_root.unwrap_or(auth_hub.config_dir()).to_path_buf(),
            executor.tools.clone(),
            executor.providers.clone(),
        )?));
        Ok(Self {
            executor,
            provider_catalog_refresh_plan,
        })
    }
}

pub fn load_redact_config(config_dir: Option<&Path>) -> RedactConfig {
    let Some(dir) = config_dir else {
        return RedactConfig::default();
    };
    crate::config_hub::ConfigHub::from_config_dir(dir)
        .redact_config()
        .unwrap_or_default()
}

pub fn build_redactor(config_dir: Option<&Path>) -> Option<Arc<crate::redact::Redactor>> {
    let cfg = load_redact_config(config_dir);
    if !cfg.enabled {
        return None;
    }
    let mut pairs: Vec<(&str, &str)> = crate::redact::BUILTIN_PATTERNS.to_vec();
    for (k, r) in &cfg.custom_patterns {
        pairs.push((k.as_str(), r.as_str()));
    }
    let mode = if cfg.partial {
        crate::redact::RedactMode::Partial
    } else {
        crate::redact::RedactMode::Full
    };
    let redactor = crate::redact::Redactor::from_pairs(&pairs, mode).with_allowlist(cfg.allowlist);
    Some(Arc::new(redactor))
}

fn build_sandbox(
    project_root: Option<&Path>,
    config_dir: Option<&Path>,
) -> Result<Option<Arc<dyn Sandbox>>> {
    let cfg = load_sandbox_config(config_dir);
    if !cfg.enabled {
        return Ok(None);
    }
    let template = match &cfg.template_path {
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("read sandbox template {}", path.display()))?,
        None => crate::sandbox::DEFAULT_PROFILE.to_string(),
    };
    let sandbox = project_root
        .map_or_else(SandboxExec::without_project, SandboxExec::new)
        .with_extra_read(cfg.extra_read.clone())
        .with_extra_write(cfg.extra_write.clone())
        .with_allow_network(cfg.allow_network)
        .with_template(template);
    if !sandbox.is_available() {
        if cfg.strict {
            anyhow::bail!("sandbox enabled + strict, but sandbox-exec not available on this host");
        }
        crate::notify!(
            warn,
            "sandbox enabled but sandbox-exec not available; falling back to no-sandbox path"
        );
        return Ok(None);
    }
    Ok(Some(Arc::new(sandbox)))
}

async fn build_rule_fetch(
    project_root: Option<&Path>,
    home: Option<&Path>,
) -> tools::memory_stubs::RuleFetch {
    let rule_fetch = tools::memory_stubs::RuleFetch::new();
    if std::env::var("ATMAN_DISABLE_MIGRATION").is_ok() {
        return rule_fetch;
    }
    let rules = crate::migration::scan_rule_sources(project_root, home);
    rule_fetch.set_migrated(rules).await;
    rule_fetch
}

pub fn load_sandbox_config(config_dir: Option<&Path>) -> SandboxConfig {
    let Some(dir) = config_dir else {
        return SandboxConfig::default();
    };
    ConfigHub::from_config_dir(dir)
        .sandbox_config()
        .unwrap_or_default()
}

pub fn load_preview_config(config_dir: Option<&Path>) -> tools::preview::PreviewConfig {
    let Some(dir) = config_dir else {
        return tools::preview::PreviewConfig::default();
    };
    ConfigHub::from_config_dir(dir)
        .preview_config()
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default)]
pub struct AtmanWebConfig {
    pub fetch: tools::web::WebConfig,
    pub search: tools::web::SearchConfig,
}

pub fn load_web_config(config_dir: Option<&Path>) -> AtmanWebConfig {
    let Some(dir) = config_dir else {
        return AtmanWebConfig::default();
    };
    let hub = ConfigHub::from_config_dir(dir);
    AtmanWebConfig {
        fetch: hub.web_fetch_config().unwrap_or_default(),
        search: hub.web_search_config().unwrap_or_default(),
    }
}

pub fn load_trust_config(config_dir: Option<&Path>) -> crate::trust::TrustConfig {
    let Some(dir) = config_dir else {
        return crate::trust::TrustConfig::default();
    };
    ConfigHub::from_config_dir(dir)
        .trust_config()
        .unwrap_or_default()
}
