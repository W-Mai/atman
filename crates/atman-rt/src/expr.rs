use alloc::{boxed::Box, collections::BTreeSet, format, string::String, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicU8, Ordering};

use crate::{
    Env, FlowDriveMode, FlowFuture, HostFuture, HostValueOps, Value, ValueError,
    ast::{Arg, Expr, FlowRef, MessageRole, Node},
    fanout::join_fanout_all,
    list::{ListIntrinsic, eval_list_intrinsic},
    ops::{eval_binary, eval_literal, eval_unary},
    watch::WatchRules,
};

impl<P: HostValueOps, E> ExpressionEffect<P, E> {
    pub fn contains_flow_future(&self) -> bool {
        match self {
            Self::FileRef(_) => false,
            Self::ToolCall {
                positional, named, ..
            }
            | Self::Message {
                positional, named, ..
            } => {
                positional.iter().any(Value::contains_flow_future)
                    || named.iter().any(|(_, value)| value.contains_flow_future())
            }
            Self::Confirm(value) | Self::FixSnapshot { target: value } => {
                value.contains_flow_future()
            }
            Self::Call { args, .. } => args.iter().any(Value::contains_flow_future),
            Self::FixRestore { target, .. } => target.contains_flow_future(),
        }
    }
}

/// An external effect whose language expressions have already been evaluated.
pub enum ExpressionEffect<P, E> {
    FileRef(String),
    ToolCall {
        name: String,
        positional: Vec<Value<P, E>>,
        named: Vec<(String, Value<P, E>)>,
        watch_rules: Option<WatchRules>,
    },
    Confirm(Value<P, E>),
    Message {
        role: MessageRole,
        positional: Vec<Value<P, E>>,
        named: Vec<(String, Value<P, E>)>,
    },
    Call {
        name: String,
        args: Vec<Value<P, E>>,
    },
    FixSnapshot {
        target: Value<P, E>,
    },
    FixRestore {
        target: Value<P, E>,
        pristine: String,
    },
}

pub enum EvaluatedArg<P, E> {
    Positional(Value<P, E>),
    Named(String, Value<P, E>),
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
pub trait ExpressionHost: Sync + Clone + Send {
    type Payload: HostValueOps + Clone + Send + Sync;
    type Error: ValueError + Clone + Send + Sync;

    fn undefined_var(&self, name: String) -> Self::Error;
    fn undefined_field(&self, name: String) -> Self::Error;
    fn cancellation_error(&self) -> Option<Self::Error> {
        None
    }
    fn await_drive_mode(&self) -> FlowDriveMode {
        FlowDriveMode::Inline
    }
    fn fanout_branch_start(&self, _index: usize) {}
    fn branch_host(&self, _index: usize) -> Self {
        self.clone()
    }
    fn fanout_branch_end(&self, _index: usize, _value: &Value<Self::Payload, Self::Error>) {}
    /// Return a value to reject or short-circuit a tool before its arguments run.
    fn preflight_tool(&self, _name: &str) -> Option<Value<Self::Payload, Self::Error>> {
        None
    }
    /// Check a resolved flow call before its arguments run.
    fn preflight_flow(&self, _name: &FlowRef, _args: &[Arg]) -> Result<(), Self::Error> {
        Ok(())
    }
    fn make_flow_future(
        &self,
        name: FlowRef,
        _args: Vec<EvaluatedArg<Self::Payload, Self::Error>>,
    ) -> Value<Self::Payload, Self::Error> {
        Value::Err(Self::Error::type_mismatch(
            "linked flow",
            name.display_name(),
        ))
    }
    fn validate_flow_future(
        &self,
        _future: &FlowFuture<Self::Payload, Self::Error>,
    ) -> Result<(), Self::Error> {
        Err(Self::Error::type_mismatch(
            "flow future from current invocation",
            "foreign future".into(),
        ))
    }
    fn drive_flow_future<'a>(
        &'a self,
        _future: &'a FlowFuture<Self::Payload, Self::Error>,
        _mode: FlowDriveMode,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>> {
        Box::pin(async {
            Value::Err(Self::Error::type_mismatch(
                "linked flow future",
                "unavailable".into(),
            ))
        })
    }
    fn eval_external<'a>(
        &'a self,
        effect: ExpressionEffect<Self::Payload, Self::Error>,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>>;
}

/// Evaluates portable expressions and delegates external effects to the host.
pub fn eval_expr<'a, H: ExpressionHost>(
    expr: &'a Expr,
    env: &'a Env<Value<H::Payload, H::Error>>,
    host: &'a H,
) -> HostFuture<'a, Value<H::Payload, H::Error>> {
    eval_expr_with_watch(expr, env, host, None)
}

/// Evaluates a binding expression with its compiled stream watch rules.
pub fn eval_expr_with_watch<'a, H: ExpressionHost>(
    expr: &'a Expr,
    env: &'a Env<Value<H::Payload, H::Error>>,
    host: &'a H,
    watch_rules: Option<&'a WatchRules>,
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
            Expr::Await { value } => {
                let value = eval_expr(value, env, host).await;
                match value {
                    Value::FlowFuture(future) => {
                        if let Err(error) = host.validate_flow_future(&future) {
                            return Value::Err(error);
                        }
                        host.drive_flow_future(&future, host.await_drive_mode())
                            .await
                    }
                    Value::Err(_) => value,
                    other => Value::Err(H::Error::type_mismatch(
                        "flow future",
                        other.kind_name().into(),
                    )),
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
                host.eval_external(ExpressionEffect::FileRef(file.path.clone()))
                    .await
            }
            Expr::Node(node) => {
                if let Some(error) = host.cancellation_error() {
                    return Value::Err(error);
                }
                match node {
                    Node::DynamicFanout { source, lambda } => {
                        eval_dynamic_fanout(source, lambda, env, host).await
                    }
                    Node::Fanout { source } => eval_fanout(source, env, host).await,
                    Node::ToolCall { path, args } => match ListIntrinsic::from_path(path) {
                        Some(intrinsic) => eval_list_intrinsic(intrinsic, args, env, host).await,
                        None => {
                            let name = path
                                .iter()
                                .map(|id| id.name.as_str())
                                .collect::<Vec<_>>()
                                .join(".");
                            if let Some(value) = host.preflight_tool(&name) {
                                return value;
                            }
                            let (positional, named) = match eval_args(args, env, host).await {
                                Ok(values) => values,
                                Err(value) => return value,
                            };
                            host.eval_external(ExpressionEffect::ToolCall {
                                name,
                                positional,
                                named,
                                watch_rules: watch_rules.cloned(),
                            })
                            .await
                        }
                    },
                    Node::UserConfirm { msg } => {
                        let value = eval_expr(msg, env, host).await;
                        if value.is_err() {
                            return value;
                        }
                        host.eval_external(ExpressionEffect::Confirm(value)).await
                    }
                    Node::Message { role, args } => {
                        let (positional, named) = match eval_message_args(args, env, host).await {
                            Ok(values) => values,
                            Err(value) => return value,
                        };
                        host.eval_external(ExpressionEffect::Message {
                            role: *role,
                            positional,
                            named,
                        })
                        .await
                    }
                    Node::FlowCall { name, args } => {
                        if let Err(error) = host.preflight_flow(name, args) {
                            return Value::Err(error);
                        }
                        let args = match eval_ordered_args(args, env, host).await {
                            Ok(values) => values,
                            Err(value) => return value,
                        };
                        host.make_flow_future(name.clone(), args)
                    }
                    Node::FixUntilTestPasses { kwargs } => {
                        crate::fix::eval_fix_until_test_passes(kwargs, env, host).await
                    }
                }
            }
            Expr::Call { func, args } => {
                let mut values = Vec::with_capacity(args.len());
                for arg in args {
                    let value = eval_expr(arg, env, host).await;
                    if value.is_err() {
                        return value;
                    }
                    values.push(value);
                }
                host.eval_external(ExpressionEffect::Call {
                    name: func.name.clone(),
                    args: values,
                })
                .await
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

pub async fn eval_args<'a, H: ExpressionHost>(
    args: &'a [Arg],
    env: &'a Env<Value<H::Payload, H::Error>>,
    host: &'a H,
) -> Result<
    (
        Vec<Value<H::Payload, H::Error>>,
        Vec<(String, Value<H::Payload, H::Error>)>,
    ),
    Value<H::Payload, H::Error>,
> {
    let ordered = eval_ordered_args(args, env, host).await?;
    let mut positional = Vec::new();
    let mut named = Vec::new();
    for arg in ordered {
        match arg {
            EvaluatedArg::Positional(value) => {
                positional.push(value);
            }
            EvaluatedArg::Named(name, value) => {
                named.push((name, value));
            }
        }
    }
    Ok((positional, named))
}

pub async fn eval_ordered_args<'a, H: ExpressionHost>(
    args: &'a [Arg],
    env: &'a Env<Value<H::Payload, H::Error>>,
    host: &'a H,
) -> Result<Vec<EvaluatedArg<H::Payload, H::Error>>, Value<H::Payload, H::Error>> {
    let mut values = Vec::with_capacity(args.len());
    for arg in args {
        match arg {
            Arg::Positional(expr) => {
                let value = eval_expr(expr, env, host).await;
                if value.is_err() {
                    return Err(value);
                }
                values.push(EvaluatedArg::Positional(value));
            }
            Arg::Named { name, value } => {
                let value = eval_expr(value, env, host).await;
                if value.is_err() {
                    return Err(value);
                }
                values.push(EvaluatedArg::Named(name.name.clone(), value));
            }
        }
    }
    Ok(values)
}

async fn eval_message_args<'a, H: ExpressionHost>(
    args: &'a [Arg],
    env: &'a Env<Value<H::Payload, H::Error>>,
    host: &'a H,
) -> Result<
    (
        Vec<Value<H::Payload, H::Error>>,
        Vec<(String, Value<H::Payload, H::Error>)>,
    ),
    Value<H::Payload, H::Error>,
> {
    let mut positional = Vec::new();
    let mut named = Vec::new();
    for arg in args {
        match arg {
            Arg::Positional(expr) => {
                let value = eval_expr(expr, env, host).await;
                if value.is_err() {
                    return Err(value);
                }
                positional.push(value);
            }
            Arg::Named { name, value } if name.name == "attachments" => {
                if let Expr::List(items) = value
                    && items.iter().all(|item| matches!(item, Expr::FileRef(_)))
                {
                    let paths = items
                        .iter()
                        .map(|item| match item {
                            Expr::FileRef(file) => Value::Str(file.path.clone()),
                            _ => unreachable!(),
                        })
                        .collect();
                    named.push((name.name.clone(), Value::List(paths)));
                    continue;
                }
                let value = eval_expr(value, env, host).await;
                if value.is_err() {
                    return Err(value);
                }
                named.push((name.name.clone(), value));
            }
            Arg::Named { name, value } => {
                let value = eval_expr(value, env, host).await;
                if value.is_err() {
                    return Err(value);
                }
                named.push((name.name.clone(), value));
            }
        }
    }
    Ok((positional, named))
}

/// Evaluates a list source, polling literal branches concurrently.
pub async fn eval_fanout<'a, H: ExpressionHost>(
    source: &'a Expr,
    env: &'a Env<Value<H::Payload, H::Error>>,
    host: &'a H,
) -> Value<H::Payload, H::Error> {
    if let Expr::List(items) = source {
        let mut scope = FanoutBranchScope::start(host, items.len());
        let states = scope.states();
        let branches = items.iter().enumerate().map(|(index, expr)| {
            let branch_host = host.branch_host(index);
            let states = Arc::clone(&states);
            async move {
                let value = eval_expr(expr, env, &branch_host).await;
                record_prepared_branch(&states, index, &value);
                (branch_host, value)
            }
        });
        let prepared = futures::future::join_all(branches).await;
        let mut unique = BTreeSet::new();
        for (branch_host, value) in &prepared {
            if let Err(error) = validate_fanout_value(value, branch_host, &mut unique) {
                let rejected = Value::Err(error.clone());
                for (index, (_, value)) in prepared.iter().enumerate() {
                    let terminal = if matches!(value, Value::FlowFuture(_)) {
                        &rejected
                    } else {
                        value
                    };
                    scope.end(index, terminal);
                }
                return Value::Err(error);
            }
        }
        let states = scope.states();
        return join_fanout_all(
            prepared
                .into_iter()
                .enumerate()
                .map(|(index, (branch_host, value))| {
                    let states = Arc::clone(&states);
                    async move {
                        let value = drive_fanout_value(value, &branch_host).await;
                        record_finished_branch(&states, index, &value);
                        value
                    }
                }),
            |index, value| scope.end(index, value),
        )
        .await;
    }

    let values = match eval_expr(source, env, host).await {
        Value::List(values) => values,
        error @ Value::Err(_) => return error,
        other => return Value::Err(H::Error::type_mismatch("list", other.kind_name().into())),
    };
    let mut unique = BTreeSet::new();
    for value in &values {
        if let Err(error) = validate_fanout_value(value, host, &mut unique) {
            return Value::Err(error);
        }
    }
    let mut scope = FanoutBranchScope::start(host, values.len());
    let states = scope.states();
    for (index, value) in values.iter().enumerate() {
        record_prepared_branch(&states, index, value);
    }
    join_fanout_all(
        values.into_iter().enumerate().map(|(index, value)| {
            let branch_host = host.branch_host(index);
            let states = Arc::clone(&states);
            async move {
                let value = drive_fanout_value(value, &branch_host).await;
                record_finished_branch(&states, index, &value);
                value
            }
        }),
        |index, value| scope.end(index, value),
    )
    .await
}

pub(crate) const MAX_ACTIVE_FLOW_FUTURES: usize = 128;

const BRANCH_RUNNING: u8 = 0;
const BRANCH_OK: u8 = 1;
const BRANCH_ERR: u8 = 2;

struct FanoutBranchScope<'a, H: ExpressionHost> {
    host: &'a H,
    states: Arc<Vec<AtomicU8>>,
    ended: Vec<bool>,
}

impl<'a, H: ExpressionHost> FanoutBranchScope<'a, H> {
    fn start(host: &'a H, count: usize) -> Self {
        let states = Arc::new((0..count).map(|_| AtomicU8::new(BRANCH_RUNNING)).collect());
        for index in 0..count {
            host.fanout_branch_start(index);
        }
        Self {
            host,
            states,
            ended: vec![false; count],
        }
    }

    fn states(&self) -> Arc<Vec<AtomicU8>> {
        Arc::clone(&self.states)
    }

    fn end(&mut self, index: usize, value: &Value<H::Payload, H::Error>) {
        if !self.ended[index] {
            self.ended[index] = true;
            self.host.fanout_branch_end(index, value);
        }
    }
}

impl<H: ExpressionHost> Drop for FanoutBranchScope<'_, H> {
    fn drop(&mut self) {
        if self.ended.iter().all(|ended| *ended) {
            return;
        }
        let cancelled: Value<H::Payload, H::Error> =
            Value::Err(self.host.cancellation_error().unwrap_or_else(|| {
                H::Error::type_mismatch("completed fanout branch", "cancelled".into())
            }));
        let completed: Value<H::Payload, H::Error> = Value::Unit;
        for index in 0..self.ended.len() {
            if !self.ended[index] {
                let value = if self.states[index].load(Ordering::Acquire) == BRANCH_OK {
                    &completed
                } else {
                    &cancelled
                };
                self.end(index, value);
            }
        }
    }
}

fn record_prepared_branch<P, E>(states: &[AtomicU8], index: usize, value: &Value<P, E>) {
    if !matches!(value, Value::FlowFuture(_)) {
        record_finished_branch(states, index, value);
    }
}

fn record_finished_branch<P, E>(states: &[AtomicU8], index: usize, value: &Value<P, E>) {
    let state = if matches!(value, Value::Err(_)) {
        BRANCH_ERR
    } else {
        BRANCH_OK
    };
    states[index].store(state, Ordering::Release);
}

fn validate_fanout_value<H: ExpressionHost>(
    value: &Value<H::Payload, H::Error>,
    host: &H,
    unique: &mut BTreeSet<usize>,
) -> Result<(), H::Error> {
    let Value::FlowFuture(future) = value else {
        return Ok(());
    };
    host.validate_flow_future(future)?;
    unique.insert(Arc::as_ptr(future) as usize);
    if unique.len() > MAX_ACTIVE_FLOW_FUTURES {
        return Err(H::Error::type_mismatch(
            "fanout with at most 128 concurrent flow calls",
            format!("{} distinct flow futures", unique.len()),
        ));
    }
    Ok(())
}

async fn drive_fanout_value<H: ExpressionHost>(
    value: Value<H::Payload, H::Error>,
    host: &H,
) -> Value<H::Payload, H::Error> {
    match value {
        Value::FlowFuture(future) => {
            host.drive_flow_future(&future, FlowDriveMode::Parallel)
                .await
        }
        other => other,
    }
}

/// Evaluates a dynamic fanout through the shared expression engine.
pub async fn eval_dynamic_fanout<'a, H: ExpressionHost>(
    source: &'a Expr,
    lambda: &'a Expr,
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
        results.push(result);
    }
    let mut unique = BTreeSet::new();
    for value in &results {
        if let Err(error) = validate_fanout_value(value, host, &mut unique) {
            return Value::Err(error);
        }
    }
    if unique.is_empty() {
        return Value::List(results);
    }
    let mut scope = FanoutBranchScope::start(host, results.len());
    let states = scope.states();
    for (index, value) in results.iter().enumerate() {
        record_prepared_branch(&states, index, value);
    }
    join_fanout_all(
        results.into_iter().enumerate().map(|(index, value)| {
            let branch_host = host.branch_host(index);
            let states = Arc::clone(&states);
            async move {
                let value = drive_fanout_value(value, &branch_host).await;
                record_finished_branch(&states, index, &value);
                value
            }
        }),
        |index, value| scope.end(index, value),
    )
    .await
}

#[cfg(test)]
mod tests {
    use alloc::vec;
    use core::{
        future::{Future, poll_fn},
        pin::pin,
        sync::atomic::{AtomicU8, Ordering},
        task::{Context, Poll, Waker},
    };

    use super::*;
    use crate::{
        EvalError,
        ast::{BinOp, Ident, Literal, Node, Span},
        program::{FlowId, ModuleId},
        value::FlowFuture,
    };

    #[derive(Default)]
    struct BranchTrace {
        starts: [AtomicU8; 2],
        ends: [AtomicU8; 2],
        statuses: [AtomicU8; 2],
        order: [AtomicU8; 2],
        next_order: AtomicU8,
    }

    #[derive(Clone, Default)]
    struct BranchTraceHost(Arc<BranchTrace>);

    impl ExpressionHost for BranchTraceHost {
        type Payload = ();
        type Error = EvalError;

        fn undefined_var(&self, name: String) -> EvalError {
            EvalError::type_mismatch("bound variable", name)
        }

        fn undefined_field(&self, name: String) -> EvalError {
            EvalError::type_mismatch("existing field", name)
        }

        fn fanout_branch_start(&self, index: usize) {
            self.0.starts[index].fetch_add(1, Ordering::SeqCst);
        }

        fn fanout_branch_end(&self, index: usize, value: &Value<(), EvalError>) {
            self.0.ends[index].fetch_add(1, Ordering::SeqCst);
            self.0.statuses[index].store(
                if value.is_err() {
                    BRANCH_ERR
                } else {
                    BRANCH_OK
                },
                Ordering::SeqCst,
            );
            self.0.order[index].store(
                self.0.next_order.fetch_add(1, Ordering::SeqCst) + 1,
                Ordering::SeqCst,
            );
        }

        fn validate_flow_future(
            &self,
            _future: &FlowFuture<(), EvalError>,
        ) -> Result<(), EvalError> {
            Ok(())
        }

        fn drive_flow_future<'a>(
            &'a self,
            _future: &'a FlowFuture<(), EvalError>,
            _mode: FlowDriveMode,
        ) -> HostFuture<'a, Value<(), EvalError>> {
            Box::pin(poll_fn(|_| Poll::Pending))
        }

        fn eval_external<'a>(
            &'a self,
            effect: ExpressionEffect<(), EvalError>,
        ) -> HostFuture<'a, Value<(), EvalError>> {
            Box::pin(async move {
                match effect {
                    ExpressionEffect::ToolCall { name, .. } if name == "ready" => Value::Int(1),
                    ExpressionEffect::ToolCall { name, .. } if name == "pending" => {
                        poll_fn(|_| Poll::Pending).await
                    }
                    _ => panic!("unexpected effect"),
                }
            })
        }
    }

    fn pending_flow_value() -> Value<(), EvalError> {
        Value::FlowFuture(Arc::new(FlowFuture::new(
            FlowId {
                module: ModuleId(0),
                name: "pending".into(),
            },
            Vec::new(),
            Arc::new(()),
        )))
    }

    fn poll_then_drop<F: Future<Output = Value<(), EvalError>>>(future: F) {
        let mut future = Box::pin(future);
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Pending
        ));
        drop(future);
    }

    fn assert_branch_trace(host: &BranchTraceHost) {
        for index in 0..2 {
            assert_eq!(host.0.starts[index].load(Ordering::SeqCst), 1);
            assert_eq!(host.0.ends[index].load(Ordering::SeqCst), 1);
        }
        assert_eq!(host.0.statuses[0].load(Ordering::SeqCst), BRANCH_OK);
        assert_eq!(host.0.statuses[1].load(Ordering::SeqCst), BRANCH_ERR);
        assert_eq!(host.0.order[0].load(Ordering::SeqCst), 1);
        assert_eq!(host.0.order[1].load(Ordering::SeqCst), 2);
    }

    #[test]
    fn dropped_static_variable_and_dynamic_fanout_close_every_started_branch() {
        let tool = |name: &str| {
            Expr::Node(Node::ToolCall {
                path: vec![Ident::new(name, Span::default())],
                args: vec![],
            })
        };
        let static_source = Expr::List(vec![tool("ready"), tool("pending")]);
        let env = Env::new();
        let host = BranchTraceHost::default();
        poll_then_drop(eval_fanout(&static_source, &env, &host));
        assert_branch_trace(&host);

        let mut env = Env::new();
        env.bind(
            "items",
            Value::List(vec![Value::Int(1), pending_flow_value()]),
        );
        let source = Expr::Ident(Ident::new("items", Span::default()));
        let host = BranchTraceHost::default();
        poll_then_drop(eval_fanout(&source, &env, &host));
        assert_branch_trace(&host);

        let lambda = Expr::Lambda {
            params: vec![Ident::new("item", Span::default())],
            body: Box::new(Expr::Ident(Ident::new("item", Span::default()))),
        };
        let host = BranchTraceHost::default();
        poll_then_drop(eval_dynamic_fanout(&source, &lambda, &env, &host));
        assert_branch_trace(&host);
    }

    #[test]
    fn completed_fanout_closes_branches_once_in_source_order() {
        let source = Expr::List(vec![
            Expr::Literal(Literal::Int(1)),
            Expr::Literal(Literal::Int(2)),
        ]);
        let env = Env::new();
        let host = BranchTraceHost::default();
        let result = run_ready(eval_fanout(&source, &env, &host));
        assert!(
            matches!(result, Value::List(values) if matches!(&values[..], [Value::Int(1), Value::Int(2)]))
        );
        for index in 0..2 {
            assert_eq!(host.0.starts[index].load(Ordering::SeqCst), 1);
            assert_eq!(host.0.ends[index].load(Ordering::SeqCst), 1);
            assert_eq!(host.0.statuses[index].load(Ordering::SeqCst), BRANCH_OK);
            assert_eq!(host.0.order[index].load(Ordering::SeqCst), index as u8 + 1);
        }
    }

    #[derive(Clone)]
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

        fn eval_external<'a>(
            &'a self,
            effect: ExpressionEffect<(), EvalError>,
        ) -> HostFuture<'a, Value<(), EvalError>> {
            Box::pin(async move {
                match effect {
                    ExpressionEffect::ToolCall { .. } => Value::Int(5),
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

        let all = run_ready(eval_dynamic_fanout(&source, &lambda, &env, &ACTIVE_HOST));
        assert!(
            matches!(all, Value::List(items) if matches!(&items[..], [Value::Int(11), Value::Int(12)]))
        );
    }

    #[test]
    fn static_fanout_dispatches_without_external_node_handling() {
        let expr = Expr::Node(Node::Fanout {
            source: Box::new(Expr::List(vec![
                Expr::Literal(Literal::Int(1)),
                Expr::Node(Node::ToolCall {
                    path: vec![],
                    args: vec![],
                }),
            ])),
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
