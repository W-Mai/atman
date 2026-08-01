use atman_runtime::help;
use atman_runtime::meta_commands::META_COMMANDS;
use atman_runtime::tool::{ToolCtx, ToolRegistry};
use atman_runtime::tools;

fn ctx_with_tools() -> ToolCtx {
    let mut reg = ToolRegistry::new();
    tools::register_tier_zero(&mut reg);
    ToolCtx {
        registry: Some(std::sync::Arc::new(reg)),
        ..Default::default()
    }
}

fn ctx_empty() -> ToolCtx {
    ToolCtx::default()
}

#[test]
fn all_topics_non_empty() {
    let ctx = ctx_with_tools();
    for topic in help::TOPICS {
        let content = help::topic_content(topic.id, &ctx)
            .unwrap_or_else(|| panic!("topic '{}' returned None", topic.id));
        assert!(!content.is_empty(), "topic '{}' is empty", topic.id);
    }
}

#[test]
fn unknown_topic_returns_none() {
    let ctx = ctx_empty();
    assert!(help::topic_content("nonexistent", &ctx).is_none());
}

#[test]
fn index_contains_version() {
    let content = help::topic_content("index", &ctx_empty()).unwrap();
    assert!(content.contains(help::version()));
}

#[test]
fn cli_topic_contains_every_meta_command() {
    let content = help::topic_content("cli", &ctx_empty()).unwrap();
    for cmd in META_COMMANDS {
        assert!(
            content.contains(&format!(":{}`", cmd.name)),
            "cli topic missing ':{}'",
            cmd.name
        );
    }
}

#[test]
fn tools_topic_contains_every_registered_tool() {
    let ctx = ctx_with_tools();
    let content = help::topic_content("tools", &ctx).unwrap();
    let registry = ctx.registry.as_ref().unwrap();
    for name in registry.names() {
        assert!(
            content.contains(&format!("`{}`", name)),
            "tools topic missing '{}'",
            name
        );
    }
}

#[test]
fn tools_topic_offline_when_no_registry() {
    let content = help::topic_content("tools", &ctx_empty()).unwrap();
    assert!(content.contains("not available"));
}

#[test]
fn config_topic_contains_key_fields() {
    let content = help::topic_content("config", &ctx_empty()).unwrap();
    for field in ["enabled", "mode", "auto_rewrite", "scope", "outside"] {
        assert!(
            content.contains(&format!("`{field}`")),
            "config topic missing field '{field}'"
        );
    }
}

#[test]
fn mcp_topic_contains_key_fields() {
    let content = help::topic_content("mcp", &ctx_empty()).unwrap();
    for field in ["transport", "command", "tier", "timeout_ms", "disabled"] {
        assert!(
            content.contains(&format!("`{field}`")),
            "mcp topic missing field '{field}'"
        );
    }
}

#[test]
fn dsl_topic_contains_key_syntax() {
    let content = help::topic_content("dsl", &ctx_empty()).unwrap();
    assert!(content.contains("flow "));
    assert!(content.contains("llm {"));
    assert!(content.contains("fanout"));
    assert!(content.contains("contract"));
}

#[test]
fn features_topic_contains_key_features() {
    let content = help::topic_content("features", &ctx_empty()).unwrap();
    assert!(content.contains("Compaction"));
    assert!(content.contains("MCP"));
    assert!(content.contains("Hunk Review"));
    assert!(content.contains("Injection"));
}
