use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MigratedRule {
    pub name: String,
    pub source_tool: String,
    pub source_path: PathBuf,
    pub scope: RuleScope,
    pub content: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RuleScope {
    Project,
    Global,
}

const MAX_RULE_BYTES: usize = 100_000;

pub fn scan_migrated_rules(project_root: &Path, home: &Path) -> Vec<MigratedRule> {
    let mut out = Vec::new();

    for (rel, tool) in &[
        ("CLAUDE.md", "claude"),
        ("AGENTS.md", "opencode"),
        (".cursorrules", "cursor"),
        ("CONVENTIONS.md", "aider"),
    ] {
        let path = project_root.join(rel);
        if let Some(rule) = load_file(&path, tool, RuleScope::Project) {
            out.push(rule);
        }
    }

    push_dir(
        &mut out,
        &project_root.join(".cursor/rules"),
        "cursor",
        "md",
    );
    push_dir(&mut out, &project_root.join(".kiro/steering"), "kiro", "md");

    for (rel, tool) in &[
        (".claude/CLAUDE.md", "claude"),
        (".config/opencode/AGENTS.md", "opencode"),
    ] {
        let path = home.join(rel);
        if let Some(rule) = load_file(&path, tool, RuleScope::Global) {
            out.push(rule);
        }
    }

    out
}

fn push_dir(out: &mut Vec<MigratedRule>, dir: &Path, tool: &str, ext: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| s == ext)
            .unwrap_or(false)
            && let Some(rule) = load_file(&path, tool, RuleScope::Project)
        {
            out.push(rule);
        }
    }
}

fn load_file(path: &Path, tool: &str, scope: RuleScope) -> Option<MigratedRule> {
    let raw = std::fs::read_to_string(path).ok()?;
    let content = if raw.len() > MAX_RULE_BYTES {
        let mut truncated = raw[..MAX_RULE_BYTES].to_string();
        truncated.push_str(&format!(
            "\n\n[atman: truncated at {MAX_RULE_BYTES} bytes; full at {}]",
            path.display()
        ));
        truncated
    } else {
        raw
    };
    let name = extract_rule_name(&content).unwrap_or_else(|| basename(path));
    Some(MigratedRule {
        name,
        source_tool: tool.into(),
        source_path: path.to_path_buf(),
        scope,
        content,
    })
}

fn extract_rule_name(content: &str) -> Option<String> {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("# ") {
            return Some(rest.trim().to_string());
        }
        break;
    }
    None
}

fn basename(path: &Path) -> String {
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unnamed")
        .to_string()
}

pub fn resolve_by_name<'a>(rules: &'a [MigratedRule], query: &str) -> Option<&'a MigratedRule> {
    if let Some((name, tool)) = query.split_once('@') {
        return rules
            .iter()
            .find(|r| r.name == name && r.source_tool == tool);
    }
    let matches: Vec<&MigratedRule> = rules.iter().filter(|r| r.name == query).collect();
    if matches.is_empty() {
        return None;
    }
    if let Some(project) = matches
        .iter()
        .find(|r| matches!(r.scope, RuleScope::Project))
    {
        return Some(*project);
    }
    matches.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, rel: &str, content: &str) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn detects_claude_md_in_project_root() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(dir.path(), "CLAUDE.md", "# atman rules\n\nBe terse.\n");
        let rules = scan_migrated_rules(dir.path(), home.path());
        let claude = rules
            .iter()
            .find(|r| r.source_tool == "claude")
            .expect("expected CLAUDE.md rule");
        assert_eq!(claude.name, "atman rules");
        assert!(matches!(claude.scope, RuleScope::Project));
    }

    #[test]
    fn detects_agents_md_and_cursorrules_and_conventions() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(dir.path(), "AGENTS.md", "# opencode-rules\ncontent");
        write(dir.path(), ".cursorrules", "# cursor-flat\nuse rust");
        write(dir.path(), "CONVENTIONS.md", "# aider-conv\nx");
        let rules = scan_migrated_rules(dir.path(), home.path());
        let tools: Vec<&str> = rules.iter().map(|r| r.source_tool.as_str()).collect();
        assert!(tools.contains(&"opencode"));
        assert!(tools.contains(&"cursor"));
        assert!(tools.contains(&"aider"));
    }

    #[test]
    fn detects_cursor_rules_directory() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(dir.path(), ".cursor/rules/rust.md", "# rust\nuse borrow");
        write(dir.path(), ".cursor/rules/style.md", "# style\nno emoji");
        let rules = scan_migrated_rules(dir.path(), home.path());
        let cursor_rules: Vec<&str> = rules
            .iter()
            .filter(|r| r.source_tool == "cursor")
            .map(|r| r.name.as_str())
            .collect();
        assert!(cursor_rules.contains(&"rust"));
        assert!(cursor_rules.contains(&"style"));
    }

    #[test]
    fn detects_kiro_steering_directory() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(dir.path(), ".kiro/steering/api.md", "# api-guide\ncontent");
        let rules = scan_migrated_rules(dir.path(), home.path());
        let found = rules.iter().find(|r| r.source_tool == "kiro").unwrap();
        assert_eq!(found.name, "api-guide");
    }

    #[test]
    fn detects_user_scope_files_from_home() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(home.path(), ".claude/CLAUDE.md", "# global-claude\nx");
        write(
            home.path(),
            ".config/opencode/AGENTS.md",
            "# global-opencode\ny",
        );
        let rules = scan_migrated_rules(dir.path(), home.path());
        let global_names: Vec<&str> = rules
            .iter()
            .filter(|r| matches!(r.scope, RuleScope::Global))
            .map(|r| r.name.as_str())
            .collect();
        assert!(global_names.contains(&"global-claude"));
        assert!(global_names.contains(&"global-opencode"));
    }

    #[test]
    fn extract_name_from_first_heading_or_basename() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(dir.path(), "CLAUDE.md", "no heading here\nblah");
        let rules = scan_migrated_rules(dir.path(), home.path());
        let claude = rules.iter().find(|r| r.source_tool == "claude").unwrap();
        assert_eq!(claude.name, "CLAUDE", "basename fallback");
    }

    #[test]
    fn truncates_oversized_rule() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let big = "# huge\n".to_string() + &"x".repeat(MAX_RULE_BYTES + 1000);
        write(dir.path(), "CLAUDE.md", &big);
        let rules = scan_migrated_rules(dir.path(), home.path());
        let claude = rules.iter().find(|r| r.source_tool == "claude").unwrap();
        assert!(claude.content.contains("[atman: truncated"));
        assert!(claude.content.len() < MAX_RULE_BYTES + 200);
    }

    #[test]
    fn resolve_by_name_prefers_project_over_global() {
        let rules = vec![
            MigratedRule {
                name: "code-review".into(),
                source_tool: "opencode".into(),
                source_path: "/user".into(),
                scope: RuleScope::Global,
                content: "global-version".into(),
            },
            MigratedRule {
                name: "code-review".into(),
                source_tool: "claude".into(),
                source_path: "/proj".into(),
                scope: RuleScope::Project,
                content: "project-version".into(),
            },
        ];
        let r = resolve_by_name(&rules, "code-review").unwrap();
        assert!(matches!(r.scope, RuleScope::Project));
        assert_eq!(r.content, "project-version");
    }

    #[test]
    fn resolve_by_name_with_at_tool_disambiguation() {
        let rules = vec![
            MigratedRule {
                name: "code-review".into(),
                source_tool: "opencode".into(),
                source_path: "/x".into(),
                scope: RuleScope::Global,
                content: "opencode-version".into(),
            },
            MigratedRule {
                name: "code-review".into(),
                source_tool: "claude".into(),
                source_path: "/y".into(),
                scope: RuleScope::Project,
                content: "claude-version".into(),
            },
        ];
        let r = resolve_by_name(&rules, "code-review@opencode").unwrap();
        assert_eq!(r.content, "opencode-version");
    }
}
