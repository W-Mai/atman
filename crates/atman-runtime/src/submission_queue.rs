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
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueuedSubmissionView {
    pub id: SubmissionId,
    pub revision: u64,
    pub text: String,
    pub created_at: DateTime<Utc>,
}

impl From<&QueuedSubmission> for QueuedSubmissionView {
    fn from(submission: &QueuedSubmission) -> Self {
        Self {
            id: submission.id.clone(),
            revision: submission.revision,
            text: submission.text.clone(),
            created_at: submission.created_at,
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
}
