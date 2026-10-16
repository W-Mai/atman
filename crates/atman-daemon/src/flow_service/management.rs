//! Authenticated operator management of the profile binding. Opening the container does not launch its flows.

use anyhow::{Result, anyhow};
use atman_proto::flows::{ManagementAction, ManagementTarget, SelectManagementRequest};
use atman_runtime::{
    flow_runtime::host::ProfileHandle,
    flow_store::{
        FlowRef, RuntimeStore, SessionConfiguration,
        access::{AccessTarget, FlowAction, FlowGrant},
        profile::ProfileStore,
    },
};
use serde_json::{Value, json};

use super::{SessionService, core_address, describe_session, events};
use crate::DaemonState;

impl SessionService {
    pub(super) async fn profile(&self) -> Result<&ProfileHandle> {
        let owner = self
            .profile
            .get_or_init(|| async {
                let data = self.data_dir.clone();
                let result = async {
                    let store = tokio::task::spawn_blocking(move || {
                        ProfileStore::open(&RuntimeStore::open(&data)?)
                    })
                    .await??;
                    Ok::<_, anyhow::Error>(self.host.attach_profile(store)?)
                }
                .await;
                if let Err(error) = &result {
                    eprintln!("management profile unavailable: {error}");
                }
                result.map_err(|error| error.to_string())
            })
            .await
            .as_ref()
            .map_err(|error| anyhow!(error.clone()))?;
        // Cache ownership before awaiting validation. A cancelled caller must not leave an attached owner outside the initialization slot.
        owner.snapshot().await?;
        Ok(owner)
    }

    pub(super) async fn open_management(&self, state: &DaemonState) -> Result<Value> {
        let config_dir = state
            .launcher()
            .and_then(|launcher| launcher.config_dir.clone());
        let source = crate::bootstrap::resolve_config_hub(config_dir.as_deref())?.source();
        let view = self
            .profile()
            .await?
            .bootstrap(SessionConfiguration::Atman(source))
            .await?;
        let binding = view
            .profile
            .management
            .as_ref()
            .ok_or_else(|| anyhow!("management binding is absent"))?;
        let session = self.attach(binding.session.id, None).await?;
        Ok(
            json!({"profile": view.profile, "unavailable": view.unavailable, "session": describe_session(&session).await?}),
        )
    }

    pub(super) async fn select_management(
        &self,
        request: SelectManagementRequest,
    ) -> Result<Value> {
        let session_id = request.session_id.0;
        let grants = request
            .grants
            .into_iter()
            .map(|grant| FlowGrant {
                id: grant.id,
                grantee: FlowRef {
                    session_id: session_id.into(),
                    instance_id: atman_rt::FlowInstanceId(grant.instance_id.into()),
                },
                target: match grant.target {
                    ManagementTarget::AllSessions => AccessTarget::AllSessions,
                    ManagementTarget::Session(session) => AccessTarget::Session(session.0),
                    ManagementTarget::Flow(flow) => AccessTarget::Flow(core_address(flow)),
                },
                actions: grant
                    .actions
                    .into_iter()
                    .map(|action| match action {
                        ManagementAction::Discover => FlowAction::Discover,
                        ManagementAction::ReadOutput => FlowAction::ReadOutput,
                        ManagementAction::ReadContext => FlowAction::ReadContext,
                        ManagementAction::Send => FlowAction::Send,
                        ManagementAction::Watch => FlowAction::Watch,
                        ManagementAction::Control => FlowAction::Control,
                        ManagementAction::ManageGoal => FlowAction::ManageGoal,
                        ManagementAction::Delegate => FlowAction::Delegate,
                    })
                    .collect(),
            })
            .collect();
        let profile = self.profile().await?;
        let receipt = profile
            .select(
                request.request_id,
                events::receipt(request.expected),
                session_id,
                grants,
            )
            .await?;
        Ok(json!({"receipt": receipt, "current": profile.snapshot().await?}))
    }
}
