use alloc::{boxed::Box, format, string::String, sync::Arc, vec, vec::Vec};

use crate::{
    Env, HostFuture, HostValueOps, Value, ValueError,
    ast::{Expr, FanoutCollect, Ident, Node},
    fanout::join_fanout_all,
    list::{ListIntrinsic, eval_list_intrinsic},
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
    fn cancellation_error(&self) -> Option<Self::Error> {
        None
    }
    fn fanout_branch_start(&self, _index: usize) {}
    fn eval_fanout_branch<'a>(
        &'a self,
        expr: &'a Expr,
        env: &'a Env<Value<Self::Payload, Self::Error>>,
        _index: usize,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>>
    where
        Self: Sized,
    {
        eval_expr(expr, env, self)
    }
    fn fanout_branch_end(&self, _index: usize, _value: &Value<Self::Payload, Self::Error>) {}
    fn unsupported_fanout_first(&self) -> Self::Error {
        Self::Error::type_mismatch("fanout collect: all", "first".into())
    }
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
            Expr::Node(node) => {
                if let Some(error) = host.cancellation_error() {
                    return Value::Err(error);
                }
                match node {
                    Node::DynamicFanout {
                        source,
                        lambda,
                        collect,
                    } => eval_dynamic_fanout(source, lambda, collect, env, host).await,
                    Node::Fanout { items, collect } => {
                        eval_static_fanout(items, collect, env, host).await
                    }
                    Node::ToolCall { path, args } => match ListIntrinsic::from_path(path) {
                        Some(intrinsic) => eval_list_intrinsic(intrinsic, args, env, host).await,
                        None => host.eval_external(ExpressionEffect::Node(node), env).await,
                    },
                    _ => host.eval_external(ExpressionEffect::Node(node), env).await,
                }
            }
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

/// Evaluates static fanout while the host supplies branch context and events.
pub async fn eval_static_fanout<'a, H: ExpressionHost>(
    items: &'a [Expr],
    collect: &'a FanoutCollect,
    env: &'a Env<Value<H::Payload, H::Error>>,
    host: &'a H,
) -> Value<H::Payload, H::Error> {
    if matches!(collect, FanoutCollect::First) {
        return Value::Err(host.unsupported_fanout_first());
    }
    for index in 0..items.len() {
        host.fanout_branch_start(index);
    }
    let branches = items
        .iter()
        .enumerate()
        .map(|(index, expr)| host.eval_fanout_branch(expr, env, index));
    join_fanout_all(branches, |index, value| {
        host.fanout_branch_end(index, value)
    })
    .await
}

/// Evaluates a dynamic fanout through the shared expression engine.
pub async fn eval_dynamic_fanout<'a, H: ExpressionHost>(
    source: &'a Expr,
    lambda: &'a Expr,
    collect: &'a FanoutCollect,
    env: &'a Env<Value<H::Payload, H::Error>>,
    host: &'a H,
) -> Value<H::Payload, H::Error> {
    let list_val = eval_expr(source, env, host).await;
    let Value::List(items) = list_val else {
        return Value::Err(H::Error::type_mismatch("list", list_val.kind_name().into()));
    };

    let lambda_val = eval_expr(lambda, env, host).await;
    let Value::Lambda {
        params,
        body,
        captured_env,
    } = lambda_val
    else {
        return Value::Err(H::Error::type_mismatch(
            "lambda",
            lambda_val.kind_name().into(),
        ));
    };

    let mut results = Vec::new();
    for item in items {
        let mut call_env = captured_env.child();
        if let Some(param) = params.first() {
            call_env.bind(param.name.clone(), item);
        }
        let result = eval_expr(&body, &call_env, host).await;
        if let Value::Err(error) = &result {
            return Value::Err(error.clone());
        }
        if matches!(collect, FanoutCollect::First) {
            return result;
        }
        results.push(result);
    }

    match collect {
        FanoutCollect::All => Value::List(results),
        FanoutCollect::First => results.into_iter().next().unwrap_or(Value::Unit),
    }
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

    struct TestHost {
        cancelled: bool,
    }

    const ACTIVE_HOST: TestHost = TestHost { cancelled: false };
    const CANCELLED_HOST: TestHost = TestHost { cancelled: true };

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

        fn cancellation_error(&self) -> Option<EvalError> {
            self.cancelled.then(|| EvalError::TypeMismatch {
                expected: "active flow".into(),
                actual: "cancelled".into(),
            })
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
            run_ready(eval_expr(&expr, &env, &ACTIVE_HOST)),
            Value::Int(12)
        ));

        let missing = Expr::List(vec![
            Expr::Literal(Literal::Int(1)),
            Expr::Ident(Ident::new("missing", Span::default())),
        ]);
        assert!(matches!(
            run_ready(eval_expr(&missing, &env, &ACTIVE_HOST)),
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
        let value = run_ready(eval_expr(&list, &env, &ACTIVE_HOST));
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
            run_ready(eval_expr(&ordinary, &env, &ACTIVE_HOST)),
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
            run_ready(eval_expr(&pipe, &env, &ACTIVE_HOST)),
            Value::Int(8)
        ));

        let missing = Expr::Pipe {
            lhs: Box::new(Expr::Ident(Ident::new("missing", Span::default()))),
            rhs,
        };
        assert!(matches!(
            run_ready(eval_expr(&missing, &env, &ACTIVE_HOST)),
            Value::Err(EvalError::TypeMismatch { actual, .. }) if actual == "missing"
        ));
    }

    #[test]
    fn dynamic_fanout_evaluates_lambda_with_captured_environment() {
        let source = Expr::List(vec![
            Expr::Literal(Literal::Int(1)),
            Expr::Literal(Literal::Int(2)),
        ]);
        let lambda = Expr::Lambda {
            params: vec![Ident::new("item", Span::default())],
            body: Box::new(Expr::Binary {
                op: BinOp::Add,
                left: Box::new(Expr::Ident(Ident::new("item", Span::default()))),
                right: Box::new(Expr::Ident(Ident::new("offset", Span::default()))),
            }),
        };
        let mut env = Env::new();
        env.bind("offset", Value::<(), EvalError>::Int(10));

        let all = run_ready(eval_dynamic_fanout(
            &source,
            &lambda,
            &FanoutCollect::All,
            &env,
            &ACTIVE_HOST,
        ));
        assert!(
            matches!(all, Value::List(items) if matches!(&items[..], [Value::Int(11), Value::Int(12)]))
        );
        assert!(matches!(
            run_ready(eval_dynamic_fanout(
                &source,
                &lambda,
                &FanoutCollect::First,
                &env,
                &ACTIVE_HOST,
            )),
            Value::Int(11)
        ));
    }

    #[test]
    fn static_fanout_dispatches_without_external_node_handling() {
        let expr = Expr::Node(Node::Fanout {
            items: vec![
                Expr::Literal(Literal::Int(1)),
                Expr::Node(Node::ToolCall {
                    path: vec![],
                    args: vec![],
                }),
            ],
            collect: FanoutCollect::All,
        });
        assert!(matches!(
            run_ready(eval_expr(&expr, &Env::new(), &ACTIVE_HOST)),
            Value::List(values) if matches!(&values[..], [Value::Int(1), Value::Int(5)])
        ));
    }

    #[test]
    fn cancellation_precedes_portable_and_external_nodes() {
        let list = Expr::Node(Node::ToolCall {
            path: vec![
                Ident::new("list", Span::default()),
                Ident::new("map", Span::default()),
            ],
            args: vec![],
        });
        let external = Expr::Node(Node::ToolCall {
            path: vec![],
            args: vec![],
        });
        for expr in [&list, &external] {
            assert!(matches!(
                run_ready(eval_expr(expr, &Env::new(), &CANCELLED_HOST)),
                Value::Err(EvalError::TypeMismatch { actual, .. }) if actual == "cancelled"
            ));
        }
    }
}
