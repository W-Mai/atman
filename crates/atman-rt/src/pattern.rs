use alloc::string::String;

use crate::{
    Env,
    ast::{Pattern, PatternFieldBinding},
};

/// The value shape needed by a destructuring pattern.
pub trait PatternValue: Clone {
    fn struct_fields(&self) -> Option<&[(String, Self)]>;
    fn kind_name(&self) -> &str;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatternBindError {
    NonStruct { actual: String },
    MissingField { name: String },
}

/// Bind a value using a flow pattern, preserving declaration order and scope.
pub fn bind_pattern<V: PatternValue>(
    pattern: &Pattern,
    value: V,
    env: &mut Env<V>,
) -> Result<(), PatternBindError> {
    match pattern {
        Pattern::Ident(id) => {
            env.bind(id.name.clone(), value);
            Ok(())
        }
        Pattern::Struct { fields } => {
            let pairs = value
                .struct_fields()
                .ok_or_else(|| PatternBindError::NonStruct {
                    actual: value.kind_name().into(),
                })?;
            for field in fields {
                let (_, matched) = pairs
                    .iter()
                    .find(|(name, _)| name == &field.source.name)
                    .ok_or_else(|| PatternBindError::MissingField {
                        name: field.source.name.clone(),
                    })?;
                match &field.binding {
                    PatternFieldBinding::Same => {
                        env.bind(field.source.name.clone(), matched.clone());
                    }
                    PatternFieldBinding::Rename(target) => {
                        env.bind(target.name.clone(), matched.clone());
                    }
                    PatternFieldBinding::Nested(inner) => {
                        bind_pattern(inner, matched.clone(), env)?;
                    }
                }
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use alloc::{boxed::Box, string::ToString, vec, vec::Vec};

    use super::*;
    use crate::ast::{Ident, PatternField, Span};

    #[derive(Clone)]
    enum TestValue {
        Int(i64),
        Struct(Vec<(String, Self)>),
    }

    impl PatternValue for TestValue {
        fn struct_fields(&self) -> Option<&[(String, Self)]> {
            match self {
                Self::Struct(fields) => Some(fields),
                Self::Int(_) => None,
            }
        }

        fn kind_name(&self) -> &str {
            match self {
                Self::Int(_) => "int",
                Self::Struct(_) => "struct",
            }
        }
    }

    fn ident(name: &str) -> Ident {
        Ident::new(name, Span::default())
    }

    #[test]
    fn nested_binding_uses_only_the_value_shape_contract() {
        let pattern = Pattern::Struct {
            fields: vec![PatternField {
                source: ident("outer"),
                binding: PatternFieldBinding::Nested(Box::new(Pattern::Struct {
                    fields: vec![PatternField {
                        source: ident("inner"),
                        binding: PatternFieldBinding::Rename(ident("renamed")),
                    }],
                })),
            }],
        };
        let value = TestValue::Struct(vec![(
            "outer".to_string(),
            TestValue::Struct(vec![("inner".to_string(), TestValue::Int(7))]),
        )]);
        let mut env = Env::new();

        bind_pattern(&pattern, value, &mut env).unwrap();

        assert!(matches!(env.lookup("renamed"), Some(TestValue::Int(7))));
    }

    #[test]
    fn bind_errors_keep_value_shape_and_missing_field_distinct() {
        let pattern = Pattern::Struct {
            fields: vec![PatternField {
                source: ident("missing"),
                binding: PatternFieldBinding::Same,
            }],
        };
        let mut env = Env::new();
        assert_eq!(
            bind_pattern(&pattern, TestValue::Int(1), &mut env),
            Err(PatternBindError::NonStruct {
                actual: "int".to_string()
            })
        );
        assert_eq!(
            bind_pattern(&pattern, TestValue::Struct(vec![]), &mut env),
            Err(PatternBindError::MissingField {
                name: "missing".to_string()
            })
        );
    }
}
