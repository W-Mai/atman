use std::path::PathBuf;

use crate::error::RuntimeError;
use crate::hunk::EditProposal;
use crate::message::Message;

#[derive(Debug, Clone)]
pub enum Value {
    Unit,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Path(PathBuf),
    List(Vec<Value>),
    Struct(Vec<(String, Value)>),
    Message(Message),
    EditProposal(Box<EditProposal>),
    Err(RuntimeError),
}

impl Value {
    pub fn is_err(&self) -> bool {
        matches!(self, Value::Err(_))
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Value::Unit => "unit",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::Path(_) => "path",
            Value::List(_) => "list",
            Value::Struct(_) => "struct",
            Value::Message(_) => "message",
            Value::EditProposal(_) => "edit_proposal",
            Value::Err(_) => "err",
        }
    }

    pub fn field(&self, name: &str) -> Option<&Value> {
        if let Value::Struct(fields) = self {
            fields.iter().find(|(k, _)| k == name).map(|(_, v)| v)
        } else {
            None
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Unit => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(i) => serde_json::Value::Number((*i).into()),
            Value::Float(f) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Value::Str(s) => serde_json::Value::String(s.clone()),
            Value::Path(p) => serde_json::Value::String(p.display().to_string()),
            Value::List(items) => {
                serde_json::Value::Array(items.iter().map(|v| v.to_json()).collect())
            }
            Value::Struct(fields) => {
                let mut m = serde_json::Map::with_capacity(fields.len());
                for (k, v) in fields {
                    m.insert(k.clone(), v.to_json());
                }
                serde_json::Value::Object(m)
            }
            Value::Message(msg) => serde_json::to_value(msg).unwrap_or(serde_json::Value::Null),
            Value::EditProposal(p) => serde_json::to_value(p).unwrap_or(serde_json::Value::Null),
            Value::Err(e) => serde_json::json!({ "error": e.to_string() }),
        }
    }

    pub fn from_json(v: serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => Value::Unit,
            serde_json::Value::Bool(b) => Value::Bool(b),
            serde_json::Value::Number(n) => {
                if let Some(i) = n.as_i64() {
                    Value::Int(i)
                } else if let Some(f) = n.as_f64() {
                    Value::Float(f)
                } else {
                    Value::Str(n.to_string())
                }
            }
            serde_json::Value::String(s) => Value::Str(s),
            serde_json::Value::Array(items) => {
                Value::List(items.into_iter().map(Value::from_json).collect())
            }
            serde_json::Value::Object(map) => Value::Struct(
                map.into_iter()
                    .map(|(k, v)| (k, Value::from_json(v)))
                    .collect(),
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_names_are_stable() {
        assert_eq!(Value::Unit.kind_name(), "unit");
        assert_eq!(Value::Bool(true).kind_name(), "bool");
        assert_eq!(Value::Int(1).kind_name(), "int");
        assert_eq!(Value::Float(1.0).kind_name(), "float");
        assert_eq!(Value::Str("x".into()).kind_name(), "string");
        assert_eq!(Value::Path(PathBuf::from("/tmp")).kind_name(), "path");
        assert_eq!(Value::List(vec![]).kind_name(), "list");
        assert_eq!(Value::Struct(vec![]).kind_name(), "struct");
        assert_eq!(
            Value::Err(RuntimeError::UndefinedVar("x".into())).kind_name(),
            "err",
        );
    }

    #[test]
    fn is_err_only_true_for_err_variant() {
        assert!(!Value::Unit.is_err());
        assert!(!Value::Bool(false).is_err());
        assert!(Value::Err(RuntimeError::Cancelled("stop".into())).is_err());
    }

    #[test]
    fn struct_field_lookup_returns_by_first_match() {
        let v = Value::Struct(vec![
            ("severity".into(), Value::Str("critical".into())),
            ("count".into(), Value::Int(3)),
        ]);
        assert!(matches!(v.field("severity"), Some(Value::Str(s)) if s == "critical"));
        assert!(matches!(v.field("count"), Some(Value::Int(3))));
        assert!(v.field("missing").is_none());
    }

    #[test]
    fn struct_field_preserves_declaration_order() {
        let v = Value::Struct(vec![
            ("a".into(), Value::Int(1)),
            ("b".into(), Value::Int(2)),
        ]);
        if let Value::Struct(fields) = &v {
            assert_eq!(fields[0].0, "a");
            assert_eq!(fields[1].0, "b");
        } else {
            panic!("expected struct");
        }
    }

    #[test]
    fn runtime_error_display_is_stable() {
        let msg = RuntimeError::TypeMismatch {
            expected: "int".into(),
            actual: "string".into(),
        }
        .to_string();
        assert_eq!(msg, "type mismatch: expected int, got string");
    }
}
