use alloc::vec::Vec;

use crate::{
    Env, ExpressionHost, HostValueOps, Value, ValueError,
    ast::{Arg, Expr, Ident},
    eval_expr,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ListIntrinsic {
    Len,
    LenOrString,
    IsEmpty,
    IsEmptyOrString,
    Get,
    First,
    Last,
    Tail,
    Concat,
    Map,
    Filter,
    Reduce,
    Find,
    Any,
    All,
}

impl ListIntrinsic {
    pub(crate) fn from_path(path: &[Ident]) -> Option<Self> {
        match path {
            [name] => Self::from_legacy_name(&name.name),
            [namespace, name] if namespace.name == "list" => Self::from_member(&name.name),
            _ => None,
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        name.strip_prefix("list.")
            .and_then(Self::from_member)
            .or_else(|| Self::from_legacy_name(name))
    }

    fn from_legacy_name(name: &str) -> Option<Self> {
        Some(match name {
            "len" => Self::LenOrString,
            "is_empty" => Self::IsEmptyOrString,
            "head" => Self::First,
            "tail" => Self::Tail,
            "concat" => Self::Concat,
            _ => return None,
        })
    }

    fn from_member(name: &str) -> Option<Self> {
        Some(match name {
            "len" => Self::Len,
            "is_empty" => Self::IsEmpty,
            "get" => Self::Get,
            "first" => Self::First,
            "last" => Self::Last,
            "tail" => Self::Tail,
            "concat" => Self::Concat,
            "map" => Self::Map,
            "filter" => Self::Filter,
            "reduce" => Self::Reduce,
            "find" => Self::Find,
            "any" => Self::Any,
            "all" => Self::All,
            _ => return None,
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Len => "list.len",
            Self::LenOrString => "len",
            Self::IsEmpty => "list.is_empty",
            Self::IsEmptyOrString => "is_empty",
            Self::Get => "list.get",
            Self::First => "list.first",
            Self::Last => "list.last",
            Self::Tail => "list.tail",
            Self::Concat => "list.concat",
            Self::Map => "list.map",
            Self::Filter => "list.filter",
            Self::Reduce => "list.reduce",
            Self::Find => "list.find",
            Self::Any => "list.any",
            Self::All => "list.all",
        }
    }

    fn is_basic(self) -> bool {
        matches!(
            self,
            Self::Len
                | Self::LenOrString
                | Self::IsEmpty
                | Self::IsEmptyOrString
                | Self::Get
                | Self::First
                | Self::Last
                | Self::Tail
                | Self::Concat
        )
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

fn argument<'a, E: ValueError>(
    args: &'a [Arg],
    index: usize,
    parameter: &str,
    name: &str,
) -> Result<&'a Expr, E> {
    args.iter()
        .find_map(|arg| match arg {
            Arg::Named { name, value } if name.name == parameter => Some(value),
            _ => None,
        })
        .map(Ok)
        .unwrap_or_else(|| positional(args, index, name))
}

pub(crate) fn eval_list_index<P: HostValueOps + Clone, E: ValueError + Clone>(
    base: Value<P, E>,
    index: Value<P, E>,
) -> Value<P, E> {
    let Value::List(items) = base else {
        return Value::Err(E::type_mismatch("list", base.kind_name().into()));
    };
    let Value::Int(index) = index else {
        return Value::Err(E::type_mismatch("int", index.kind_name().into()));
    };
    usize::try_from(index)
        .ok()
        .and_then(|index| items.get(index))
        .cloned()
        .unwrap_or_else(|| Value::Err(E::index_out_of_bounds(index, items.len())))
}

/// Evaluates a list intrinsic while delegating external lambda effects to the host.
pub async fn eval_list_intrinsic<'a, H: ExpressionHost>(
    intrinsic: ListIntrinsic,
    args: &'a [Arg],
    env: &'a Env<Value<H::Payload, H::Error>>,
    host: &'a H,
) -> Value<H::Payload, H::Error> {
    eval_list_intrinsic_named(intrinsic, intrinsic.name(), args, env, host).await
}

pub(crate) async fn eval_list_intrinsic_named<'a, H: ExpressionHost>(
    intrinsic: ListIntrinsic,
    name: &str,
    args: &'a [Arg],
    env: &'a Env<Value<H::Payload, H::Error>>,
    host: &'a H,
) -> Value<H::Payload, H::Error> {
    if intrinsic.is_basic() {
        return eval_basic_list_intrinsic(intrinsic, name, args, env, host).await;
    }

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
            ListIntrinsic::Len
            | ListIntrinsic::LenOrString
            | ListIntrinsic::IsEmpty
            | ListIntrinsic::IsEmptyOrString
            | ListIntrinsic::Get
            | ListIntrinsic::First
            | ListIntrinsic::Last
            | ListIntrinsic::Tail
            | ListIntrinsic::Concat
            | ListIntrinsic::Reduce => unreachable!(),
        }
    }

    match intrinsic {
        ListIntrinsic::Map | ListIntrinsic::Filter => Value::List(results),
        ListIntrinsic::Find => Value::Unit,
        ListIntrinsic::Any => Value::Bool(false),
        ListIntrinsic::All => Value::Bool(true),
        ListIntrinsic::Len
        | ListIntrinsic::LenOrString
        | ListIntrinsic::IsEmpty
        | ListIntrinsic::IsEmptyOrString
        | ListIntrinsic::Get
        | ListIntrinsic::First
        | ListIntrinsic::Last
        | ListIntrinsic::Tail
        | ListIntrinsic::Concat
        | ListIntrinsic::Reduce => unreachable!(),
    }
}

async fn eval_basic_list_intrinsic<'a, H: ExpressionHost>(
    intrinsic: ListIntrinsic,
    name: &str,
    args: &'a [Arg],
    env: &'a Env<Value<H::Payload, H::Error>>,
    host: &'a H,
) -> Value<H::Payload, H::Error> {
    let first_parameter = if intrinsic == ListIntrinsic::Concat {
        "left"
    } else {
        "items"
    };
    let first_expr = match argument::<H::Error>(args, 0, first_parameter, name) {
        Ok(expr) => expr,
        Err(error) => return Value::Err(error),
    };
    let first = eval_expr(first_expr, env, host).await;
    if first.is_err() {
        return first;
    }

    match intrinsic {
        ListIntrinsic::Len => match first {
            Value::List(items) => Value::Int(items.len() as i64),
            other => Value::Err(H::Error::type_mismatch("list", other.kind_name().into())),
        },
        ListIntrinsic::LenOrString => match first {
            Value::List(items) => Value::Int(items.len() as i64),
            Value::Str(value) => Value::Int(value.chars().count() as i64),
            other => Value::Err(H::Error::type_mismatch(
                "list or string",
                other.kind_name().into(),
            )),
        },
        ListIntrinsic::IsEmpty => match first {
            Value::List(items) => Value::Bool(items.is_empty()),
            other => Value::Err(H::Error::type_mismatch("list", other.kind_name().into())),
        },
        ListIntrinsic::IsEmptyOrString => match first {
            Value::List(items) => Value::Bool(items.is_empty()),
            Value::Str(value) => Value::Bool(value.is_empty()),
            other => Value::Err(H::Error::type_mismatch(
                "list or string",
                other.kind_name().into(),
            )),
        },
        ListIntrinsic::Get => {
            let index_expr = match argument::<H::Error>(args, 1, "index", name) {
                Ok(expr) => expr,
                Err(error) => return Value::Err(error),
            };
            let index = eval_expr(index_expr, env, host).await;
            if index.is_err() {
                return index;
            }
            eval_list_index(first, index)
        }
        ListIntrinsic::First => match first {
            Value::List(items) => items
                .first()
                .cloned()
                .unwrap_or_else(|| Value::Err(H::Error::empty_list(name))),
            other => Value::Err(H::Error::type_mismatch("list", other.kind_name().into())),
        },
        ListIntrinsic::Last => match first {
            Value::List(items) => items
                .last()
                .cloned()
                .unwrap_or_else(|| Value::Err(H::Error::empty_list(name))),
            other => Value::Err(H::Error::type_mismatch("list", other.kind_name().into())),
        },
        ListIntrinsic::Tail => match first {
            Value::List(items) if !items.is_empty() => Value::List(items[1..].to_vec()),
            Value::List(_) => Value::Err(H::Error::empty_list(name)),
            other => Value::Err(H::Error::type_mismatch("list", other.kind_name().into())),
        },
        ListIntrinsic::Concat => {
            let second_expr = match argument::<H::Error>(args, 1, "right", name) {
                Ok(expr) => expr,
                Err(error) => return Value::Err(error),
            };
            let second = eval_expr(second_expr, env, host).await;
            if second.is_err() {
                return second;
            }
            let Value::List(mut left) = first else {
                return Value::Err(H::Error::type_mismatch("list", first.kind_name().into()));
            };
            let Value::List(right) = second else {
                return Value::Err(H::Error::type_mismatch("list", second.kind_name().into()));
            };
            left.extend(right);
            Value::List(left)
        }
        ListIntrinsic::Map
        | ListIntrinsic::Filter
        | ListIntrinsic::Reduce
        | ListIntrinsic::Find
        | ListIntrinsic::Any
        | ListIntrinsic::All => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use alloc::{boxed::Box, string::String, sync::Arc, vec};
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

    #[derive(Clone, Default)]
    struct TestHost {
        effects: Arc<AtomicUsize>,
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

        fn eval_external<'a>(
            &'a self,
            _effect: ExpressionEffect<(), EvalError>,
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
