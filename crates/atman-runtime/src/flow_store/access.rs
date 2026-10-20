//! Durable peer access grants, independent of creation and view bindings.

use std::{
    collections::{BTreeMap, BTreeSet},
    io,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{FlowRef, SessionFact, SessionStore, invalid};
use crate::event_log::journal::CommitReceipt;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FlowAction {
    Discover,
    ReadOutput,
    ReadContext,
    Send,
    Watch,
    Control,
    ManageGoal,
    /// Review another instance's pending tool approvals; never implied by creation or self access.
    ApprovePermission,
    Delegate,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "target", rename_all = "snake_case")]
pub enum AccessTarget {
    Flow(FlowRef),
    Session(Uuid),
    /// Only sessions managed by the same trusted host; no cross-user or network authority.
    AllSessions,
}

impl AccessTarget {
    fn covers(&self, target: &Self) -> bool {
        match (self, target) {
            (Self::AllSessions, _) => true,
            (Self::Session(left), Self::Session(right)) => left == right,
            (Self::Session(session), Self::Flow(flow)) => *session == flow.session_id.0,
            (Self::Flow(left), Self::Flow(right)) => left == right,
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FlowGrant {
    pub id: Uuid,
    pub grantee: FlowRef,
    pub target: AccessTarget,
    pub actions: BTreeSet<FlowAction>,
}

impl FlowGrant {
    pub(crate) fn allows_scope(
        &self,
        source: &FlowRef,
        target: &AccessTarget,
        action: FlowAction,
    ) -> bool {
        self.grantee == *source && self.target.covers(target) && self.actions.contains(&action)
    }
    pub(crate) fn allows(&self, source: &FlowRef, target: &FlowRef, action: FlowAction) -> bool {
        self.allows_scope(source, &AccessTarget::Flow(target.clone()), action)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GrantRecord {
    pub grant: FlowGrant,
    pub revoked: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct SessionAccess {
    grants: BTreeMap<Uuid, GrantRecord>,
}

impl SessionAccess {
    pub(crate) fn allows_scope(
        &self,
        source: &FlowRef,
        target: &AccessTarget,
        action: FlowAction,
    ) -> bool {
        if let AccessTarget::Flow(flow) = target {
            return self.allows(source, flow, action);
        }
        if matches!(target, AccessTarget::Session(id) if *id == source.session_id.0)
            && action == FlowAction::Discover
        {
            return true;
        }
        self.grants
            .values()
            .any(|record| !record.revoked && record.grant.allows_scope(source, target, action))
    }
    pub fn grants(&self) -> impl Iterator<Item = &GrantRecord> {
        self.grants.values()
    }

    /// The host authenticates the source and resolves the target before using this policy.
    pub fn allows(&self, source: &FlowRef, target: &FlowRef, action: FlowAction) -> bool {
        if source == target
            && !matches!(action, FlowAction::Delegate | FlowAction::ApprovePermission)
        {
            return true;
        }
        if source.session_id == target.session_id
            && matches!(
                action,
                FlowAction::Discover
                    | FlowAction::ReadOutput
                    | FlowAction::Send
                    | FlowAction::Watch
            )
        {
            return true;
        }
        self.grants
            .values()
            .any(|record| !record.revoked && record.grant.allows(source, target, action))
    }

    pub(super) fn grant(&mut self, grant: FlowGrant) -> io::Result<()> {
        if grant.id.is_nil() || grant.actions.is_empty() || self.grants.contains_key(&grant.id) {
            return Err(invalid("invalid or duplicate access grant"));
        }
        self.grants.insert(
            grant.id,
            GrantRecord {
                grant,
                revoked: false,
            },
        );
        Ok(())
    }

    pub(super) fn revoke(&mut self, id: Uuid) -> io::Result<()> {
        let record = self
            .grants
            .get_mut(&id)
            .ok_or_else(|| invalid("access grant does not exist"))?;
        if record.revoked {
            return Err(invalid("access grant is already revoked"));
        }
        record.revoked = true;
        Ok(())
    }
}

impl SessionStore {
    /// Trusted host operation; peer delegation must separately prove its authority ceiling.
    pub fn grant_access(
        &mut self,
        expected: &CommitReceipt,
        grant: FlowGrant,
    ) -> io::Result<CommitReceipt> {
        if let Some(previous) = self.projection.access.grants.get(&grant.id) {
            if previous.grant != grant || previous.revoked {
                return Err(invalid(
                    "access grant identity conflicts with its recorded disposition",
                ));
            }
            return Ok(self.cursor.clone());
        }
        if expected != &self.cursor {
            return Err(invalid("session access revision changed"));
        }
        self.commit(vec![SessionFact::AccessGranted(grant)])
    }

    pub fn revoke_access(
        &mut self,
        expected: &CommitReceipt,
        id: Uuid,
    ) -> io::Result<CommitReceipt> {
        if self
            .projection
            .access
            .grants
            .get(&id)
            .is_some_and(|record| record.revoked)
        {
            return Ok(self.cursor.clone());
        }
        if expected != &self.cursor {
            return Err(invalid("session access revision changed"));
        }
        self.commit(vec![SessionFact::AccessRevoked(id)])
    }
}
