use std::path::PathBuf;

use crate::error::RuntimeError;
use crate::hunk::EditProposal;
use crate::message::Message;

#[derive(Debug, Clone)]
pub enum AtmanPayload {
    Path(PathBuf),
    Message(Message),
    EditProposal(Box<EditProposal>),
}

impl atman_rt::HostPayload for AtmanPayload {
    fn kind_name(&self) -> &'static str {
        match self {
            Self::Path(_) => "path",
            Self::Message(_) => "message",
            Self::EditProposal(_) => "edit_proposal",
        }
    }
}

impl atman_rt::HostValueOps for AtmanPayload {
    fn additive_text(&self) -> Option<String> {
        match self {
            Self::Path(path) => Some(path.display().to_string()),
            _ => None,
        }
    }

    fn equals(&self, other: &Self) -> bool {
        matches!((self, other), (Self::Path(left), Self::Path(right)) if left == right)
    }

    fn add_expected() -> &'static str {
        "int+int | float+float | string+string | string+path | path+string"
    }
}

impl atman_rt::ValueError for RuntimeError {
    fn type_mismatch(expected: &str, actual: String) -> Self {
        Self::TypeMismatch {
            expected: expected.into(),
            actual,
        }
    }

    fn integer_div_by_zero() -> Self {
        Self::ToolFailed("integer div by zero".into())
    }

    fn integer_mod_by_zero() -> Self {
        Self::ToolFailed("integer mod by zero".into())
    }
}

pub type AtmanValue = atman_rt::Value<AtmanPayload, RuntimeError>;
pub(crate) use AtmanValue as Value;

pub trait ValueJson {
    fn to_json(&self) -> serde_json::Value;
    fn from_json(value: serde_json::Value) -> Self;
}

impl ValueJson for AtmanValue {
    fn to_json(&self) -> serde_json::Value {
        match self {
            Value::Unit => serde_json::Value::Null,
            Value::Bool(b) => serde_json::Value::Bool(*b),
            Value::Int(i) => serde_json::Value::Number((*i).into()),
            Value::Float(f) => serde_json::Number::from_f64(*f)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Value::Str(s) => serde_json::Value::String(s.clone()),
            Value::Host(AtmanPayload::Path(p)) => {
                serde_json::Value::String(p.display().to_string())
            }
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
            Value::Host(AtmanPayload::Message(msg)) => {
                serde_json::to_value(msg).unwrap_or(serde_json::Value::Null)
            }
            Value::Host(AtmanPayload::EditProposal(p)) => {
                serde_json::to_value(p).unwrap_or(serde_json::Value::Null)
            }
            Value::Err(e) => serde_json::json!({ "error": e.to_string() }),
            Value::Lambda { .. } => serde_json::Value::Null,
        }
    }

    fn from_json(v: serde_json::Value) -> Self {
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
        assert_eq!(
            Value::Host(AtmanPayload::Path(PathBuf::from("/tmp"))).kind_name(),
            "path"
        );
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

    #[test]
    fn host_values_keep_the_existing_json_shape() {
        let path = Value::Host(AtmanPayload::Path(PathBuf::from("src/main.rs")));
        assert_eq!(path.to_json(), serde_json::json!("src/main.rs"));

        let message = Message::assistant_text(crate::event::TurnId::now(), "hello");
        let value = Value::Host(AtmanPayload::Message(message.clone()));
        assert_eq!(value.to_json(), serde_json::to_value(message).unwrap());
    }

    #[test]
    fn path_expression_behavior_uses_the_atman_host_adapter() {
        use atman_rt::ast::BinOp;

        let path = Value::Host(AtmanPayload::Path(PathBuf::from("src/main.rs")));
        let text = Value::Str("file: ".into());
        assert!(matches!(
            atman_rt::eval_binary(BinOp::Add, &text, &path),
            Value::Str(result) if result == "file: src/main.rs"
        ));
        assert!(matches!(
            atman_rt::eval_binary(BinOp::Eq, &path, &path),
            Value::Bool(true)
        ));
    }
}
