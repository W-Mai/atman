use atman_dsl::ast::{FlowDecl, Stmt};

use crate::env::Env;
use crate::error::RuntimeError;
use crate::eval::{EvalCtx, eval_expr};
use crate::tool::{BoxFut, ToolCtx, ToolRegistry};
use crate::value::Value;

pub enum StmtOutcome {
    Continue,
    Return(Value),
    Err(RuntimeError),
}

pub fn exec_stmts<'a>(
    stmts: &'a [Stmt],
    env: &'a mut Env,
    ctx: &'a EvalCtx<'a>,
) -> BoxFut<'a, StmtOutcome> {
    Box::pin(async move {
        for stmt in stmts {
            let outcome = exec_stmt(stmt, env, ctx).await;
            match outcome {
                StmtOutcome::Continue => continue,
                other => return other,
            }
        }
        StmtOutcome::Continue
    })
}

fn exec_stmt<'a>(
    stmt: &'a Stmt,
    env: &'a mut Env,
    ctx: &'a EvalCtx<'a>,
) -> BoxFut<'a, StmtOutcome> {
    Box::pin(async move {
        match stmt {
            Stmt::Bind { name, value } => {
                let v = eval_expr(value, env, ctx).await;
                if let Value::Err(e) = v {
                    return StmtOutcome::Err(e);
                }
                env.bind(name.name.clone(), v);
                StmtOutcome::Continue
            }
            Stmt::When { cond, body } => {
                let c = eval_expr(cond, env, ctx).await;
                match c {
                    Value::Bool(true) => exec_stmts(body, env, ctx).await,
                    Value::Bool(false) => StmtOutcome::Continue,
                    Value::Err(e) => StmtOutcome::Err(e),
                    other => StmtOutcome::Err(RuntimeError::TypeMismatch {
                        expected: "bool".into(),
                        actual: other.kind_name().into(),
                    }),
                }
            }
            Stmt::Return { value } => {
                let v = eval_expr(value, env, ctx).await;
                if let Value::Err(e) = v {
                    return StmtOutcome::Err(e);
                }
                StmtOutcome::Return(v)
            }
            Stmt::Expr(e) => {
                let v = eval_expr(e, env, ctx).await;
                if let Value::Err(err) = v {
                    return StmtOutcome::Err(err);
                }
                StmtOutcome::Continue
            }
        }
    })
}

pub async fn exec_flow(
    flow: &FlowDecl,
    args: Vec<(String, Value)>,
    tools: &ToolRegistry,
    tool_ctx: &ToolCtx,
) -> Result<Value, RuntimeError> {
    let mut env = Env::new();
    for (name, value) in args {
        env.bind(name, value);
    }
    let ctx = EvalCtx { tools, tool_ctx };
    match exec_stmts(&flow.body, &mut env, &ctx).await {
        StmtOutcome::Return(v) => Ok(v),
        StmtOutcome::Err(e) => Err(e),
        StmtOutcome::Continue => Ok(Value::Unit),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_dsl::parse::parse_file;

    async fn run(src: &str, args: Vec<(String, Value)>) -> Result<Value, RuntimeError> {
        let file = parse_file(src).expect("parse test src");
        let tools = ToolRegistry::new();
        let tool_ctx = ToolCtx::new();
        exec_flow(&file.flows[0], args, &tools, &tool_ctx).await
    }

    #[tokio::test]
    async fn bind_and_return() {
        let out = run(
            r#"flow t() -> Int {
    x = 1
    y = x + 2
    return y
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(3)));
    }

    #[tokio::test]
    async fn when_true_executes_body() {
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
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(42)));
    }

    #[tokio::test]
    async fn when_false_skips_body() {
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
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(0)));
    }

    #[tokio::test]
    async fn err_in_bind_stops_flow() {
        let err = run(
            r#"flow t() -> Int {
    x = missing
    return 1
}
"#,
            vec![],
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RuntimeError::UndefinedVar(n) if n == "missing"));
    }

    #[tokio::test]
    async fn flow_args_bind_before_body() {
        let out = run(
            r#"flow t() -> Int {
    return n + 1
}
"#,
            vec![("n".into(), Value::Int(4))],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Int(5)));
    }

    #[tokio::test]
    async fn when_cond_non_bool_is_type_error() {
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
        .await
        .unwrap_err();
        assert!(matches!(err, RuntimeError::TypeMismatch { .. }));
    }

    #[tokio::test]
    async fn flow_falls_through_to_unit_without_return() {
        let out = run(
            r#"flow t() {
    x = 1
}
"#,
            vec![],
        )
        .await
        .unwrap();
        assert!(matches!(out, Value::Unit));
    }
}
