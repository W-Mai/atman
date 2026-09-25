use alloc::{boxed::Box, format, string::String};
use core::{future::Future, pin::Pin};

use crate::ast::Stmt;

pub type HostFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
pub type StatementExecution<V, E> = (StatementOutcome<V, E>, Option<String>);

pub enum StatementOutcome<V, E> {
    Continue,
    Return(V),
    Err(E),
    LoopBreak,
    LoopContinue,
}

pub enum Preflight<E> {
    Continue,
    Stop(E),
    StopAfterNode { error: E, preview: String },
}

/// Supplies product-specific execution, cancellation decisions, and event sinks.
pub trait StatementHost: Send {
    type Value: Send;
    type Error: Send;

    fn preflight(&mut self, stmt: &Stmt, node_id: &str) -> Preflight<Self::Error>;
    fn node_start(&mut self, stmt: &Stmt, node_id: &str, parent_node_id: Option<&str>);
    fn execute<'a>(
        &'a mut self,
        stmt: &'a Stmt,
        node_id: &'a str,
    ) -> HostFuture<'a, StatementExecution<Self::Value, Self::Error>>;
    fn node_end(
        &mut self,
        node_id: &str,
        outcome: &StatementOutcome<Self::Value, Self::Error>,
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

    pub async fn run_statements(
        &mut self,
        stmts: &[Stmt],
        prefix: &str,
        parent_node_id: Option<&str>,
    ) -> StatementOutcome<H::Value, H::Error> {
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
            let (outcome, preview) = self.host.execute(stmt, &node_id).await;
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

    struct TestHost<'a> {
        events: &'a mut Vec<String>,
        stop_before: bool,
    }

    impl StatementHost for TestHost<'_> {
        type Value = i32;
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

        fn node_start(&mut self, _stmt: &Stmt, node_id: &str, _parent_node_id: Option<&str>) {
            self.events.push(alloc::format!("start:{node_id}"));
        }

        fn execute<'b>(
            &'b mut self,
            _stmt: &'b Stmt,
            node_id: &'b str,
        ) -> HostFuture<'b, StatementExecution<Self::Value, Self::Error>> {
            Box::pin(async move {
                self.events.push(alloc::format!("execute:{node_id}"));
                let outcome = if node_id == "branch.1" {
                    StatementOutcome::Return(7)
                } else {
                    StatementOutcome::Continue
                };
                (outcome, None)
            })
        }

        fn node_end(
            &mut self,
            node_id: &str,
            _outcome: &StatementOutcome<Self::Value, Self::Error>,
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
                &[Stmt::Break, Stmt::Break, Stmt::Break],
                "branch",
                None,
            ))
        };
        assert!(matches!(result, StatementOutcome::Return(7)));
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
}
