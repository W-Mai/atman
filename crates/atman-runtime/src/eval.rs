use atman_dsl::ast::{BinOp, Expr, Literal};

use crate::env::Env;
use crate::error::RuntimeError;
use crate::value::Value;

pub fn eval_expr(expr: &Expr, env: &Env) -> Value {
    match expr {
        Expr::Literal(lit) => eval_literal(lit),
        Expr::Ident(id) => match env.lookup(&id.name) {
            Some(v) => v.clone(),
            None => Value::Err(RuntimeError::UndefinedVar(id.name.clone())),
        },
        Expr::FileRef(_) => {
            // File loading is a runtime concern handled by tool nodes, not
            // a bare expression: keep it symbolic so downstream nodes decide.
            Value::Err(RuntimeError::ToolFailed(
                "file references only resolve inside node args".into(),
            ))
        }
        Expr::Member { base, field } => {
            let base_v = eval_expr(base, env);
            if base_v.is_err() {
                return base_v;
            }
            match base_v.field(&field.name) {
                Some(v) => v.clone(),
                None => Value::Err(RuntimeError::UndefinedVar(format!(".{}", field.name))),
            }
        }
        Expr::Binary { op, left, right } => {
            let l = eval_expr(left, env);
            if l.is_err() {
                return l;
            }
            let r = eval_expr(right, env);
            if r.is_err() {
                return r;
            }
            eval_binop(*op, &l, &r)
        }
        Expr::List(items) => {
            let mut acc = Vec::with_capacity(items.len());
            for item in items {
                let v = eval_expr(item, env);
                if v.is_err() {
                    return v;
                }
                acc.push(v);
            }
            Value::List(acc)
        }
        Expr::Struct(fields) => {
            let mut acc = Vec::with_capacity(fields.len());
            for (k, v) in fields {
                let val = eval_expr(v, env);
                if val.is_err() {
                    return val;
                }
                acc.push((k.name.clone(), val));
            }
            Value::Struct(acc)
        }
        Expr::Node(_) | Expr::Call { .. } => Value::Err(RuntimeError::ToolFailed(
            "node/call evaluation not yet implemented in W2 T2".into(),
        )),
    }
}

fn eval_literal(lit: &Literal) -> Value {
    match lit {
        Literal::Str(s) => Value::Str(s.clone()),
        Literal::Int(n) => Value::Int(*n),
        Literal::Float(f) => Value::Float(*f),
        Literal::Bool(b) => Value::Bool(*b),
    }
}

fn eval_binop(op: BinOp, l: &Value, r: &Value) -> Value {
    match op {
        BinOp::Eq => Value::Bool(value_eq(l, r)),
        BinOp::Ne => Value::Bool(!value_eq(l, r)),
        BinOp::Lt => value_cmp(l, r, |a, b| a < b, |a, b| a < b, |a, b| a < b),
        BinOp::Le => value_cmp(l, r, |a, b| a <= b, |a, b| a <= b, |a, b| a <= b),
        BinOp::Gt => value_cmp(l, r, |a, b| a > b, |a, b| a > b, |a, b| a > b),
        BinOp::Ge => value_cmp(l, r, |a, b| a >= b, |a, b| a >= b, |a, b| a >= b),
        BinOp::And => match (l, r) {
            (Value::Bool(a), Value::Bool(b)) => Value::Bool(*a && *b),
            _ => type_mismatch("bool && bool", l, r),
        },
        BinOp::Or => match (l, r) {
            (Value::Bool(a), Value::Bool(b)) => Value::Bool(*a || *b),
            _ => type_mismatch("bool || bool", l, r),
        },
        BinOp::Add => match (l, r) {
            (Value::Int(a), Value::Int(b)) => Value::Int(a + b),
            (Value::Float(a), Value::Float(b)) => Value::Float(a + b),
            (Value::Str(a), Value::Str(b)) => Value::Str(format!("{a}{b}")),
            _ => type_mismatch("int+int | float+float | string+string", l, r),
        },
    }
}

fn value_eq(l: &Value, r: &Value) -> bool {
    match (l, r) {
        (Value::Unit, Value::Unit) => true,
        (Value::Bool(a), Value::Bool(b)) => a == b,
        (Value::Int(a), Value::Int(b)) => a == b,
        (Value::Float(a), Value::Float(b)) => a == b,
        (Value::Str(a), Value::Str(b)) => a == b,
        (Value::Path(a), Value::Path(b)) => a == b,
        _ => false,
    }
}

fn value_cmp(
    l: &Value,
    r: &Value,
    int_cmp: fn(i64, i64) -> bool,
    float_cmp: fn(f64, f64) -> bool,
    str_cmp: fn(&str, &str) -> bool,
) -> Value {
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => Value::Bool(int_cmp(*a, *b)),
        (Value::Float(a), Value::Float(b)) => Value::Bool(float_cmp(*a, *b)),
        (Value::Str(a), Value::Str(b)) => Value::Bool(str_cmp(a, b)),
        _ => type_mismatch("comparable pair", l, r),
    }
}

fn type_mismatch(expected: &str, l: &Value, r: &Value) -> Value {
    Value::Err(RuntimeError::TypeMismatch {
        expected: expected.into(),
        actual: format!("{} vs {}", l.kind_name(), r.kind_name()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_dsl::parse::parse_file;

    fn eval_snippet(expr_src: &str) -> Value {
        let src = format!("flow t() {{\n    return {expr_src}\n}}\n");
        let file = parse_file(&src).expect("parse test snippet");
        let stmt = &file.flows[0].body[0];
        if let atman_dsl::ast::Stmt::Return { value } = stmt {
            eval_expr(value, &Env::new())
        } else {
            panic!("expected return statement");
        }
    }

    #[test]
    fn literals_evaluate() {
        assert!(matches!(eval_snippet("42"), Value::Int(42)));
        assert!(matches!(eval_snippet("true"), Value::Bool(true)));
        assert!(matches!(eval_snippet(r#""hello""#), Value::Str(s) if s == "hello"));
    }

    #[test]
    fn ident_lookup_uses_env() {
        let mut env = Env::new();
        env.bind("x", Value::Int(9));
        let src = "flow t() { return x }\n";
        let file = parse_file(src).unwrap();
        if let atman_dsl::ast::Stmt::Return { value } = &file.flows[0].body[0] {
            assert!(matches!(eval_expr(value, &env), Value::Int(9)));
        }
    }

    #[test]
    fn undefined_ident_yields_err_value() {
        assert!(matches!(
            eval_snippet("missing"),
            Value::Err(RuntimeError::UndefinedVar(name)) if name == "missing"
        ));
    }

    #[test]
    fn binary_arithmetic_and_comparison() {
        assert!(matches!(eval_snippet("1 == 1"), Value::Bool(true)));
        assert!(matches!(eval_snippet("2 < 3"), Value::Bool(true)));
        assert!(matches!(eval_snippet(r#""a" + "b""#), Value::Str(s) if s == "ab"));
    }

    #[test]
    fn type_mismatch_bubbles_up() {
        assert!(matches!(
            eval_snippet(r#"1 + "x""#),
            Value::Err(RuntimeError::TypeMismatch { .. })
        ));
    }

    #[test]
    fn err_short_circuits_binary() {
        assert!(matches!(
            eval_snippet("missing == 1"),
            Value::Err(RuntimeError::UndefinedVar(name)) if name == "missing"
        ));
    }

    #[test]
    fn list_evaluates_all_items() {
        let v = eval_snippet("[1, 2, 3]");
        if let Value::List(items) = v {
            assert_eq!(items.len(), 3);
            assert!(matches!(items[2], Value::Int(3)));
        } else {
            panic!("expected list");
        }
    }

    #[test]
    fn struct_literal_evaluates_fields_in_order() {
        let v = eval_snippet(r#"{ severity: "critical", count: 3 }"#);
        if let Value::Struct(fields) = v {
            assert_eq!(fields[0].0, "severity");
            assert!(matches!(&fields[0].1, Value::Str(s) if s == "critical"));
            assert_eq!(fields[1].0, "count");
        } else {
            panic!("expected struct");
        }
    }

    #[test]
    fn member_access_on_struct() {
        let mut env = Env::new();
        env.bind(
            "primary",
            Value::Struct(vec![("severity".into(), Value::Str("critical".into()))]),
        );
        let src = "flow t() { return primary.severity }\n";
        let file = parse_file(src).unwrap();
        if let atman_dsl::ast::Stmt::Return { value } = &file.flows[0].body[0] {
            assert!(matches!(eval_expr(value, &env), Value::Str(s) if s == "critical"));
        }
    }
}
