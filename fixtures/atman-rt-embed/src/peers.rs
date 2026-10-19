//! In-memory host policy over portable instance and inbox mechanisms.

use atman_rt::{
    EvalError, FlowInstance, FlowInstanceId, FlowRef, InstanceError, RunId, Source,
    StatementOutcome, ToolRouter, TurnId, TurnLease, TurnOutcome, TurnPhase, Value, Vm,
    VmDelegates, inbox::PendingBatch,
};

struct Peer {
    state: FlowInstance<u64>,
    inbox: PendingBatch<i64>,
    inputs: Vec<Vec<i64>>,
    turns: u64,
}

impl Peer {
    fn new(id: u64) -> Self {
        let mut state = FlowInstance::prepared(FlowRef {
            session_id: 1,
            instance_id: FlowInstanceId(id),
        });
        state.activate().unwrap();
        Self {
            state,
            inbox: PendingBatch::default(),
            inputs: Vec::new(),
            turns: 0,
        }
    }

    fn receive(&mut self, value: i64) {
        assert!(self.state.accepts_input());
        self.inbox.push_back(value);
    }

    fn drive(&mut self, vm: &Vm) -> Option<(TurnLease<u64>, i64)> {
        if !self.state.can_start_turn() {
            return None;
        }
        let batch = self.inbox.reserve()?;
        let input = batch.items().iter().sum();
        self.turns += 1;
        let lease = self
            .state
            .begin_turn(TurnId(self.turns), RunId(self.turns))
            .unwrap();
        // This demo records input in memory. A durable host commits input and admission together.
        assert!(
            self.inbox
                .commit(&batch, || {
                    self.inputs.push(batch.items().to_vec());
                    Ok::<_, std::convert::Infallible>(())
                })
                .unwrap()
        );
        let outcome = super::block_on(vm.run(
            "handle",
            vec![("input".into(), Value::Int(input))],
            VmDelegates::new(ToolRouter::<(), EvalError>::new()),
        ));
        let StatementOutcome::Return(Value::Int(answer)) = outcome else {
            panic!("peer entry did not return an integer");
        };
        self.state.set_phase(&lease, TurnPhase::Finishing).unwrap();
        self.state
            .finish_turn(&lease, TurnOutcome::Completed)
            .unwrap();
        println!(
            "peer {}: turn {}, batch {:?}, answer {}, idle",
            self.state.address().instance_id,
            self.turns,
            batch.items(),
            answer
        );
        Some((lease, answer))
    }
}

pub fn run() {
    let vm = Vm::compile(
        Source::new("peers.at", include_str!("peers.at")),
        &super::FixtureSources,
    )
    .unwrap();
    let mut a = Peer::new(1);
    let mut b = Peer::new(2);
    a.receive(7);
    let (old_lease, answer) = a.drive(&vm).unwrap();
    assert_eq!(answer, 8);

    b.state.pause(10).unwrap();
    b.state.pause(20).unwrap();
    b.receive(answer);
    b.receive(10);
    assert!(b.drive(&vm).is_none());
    assert_eq!(b.inbox.len(), 2);
    b.state.resume(&10).unwrap();
    assert!(b.drive(&vm).is_none());
    b.state.resume(&20).unwrap();
    let (_, reply) = b.drive(&vm).unwrap();
    assert_eq!(b.inputs, [vec![8, 10]]);
    assert!(b.drive(&vm).is_none());

    a.receive(reply);
    assert_eq!(a.drive(&vm).unwrap().1, 20);
    assert_eq!(
        a.state.finish_turn(&old_lease, TurnOutcome::Completed),
        Err(InstanceError::StaleLease)
    );
    b.state.begin_close().unwrap();
    b.state.finish_close().unwrap();
    assert!(!b.state.accepts_input());
    a.receive(5);
    assert_eq!(a.drive(&vm).unwrap().1, 6);
    println!("peer 2 closed; peer 1 remains available; no runtime or storage dependency");
}
