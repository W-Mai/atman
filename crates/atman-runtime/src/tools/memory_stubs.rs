use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;

use crate::error::RuntimeError;
use crate::tool::{BoxFut, Tier, Tool, ToolArgs, ToolCtx, ToolResult};
use crate::value::Value;

use crate::migration::MigratedRule;

#[derive(Default, Clone)]
pub struct FetchRule {
    entries: Arc<RwLock<HashMap<String, String>>>,
    migrated: Arc<RwLock<Vec<MigratedRule>>>,
}

impl FetchRule {
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

impl Tool for FetchRule {
    fn name(&self) -> &str {
        "fetch_rule"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move {
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

pub struct FetchConfessions;

impl Tool for FetchConfessions {
    fn name(&self) -> &str {
        "fetch_confessions"
    }

    fn tier(&self) -> Tier {
        Tier::Zero
    }

    fn call<'a>(&'a self, _args: ToolArgs, _ctx: &'a ToolCtx) -> BoxFut<'a, ToolResult> {
        Box::pin(async move { Ok(Value::List(Vec::new())) })
    }
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
    async fn fetch_rule_returns_stored_content() {
        let tool = FetchRule::new();
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
    async fn fetch_rule_missing_returns_empty_string() {
        let tool = FetchRule::new();
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

    #[tokio::test]
    async fn fetch_confessions_returns_empty_list() {
        let out = FetchConfessions
            .call(ToolArgs::default(), &ToolCtx::new())
            .await
            .unwrap();
        assert!(matches!(out, Value::List(items) if items.is_empty()));
    }
}
