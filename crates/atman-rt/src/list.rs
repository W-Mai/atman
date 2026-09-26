use alloc::vec::Vec;

use crate::{
    Env, ExpressionHost, Value, ValueError,
    ast::{Arg, Expr},
    eval_expr,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListIntrinsic {
    Map,
    Filter,
    Reduce,
    Find,
    Any,
    All,
}

impl ListIntrinsic {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "list.map" => Self::Map,
            "list.filter" => Self::Filter,
            "list.reduce" => Self::Reduce,
            "list.find" => Self::Find,
            "list.any" => Self::Any,
            "list.all" => Self::All,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Map => "list.map",
            Self::Filter => "list.filter",
            Self::Reduce => "list.reduce",
            Self::Find => "list.find",
            Self::Any => "list.any",
            Self::All => "list.all",
        }
    }
}

fn positional<'a, E: ValueError>(args: &'a [Arg], index: usize, name: &str) -> Result<&'a Expr, E> {
    let mut position = 0;
    for arg in args {
        if let Arg::Positional(expr) = arg {
            if position == index {
                return Ok(expr);
            }
            position += 1;
        }
    }
    Err(E::missing_positional_argument(name, index))
}

/// Evaluates a list intrinsic while delegating external lambda effects to the host.
pub async fn eval_list_intrinsic<'a, H: ExpressionHost>(
    intrinsic: ListIntrinsic,
    args: &'a [Arg],
    env: &'a Env<Value<H::Payload, H::Error>>,
    host: &'a H,
) -> Value<H::Payload, H::Error> {
    let name = intrinsic.name();
    let list_expr = match positional::<H::Error>(args, 0, name) {
        Ok(expr) => expr,
        Err(error) => return Value::Err(error),
    };
    let lambda_expr = match positional::<H::Error>(args, 1, name) {
        Ok(expr) => expr,
        Err(error) => return Value::Err(error),
    };
    let init_expr = if intrinsic == ListIntrinsic::Reduce {
        match positional::<H::Error>(args, 2, name) {
            Ok(expr) => Some(expr),
            Err(error) => return Value::Err(error),
        }
    } else {
        None
    };

    let list_value = eval_expr(list_expr, env, host).await;
    // The non-reduce intrinsics evaluate both expressions before validating the list.
    let lambda_value = if intrinsic == ListIntrinsic::Reduce {
        None
    } else {
        Some(eval_expr(lambda_expr, env, host).await)
    };
    let Value::List(items) = list_value else {
        return Value::Err(H::Error::type_mismatch(
            "list",
            list_value.kind_name().into(),
        ));
    };
    let lambda_value = match lambda_value {
        Some(value) => value,
        None => eval_expr(lambda_expr, env, host).await,
    };
    let Value::Lambda {
        params,
        body,
        captured_env,
    } = lambda_value
    else {
        return Value::Err(H::Error::type_mismatch(
            "lambda",
            lambda_value.kind_name().into(),
        ));
    };

    let expected = if intrinsic == ListIntrinsic::Reduce {
        "2 parameters (acc, x)"
    } else {
        "1 parameter"
    };
    let arity = if intrinsic == ListIntrinsic::Reduce {
        2
    } else {
        1
    };
    if params.len() != arity {
        return Value::Err(H::Error::invalid_lambda_arity(name, expected, params.len()));
    }

    if intrinsic == ListIntrinsic::Reduce {
        let mut acc = eval_expr(
            init_expr.expect("reduce requires an initial expression"),
            env,
            host,
        )
        .await;
        if acc.is_err() {
            return acc;
        }
        for item in items {
            let mut call_env = captured_env.child();
            call_env.bind(params[0].name.clone(), acc.clone());
            call_env.bind(params[1].name.clone(), item);
            acc = eval_expr(&body, &call_env, host).await;
            if acc.is_err() {
                return acc;
            }
        }
        return acc;
    }

    let mut results = if intrinsic == ListIntrinsic::Map {
        Vec::with_capacity(items.len())
    } else {
        Vec::new()
    };
    for item in items {
        let mut call_env = captured_env.child();
        let retained_item =
            matches!(intrinsic, ListIntrinsic::Filter | ListIntrinsic::Find).then(|| item.clone());
        call_env.bind(params[0].name.clone(), item);
        let value = eval_expr(&body, &call_env, host).await;
        match intrinsic {
            ListIntrinsic::Map => results.push(value),
            ListIntrinsic::Filter => {
                if matches!(value, Value::Bool(true)) {
                    results.push(retained_item.expect("filter retains each source item"));
                }
            }
            ListIntrinsic::Find => {
                if matches!(value, Value::Bool(true)) {
                    return retained_item.expect("find retains each source item");
                }
            }
            ListIntrinsic::Any => {
                if matches!(value, Value::Bool(true)) {
                    return Value::Bool(true);
                }
            }
            ListIntrinsic::All => {
                if !matches!(value, Value::Bool(true)) {
                    return Value::Bool(false);
                }
            }
            ListIntrinsic::Reduce => unreachable!(),
        }
    }

    match intrinsic {
        ListIntrinsic::Map | ListIntrinsic::Filter => Value::List(results),
        ListIntrinsic::Find => Value::Unit,
        ListIntrinsic::Any => Value::Bool(false),
        ListIntrinsic::All => Value::Bool(true),
        ListIntrinsic::Reduce => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use alloc::{boxed::Box, string::String, vec};
    use core::{
        future::Future,
        pin::pin,
        sync::atomic::{AtomicUsize, Ordering},
        task::{Context, Poll, Waker},
    };

    use super::*;
    use crate::{
        EvalError, ExpressionEffect, HostFuture,
        ast::{BinOp, Ident, Literal, Node, Span},
    };

    #[derive(Default)]
    struct TestHost {
        effects: AtomicUsize,
    }

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
            _piped: Value<(), EvalError>,
            _env: &'a Env<Value<(), EvalError>>,
        ) -> HostFuture<'a, Value<(), EvalError>> {
            Box::pin(async { unreachable!() })
        }

        fn eval_external<'a>(
            &'a self,
            _effect: ExpressionEffect<'a>,
            _env: &'a Env<Value<(), EvalError>>,
        ) -> HostFuture<'a, Value<(), EvalError>> {
            self.effects.fetch_add(1, Ordering::SeqCst);
            Box::pin(async { Value::Unit })
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
    fn list_map_uses_captured_environment_without_atman_services() {
        let args = vec![
            Arg::Positional(Expr::List(vec![
                Expr::Literal(Literal::Int(1)),
                Expr::Literal(Literal::Int(2)),
            ])),
            Arg::Positional(Expr::Lambda {
                params: vec![Ident::new("item", Span::default())],
                body: Box::new(Expr::Binary {
                    op: BinOp::Add,
                    left: Box::new(Expr::Ident(Ident::new("item", Span::default()))),
                    right: Box::new(Expr::Ident(Ident::new("offset", Span::default()))),
                }),
            }),
        ];
        let mut env = Env::new();
        env.bind("offset", Value::<(), EvalError>::Int(10));
        let result = run_ready(eval_list_intrinsic(
            ListIntrinsic::Map,
            &args,
            &env,
            &TestHost::default(),
        ));
        assert!(
            matches!(result, Value::List(items) if matches!(&items[..], [Value::Int(11), Value::Int(12)]))
        );
    }

    #[test]
    fn reduce_validates_list_before_evaluating_lambda() {
        let host = TestHost::default();
        let mut args = vec![
            Arg::Positional(Expr::Literal(Literal::Int(1))),
            Arg::Positional(Expr::Node(Node::ToolCall {
                path: vec![],
                args: vec![],
            })),
        ];
        let env = Env::new();
        assert!(matches!(
            run_ready(eval_list_intrinsic(ListIntrinsic::Map, &args, &env, &host)),
            Value::Err(EvalError::TypeMismatch { expected, actual }) if expected == "list" && actual == "int"
        ));
        assert_eq!(host.effects.load(Ordering::SeqCst), 1);

        args.push(Arg::Positional(Expr::Literal(Literal::Int(0))));
        assert!(matches!(
            run_ready(eval_list_intrinsic(ListIntrinsic::Reduce, &args, &env, &host)),
            Value::Err(EvalError::TypeMismatch { expected, actual }) if expected == "list" && actual == "int"
        ));
        assert_eq!(host.effects.load(Ordering::SeqCst), 1);
    }
}
