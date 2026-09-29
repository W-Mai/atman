use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    task::{Context, Poll, Wake, Waker},
};

use atman_rt::{
    Engine, EvalError, ExpressionEffect, ExpressionHost, HostFuture, PatternBindError, Preflight,
    Source, SourceResolver, StatementHost, StatementOutcome, ToolRouter, Value, Vm, VmDelegates,
    ast::{Arg, BinOp, Expr, FlowDecl, Ident, Literal, Node, ParamDecl, Span, Stmt, TypeExpr},
};

type FixtureValue = Value<(), EvalError>;

struct FixtureSources;

impl SourceResolver for FixtureSources {
    type Error = &'static str;

    fn resolve(&self, importer_id: &str, specifier: &str) -> Result<Source, Self::Error> {
        match (importer_id, specifier) {
            ("demo.at", "helper.at") => Ok(Source::new("helper.at", include_str!("helper.at"))),
            _ => Err("unknown source"),
        }
    }
}

#[derive(Clone)]
struct FixtureHost {
    effect_seen: Arc<AtomicBool>,
}

impl FixtureHost {
    fn new() -> Self {
        Self {
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

    fn eval_external<'a>(
        &'a self,
        effect: ExpressionEffect<(), EvalError>,
    ) -> HostFuture<'a, FixtureValue> {
        Box::pin(async move {
            match effect {
                ExpressionEffect::ToolCall { name, .. } if name == "foreign" => {
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
    type ExprHost = Self;
    type NodeScope = ();
    type IterationScope = ();

    fn preflight(
        &mut self,
        _stmt: &Stmt,
        _node_id: &str,
        _parent: Option<&str>,
    ) -> Preflight<EvalError> {
        Preflight::Continue
    }

    fn node_start(
        &mut self,
        _stmt: &Stmt,
        _node_id: &str,
        _parent: Option<&str>,
    ) -> Self::NodeScope {
    }

    fn expression_host(
        &self,
        _node_id: Option<&str>,
        _parent: Option<&str>,
    ) -> Self::ExprHost {
        self.clone()
    }

    fn pattern_error(&self, error: PatternBindError) -> EvalError {
        EvalError::TypeMismatch {
            expected: "matching pattern".into(),
            actual: format!("{error:?}"),
        }
    }

    fn preview(
        &self,
        _value: &FixtureValue,
        _node_id: &str,
        _parent_node_id: Option<&str>,
    ) -> Option<String> {
        None
    }

    fn iteration_start(
        &mut self,
        _iteration: u64,
        _node_id: &str,
        _parent: Option<&str>,
    ) -> Self::IterationScope {
    }
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
    if std::env::args().nth(1).as_deref() == Some("--demo") {
        if let Err(error) = run_demo() {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }

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

    let portable_nodes = FlowDecl {
        name: ident("portable_nodes"),
        params: vec![],
        ret: None,
        contract: None,
        body: vec![Stmt::Return {
            value: Expr::Node(Node::DynamicFanout {
                source: Box::new(Expr::Node(Node::ToolCall {
                    path: vec![ident("list"), ident("map")],
                    args: vec![
                        Arg::Positional(Expr::List(vec![literal(1), literal(2)])),
                        Arg::Positional(Expr::Lambda {
                            params: vec![ident("item")],
                            body: Box::new(Expr::Binary {
                                op: BinOp::Mul,
                                left: Box::new(Expr::Ident(ident("item"))),
                                right: Box::new(literal(2)),
                            }),
                        }),
                    ],
                })),
                lambda: Box::new(Expr::Lambda {
                    params: vec![ident("item")],
                    body: Box::new(Expr::Binary {
                        op: BinOp::Add,
                        left: Box::new(Expr::Ident(ident("item"))),
                        right: Box::new(literal(1)),
                    }),
                }),
            }),
        }],
    };
    let mut engine = Engine::new(FixtureHost::new());
    assert!(matches!(
        block_on(engine.run_flow(&portable_nodes, vec![])),
        StatementOutcome::Return(Value::List(items))
            if matches!(&items[..], [Value::Int(3), Value::Int(5)])
    ));

    let static_fanout = FlowDecl {
        name: ident("static_fanout"),
        params: vec![],
        ret: None,
        contract: None,
        body: vec![Stmt::Return {
            value: Expr::Node(Node::Fanout {
                source: Box::new(Expr::List(vec![
                    Expr::Node(Node::ToolCall {
                        path: vec![ident("foreign")],
                        args: vec![],
                    }),
                    literal(2),
                ])),
            }),
        }],
    };
    let mut engine = Engine::new(FixtureHost::new());
    assert!(matches!(
        block_on(engine.run_flow(&static_fanout, vec![])),
        StatementOutcome::Return(Value::List(items))
            if matches!(&items[..], [Value::Int(5), Value::Int(2)])
    ));

    let loop_flow = FlowDecl {
        name: ident("loop"),
        params: vec![],
        ret: None,
        contract: None,
        body: vec![
            Stmt::Loop {
                body: vec![Stmt::Break],
            },
            Stmt::Yield,
            Stmt::Return { value: literal(1) },
        ],
    };
    let mut engine = Engine::new(FixtureHost::new());
    assert!(matches!(
        block_on(engine.run_flow(&loop_flow, vec![])),
        StatementOutcome::Return(Value::Int(1))
    ));
}

fn run_demo() -> Result<(), String> {
    use std::io::{self, Write};

    let mut args = std::env::args().skip(2);
    let input = match args.next() {
        Some(value) => value,
        None => {
            print!("Enter an integer: ");
            io::stdout().flush().map_err(|error| error.to_string())?;
            let mut value = String::new();
            io::stdin()
                .read_line(&mut value)
                .map_err(|error| error.to_string())?;
            value.trim().to_owned()
        }
    };
    if args.next().is_some() {
        return Err("usage: --demo [integer]".into());
    }
    let input = input
        .parse::<i64>()
        .map_err(|_| "input must be an integer".to_string())?;

    let vm = Vm::compile(
        Source::new("demo.at", include_str!("demo.at")),
        &FixtureSources,
    )
    .map_err(|error| format!("invalid demo program: {error}"))?;
    let mut tools = ToolRouter::<(), EvalError>::new();
    tools
        .register("foreign", |_| async { Ok(Value::Int(5)) })
        .map_err(|error| error.to_string())?;
    match block_on(vm.run(
        "demo",
        vec![("input".into(), Value::Int(input))],
        VmDelegates::new(tools),
    )) {
        StatementOutcome::Return(Value::Int(value)) => {
            println!("result: {value}");
            Ok(())
        }
        StatementOutcome::Err(error) => Err(format!("flow failed: {error:?}")),
        _ => Err("flow did not return an integer".into()),
    }
}
