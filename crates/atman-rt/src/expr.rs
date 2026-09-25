use alloc::{boxed::Box, format, string::String, sync::Arc, vec::Vec};

use crate::{
    Env, HostFuture, HostValueOps, Value, ValueError,
    ast::{Expr, Ident, Node},
    ops::{eval_binary, eval_literal, eval_unary},
};

pub enum ExpressionEffect<'a> {
    FileRef(&'a str),
    Node(&'a Node),
    Call { func: &'a Ident, args: &'a [Expr] },
    Pipe { lhs: &'a Expr, rhs: &'a Expr },
    Annotated { expr: &'a Expr, annotation: &'a str },
}

/// Supplies external expressions and product-specific error messages.
pub trait ExpressionHost: Sync {
    type Payload: HostValueOps + Clone + Send + Sync;
    type Error: ValueError + Clone + Send + Sync;

    fn undefined_var(&self, name: String) -> Self::Error;
    fn undefined_field(&self, name: String) -> Self::Error;
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
                host.eval_external(ExpressionEffect::Pipe { lhs, rhs }, env)
                    .await
            }
            Expr::Annotated { expr, annotation } => {
                host.eval_external(ExpressionEffect::Annotated { expr, annotation }, env)
                    .await
            }
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
}
