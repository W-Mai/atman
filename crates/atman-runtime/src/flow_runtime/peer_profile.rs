//! Typed access to the one Profile writer. Remote views never grant execution authority.

use std::io;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

mod delivery;
pub use delivery::{ControlDelivery, ControlDeliveryReply, deliver};
pub(crate) use delivery::{application_receipt, deliver_remote};

use super::host::{ProfileHandle, ProfileView, RuntimeHost};
use crate::{
    event_log::journal::CommitReceipt,
    flow_store::{
        FlowRef, SessionConfiguration,
        access::{AccessTarget, FlowAction, FlowGrant},
        inbox::SenderIdentity,
        profile::{ControlBatchPage, ControlBatchRecord, ControlBatchRequest, ReplaceManagement},
    },
};

/// Host-only Profile management. Invocation operations use the original peer actor instead.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "operation",
    content = "arguments",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ProfileOperation {
    Snapshot,
    Authorize {
        source: FlowRef,
        target: AccessTarget,
        action: FlowAction,
    },
    ControlEvidence {
        sender: SenderIdentity,
        batch: Uuid,
        target: FlowRef,
    },
    Bootstrap {
        configuration: SessionConfiguration,
    },
    Replace(ReplaceManagement),
    Select {
        request_id: Uuid,
        expected: CommitReceipt,
        session_id: Uuid,
        grants: Vec<FlowGrant>,
    },
    SubmitUserControls(ControlBatchRequest),
    UserControlBatch {
        sender: SenderIdentity,
        id: Uuid,
    },
    UserControlHistory {
        sender: SenderIdentity,
        before: Option<Uuid>,
        limit: usize,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ProfileReply {
    View(ProfileView),
    Receipt(CommitReceipt),
    Submitted(ControlBatchRecord),
    Batch(Option<ControlBatchRecord>),
    History(ControlBatchPage),
    Authorization {
        profile_id: Uuid,
        cursor: CommitReceipt,
        allowed: bool,
    },
    ControlEvidence {
        submitted: CommitReceipt,
        request: crate::flow_store::control::ControlRequest,
        cursor: CommitReceipt,
    },
}

/// Only trusted host construction can install this port; user RPC cannot replace it.
pub trait ProfileControlPort: Send + Sync {
    fn call(&self, operation: ProfileOperation) -> BoxFuture<'static, io::Result<ProfileReply>>;
}

fn user(sender: &SenderIdentity) -> io::Result<()> {
    match sender {
        SenderIdentity::User(_) => Ok(()),
        SenderIdentity::Flow(_) | SenderIdentity::Host(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Profile host management requires the original User actor",
        )),
    }
}

/// Serve the original local writer. This function never opens or forwards to another writer.
pub async fn dispatch(host: &RuntimeHost, operation: ProfileOperation) -> io::Result<ProfileReply> {
    let profile: ProfileHandle = host
        .profile_handle()
        .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "Profile owner changed"))?;
    match operation {
        ProfileOperation::Snapshot => profile.snapshot().await.map(ProfileReply::View),
        ProfileOperation::Authorize {
            source,
            target,
            action,
        } => {
            let view = profile.published_view()?;
            let allowed = view.unavailable.is_none()
                && view
                    .profile
                    .management
                    .as_ref()
                    .is_some_and(|binding| binding.allows_scope(&source, &target, action));
            Ok(ProfileReply::Authorization {
                profile_id: view.profile.profile_id,
                cursor: view.profile.cursor,
                allowed,
            })
        }
        ProfileOperation::ControlEvidence {
            sender,
            batch,
            target,
        } => {
            let record = profile.published_control(&sender, batch)?;
            let request = record
                .items
                .iter()
                .find(|item| item.request.target == target)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::NotFound,
                        "Profile batch target does not exist",
                    )
                })?
                .request
                .clone();
            Ok(ProfileReply::ControlEvidence {
                submitted: record.submitted,
                request,
                cursor: profile.published_view()?.profile.cursor,
            })
        }
        ProfileOperation::Bootstrap { configuration } => profile
            .bootstrap(configuration)
            .await
            .map(ProfileReply::View),
        ProfileOperation::Replace(request) => {
            profile.replace(request).await.map(ProfileReply::Receipt)
        }
        ProfileOperation::Select {
            request_id,
            expected,
            session_id,
            grants,
        } => profile
            .select(request_id, expected, session_id, grants)
            .await
            .map(ProfileReply::Receipt),
        ProfileOperation::SubmitUserControls(request) => {
            user(&request.sender)?;
            host.submit_controls(request, |_| Ok(()))
                .await
                .map(ProfileReply::Submitted)
        }
        ProfileOperation::UserControlBatch { sender, id } => {
            user(&sender)?;
            profile
                .control_batch(sender, id)
                .await
                .map(ProfileReply::Batch)
        }
        ProfileOperation::UserControlHistory {
            sender,
            before,
            limit,
        } => {
            user(&sender)?;
            profile
                .control_batches(sender, before, limit)
                .await
                .map(ProfileReply::History)
        }
    }
}
