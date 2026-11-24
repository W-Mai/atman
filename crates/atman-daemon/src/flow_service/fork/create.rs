//! Confirm and reconcile an accepted Session Fork on the source owner.

use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result, ensure};
use atman_proto::{
    JsonRpcError,
    flows::{FlowAddress, FlowCursor},
    history::*,
};
use atman_runtime::{
    flow_runtime::{
        host::ForkPublicationRequest,
        source_capture::{
            CapturedOrigin, FORK_STAGING_CAPACITY_BYTES, ForkBoundaryRequest, ForkReadBudget,
            SourceCaptureControl,
        },
    },
    flow_store::{
        FlowDescriptor, FlowRef, SessionDescriptor,
        launch::{EntryBinding, EntryRequest, EntryStart, LaunchInput},
        local_operator::LocalOperator,
        source_capture::{
            ForkConfirmation, ForkCutoff, ForkImportPlan, ForkOperation, ForkOperationStatus,
        },
    },
    storage::ProjectContext,
};
use serde_json::Value;

use super::super::{SessionService, address, core_address, events, execution, history_wire, wire};
use crate::DaemonState;

pub(in crate::flow_service) fn validate_create(
    request: &CreateForkRequest,
) -> Result<(), JsonRpcError> {
    super::validate(&request.review)?;
    let observed = &request.review.observed;
    let within = |cursor: &FlowCursor| {
        cursor.journal_id == observed.cursor.journal_id
            && (1..=observed.cursor.sequence).contains(&cursor.sequence)
            && (cursor.sequence != observed.cursor.sequence || cursor == &observed.cursor)
    };
    if request.operation_id.is_nil()
        || request.source.flow != observed.flow
        || request.source.branch != observed.branch
        || !within(&request.source.cursor)
        || request.catalog.journal_id != observed.flow.session_id.0
        || request.catalog.sequence == 0
        || ![&request.catalog, &request.source.cursor]
            .into_iter()
            .all(|cursor| blake3::Hash::from_hex(&cursor.digest).is_ok())
        || request.asset_copy_bytes > FORK_STAGING_CAPACITY_BYTES
        || request.title.trim().is_empty()
        || request.title.len() > 256
        || request.title.chars().any(char::is_control)
    {
        return Err(JsonRpcError::invalid_params(
            "Fork requires an exact source review and destination",
        ));
    }
    Ok(())
}

pub(in crate::flow_service) fn validate_status(
    request: &ForkStatusRequest,
) -> Result<(), JsonRpcError> {
    if request.operation_id.is_nil() {
        return Err(JsonRpcError::invalid_params(
            "Fork operation identity is empty",
        ));
    }
    Ok(())
}

pub(in crate::flow_service) async fn status(
    state: &DaemonState,
    request: ForkStatusRequest,
) -> Result<Value> {
    let operation = ForkOperation::new(
        state.data_dir(),
        LocalOperator::new(state.operator_id()),
        request.operation_id,
    )?;
    let observed = operation.observe().await?;
    check_confirmation(&observed, &core_address(request.source.clone()), None)?;
    response(request.operation_id, request.source, &operation, observed)
}

pub(crate) async fn completed_retry(
    state: &DaemonState,
    request: &CreateForkRequest,
) -> Result<Option<Value>, JsonRpcError> {
    validate_create(request)?;
    let reconcile = async {
        let source = &request.review.observed.flow;
        let operation = ForkOperation::new(
            state.data_dir(),
            LocalOperator::new(state.operator_id()),
            request.operation_id,
        )?;
        let observed = operation.observe().await?;
        if matches!(observed, ForkOperationStatus::Unseen) {
            return Ok(None);
        }
        let digest = confirmation_digest(request)?;
        check_confirmation(&observed, &core_address(source.clone()), Some(&digest))?;
        if matches!(observed, ForkOperationStatus::Ready { .. }) {
            return Ok(Some(response(
                request.operation_id,
                source.clone(),
                &operation,
                observed,
            )?));
        }
        Ok(None)
    };
    reconcile.await.map_err(wire::application_error)
}

pub(in crate::flow_service) async fn create(
    state: &Arc<DaemonState>,
    service: &SessionService,
    request: CreateForkRequest,
) -> Result<Value> {
    let observed_flow = request.review.observed.flow.clone();
    let source = core_address(observed_flow.clone());
    let operation_id = request.operation_id;
    let operation = ForkOperation::new(
        state.data_dir(),
        LocalOperator::new(state.operator_id()),
        operation_id,
    )?;
    let request_digest = confirmation_digest(&request)?;
    let previous = operation.observe().await?;
    check_confirmation(&previous, &source, Some(&request_digest))?;
    if matches!(previous, ForkOperationStatus::Ready { .. }) {
        return response(operation_id, observed_flow, &operation, previous);
    }
    let guard = match operation.try_lock().await {
        Ok(guard) => guard,
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
            let concurrent = operation.observe().await?;
            check_confirmation(&concurrent, &source, Some(&request_digest))?;
            return response(operation_id, observed_flow, &operation, concurrent);
        }
        Err(error) => return Err(error.into()),
    };
    let current = operation.observe().await?;
    check_confirmation(&current, &source, Some(&request_digest))?;
    if matches!(current, ForkOperationStatus::Ready { .. }) {
        return response(operation_id, observed_flow, &operation, current);
    }

    let _host_operation = service.host.retain_host_operation()?;
    service.host.flow(&source).await?;
    let allowed_source = source.clone();
    let captured = service
        .host
        .prepare_fork_import(
            ForkBoundaryRequest {
                observed: history_wire::core_source(request.review.observed),
                expected_head: wire::core_head(request.review.expected_head),
                cutoff: request
                    .review
                    .cutoff
                    .map_or(ForkCutoff::LatestSafe, |cutoff| {
                        ForkCutoff::At(Box::new(history_wire::core_cutoff(cutoff)))
                    }),
            },
            ForkReadBudget::default(),
            state
                .data_dir()
                .join("runtime-v1")
                .join("host")
                .join("fork-staging"),
            request.asset_copy_bytes,
            SourceCaptureControl::default(),
            move |flow| {
                if flow == &allowed_source {
                    Ok(())
                } else {
                    Err(io::ErrorKind::PermissionDenied.into())
                }
            },
        )
        .await?;
    let expected_source_catalog = events::receipt(request.catalog);
    ensure!(
        captured.source == history_wire::core_source(request.source)
            && captured.origin.catalog == expected_source_catalog,
        "Fork source or project changed after review"
    );
    let source_entry = captured.product.entry();
    let original = source_entry
        .execution
        .as_ref()
        .context("Fork source has no recorded execution binding")?;
    let destination = operation.destination();
    let (confirmation, _guard) = match current {
        ForkOperationStatus::Preparing(confirmation)
        | ForkOperationStatus::RetryRequired(confirmation) => (confirmation, guard),
        ForkOperationStatus::Unseen => {
            let confirmation = destination_confirmation(
                state,
                &captured.origin,
                source.clone(),
                &destination,
                request_digest,
                request.location,
                request.title,
            )
            .await?;
            let guard = guard.accept(confirmation.clone()).await?;
            (confirmation, guard)
        }
        ForkOperationStatus::Ready { .. } => {
            unreachable!("published Fork was reconciled before capture")
        }
    };
    check_directory(confirmation.working_directory.clone()).await?;
    if let Some(root) = original.authority.filesystem_root() {
        ensure!(
            confirmation.working_directory.starts_with(root),
            "Fork destination is outside the source entry's filesystem scope"
        );
    }
    service
        .host
        .create_session(state.data_dir(), confirmation.descriptor.clone())
        .await?;
    let entry = FlowDescriptor {
        address: destination.clone(),
        creation_request: operation_id,
        content: source_entry.descriptor.content.clone(),
        created_by: None,
    };
    let (_, destination_authority, _) = service
        .prepare_entry(
            state,
            &confirmation.descriptor,
            &entry,
            confirmation.descriptor.project.as_ref(),
            Some(&confirmation.working_directory),
            execution::launcher_home(state),
        )
        .await?;
    let mut authority = original
        .authority
        .for_child(&destination_authority, destination_authority.shell, None)
        .map_err(anyhow::Error::msg)?;
    authority.workspace_root = Some(confirmation.working_directory.clone());
    check_directory(confirmation.working_directory.clone()).await?;
    let plan = ForkImportPlan::from_retained_snapshot(
        operation_id,
        destination.clone(),
        confirmation.descriptor.project,
        confirmation.working_directory,
        captured.product.snapshot(),
    )?;
    let input = LaunchInput {
        name: None,
        display_label: None,
        scope: None,
        workspace: None,
        initial_goal: None,
        request: EntryRequest {
            request_id: operation_id,
            sender: operation.sender(),
            recipient: destination.clone(),
            arguments: Default::default(),
            invocation: Default::default(),
            start: EntryStart::Waiting,
        },
        inherited_context: Vec::new(),
        inherited_outputs: Vec::new(),
        continued_from: None,
        result_route: None,
        execution: Some(EntryBinding {
            user_routes: original.user_routes,
            session_start: false,
            first_turn: atman_rt::TurnId(destination.instance_id.0),
            first_run: atman_rt::RunId(destination.instance_id.0),
            authority,
        }),
    };
    service
        .host
        .publish_fork_import(
            ForkPublicationRequest {
                expected_source_catalog,
                captured,
                plan,
                input,
            },
            move |descriptor, _| {
                if *descriptor == entry {
                    Ok(())
                } else {
                    Err(io::ErrorKind::PermissionDenied.into())
                }
            },
        )
        .await?;
    let published = operation.observe().await?;
    ensure!(
        matches!(published, ForkOperationStatus::Ready { .. }),
        "Fork outcome is pending reconciliation"
    );
    response(operation_id, observed_flow, &operation, published)
}

fn confirmation_digest(request: &CreateForkRequest) -> Result<String> {
    Ok(blake3::hash(&serde_json::to_vec(request)?)
        .to_hex()
        .to_string())
}

fn check_confirmation(
    status: &ForkOperationStatus,
    source: &FlowRef,
    digest: Option<&str>,
) -> Result<()> {
    let confirmation = match status {
        ForkOperationStatus::Unseen => return Ok(()),
        ForkOperationStatus::Preparing(confirmation)
        | ForkOperationStatus::RetryRequired(confirmation)
        | ForkOperationStatus::Ready { confirmation, .. } => confirmation,
    };
    ensure!(
        &confirmation.source == source
            && digest.is_none_or(|expected| confirmation.request_digest == expected),
        "Fork operation conflicts with its original confirmation"
    );
    Ok(())
}

fn response(
    id: uuid::Uuid,
    source: FlowAddress,
    operation: &ForkOperation,
    status: ForkOperationStatus,
) -> Result<Value> {
    let progress = match status {
        ForkOperationStatus::Unseen => ForkProgress::Unseen,
        ForkOperationStatus::Preparing(_) => ForkProgress::Preparing {
            destination: address(&operation.destination()),
        },
        ForkOperationStatus::RetryRequired(_) => ForkProgress::RetryRequired {
            destination: address(&operation.destination()),
        },
        ForkOperationStatus::Ready {
            activation,
            publication_digest,
            ..
        } => ForkProgress::Ready {
            destination: address(&operation.destination()),
            activation: events::cursor(&activation),
            publication_digest: publication_digest.to_hex().to_string(),
        },
    };
    Ok(serde_json::to_value(ForkOperationResponse {
        operation_id: id,
        source,
        progress,
    })?)
}

async fn destination_confirmation(
    state: &DaemonState,
    origin: &CapturedOrigin,
    source: FlowRef,
    destination: &FlowRef,
    request_digest: String,
    location: ForkLocation,
    title: String,
) -> Result<ForkConfirmation> {
    let recorded = origin
        .execution
        .as_ref()
        .and_then(|binding| binding.authority.workspace_root.as_deref());
    let (project, cwd) = match location {
        ForkLocation::SourceProject => {
            let project = origin
                .session_project
                .clone()
                .context("Source Session has no project")?;
            let cwd = recorded
                .filter(|path| {
                    origin.flow_project.as_ref() == Some(&project)
                        && path.starts_with(project.root())
                })
                .unwrap_or_else(|| project.root())
                .to_owned();
            (Some(project), cwd)
        }
        ForkLocation::SourceWorkingDirectory => {
            let cwd = recorded
                .context("Source Flow has no recorded working directory")?
                .to_owned();
            (
                origin
                    .flow_project
                    .clone()
                    .filter(|project| cwd.starts_with(project.root())),
                cwd,
            )
        }
    };
    let data = state.data_dir().to_owned();
    let descriptor = SessionDescriptor {
        id: destination.session_id.0,
        configuration: origin.configuration.clone(),
        project,
        title,
    };
    tokio::task::spawn_blocking(move || {
        validate_directory(&cwd)?;
        let mut descriptor = descriptor;
        let config = execution::config(&descriptor)?;
        if descriptor.project.is_none() {
            descriptor.project = Some(ProjectContext::resolve(&config, &cwd, &data)?);
        }
        Ok(ForkConfirmation {
            request_digest,
            source,
            descriptor,
            working_directory: cwd,
        })
    })
    .await?
}

async fn check_directory(path: PathBuf) -> Result<()> {
    tokio::task::spawn_blocking(move || validate_directory(&path)).await?
}

fn validate_directory(path: &Path) -> Result<()> {
    ensure!(
        path.is_absolute() && path.canonicalize()? == path && std::fs::metadata(path)?.is_dir(),
        "Reviewed Fork working directory is unavailable or changed"
    );
    Ok(())
}
