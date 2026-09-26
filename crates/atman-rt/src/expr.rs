use alloc::{boxed::Box, format, string::String, sync::Arc, vec, vec::Vec};

use crate::{
    Env, HostFuture, HostValueOps, Value, ValueError,
    ast::{Expr, Ident, Node},
    ops::{eval_binary, eval_literal, eval_unary},
};

pub enum ExpressionEffect<'a> {
    FileRef(&'a str),
    Node(&'a Node),
    Call { func: &'a Ident, args: &'a [Expr] },
}

pub fn is_type_name(name: &str) -> bool {
    matches!(
        name,
        "bool" | "int" | "float" | "string" | "path" | "bytes" | "duration"
    )
}

fn annotation_type_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Ident(id) if is_type_name(&id.name) => Some(id.name.clone()),
        Expr::List(items) if items.len() == 1 => match &items[0] {
            Expr::Ident(id) if is_type_name(&id.name) => Some(format!("list of {}", id.name)),
            _ => None,
        },
        _ => None,
    }
}

/// Supplies external expressions and product-specific error messages.
pub trait ExpressionHost: Sync {
    type Payload: HostValueOps + Clone + Send + Sync;
    type Error: ValueError + Clone + Send + Sync;

    fn undefined_var(&self, name: String) -> Self::Error;
    fn undefined_field(&self, name: String) -> Self::Error;
    fn eval_pipe_rhs<'a>(
        &'a self,
        rhs: &'a Expr,
        piped: Value<Self::Payload, Self::Error>,
        env: &'a Env<Value<Self::Payload, Self::Error>>,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>>;
    fn eval_external<'a>(
        &'a self,
        effect: ExpressionEffect<'a>,
        env: &'a Env<Value<Self::Payload, Self::Error>>,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>>;
}

/// Evaluates portable expressions and delegates external effects to the host.
pub fn eval_expr<'a, H: ExpressionHost>(
    expr: &'a Expr,
    env: &'a Env<Value<H::Payload, H::Error>>,
    host: &'a H,
) -> HostFuture<'a, Value<H::Payload, H::Error>> {
    Box::pin(async move {
        match expr {
            Expr::Literal(literal) => eval_literal(literal),
            Expr::Ident(id) => match env.lookup(&id.name) {
                Some(value) => value.clone(),
                None => Value::Err(host.undefined_var(id.name.clone())),
            },
            Expr::Member { base, field } => {
                let value = eval_expr(base, env, host).await;
                if value.is_err() {
                    return value;
                }
                match value.field(&field.name) {
                    Some(field) => field.clone(),
                    None => Value::Err(host.undefined_field(format!(".{}", field.name))),
                }
            }
            Expr::Binary { op, left, right } => {
                let left = eval_expr(left, env, host).await;
                if left.is_err() {
                    return left;
                }
                let right = eval_expr(right, env, host).await;
                if right.is_err() {
                    return right;
                }
                eval_binary(*op, &left, &right)
            }
            Expr::Unary { op, operand } => {
                let value = eval_expr(operand, env, host).await;
                if value.is_err() {
                    return value;
                }
                eval_unary(*op, &value)
            }
            Expr::List(items) => {
                let mut values = Vec::with_capacity(items.len());
                for item in items {
                    let value = eval_expr(item, env, host).await;
                    if value.is_err() {
                        return value;
                    }
                    values.push(value);
                }
                Value::List(values)
            }
            Expr::Struct(fields) => {
                let mut values = Vec::with_capacity(fields.len());
                for (name, expr) in fields {
                    let value = eval_expr(expr, env, host).await;
                    if value.is_err() {
                        return value;
                    }
                    values.push((name.name.clone(), value));
                }
                Value::Struct(values)
            }
            Expr::Lambda { params, body } => Value::Lambda {
                params: params.clone(),
                body: Arc::new((**body).clone()),
                captured_env: env.clone(),
            },
            Expr::FileRef(file) => {
                host.eval_external(ExpressionEffect::FileRef(&file.path), env)
                    .await
            }
            Expr::Node(node) => host.eval_external(ExpressionEffect::Node(node), env).await,
            Expr::Call { func, args } => {
                host.eval_external(ExpressionEffect::Call { func, args }, env)
                    .await
            }
            Expr::Pipe { lhs, rhs } => {
                let piped = eval_expr(lhs, env, host).await;
                if piped.is_err() {
                    return piped;
                }
                host.eval_pipe_rhs(rhs, piped, env).await
            }
            Expr::Annotated { expr, annotation } => match annotation_type_name(expr) {
                Some(type_name) => Value::Struct(vec![
                    ("type".into(), Value::Str(type_name)),
                    ("desc".into(), Value::Str(annotation.clone())),
                ]),
                None => eval_expr(expr, env, host).await,
            },
        }
    })
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };

    use super::*;
    use crate::{
        EvalError,
        ast::{BinOp, Ident, Literal, Node, Span},
    };

    struct TestHost;

    impl ExpressionHost for TestHost {
        type Payload = ();
        type Error = EvalError;

        fn undefined_var(&self, name: String) -> EvalError {
            EvalError::TypeMismatch {
                expected: "defined variable".into(),
                actual: name,
            }
        }

        fn undefined_field(&self, name: String) -> EvalError {
            EvalError::TypeMismatch {
                expected: "defined field".into(),
                actual: name,
            }
        }

        fn eval_pipe_rhs<'a>(
            &'a self,
            _rhs: &'a Expr,
            piped: Value<(), EvalError>,
            _env: &'a Env<Value<(), EvalError>>,
        ) -> HostFuture<'a, Value<(), EvalError>> {
            Box::pin(async move {
                match piped {
                    Value::Int(value) => Value::Int(value + 1),
                    _ => panic!("pipe left side must be evaluated before host dispatch"),
                }
            })
        }

        fn eval_external<'a>(
            &'a self,
            effect: ExpressionEffect<'a>,
            _env: &'a Env<Value<(), EvalError>>,
        ) -> HostFuture<'a, Value<(), EvalError>> {
            Box::pin(async move {
                match effect {
                    ExpressionEffect::Node(_) => Value::Int(5),
                    _ => panic!("unexpected external expression"),
                }
            })
        }
    }

    fn run_ready<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("test host must complete synchronously"),
        }
    }

    #[test]
    fn portable_expression_and_host_effect_share_one_evaluator() {
        let expr = Expr::Binary {
            op: BinOp::Add,
            left: Box::new(Expr::Ident(Ident::new("x", Span::default()))),
            right: Box::new(Expr::Node(Node::ToolCall {
                path: vec![],
                args: vec![],
            })),
        };
        let mut env = Env::new();
        env.bind("x", Value::<(), EvalError>::Int(7));
        assert!(matches!(
            run_ready(eval_expr(&expr, &env, &TestHost)),
            Value::Int(12)
        ));

        let missing = Expr::List(vec![
            Expr::Literal(Literal::Int(1)),
            Expr::Ident(Ident::new("missing", Span::default())),
        ]);
        assert!(matches!(
            run_ready(eval_expr(&missing, &env, &TestHost)),
            Value::Err(EvalError::TypeMismatch { actual, .. }) if actual == "missing"
        ));
    }

    #[test]
    fn annotations_build_portable_type_descriptors() {
        let env = Env::new();
        let list = Expr::Annotated {
            expr: Box::new(Expr::List(vec![Expr::Ident(Ident::new(
                "string",
                Span::default(),
            ))])),
            annotation: "items".into(),
        };
        let value = run_ready(eval_expr(&list, &env, &TestHost));
        let Value::Struct(fields) = value else {
            panic!("expected type descriptor")
        };
        assert!(
            matches!(&fields[0], (key, Value::Str(value)) if key == "type" && value == "list of string")
        );
        assert!(
            matches!(&fields[1], (key, Value::Str(value)) if key == "desc" && value == "items")
        );

        let ordinary = Expr::Annotated {
            expr: Box::new(Expr::Literal(Literal::Int(7))),
            annotation: "ignored".into(),
        };
        assert!(matches!(
            run_ready(eval_expr(&ordinary, &env, &TestHost)),
            Value::Int(7)
        ));
    }

    #[test]
    fn pipe_evaluates_left_once_and_stops_on_error() {
        let mut env = Env::new();
        env.bind("x", Value::<(), EvalError>::Int(7));
        let rhs = Box::new(Expr::Node(Node::ToolCall {
            path: vec![],
            args: vec![],
        }));
        let pipe = Expr::Pipe {
            lhs: Box::new(Expr::Ident(Ident::new("x", Span::default()))),
            rhs: rhs.clone(),
        };
        assert!(matches!(
            run_ready(eval_expr(&pipe, &env, &TestHost)),
            Value::Int(8)
        ));

        let missing = Expr::Pipe {
            lhs: Box::new(Expr::Ident(Ident::new("missing", Span::default()))),
            rhs,
        };
        assert!(matches!(
            run_ready(eval_expr(&missing, &env, &TestHost)),
            Value::Err(EvalError::TypeMismatch { actual, .. }) if actual == "missing"
        ));
    }
}
