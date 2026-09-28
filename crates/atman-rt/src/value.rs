use alloc::{format, string::String, sync::Arc, vec::Vec};
use core::fmt;

use crate::{
    Env, VmContext,
    ast::{Expr, Ident, TypeExpr},
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
    pub(crate) audit_origin: Option<VmContext>,
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
        audit_origin: Option<VmContext>,
    ) -> Self {
        Self {
            name,
            positional,
            named,
            watch_rules,
            owner,
            audit_origin,
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

pub(crate) struct ValueTypeMismatch {
    pub expected: String,
    pub actual: String,
}

pub(crate) fn validate_value_type<P: HostPayload, E>(
    value: &Value<P, E>,
    ty: &TypeExpr,
    location: &str,
) -> Result<(), ValueTypeMismatch> {
    let mut path = Vec::new();
    match match_type(value, ty, &mut path) {
        Ok(()) => Ok(()),
        Err(actual) => Err(ValueTypeMismatch {
            expected: format!("{location}: {}", format_type(ty)),
            actual: format_actual(location, &path, actual),
        }),
    }
}

#[derive(Clone, Copy)]
enum TypePathSegment<'a> {
    Index(usize),
    Field(&'a str),
}

#[derive(Clone, Copy)]
enum ActualType {
    Kind(&'static str),
    Missing,
}

fn match_type<'a, P: HostPayload, E>(
    value: &Value<P, E>,
    ty: &'a TypeExpr,
    path: &mut Vec<TypePathSegment<'a>>,
) -> Result<(), ActualType> {
    if matches!(
        value,
        Value::Err(_) | Value::FlowFuture(_) | Value::ToolFuture(_)
    ) {
        return Err(ActualType::Kind(value.kind_name()));
    }
    match ty {
        TypeExpr::Named(name) => {
            let expected_kind = name.name.as_str();
            if expected_kind.eq_ignore_ascii_case("any")
                || expected_kind.eq_ignore_ascii_case("value")
            {
                return Ok(());
            }
            let core_kind = core_kind(expected_kind);
            if let Some(core_kind) = core_kind {
                return if value.kind_name() == core_kind {
                    Ok(())
                } else {
                    Err(ActualType::Kind(value.kind_name()))
                };
            }
            match value {
                Value::Host(payload) if payload.kind_name().eq_ignore_ascii_case(expected_kind) => {
                    Ok(())
                }
                _ if expected_kind
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_uppercase) =>
                {
                    Ok(())
                }
                _ => Err(ActualType::Kind(value.kind_name())),
            }
        }
        TypeExpr::List(item_type) => {
            let Value::List(items) = value else {
                return Err(ActualType::Kind(value.kind_name()));
            };
            for (index, item) in items.iter().enumerate() {
                path.push(TypePathSegment::Index(index));
                match match_type(item, item_type, path) {
                    Ok(()) => {
                        path.pop();
                    }
                    Err(actual) => return Err(actual),
                }
            }
            Ok(())
        }
        TypeExpr::Struct(expected_fields) => {
            let Value::Struct(actual_fields) = value else {
                return Err(ActualType::Kind(value.kind_name()));
            };
            for (field, field_type) in expected_fields {
                path.push(TypePathSegment::Field(&field.name));
                let Some((_, field_value)) = actual_fields
                    .iter()
                    .find(|(actual_name, _)| actual_name == &field.name)
                else {
                    return Err(ActualType::Missing);
                };
                match match_type(field_value, field_type, path) {
                    Ok(()) => {
                        path.pop();
                    }
                    Err(actual) => return Err(actual),
                }
            }
            Ok(())
        }
    }
}

fn format_actual(location: &str, path: &[TypePathSegment<'_>], actual: ActualType) -> String {
    use core::fmt::Write;

    let mut rendered = String::from(location);
    for segment in path {
        match segment {
            TypePathSegment::Index(index) => write!(rendered, "[{index}]").unwrap(),
            TypePathSegment::Field(field) => write!(rendered, ".{field}").unwrap(),
        }
    }
    match actual {
        ActualType::Kind(kind) => write!(rendered, ": {kind}").unwrap(),
        ActualType::Missing => rendered.push_str(": missing"),
    }
    rendered
}

fn core_kind(name: &str) -> Option<&'static str> {
    if name.eq_ignore_ascii_case("unit") {
        Some("unit")
    } else if name.eq_ignore_ascii_case("bool") {
        Some("bool")
    } else if name.eq_ignore_ascii_case("int") {
        Some("int")
    } else if name.eq_ignore_ascii_case("float") {
        Some("float")
    } else if name.eq_ignore_ascii_case("string") {
        Some("string")
    } else if name.eq_ignore_ascii_case("list") {
        Some("list")
    } else if name.eq_ignore_ascii_case("struct") {
        Some("struct")
    } else {
        None
    }
}

fn format_type(ty: &TypeExpr) -> String {
    match ty {
        TypeExpr::Named(name) => name.name.clone(),
        TypeExpr::List(item) => format!("[{}]", format_type(item)),
        TypeExpr::Struct(fields) => {
            let fields = fields
                .iter()
                .map(|(name, ty)| format!("{}: {}", name.name, format_type(ty)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("{{ {fields} }}")
        }
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
    enum ForeignPayload {
        Foreign,
        Resource,
    }

    impl HostPayload for ForeignPayload {
        fn kind_name(&self) -> &'static str {
            match self {
                Self::Foreign => "foreign",
                Self::Resource => "resource",
            }
        }
    }

    #[test]
    fn foreign_host_values_share_the_core_environment_and_struct_shape() {
        let mut env = Env::<Value<ForeignPayload, &'static str>>::new();
        env.bind("external", Value::Host(ForeignPayload::Foreign));
        let item = env.lookup("external").unwrap().clone();
        let record = Value::Struct(vec![("item".into(), item)]);
        assert_eq!(record.field("item").unwrap().kind_name(), "foreign");
        assert!(!record.is_err());
    }

    fn named(name: &str) -> TypeExpr {
        TypeExpr::Named(Ident::new(name, Default::default()))
    }

    #[test]
    fn boundary_types_recurse_through_lists_and_structs() {
        let ty = TypeExpr::List(alloc::boxed::Box::new(TypeExpr::Struct(vec![(
            Ident::new("count", Default::default()),
            named("int"),
        )])));
        let valid = Value::<ForeignPayload, ()>::List(vec![Value::Struct(vec![
            ("count".into(), Value::Int(2)),
            ("extra".into(), Value::Bool(true)),
        ])]);
        assert!(validate_value_type(&valid, &ty, "parameter `items`").is_ok());

        let invalid = Value::<ForeignPayload, ()>::List(vec![Value::Struct(vec![(
            "count".into(),
            Value::Str("two".into()),
        )])]);
        let error = validate_value_type(&invalid, &ty, "parameter `items`").unwrap_err();
        assert_eq!(error.expected, "parameter `items`: [{ count: int }]");
        assert_eq!(error.actual, "parameter `items`[0].count: string");
    }

    #[test]
    fn named_types_distinguish_host_kinds_schema_markers_and_any() {
        let string = Value::<ForeignPayload, ()>::Str("value".into());
        let error = validate_value_type(&string, &named("resource"), "return value").unwrap_err();
        assert_eq!(error.expected, "return value: resource");
        assert_eq!(error.actual, "return value: string");
        assert!(validate_value_type(&string, &named("any"), "return value").is_ok());
        assert!(validate_value_type(&string, &named("value"), "return value").is_ok());
        assert!(validate_value_type(&string, &named("Review"), "return value").is_ok());
        assert!(validate_value_type(&string, &named("innt"), "return value").is_err());
        assert!(
            validate_value_type(
                &Value::<ForeignPayload, ()>::List(vec![Value::Int(1)]),
                &named("list"),
                "return value",
            )
            .is_ok()
        );
        assert!(
            validate_value_type(
                &Value::<ForeignPayload, ()>::Host(ForeignPayload::Resource),
                &named("Resource"),
                "return value",
            )
            .is_ok()
        );
        assert!(
            validate_value_type(
                &Value::<ForeignPayload, ()>::Err(()),
                &named("any"),
                "return value",
            )
            .is_err()
        );
    }
}
