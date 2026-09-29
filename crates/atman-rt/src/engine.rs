use alloc::{
    boxed::Box,
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::{future::Future, marker::PhantomData, pin::Pin};

use crate::{
    Env, ExpressionHost, HostValueOps, Value, ValueError,
    ast::{Arg, Expr, FlowDecl, ParamDecl, Stmt, WatchDecl},
    bind_pattern, eval_expr,
    expr::{EvaluatedArg, eval_expr_with_watch},
    pattern::PatternBindError,
    value::validate_value_type,
    watch::WatchRules,
};

pub type HostFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type StatementExecution<V, E> = (StatementOutcome<V, E>, Option<String>);
pub type FlowArgs<P, E> = Vec<(String, Value<P, E>)>;
pub type FlowOutcome<P, E> = StatementOutcome<Value<P, E>, E>;
pub type FlowExecution<P, E> = StatementExecution<Value<P, E>, E>;

#[derive(Debug)]
pub enum CallArgumentError<P, E> {
    TooManyPositional,
    Evaluation(Value<P, E>),
}

/// Evaluates explicit call arguments in source order and binds their names.
pub async fn bind_call_arguments<'a, P, E, F>(
    params: &'a [ParamDecl],
    args: &'a [Arg],
    mut evaluate: F,
) -> Result<FlowArgs<P, E>, CallArgumentError<P, E>>
where
    F: FnMut(&'a Expr) -> HostFuture<'a, Value<P, E>>,
{
    let mut bindings = Vec::with_capacity(args.len());
    for (index, arg) in args.iter().enumerate() {
        let (name, expr) = match arg {
            Arg::Positional(expr) => {
                let param = params
                    .get(index)
                    .ok_or(CallArgumentError::TooManyPositional)?;
                (param.name.name.clone(), expr)
            }
            Arg::Named { name, value } => (name.name.clone(), value),
        };
        let value = evaluate(expr).await;
        if matches!(value, Value::Err(_)) {
            return Err(CallArgumentError::Evaluation(value));
        }
        bindings.push((name, value));
    }
    Ok(bindings)
}

/// Binds already-evaluated arguments without delegating parameter semantics.
pub fn bind_evaluated_call_arguments<P, E>(
    params: &[ParamDecl],
    args: Vec<EvaluatedArg<P, E>>,
) -> Result<FlowArgs<P, E>, CallArgumentError<P, E>> {
    let mut bindings = Vec::with_capacity(args.len());
    for (index, arg) in args.into_iter().enumerate() {
        match arg {
            EvaluatedArg::Positional(value) => {
                let param = params
                    .get(index)
                    .ok_or(CallArgumentError::TooManyPositional)?;
                bindings.push((param.name.name.clone(), value));
            }
            EvaluatedArg::Named(name, value) => bindings.push((name, value)),
        }
    }
    Ok(bindings)
}

pub enum StatementOutcome<V, E> {
    Continue,
    Return(V),
    Err(E),
    LoopBreak,
    LoopContinue,
}

/// Owns one execution lifecycle after its start event has been emitted.
///
/// The engine consumes the scope with [`ExecutionScope::finish`] on a normal
/// terminal path and with [`ExecutionScope::abort`] when the driving future is
/// dropped while execution is suspended.
pub trait ExecutionScope<V, E>: Send {
    fn finish(self, outcome: &StatementOutcome<V, E>, preview: Option<&str>);
    fn abort(self);
}

impl<V, E> ExecutionScope<V, E> for () {
    fn finish(self, _outcome: &StatementOutcome<V, E>, _preview: Option<&str>) {}

    fn abort(self) {}
}

struct ScopeExit<S, V, E>
where
    S: ExecutionScope<V, E>,
{
    scope: Option<S>,
    marker: PhantomData<fn() -> (V, E)>,
}

impl<S, V, E> ScopeExit<S, V, E>
where
    S: ExecutionScope<V, E>,
{
    fn new(scope: S) -> Self {
        Self {
            scope: Some(scope),
            marker: PhantomData,
        }
    }

    fn finish(mut self, outcome: &StatementOutcome<V, E>, preview: Option<&str>) {
        self.scope
            .take()
            .expect("execution scope must be present")
            .finish(outcome, preview);
    }
}

impl<S, V, E> Drop for ScopeExit<S, V, E>
where
    S: ExecutionScope<V, E>,
{
    fn drop(&mut self) {
        if let Some(scope) = self.scope.take() {
            scope.abort();
        }
    }
}

pub enum LoopExit<V, E> {
    Break,
    Interrupted(StatementOutcome<V, E>),
}

/// Supplies an iteration body and its product-specific node events.
pub trait LoopHost: Send {
    type Value: Send;
    type Error: Send;
    type IterationScope: ExecutionScope<Self::Value, Self::Error>;

    fn iteration_start(
        &mut self,
        iteration: u64,
        node_id: &str,
        parent_node_id: Option<&str>,
    ) -> Self::IterationScope;
    fn preflight_iteration(
        &mut self,
        _iteration: u64,
        _node_id: &str,
        _parent_node_id: Option<&str>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
    fn execute_iteration<'a>(
        &'a mut self,
        node_id: &'a str,
    ) -> HostFuture<'a, StatementOutcome<Self::Value, Self::Error>>;
}

/// Runs loop iterations and consumes only break and continue outcomes.
pub async fn run_loop<H: LoopHost>(
    host: &mut H,
    parent_node_id: Option<&str>,
) -> LoopExit<H::Value, H::Error> {
    let mut iteration = 0u64;
    loop {
        let node_id = match parent_node_id {
            Some(parent) => format!("{parent}.iter[{iteration}]"),
            None => format!("iter[{iteration}]"),
        };
        let preflight = host.preflight_iteration(iteration, &node_id, parent_node_id);
        if let Err(error) = preflight {
            return LoopExit::Interrupted(StatementOutcome::Err(error));
        }
        let scope = ScopeExit::new(host.iteration_start(iteration, &node_id, parent_node_id));
        let outcome = host.execute_iteration(&node_id).await;
        let preview = match &outcome {
            StatementOutcome::LoopBreak => Some("break"),
            StatementOutcome::LoopContinue => Some("continue"),
            _ => None,
        };
        scope.finish(&outcome, preview);
        match outcome {
            StatementOutcome::Continue | StatementOutcome::LoopContinue => {}
            StatementOutcome::LoopBreak => return LoopExit::Break,
            other => return LoopExit::Interrupted(other),
        }
        iteration += 1;
    }
}

/// Evaluates a condition and runs its body only when the value is truthy.
pub async fn run_when<P, E, F, Fut>(
    condition: Value<P, E>,
    body: F,
) -> (StatementOutcome<Value<P, E>, E>, Option<bool>)
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = StatementOutcome<Value<P, E>, E>>,
{
    match condition {
        Value::Err(error) => (StatementOutcome::Err(error), None),
        Value::Unit | Value::Bool(false) => (StatementOutcome::Continue, Some(false)),
        _ => (body().await, Some(true)),
    }
}

pub enum Preflight<E> {
    Continue,
    Stop(E),
    StopAfterNode { error: E, preview: String },
}

/// Supplies external effects, cancellation decisions, and execution observations.
/// Language bindings and control flow remain inside the engine.
pub trait StatementHost: Send + Sync {
    type Payload: HostValueOps + Clone + Send + Sync;
    type Error: ValueError + Clone + Send + Sync;
    type ExprHost: ExpressionHost<Payload = Self::Payload, Error = Self::Error> + Send;
    type NodeScope: ExecutionScope<Value<Self::Payload, Self::Error>, Self::Error>;
    type IterationScope: ExecutionScope<Value<Self::Payload, Self::Error>, Self::Error>;

    fn preflight(
        &mut self,
        stmt: &Stmt,
        node_id: &str,
        parent_node_id: Option<&str>,
    ) -> Preflight<Self::Error>;
    fn node_start(
        &mut self,
        stmt: &Stmt,
        node_id: &str,
        parent_node_id: Option<&str>,
    ) -> Self::NodeScope;
    fn expression_host(
        &self,
        node_id: Option<&str>,
        parent_node_id: Option<&str>,
    ) -> Self::ExprHost;
    fn pattern_error(&self, error: PatternBindError) -> Self::Error;
    fn iteration_start(
        &mut self,
        iteration: u64,
        _node_id: &str,
        _parent_node_id: Option<&str>,
    ) -> Self::IterationScope;
    fn preflight_iteration(
        &mut self,
        _iteration: u64,
        _node_id: &str,
        _parent_node_id: Option<&str>,
    ) -> Result<(), Self::Error> {
        Ok(())
    }
    fn preview(
        &self,
        value: &Value<Self::Payload, Self::Error>,
        node_id: &str,
        parent_node_id: Option<&str>,
    ) -> Option<String>;
}

/// Drives the ordered statement sequence through one host implementation.
pub struct Engine<H: StatementHost> {
    host: H,
    env: Env<Value<H::Payload, H::Error>>,
}

impl<H: StatementHost> Engine<H> {
    pub fn new(host: H) -> Self {
        Self::with_env(host, Env::new())
    }

    pub fn with_env(host: H, env: Env<Value<H::Payload, H::Error>>) -> Self {
        Self { host, env }
    }

    pub fn into_env(self) -> Env<Value<H::Payload, H::Error>> {
        self.env
    }

    fn evaluate<'a>(
        &'a self,
        expr: &'a Expr,
        node_id: Option<&'a str>,
        parent_node_id: Option<&'a str>,
    ) -> HostFuture<'a, Value<H::Payload, H::Error>> {
        let host = self.host.expression_host(node_id, parent_node_id);
        Box::pin(async move { eval_expr(expr, &self.env, &host).await })
    }

    pub async fn run_flow(
        &mut self,
        flow: &FlowDecl,
        args: FlowArgs<H::Payload, H::Error>,
    ) -> FlowOutcome<H::Payload, H::Error> {
        let provided: Vec<String> = args.iter().map(|(name, _)| name.clone()).collect();
        for (name, value) in args {
            let value = match value {
                Value::Err(error) => return StatementOutcome::Err(error),
                value => value,
            };
            if value.contains_pending_call() {
                return StatementOutcome::Err(H::Error::type_mismatch(
                    "resolved flow argument",
                    "pending call".into(),
                ));
            }
            if let Some(param) = flow.params.iter().find(|param| param.name.name == name)
                && let Err(error) = validate_value_type(
                    &value,
                    &param.ty,
                    &format!("parameter `{}`", param.name.name),
                )
            {
                return StatementOutcome::Err(H::Error::type_mismatch(
                    &error.expected,
                    error.actual,
                ));
            }
            self.env.bind(name, value);
        }
        for param in &flow.params {
            if !provided.iter().any(|name| name == &param.name.name) {
                let Some(default) = &param.default else {
                    return StatementOutcome::Err(H::Error::missing_argument(&param.name.name));
                };
                let value = match self.evaluate(default, None, None).await {
                    Value::Err(error) => return StatementOutcome::Err(error),
                    value => value,
                };
                if value.contains_pending_call() {
                    return StatementOutcome::Err(H::Error::type_mismatch(
                        "resolved flow argument",
                        "pending call".into(),
                    ));
                }
                if let Err(error) = validate_value_type(
                    &value,
                    &param.ty,
                    &format!("parameter `{}`", param.name.name),
                ) {
                    return StatementOutcome::Err(H::Error::type_mismatch(
                        &error.expected,
                        error.actual,
                    ));
                }
                self.env.bind(param.name.name.clone(), value);
            }
        }
        let outcome = self.run_statements(&flow.body, "", None).await;
        let Some(return_type) = &flow.ret else {
            return outcome;
        };
        let implicit_unit = Value::Unit;
        let returned = match &outcome {
            StatementOutcome::Return(value) => value,
            StatementOutcome::Continue => &implicit_unit,
            _ => return outcome,
        };
        if let Err(error) = validate_value_type(returned, return_type, "return value") {
            return StatementOutcome::Err(H::Error::type_mismatch(&error.expected, error.actual));
        }
        outcome
    }

    pub fn run_statements<'a>(
        &'a mut self,
        stmts: &'a [Stmt],
        prefix: &'a str,
        parent_node_id: Option<&'a str>,
    ) -> HostFuture<'a, FlowOutcome<H::Payload, H::Error>> {
        Box::pin(async move {
            let mut watches: BTreeMap<&str, Vec<&WatchDecl>> = BTreeMap::new();
            for stmt in stmts {
                if let Stmt::Watch(watch) = stmt {
                    watches.entry(&watch.target.name).or_default().push(watch);
                }
            }
            for (index, stmt) in stmts.iter().enumerate() {
                let node_id = if prefix.is_empty() {
                    format!("{index}")
                } else {
                    format!("{prefix}.{index}")
                };
                match self.host.preflight(stmt, &node_id, parent_node_id) {
                    Preflight::Continue => {}
                    Preflight::Stop(error) => return StatementOutcome::Err(error),
                    Preflight::StopAfterNode { error, preview } => {
                        self.host
                            .node_start(stmt, &node_id, parent_node_id)
                            .finish(&StatementOutcome::Continue, Some(&preview));
                        return StatementOutcome::Err(error);
                    }
                }
                let scope = ScopeExit::new(self.host.node_start(stmt, &node_id, parent_node_id));
                let (outcome, preview) = match stmt {
                    Stmt::Bind { name, value } => {
                        let target = name.as_single_ident().map(|id| id.name.as_str());
                        let watch_rules = target
                            .and_then(|target| watches.get(target))
                            .map(|watches| WatchRules::compile(watches));
                        let expr_host = self.host.expression_host(Some(&node_id), parent_node_id);
                        let value = eval_expr_with_watch(
                            value,
                            &self.env,
                            &expr_host,
                            watch_rules.as_ref(),
                        )
                        .await;
                        match value {
                            Value::Err(error) => (StatementOutcome::Err(error), None),
                            value => {
                                let preview = self.host.preview(&value, &node_id, parent_node_id);
                                match bind_pattern(name, value, &mut self.env) {
                                    Ok(()) => (StatementOutcome::Continue, preview),
                                    Err(error) => (
                                        StatementOutcome::Err(self.host.pattern_error(error)),
                                        None,
                                    ),
                                }
                            }
                        }
                    }
                    Stmt::When { cond, body } => {
                        let condition = self.evaluate(cond, Some(&node_id), parent_node_id).await;
                        let (outcome, taken) = run_when(condition, || {
                            self.run_statements(body, &node_id, Some(&node_id))
                        })
                        .await;
                        (outcome, taken.map(|taken| taken.to_string()))
                    }
                    Stmt::Return { value } => {
                        let value = self.evaluate(value, Some(&node_id), parent_node_id).await;
                        match value {
                            Value::Err(error) => (StatementOutcome::Err(error), None),
                            value if value.contains_pending_call() => (
                                StatementOutcome::Err(H::Error::type_mismatch(
                                    "resolved flow result",
                                    "pending call".into(),
                                )),
                                None,
                            ),
                            value => {
                                let preview = self.host.preview(&value, &node_id, parent_node_id);
                                (StatementOutcome::Return(value), preview)
                            }
                        }
                    }
                    Stmt::Expr(expr) => {
                        let value = self.evaluate(expr, Some(&node_id), parent_node_id).await;
                        match value {
                            Value::Err(error) => (StatementOutcome::Err(error), None),
                            value => (
                                StatementOutcome::Continue,
                                self.host.preview(&value, &node_id, parent_node_id),
                            ),
                        }
                    }
                    Stmt::Watch(_) => (StatementOutcome::Continue, None),
                    Stmt::Loop { body } => {
                        let mut loop_host = EngineLoopHost { engine: self, body };
                        match run_loop(&mut loop_host, Some(&node_id)).await {
                            LoopExit::Break => {
                                (StatementOutcome::Continue, Some("loop end".into()))
                            }
                            LoopExit::Interrupted(outcome) => {
                                (outcome, Some("loop interrupted".into()))
                            }
                        }
                    }
                    Stmt::Break => (StatementOutcome::LoopBreak, Some("break".into())),
                    Stmt::Continue => (StatementOutcome::LoopContinue, Some("continue".into())),
                };
                scope.finish(&outcome, preview.as_deref());
                if !matches!(outcome, StatementOutcome::Continue) {
                    return outcome;
                }
            }
            StatementOutcome::Continue
        })
    }
}

struct EngineLoopHost<'e, 'b, H: StatementHost> {
    engine: &'e mut Engine<H>,
    body: &'b [Stmt],
}

impl<H: StatementHost> LoopHost for EngineLoopHost<'_, '_, H> {
    type Value = Value<H::Payload, H::Error>;
    type Error = H::Error;
    type IterationScope = H::IterationScope;

    fn iteration_start(
        &mut self,
        iteration: u64,
        node_id: &str,
        parent_node_id: Option<&str>,
    ) -> Self::IterationScope {
        self.engine
            .host
            .iteration_start(iteration, node_id, parent_node_id)
    }

    fn preflight_iteration(
        &mut self,
        iteration: u64,
        node_id: &str,
        parent_node_id: Option<&str>,
    ) -> Result<(), Self::Error> {
        self.engine
            .host
            .preflight_iteration(iteration, node_id, parent_node_id)
    }

    fn execute_iteration<'a>(
        &'a mut self,
        node_id: &'a str,
    ) -> HostFuture<'a, FlowOutcome<H::Payload, H::Error>> {
        self.engine
            .run_statements(self.body, node_id, Some(node_id))
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use alloc::{string::ToString, vec, vec::Vec};
    use core::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::{
        EvalError,
        ast::{Ident, Literal, Span, TypeExpr},
    };

    fn test_param(name: &str) -> ParamDecl {
        ParamDecl {
            name: Ident::new(name, Span::default()),
            ty: TypeExpr::Named(Ident::new("Int", Span::default())),
            default: None,
        }
    }

    fn test_argument<'a>(expr: &'a Expr) -> HostFuture<'a, Value<(), &'static str>> {
        Box::pin(async move {
            match expr {
                Expr::Literal(Literal::Int(99)) => panic!("argument after stop was evaluated"),
                Expr::Literal(Literal::Int(value)) => Value::Int(*value),
                _ => Value::Err("bad argument"),
            }
        })
    }

    #[test]
    fn call_arguments_bind_in_order_and_stop_before_later_values() {
        let params = [test_param("first"), test_param("second")];
        let args = [
            Arg::Positional(Expr::Literal(Literal::Int(1))),
            Arg::Named {
                name: Ident::new("chosen", Span::default()),
                value: Expr::Literal(Literal::Int(2)),
            },
        ];
        let bindings = run_ready(bind_call_arguments(&params, &args, test_argument)).unwrap();
        assert_eq!(bindings[0].0, "first");
        assert!(matches!(bindings[0].1, Value::Int(1)));
        assert_eq!(bindings[1].0, "chosen");
        assert!(matches!(bindings[1].1, Value::Int(2)));

        let error_args = [
            Arg::Positional(Expr::Literal(Literal::Int(1))),
            Arg::Named {
                name: Ident::new("chosen", Span::default()),
                value: Expr::Ident(Ident::new("bad", Span::default())),
            },
            Arg::Positional(Expr::Literal(Literal::Int(99))),
        ];
        assert!(matches!(
            run_ready(bind_call_arguments(&params, &error_args, test_argument)),
            Err(CallArgumentError::Evaluation(Value::Err("bad argument")))
        ));

        let excess_args = [
            Arg::Positional(Expr::Literal(Literal::Int(1))),
            Arg::Positional(Expr::Literal(Literal::Int(2))),
            Arg::Positional(Expr::Literal(Literal::Int(99))),
        ];
        assert!(matches!(
            run_ready(bind_call_arguments(&params, &excess_args, test_argument)),
            Err(CallArgumentError::TooManyPositional)
        ));
    }

    struct RecordedScope {
        events: Arc<Mutex<Vec<String>>>,
        node_id: String,
        include_preview: bool,
    }

    impl<V, E> ExecutionScope<V, E> for RecordedScope {
        fn finish(self, _outcome: &StatementOutcome<V, E>, preview: Option<&str>) {
            let event = if self.include_preview {
                alloc::format!("end:{}:{}", self.node_id, preview.unwrap_or(""))
            } else {
                alloc::format!("end:{}", self.node_id)
            };
            self.events.lock().unwrap().push(event);
        }

        fn abort(self) {
            self.events
                .lock()
                .unwrap()
                .push(alloc::format!("cancel:{}", self.node_id));
        }
    }

    struct TestHost {
        events: Arc<Mutex<Vec<String>>>,
        stop_before: bool,
        pending_external: bool,
    }

    #[derive(Clone)]
    struct TestExprHost {
        pending_external: bool,
    }

    impl ExpressionHost for TestExprHost {
        type Payload = ();
        type Error = EvalError;

        fn undefined_var(&self, name: String) -> EvalError {
            EvalError::type_mismatch("defined variable", name)
        }

        fn undefined_field(&self, name: String) -> EvalError {
            EvalError::type_mismatch("defined field", name)
        }

        fn eval_external<'b>(
            &'b self,
            _effect: crate::ExpressionEffect<(), EvalError>,
        ) -> HostFuture<'b, Value<(), EvalError>> {
            if self.pending_external {
                Box::pin(core::future::pending())
            } else {
                Box::pin(async { panic!("unexpected external expression") })
            }
        }
    }

    impl StatementHost for TestHost {
        type Payload = ();
        type Error = EvalError;
        type ExprHost = TestExprHost;
        type NodeScope = RecordedScope;
        type IterationScope = RecordedScope;

        fn preflight(
            &mut self,
            _stmt: &Stmt,
            _node_id: &str,
            _parent_node_id: Option<&str>,
        ) -> Preflight<Self::Error> {
            if self.stop_before {
                Preflight::StopAfterNode {
                    error: EvalError::type_mismatch("active flow", "cancelled".into()),
                    preview: "hard stop".to_string(),
                }
            } else {
                Preflight::Continue
            }
        }

        fn node_start(
            &mut self,
            _stmt: &Stmt,
            node_id: &str,
            _parent_node_id: Option<&str>,
        ) -> Self::NodeScope {
            self.events
                .lock()
                .unwrap()
                .push(alloc::format!("start:{node_id}"));
            RecordedScope {
                events: self.events.clone(),
                node_id: node_id.to_string(),
                include_preview: true,
            }
        }

        fn expression_host(
            &self,
            _node_id: Option<&str>,
            _parent_node_id: Option<&str>,
        ) -> Self::ExprHost {
            TestExprHost {
                pending_external: self.pending_external,
            }
        }

        fn pattern_error(&self, error: PatternBindError) -> EvalError {
            EvalError::type_mismatch("matching pattern", alloc::format!("{error:?}"))
        }

        fn preview(
            &self,
            _value: &Value<(), EvalError>,
            _node_id: &str,
            _parent_node_id: Option<&str>,
        ) -> Option<String> {
            None
        }

        fn iteration_start(
            &mut self,
            iteration: u64,
            node_id: &str,
            _parent_node_id: Option<&str>,
        ) -> Self::IterationScope {
            self.events
                .lock()
                .unwrap()
                .push(alloc::format!("iteration:{iteration}:{node_id}"));
            RecordedScope {
                events: self.events.clone(),
                node_id: node_id.to_string(),
                include_preview: false,
            }
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
    fn engine_orders_node_events_and_stops_after_return() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let result = {
            let host = TestHost {
                events: events.clone(),
                stop_before: false,
                pending_external: false,
            };
            let mut engine = Engine::new(host);
            run_ready(engine.run_statements(
                &[
                    Stmt::Expr(Expr::Literal(Literal::Int(0))),
                    Stmt::Return {
                        value: Expr::Literal(Literal::Int(7)),
                    },
                    Stmt::Expr(Expr::Literal(Literal::Int(99))),
                ],
                "branch",
                None,
            ))
        };
        assert!(matches!(result, StatementOutcome::Return(Value::Int(7))));
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "start:branch.0",
                "end:branch.0:",
                "start:branch.1",
                "end:branch.1:",
            ]
        );
    }

    fn engine_for_type_tests() -> Engine<TestHost> {
        Engine::new(TestHost {
            events: Arc::new(Mutex::new(Vec::new())),
            stop_before: false,
            pending_external: false,
        })
    }

    fn typed_flow(params: Vec<ParamDecl>, ret: Option<TypeExpr>, body: Vec<Stmt>) -> FlowDecl {
        FlowDecl {
            name: Ident::new("typed", Span::default()),
            params,
            ret,
            contract: None,
            body,
        }
    }

    #[test]
    fn flow_boundary_checks_parameters_defaults_and_missing_arguments() {
        let param = test_param("count");
        let flow = typed_flow(
            vec![param.clone()],
            Some(TypeExpr::Named(Ident::new("Int", Span::default()))),
            vec![Stmt::Return {
                value: Expr::Ident(param.name.clone()),
            }],
        );
        let mismatch = run_ready(
            engine_for_type_tests()
                .run_flow(&flow, vec![("count".into(), Value::Str("one".into()))]),
        );
        assert!(matches!(
            mismatch,
            StatementOutcome::Err(EvalError::TypeMismatch { expected, actual })
                if expected == "parameter `count`: Int"
                    && actual == "parameter `count`: string"
        ));

        let missing = run_ready(engine_for_type_tests().run_flow(&flow, Vec::new()));
        assert!(matches!(
            missing,
            StatementOutcome::Err(EvalError::MissingArgument(name)) if name == "count"
        ));

        let mut default_param = param;
        default_param.default = Some(Expr::Literal(Literal::Str("one".into())));
        let bad_default = typed_flow(
            vec![default_param],
            None,
            vec![Stmt::Return {
                value: Expr::Literal(Literal::Int(1)),
            }],
        );
        let mismatch = run_ready(engine_for_type_tests().run_flow(&bad_default, Vec::new()));
        assert!(matches!(
            mismatch,
            StatementOutcome::Err(EvalError::TypeMismatch { expected, actual })
                if expected == "parameter `count`: Int"
                    && actual == "parameter `count`: string"
        ));
    }

    #[test]
    fn flow_boundary_checks_explicit_and_implicit_returns() {
        let list_type = TypeExpr::List(alloc::boxed::Box::new(TypeExpr::Named(Ident::new(
            "int",
            Span::default(),
        ))));
        let bad_return = typed_flow(
            Vec::new(),
            Some(list_type),
            vec![Stmt::Return {
                value: Expr::List(vec![
                    Expr::Literal(Literal::Int(1)),
                    Expr::Literal(Literal::Str("two".into())),
                ]),
            }],
        );
        let mismatch = run_ready(engine_for_type_tests().run_flow(&bad_return, Vec::new()));
        assert!(matches!(
            mismatch,
            StatementOutcome::Err(EvalError::TypeMismatch { expected, actual })
                if expected == "return value: [int]" && actual == "return value[1]: string"
        ));

        let fallthrough = typed_flow(
            Vec::new(),
            Some(TypeExpr::Named(Ident::new("int", Span::default()))),
            Vec::new(),
        );
        let mismatch = run_ready(engine_for_type_tests().run_flow(&fallthrough, Vec::new()));
        assert!(matches!(
            mismatch,
            StatementOutcome::Err(EvalError::TypeMismatch { expected, actual })
                if expected == "return value: int" && actual == "return value: unit"
        ));

        let schema_marker = typed_flow(
            Vec::new(),
            Some(TypeExpr::Named(Ident::new("Review", Span::default()))),
            vec![Stmt::Return {
                value: Expr::Literal(Literal::Str("review".into())),
            }],
        );
        assert!(matches!(
            run_ready(engine_for_type_tests().run_flow(&schema_marker, Vec::new())),
            StatementOutcome::Return(Value::Str(value)) if value == "review"
        ));
    }

    #[test]
    fn preflight_stop_records_node_without_executing_it() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let result = {
            let host = TestHost {
                events: events.clone(),
                stop_before: true,
                pending_external: false,
            };
            let mut engine = Engine::new(host);
            run_ready(engine.run_statements(&[Stmt::Break], "", None))
        };
        assert!(
            matches!(result, StatementOutcome::Err(EvalError::TypeMismatch { actual, .. }) if actual == "cancelled")
        );
        assert_eq!(*events.lock().unwrap(), vec!["start:0", "end:0:hard stop"]);
    }

    #[test]
    fn dropping_engine_future_cancels_open_node_scope_once() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let host = TestHost {
            events: events.clone(),
            stop_before: false,
            pending_external: true,
        };
        let mut engine = Engine::new(host);
        let stmts = [Stmt::Expr(Expr::Call {
            func: Ident::new("wait", Span::default()),
            args: Vec::new(),
        })];
        {
            let mut future = engine.run_statements(&stmts, "", None);
            assert!(matches!(
                future
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop())),
                Poll::Pending
            ));
        }
        assert_eq!(*events.lock().unwrap(), vec!["start:0", "cancel:0"]);
    }

    struct TestLoopHost {
        events: Arc<Mutex<Vec<String>>>,
        iteration: usize,
        return_after_continue: bool,
        pending: bool,
    }

    impl LoopHost for TestLoopHost {
        type Value = i32;
        type Error = &'static str;
        type IterationScope = RecordedScope;

        fn iteration_start(
            &mut self,
            iteration: u64,
            node_id: &str,
            parent: Option<&str>,
        ) -> Self::IterationScope {
            self.events.lock().unwrap().push(alloc::format!(
                "start:{iteration}:{node_id}:{}",
                parent.unwrap_or("")
            ));
            RecordedScope {
                events: self.events.clone(),
                node_id: node_id.to_string(),
                include_preview: false,
            }
        }

        fn execute_iteration<'b>(
            &'b mut self,
            node_id: &'b str,
        ) -> HostFuture<'b, StatementOutcome<i32, &'static str>> {
            if self.pending {
                return Box::pin(core::future::pending());
            }
            Box::pin(async move {
                self.events
                    .lock()
                    .unwrap()
                    .push(alloc::format!("execute:{node_id}"));
                let outcome = match self.iteration {
                    0 => StatementOutcome::Continue,
                    1 if self.return_after_continue => StatementOutcome::Return(7),
                    1 => StatementOutcome::LoopContinue,
                    _ => StatementOutcome::LoopBreak,
                };
                self.iteration += 1;
                outcome
            })
        }
    }

    #[test]
    fn loop_consumes_continue_and_break_after_ending_each_iteration() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let result = run_ready(run_loop(
            &mut TestLoopHost {
                events: events.clone(),
                iteration: 0,
                return_after_continue: false,
                pending: false,
            },
            Some("loop.0"),
        ));
        assert!(matches!(result, LoopExit::Break));
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "start:0:loop.0.iter[0]:loop.0",
                "execute:loop.0.iter[0]",
                "end:loop.0.iter[0]",
                "start:1:loop.0.iter[1]:loop.0",
                "execute:loop.0.iter[1]",
                "end:loop.0.iter[1]",
                "start:2:loop.0.iter[2]:loop.0",
                "execute:loop.0.iter[2]",
                "end:loop.0.iter[2]",
            ]
        );
    }

    #[test]
    fn loop_preserves_return_after_ending_its_iteration() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let result = run_ready(run_loop(
            &mut TestLoopHost {
                events: events.clone(),
                iteration: 0,
                return_after_continue: true,
                pending: false,
            },
            None,
        ));
        assert!(matches!(
            result,
            LoopExit::Interrupted(StatementOutcome::Return(7))
        ));
        assert_eq!(
            *events.lock().unwrap(),
            vec![
                "start:0:iter[0]:",
                "execute:iter[0]",
                "end:iter[0]",
                "start:1:iter[1]:",
                "execute:iter[1]",
                "end:iter[1]",
            ]
        );
    }

    #[test]
    fn dropping_loop_future_cancels_open_iteration_scope_once() {
        let events = Arc::new(Mutex::new(Vec::new()));
        let mut host = TestLoopHost {
            events: events.clone(),
            iteration: 0,
            return_after_continue: false,
            pending: true,
        };
        {
            let mut future = Box::pin(run_loop(&mut host, Some("loop.0")));
            assert!(matches!(
                future
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop())),
                Poll::Pending
            ));
        }
        assert_eq!(
            *events.lock().unwrap(),
            vec!["start:0:loop.0.iter[0]:loop.0", "cancel:loop.0.iter[0]"]
        );
    }

    #[test]
    fn when_skips_false_values_and_propagates_condition_errors() {
        let mut called = false;
        let (outcome, taken) = run_ready(run_when(Value::<(), &'static str>::Unit, || {
            called = true;
            async { StatementOutcome::Return(Value::Int(42)) }
        }));
        assert!(matches!(outcome, StatementOutcome::Continue));
        assert_eq!(taken, Some(false));
        assert!(!called);

        let (outcome, taken) = run_ready(run_when(
            Value::<(), &'static str>::Err("condition"),
            || {
                called = true;
                async { StatementOutcome::Return(Value::Int(42)) }
            },
        ));
        assert!(matches!(outcome, StatementOutcome::Err("condition")));
        assert_eq!(taken, None);
        assert!(!called);

        let (outcome, taken) = run_ready(run_when(Value::<(), &'static str>::Int(1), || {
            called = true;
            async { StatementOutcome::Return(Value::Int(42)) }
        }));
        assert!(matches!(outcome, StatementOutcome::Return(Value::Int(42))));
        assert_eq!(taken, Some(true));
        assert!(called);
    }
}
