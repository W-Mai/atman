use std::sync::Arc;

use crate::value::Value;

#[derive(Debug, Clone, Default)]
pub struct Env {
    inner: Arc<EnvInner>,
}

#[derive(Debug, Clone, Default)]
struct EnvInner {
    bindings: Vec<(String, Value)>,
    parent: Option<Arc<EnvInner>>,
}

impl Env {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(&mut self, name: impl Into<String>, value: Value) {
        let inner = Arc::make_mut(&mut self.inner);
        inner.bindings.push((name.into(), value));
    }

    /// Create a child environment that inherits from this one.
    /// The child starts with no own bindings; lookups fall through to parent.
    pub fn child(&self) -> Self {
        Env {
            inner: Arc::new(EnvInner {
                bindings: Vec::new(),
                parent: Some(Arc::clone(&self.inner)),
            }),
        }
    }

    pub fn lookup(&self, name: &str) -> Option<&Value> {
        let mut inner = &self.inner;
        loop {
            if let Some(v) = inner
                .bindings
                .iter()
                .rev()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v)
            {
                return Some(v);
            }
            match &inner.parent {
                Some(p) => inner = p,
                None => return None,
            }
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        let mut chain: Vec<&EnvInner> = Vec::new();
        let mut inner = &*self.inner;
        loop {
            chain.push(inner);
            match &inner.parent {
                Some(p) => inner = p,
                None => break,
            }
        }
        chain
            .into_iter()
            .flat_map(|inner| inner.bindings.iter().map(|(k, v)| (k.as_str(), v)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_env_lookup_returns_none() {
        assert!(Env::new().lookup("x").is_none());
    }

    #[test]
    fn bind_then_lookup_returns_value() {
        let mut env = Env::new();
        env.bind("x", Value::Int(1));
        assert!(matches!(env.lookup("x"), Some(Value::Int(1))));
    }

    #[test]
    fn later_binding_shadows_earlier() {
        let mut env = Env::new();
        env.bind("x", Value::Int(1));
        env.bind("x", Value::Int(2));
        assert!(matches!(env.lookup("x"), Some(Value::Int(2))));
    }

    #[test]
    fn unrelated_lookup_after_shadow_still_works() {
        let mut env = Env::new();
        env.bind("x", Value::Int(1));
        env.bind("y", Value::Str("hi".into()));
        env.bind("x", Value::Int(2));
        assert!(matches!(env.lookup("y"), Some(Value::Str(s)) if s == "hi"));
    }

    #[test]
    fn iter_yields_declaration_order() {
        let mut env = Env::new();
        env.bind("a", Value::Int(1));
        env.bind("b", Value::Int(2));
        let names: Vec<_> = env.iter().map(|(k, _)| k).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn child_env_inherits_parent() {
        let mut parent = Env::new();
        parent.bind("x", Value::Int(42));
        let child = parent.child();
        assert!(matches!(child.lookup("x"), Some(Value::Int(42))));
    }

    #[test]
    fn child_env_bind_does_not_affect_parent() {
        let mut parent = Env::new();
        parent.bind("x", Value::Int(1));
        let mut child = parent.child();
        child.bind("x", Value::Int(99));
        assert!(matches!(parent.lookup("x"), Some(Value::Int(1))));
        assert!(matches!(child.lookup("x"), Some(Value::Int(99))));
    }

    #[test]
    fn clone_is_cheap() {
        let mut env = Env::new();
        env.bind("x", Value::Int(1));
        let cloned = env.clone();
        // Both should see the same binding
        assert!(matches!(cloned.lookup("x"), Some(Value::Int(1))));
        // Mutating original after clone should not affect clone (COW)
        env.bind("y", Value::Int(2));
        assert!(cloned.lookup("y").is_none());
    }
}
