use std::{io, sync::Arc};

use atman_proto::{
    JsonRpcError, SessionId,
    flows::{FlowAddress, FlowCursor},
    projects::{
        OpenWorkspaceRequest, WorkspaceOpenFailure as Failure, WorkspaceOpenProgress as Progress,
        WorkspaceOpenResponse,
    },
};
use atman_runtime::{
    event_log::journal::CommitReceipt,
    flow_store::profile::workspace_open::{
        WorkspaceOpenAction, WorkspaceOpenFailure, WorkspaceOpenOutcome, WorkspaceProgress,
        observe_workspace_open,
    },
};
use uuid::Uuid;

use crate::DaemonState;

static OPEN_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(4);

pub(in crate::workspace_directory) async fn status(
    state: &DaemonState,
    operation: Uuid,
) -> Result<WorkspaceOpenResponse, JsonRpcError> {
    let outcome = observe_workspace_open(state.data_dir().to_owned(), operation)
        .await
        .map_err(receipt_error)?;
    Ok(response(operation, outcome))
}

pub(in crate::workspace_directory) async fn open(
    state: Arc<DaemonState>,
    request: OpenWorkspaceRequest,
) -> Result<WorkspaceOpenResponse, JsonRpcError> {
    let slot = OPEN_SLOTS
        .try_acquire()
        .map_err(|_| JsonRpcError::application("Workspace open operations are busy"))?;
    let observed = status(&state, request.operation_id).await?;
    let candidate = state.workspace_folders.selection(request.selection);
    let reader = super::reader(&state)
        .map_err(|_| JsonRpcError::application("Workspace configuration is unavailable"))?;
    if matches!(observed.progress, Progress::Unseen) {
        let Some(candidate) = &candidate else {
            return Ok(WorkspaceOpenResponse {
                failure: Some(Failure::SelectionExpired),
                ..observed
            });
        };
        if let Err(error) = reader
            .clone()
            .check_selection(candidate.selection.clone())
            .await
        {
            return Ok(WorkspaceOpenResponse {
                failure: Some(failure(error.into())),
                ..observed
            });
        }
    }
    let host = state.runtime_host();
    let admission = Arc::new(host.retain_host_operation().map_err(receipt_error)?);
    tokio::spawn(async move {
        let _slot = slot;
        let profile = match host.profile_control_port() {
            Some(profile) => profile,
            None => host
                .open_profile(state.data_dir())
                .await
                .map_err(receipt_error)?,
        };
        let mut opened = profile
            .workspace_open(
                WorkspaceOpenAction::Open {
                    operation: request.operation_id,
                    selection_id: request.selection,
                    candidate,
                    reader: reader.clone(),
                },
                admission.clone(),
            )
            .await
            .map_err(receipt_error)?;
        let proof = opened.bootstrap.take();
        let first = if let (Some(proof), WorkspaceProgress::SessionCreated { catalog, .. }) =
            (proof, &opened.progress)
        {
            crate::flow_service::bootstrap_workspace(&state, catalog.clone(), proof, reader).await
        } else {
            return Ok(response(request.operation_id, opened));
        };
        let launch = match first {
            Ok(launch) => launch,
            Err(_) => {
                let latest = status(&state, request.operation_id).await?;
                return Ok(
                    if matches!(latest.progress, Progress::ConversationReady { .. }) {
                        latest
                    } else {
                        WorkspaceOpenResponse {
                            failure: Some(Failure::ConversationUnavailable),
                            ..latest
                        }
                    },
                );
            }
        };
        let completed = profile
            .workspace_open(
                WorkspaceOpenAction::Complete {
                    operation: request.operation_id,
                    selection_id: request.selection,
                    conversation: CommitReceipt {
                        journal_id: launch.receipt.journal_id,
                        sequence: launch.receipt.sequence,
                        digest: launch.receipt.digest,
                    },
                },
                admission,
            )
            .await
            .map_err(receipt_error)?;
        Ok(response(request.operation_id, completed))
    })
    .await
    .map_err(|_| {
        JsonRpcError::application(
            "Workspace open outcome is unavailable; check the original operation",
        )
    })?
}

fn cursor(receipt: CommitReceipt) -> FlowCursor {
    FlowCursor {
        journal_id: receipt.journal_id,
        sequence: receipt.sequence,
        digest: receipt.digest,
    }
}

fn response(operation_id: Uuid, outcome: WorkspaceOpenOutcome) -> WorkspaceOpenResponse {
    let progress = match outcome.progress {
        WorkspaceProgress::Unseen => Progress::Unseen,
        WorkspaceProgress::Prepared => Progress::Prepared,
        WorkspaceProgress::ProjectPinned { fingerprint } => Progress::ProjectPinned { fingerprint },
        WorkspaceProgress::SessionCreated {
            fingerprint,
            catalog,
        } => Progress::SessionCreated {
            fingerprint,
            session_id: SessionId(catalog.journal_id),
            catalog: cursor(catalog),
        },
        WorkspaceProgress::ConversationReady {
            fingerprint,
            catalog,
            conversation,
        } => Progress::ConversationReady {
            fingerprint,
            flow: FlowAddress {
                session_id: SessionId(catalog.journal_id),
                instance_id: conversation.journal_id,
            },
            catalog: cursor(catalog),
            receipt: cursor(conversation),
        },
    };
    WorkspaceOpenResponse {
        operation_id,
        progress,
        failure: outcome.failure.map(failure),
    }
}

fn failure(value: WorkspaceOpenFailure) -> Failure {
    match value {
        WorkspaceOpenFailure::SelectionExpired => Failure::SelectionExpired,
        WorkspaceOpenFailure::DirectoryChanged => Failure::DirectoryChanged,
        WorkspaceOpenFailure::ReadOnlyRoot => Failure::ReadOnlyRoot,
        WorkspaceOpenFailure::DirectoryUnavailable => Failure::DirectoryUnavailable,
        WorkspaceOpenFailure::InvalidConfiguration => Failure::InvalidConfiguration,
        WorkspaceOpenFailure::ProjectUnavailable => Failure::ProjectUnavailable,
        WorkspaceOpenFailure::SessionUnavailable => Failure::SessionUnavailable,
    }
}

fn receipt_error(error: io::Error) -> JsonRpcError {
    if error.kind() == io::ErrorKind::AlreadyExists {
        return JsonRpcError::application(
            "Workspace operation identity conflicts with its original selection",
        );
    }
    JsonRpcError::application("Workspace open outcome is unavailable; check the original operation")
}
