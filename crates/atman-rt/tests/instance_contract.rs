use atman_rt::{
    FlowInstance, FlowInstanceId, FlowRef, InstanceActivity, InstanceError, InstanceState,
    Interruption, RunId, TurnId, TurnOutcome, TurnPhase, WaitReason,
};

fn instance(id: u64) -> FlowInstance<u64> {
    FlowInstance::prepared(FlowRef {
        session_id: 1,
        instance_id: FlowInstanceId::new(id),
    })
}

#[test]
fn persistent_identity_outlives_turns_and_rejects_stale_or_foreign_callbacks() {
    let mut a = instance(10);
    let mut b = instance(11);
    assert!(!a.accepts_input());
    assert!(!a.can_start_turn());
    a.activate().unwrap();
    b.activate().unwrap();
    let address = a.address().clone();
    let first = a.begin_turn(TurnId::new(20), RunId::new(30)).unwrap();
    let foreign = b.begin_turn(TurnId::new(20), RunId::new(30)).unwrap();
    assert_eq!(
        a.finish_turn(&foreign, TurnOutcome::Completed),
        Err(InstanceError::StaleLease)
    );
    assert_eq!(
        a.finish_turn(&first, TurnOutcome::Completed),
        Ok(TurnOutcome::Completed)
    );
    assert!(a.can_start_turn());
    assert_eq!(a.address(), &address);
    // Epochs fence old callbacks even if a host accidentally repeats external IDs.
    let next = a.begin_turn(TurnId::new(20), RunId::new(30)).unwrap();
    assert_ne!(first.epoch, next.epoch);
    let before = a.clone();
    assert_eq!(
        a.finish_turn(&first, TurnOutcome::Completed),
        Err(InstanceError::StaleLease)
    );
    assert_eq!(a, before);
    a.finish_turn(&next, TurnOutcome::Completed).unwrap();
    assert!(matches!(
        a.state(),
        InstanceState::Open(InstanceActivity::Idle)
    ));
}

#[test]
fn independent_pauses_preserve_reception_and_require_all_owners_to_resume() {
    let mut flow = instance(10);
    flow.activate().unwrap();
    let lease = flow.begin_turn(TurnId::new(20), RunId::new(30)).unwrap();
    flow.pause(100).unwrap();
    let revision = flow.revision();
    flow.pause(100).unwrap();
    assert_eq!(flow.revision(), revision);
    flow.pause(200).unwrap();
    assert!(flow.accepts_input());
    assert!(!flow.can_receive_at_boundary(&lease).unwrap());
    flow.set_phase(&lease, TurnPhase::Waiting(WaitReason::ExecutionGate))
        .unwrap();
    flow.resume(&100).unwrap();
    assert_eq!(
        flow.set_phase(&lease, TurnPhase::Running),
        Err(InstanceError::Paused)
    );
    flow.resume(&200).unwrap();
    flow.set_phase(&lease, TurnPhase::Running).unwrap();
    assert!(flow.can_receive_at_boundary(&lease).unwrap());
    flow.finish_turn(&lease, TurnOutcome::Completed).unwrap();
    flow.pause(100).unwrap();
    assert_eq!(
        flow.begin_turn(TurnId::new(21), RunId::new(31)),
        Err(InstanceError::Paused)
    );
    assert!(flow.accepts_input());
}

#[test]
fn interruptions_require_explicit_recovery_without_releasing_other_controls() {
    for reason in [
        Interruption::Failed,
        Interruption::Cancelled,
        Interruption::HostLost,
    ] {
        let mut flow = instance(10);
        flow.activate().unwrap();
        let lease = flow.begin_turn(TurnId::new(20), RunId::new(30)).unwrap();
        flow.finish_turn(&lease, TurnOutcome::Interrupted(reason))
            .unwrap();
        assert!(flow.accepts_input());
        assert!(!flow.can_start_turn());
        assert_eq!(
            flow.begin_turn(TurnId::new(21), RunId::new(31)),
            Err(InstanceError::InvalidTransition)
        );
        flow.pause(100).unwrap();
        flow.recover_interrupted().unwrap();
        assert!(!flow.can_start_turn());
        flow.resume(&100).unwrap();
        assert!(flow.can_start_turn());
    }
}

#[test]
fn closing_waits_for_entry_and_resource_cleanup_and_never_reopens() {
    let mut flow = instance(10);
    flow.activate().unwrap();
    let lease = flow.begin_turn(TurnId::new(20), RunId::new(30)).unwrap();
    flow.begin_close().unwrap();
    assert!(!flow.accepts_input());
    assert!(!flow.can_receive_at_boundary(&lease).unwrap());
    assert_eq!(flow.finish_close(), Err(InstanceError::InvalidTransition));
    assert_eq!(
        flow.set_phase(&lease, TurnPhase::Running),
        Err(InstanceError::InvalidTransition)
    );
    assert_eq!(
        flow.finish_turn(&lease, TurnOutcome::Completed),
        Ok(TurnOutcome::Interrupted(Interruption::Cancelled))
    );
    assert_eq!(flow.state(), &InstanceState::Closing { active: None });
    flow.finish_close().unwrap();
    assert_eq!(flow.state(), &InstanceState::Closed);
    assert_eq!(flow.activate(), Err(InstanceError::InvalidTransition));
    assert_eq!(flow.resume(&100), Err(InstanceError::InvalidTransition));
    assert_eq!(
        flow.recover_interrupted(),
        Err(InstanceError::InvalidTransition)
    );
    assert_eq!(
        flow.finish_turn(&lease, TurnOutcome::Completed),
        Err(InstanceError::StaleLease)
    );
    let revision = flow.revision();
    flow.begin_close().unwrap();
    flow.finish_close().unwrap();
    assert_eq!(flow.revision(), revision);
}

#[test]
fn finishing_and_cancelling_cannot_regress_or_accept_new_boundary_input() {
    let mut flow = instance(10);
    flow.activate().unwrap();
    let lease = flow.begin_turn(TurnId::new(20), RunId::new(30)).unwrap();
    flow.set_phase(&lease, TurnPhase::Waiting(WaitReason::Reply))
        .unwrap();
    assert!(!flow.can_receive_at_boundary(&lease).unwrap());
    flow.set_phase(&lease, TurnPhase::Running).unwrap();
    flow.set_phase(&lease, TurnPhase::Finishing).unwrap();
    assert!(!flow.can_receive_at_boundary(&lease).unwrap());
    assert_eq!(
        flow.set_phase(&lease, TurnPhase::Running),
        Err(InstanceError::InvalidTransition)
    );
    flow.set_phase(&lease, TurnPhase::Cancelling).unwrap();
    assert_eq!(
        flow.set_phase(&lease, TurnPhase::Finishing),
        Err(InstanceError::InvalidTransition)
    );
    assert_eq!(
        flow.finish_turn(&lease, TurnOutcome::Completed),
        Ok(TurnOutcome::Interrupted(Interruption::Cancelled))
    );
    assert!(!flow.can_start_turn());
}

#[test]
fn control_of_one_instance_has_no_effect_on_its_peers() {
    let mut a = instance(10);
    let mut b = instance(11);
    a.activate().unwrap();
    b.activate().unwrap();
    let peer = b.clone();
    a.pause(100).unwrap();
    a.begin_close().unwrap();
    a.finish_close().unwrap();
    assert_eq!(b, peer);
    assert!(b.can_start_turn());
}
