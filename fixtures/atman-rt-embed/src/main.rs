use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

use atman_rt::{
    Engine, Env, EvalError, ExpressionEffect, ExpressionHost, FlowExecution, FlowOutcome,
    HostFuture, LoopExit, LoopHost, PatternBindError, Preflight, StatementHost, StatementOutcome,
    Value,
    ast::{BinOp, Expr, FlowDecl, Ident, Literal, Node, ParamDecl, Pattern, Span, Stmt, TypeExpr},
    bind_pattern, eval_expr, run_loop,
};

type FixtureValue = Value<(), EvalError>;
type FixtureOutcome = FlowOutcome<(), EvalError>;

#[derive(Clone)]
struct FixtureHost {
    env: Arc<Mutex<Env<FixtureValue>>>,
    effect_seen: Arc<AtomicBool>,
}

impl FixtureHost {
    fn new() -> Self {
        Self {
            env: Arc::new(Mutex::new(Env::new())),
            effect_seen: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl ExpressionHost for FixtureHost {
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
        _piped: FixtureValue,
        _env: &'a Env<FixtureValue>,
    ) -> HostFuture<'a, FixtureValue> {
        Box::pin(async { panic!("unexpected pipe in embedding fixture") })
    }

    fn eval_external<'a>(
        &'a self,
        effect: ExpressionEffect<'a>,
        _env: &'a Env<FixtureValue>,
    ) -> HostFuture<'a, FixtureValue> {
        Box::pin(async move {
            match effect {
                ExpressionEffect::Node(Node::ToolCall { path, .. })
                    if path.len() == 1 && path[0].name == "foreign" =>
                {
                    self.effect_seen.store(true, Ordering::SeqCst);
                    Value::Int(5)
                }
                _ => Value::Err(EvalError::TypeMismatch {
                    expected: "foreign tool call".into(),
                    actual: "unsupported effect".into(),
                }),
            }
        })
    }
}

impl StatementHost for FixtureHost {
    type Payload = ();
    type Error = EvalError;

    fn preflight(&mut self, _stmt: &Stmt, _node_id: &str) -> Preflight<EvalError> {
        Preflight::Continue
    }

    fn bind_parameter(&mut self, name: String, value: FixtureValue) {
        self.env.lock().unwrap().bind(name, value);
    }

    fn evaluate_default<'a>(&'a mut self, expr: &'a Expr) -> HostFuture<'a, FixtureValue> {
        self.evaluate(expr, "")
    }

    fn node_start(&mut self, _stmt: &Stmt, _node_id: &str, _parent: Option<&str>) {}

    fn evaluate<'a>(
        &'a mut self,
        expr: &'a Expr,
        _node_id: &'a str,
    ) -> HostFuture<'a, FixtureValue> {
        Box::pin(async move {
            let env = self.env.lock().unwrap().clone();
            eval_expr(expr, &env, self).await
        })
    }

    fn bind<'a>(
        &'a mut self,
        pattern: &'a Pattern,
        expr: &'a Expr,
        node_id: &'a str,
    ) -> HostFuture<'a, FlowExecution<(), EvalError>> {
        Box::pin(async move {
            let value = self.evaluate(expr, node_id).await;
            if let Value::Err(error) = value {
                return (StatementOutcome::Err(error), None);
            }
            let mut env = self.env.lock().unwrap();
            match bind_pattern(pattern, value, &mut env) {
                Ok(()) => (StatementOutcome::Continue, None),
                Err(error) => {
                    let actual = match error {
                        PatternBindError::NonStruct { actual } => actual,
                        PatternBindError::MissingField { name } => name,
                    };
                    (
                        StatementOutcome::Err(EvalError::TypeMismatch {
                            expected: "matching pattern".into(),
                            actual,
                        }),
                        None,
                    )
                }
            }
        })
    }

    fn run_body<'a>(
        &'a mut self,
        body: &'a [Stmt],
        node_id: &'a str,
    ) -> HostFuture<'a, FixtureOutcome> {
        Box::pin(async move {
            Engine::new(self.clone())
                .run_statements(body, node_id, Some(node_id))
                .await
        })
    }

    fn run_loop<'a>(
        &'a mut self,
        body: &'a [Stmt],
        node_id: &'a str,
    ) -> HostFuture<'a, FlowExecution<(), EvalError>> {
        Box::pin(async move {
            let mut host = FixtureLoopHost { host: self, body };
            match run_loop(&mut host, Some(node_id)).await {
                LoopExit::Break => (StatementOutcome::Continue, None),
                LoopExit::Interrupted(outcome) => (outcome, None),
            }
        })
    }

    fn preview(&self, _value: &FixtureValue) -> Option<String> {
        None
    }

    fn node_end(
        &mut self,
        _node_id: &str,
        _outcome: &FixtureOutcome,
        _parent: Option<&str>,
        _preview: Option<&str>,
    ) {
    }
}

struct FixtureLoopHost<'a> {
    host: &'a mut FixtureHost,
    body: &'a [Stmt],
}

impl LoopHost for FixtureLoopHost<'_> {
    type Value = FixtureValue;
    type Error = EvalError;

    fn iteration_start(&mut self, _iteration: u64, _node_id: &str, _parent: Option<&str>) {}

    fn execute_iteration<'a>(&'a mut self, node_id: &'a str) -> HostFuture<'a, FixtureOutcome> {
        self.host.run_body(self.body, node_id)
    }

    fn iteration_end(&mut self, _node_id: &str, _outcome: &FixtureOutcome, _parent: Option<&str>) {}
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(NoopWake));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match Pin::as_mut(&mut future).poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => std::thread::yield_now(),
        }
    }
}

fn ident(name: &str) -> Ident {
    Ident::new(name, Span::default())
}

fn literal(value: i64) -> Expr {
    Expr::Literal(Literal::Int(value))
}

fn main() {
    let pure = FlowDecl {
        name: ident("pure"),
        params: vec![ParamDecl {
            name: ident("x"),
            ty: TypeExpr::Named(ident("Int")),
            default: Some(literal(7)),
        }],
        ret: None,
        contract: None,
        body: vec![Stmt::When {
            cond: Expr::Literal(Literal::Bool(true)),
            body: vec![Stmt::Return {
                value: Expr::Binary {
                    op: BinOp::Add,
                    left: Box::new(Expr::Ident(ident("x"))),
                    right: Box::new(literal(2)),
                },
            }],
        }],
    };
    let mut engine = Engine::new(FixtureHost::new());
    assert!(matches!(
        block_on(engine.run_flow(&pure, vec![])),
        StatementOutcome::Return(Value::Int(9))
    ));

    let effect = FlowDecl {
        name: ident("effect"),
        params: vec![],
        ret: None,
        contract: None,
        body: vec![Stmt::Return {
            value: Expr::Binary {
                op: BinOp::Add,
                left: Box::new(Expr::Ident(ident("y"))),
                right: Box::new(Expr::Node(Node::ToolCall {
                    path: vec![ident("foreign")],
                    args: vec![],
                })),
            },
        }],
    };
    let host = FixtureHost::new();
    let seen = Arc::clone(&host.effect_seen);
    let mut engine = Engine::new(host);
    assert!(matches!(
        block_on(engine.run_flow(&effect, vec![("y".into(), Value::Int(7))])),
        StatementOutcome::Return(Value::Int(12))
    ));
    assert!(seen.load(Ordering::SeqCst));

    let loop_flow = FlowDecl {
        name: ident("loop"),
        params: vec![],
        ret: None,
        contract: None,
        body: vec![
            Stmt::Loop {
                body: vec![Stmt::Break],
            },
            Stmt::Return { value: literal(1) },
        ],
    };
    let mut engine = Engine::new(FixtureHost::new());
    assert!(matches!(
        block_on(engine.run_flow(&loop_flow, vec![])),
        StatementOutcome::Return(Value::Int(1))
    ));
}
