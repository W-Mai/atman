use anyhow::{Context, Result, bail};
use atman_client::{Client, SessionClient};
use atman_proto::{
    CompactReviewDecision, DeleteSessionResponse, FlowRunId, FormAnswer, FormSubmission,
    InlineImage, InterjectionLevel, PermissionRpcAction, PermissionRpcScope, PermissionRpcSelector,
    RunLifecycle, SessionId, TrustEscalation, TrustMode, TrustPolicyAction, TrustProjection,
    TrustRiskOverrides, TrustTheme, TrustTierOverrides,
};

use crate::{TuiDomainCommand, TuiSubmission};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonCommandOutcome {
    Applied,
    SessionRenamed {
        session_id: SessionId,
        title: Option<String>,
    },
    SessionDelete(DeleteSessionResponse),
}

#[derive(Clone)]
pub struct DaemonTuiAdapter {
    client: Client,
    session: SessionClient,
}

impl DaemonTuiAdapter {
    pub fn new(client: Client, session: SessionClient) -> Self {
        Self { client, session }
    }

    pub fn session(&self) -> &SessionClient {
        &self.session
    }

    pub async fn dispatch(&self, command: TuiDomainCommand) -> Result<DaemonCommandOutcome> {
        match command {
            TuiDomainCommand::Submit(submission) => self.submit(submission).await,
            TuiDomainCommand::UpdateTrust(trust) => {
                let mut trust = trust_projection(&trust);
                trust.theme = self.session.current().projection().trust.theme;
                self.session.update_trust(trust).await?;
                Ok(DaemonCommandOutcome::Applied)
            }
            TuiDomainCommand::CancelFlow => {
                let run_id = active_root_run(self.session.current().projection())?;
                self.session.cancel_run(run_id).await?;
                Ok(DaemonCommandOutcome::Applied)
            }
            TuiDomainCommand::HardStop => {
                let run_id = active_root_run(self.session.current().projection())?;
                self.session
                    .interject(run_id, "stop", InterjectionLevel::HardStop, None)
                    .await?;
                Ok(DaemonCommandOutcome::Applied)
            }
            TuiDomainCommand::ResolvePermission {
                selector,
                expected_revision,
                action,
                grant_scope,
                reason,
            } => {
                self.session
                    .resolve_permissions(
                        permission_selector(selector, expected_revision)?,
                        permission_action(action),
                        grant_scope.map(permission_scope),
                        reason,
                    )
                    .await?;
                Ok(DaemonCommandOutcome::Applied)
            }
            TuiDomainCommand::CompactNow => {
                self.session.compact().await?;
                Ok(DaemonCommandOutcome::Applied)
            }
            TuiDomainCommand::CompactReviewAccept { review_id, edited } => {
                let decision = edited.map_or(CompactReviewDecision::AcceptAsIs, |summary| {
                    CompactReviewDecision::AcceptEdited { summary }
                });
                self.session
                    .resolve_compact_review(review_id, decision)
                    .await?;
                Ok(DaemonCommandOutcome::Applied)
            }
            TuiDomainCommand::CompactReviewReject { review_id } => {
                self.session
                    .resolve_compact_review(review_id, CompactReviewDecision::Reject)
                    .await?;
                Ok(DaemonCommandOutcome::Applied)
            }
            TuiDomainCommand::DeleteSession(session_id) => {
                let session_id = parse_session_id(&session_id)?;
                let response = self.client.delete_session(session_id).await?;
                Ok(DaemonCommandOutcome::SessionDelete(response))
            }
            TuiDomainCommand::RenameSession { session_id, title } => {
                let session_id = parse_session_id(&session_id)?;
                let session = if session_id == *self.session.session_id() {
                    self.session.clone()
                } else {
                    self.client.attach_session(session_id.clone()).await?
                };
                match &title {
                    Some(title) => {
                        session.rename(title).await?;
                    }
                    None => {
                        session.clear_title().await?;
                    }
                }
                Ok(DaemonCommandOutcome::SessionRenamed { session_id, title })
            }
            TuiDomainCommand::FormSubmit {
                form_id,
                submission,
            } => {
                self.session
                    .submit_form(form_id, form_submission(submission))
                    .await?;
                Ok(DaemonCommandOutcome::Applied)
            }
            TuiDomainCommand::TermResize {
                resource_id,
                rows,
                cols,
            } => {
                self.session
                    .resize_terminal(resource_id, rows, cols)
                    .await?;
                Ok(DaemonCommandOutcome::Applied)
            }
        }
    }

    async fn submit(&self, submission: TuiSubmission) -> Result<DaemonCommandOutcome> {
        if let Some((level, text, redirect_target)) = explicit_interjection(&submission.text)? {
            if !submission.images.is_empty() {
                bail!("interjections cannot include image attachments");
            }
            let run_id = active_root_run(self.session.current().projection())?;
            self.session
                .interject(run_id, text, level, redirect_target)
                .await?;
            return Ok(DaemonCommandOutcome::Applied);
        }
        let images = submission
            .images
            .iter()
            .map(|source| {
                Ok(InlineImage {
                    data_base64: atman_runtime::attachment_store::image_base64(source, None)?,
                    name: Some(atman_runtime::attachment_store::display_name(source)),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        self.session
            .send_message(
                submission.text,
                submission.reasoning.map(|selection| selection.to_string()),
                images,
            )
            .await?;
        Ok(DaemonCommandOutcome::Applied)
    }
}

fn parse_session_id(value: &str) -> Result<SessionId> {
    Ok(SessionId(uuid::Uuid::parse_str(value).with_context(
        || format!("invalid session id `{value}`"),
    )?))
}

fn active_root_run(projection: &atman_proto::SessionProjection) -> Result<FlowRunId> {
    let active = projection
        .runs
        .iter()
        .filter(|run| {
            run.parent_run_id.is_none()
                && matches!(
                    run.state,
                    RunLifecycle::Queued
                        | RunLifecycle::Starting
                        | RunLifecycle::Running
                        | RunLifecycle::WaitingInput
                        | RunLifecycle::Cancelling
                )
        })
        .map(|run| run.id.clone())
        .collect::<Vec<_>>();
    match active.as_slice() {
        [run_id] => Ok(run_id.clone()),
        [] => bail!("the session has no active root run"),
        _ => bail!("the session has multiple active root runs; select a run first"),
    }
}

fn explicit_interjection(
    text: &str,
) -> Result<Option<(InterjectionLevel, String, Option<String>)>> {
    let text = text.trim();
    if text == "!stop" {
        return Ok(Some((InterjectionLevel::HardStop, "stop".into(), None)));
    }
    let Some(rest) = text.strip_prefix('!') else {
        return Ok(None);
    };
    if rest.trim().is_empty() {
        bail!("interjection requires text or a command");
    }
    let (command, value) = rest.split_once(' ').unwrap_or((rest, ""));
    let value = value.trim();
    let (level, redirect_target) = match command {
        "nudge" => (InterjectionLevel::Nudge, None),
        "course-correct" => (InterjectionLevel::CourseCorrect, None),
        "redirect" => (InterjectionLevel::Redirect, Some(value.to_owned())),
        _ => {
            return Ok(Some((
                InterjectionLevel::Nudge,
                rest.trim().to_owned(),
                None,
            )));
        }
    };
    if value.is_empty() {
        bail!("interjection `!{command}` requires a value");
    }
    Ok(Some((level, value.to_owned(), redirect_target)))
}

fn permission_selector(
    selector: atman_runtime::permission::PermissionSelector,
    expected_revision: u64,
) -> Result<PermissionRpcSelector> {
    match selector {
        atman_runtime::permission::PermissionSelector::RequestIds(request_ids) => {
            let request_ids = request_ids
                .into_iter()
                .map(|request_id| request_id.0)
                .collect::<Vec<_>>();
            let expected_request_revisions = request_ids
                .iter()
                .copied()
                .map(|request_id| (request_id, expected_revision))
                .collect();
            Ok(PermissionRpcSelector::Requests {
                request_ids,
                expected_request_revisions,
            })
        }
        atman_runtime::permission::PermissionSelector::Group(group_id) => {
            Ok(PermissionRpcSelector::Group {
                group_id: group_id.0,
                expected_group_revision: expected_revision,
            })
        }
        _ => bail!("the daemon protocol does not support this permission selector"),
    }
}

fn permission_action(action: atman_runtime::permission::PermissionAction) -> PermissionRpcAction {
    match action {
        atman_runtime::permission::PermissionAction::Approve => PermissionRpcAction::Approve,
        atman_runtime::permission::PermissionAction::Deny => PermissionRpcAction::Deny,
        atman_runtime::permission::PermissionAction::Defer => PermissionRpcAction::Defer,
    }
}

fn permission_scope(scope: atman_runtime::permission::GrantScope) -> PermissionRpcScope {
    match scope {
        atman_runtime::permission::GrantScope::CurrentCall => PermissionRpcScope::CurrentCall,
        atman_runtime::permission::GrantScope::ChildRunSameTool { run_id, tool_name } => {
            PermissionRpcScope::ChildRunSameTool {
                run_id: FlowRunId(run_id.0),
                tool_name,
            }
        }
        atman_runtime::permission::GrantScope::ChildRunSamePathRule {
            run_id,
            tool_name,
            workspace_relative_path,
        } => PermissionRpcScope::ChildRunSamePathRule {
            run_id: FlowRunId(run_id.0),
            tool_name,
            workspace_relative_path,
        },
    }
}

fn form_submission(submission: atman_runtime::form::FormSubmission) -> FormSubmission {
    match submission {
        atman_runtime::form::FormSubmission::Rejected => FormSubmission::Rejected,
        atman_runtime::form::FormSubmission::Submitted { answers } => FormSubmission::Submitted {
            answers: answers.into_iter().map(form_answer).collect(),
        },
    }
}

fn form_answer(answer: atman_runtime::form::FormAnswer) -> FormAnswer {
    match answer {
        atman_runtime::form::FormAnswer::Confirmed { value } => FormAnswer::Confirmed { value },
        atman_runtime::form::FormAnswer::Selected { index, label } => {
            FormAnswer::Selected { index, label }
        }
        atman_runtime::form::FormAnswer::MultiSelected { indices, labels } => {
            FormAnswer::MultiSelected { indices, labels }
        }
        atman_runtime::form::FormAnswer::TextEntered { text } => FormAnswer::TextEntered { text },
        atman_runtime::form::FormAnswer::Cancelled => FormAnswer::Cancelled,
    }
}

fn trust_projection(trust: &atman_runtime::trust::TrustConfig) -> TrustProjection {
    let action = |action: Option<atman_runtime::trust::PolicyAction>| {
        action.map(|action| match action {
            atman_runtime::trust::PolicyAction::Auto => TrustPolicyAction::Auto,
            atman_runtime::trust::PolicyAction::Ask => TrustPolicyAction::Ask,
            atman_runtime::trust::PolicyAction::Deny => TrustPolicyAction::Deny,
        })
    };
    TrustProjection {
        mode: match trust.mode {
            atman_runtime::trust::TrustMode::Calm => TrustMode::Calm,
            atman_runtime::trust::TrustMode::Steady => TrustMode::Steady,
            atman_runtime::trust::TrustMode::Eager => TrustMode::Eager,
            atman_runtime::trust::TrustMode::Reckless => TrustMode::Reckless,
        },
        theme: match trust.theme {
            atman_runtime::trust::Theme::Default => TrustTheme::Default,
            atman_runtime::trust::Theme::Wuxia => TrustTheme::Wuxia,
            atman_runtime::trust::Theme::Animal => TrustTheme::Animal,
            atman_runtime::trust::Theme::Weather => TrustTheme::Weather,
            atman_runtime::trust::Theme::Drink => TrustTheme::Drink,
        },
        escalation: match trust.escalation {
            atman_runtime::trust::EscalationPolicy::Deny => TrustEscalation::Deny,
            atman_runtime::trust::EscalationPolicy::Ask => TrustEscalation::Ask,
            atman_runtime::trust::EscalationPolicy::Allow => TrustEscalation::Allow,
        },
        eager_tiers: TrustTierOverrides {
            tier0: action(trust.tiers.eager.tier0),
            tier1: action(trust.tiers.eager.tier1),
            tier2: action(trust.tiers.eager.tier2),
            tier3: action(trust.tiers.eager.tier3),
            tier4: action(trust.tiers.eager.tier4),
        },
        eager_risks: TrustRiskOverrides {
            outside_workspace: action(trust.risks.eager.outside_workspace),
            network: action(trust.risks.eager.network),
            irreversible: action(trust.risks.eager.irreversible),
            filesystem_write: action(trust.risks.eager.filesystem_write),
            process_spawn: action(trust.risks.eager.process_spawn),
            repository_mutation: action(trust.risks.eager.repository_mutation),
        },
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::*;

    fn projection(runs: Vec<atman_proto::RunProjection>) -> atman_proto::SessionProjection {
        atman_proto::SessionProjection {
            revision: atman_proto::Revision(0),
            metadata: atman_proto::SessionMetadataProjection {
                id: SessionId(uuid::Uuid::nil()),
                title: String::new(),
                name_source: atman_proto::NameSource::Auto,
                project_root: None,
                created_at: None,
                updated_at: None,
            },
            lifecycle: atman_proto::SessionLifecycle::Idle,
            runs,
            transcript: Vec::new(),
            workflows: Vec::new(),
            compactions: Vec::new(),
            goal: None,
            todos: Vec::new(),
            plans: Vec::new(),
            context: Default::default(),
            trust: Default::default(),
            interactions: Default::default(),
            resources: Vec::new(),
            usage: Default::default(),
        }
    }

    fn run(parent_run_id: Option<FlowRunId>, state: RunLifecycle) -> atman_proto::RunProjection {
        atman_proto::RunProjection {
            id: FlowRunId(uuid::Uuid::now_v7()),
            turn_id: None,
            flow_name: "agent".into(),
            model: None,
            provider: None,
            parent_run_id,
            parent_node_id: None,
            state,
            started_at: Utc::now(),
            finished_at: None,
            error: None,
            output: None,
        }
    }

    #[test]
    fn active_run_requires_one_root_and_ignores_children_and_terminal_runs() {
        let root = run(None, RunLifecycle::Running);
        let child = run(Some(root.id.clone()), RunLifecycle::Running);
        let done = run(None, RunLifecycle::Succeeded);
        assert_eq!(
            active_root_run(&projection(vec![root.clone(), child, done])).unwrap(),
            root.id
        );
        assert!(active_root_run(&projection(Vec::new())).is_err());
        assert!(
            active_root_run(&projection(vec![
                run(None, RunLifecycle::Running),
                run(None, RunLifecycle::WaitingInput),
            ]))
            .unwrap_err()
            .to_string()
            .contains("select a run")
        );
    }

    #[test]
    fn explicit_interjection_preserves_freeform_nudges() {
        assert_eq!(explicit_interjection("hello").unwrap(), None);
        assert_eq!(
            explicit_interjection("!redirect implementation").unwrap(),
            Some((
                InterjectionLevel::Redirect,
                "implementation".into(),
                Some("implementation".into())
            ))
        );
        assert!(explicit_interjection("!redirect").is_err());
        assert_eq!(
            explicit_interjection("!unknown value").unwrap(),
            Some((InterjectionLevel::Nudge, "unknown value".into(), None))
        );
        assert!(explicit_interjection("!").is_err());
    }

    #[test]
    fn permission_requests_keep_every_expected_revision() {
        let first = atman_runtime::permission::PermissionRequestId::now();
        let second = atman_runtime::permission::PermissionRequestId::now();
        let selector = permission_selector(
            atman_runtime::permission::PermissionSelector::RequestIds(vec![
                first.clone(),
                second.clone(),
            ]),
            7,
        )
        .unwrap();
        let PermissionRpcSelector::Requests {
            request_ids,
            expected_request_revisions,
        } = selector
        else {
            panic!("expected request selector");
        };
        assert_eq!(request_ids, vec![first.0, second.0]);
        assert_eq!(expected_request_revisions.len(), 2);
        assert!(
            expected_request_revisions
                .values()
                .all(|revision| *revision == 7)
        );
    }
}
