use alloc::{string::String, sync::Arc, vec::Vec};
use core::fmt;

use crate::{
    Env,
    ast::{Expr, Ident},
    engine::FlowArgs,
    program::FlowId,
    watch::WatchRules,
};

/// A resolved flow call that has not executed its body yet.
#[derive(Debug)]
pub struct FlowFuture<P, E> {
    pub(crate) target: FlowId,
    pub(crate) args: FlowArgs<P, E>,
    pub(crate) owner: Arc<()>,
    pub(crate) result: async_lock::Mutex<Option<Value<P, E>>>,
}

impl<P, E> FlowFuture<P, E> {
    pub(crate) fn new(target: FlowId, args: FlowArgs<P, E>, owner: Arc<()>) -> Self {
        Self {
            target,
            args,
            owner,
            result: async_lock::Mutex::new(None),
        }
    }

    pub(crate) fn belongs_to(&self, owner: &Arc<()>) -> bool {
        Arc::ptr_eq(&self.owner, owner)
    }
}

/// An evaluated host tool call that starts only when awaited or fanned out.
pub struct ToolFuture<P, E> {
    pub(crate) name: String,
    pub(crate) positional: Vec<Value<P, E>>,
    pub(crate) named: Vec<(String, Value<P, E>)>,
    pub(crate) watch_rules: Option<WatchRules>,
    pub(crate) owner: Option<Arc<()>>,
    pub(crate) result: async_lock::Mutex<Option<Value<P, E>>>,
}

impl<P, E> fmt::Debug for ToolFuture<P, E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ToolFuture")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl<P, E> ToolFuture<P, E> {
    pub(crate) fn new(
        name: String,
        positional: Vec<Value<P, E>>,
        named: Vec<(String, Value<P, E>)>,
        watch_rules: Option<WatchRules>,
        owner: Option<Arc<()>>,
    ) -> Self {
        Self {
            name,
            positional,
            named,
            watch_rules,
            owner,
            result: async_lock::Mutex::new(None),
        }
    }

    pub(crate) fn belongs_to(&self, owner: &Arc<()>) -> bool {
        self.owner
            .as_ref()
            .is_none_or(|future_owner| Arc::ptr_eq(future_owner, owner))
    }
}

/// Describes a host-owned value without requiring the core to know its shape.
pub trait HostPayload {
    fn kind_name(&self) -> &'static str;
}

/// Named values passed to a tool after expression evaluation.
pub type NamedValues<P, E> = Vec<(String, Value<P, E>)>;

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
    FlowFuture(Arc<FlowFuture<P, E>>),
    ToolFuture(Arc<ToolFuture<P, E>>),
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
            Self::FlowFuture(_) => "flow future",
            Self::ToolFuture(_) => "tool future",
            Self::Lambda { .. } => "lambda",
        }
    }

    /// Detects a pending call at any depth before crossing a host boundary.
    pub fn contains_pending_call(&self) -> bool {
        match self {
            Self::FlowFuture(_) | Self::ToolFuture(_) => true,
            Self::List(items) => items.iter().any(Self::contains_pending_call),
            Self::Struct(fields) => fields
                .iter()
                .any(|(_, value)| value.contains_pending_call()),
            Self::Lambda { captured_env, .. } => captured_env
                .iter()
                .any(|(_, value)| value.contains_pending_call()),
            _ => false,
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
