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

    scan_aider_yaml(project_root, &mut out);
    scan_skill_references(home, &mut out);

    out
}

fn scan_aider_yaml(project_root: &Path, out: &mut Vec<MigratedRule>) {
    let yml = project_root.join(".aider.conf.yml");
    let Ok(raw) = std::fs::read_to_string(&yml) else {
        return;
    };
    for path in parse_aider_conventions(&raw) {
        let full = if path.is_absolute() {
            path
        } else {
            project_root.join(path)
        };
        if let Some(rule) = load_file(&full, "aider", RuleScope::Project) {
            out.push(rule);
        }
    }
}

fn parse_aider_conventions(yaml: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut inside = false;
    for line in yaml.lines() {
        let stripped = strip_yaml_comment(line);
        if let Some(rest) = stripped.strip_prefix("conventions:") {
            let rest = rest.trim();
            inside = true;
            if let Some(list) = rest.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
                for item in list.split(',') {
                    let value = item.trim().trim_matches(|c| c == '"' || c == '\'');
                    if !value.is_empty() {
                        out.push(PathBuf::from(value));
                    }
                }
                inside = false;
            }
            continue;
        }
        if !inside {
            continue;
        }
        let indent = stripped.len() - stripped.trim_start().len();
        if indent == 0 && !stripped.trim().is_empty() {
            inside = false;
            continue;
        }
        let trimmed = stripped.trim();
        if let Some(item) = trimmed.strip_prefix('-') {
            let value = item.trim().trim_matches(|c| c == '"' || c == '\'');
            if !value.is_empty() {
                out.push(PathBuf::from(value));
            }
        }
    }
    out
}

fn strip_yaml_comment(line: &str) -> &str {
    if let Some((code, _)) = line.split_once(" #") {
        code
    } else if let Some(rest) = line.strip_prefix('#') {
        &rest[..0]
    } else {
        line
    }
}

fn scan_skill_references(home: &Path, out: &mut Vec<MigratedRule>) {
    let skills_root = home.join(".claude").join("skills");
    let Ok(entries) = std::fs::read_dir(&skills_root) else {
        return;
    };
    for entry in entries.flatten() {
        let skill_dir = entry.path();
        if !skill_dir.is_dir() {
            continue;
        }
        let skill_md = skill_dir.join("SKILL.md");
        let Ok(body) = std::fs::read_to_string(&skill_md) else {
            continue;
        };
        let skill_name = skill_dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("unnamed");
        for rel in parse_markdown_local_links(&body) {
            let full = skill_dir.join(&rel);
            if let Some(mut rule) = load_file(&full, "skill", RuleScope::Global) {
                rule.name = format!("skill:{skill_name}::{}", rel.display());
                out.push(rule);
            }
        }
    }
}

fn parse_markdown_local_links(body: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut cursor = body;
    while let Some(open) = cursor.find("](") {
        let after = &cursor[open + 2..];
        let Some(close) = after.find(')') else {
            break;
        };
        let target = &after[..close];
        cursor = &after[close + 1..];
        if target.starts_with("http")
            || target.starts_with('/')
            || target.starts_with('#')
            || target.contains("://")
        {
            continue;
        }
        if !target.ends_with(".md") {
            continue;
        }
        if target.starts_with("references/") || target.starts_with("templates/") {
            out.push(PathBuf::from(target));
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
    fn aider_conf_yml_block_list_loads_convention_markdown() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".aider.conf.yml",
            "model: claude-sonnet-4\nconventions:\n  - docs/style.md\n  - \"docs/security.md\"\nedit-format: diff\n",
        );
        write(dir.path(), "docs/style.md", "# aider-style\nuse rustfmt\n");
        write(dir.path(), "docs/security.md", "# aider-sec\nno unsafe\n");
        let rules = scan_migrated_rules(dir.path(), home.path());
        let aider_names: Vec<&str> = rules
            .iter()
            .filter(|r| r.source_tool == "aider")
            .map(|r| r.name.as_str())
            .collect();
        assert!(aider_names.contains(&"aider-style"), "{aider_names:?}");
        assert!(aider_names.contains(&"aider-sec"), "{aider_names:?}");
    }

    #[test]
    fn aider_conf_yml_flow_style_list_also_loads() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            ".aider.conf.yml",
            "conventions: [docs/inline.md]\n",
        );
        write(dir.path(), "docs/inline.md", "# aider-inline\n");
        let rules = scan_migrated_rules(dir.path(), home.path());
        assert!(
            rules
                .iter()
                .any(|r| r.source_tool == "aider" && r.name == "aider-inline")
        );
    }

    #[test]
    fn skill_references_are_scanned_from_home_claude_skills() {
        let dir = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        write(
            home.path(),
            ".claude/skills/demo/SKILL.md",
            "# demo skill\n\nRead [rule A](references/a.md) and [rule B](templates/b.md).\n\
             External link https://example.com should be ignored.\n\
             Local absolute /nope/x.md too.\n",
        );
        write(home.path(), ".claude/skills/demo/references/a.md", "# aa\n");
        write(home.path(), ".claude/skills/demo/templates/b.md", "# bb\n");

        let rules = scan_migrated_rules(dir.path(), home.path());
        let skill_names: Vec<&str> = rules
            .iter()
            .filter(|r| r.source_tool == "skill")
            .map(|r| r.name.as_str())
            .collect();
        assert!(
            skill_names.contains(&"skill:demo::references/a.md"),
            "{skill_names:?}"
        );
        assert!(
            skill_names.contains(&"skill:demo::templates/b.md"),
            "{skill_names:?}"
        );
        assert_eq!(skill_names.len(), 2, "external / absolute filtered out");
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
