//! Explicit history protocol projections; private sender identity never crosses the wire.

use super::{
    OPERATOR, address, core_address, events,
    wire::{core_head, cursor, head},
};
use atman_proto::{history as wire, responses::InputTurn};
use atman_runtime::{
    event::AtmanUuid,
    flow_store::{
        branches, goal::GoalVersion, history_operation as core, inbox::SenderIdentity,
        source_capture as source,
    },
};
use std::io;

fn core_lease(value: InputTurn) -> atman_rt::TurnLease<AtmanUuid> {
    atman_rt::TurnLease {
        flow: core_address(value.flow),
        turn: atman_rt::TurnId(value.turn.into()),
        run: atman_rt::RunId(value.run.into()),
        epoch: value.epoch,
    }
}

fn lease(value: &atman_rt::TurnLease<AtmanUuid>) -> InputTurn {
    InputTurn {
        flow: address(&value.flow),
        turn: value.turn.0.0,
        run: value.run.0.0,
        epoch: value.epoch,
    }
}

fn core_source(value: wire::HistorySource) -> source::SourceSelection {
    source::SourceSelection {
        flow: core_address(value.flow),
        branch: value.branch.into(),
        cursor: events::receipt(value.cursor),
    }
}

fn source(value: &source::SourceSelection) -> wire::HistorySource {
    wire::HistorySource {
        flow: address(&value.flow),
        branch: *value.branch.as_uuid(),
        cursor: events::cursor(&value.cursor),
    }
}

fn core_target(value: wire::HistoryTarget) -> core::HistoryOperationTarget {
    match value {
        wire::HistoryTarget::Revert {
            observed,
            cutoff,
            branch,
        } => core::HistoryOperationTarget::Revert {
            observed: core_source(observed),
            branch: branch.into(),
            cutoff: Box::new(match *cutoff {
                wire::ConversationCutoff::BeforeUserTurn {
                    lease,
                    input,
                    receipt,
                } => source::ConversationCutoff::BeforeUserTurn {
                    lease: core_lease(lease),
                    input,
                    receipt: events::receipt(receipt),
                },
                wire::ConversationCutoff::AfterCompletedTurn { lease, receipt } => {
                    source::ConversationCutoff::AfterCompletedTurn {
                        lease: core_lease(lease),
                        receipt: events::receipt(receipt),
                    }
                }
            }),
        },
        wire::HistoryTarget::Restore { branch } => core::HistoryOperationTarget::Restore {
            branch: branch.into(),
        },
    }
}

fn target(value: &core::HistoryOperationTarget) -> wire::HistoryTarget {
    match value {
        core::HistoryOperationTarget::Revert {
            observed,
            cutoff,
            branch,
        } => wire::HistoryTarget::Revert {
            observed: source(observed),
            branch: *branch.as_uuid(),
            cutoff: Box::new(match &**cutoff {
                source::ConversationCutoff::BeforeUserTurn {
                    lease: turn,
                    input,
                    receipt,
                } => wire::ConversationCutoff::BeforeUserTurn {
                    lease: lease(turn),
                    input: *input,
                    receipt: events::cursor(receipt),
                },
                source::ConversationCutoff::AfterCompletedTurn {
                    lease: turn,
                    receipt,
                } => wire::ConversationCutoff::AfterCompletedTurn {
                    lease: lease(turn),
                    receipt: events::cursor(receipt),
                },
            }),
        },
        core::HistoryOperationTarget::Restore { branch } => wire::HistoryTarget::Restore {
            branch: *branch.as_uuid(),
        },
    }
}

pub(super) fn intent(value: wire::HistoryReviewRequest) -> core::HistoryReviewRequest {
    core::HistoryReviewRequest {
        id: value.request_id,
        sender: SenderIdentity::User(OPERATOR),
        flow: core_address(value.flow),
        expected_head: core_head(value.expected_head),
        target: core_target(value.target),
    }
}

pub(super) fn command(value: wire::HistoryOperationRequest) -> core::HistoryOperationRequest {
    core::HistoryOperationRequest {
        id: value.request_id,
        sender: SenderIdentity::User(OPERATOR),
        flow: core_address(value.flow),
        expected_head: core_head(value.expected_head),
        target: core_target(value.target),
        execution_revision: value.execution_revision,
        active: value.active.map(core_lease),
        goal: value.goal.map(|value| GoalVersion {
            id: value.id,
            revision: value.revision,
        }),
        queued: value
            .queued
            .into_iter()
            .map(|value| core::HistoryQueueItem {
                message: value.message,
                revision: events::receipt(value.revision).into(),
            })
            .collect(),
    }
}

fn request(value: &core::HistoryOperationRequest) -> io::Result<wire::HistoryOperationRequest> {
    Ok(wire::HistoryOperationRequest {
        flow: address(&value.flow),
        request_id: value.id,
        expected_head: head(&value.expected_head),
        execution_revision: value.execution_revision,
        active: value.active.as_ref().map(lease),
        goal: value.goal.as_ref().map(|value| wire::HistoryGoalVersion {
            id: value.id.clone(),
            revision: value.revision,
        }),
        queued: value
            .queued
            .iter()
            .map(|value| {
                Ok(wire::HistoryQueueItem {
                    message: value.message,
                    revision: cursor(&value.revision)?,
                })
            })
            .collect::<io::Result<_>>()?,
        target: target(&value.target),
    })
}

fn selection(value: &branches::HistorySelection) -> wire::HistorySelection {
    match value {
        branches::HistorySelection::Create { branch, parent } => wire::HistorySelection::Create {
            branch: *branch.as_uuid(),
            parent: source(parent),
        },
        branches::HistorySelection::Restore { branch } => wire::HistorySelection::Restore {
            branch: *branch.as_uuid(),
        },
    }
}

pub(super) fn review(
    value: core::HistoryOperationReview,
) -> io::Result<wire::HistoryReviewResponse> {
    Ok(wire::HistoryReviewResponse {
        request: request(&value.request)?,
        selection: selection(&value.selection),
        affected_turns: value.affected_turns,
        admitted_inputs: value.admitted_inputs,
    })
}

pub(super) fn record(
    value: core::HistoryOperationRecord,
) -> io::Result<wire::HistoryOperationRecord> {
    Ok(wire::HistoryOperationRecord {
        request: request(&value.request)?,
        selection: selection(&value.selection),
        accepted: cursor(&value.accepted)?,
        cancellation: value
            .cancellation
            .map(|value| -> io::Result<_> {
                Ok(wire::HistoryCancellation {
                    request_id: value.request.id,
                    receipt: cursor(&value.receipt)?,
                })
            })
            .transpose()?,
        status: match value.status {
            core::HistoryOperationStatus::Preparing => wire::HistoryOperationStatus::Preparing,
            core::HistoryOperationStatus::Stopping => wire::HistoryOperationStatus::Stopping,
            core::HistoryOperationStatus::Ready {
                receipt,
                completion,
            } => wire::HistoryOperationStatus::Ready {
                receipt: cursor(&receipt)?,
                completion: completion.as_ref().map(cursor).transpose()?,
            },
            core::HistoryOperationStatus::Aborted { receipt } => {
                wire::HistoryOperationStatus::Aborted {
                    receipt: cursor(&receipt)?,
                }
            }
            core::HistoryOperationStatus::Applied {
                receipt,
                head: selected,
            } => wire::HistoryOperationStatus::Applied {
                receipt: cursor(&receipt)?,
                head: head(&selected),
            },
        },
    })
}
