use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

use crate::migration::MigratedRule;

#[derive(Default, Clone)]
pub struct RuleFetch {
    entries: Arc<RwLock<HashMap<String, String>>>,
    migrated: Arc<RwLock<Vec<MigratedRule>>>,
}

impl RuleFetch {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn insert(&self, name: impl Into<String>, content: impl Into<String>) {
        self.entries
            .write()
            .await
            .insert(name.into(), content.into());
    }

    pub async fn set_migrated(&self, rules: Vec<MigratedRule>) {
        *self.migrated.write().await = rules;
    }

    pub async fn migrated_count(&self) -> usize {
        self.migrated.read().await.len()
    }
}

impl Tool for RuleFetch {
    fn name(&self) -> &str {
        "rule.fetch"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn description(&self) -> Option<&str> {
        Some(
            "Load a skill/rule's full content by name, or search the rule index by keyword. \
             Sources: CLAUDE.md, AGENTS.md, ~/.claude/skills/*/SKILL.md, .cursorrules, \
             .kiro/steering/*.md, and aider conventions.",
        )
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "name": {
                    "type": "string",
                    "description": "Exact rule name to load full content (e.g. 'skill:code-review::references/rules.md')."
                },
                "query": {
                    "type": "string",
                    "description": "Keyword to search across rule names and descriptions. Returns a list of {name, description, scope, source}. Omit both name and query to list the full index."
                }
            }
        })
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
            // Search mode: query keyword → list of matching rules (index).
            if let Some(Value::Str(query)) = args.named("query") {
                let migrated = self.migrated.read().await;
                let lower = query.to_lowercase();
                let results: Vec<Value> = migrated
                    .iter()
                    .filter(|r| {
                        r.name.to_lowercase().contains(&lower)
                            || r.description
                                .as_deref()
                                .map(|d| d.to_lowercase().contains(&lower))
                                .unwrap_or(false)
                    })
                    .map(rule_to_index_value)
                    .collect();
                return Ok(Value::List(results));
            }

            // No args → return the full compact index (name + description + scope + source).
            if args.named("name").is_none() && args.positional(0).is_err() {
                let migrated = self.migrated.read().await;
                let index: Vec<Value> = migrated.iter().map(rule_to_index_value).collect();
                return Ok(Value::List(index));
            }

            let name = extract_string(&args, "name", 0)?;
            let entries = self.entries.read().await;
            if let Some(content) = entries.get(&name) {
                return Ok(Value::Str(content.clone()));
            }
            drop(entries);
            let migrated = self.migrated.read().await;
            if let Some(rule) = crate::migration::resolve_by_name(&migrated, &name) {
                return Ok(Value::Str(rule.content.clone()));
            }
            Ok(Value::Str(String::new()))
        })
    }
}

/// Serialize a migrated rule into a compact index entry `{name, description, scope, source}`.
fn rule_to_index_value(rule: &MigratedRule) -> Value {
    use crate::value::Value as V;
    V::Struct(vec![
        ("name".into(), V::Str(rule.name.clone())),
        (
            "description".into(),
            V::Str(rule.description.clone().unwrap_or_default()),
        ),
        ("scope".into(), V::Str(rule.scope.as_str().to_string())),
        ("source".into(), V::Str(rule.source_tool.clone())),
    ])
}

fn extract_string(args: &ToolArgs, name: &str, pos: usize) -> Result<String, RuntimeError> {
    let value = match args.named(name) {
        Some(v) => v,
        None => args.positional(pos)?,
    };
    match value {
        Value::Str(s) => Ok(s.clone()),
        other => Err(RuntimeError::TypeMismatch {
            expected: "string".into(),
            actual: other.kind_name().into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rule_fetch_returns_stored_content() {
        let tool = RuleFetch::new();
        tool.insert("code-review", "review carefully").await;
        let out = tool
            .call(
                ToolArgs {
                    positional: vec![Value::Str("code-review".into())],
                    named: vec![],
                },
                &ToolCtx::new(),
            )
            .await
            .unwrap();
        assert!(matches!(out, Value::Str(s) if s == "review carefully"));
    }

    #[tokio::test]
    async fn rule_fetch_missing_returns_empty_string() {
        let tool = RuleFetch::new();
        let out = tool
            .call(
                ToolArgs {
                    positional: vec![Value::Str("missing".into())],
                    named: vec![],
                },
                &ToolCtx::new(),
            )
            .await
            .unwrap();
        assert!(matches!(out, Value::Str(s) if s.is_empty()));
    }

    fn migrated_rule(name: &str, description: &str) -> MigratedRule {
        MigratedRule {
            name: name.to_string(),
            source_tool: "skill".to_string(),
            source_path: "/tmp/x".into(),
            scope: crate::migration::RuleScope::Global,
            content: "content".to_string(),
            description: Some(description.to_string()),
        }
    }

    #[tokio::test]
    async fn rule_fetch_query_searches_name_and_description() {
        let tool = RuleFetch::new();
        tool.set_migrated(vec![
            migrated_rule("skill:code-review::a.md", "structured code review"),
            migrated_rule("skill:deploy::b.md", "deployment checklist"),
        ])
        .await;

        let out = tool
            .call(
                ToolArgs {
                    positional: vec![],
                    named: vec![("query".into(), Value::Str("review".into()))],
                },
                &ToolCtx::new(),
            )
            .await
            .unwrap();
        let Value::List(items) = out else {
            panic!("expected list, got {out:?}")
        };
        assert_eq!(items.len(), 1, "only the review rule should match");
        let Value::Struct(fields) = &items[0] else {
            panic!("expected struct entry")
        };
        assert_eq!(fields[0].0, "name");
        assert!(matches!(&fields[0].1, Value::Str(s) if s == "skill:code-review::a.md"));
    }

    #[tokio::test]
    async fn rule_fetch_no_args_returns_full_index() {
        let tool = RuleFetch::new();
        tool.set_migrated(vec![
            migrated_rule("a", "first"),
            migrated_rule("b", "second"),
        ])
        .await;

        let out = tool
            .call(ToolArgs::default(), &ToolCtx::new())
            .await
            .unwrap();
        let Value::List(items) = out else {
            panic!("expected list, got {out:?}")
        };
        assert_eq!(items.len(), 2);
    }
}
