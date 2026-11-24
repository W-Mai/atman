//! Human submissions and canonical conversation startup through the original owners.

use std::sync::Arc;

use anyhow::{Result, ensure};
use atman_proto::{
    flows::{BootstrapSessionRequest, FlowUserContent, SubmitFlowRequest, SubmittedUserContent},
    responses::SubmitFlowResponse,
};
use atman_runtime::{
    flow_runtime::launch::EntryExecution,
    flow_store::{
        FlowDescriptor, FlowRef,
        inbox::{DeliveryMode, SenderIdentity, UserContent},
        launch::{EntryBinding, EntryRequest, LaunchInput, LaunchRecord},
        local_operator::LocalOperator,
    },
    peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan},
    user_input::{
        HumanInputAuthorization, HumanInputRequest, HumanInputService, PreparedConversationEntry,
        QuoteSnapshot, UserInputPresentation, prepare_conversation_entry,
    },
};

use super::{SessionService, address, core_address, execution, wire};
use crate::DaemonState;

#[cfg(test)]
mod tests;

pub(super) enum BootstrapScope {
    Session,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Workspace {
        proof: Box<atman_runtime::flow_store::profile::workspace_open::WorkspaceBootstrap>,
        reader: atman_runtime::project_catalog::workspace::WorkspaceReader,
    },
}

impl BootstrapScope {
    async fn verify(&self, _catalog: &atman_runtime::flow_store::SessionProjection) -> Result<()> {
        match self {
            Self::Session => Ok(()),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Workspace { proof, reader } => {
                ensure!(proof.matches(_catalog), "Workspace Session binding changed");
                Ok(proof.verify(reader.clone()).await?)
            }
        }
    }
}

pub(super) fn authored_content(input: SubmittedUserContent) -> Result<UserContent> {
    let user = FlowUserContent {
        text: input.text,
        images: input.images,
    };
    let mut content = execution::user_content(Some(&user))?.expect("provided user content");
    if let Some(quote) = input.quote {
        let presentation = UserInputPresentation {
            prompt: content.text,
            quote: Some(QuoteSnapshot { text: quote }),
        };
        content.text = presentation.model_text();
        content.presentation = Some(presentation);
    }
    Ok(content)
}

pub(crate) async fn submit_device_input(
    state: Arc<DaemonState>,
    request: SubmitFlowRequest,
    operator: LocalOperator,
    authorization: HumanInputAuthorization,
    forwarded: bool,
) -> Result<serde_json::Value> {
    ensure!(!state.is_stopping(), "daemon is stopping");
    let service = super::service(&state);
    let _operation = service.host.retain_host_operation()?;
    let session = request.flow.session_id.0;
    if service.host.session(session).is_err() {
        ensure!(
            !forwarded,
            "execution owner changed; refresh host discovery"
        );
        service.attach(session, None).await?;
    }
    service
        .submit_as(&state, request, operator, Some(authorization))
        .await
}

impl SessionService {
    pub(super) async fn submit(
        &self,
        state: &Arc<DaemonState>,
        request: SubmitFlowRequest,
    ) -> Result<serde_json::Value> {
        self.submit_as(state, request, LocalOperator::new(self.operator), None)
            .await
    }

    async fn submit_as(
        &self,
        state: &Arc<DaemonState>,
        request: SubmitFlowRequest,
        operator: LocalOperator,
        authorization: Option<HumanInputAuthorization>,
    ) -> Result<serde_json::Value> {
        let _operation = self.host.retain_host_operation()?;
        let flow = core_address(request.flow);
        let session = self.session(flow.session_id.0)?;
        let _owner = session.retain_operation().await?;
        let worker = self.host.flow(&flow).await?;
        let request = HumanInputRequest {
            request_id: request.request_id,
            content: authored_content(request.user)?,
            invocation: request.invocation,
            delivery: DeliveryMode::NextTurn,
            expected_head: request.expected_head.map(wire::core_head),
        };
        let observe = || {
            HumanInputService::observe_accepted(&self.host, &session, &worker, operator, &request)
        };
        let prepare = async {
            let descriptor = session.snapshot().await?.catalog.descriptor;
            let config = execution::config(&descriptor)?;
            let project = worker.project().await?;
            let executor = self
                .executor(
                    state,
                    &descriptor,
                    project.as_ref(),
                    execution::launcher_home(state),
                )
                .await?;
            Ok::<_, anyhow::Error>(HumanInputService::new(
                self.host.clone(),
                session.clone(),
                worker.clone(),
                executor,
                config,
                project,
                execution::YIELD_INTERVAL,
            ))
        };
        let resume = async |accepted: &atman_runtime::flow_store::inbox::Acceptance| {
            HumanInputService::resume_retained_input(&self.host, &session, &worker, accepted).await
        };
        let accepted = match observe().await? {
            Some(accepted) => {
                if resume(&accepted).await? {
                    prepare.await?.bind().await?;
                }
                accepted
            }
            None => match prepare.await {
                Ok(service) => {
                    service
                        .submit_with_authorization(operator, request.clone(), authorization)
                        .await?
                }
                Err(error) => {
                    let Some(accepted) = observe().await? else {
                        return Err(error);
                    };
                    if resume(&accepted).await? {
                        return Err(error.context("accepted input is waiting for its scheduler"));
                    }
                    accepted
                }
            },
        };
        Ok(serde_json::to_value(SubmitFlowResponse {
            flow: address(&flow),
            request_id: request.request_id,
            acceptance: wire::acceptance(&accepted)?,
        })?)
    }

    pub(super) async fn bootstrap(
        &self,
        state: &Arc<DaemonState>,
        request: BootstrapSessionRequest,
    ) -> Result<serde_json::Value> {
        self.bootstrap_scoped(state, request, BootstrapScope::Session)
            .await
    }

    pub(super) async fn bootstrap_scoped(
        &self,
        state: &Arc<DaemonState>,
        request: BootstrapSessionRequest,
        scope: BootstrapScope,
    ) -> Result<serde_json::Value> {
        let _operation = matches!(scope, BootstrapScope::Session)
            .then(|| self.host.retain_host_operation())
            .transpose()?;
        let session = self.session(request.session_id.0)?;
        let _owner = session.retain_operation().await?;
        let flow = FlowRef {
            session_id: request.session_id.0.into(),
            instance_id: atman_rt::FlowInstanceId(request.session_id.0.into()),
        };
        let snapshot = session.snapshot().await?;
        scope.verify(&snapshot.catalog).await?;
        let registered = snapshot
            .catalog
            .flows
            .iter()
            .find(|row| row.id == flow.instance_id);
        let retained = if registered.is_some() {
            let worker = self.host.flow(&flow).await?;
            let descriptor = worker.descriptor().await?;
            ensure!(
                descriptor.creation_request == request.session_id.0
                    && descriptor.created_by.is_none(),
                "canonical conversation conflicts with its recorded creation"
            );
            if let Some(record) = worker.launch_record().await? {
                validate_conversation(&record, request.session_id.0)?;
                return Ok(serde_json::to_value(wire::launch(
                    address(&flow),
                    &record,
                )?)?);
            }
            Some(descriptor)
        } else {
            ensure!(
                snapshot.cursor == super::events::receipt(request.expected_catalog),
                "session catalog changed; inspect it before bootstrapping"
            );
            None
        };
        ensure!(
            snapshot
                .catalog
                .flows
                .iter()
                .all(|row| row.id == flow.instance_id)
                && snapshot
                    .catalog
                    .default_flow
                    .as_ref()
                    .is_none_or(|id| id == &flow.instance_id),
            "bootstrap requires an empty Session or its retained canonical entry"
        );
        let descriptor = snapshot.catalog.descriptor.clone();
        let project = registered.map_or_else(
            || snapshot.catalog.project.clone(),
            |row| row.project.clone(),
        );
        let config = execution::config(&descriptor)?;
        let executor = self
            .executor(
                state,
                &descriptor,
                project.as_ref(),
                execution::launcher_home(state),
            )
            .await?;
        let authority = match self.host.authority_domain() {
            Some(domain) => Some(
                domain
                    .acquire(
                        AuthorityPlan::new([
                            (AuthorityKey::Profile, AuthorityMode::Exclusive),
                            (
                                AuthorityKey::Session(session.id()),
                                AuthorityMode::Exclusive,
                            ),
                        ])?,
                        tokio_util::sync::CancellationToken::new(),
                        std::time::Instant::now() + std::time::Duration::from_secs(10),
                    )
                    .await?,
            ),
            None => None,
        };
        ensure!(
            self.host.session(session.id())?.same_owner(&session),
            "conversation preparation requires its original Session owner"
        );
        let source = retained
            .as_ref()
            .map(FlowDescriptor::entry)
            .transpose()?
            .cloned();
        scope.verify(&snapshot.catalog).await?;
        let PreparedConversationEntry {
            entry,
            start,
            trust,
            authority,
        } = tokio::task::spawn_blocking(move || {
            let _authority = authority;
            prepare_conversation_entry(
                &config,
                &descriptor,
                project.as_ref(),
                source.as_ref(),
                true,
                None,
            )
        })
        .await??;
        let descriptor = retained.unwrap_or(FlowDescriptor {
            address: flow.clone(),
            creation_request: request.session_id.0,
            created_by: None,
            content: entry.into(),
        });
        let input = LaunchInput {
            name: None,
            display_label: None,
            scope: None,
            workspace: None,
            initial_goal: None,
            execution: Some(EntryBinding {
                user_routes: true,
                session_start: true,
                first_turn: atman_rt::TurnId(request.session_id.0.into()),
                first_run: atman_rt::RunId(request.session_id.0.into()),
                authority: authority.clone(),
            }),
            request: EntryRequest {
                request_id: request.session_id.0,
                sender: SenderIdentity::Host(self.operator),
                recipient: flow.clone(),
                arguments: Default::default(),
                invocation: Default::default(),
                start,
            },
            inherited_outputs: vec![],
            continued_from: None,
            inherited_context: vec![],
            result_route: None,
        };
        let launched = self
            .host
            .launch_entry(
                descriptor,
                input,
                EntryExecution {
                    executor,
                    authority,
                    trust,
                    yield_interval: execution::YIELD_INTERVAL,
                    catalog: snapshot.cursor,
                },
                |_, _| Ok(()),
            )
            .await?;
        Ok(serde_json::to_value(wire::launch(
            address(&flow),
            &launched.record,
        )?)?)
    }
}

fn validate_conversation(record: &LaunchRecord, session: uuid::Uuid) -> Result<()> {
    ensure!(
        record.input.execution.is_some(),
        "canonical conversation has no entry binding"
    );
    ensure!(
        record.input.is_canonical_session_start(session),
        "canonical conversation conflicts with its recorded launch"
    );
    Ok(())
}
