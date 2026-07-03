use std::collections::HashMap;
use std::sync::Arc;

use crate::error::RuntimeError;
use crate::tool::BoxFut;
use crate::value::Value;

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub prompt: String,
    pub input: Value,
    pub schema: Option<String>,
}

pub trait Provider {
    fn name(&self) -> &str;
    fn call<'a>(&'a self, req: LlmRequest) -> BoxFut<'a, Result<Value, RuntimeError>>;
}

#[derive(Default, Clone)]
pub struct ProviderRegistry {
    providers: HashMap<String, Arc<dyn Provider>>,
    default: Option<String>,
}

impl ProviderRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Arc<dyn Provider>) {
        let name = provider.name().to_string();
        if self.default.is_none() {
            self.default = Some(name.clone());
        }
        self.providers.insert(name, provider);
    }

    pub fn set_default(&mut self, name: &str) {
        if self.providers.contains_key(name) {
            self.default = Some(name.to_string());
        }
    }

    pub fn resolve(&self, _model: &str) -> Option<Arc<dyn Provider>> {
        self.default
            .as_ref()
            .and_then(|n| self.providers.get(n).cloned())
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Provider>> {
        self.providers.get(name).cloned()
    }
}
