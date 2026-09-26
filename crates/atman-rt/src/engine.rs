use alloc::{
    boxed::Box,
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::{future::Future, pin::Pin};

use crate::{
    Value,
    ast::{Arg, Expr, FlowDecl, ParamDecl, Pattern, Stmt},
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

/// Supplies product-specific execution, cancellation decisions, and event sinks.
pub trait StatementHost: Send {
    type Payload: Clone + Send + Sync;
    type Error: Send;

    fn preflight(&mut self, stmt: &Stmt, node_id: &str) -> Preflight<Self::Error>;
    fn bind_parameter(&mut self, name: String, value: Value<Self::Payload, Self::Error>);
    fn evaluate_default<'a>(
        &'a mut self,
        expr: &'a Expr,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>>;
    fn node_start(&mut self, stmt: &Stmt, node_id: &str, parent_node_id: Option<&str>);
    fn evaluate<'a>(
        &'a mut self,
        expr: &'a Expr,
        node_id: &'a str,
    ) -> HostFuture<'a, Value<Self::Payload, Self::Error>>;
    fn bind<'a>(
        &'a mut self,
        pattern: &'a Pattern,
        expr: &'a Expr,
        node_id: &'a str,
    ) -> HostFuture<'a, FlowExecution<Self::Payload, Self::Error>>;
    fn run_body<'a>(
        &'a mut self,
        body: &'a [Stmt],
        node_id: &'a str,
    ) -> HostFuture<'a, FlowOutcome<Self::Payload, Self::Error>>;
    fn run_loop<'a>(
        &'a mut self,
        body: &'a [Stmt],
        node_id: &'a str,
    ) -> HostFuture<'a, FlowExecution<Self::Payload, Self::Error>>;
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
pub struct Engine<H> {
    host: H,
}

impl<H: StatementHost> Engine<H> {
    pub fn new(host: H) -> Self {
        Self { host }
    }

    pub async fn run_flow(
        &mut self,
        flow: &FlowDecl,
        args: FlowArgs<H::Payload, H::Error>,
    ) -> FlowOutcome<H::Payload, H::Error> {
        let provided: Vec<String> = args.iter().map(|(name, _)| name.clone()).collect();
        for (name, value) in args {
            self.host.bind_parameter(name, value);
        }
        for param in &flow.params {
            if !provided.iter().any(|name| name == &param.name.name)
                && let Some(default) = &param.default
            {
                let value = self.host.evaluate_default(default).await;
                self.host.bind_parameter(param.name.name.clone(), value);
            }
        }
        self.run_statements(&flow.body, "", None).await
    }

    pub async fn run_statements(
        &mut self,
        stmts: &[Stmt],
        prefix: &str,
        parent_node_id: Option<&str>,
    ) -> FlowOutcome<H::Payload, H::Error> {
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
                Stmt::Bind { name, value } => self.host.bind(name, value, &node_id).await,
                Stmt::When { cond, body } => {
                    let condition = self.host.evaluate(cond, &node_id).await;
                    let (outcome, taken) =
                        run_when(condition, || self.host.run_body(body, &node_id)).await;
                    (outcome, taken.map(|taken| taken.to_string()))
                }
                Stmt::Return { value } => {
                    let value = self.host.evaluate(value, &node_id).await;
                    match value {
                        Value::Err(error) => (StatementOutcome::Err(error), None),
                        value => {
                            let preview = self.host.preview(&value);
                            (StatementOutcome::Return(value), preview)
                        }
                    }
                }
                Stmt::Expr(expr) => {
                    let value = self.host.evaluate(expr, &node_id).await;
                    match value {
                        Value::Err(error) => (StatementOutcome::Err(error), None),
                        value => (StatementOutcome::Continue, self.host.preview(&value)),
                    }
                }
                Stmt::Watch(_) => (StatementOutcome::Continue, None),
                Stmt::Loop { body } => self.host.run_loop(body, &node_id).await,
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
    use crate::ast::{Ident, Literal, Span, TypeExpr};

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

    impl StatementHost for TestHost<'_> {
        type Payload = ();
        type Error = &'static str;

        fn preflight(&mut self, _stmt: &Stmt, _node_id: &str) -> Preflight<Self::Error> {
            if self.stop_before {
                Preflight::StopAfterNode {
                    error: "cancelled",
                    preview: "hard stop".to_string(),
                }
            } else {
                Preflight::Continue
            }
        }

        fn bind_parameter(&mut self, _name: String, _value: Value<(), &'static str>) {
            panic!("unexpected test parameter")
        }

        fn evaluate_default<'b>(
            &'b mut self,
            _expr: &'b Expr,
        ) -> HostFuture<'b, Value<(), &'static str>> {
            Box::pin(async { panic!("unexpected test default") })
        }

        fn node_start(&mut self, _stmt: &Stmt, node_id: &str, _parent_node_id: Option<&str>) {
            self.events.push(alloc::format!("start:{node_id}"));
        }

        fn evaluate<'b>(
            &'b mut self,
            expr: &'b Expr,
            node_id: &'b str,
        ) -> HostFuture<'b, Value<(), &'static str>> {
            Box::pin(async move {
                self.events.push(alloc::format!("execute:{node_id}"));
                match expr {
                    Expr::Literal(Literal::Int(value)) => Value::Int(*value),
                    _ => panic!("unexpected test expression"),
                }
            })
        }

        fn bind<'b>(
            &'b mut self,
            _pattern: &'b Pattern,
            _expr: &'b Expr,
            _node_id: &'b str,
        ) -> HostFuture<'b, StatementExecution<Value<(), &'static str>, &'static str>> {
            Box::pin(async { panic!("unexpected test binding") })
        }

        fn run_body<'b>(
            &'b mut self,
            _body: &'b [Stmt],
            _node_id: &'b str,
        ) -> HostFuture<'b, StatementOutcome<Value<(), &'static str>, &'static str>> {
            Box::pin(async { panic!("unexpected test branch") })
        }

        fn run_loop<'b>(
            &'b mut self,
            _body: &'b [Stmt],
            _node_id: &'b str,
        ) -> HostFuture<'b, StatementExecution<Value<(), &'static str>, &'static str>> {
            Box::pin(async { panic!("unexpected test loop") })
        }

        fn preview(&self, _value: &Value<(), &'static str>) -> Option<String> {
            None
        }

        fn node_end(
            &mut self,
            node_id: &str,
            _outcome: &StatementOutcome<Value<(), &'static str>, Self::Error>,
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
                "execute:branch.0",
                "end:branch.0:",
                "start:branch.1",
                "execute:branch.1",
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
        assert!(matches!(result, StatementOutcome::Err("cancelled")));
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
