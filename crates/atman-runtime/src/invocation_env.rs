use std::collections::BTreeMap;
use std::sync::Arc;

use crate::value::Value;

/// Immutable caller-supplied values scoped to one root flow invocation,
/// independent of the process environment.
#[derive(Clone, Debug, Default)]
pub struct InvocationEnv(Arc<BTreeMap<String, Value>>);

impl InvocationEnv {
    pub fn from_values(values: impl IntoIterator<Item = (String, Value)>) -> Self {
        Self(Arc::new(values.into_iter().collect()))
    }

    pub fn single(key: impl Into<String>, value: Value) -> Self {
        Self::from_values([(key.into(), value)])
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.get(key)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_an_immutable_snapshot() {
        let env = InvocationEnv::single("effort", Value::Str("high".into()));
        let cloned = env.clone();

        assert!(matches!(
            cloned.get("effort"),
            Some(Value::Str(value)) if value == "high"
        ));
        assert!(cloned.get("missing").is_none());
    }
}
