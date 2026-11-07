//! View-owned permission decisions and their original host results.

use std::collections::VecDeque;

use atman_runtime::permission::{
    BatchResolution, GrantScope, PermissionAction, PermissionRequestId, PermissionSelector,
};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionRequest {
    pub id: Uuid,
    pub selector: PermissionSelector,
    pub expected_revision: u64,
    pub action: PermissionAction,
    pub grant_scope: Option<GrantScope>,
    pub reason: Option<String>,
}

pub type DecisionResult = Result<Vec<BatchResolution>, String>;

pub(crate) struct DecisionOperation {
    pub request: DecisionRequest,
    pub members: Vec<PermissionRequestId>,
    pub result: Option<DecisionResult>,
    feedback: String,
    failed: bool,
}

#[derive(Default)]
pub(crate) struct Decisions {
    operations: VecDeque<DecisionOperation>,
}

impl Decisions {
    pub fn is_waiting(&self) -> bool {
        self.operations
            .iter()
            .any(|operation| operation.result.is_none())
    }

    pub fn blocks(&self, members: &[PermissionRequestId]) -> bool {
        self.operations.iter().any(|operation| {
            operation.result.is_none() && operation.members.iter().any(|id| members.contains(id))
        })
    }

    pub fn begin(&mut self, request: DecisionRequest, members: Vec<PermissionRequestId>) {
        self.operations.push_back(DecisionOperation {
            request,
            members,
            result: None,
            feedback: "Submitting".into(),
            failed: false,
        });
    }

    pub fn finish(&mut self, id: Uuid, result: DecisionResult) -> bool {
        let Some(operation) = self
            .operations
            .iter_mut()
            .find(|operation| operation.request.id == id && operation.result.is_none())
        else {
            return false;
        };
        let (message, failed) = match &result {
            Err(error) => (error.clone(), true),
            Ok(rows) => {
                use atman_runtime::permission::BatchRequestOutcome;
                let mut counts = [0usize; 4];
                for row in rows {
                    counts[match row.outcome {
                        BatchRequestOutcome::Approved(_) => 0,
                        BatchRequestOutcome::Denied(_) => 1,
                        BatchRequestOutcome::Deferred(_) => 2,
                        _ => 3,
                    }] += 1;
                }
                let parts = counts
                    .into_iter()
                    .zip(["allowed", "denied", "cancelled", "rejected"])
                    .filter(|(count, _)| *count > 0)
                    .map(|(count, label)| format!("{count} {label}"))
                    .collect::<Vec<_>>();
                (
                    if parts.is_empty() {
                        "0 decisions".into()
                    } else {
                        parts.join(" · ")
                    },
                    counts[3] > 0,
                )
            }
        };
        operation.feedback = message;
        operation.failed = failed;
        operation.result = Some(result);
        while self.operations.len() > 32 {
            let Some(index) = self
                .operations
                .iter()
                .position(|operation| operation.result.is_some())
            else {
                break;
            };
            self.operations.remove(index);
        }
        true
    }

    pub fn feedback(&self, id: Uuid) -> Option<(&str, bool)> {
        self.operations
            .iter()
            .find(|operation| operation.request.id == id)
            .map(|operation| (operation.feedback.as_str(), operation.failed))
    }

    pub fn feedback_for(&self, members: &[PermissionRequestId]) -> Option<(&str, bool, bool)> {
        self.operations
            .iter()
            .rev()
            .find(|operation| operation.members.iter().any(|id| members.contains(id)))
            .map(|operation| {
                (
                    operation.feedback.as_str(),
                    operation.failed,
                    operation.result.is_none(),
                )
            })
    }

    pub fn result_for(&self, members: &[PermissionRequestId]) -> Option<&DecisionResult> {
        self.operations
            .iter()
            .rev()
            .find(|operation| operation.members.iter().any(|id| members.contains(id)))?
            .result
            .as_ref()
    }
}

pub(crate) fn grant_scope(
    request: &crate::app::PendingPermission,
    index: u8,
) -> Option<GrantScope> {
    match index {
        0 => Some(GrantScope::CurrentCall),
        1 => Some(GrantScope::ChildRunSameTool {
            run_id: request.payload.requesting_run_id.clone(),
            tool_name: request.payload.tool.clone(),
        }),
        2 => {
            let path = request.payload.provenance.path.as_ref()?;
            let root = request.payload.provenance.workspace_root.as_ref()?;
            let relative = std::path::Path::new(path)
                .strip_prefix(root)
                .ok()?
                .to_str()?;
            Some(GrantScope::ChildRunSamePathRule {
                run_id: request.payload.requesting_run_id.clone(),
                tool_name: request.payload.tool.clone(),
                workspace_relative_path: relative.into(),
            })
        }
        _ => None,
    }
}

impl crate::app::AppState {
    pub(crate) fn submit_permission(
        &mut self,
        sender: &crate::ControlSender,
        request: DecisionRequest,
    ) {
        if request.action == PermissionAction::Approve && request.grant_scope.is_none() {
            self.push_toast(
                "Scope unavailable; select Current call or Same run/tool",
                crate::app::NoteLevel::Warn,
                std::time::Duration::from_secs(4),
                crate::app::ToastPosition::TopRight,
            );
            return;
        }
        let members = match &request.selector {
            PermissionSelector::RequestIds(ids) => ids.clone(),
            PermissionSelector::Group(id) => self
                .pending_permission_groups
                .get(id)
                .map(|group| group.payload.request_ids.clone())
                .unwrap_or_default(),
            _ => return,
        };
        if members.is_empty() {
            self.push_toast(
                "Request changed; refresh the review",
                crate::app::NoteLevel::Warn,
                std::time::Duration::from_secs(4),
                crate::app::ToastPosition::TopRight,
            );
            return;
        }
        if self.approval_decisions.blocks(&members) {
            return;
        }
        let id = request.id;
        self.approval_decisions.begin(request.clone(), members);
        if sender
            .send(crate::TuiControl::ResolvePermission(request))
            .is_err()
        {
            self.approval_decisions
                .finish(id, Err("Approval connection closed".into()));
            self.push_toast(
                "Approval connection closed",
                crate::app::NoteLevel::Error,
                std::time::Duration::from_secs(4),
                crate::app::ToastPosition::TopRight,
            );
        }
        self.mark_visual_dirty();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_identity_owns_completion_and_duplicate_admission() {
        let member = PermissionRequestId::now();
        let request = DecisionRequest {
            id: Uuid::now_v7(),
            selector: PermissionSelector::RequestIds(vec![member.clone()]),
            expected_revision: 7,
            action: PermissionAction::Approve,
            grant_scope: Some(GrantScope::CurrentCall),
            reason: None,
        };
        let mut decisions = Decisions::default();
        decisions.begin(request.clone(), vec![member.clone()]);
        assert!(decisions.blocks(std::slice::from_ref(&member)));
        assert!(!decisions.finish(Uuid::now_v7(), Ok(Vec::new())));
        assert!(decisions.is_waiting());
        assert!(decisions.finish(request.id, Err("stale revision".into())));
        assert!(!decisions.is_waiting());
        assert!(!decisions.blocks(&[member]));
        assert!(!decisions.finish(request.id, Ok(Vec::new())));
        assert_eq!(
            decisions
                .operations
                .back()
                .unwrap()
                .request
                .expected_revision,
            7
        );
        assert_eq!(
            decisions
                .operations
                .back()
                .unwrap()
                .result
                .as_ref()
                .unwrap()
                .as_ref()
                .unwrap_err(),
            "stale revision"
        );
    }
}
