use alloc::{format, string::String};

use crate::{
    HostPayload, Value,
    ast::{BinOp, Literal, UnOp},
};

/// Integer operation that could not produce an `i64` result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IntegerOperation {
    Add,
    Subtract,
    Multiply,
    Divide,
    Remainder,
    Negate,
}

impl IntegerOperation {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Add => "addition",
            Self::Subtract => "subtraction",
            Self::Multiply => "multiplication",
            Self::Divide => "division",
            Self::Remainder => "remainder",
            Self::Negate => "negation",
        }
    }
}

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

/// Maps portable evaluation and execution errors into an embedding host's error type.
pub trait ValueError {
    fn type_mismatch(expected: &str, actual: String) -> Self;
    fn missing_argument(name: &str) -> Self;
    fn binding_error(error: crate::binding::BindingError) -> Self
    where
        Self: Sized,
    {
        Self::type_mismatch("valid host binding value", format!("{error}"))
    }
    fn empty_list(name: &str) -> Self
    where
        Self: Sized,
    {
        Self::type_mismatch("non-empty list", format!("{name}: empty list"))
    }
    fn index_out_of_bounds(index: i64, len: usize) -> Self
    where
        Self: Sized,
    {
        Self::type_mismatch(
            "list index within bounds",
            format!("index {index} for length {len}"),
        )
    }
    fn integer_div_by_zero() -> Self;
    fn integer_mod_by_zero() -> Self;
    fn integer_overflow(operation: IntegerOperation) -> Self
    where
        Self: Sized,
    {
        Self::type_mismatch(
            "integer result within i64 range",
            format!("{} overflow", operation.as_str()),
        )
    }
    fn operation_limit_exceeded(max_operations: usize) -> Self
    where
        Self: Sized,
    {
        Self::type_mismatch(
            "execution within the configured operation limit",
            format!("operation limit of {max_operations} exceeded"),
        )
    }
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
    IntegerOverflow {
        operation: IntegerOperation,
    },
    MissingArgument(String),
    EmptyList(String),
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

    fn empty_list(name: &str) -> Self {
        Self::EmptyList(name.into())
    }

    fn integer_div_by_zero() -> Self {
        Self::IntegerDivByZero
    }

    fn integer_mod_by_zero() -> Self {
        Self::IntegerModByZero
    }

    fn integer_overflow(operation: IntegerOperation) -> Self {
        Self::IntegerOverflow { operation }
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
            (Value::Int(a), Value::Int(b)) => {
                checked_integer(a.checked_add(*b), IntegerOperation::Add)
            }
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
            (Value::Int(a), Value::Int(b)) => {
                checked_integer(a.checked_sub(*b), IntegerOperation::Subtract)
            }
            (Value::Float(a), Value::Float(b)) => Value::Float(a - b),
            _ => type_mismatch("int-int | float-float", left, right),
        },
        BinOp::Mul => match (left, right) {
            (Value::Int(a), Value::Int(b)) => {
                checked_integer(a.checked_mul(*b), IntegerOperation::Multiply)
            }
            (Value::Float(a), Value::Float(b)) => Value::Float(a * b),
            _ => type_mismatch("int*int | float*float", left, right),
        },
        BinOp::Div => match (left, right) {
            (Value::Int(_), Value::Int(0)) => Value::Err(E::integer_div_by_zero()),
            (Value::Int(a), Value::Int(b)) => {
                checked_integer(a.checked_div(*b), IntegerOperation::Divide)
            }
            (Value::Float(a), Value::Float(b)) => Value::Float(a / b),
            _ => type_mismatch("int/int | float/float", left, right),
        },
        BinOp::Mod => match (left, right) {
            (Value::Int(_), Value::Int(0)) => Value::Err(E::integer_mod_by_zero()),
            (Value::Int(a), Value::Int(b)) => {
                checked_integer(a.checked_rem(*b), IntegerOperation::Remainder)
            }
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
            Value::Int(number) => checked_integer(number.checked_neg(), IntegerOperation::Negate),
            Value::Float(number) => Value::Float(-number),
            other => Value::Err(E::type_mismatch("int or float", other.kind_name().into())),
        },
    }
}

fn checked_integer<P, E: ValueError>(
    result: Option<i64>,
    operation: IntegerOperation,
) -> Value<P, E> {
    match result {
        Some(value) => Value::Int(value),
        None => Value::Err(E::integer_overflow(operation)),
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

    fn assert_overflow(value: Value<(), EvalError>, operation: IntegerOperation) {
        assert_eq!(
            match value {
                Value::Err(EvalError::IntegerOverflow { operation }) => operation,
                _ => panic!("expected integer overflow"),
            },
            operation,
        );
    }

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

    #[test]
    fn integer_arithmetic_reports_every_overflow_without_panicking() {
        let max = Value::<(), EvalError>::Int(i64::MAX);
        let min = Value::<(), EvalError>::Int(i64::MIN);
        let one = Value::<(), EvalError>::Int(1);
        let two = Value::<(), EvalError>::Int(2);
        let negative_one = Value::<(), EvalError>::Int(-1);

        assert_overflow(eval_binary(BinOp::Add, &max, &one), IntegerOperation::Add);
        assert_overflow(
            eval_binary(BinOp::Sub, &min, &one),
            IntegerOperation::Subtract,
        );
        assert_overflow(
            eval_binary(BinOp::Mul, &max, &two),
            IntegerOperation::Multiply,
        );
        assert_overflow(
            eval_binary(BinOp::Div, &min, &negative_one),
            IntegerOperation::Divide,
        );
        assert_overflow(
            eval_binary(BinOp::Mod, &min, &negative_one),
            IntegerOperation::Remainder,
        );
        assert_overflow(eval_unary(UnOp::Neg, &min), IntegerOperation::Negate);
    }

    #[test]
    fn integer_zero_divisors_keep_their_specific_errors() {
        let value = Value::<(), EvalError>::Int(1);
        let zero = Value::<(), EvalError>::Int(0);

        assert!(matches!(
            eval_binary(BinOp::Div, &value, &zero),
            Value::Err(EvalError::IntegerDivByZero)
        ));
        assert!(matches!(
            eval_binary(BinOp::Mod, &value, &zero),
            Value::Err(EvalError::IntegerModByZero)
        ));
    }
}
