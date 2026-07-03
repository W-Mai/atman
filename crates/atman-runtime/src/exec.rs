use atman_dsl::ast::{FlowDecl, Stmt};

use crate::env::Env;
use crate::error::RuntimeError;
use crate::eval::eval_expr;
use crate::value::Value;

pub enum StmtOutcome {
    Continue,
    Return(Value),
    Err(RuntimeError),
}

pub fn exec_stmts(stmts: &[Stmt], env: &mut Env) -> StmtOutcome {
    for stmt in stmts {
        match exec_stmt(stmt, env) {
            StmtOutcome::Continue => continue,
            outcome => return outcome,
        }
    }
    StmtOutcome::Continue
}

fn exec_stmt(stmt: &Stmt, env: &mut Env) -> StmtOutcome {
    match stmt {
        Stmt::Bind { name, value } => {
            let v = eval_expr(value, env);
            if let Value::Err(e) = v {
                return StmtOutcome::Err(e);
            }
            env.bind(name.name.clone(), v);
            StmtOutcome::Continue
        }
        Stmt::When { cond, body } => {
            let c = eval_expr(cond, env);
            match c {
                Value::Bool(true) => exec_stmts(body, env),
                Value::Bool(false) => StmtOutcome::Continue,
                Value::Err(e) => StmtOutcome::Err(e),
                other => StmtOutcome::Err(RuntimeError::TypeMismatch {
                    expected: "bool".into(),
                    actual: other.kind_name().into(),
                }),
            }
        }
        Stmt::Return { value } => {
            let v = eval_expr(value, env);
            if let Value::Err(e) = v {
                return StmtOutcome::Err(e);
            }
            StmtOutcome::Return(v)
        }
        Stmt::Expr(e) => {
            let v = eval_expr(e, env);
            if let Value::Err(err) = v {
                return StmtOutcome::Err(err);
            }
            StmtOutcome::Continue
        }
    }
}

pub fn exec_flow(flow: &FlowDecl, args: Vec<(String, Value)>) -> Result<Value, RuntimeError> {
    let mut env = Env::new();
    for (name, value) in args {
        env.bind(name, value);
    }
    match exec_stmts(&flow.body, &mut env) {
        StmtOutcome::Return(v) => Ok(v),
        StmtOutcome::Err(e) => Err(e),
        StmtOutcome::Continue => Ok(Value::Unit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_dsl::parse::parse_file;

    fn run(src: &str, args: Vec<(String, Value)>) -> Result<Value, RuntimeError> {
        let file = parse_file(src).expect("parse test src");
        exec_flow(&file.flows[0], args)
    }

    #[test]
    fn bind_and_return() {
        let out = run(
            r#"flow t() -> Int {
    x = 1
    y = x + 2
    return y
}
"#,
            vec![],
        )
        .unwrap();
        assert!(matches!(out, Value::Int(3)));
    }

    #[test]
    fn when_true_executes_body() {
        let out = run(
            r#"flow t() -> Int {
    x = 5
    when x > 3 {
        return 42
    }
    return 0
}
"#,
            vec![],
        )
        .unwrap();
        assert!(matches!(out, Value::Int(42)));
    }

    #[test]
    fn when_false_skips_body() {
        let out = run(
            r#"flow t() -> Int {
    x = 1
    when x > 3 {
        return 42
    }
    return 0
}
"#,
            vec![],
        )
        .unwrap();
        assert!(matches!(out, Value::Int(0)));
    }

    #[test]
    fn err_in_bind_stops_flow() {
        let err = run(
            r#"flow t() -> Int {
    x = missing
    return 1
}
"#,
            vec![],
        )
        .unwrap_err();
        assert!(matches!(err, RuntimeError::UndefinedVar(n) if n == "missing"));
    }

    #[test]
    fn flow_args_bind_before_body() {
        let out = run(
            r#"flow t() -> Int {
    return n + 1
}
"#,
            vec![("n".into(), Value::Int(4))],
        )
        .unwrap();
        assert!(matches!(out, Value::Int(5)));
    }

    #[test]
    fn when_cond_non_bool_is_type_error() {
        let err = run(
            r#"flow t() -> Int {
    when 1 {
        return 1
    }
    return 0
}
"#,
            vec![],
        )
        .unwrap_err();
        assert!(matches!(err, RuntimeError::TypeMismatch { .. }));
    }

    #[test]
    fn flow_falls_through_to_unit_without_return() {
        let out = run(
            r#"flow t() {
    x = 1
}
"#,
            vec![],
        )
        .unwrap();
        assert!(matches!(out, Value::Unit));
    }
}
