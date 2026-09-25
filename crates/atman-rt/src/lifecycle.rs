use alloc::string::String;

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
    use alloc::string::ToString;

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
}
