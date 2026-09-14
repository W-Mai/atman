use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use atman_runtime::templates::{
    AGENT_AT, LOOP_ACTION_MD, LOOP_CONTINUATION_MD, LOOP_DISPOSITION_MD, LOOP_FINAL_ANSWER_MD,
    SPEC_AT, SPEC_DESIGN_MD, SPEC_MD, SPEC_QUESTIONS_MD, SYSTEM_MD, write_managed_template,
};

pub struct InitReport {
    pub config_dir: PathBuf,
    pub written: Vec<PathBuf>,
    pub skipped: Vec<PathBuf>,
    pub managed: Vec<PathBuf>,
}

#[cfg(test)]
pub fn init_config_dir(config_dir: &Path) -> Result<InitReport> {
    init_config_dir_with_mode(config_dir, None)
}

pub fn init_config_dir_with_mode(
    config_dir: &Path,
    fs_access: Option<atman_runtime::fs_access::FsAccessMode>,
) -> Result<InitReport> {
    std::fs::create_dir_all(config_dir)
        .with_context(|| format!("mkdir {}", config_dir.display()))?;
    let commands_dir = config_dir.join("commands");
    std::fs::create_dir_all(&commands_dir)
        .with_context(|| format!("mkdir {}", commands_dir.display()))?;

    let config_toml_body: String = match fs_access {
        Some(mode) => CONFIG_TOML.replace(
            "# [fs_access]\n# mode = \"workspace-write\"",
            &format!("[fs_access]\nmode = \"{}\"", mode.as_str()),
        ),
        None => CONFIG_TOML.to_string(),
    };

    let config_path = config_dir.join("config.toml");
    let optional_templates: [(PathBuf, String); 4] = [
        (config_path.clone(), config_toml_body),
        (config_dir.join("routes.at"), ROUTES_AT.into()),
        (
            config_dir.join("on_session_start.at"),
            ON_SESSION_START_AT.into(),
        ),
        (commands_dir.join("hello.at"), HELLO_AT.into()),
    ];

    let mut written = Vec::new();
    let mut skipped = Vec::new();
    let mut managed = Vec::new();

    let prompts_dir = config_dir.join("prompts");
    std::fs::create_dir_all(&prompts_dir)
        .with_context(|| format!("mkdir {}", prompts_dir.display()))?;
    let managed_templates = [
        (commands_dir.join("agent.at"), AGENT_AT),
        (commands_dir.join("spec.at"), SPEC_AT),
        (prompts_dir.join("system.md"), SYSTEM_MD),
        (prompts_dir.join("spec.md"), SPEC_MD),
        (prompts_dir.join("spec-questions.md"), SPEC_QUESTIONS_MD),
        (prompts_dir.join("spec-design.md"), SPEC_DESIGN_MD),
        (prompts_dir.join("loop-disposition.md"), LOOP_DISPOSITION_MD),
        (
            prompts_dir.join("loop-continuation.md"),
            LOOP_CONTINUATION_MD,
        ),
        (prompts_dir.join("loop-action.md"), LOOP_ACTION_MD),
        (
            prompts_dir.join("loop-final-answer.md"),
            LOOP_FINAL_ANSWER_MD,
        ),
    ];
    for (path, contents) in managed_templates {
        if write_managed_template(&path, contents)? {
            written.push(path.clone());
        }
        managed.push(path);
    }

    for (path, body) in optional_templates {
        if path.exists() {
            skipped.push(path);
            continue;
        }
        std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
        written.push(path);
    }
    Ok(InitReport {
        config_dir: config_dir.to_path_buf(),
        written,
        skipped,
        managed,
    })
}

pub const CONFIG_TOML: &str = r#"# atman configuration
#
# Every section here is optional. atman ships with sensible defaults;
# uncomment a section to override.

# ── Model config + alias ──────────────────────────────────────────
# Flows reference models by alias (e.g. model: "smart") so you can
# switch providers without editing flows.
#
# Each [models.xxx] block can declare:
#   provider   = "anthropic" | "openai"   — which API protocol to use
#   api_key    = "sk-..."                  — key (or use env vars below)
#   base_url   = "https://..."             — override the default endpoint
#   max_tokens = 8192                      — response token cap
#   context_budget = 200000                — context window size
#   thinking   = true                      — enable extended thinking
#
# Provider env vars (alternative to api_key in config):
#   Anthropic / DeepSeek (anthropic-compat):  ANTHROPIC_API_KEY + ANTHROPIC_BASE_URL
#   OpenAI / OpenAI-compat:                   OPENAI_API_KEY + OPENAI_BASE_URL

# Note: model names containing "/" must be quoted: [models."provider/model-id"]

[models.claude-opus-4.7]
context_budget = 200000
thinking = true

[models.gpt-4o-mini]
context_budget = 128000

[alias.smart]
model = "claude-opus-4.7"

[alias.cheap]
model = "smart"

[suggest]
# model = "cheap"

[registry]
# auto_snapshot = true

[interjection]
# classifier = "rule"

[sandbox]
# enabled = true
# strict = false
# allow_network = false

# Theme: auto (detect from terminal), dark, or light.
# [theme]
# mode = "auto"

# [fs_access]
# mode = "workspace-write"

[preview]
# base_url = "http://127.0.0.1:65097"
# timeout_ms = 3000
"#;

pub const ROUTES_AT: &str = r#"route "hi " { flow: hello }

default_route { flow: agent }
"#;

pub const ON_SESSION_START_AT: &str = r#"flow on_session_start() -> string {
    return "atman ready. `/hello` for a smoke test, plain text to chat."
}
"#;

pub const HELLO_AT: &str = r#"flow hello() -> string {
    return "hello from atman"
}
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_init_writes_every_template() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("atman");
        let rep = init_config_dir(&cfg).unwrap();
        assert!(rep.skipped.is_empty());
        assert_eq!(rep.written.len(), 14, "written: {:?}", rep.written);
        assert_eq!(rep.managed.len(), 10);
        assert!(cfg.join("config.toml").exists());
        assert!(cfg.join("routes.at").exists());
        assert!(cfg.join("on_session_start.at").exists());
        assert!(cfg.join("commands/agent.at").exists());
        assert!(cfg.join("commands/spec.at").exists());
        assert!(cfg.join("commands/hello.at").exists());
        assert!(cfg.join("prompts/system.md").exists());
        assert!(cfg.join("prompts/spec.md").exists());
        assert!(cfg.join("prompts/spec-questions.md").exists());
        assert!(cfg.join("prompts/spec-design.md").exists());
        assert!(cfg.join("prompts/loop-disposition.md").exists());
        assert!(cfg.join("prompts/loop-continuation.md").exists());
        assert!(cfg.join("prompts/loop-action.md").exists());
        assert!(cfg.join("prompts/loop-final-answer.md").exists());
    }

    #[test]
    fn init_with_explicit_mode_persists_uncommented_section() {
        use atman_runtime::fs_access::FsAccessMode;
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("atman");
        init_config_dir_with_mode(&cfg, Some(FsAccessMode::ReadOnly)).unwrap();
        let body = std::fs::read_to_string(cfg.join("config.toml")).unwrap();
        assert!(body.contains("[fs_access]"));
        assert!(body.contains("mode = \"read-only\""));
        assert!(
            !body.contains("# [fs_access]"),
            "explicit mode must uncomment the block"
        );
    }

    #[test]
    fn init_without_mode_leaves_section_commented() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("atman");
        init_config_dir(&cfg).unwrap();
        let body = std::fs::read_to_string(cfg.join("config.toml")).unwrap();
        assert!(body.contains("# [fs_access]"));
    }

    #[test]
    fn second_init_leaves_existing_files_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("atman");
        init_config_dir(&cfg).unwrap();
        let touched = cfg.join("commands/hello.at");
        std::fs::write(&touched, "flow hello() { return \"CUSTOM\" }\n").unwrap();
        let agent = cfg.join("commands/agent.at");
        std::fs::write(&agent, "flow agent() { return \"BROKEN\" }\n").unwrap();
        let loop_disposition = cfg.join("prompts/loop-disposition.md");
        std::fs::write(&loop_disposition, "STALE\n").unwrap();
        let loop_continuation = cfg.join("prompts/loop-continuation.md");
        std::fs::write(&loop_continuation, "STALE\n").unwrap();
        let loop_action = cfg.join("prompts/loop-action.md");
        std::fs::write(&loop_action, "STALE\n").unwrap();
        let loop_final_answer = cfg.join("prompts/loop-final-answer.md");
        std::fs::write(&loop_final_answer, "STALE\n").unwrap();

        let rep = init_config_dir(&cfg).unwrap();
        assert!(rep.skipped.iter().any(|p| p.ends_with("hello.at")));
        assert!(rep.managed.iter().any(|p| p.ends_with("agent.at")));
        assert!(
            rep.managed
                .iter()
                .any(|p| p.ends_with("loop-disposition.md"))
        );
        assert!(
            rep.managed
                .iter()
                .any(|p| p.ends_with("loop-continuation.md"))
        );
        assert!(rep.managed.iter().any(|p| p.ends_with("loop-action.md")));
        assert!(
            rep.managed
                .iter()
                .any(|p| p.ends_with("loop-final-answer.md"))
        );
        let hello_body = std::fs::read_to_string(&touched).unwrap();
        assert!(
            hello_body.contains("CUSTOM"),
            "user edit preserved: {hello_body}"
        );
        let agent_body = std::fs::read_to_string(&agent).unwrap();
        assert!(agent_body.contains("flow agent(user_prompt: string) -> string"));
        assert!(!agent_body.contains("BROKEN"));
        assert_eq!(
            std::fs::read_to_string(loop_disposition).unwrap(),
            LOOP_DISPOSITION_MD
        );
        assert_eq!(
            std::fs::read_to_string(loop_continuation).unwrap(),
            LOOP_CONTINUATION_MD
        );
        assert_eq!(
            std::fs::read_to_string(loop_action).unwrap(),
            LOOP_ACTION_MD
        );
        assert_eq!(
            std::fs::read_to_string(loop_final_answer).unwrap(),
            LOOP_FINAL_ANSWER_MD
        );
    }

    #[test]
    fn second_unchanged_init_reports_no_managed_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("atman");
        init_config_dir(&cfg).unwrap();

        let rep = init_config_dir(&cfg).unwrap();

        assert!(rep.written.is_empty(), "written: {:?}", rep.written);
        assert_eq!(rep.managed.len(), 10);
        assert_eq!(rep.skipped.len(), 4);
    }

    #[test]
    fn init_creates_commands_dir_even_if_config_dir_pre_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("atman");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("some-other.toml"), "unrelated").unwrap();
        let rep = init_config_dir(&cfg).unwrap();
        assert!(cfg.join("commands").is_dir());
        assert_eq!(rep.written.len(), 14);
    }

    #[test]
    fn agent_template_parses_as_valid_dsl() {
        let file = atman_dsl::parse::parse_file(AGENT_AT).expect("agent template must parse");
        let names: Vec<&str> = file.flows.iter().map(|f| f.name.name.as_str()).collect();
        assert_eq!(names, vec!["agent", "requirements_gate"]);
    }

    #[test]
    fn agent_template_exposes_flow_named_agent_for_slash_resolver() {
        let file = atman_dsl::parse::parse_file(AGENT_AT).unwrap();
        let entry = file.flows.iter().find(|f| f.name.name == "agent");
        assert!(
            entry.is_some(),
            "commands/agent.at must contain a `flow agent(...)` so slash-command resolver can find it by name (regression: 2-flow file previously errored)"
        );
    }

    #[test]
    fn hello_template_parses_and_returns_hello() {
        let file = atman_dsl::parse::parse_file(HELLO_AT).expect("hello template must parse");
        assert_eq!(file.flows.len(), 1);
        assert_eq!(file.flows[0].name.name, "hello");
    }

    #[test]
    fn routes_template_parses() {
        let file = atman_dsl::parse::parse_file(ROUTES_AT).expect("routes template must parse");
        assert!(
            !file.routes.is_empty() || file.default_route.is_some(),
            "want at least one route or default_route"
        );
    }

    #[test]
    fn on_session_start_template_parses() {
        let file = atman_dsl::parse::parse_file(ON_SESSION_START_AT)
            .expect("on_session_start template must parse");
        assert_eq!(file.flows.len(), 1);
        assert_eq!(file.flows[0].name.name, "on_session_start");
    }
}
