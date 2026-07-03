use crate::value::Value;

#[derive(Debug, Clone, Default)]
pub struct Env {
    bindings: Vec<(String, Value)>,
}

impl Env {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind(&mut self, name: impl Into<String>, value: Value) {
        self.bindings.push((name.into(), value));
    }

    pub fn lookup(&self, name: &str) -> Option<&Value> {
        self.bindings
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Value)> {
        self.bindings.iter().map(|(k, v)| (k.as_str(), v))
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
}
