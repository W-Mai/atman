use super::*;
use crate::flow_store::source_capture::{
    BoundaryReferenceRequest, ReferenceBudget, ReferenceRequest,
};

#[test]
fn reference_capture_selects_ancestry_checkpoints_and_immutable_boundaries_with_existing_budgets() {
    let (_directory, mut flow, operation) = reviewed();
    let HistoryOperationTarget::Revert { cutoff, .. } = operation.target.clone() else {
        unreachable!()
    };
    let abandoned = turn(&mut flow, "unselected tail");
    let unselected_cursor = flow.cursor().require_durable().unwrap().clone();
    let original = checkpoint(&mut flow);
    flow.checkpoint_context(original.clone()).unwrap();
    let request_before = ReferenceRequest::current(operation.flow.clone());
    let previous = flow.capture_reference_plan(&request_before).unwrap();
    let reviewed = request(&flow, *cutoff.clone());
    apply(&mut flow, &reviewed);
    let selected_head = flow.history_head();
    let mut current = ReferenceRequest::current(operation.flow.clone());
    current.branch = Some(selected_head.branch);
    current.budget.max_source_batches = 1;
    let plan = flow.capture_reference_plan(&current).unwrap();
    let (selection, snapshot) = plan.prepare().unwrap();
    assert_eq!(selection.branch, selected_head.branch);
    assert_eq!(snapshot.messages.as_ref(), flow.context_messages().unwrap());
    assert_eq!(snapshot.messages[0].text_concat(), "retained");
    assert!(!snapshot.omissions.earlier_context);
    let (_, old_snapshot) = previous.prepare().unwrap();
    assert_eq!(old_snapshot.messages.as_ref(), original.messages);

    let observed = SourceSelection {
        flow: operation.flow.clone(),
        branch: selected_head.branch,
        cursor: flow.cursor().require_durable().unwrap().clone(),
    };
    let boundary = BoundaryReferenceRequest {
        observed: observed.clone(),
        cutoff: *cutoff,
        budget: ReferenceBudget::default(),
    };
    let (plan, _) = flow.capture_reference_boundary_plan(&boundary).unwrap();
    assert_eq!(
        plan.prepare().unwrap().1.messages[0].text_concat(),
        "retained"
    );
    let foreign = BoundaryReferenceRequest {
        cutoff: abandoned,
        ..boundary.clone()
    };
    assert!(flow.capture_reference_boundary_plan(&foreign).is_err());
    let mut stale = current.clone();
    stale.at = Some(unselected_cursor);
    assert!(flow.capture_reference_plan(&stale).is_err());
    stale.at = None;
    stale.branch = Some(operation.expected_head.branch);
    assert!(flow.capture_reference_plan(&stale).is_err());

    let new_cutoff = turn(&mut flow, "new branch");
    let before_checkpoint = flow.context_messages().unwrap();
    let mut selected_checkpoint = checkpoint(&mut flow);
    selected_checkpoint.messages[0] = Message::assistant_text(TurnId::now(), "selected summary");
    flow.checkpoint_context(selected_checkpoint.clone())
        .unwrap();
    let (source, selected) = flow
        .capture_reference_plan(&current)
        .unwrap()
        .prepare()
        .unwrap();
    assert_eq!(selected.messages.as_ref(), selected_checkpoint.messages);
    let mut restore = request(&flow, new_cutoff.clone());
    restore.target = HistoryOperationTarget::Restore {
        branch: operation.expected_head.branch,
    };
    apply(&mut flow, &restore);
    let request_restored = ReferenceRequest::current(operation.flow.clone());
    let (_, restored) = flow
        .capture_reference_plan(&request_restored)
        .unwrap()
        .prepare()
        .unwrap();
    assert_eq!(restored.messages.as_ref(), original.messages);
    assert!(flow.capture_reference_plan(&current).is_err());
    assert_eq!(selected.messages.as_ref(), selected_checkpoint.messages);

    let mut back = request(&flow, new_cutoff.clone());
    back.target = HistoryOperationTarget::Restore {
        branch: selected_head.branch,
    };
    apply(&mut flow, &back);
    let (_, selected_again) = flow
        .capture_reference_plan(&current)
        .unwrap()
        .prepare()
        .unwrap();
    assert_eq!(
        selected_again.messages.as_ref(),
        selected_checkpoint.messages
    );
    let (plan, _) = flow
        .capture_reference_boundary_plan(&BoundaryReferenceRequest {
            observed: SourceSelection {
                cursor: flow.cursor().require_durable().unwrap().clone(),
                ..source
            },
            cutoff: new_cutoff,
            budget: ReferenceBudget::default(),
        })
        .unwrap();
    assert_eq!(
        plan.prepare().unwrap().1.messages.as_ref(),
        before_checkpoint
    );
    assert!(flow.capture_reference_boundary_plan(&foreign).is_err());
}
