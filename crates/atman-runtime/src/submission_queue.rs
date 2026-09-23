use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::InvocationEnv;
use crate::message::{ImageSource, MessageOrigin};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SubmissionId(Uuid);

impl SubmissionId {
    pub fn now() -> Self {
        Self(Uuid::now_v7())
    }
}

impl std::fmt::Display for SubmissionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone)]
pub struct QueuedSubmission {
    pub id: SubmissionId,
    pub revision: u64,
    pub text: String,
    pub images: Vec<ImageSource>,
    pub invocation_env: InvocationEnv,
    pub created_at: DateTime<Utc>,
    pub origin: MessageOrigin,
    pub presentation: Option<crate::user_input::UserInputPresentation>,
}

impl QueuedSubmission {
    pub fn new(
        text: impl Into<String>,
        images: Vec<ImageSource>,
        invocation_env: InvocationEnv,
        origin: MessageOrigin,
    ) -> Self {
        Self {
            id: SubmissionId::now(),
            revision: 0,
            text: text.into(),
            images,
            invocation_env,
            created_at: Utc::now(),
            origin,
            presentation: None,
        }
    }

    pub fn next_call_block_reason(&self, allow_images: bool) -> Option<&'static str> {
        let routed_text = self
            .presentation
            .as_ref()
            .map_or(self.text.as_str(), |value| value.prompt.as_str())
            .trim_start();
        if routed_text.starts_with(':') || routed_text.starts_with('/') {
            return Some("commands run as a separate turn");
        }
        if routed_text.split_whitespace().any(|word| {
            word.starts_with("@./") || word.starts_with("@../") || word.starts_with("@/")
        }) {
            return Some("path attachments need a separate turn");
        }
        if !self.invocation_env.is_empty() {
            return Some("invocation settings need a separate turn");
        }
        if !allow_images && !self.images.is_empty() {
            return Some("images cannot enter an L1 insertion");
        }
        None
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedSubmissionView {
    pub id: SubmissionId,
    pub revision: u64,
    pub text: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<crate::user_input::UserInputPresentation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_block_reason: Option<String>,
}

impl From<&QueuedSubmission> for QueuedSubmissionView {
    fn from(submission: &QueuedSubmission) -> Self {
        Self {
            id: submission.id.clone(),
            revision: submission.revision,
            text: submission
                .presentation
                .as_ref()
                .map_or_else(|| submission.text.clone(), |value| value.prompt.clone()),
            created_at: submission.created_at,
            presentation: submission.presentation.clone(),
            insert_block_reason: submission
                .next_call_block_reason(false)
                .map(str::to_owned)
                .or_else(|| {
                    (submission.origin != MessageOrigin::User)
                        .then(|| "only user tasks can be inserted".to_owned())
                }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmissionMove {
    Up,
    Down,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SubmissionQueueError {
    #[error("queued submission text cannot be empty")]
    EmptyText,
    #[error("queued submission not found")]
    NotFound,
    #[error("queued submission changed; refresh and try again")]
    RevisionConflict,
    #[error("no active turn can receive an insertion")]
    NoActiveTurn,
    #[error("queued submission cannot be inserted: {0}")]
    NotInjectable(&'static str),
}
