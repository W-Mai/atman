use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

pub struct InitReport {
    pub config_dir: PathBuf,
    pub written: Vec<PathBuf>,
    pub skipped: Vec<PathBuf>,
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
    let templates: [(PathBuf, String); 5] = [
        (config_path.clone(), config_toml_body),
        (config_dir.join("routes.at"), ROUTES_AT.into()),
        (
            config_dir.join("on_session_start.at"),
            ON_SESSION_START_AT.into(),
        ),
        (commands_dir.join("agent.at"), AGENT_AT.into()),
        (commands_dir.join("hello.at"), HELLO_AT.into()),
    ];

    let mut written = Vec::new();
    let mut skipped = Vec::new();
    for (path, body) in templates {
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
    })
}

pub const CONFIG_TOML: &str = r#"# atman configuration
#
# Every section here is optional. atman ships with sensible defaults;
# uncomment a section to override.

# Which model to hand to REPL bare text (see routes.at → default_route).
# Set an ANTHROPIC_API_KEY / OPENAI_API_KEY / ATMAN_TEST_GLM_KEY in your shell.
[suggest]
# model = "gpt-4o-mini"

# Snapshot every flow on `atman run` into .atman/flow-registry.db so
# `atman flow rollback` has something to rewind to.
[registry]
# auto_snapshot = true

# Injection classifier for L2/L3 course-correction on `!nudge` / `!redirect`.
# off | rule | llm
[interjection]
# classifier = "rule"

# Sandbox for Tier 4 (shell.exec) on macOS. Enabled by default when
# sandbox-exec is available; set enabled = false to opt out.
[sandbox]
# enabled = true
# strict = false

# Filesystem access policy for fs.write / fs.edit. Defaults to
# workspace-write: writes are allowed inside the current project + the
# system tempdir, everything else is refused. Set to "read-only" to
# block every write, "danger-full-access" to skip the check entirely.
# [fs_access]
# mode = "workspace-write"

# preview daemon (agent audit UI at http://localhost:65097/). Optional.
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

pub const AGENT_AT: &str = r#"flow agent(user_prompt: string) -> string {
    contract {
        capabilities { shell: true }
    }
    _prompt_lands_via_begin_turn = user_prompt
    messages = memory.recent_turns(n: 10)
    return subflow(agent_loop, messages, 0)
}

flow agent_loop(messages: list, iteration: int) -> string {
    when iteration >= 200 {
        return "[agent: 200-iteration ceiling — task likely stuck, ask the user before continuing]"
    }
    reply = llm {
        model: "claude-opus-4.7"
        messages: messages
        tools: [
            fs.read, fs.write, fs.edit, fs.list, fs.grep,
            bash.exec,
            hunk.review, hunk.apply,
            memory.confess,
            memory.todo.set, memory.todo.done,
            memory.goal.get, memory.goal.set, memory.goal.clear,
            memory.recent_turns, memory.history.search, memory.history.read,
            plan.write, plan.read, plan.tick,
            agent.spawn
        ]
    }
    tool_uses = extract_tool_uses(reply)
    when is_empty(tool_uses) {
        return text_concat(reply)
    }
    tool_results = dispatch_all(tool_uses)
    new_history = concat(messages, concat([reply], tool_results))
    j = iteration + 1
    return subflow(agent_loop, new_history, j)
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
        assert_eq!(rep.written.len(), 5, "written: {:?}", rep.written);
        assert!(cfg.join("config.toml").exists());
        assert!(cfg.join("routes.at").exists());
        assert!(cfg.join("on_session_start.at").exists());
        assert!(cfg.join("commands/agent.at").exists());
        assert!(cfg.join("commands/hello.at").exists());
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

        let rep = init_config_dir(&cfg).unwrap();
        assert!(rep.written.is_empty(), "second run must not overwrite");
        assert_eq!(rep.skipped.len(), 5, "skipped: {:?}", rep.skipped);
        let body = std::fs::read_to_string(&touched).unwrap();
        assert!(body.contains("CUSTOM"), "user edit preserved: {body}");
    }

    #[test]
    fn init_creates_commands_dir_even_if_config_dir_pre_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = tmp.path().join("atman");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(cfg.join("some-other.toml"), "unrelated").unwrap();
        let rep = init_config_dir(&cfg).unwrap();
        assert!(cfg.join("commands").is_dir());
        assert_eq!(rep.written.len(), 5);
    }

    #[test]
    fn agent_template_parses_as_valid_dsl() {
        let file = atman_dsl::parse::parse_file(AGENT_AT).expect("agent template must parse");
        let names: Vec<&str> = file.flows.iter().map(|f| f.name.name.as_str()).collect();
        assert_eq!(names, vec!["agent", "agent_loop"]);
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
