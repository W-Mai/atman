use alloc::{string::String, sync::Arc, vec::Vec};

use crate::{
    Env,
    ast::{Expr, Ident},
};

/// Describes a host-owned value without requiring the core to know its shape.
pub trait HostPayload {
    fn kind_name(&self) -> &'static str;
}

impl HostPayload for () {
    fn kind_name(&self) -> &'static str {
        "host"
    }
}

/// A flow value whose external payload and error belong to the embedding host.
#[derive(Debug, Clone)]
pub enum Value<P, E> {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Self>),
    Struct(Vec<(String, Self)>),
    Host(P),
    Err(E),
    Lambda {
        params: Vec<Ident>,
        body: Arc<Expr>,
        captured_env: Env<Self>,
    },
}

impl<P: HostPayload, E> Value<P, E> {
    pub fn is_err(&self) -> bool {
        matches!(self, Self::Err(_))
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Unit => "unit",
            Self::Bool(_) => "bool",
            Self::Int(_) => "int",
            Self::Float(_) => "float",
            Self::Str(_) => "string",
            Self::List(_) => "list",
            Self::Struct(_) => "struct",
            Self::Host(payload) => payload.kind_name(),
            Self::Err(_) => "err",
            Self::Lambda { .. } => "lambda",
        }
    }

    pub fn field(&self, name: &str) -> Option<&Self> {
        match self {
            Self::Struct(fields) => fields
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value),
            _ => None,
        }
    }
}

impl<P: HostPayload + Clone, E: Clone> crate::PatternValue for Value<P, E> {
    fn struct_fields(&self) -> Option<&[(String, Self)]> {
        match self {
            Self::Struct(fields) => Some(fields),
            _ => None,
        }
    }

    fn kind_name(&self) -> &str {
        Value::kind_name(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[derive(Debug, Clone)]
    struct ForeignPayload;

    impl HostPayload for ForeignPayload {
        fn kind_name(&self) -> &'static str {
            "foreign"
        }
    }

    #[test]
    fn foreign_host_values_share_the_core_environment_and_struct_shape() {
        let mut env = Env::<Value<ForeignPayload, &'static str>>::new();
        env.bind("external", Value::Host(ForeignPayload));
        let item = env.lookup("external").unwrap().clone();
        let record = Value::Struct(vec![("item".into(), item)]);
        assert_eq!(record.field("item").unwrap().kind_name(), "foreign");
        assert!(!record.is_err());
    }
}
