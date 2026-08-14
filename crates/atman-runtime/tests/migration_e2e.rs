use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::migration::{RuleScope, scan_migrated_rules};
use atman_runtime::tools::memory_stubs::RuleFetch;
use atman_runtime::{Executor, Value, tools};

fn write(dir: &std::path::Path, rel: &str, content: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, content).unwrap();
}

#[tokio::test]
async fn rule_fetch_returns_migrated_project_agents_md_content() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write(
        project.path(),
        "AGENTS.md",
        "# project-rules\nBe terse. No emojis.\n",
    );

    let scanned = scan_migrated_rules(project.path(), home.path());
    let rule_fetch = RuleFetch::new();
    rule_fetch.set_migrated(scanned).await;
    assert_eq!(rule_fetch.migrated_count().await, 1);

    let ex = Executor::new();
    tools::register_tier_zero_with_rules(&ex.tools, rule_fetch);

    let file =
        parse_file(r#"flow ask() -> string { return rule.fetch("project-rules") }"#).unwrap();
    let out = ex.run(&file, "ask", vec![]).await.unwrap();
    assert!(matches!(&out, Value::Str(s) if s.contains("Be terse")));
}

#[tokio::test]
async fn rule_fetch_prefers_project_rule_over_global_when_names_collide() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write(
        project.path(),
        "AGENTS.md",
        "# code-review\nproject-version\n",
    );
    write(
        home.path(),
        ".config/opencode/AGENTS.md",
        "# code-review\nglobal-version\n",
    );

    let rules = scan_migrated_rules(project.path(), home.path());
    let project_hit = rules
        .iter()
        .find(|r| matches!(r.scope, RuleScope::Project) && r.name == "code-review")
        .unwrap();
    assert!(project_hit.content.contains("project-version"));

    let rule_fetch = RuleFetch::new();
    rule_fetch.set_migrated(rules).await;
    let ex = Executor::new();
    tools::register_tier_zero_with_rules(&ex.tools, rule_fetch);

    let file = parse_file(r#"flow ask() -> string { return rule.fetch("code-review") }"#).unwrap();
    let out = ex.run(&file, "ask", vec![]).await.unwrap();
    assert!(matches!(&out, Value::Str(s) if s.contains("project-version")));
    let Value::Str(s) = out else { unreachable!() };
    assert!(
        !s.contains("global-version"),
        "project must win over global"
    );
}

#[tokio::test]
async fn rule_fetch_at_tool_syntax_selects_specific_source() {
    let project = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    write(
        project.path(),
        "AGENTS.md",
        "# code-review\nopencode-project\n",
    );
    write(
        project.path(),
        "CLAUDE.md",
        "# code-review\nclaude-project\n",
    );

    let rules = scan_migrated_rules(project.path(), home.path());
    let rule_fetch = RuleFetch::new();
    rule_fetch.set_migrated(rules).await;

    let ex = Executor::new();
    tools::register_tier_zero_with_rules(
        &ex.tools,
        Arc::try_unwrap(Arc::new(rule_fetch)).ok().unwrap(),
    );

    let file =
        parse_file(r#"flow ask() -> string { return rule.fetch("code-review@claude") }"#).unwrap();
    let out = ex.run(&file, "ask", vec![]).await.unwrap();
    assert!(matches!(&out, Value::Str(s) if s.contains("claude-project")));
}

#[tokio::test]
async fn rule_fetch_returns_empty_string_when_migrated_rule_missing() {
    let rule_fetch = RuleFetch::new();
    let ex = Executor::new();
    tools::register_tier_zero_with_rules(&ex.tools, rule_fetch);
    let file =
        parse_file(r#"flow ask() -> string { return rule.fetch("does-not-exist") }"#).unwrap();
    let out = ex.run(&file, "ask", vec![]).await.unwrap();
    assert!(matches!(&out, Value::Str(s) if s.is_empty()));
}
