//! Read-only relations between durable flow instances.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::access::{AccessTarget, FlowAction};
use super::{FlowRef, SessionProjection, subscription::SubscriptionStatus};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKind {
    Created,
    Read,
    Watch,
    Interject,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FlowRelation {
    pub source: FlowRef,
    pub target: AccessTarget,
    pub kind: RelationKind,
    /// Stable source fact identity when one exists: creation request, grant, or subscription.
    pub id: Uuid,
    pub active: bool,
}

impl FlowRelation {
    pub(super) fn created(source: FlowRef, target: FlowRef, id: Uuid) -> Self {
        Self {
            source,
            target: AccessTarget::Flow(target),
            kind: RelationKind::Created,
            id,
            active: true,
        }
    }

    pub(super) fn access(
        source: FlowRef,
        target: AccessTarget,
        kind: RelationKind,
        id: Uuid,
    ) -> Self {
        Self {
            source,
            target,
            kind,
            id,
            active: true,
        }
    }

    pub(super) fn watch(source: FlowRef, target: FlowRef, id: Uuid, active: bool) -> Self {
        Self {
            source,
            target: AccessTarget::Flow(target),
            kind: RelationKind::Watch,
            id,
            active,
        }
    }
}

pub(super) fn access_relations(projection: &SessionProjection) -> Vec<FlowRelation> {
    let mut relations = Vec::new();
    for record in projection.access.grants() {
        if record.revoked {
            continue;
        }
        for kind in record
            .grant
            .actions
            .iter()
            .filter_map(|action| match action {
                FlowAction::ReadOutput | FlowAction::ReadContext => Some(RelationKind::Read),
                FlowAction::Send => Some(RelationKind::Interject),
                FlowAction::Watch => Some(RelationKind::Watch),
                _ => None,
            })
        {
            relations.push(FlowRelation::access(
                record.grant.grantee.clone(),
                record.grant.target.clone(),
                kind,
                record.grant.id,
            ));
        }
    }
    relations
}

pub(super) fn watch_active(status: SubscriptionStatus) -> bool {
    status == SubscriptionStatus::Active
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::flow_store::{SessionConfiguration, SessionDescriptor, access::FlowGrant};
    use atman_rt::FlowInstanceId;

    #[test]
    fn access_projection_exposes_read_watch_and_interject_without_revoked_grants() {
        let session = Uuid::now_v7();
        let source = FlowRef {
            session_id: session.into(),
            instance_id: FlowInstanceId::now(),
        };
        let target = FlowRef {
            session_id: session.into(),
            instance_id: FlowInstanceId::now(),
        };
        let mut actions = BTreeSet::new();
        actions.extend([FlowAction::ReadContext, FlowAction::Send, FlowAction::Watch]);
        let grant = FlowGrant {
            id: Uuid::now_v7(),
            grantee: source.clone(),
            target: AccessTarget::Flow(target.clone()),
            actions,
        };
        let descriptor = SessionDescriptor {
            id: session,
            project: None,
            configuration: SessionConfiguration::HostProvided,
            title: "relations".into(),
        };
        let mut projection = SessionProjection {
            descriptor,
            title: Some("relations".into()),
            flows: Vec::new(),
            default_flow: None,
            access: Default::default(),
            migrated_from: None,
        };
        projection.access.grant(grant).unwrap();
        let relations = access_relations(&projection);
        assert_eq!(relations.len(), 3);
        assert!(
            relations
                .iter()
                .any(|relation| relation.kind == RelationKind::Read)
        );
        assert!(
            relations
                .iter()
                .any(|relation| relation.kind == RelationKind::Watch)
        );
        assert!(
            relations
                .iter()
                .any(|relation| relation.kind == RelationKind::Interject)
        );
        assert!(relations.iter().all(|relation| relation.active));
    }
}
