use atman_runtime::event::{Event, EventSink, FlowRunId, FlowStatus, TurnId};

fn make_flow_start() -> Event {
    Event::FlowStart {
        seq: 0,
        run_id: FlowRunId::now(),
        flow_name: "t".into(),
        ts: chrono::Utc::now(),
    }
}

fn make_flow_end() -> Event {
    Event::FlowEnd {
        seq: 0,
        run_id: FlowRunId::now(),
        flow_name: "t".into(),
        status: FlowStatus::Ok,
        ts: chrono::Utc::now(),
    }
}

fn make_turn_start() -> Event {
    Event::TurnStart {
        seq: 0,
        turn_id: TurnId::now(),
        ts: chrono::Utc::now(),
    }
}

#[test]
fn event_sink_patches_monotonic_seq_across_variants() {
    let sink = EventSink::new();
    sink.emit(make_flow_start());
    sink.emit(make_turn_start());
    sink.emit(make_flow_end());
    let snap = sink.snapshot();
    assert_eq!(snap.len(), 3);
    assert_eq!(snap[0].seq(), 1);
    assert_eq!(snap[1].seq(), 2);
    assert_eq!(snap[2].seq(), 3);
}

#[test]
fn cloned_sink_shares_counter_state() {
    let sink1 = EventSink::new();
    let sink2 = sink1.clone();
    sink1.emit(make_flow_start());
    sink2.emit(make_flow_end());
    let snap1 = sink1.snapshot();
    let snap2 = sink2.snapshot();
    assert_eq!(
        snap1.len(),
        2,
        "cloned sink shares Vec so both entries land here"
    );
    assert_eq!(snap1[0].seq(), 1);
    assert_eq!(snap1[1].seq(), 2);
    assert_eq!(snap2.len(), snap1.len(), "same underlying Arc<Mutex<Vec>>");
}

#[test]
fn seq_is_serialized_to_json() {
    let sink = EventSink::new();
    sink.emit(make_flow_start());
    let snap = sink.snapshot();
    let json = serde_json::to_value(&snap[0]).unwrap();
    assert_eq!(json["seq"], serde_json::json!(1));
    assert_eq!(json["type"], "flow_start");
}
