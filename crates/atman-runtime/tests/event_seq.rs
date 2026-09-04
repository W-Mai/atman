use atman_runtime::event::{Event, EventSink, FlowRunId, FlowStatus, TurnId};

fn make_flow_start() -> Event {
    Event::FlowStart {
        turn_id: None,
        run_id: FlowRunId::now(),
        flow_name: "t".into(),
        parent_run_id: None,
        parent_node_id: None,
        spawned: false,
    }
}

fn make_flow_end() -> Event {
    Event::FlowEnd {
        run_id: FlowRunId::now(),
        flow_name: "t".into(),
        status: FlowStatus::Ok,
    }
}

fn make_turn_start() -> Event {
    Event::TurnStart {
        turn_id: TurnId::now(),
    }
}

#[test]
fn event_sink_patches_monotonic_seq_across_variants() {
    let sink = EventSink::new();
    sink.emit(make_flow_start());
    sink.emit(make_turn_start());
    sink.emit(make_flow_end());
    let snap = sink.snapshot_envelopes();
    assert_eq!(snap.len(), 3);
    assert_eq!(snap[0].seq, 1);
    assert_eq!(snap[1].seq, 2);
    assert_eq!(snap[2].seq, 3);
}

#[test]
fn cloned_sink_shares_counter_state() {
    let sink1 = EventSink::new();
    let sink2 = sink1.clone();
    sink1.emit(make_flow_start());
    sink2.emit(make_flow_end());
    let snap1 = sink1.snapshot_envelopes();
    let snap2 = sink2.snapshot_envelopes();
    assert_eq!(
        snap1.len(),
        2,
        "cloned sink shares Vec so both entries land here"
    );
    assert_eq!(snap1[0].seq, 1);
    assert_eq!(snap1[1].seq, 2);
    assert_eq!(snap2.len(), snap1.len(), "same underlying Arc<Mutex<Vec>>");
}

#[test]
fn cloned_sink_publishes_the_persisted_envelope_sequence() {
    let sink = EventSink::new();
    let mut receiver = sink.clone().subscribe();
    sink.emit(make_flow_start());
    sink.emit(make_flow_end());

    let first = receiver.try_recv().unwrap();
    let second = receiver.try_recv().unwrap();
    assert_eq!((first.seq, second.seq), (1, 2));
    assert_eq!(
        sink.snapshot_envelopes()
            .into_iter()
            .map(|event| event.seq)
            .collect::<Vec<_>>(),
        vec![first.seq, second.seq]
    );
}

#[test]
fn concurrent_emitters_publish_in_persisted_sequence_order() {
    const WORKERS: usize = 8;
    const EVENTS_PER_WORKER: usize = 100;

    let sink = EventSink::new();
    let mut receiver = sink.subscribe();
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(WORKERS));
    let workers = (0..WORKERS)
        .map(|_| {
            let sink = sink.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                for _ in 0..EVENTS_PER_WORKER {
                    sink.emit(make_turn_start());
                }
            })
        })
        .collect::<Vec<_>>();
    let mut published = Vec::new();
    while workers.iter().any(|worker| !worker.is_finished()) {
        let target = sink.published_seq();
        while published.last().copied().unwrap_or(0) < target {
            published.push(
                receiver
                    .try_recv()
                    .expect("published event must be available")
                    .seq,
            );
        }
        std::thread::yield_now();
    }
    for worker in workers {
        worker.join().unwrap();
    }

    let expected = (1..=(WORKERS * EVENTS_PER_WORKER) as u64).collect::<Vec<_>>();
    let persisted = sink
        .snapshot_envelopes()
        .into_iter()
        .map(|event| event.seq)
        .collect::<Vec<_>>();
    published.extend(std::iter::from_fn(|| receiver.try_recv().ok()).map(|event| event.seq));
    assert_eq!(persisted, expected);
    assert_eq!(published, expected);
}

#[test]
fn next_seq_peek_does_not_advance_counter() {
    let sink = EventSink::new();
    let a = sink.next_seq_peek();
    let b = sink.next_seq_peek();
    let c = sink.next_seq_peek();
    assert_eq!(a, 1);
    assert_eq!(b, 1);
    assert_eq!(
        c, 1,
        "peek must be idempotent — anchor labels rely on this being non-reserving"
    );
    sink.emit(make_flow_start());
    assert_eq!(sink.next_seq_peek(), 2);
}

#[test]
fn reserve_seq_advances_counter_atomically() {
    let sink = EventSink::new();
    let a = sink.reserve_seq();
    let b = sink.reserve_seq();
    let c = sink.reserve_seq();
    assert_eq!((a, b, c), (1, 2, 3));
    assert_eq!(sink.published_seq(), 0);
    assert_eq!(
        sink.next_seq_peek(),
        4,
        "peek after 3 reservations must see counter=3, next=4"
    );
    sink.emit(make_flow_start());
    assert_eq!(sink.published_seq(), 4);
    sink.reserve_seq();
    assert_eq!(sink.clone().published_seq(), 4);
    sink.drain();
    assert_eq!(sink.published_seq(), 4);
    sink.restore_seq(20);
    assert_eq!(sink.published_seq(), 20);
    sink.reserve_seq();
    assert_eq!(sink.published_seq(), 20);
    sink.emit(make_flow_end());
    assert_eq!(sink.published_seq(), 22);
}

#[test]
fn seq_is_serialized_to_json() {
    let sink = EventSink::new();
    sink.emit(make_flow_start());
    let snap = sink.snapshot_envelopes();
    let json = serde_json::to_value(&snap[0]).unwrap();
    assert_eq!(json["seq"], serde_json::json!(1));
    assert_eq!(json["type"], "flow_start");
}
