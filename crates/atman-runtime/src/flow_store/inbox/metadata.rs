//! Body-free input control state, prepared before journal commitment.

use super::*;

/// Scheduling category without message text, attachments, invocation arguments, or source bundles.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum InputKind {
    Command,
    Invocation,
    UserSubmission,
    Interjection,
    GoalResume {
        goal: super::super::goal::GoalVersion,
    },
    GoalContinuation {
        source: TurnLease<AtmanUuid>,
        goal: super::super::goal::GoalVersion,
    },
    Notification,
}

impl InputKind {
    pub(super) fn validate_replacement(
        &self,
        content: &UserContent,
        delivery: DeliveryMode,
    ) -> io::Result<()> {
        if content.text.trim().is_empty() && content.images.is_empty() {
            return Err(invalid("user input requires text or an attachment"));
        }
        match self {
            Self::UserSubmission | Self::Command => Ok(()),
            Self::Invocation if delivery == DeliveryMode::NextTurn => Ok(()),
            Self::Interjection if content.images.is_empty() && content.presentation.is_none() => {
                Ok(())
            }
            _ => Err(invalid("input does not support this content replacement")),
        }
    }
}

/// Validated routing and consumption eligibility retained independently of the request body.
#[derive(Clone, Debug, Serialize)]
pub struct InputRequest {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_head: Option<HistoryHead>,
    pub request_id: Uuid,
    pub sender: SenderIdentity,
    pub recipient: FlowRef,
    pub delivery: DeliveryMode,
    pub cause: Option<FlowEventRef>,
    pub correlation: Option<Uuid>,
    pub scope: Option<super::super::task_scope::TaskScopeRef>,
    pub payload: InputKind,
    editable: bool,
    resumes_interruption: bool,
}

impl InputRequest {
    pub fn is_editable_user_input(&self) -> bool {
        self.editable
    }
    pub(super) fn resumes_interruption(&self) -> bool {
        self.resumes_interruption
    }
}

/// Accepted input identity and control state. Load the complete envelope with `FlowStore::inbox_entry`.
#[derive(Clone, Debug, Serialize)]
pub struct InputEnvelope {
    /// Head at acceptance; a later selection holds this entry even when returning to its branch.
    pub history_head: HistoryHead,
    pub id: Uuid,
    pub accepted_at: DateTime<Utc>,
    pub request: InputRequest,
    pub is_watcher_notification: bool,
}

/// Current acceptance and edit state without retaining the accepted message body.
#[derive(Clone, Debug, Serialize)]
pub struct InboxSummary {
    pub envelope: InputEnvelope,
    pub acceptance: Acceptance,
    pub revision: FlowPosition,
    #[serde(skip)]
    pub(super) original: blake3::Hash,
    #[serde(skip)]
    pub(super) content: Option<blake3::Hash>,
}

pub(in crate::flow_store) struct PreparedInput {
    pub(super) envelope: InputEnvelope,
    pub(super) original: blake3::Hash,
    pub(super) content: Option<blake3::Hash>,
}

impl PreparedInput {
    pub(super) fn new(envelope: &AcceptedEnvelope, history_head: HistoryHead) -> io::Result<Self> {
        let request = &envelope.request;
        let payload = InputKind::from(&request.payload);
        let editable = request.is_editable_user_input();
        let content = if editable {
            let owned;
            let value = match &request.payload {
                InboxPayload::UserSubmission { content, .. } => content,
                InboxPayload::Command {
                    user: Some(user), ..
                }
                | InboxPayload::Invocation {
                    user: Some(user), ..
                } => user,
                InboxPayload::Interjection { text } => {
                    owned = UserContent {
                        text: text.clone(),
                        images: vec![],
                        presentation: None,
                    };
                    &owned
                }
                _ => return Err(invalid("editable input has no user content")),
            };
            Some(request_fingerprint(&(value, request.delivery))?)
        } else {
            None
        };
        Ok(Self {
            envelope: InputEnvelope {
                history_head,
                id: envelope.id,
                accepted_at: envelope.accepted_at,
                request: InputRequest {
                    expected_head: request.expected_head.clone(),
                    request_id: request.request_id,
                    sender: request.sender.clone(),
                    recipient: request.recipient.clone(),
                    delivery: request.delivery,
                    cause: request.cause.clone(),
                    correlation: request.correlation,
                    scope: request.scope.clone(),
                    payload,
                    editable,
                    resumes_interruption: request.resumes_interruption(),
                },
                is_watcher_notification: super::super::subscription::is_watcher_notification(
                    envelope,
                ),
            },
            original: request_fingerprint(request)?,
            content,
        })
    }
}

impl From<&InboxPayload> for InputKind {
    fn from(payload: &InboxPayload) -> Self {
        match payload {
            InboxPayload::Command { .. } => InputKind::Command,
            InboxPayload::Invocation { .. } => InputKind::Invocation,
            InboxPayload::UserSubmission { .. } => InputKind::UserSubmission,
            InboxPayload::Interjection { .. } => InputKind::Interjection,
            InboxPayload::GoalResume { goal, .. } => InputKind::GoalResume { goal: goal.clone() },
            InboxPayload::GoalContinuation { source, goal, .. } => InputKind::GoalContinuation {
                source: source.clone(),
                goal: goal.clone(),
            },
            InboxPayload::FormReply { .. }
            | InboxPayload::FlowEvent { .. }
            | InboxPayload::Watcher { .. }
            | InboxPayload::ProcessEvent { .. }
            | InboxPayload::Timer { .. }
            | InboxPayload::Internal { .. } => InputKind::Notification,
        }
    }
}
