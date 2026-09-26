use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};

use crate::config_hub::{ConfigHub, SandboxConfig};
use crate::event::EventSink;
use crate::executor::Executor;
use crate::providers::mock::MockProvider;
use crate::sandbox::{Sandbox, SandboxExec};
use crate::tools;
use crate::value::Value;

pub struct AtmanRuntimeOptions {
    pub events: EventSink,
    pub mock: bool,
    pub config_dir: Option<PathBuf>,
    pub project_root: PathBuf,
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
        let mut executor = Executor::with_events(opts.events);
        let workspace_service = crate::flow_workspace::FlowWorkspaceService::new(
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
        let task_registry = crate::TaskRegistry::new();
        let bg_registry =
            tools::register_bash_bg_with_task_registry(&executor.tools, task_registry.clone());
        let term_registry =
            tools::register_terminal_with_task_registry(&executor.tools, task_registry.clone());
        executor.tools.register(Arc::new(tools::task_ops::TaskList));
        executor.tools.register(Arc::new(tools::task_ops::TaskKill));
        let trust_config = load_trust_config(opts.config_dir.as_deref());
        let tool_output_budget = opts
            .config_dir
            .as_deref()
            .map(ConfigHub::from_config_dir)
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
        let auth_hub = match opts.config_dir.as_deref() {
            Some(dir) => ConfigHub::from_config_dir(dir),
            None => ConfigHub::global().context("resolve config hub")?,
        };
        let lifecycle = executor.attach_provider_lifecycle(auth_hub)?;
        lifecycle
            .reload_config_providers()
            .context("load model and provider configuration")?;
        let provider_catalog_refresh_plan = lifecycle.prepare_auth_runtime().await?;
        if let Some(sandbox) =
            build_sandbox(&opts.project_root, opts.config_dir.as_deref()).context("sandbox init")?
        {
            executor.tool_ctx = executor.tool_ctx.clone().with_sandbox(sandbox);
        }
        if opts.mock {
            executor.providers.register(Arc::new(
                MockProvider::new("mock").with_fallback(Value::Str("[mock response]".into())),
            ));
            use crate::model_registry::{ModelConfig, ModelEntry};
            let mut models = std::collections::HashMap::new();
            models.insert(
                "mock".into(),
                ModelEntry {
                    model: "mock".into(),
                    context_budget: Some(200_000),
                    ..Default::default()
                },
            );
            crate::model_registry::set_model_config(ModelConfig {
                models,
                providers: std::collections::HashMap::new(),
                aliases: std::collections::HashMap::new(),
            });
        }
        Ok(Self {
            executor,
            provider_catalog_refresh_plan,
        })
    }
}

fn build_sandbox(
    project_root: &Path,
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
    let sandbox = SandboxExec::new(project_root)
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
    project_root: &Path,
    home: Option<&Path>,
) -> tools::memory_stubs::RuleFetch {
    let rule_fetch = tools::memory_stubs::RuleFetch::new();
    if std::env::var("ATMAN_DISABLE_MIGRATION").is_ok() {
        return rule_fetch;
    }
    let Some(home) = home else {
        return rule_fetch;
    };
    let rules = crate::migration::scan_migrated_rules(project_root, home);
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
