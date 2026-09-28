use alloc::{format, string::String};

use crate::{
    HostPayload, Value,
    ast::{BinOp, Literal, UnOp},
};

/// Host-defined behavior for opaque values in flow expressions.
pub trait HostValueOps: HostPayload {
    fn additive_text(&self) -> Option<String> {
        None
    }

    fn equals(&self, _other: &Self) -> bool {
        false
    }

    fn add_expected() -> &'static str {
        "int+int | float+float | string+string"
    }
}

impl HostValueOps for () {}

/// Maps portable expression errors into an embedding host's error type.
pub trait ValueError {
    fn type_mismatch(expected: &str, actual: String) -> Self;
    fn missing_argument(name: &str) -> Self;
    fn integer_div_by_zero() -> Self;
    fn integer_mod_by_zero() -> Self;
    fn missing_positional_argument(name: &str, index: usize) -> Self;
    fn invalid_lambda_arity(name: &str, expected: &str, actual: usize) -> Self;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalError {
    TypeMismatch {
        expected: String,
        actual: String,
    },
    IntegerDivByZero,
    IntegerModByZero,
    MissingArgument(String),
    MissingPositionalArgument {
        name: String,
        index: usize,
    },
    InvalidLambdaArity {
        name: String,
        expected: String,
        actual: usize,
    },
}

impl ValueError for EvalError {
    fn type_mismatch(expected: &str, actual: String) -> Self {
        Self::TypeMismatch {
            expected: expected.into(),
            actual,
        }
    }

    fn missing_argument(name: &str) -> Self {
        Self::MissingArgument(name.into())
    }

    fn integer_div_by_zero() -> Self {
        Self::IntegerDivByZero
    }

    fn integer_mod_by_zero() -> Self {
        Self::IntegerModByZero
    }

    fn missing_positional_argument(name: &str, index: usize) -> Self {
        Self::MissingPositionalArgument {
            name: name.into(),
            index,
        }
    }

    fn invalid_lambda_arity(name: &str, expected: &str, actual: usize) -> Self {
        Self::InvalidLambdaArity {
            name: name.into(),
            expected: expected.into(),
            actual,
        }
    }
}

pub fn eval_literal<P, E>(literal: &Literal) -> Value<P, E> {
    match literal {
        Literal::Str(value) => Value::Str(value.clone()),
        Literal::Int(value) => Value::Int(*value),
        Literal::Float(value) => Value::Float(*value),
        Literal::Bool(value) => Value::Bool(*value),
    }
}

pub fn eval_binary<P: HostValueOps, E: ValueError>(
    op: BinOp,
    left: &Value<P, E>,
    right: &Value<P, E>,
) -> Value<P, E> {
    match op {
        BinOp::Eq => Value::Bool(value_eq(left, right)),
        BinOp::Ne => Value::Bool(!value_eq(left, right)),
        BinOp::Lt => value_cmp(left, right, |a, b| a < b, |a, b| a < b, |a, b| a < b),
        BinOp::Le => value_cmp(left, right, |a, b| a <= b, |a, b| a <= b, |a, b| a <= b),
        BinOp::Gt => value_cmp(left, right, |a, b| a > b, |a, b| a > b, |a, b| a > b),
        BinOp::Ge => value_cmp(left, right, |a, b| a >= b, |a, b| a >= b, |a, b| a >= b),
        BinOp::And => match (left, right) {
            (Value::Bool(a), Value::Bool(b)) => Value::Bool(*a && *b),
            _ => type_mismatch("bool && bool", left, right),
        },
        BinOp::Or => match (left, right) {
            (Value::Bool(a), Value::Bool(b)) => Value::Bool(*a || *b),
            _ => type_mismatch("bool || bool", left, right),
        },
        BinOp::Add => match (left, right) {
            (Value::Int(a), Value::Int(b)) => Value::Int(a + b),
            (Value::Float(a), Value::Float(b)) => Value::Float(a + b),
            (Value::Str(a), Value::Str(b)) => Value::Str(format!("{a}{b}")),
            (Value::Str(a), Value::Host(b)) => match b.additive_text() {
                Some(text) => Value::Str(format!("{a}{text}")),
                None => type_mismatch(P::add_expected(), left, right),
            },
            (Value::Host(a), Value::Str(b)) => match a.additive_text() {
                Some(text) => Value::Str(format!("{text}{b}")),
                None => type_mismatch(P::add_expected(), left, right),
            },
            _ => type_mismatch(P::add_expected(), left, right),
        },
        BinOp::Sub => match (left, right) {
            (Value::Int(a), Value::Int(b)) => Value::Int(a - b),
            (Value::Float(a), Value::Float(b)) => Value::Float(a - b),
            _ => type_mismatch("int-int | float-float", left, right),
        },
        BinOp::Mul => match (left, right) {
            (Value::Int(a), Value::Int(b)) => Value::Int(a * b),
            (Value::Float(a), Value::Float(b)) => Value::Float(a * b),
            _ => type_mismatch("int*int | float*float", left, right),
        },
        BinOp::Div => match (left, right) {
            (Value::Int(_), Value::Int(0)) => Value::Err(E::integer_div_by_zero()),
            (Value::Int(a), Value::Int(b)) => Value::Int(a / b),
            (Value::Float(a), Value::Float(b)) => Value::Float(a / b),
            _ => type_mismatch("int/int | float/float", left, right),
        },
        BinOp::Mod => match (left, right) {
            (Value::Int(_), Value::Int(0)) => Value::Err(E::integer_mod_by_zero()),
            (Value::Int(a), Value::Int(b)) => Value::Int(a % b),
            (Value::Float(a), Value::Float(b)) => Value::Float(a % b),
            _ => type_mismatch("int%int | float%float", left, right),
        },
    }
}

pub fn eval_unary<P: HostValueOps, E: ValueError>(op: UnOp, value: &Value<P, E>) -> Value<P, E> {
    match op {
        UnOp::Not => match value {
            Value::Bool(boolean) => Value::Bool(!boolean),
            other => Value::Err(E::type_mismatch("bool", other.kind_name().into())),
        },
        UnOp::Neg => match value {
            Value::Int(number) => Value::Int(-number),
            Value::Float(number) => Value::Float(-number),
            other => Value::Err(E::type_mismatch("int or float", other.kind_name().into())),
        },
    }
}

fn value_eq<P: HostValueOps, E>(left: &Value<P, E>, right: &Value<P, E>) -> bool {
    match (left, right) {
        (Value::Unit, Value::Unit) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a == b,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Host(a), Value::Host(b)) => a.equals(b),
        _ => false,
    }
}

fn value_cmp<P: HostValueOps, E: ValueError>(
    left: &Value<P, E>,
    right: &Value<P, E>,
    int_cmp: fn(i64, i64) -> bool,
    float_cmp: fn(f64, f64) -> bool,
    str_cmp: fn(&str, &str) -> bool,
) -> Value<P, E> {
    match (left, right) {
        (Value::Int(a), Value::Int(b)) => Value::Bool(int_cmp(*a, *b)),
        (Value::Float(a), Value::Float(b)) => Value::Bool(float_cmp(*a, *b)),
        (Value::Str(a), Value::Str(b)) => Value::Bool(str_cmp(a, b)),
        _ => type_mismatch("comparable pair", left, right),
    }
}

fn type_mismatch<P: HostValueOps, E: ValueError>(
    expected: &str,
    left: &Value<P, E>,
    right: &Value<P, E>,
) -> Value<P, E> {
    Value::Err(E::type_mismatch(
        expected,
        format!("{} vs {}", left.kind_name(), right.kind_name()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_values_evaluate_without_a_host() {
        let left = Value::<(), EvalError>::Int(8);
        let right = Value::<(), EvalError>::Int(2);
        assert!(matches!(
            eval_binary(BinOp::Div, &left, &right),
            Value::Int(4)
        ));
        assert!(matches!(
            eval_binary(BinOp::Mod, &left, &Value::Int(0)),
            Value::Err(EvalError::IntegerModByZero)
        ));
        assert!(matches!(
            eval_binary(BinOp::Eq, &left, &right),
            Value::Bool(false)
        ));
    }
}
