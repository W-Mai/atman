use alloc::{format, string::String, vec::Vec};
use core::future::Future;

use crate::ast::{File, FlowDecl, Ident, LifecycleDecl, LifecycleEvent};

/// Selects lifecycle bodies in their source declaration order.
pub fn lifecycle_hooks(
    file: &File,
    event: LifecycleEvent,
) -> impl Iterator<Item = (usize, &LifecycleDecl)> {
    file.lifecycles
        .iter()
        .enumerate()
        .filter(move |(_, hook)| hook.event == event)
}

/// Turns one `on` body into an executable flow without a host-side AST rewrite.
pub fn lifecycle_flow(hook: &LifecycleDecl, index: usize) -> FlowDecl {
    FlowDecl {
        name: Ident::new(
            format!("__lifecycle_{}_{}", lifecycle_event_slug(hook.event), index),
            hook.span,
        ),
        params: Vec::new(),
        ret: None,
        contract: None,
        body: hook.body.clone(),
    }
}

pub fn lifecycle_event_slug(event: LifecycleEvent) -> &'static str {
    match event {
        LifecycleEvent::SessionStart => "session.start",
        LifecycleEvent::SessionEnd => "session.end",
        LifecycleEvent::TurnStart => "turn.start",
        LifecycleEvent::TurnEnd => "turn.end",
        LifecycleEvent::ContextCompact => "session.context_compact",
    }
}

/// A sequence-independent flow start fact supplied to the embedding host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlowStartFact<R, N> {
    pub run_id: R,
    pub flow_name: String,
    pub parent_run_id: Option<R>,
    pub parent_node_id: Option<N>,
    pub spawned: bool,
}

/// A sequence-independent flow end fact supplied to the embedding host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FlowEndFact<R, S> {
    pub run_id: R,
    pub flow_name: String,
    pub status: S,
}

/// Starts one flow and retains its identity until the terminal fact is formed.
pub struct FlowLifecycle<R, N> {
    start: FlowStartFact<R, N>,
}

/// A flow that has produced its start fact and can produce one end fact.
pub struct StartedFlow<R> {
    run_id: R,
    flow_name: String,
}

impl<R: Clone, N> FlowLifecycle<R, N> {
    pub fn new(start: FlowStartFact<R, N>) -> Self {
        Self { start }
    }

    pub fn start(self) -> (StartedFlow<R>, FlowStartFact<R, N>) {
        let running = StartedFlow {
            run_id: self.start.run_id.clone(),
            flow_name: self.start.flow_name.clone(),
        };
        (running, self.start)
    }

    /// Emits start, executes the flow, then emits one terminal fact on completion.
    pub async fn run<T, S, Begin, Execute, Fut, Classify, Finish>(
        self,
        begin: Begin,
        execute: Execute,
        classify: Classify,
        finish: Finish,
    ) -> T
    where
        Begin: FnOnce(FlowStartFact<R, N>),
        Execute: FnOnce() -> Fut,
        Fut: Future<Output = T>,
        Classify: FnOnce(&T) -> S,
        Finish: FnOnce(FlowEndFact<R, S>, &T),
    {
        let (running, start) = self.start();
        begin(start);
        let output = execute().await;
        finish(running.finish(classify(&output)), &output);
        output
    }
}

impl<R> StartedFlow<R> {
    pub fn finish<S>(self, status: S) -> FlowEndFact<R, S> {
        FlowEndFact {
            run_id: self.run_id,
            flow_name: self.flow_name,
            status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::{rc::Rc, string::ToString, vec, vec::Vec};
    use core::{
        cell::RefCell,
        future::Future,
        pin::pin,
        task::{Context, Poll, Waker},
    };

    fn run_ready<F: Future>(future: F) -> F::Output {
        let mut future = pin!(future);
        match future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(value) => value,
            Poll::Pending => panic!("test flow must complete synchronously"),
        }
    }

    #[test]
    fn start_and_end_keep_one_flow_identity() {
        let (running, start) = FlowLifecycle::new(FlowStartFact {
            run_id: 7,
            flow_name: "child".to_string(),
            parent_run_id: Some(3),
            parent_node_id: Some("1.2".to_string()),
            spawned: false,
        })
        .start();
        assert_eq!(start.parent_run_id, Some(3));
        assert_eq!(start.parent_node_id.as_deref(), Some("1.2"));
        let end = running.finish("ok");
        assert_eq!(
            (end.run_id, end.flow_name.as_str(), end.status),
            (7, "child", "ok")
        );
    }

    #[test]
    fn lifecycle_orders_start_body_and_one_end() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let begin_events = Rc::clone(&events);
        let body_events = Rc::clone(&events);
        let finish_events = Rc::clone(&events);
        let result = run_ready(
            FlowLifecycle::new(FlowStartFact {
                run_id: 7,
                flow_name: "root".to_string(),
                parent_run_id: None,
                parent_node_id: None::<String>,
                spawned: false,
            })
            .run(
                move |start| {
                    assert_eq!(start.run_id, 7);
                    begin_events.borrow_mut().push("start");
                },
                move || async move {
                    body_events.borrow_mut().push("body");
                    42
                },
                |result| *result == 42,
                move |end, result| {
                    assert_eq!(
                        (end.run_id, end.flow_name.as_str(), end.status, *result),
                        (7, "root", true, 42)
                    );
                    finish_events.borrow_mut().push("end");
                },
            ),
        );
        assert_eq!(result, 42);
        assert_eq!(*events.borrow(), vec!["start", "body", "end"]);
    }

    #[test]
    fn lifecycle_finishes_when_body_returns_error() {
        let events = Rc::new(RefCell::new(Vec::new()));
        let begin_events = Rc::clone(&events);
        let finish_events = Rc::clone(&events);
        let result = run_ready(
            FlowLifecycle::new(FlowStartFact {
                run_id: 7,
                flow_name: "child".to_string(),
                parent_run_id: None,
                parent_node_id: None::<String>,
                spawned: true,
            })
            .run(
                move |_| begin_events.borrow_mut().push("start"),
                || async { Err::<(), _>("context initialization failed") },
                |result| result.is_ok(),
                move |end, _| {
                    assert_eq!((end.run_id, end.status), (7, false));
                    finish_events.borrow_mut().push("end");
                },
            ),
        );
        assert_eq!(result, Err("context initialization failed"));
        assert_eq!(*events.borrow(), vec!["start", "end"]);
    }
}
