use alloc::{
    boxed::Box,
    collections::BTreeMap,
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::{future::Future, pin::Pin};

use crate::{
    Env, ExpressionHost, HostValueOps, Value, ValueError,
    ast::{Arg, Expr, FlowDecl, ParamDecl, Stmt, WatchDecl},
    bind_pattern, eval_expr,
    expr::{EvaluatedArg, eval_expr_with_watch},
    pattern::PatternBindError,
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

pub enum LoopExit<V, E> {
    Break,
    Interrupted(StatementOutcome<V, E>),
}

/// Supplies an iteration body and its product-specific node events.
pub trait LoopHost: Send {
    type Value: Send;
    type Error: Send;

    fn iteration_start(&mut self, iteration: u64, node_id: &str, parent_node_id: Option<&str>);
    fn execute_iteration<'a>(
        &'a mut self,
        node_id: &'a str,
    ) -> HostFuture<'a, StatementOutcome<Self::Value, Self::Error>>;
    fn iteration_end(
        &mut self,
        node_id: &str,
        outcome: &StatementOutcome<Self::Value, Self::Error>,
        parent_node_id: Option<&str>,
    );
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
        host.iteration_start(iteration, &node_id, parent_node_id);
        let outcome = host.execute_iteration(&node_id).await;
        host.iteration_end(&node_id, &outcome, parent_node_id);
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

    fn preflight(&mut self, stmt: &Stmt, node_id: &str) -> Preflight<Self::Error>;
    fn node_start(&mut self, stmt: &Stmt, node_id: &str, parent_node_id: Option<&str>);
    fn expression_host(&self, node_id: &str) -> Self::ExprHost;
    fn pattern_error(&self, error: PatternBindError) -> Self::Error;
    fn iteration_start(&mut self, _iteration: u64, _node_id: &str, _parent_node_id: Option<&str>) {}
    fn iteration_end(
        &mut self,
        _node_id: &str,
        _outcome: &FlowOutcome<Self::Payload, Self::Error>,
        _parent_node_id: Option<&str>,
    ) {
    }
    fn preview(&self, value: &Value<Self::Payload, Self::Error>) -> Option<String>;
    fn node_end(
        &mut self,
        node_id: &str,
        outcome: &FlowOutcome<Self::Payload, Self::Error>,
        parent_node_id: Option<&str>,
        preview: Option<&str>,
    );
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
        node_id: &'a str,
    ) -> HostFuture<'a, Value<H::Payload, H::Error>> {
        let host = self.host.expression_host(node_id);
        Box::pin(async move { eval_expr(expr, &self.env, &host).await })
    }

    pub async fn run_flow(
        &mut self,
        flow: &FlowDecl,
        args: FlowArgs<H::Payload, H::Error>,
    ) -> FlowOutcome<H::Payload, H::Error> {
        let provided: Vec<String> = args.iter().map(|(name, _)| name.clone()).collect();
        for (name, value) in args {
            self.env.bind(name, value);
        }
        for param in &flow.params {
            if !provided.iter().any(|name| name == &param.name.name)
                && let Some(default) = &param.default
            {
                let value = self.evaluate(default, "").await;
                if let Value::Err(error) = value {
                    return StatementOutcome::Err(error);
                }
                self.env.bind(param.name.name.clone(), value);
            }
        }
        self.run_statements(&flow.body, "", None).await
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
                match self.host.preflight(stmt, &node_id) {
                    Preflight::Continue => {}
                    Preflight::Stop(error) => return StatementOutcome::Err(error),
                    Preflight::StopAfterNode { error, preview } => {
                        self.host.node_start(stmt, &node_id, parent_node_id);
                        self.host.node_end(
                            &node_id,
                            &StatementOutcome::Continue,
                            parent_node_id,
                            Some(&preview),
                        );
                        return StatementOutcome::Err(error);
                    }
                }
                self.host.node_start(stmt, &node_id, parent_node_id);
                let (outcome, preview) = match stmt {
                    Stmt::Bind { name, value } => {
                        let target = name.as_single_ident().map(|id| id.name.as_str());
                        let watch_rules = target
                            .and_then(|target| watches.get(target))
                            .map(|watches| WatchRules::compile(watches));
                        let expr_host = self.host.expression_host(&node_id);
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
                                let preview = self.host.preview(&value);
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
                        let condition = self.evaluate(cond, &node_id).await;
                        let (outcome, taken) = run_when(condition, || {
                            self.run_statements(body, &node_id, Some(&node_id))
                        })
                        .await;
                        (outcome, taken.map(|taken| taken.to_string()))
                    }
                    Stmt::Return { value } => {
                        let value = self.evaluate(value, &node_id).await;
                        match value {
                            Value::Err(error) => (StatementOutcome::Err(error), None),
                            value if value.contains_flow_future() => (
                                StatementOutcome::Err(H::Error::type_mismatch(
                                    "resolved flow result",
                                    "flow future".into(),
                                )),
                                None,
                            ),
                            value => {
                                let preview = self.host.preview(&value);
                                (StatementOutcome::Return(value), preview)
                            }
                        }
                    }
                    Stmt::Expr(expr) => {
                        let value = self.evaluate(expr, &node_id).await;
                        match value {
                            Value::Err(error) => (StatementOutcome::Err(error), None),
                            value => (StatementOutcome::Continue, self.host.preview(&value)),
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
                self.host
                    .node_end(&node_id, &outcome, parent_node_id, preview.as_deref());
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

    fn iteration_start(&mut self, iteration: u64, node_id: &str, parent_node_id: Option<&str>) {
        self.engine
            .host
            .iteration_start(iteration, node_id, parent_node_id);
    }

    fn execute_iteration<'a>(
        &'a mut self,
        node_id: &'a str,
    ) -> HostFuture<'a, FlowOutcome<H::Payload, H::Error>> {
        self.engine
            .run_statements(self.body, node_id, Some(node_id))
    }

    fn iteration_end(
        &mut self,
        node_id: &str,
        outcome: &FlowOutcome<H::Payload, H::Error>,
        parent_node_id: Option<&str>,
    ) {
        self.engine
            .host
            .iteration_end(node_id, outcome, parent_node_id);
    }
}

#[cfg(test)]
mod tests {
    use alloc::{string::ToString, vec, vec::Vec};
    use core::{
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };

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

    struct TestHost<'a> {
        events: &'a mut Vec<String>,
        stop_before: bool,
    }

    #[derive(Clone)]
    struct TestExprHost;

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
            Box::pin(async { panic!("unexpected external expression") })
        }
    }

    impl StatementHost for TestHost<'_> {
        type Payload = ();
        type Error = EvalError;
        type ExprHost = TestExprHost;

        fn preflight(&mut self, _stmt: &Stmt, _node_id: &str) -> Preflight<Self::Error> {
            if self.stop_before {
                Preflight::StopAfterNode {
                    error: EvalError::type_mismatch("active flow", "cancelled".into()),
                    preview: "hard stop".to_string(),
                }
            } else {
                Preflight::Continue
            }
        }

        fn node_start(&mut self, _stmt: &Stmt, node_id: &str, _parent_node_id: Option<&str>) {
            self.events.push(alloc::format!("start:{node_id}"));
        }

        fn expression_host(&self, _node_id: &str) -> Self::ExprHost {
            TestExprHost
        }

        fn pattern_error(&self, error: PatternBindError) -> EvalError {
            EvalError::type_mismatch("matching pattern", alloc::format!("{error:?}"))
        }

        fn preview(&self, _value: &Value<(), EvalError>) -> Option<String> {
            None
        }

        fn node_end(
            &mut self,
            node_id: &str,
            _outcome: &StatementOutcome<Value<(), EvalError>, Self::Error>,
            _parent_node_id: Option<&str>,
            preview: Option<&str>,
        ) {
            self.events
                .push(alloc::format!("end:{node_id}:{}", preview.unwrap_or("")));
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
        let mut events = Vec::new();
        let result = {
            let host = TestHost {
                events: &mut events,
                stop_before: false,
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
            events,
            vec![
                "start:branch.0",
                "end:branch.0:",
                "start:branch.1",
                "end:branch.1:",
            ]
        );
    }

    #[test]
    fn preflight_stop_records_node_without_executing_it() {
        let mut events = Vec::new();
        let result = {
            let host = TestHost {
                events: &mut events,
                stop_before: true,
            };
            let mut engine = Engine::new(host);
            run_ready(engine.run_statements(&[Stmt::Break], "", None))
        };
        assert!(
            matches!(result, StatementOutcome::Err(EvalError::TypeMismatch { actual, .. }) if actual == "cancelled")
        );
        assert_eq!(events, vec!["start:0", "end:0:hard stop"]);
    }

    struct TestLoopHost<'a> {
        events: &'a mut Vec<String>,
        iteration: usize,
        return_after_continue: bool,
    }

    impl LoopHost for TestLoopHost<'_> {
        type Value = i32;
        type Error = &'static str;

        fn iteration_start(&mut self, iteration: u64, node_id: &str, parent: Option<&str>) {
            self.events.push(alloc::format!(
                "start:{iteration}:{node_id}:{}",
                parent.unwrap_or("")
            ));
        }

        fn execute_iteration<'b>(
            &'b mut self,
            node_id: &'b str,
        ) -> HostFuture<'b, StatementOutcome<i32, &'static str>> {
            Box::pin(async move {
                self.events.push(alloc::format!("execute:{node_id}"));
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

        fn iteration_end(
            &mut self,
            node_id: &str,
            _outcome: &StatementOutcome<i32, &'static str>,
            _parent: Option<&str>,
        ) {
            self.events.push(alloc::format!("end:{node_id}"));
        }
    }

    #[test]
    fn loop_consumes_continue_and_break_after_ending_each_iteration() {
        let mut events = Vec::new();
        let result = run_ready(run_loop(
            &mut TestLoopHost {
                events: &mut events,
                iteration: 0,
                return_after_continue: false,
            },
            Some("loop.0"),
        ));
        assert!(matches!(result, LoopExit::Break));
        assert_eq!(
            events,
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
        let mut events = Vec::new();
        let result = run_ready(run_loop(
            &mut TestLoopHost {
                events: &mut events,
                iteration: 0,
                return_after_continue: true,
            },
            None,
        ));
        assert!(matches!(
            result,
            LoopExit::Interrupted(StatementOutcome::Return(7))
        ));
        assert_eq!(
            events,
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
