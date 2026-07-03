use atman_dsl::ast::{Arg, BinOp, Expr, Literal, Node};

use crate::env::Env;
use crate::error::RuntimeError;
use crate::tool::{BoxFut, ToolArgs, ToolCtx, ToolRegistry};
use crate::value::Value;

pub struct EvalCtx<'a> {
    pub tools: &'a ToolRegistry,
    pub tool_ctx: &'a ToolCtx,
}

pub fn eval_expr<'a>(expr: &'a Expr, env: &'a Env, ctx: &'a EvalCtx<'a>) -> BoxFut<'a, Value> {
    Box::pin(async move { eval_expr_inner(expr, env, ctx).await })
}

async fn eval_expr_inner<'a>(expr: &'a Expr, env: &'a Env, ctx: &'a EvalCtx<'a>) -> Value {
    match expr {
        Expr::Literal(lit) => eval_literal(lit),
        Expr::Ident(id) => match env.lookup(&id.name) {
            Some(v) => v.clone(),
            None => Value::Err(RuntimeError::UndefinedVar(id.name.clone())),
        },
        Expr::FileRef(_) => Value::Err(RuntimeError::ToolFailed(
            "file references only resolve inside node args".into(),
        )),
        Expr::Member { base, field } => {
            let base_v = eval_expr(base, env, ctx).await;
            if base_v.is_err() {
                return base_v;
            }
            match base_v.field(&field.name) {
                Some(v) => v.clone(),
                None => Value::Err(RuntimeError::UndefinedVar(format!(".{}", field.name))),
            }
        }
        Expr::Binary { op, left, right } => {
            let l = eval_expr(left, env, ctx).await;
            if l.is_err() {
                return l;
            }
            let r = eval_expr(right, env, ctx).await;
            if r.is_err() {
                return r;
            }
            eval_binop(*op, &l, &r)
        }
        Expr::List(items) => {
            let mut acc = Vec::with_capacity(items.len());
            for item in items {
                let v = eval_expr(item, env, ctx).await;
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
                let val = eval_expr(v, env, ctx).await;
                if val.is_err() {
                    return val;
                }
                acc.push((k.name.clone(), val));
            }
            Value::Struct(acc)
        }
        Expr::Node(node) => eval_node(node, env, ctx).await,
        Expr::Call { .. } => Value::Err(RuntimeError::ToolFailed(
            "bare function call not supported; use namespaced tool call".into(),
        )),
    }
}

async fn eval_node<'a>(node: &'a Node, env: &'a Env, ctx: &'a EvalCtx<'a>) -> Value {
    match node {
        Node::ToolCall { path, args } => {
            let name = tool_name(path);
            let tool = match ctx.tools.get(&name) {
                Some(t) => t,
                None => return Value::Err(RuntimeError::UndefinedTool(name)),
            };
            let mut positional = Vec::new();
            let mut named = Vec::new();
            for arg in args {
                match arg {
                    Arg::Positional(e) => {
                        let v = eval_expr(e, env, ctx).await;
                        if v.is_err() {
                            return v;
                        }
                        positional.push(v);
                    }
                    Arg::Named { name, value } => {
                        let v = eval_expr(value, env, ctx).await;
                        if v.is_err() {
                            return v;
                        }
                        named.push((name.name.clone(), v));
                    }
                }
            }
            match tool
                .call(ToolArgs { positional, named }, ctx.tool_ctx)
                .await
            {
                Ok(v) => v,
                Err(e) => Value::Err(e),
            }
        }
        Node::Fanout { items, collect } => match collect {
            atman_dsl::ast::FanoutCollect::All => {
                let futs = items.iter().map(|item| eval_expr(item, env, ctx));
                let results: Vec<Value> = futures::future::join_all(futs).await;
                for v in &results {
                    if let Value::Err(e) = v {
                        return Value::Err(e.clone());
                    }
                }
                Value::List(results)
            }
            atman_dsl::ast::FanoutCollect::First => Value::Err(RuntimeError::ToolFailed(
                "fanout collect: first not yet implemented".into(),
            )),
        },
        Node::Llm { .. } | Node::UserConfirm { .. } => Value::Err(RuntimeError::ToolFailed(
            "node kind not yet implemented in W2".into(),
        )),
    }
}

fn tool_name(path: &[atman_dsl::ast::Ident]) -> String {
    let parts: Vec<&str> = path.iter().map(|i| i.name.as_str()).collect();
    parts.join(".")
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

    async fn eval_snippet(expr_src: &str) -> Value {
        let src = format!("flow t() {{\n    return {expr_src}\n}}\n");
        let file = parse_file(&src).expect("parse test snippet");
        let tools = ToolRegistry::new();
        let tool_ctx = ToolCtx::new();
        let ctx = EvalCtx {
            tools: &tools,
            tool_ctx: &tool_ctx,
        };
        let stmt = &file.flows[0].body[0];
        if let atman_dsl::ast::Stmt::Return { value } = stmt {
            eval_expr(value, &Env::new(), &ctx).await
        } else {
            panic!("expected return statement");
        }
    }

    #[tokio::test]
    async fn literals_evaluate() {
        assert!(matches!(eval_snippet("42").await, Value::Int(42)));
        assert!(matches!(eval_snippet("true").await, Value::Bool(true)));
        assert!(matches!(
            eval_snippet(r#""hello""#).await,
            Value::Str(s) if s == "hello"
        ));
    }

    #[tokio::test]
    async fn undefined_ident_yields_err_value() {
        assert!(matches!(
            eval_snippet("missing").await,
            Value::Err(RuntimeError::UndefinedVar(name)) if name == "missing"
        ));
    }

    #[tokio::test]
    async fn binary_arithmetic_and_comparison() {
        assert!(matches!(eval_snippet("1 == 1").await, Value::Bool(true)));
        assert!(matches!(eval_snippet("2 < 3").await, Value::Bool(true)));
        assert!(matches!(
            eval_snippet(r#""a" + "b""#).await,
            Value::Str(s) if s == "ab"
        ));
    }

    #[tokio::test]
    async fn type_mismatch_bubbles_up() {
        assert!(matches!(
            eval_snippet(r#"1 + "x""#).await,
            Value::Err(RuntimeError::TypeMismatch { .. })
        ));
    }

    #[tokio::test]
    async fn err_short_circuits_binary() {
        assert!(matches!(
            eval_snippet("missing == 1").await,
            Value::Err(RuntimeError::UndefinedVar(name)) if name == "missing"
        ));
    }

    #[tokio::test]
    async fn list_evaluates_all_items() {
        let v = eval_snippet("[1, 2, 3]").await;
        if let Value::List(items) = v {
            assert_eq!(items.len(), 3);
            assert!(matches!(items[2], Value::Int(3)));
        } else {
            panic!("expected list");
        }
    }

    #[tokio::test]
    async fn struct_literal_evaluates_fields_in_order() {
        let v = eval_snippet(r#"{ severity: "critical", count: 3 }"#).await;
        if let Value::Struct(fields) = v {
            assert_eq!(fields[0].0, "severity");
            assert_eq!(fields[1].0, "count");
        } else {
            panic!("expected struct");
        }
    }

    #[tokio::test]
    async fn undefined_tool_returns_undefined_tool_err() {
        let src = r#"flow t() { return fs.readnope("/tmp") }"#;
        let file = parse_file(src).unwrap();
        let tools = ToolRegistry::new();
        let tool_ctx = ToolCtx::new();
        let ctx = EvalCtx {
            tools: &tools,
            tool_ctx: &tool_ctx,
        };
        if let atman_dsl::ast::Stmt::Return { value } = &file.flows[0].body[0] {
            let v = eval_expr(value, &Env::new(), &ctx).await;
            assert!(matches!(
                v,
                Value::Err(RuntimeError::UndefinedTool(name)) if name == "fs.readnope"
            ));
        }
    }

    #[tokio::test]
    async fn fanout_all_gathers_results_in_order() {
        use crate::tools::fs::FsRead;
        use std::sync::Arc;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let pa = dir.path().join("a.txt");
        let pb = dir.path().join("b.txt");
        tokio::fs::write(&pa, b"AAA").await.unwrap();
        tokio::fs::write(&pb, b"BBB").await.unwrap();

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(FsRead));
        let tool_ctx = ToolCtx::new();
        let ctx = EvalCtx {
            tools: &tools,
            tool_ctx: &tool_ctx,
        };

        let mut env = Env::new();
        env.bind("a", Value::Path(pa));
        env.bind("b", Value::Path(pb));

        let src = r#"flow t() { return fanout [ fs.read(a), fs.read(b) ] collect: all }"#;
        let file = parse_file(src).unwrap();
        if let atman_dsl::ast::Stmt::Return { value } = &file.flows[0].body[0] {
            let v = eval_expr(value, &env, &ctx).await;
            if let Value::List(items) = v {
                assert_eq!(items.len(), 2);
                assert!(matches!(&items[0], Value::Str(s) if s == "AAA"));
                assert!(matches!(&items[1], Value::Str(s) if s == "BBB"));
            } else {
                panic!("expected list");
            }
        }
    }

    #[tokio::test]
    async fn fanout_all_short_circuits_on_err() {
        let src = r#"flow t() { return fanout [ 1, missing, 3 ] collect: all }"#;
        let file = parse_file(src).unwrap();
        let tools = ToolRegistry::new();
        let tool_ctx = ToolCtx::new();
        let ctx = EvalCtx {
            tools: &tools,
            tool_ctx: &tool_ctx,
        };
        if let atman_dsl::ast::Stmt::Return { value } = &file.flows[0].body[0] {
            let v = eval_expr(value, &Env::new(), &ctx).await;
            assert!(matches!(
                v,
                Value::Err(RuntimeError::UndefinedVar(name)) if name == "missing"
            ));
        }
    }

    #[tokio::test]
    async fn tool_call_dispatches_via_registry() {
        use crate::tools::fs::FsRead;
        use std::sync::Arc;
        use tempfile::TempDir;

        let dir = TempDir::new().unwrap();
        let path = dir.path().join("hi.txt");
        tokio::fs::write(&path, b"hello runtime").await.unwrap();

        let mut tools = ToolRegistry::new();
        tools.register(Arc::new(FsRead));
        let tool_ctx = ToolCtx::new();
        let ctx = EvalCtx {
            tools: &tools,
            tool_ctx: &tool_ctx,
        };

        let mut env = Env::new();
        env.bind("p", Value::Path(path));

        let src = r#"flow t() { return fs.read(p) }"#;
        let file = parse_file(src).unwrap();
        if let atman_dsl::ast::Stmt::Return { value } = &file.flows[0].body[0] {
            let v = eval_expr(value, &env, &ctx).await;
            assert!(matches!(v, Value::Str(s) if s == "hello runtime"));
        }
    }
}
