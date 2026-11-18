//! Human submissions and canonical conversation startup through the original owners.

use std::sync::Arc;

use anyhow::{Context, Result, ensure};
use atman_proto::{
    flows::{BootstrapSessionRequest, FlowUserContent, SubmitFlowRequest},
    responses::SubmitFlowResponse,
};
use atman_runtime::{
    flow_runtime::launch::EntryExecution,
    flow_store::{
        FlowDescriptor, FlowRef,
        inbox::{DeliveryMode, SenderIdentity},
        launch::{EntryBinding, EntryRequest, LaunchInput, LaunchRecord},
        local_operator::LocalOperator,
    },
    peer_authority::{AuthorityKey, AuthorityMode, AuthorityPlan},
    user_input::{
        HumanInputRequest, HumanInputService, PreparedConversationEntry, QuoteSnapshot,
        UserInputPresentation, prepare_conversation_entry,
    },
};

use super::{SessionService, address, core_address, execution, wire};
use crate::DaemonState;

#[cfg(test)]
mod tests;

impl SessionService {
    pub(super) async fn submit(
        &self,
        state: &Arc<DaemonState>,
        request: SubmitFlowRequest,
    ) -> Result<serde_json::Value> {
        let _operation = self.host.retain_host_operation()?;
        let flow = core_address(request.flow);
        let session = self.session(flow.session_id.0)?;
        let _owner = session.retain_operation().await?;
        let worker = self.host.flow(&flow).await?;
        let input = FlowUserContent {
            text: request.user.text,
            images: request.user.images,
        };
        let mut content = execution::user_content(Some(&input))?.expect("provided user content");
        if let Some(quote) = request.user.quote {
            let presentation = UserInputPresentation {
                prompt: content.text,
                quote: Some(QuoteSnapshot { text: quote }),
            };
            content.text = presentation.model_text();
            content.presentation = Some(presentation);
        }
        let request = HumanInputRequest {
            request_id: request.request_id,
            content,
            invocation: request.invocation,
            delivery: DeliveryMode::NextTurn,
            expected_head: request.expected_head.map(wire::core_head),
        };
        let operator = LocalOperator::new(self.operator);
        let accepted =
            HumanInputService::observe_accepted(&self.host, &session, &worker, operator, &request)
                .await?;
        let accepted = match accepted {
            Some(accepted) => accepted,
            None => {
                let result = async {
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
                    HumanInputService::new(
                        self.host.clone(),
                        session.clone(),
                        worker.clone(),
                        executor,
                        config,
                        project,
                        execution::YIELD_INTERVAL,
                    )
                    .submit(operator, request.clone())
                    .await
                }
                .await;
                match result {
                    Ok(accepted) => accepted,
                    Err(error) => HumanInputService::observe_accepted(
                        &self.host, &session, &worker, operator, &request,
                    )
                    .await?
                    .ok_or(error)?,
                }
            }
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
        let _operation = self.host.retain_host_operation()?;
        let session = self.session(request.session_id.0)?;
        let _owner = session.retain_operation().await?;
        let flow = FlowRef {
            session_id: request.session_id.0.into(),
            instance_id: atman_rt::FlowInstanceId(request.session_id.0.into()),
        };
        let snapshot = session.snapshot().await?;
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
    let binding = record
        .input
        .execution
        .as_ref()
        .context("canonical conversation has no entry binding")?;
    ensure!(
        binding.user_routes
            && binding.session_start
            && binding.first_turn.0.0 == session
            && binding.first_run.0.0 == session
            && record.input.request.request_id == session
            && matches!(record.input.request.sender, SenderIdentity::Host(_))
            && record.input.continued_from.is_none()
            && record.input.scope.is_none()
            && record.input.workspace.is_none()
            && record.input.result_route.is_none(),
        "canonical conversation conflicts with its recorded launch"
    );
    Ok(())
}
