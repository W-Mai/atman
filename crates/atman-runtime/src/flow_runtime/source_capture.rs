//! Source capture shares owner authorization while payload work stays on bounded read workers.

use std::{io, sync::Arc};

pub use crate::flow_store::source_capture::{
    BoundaryReferenceRequest, BoundaryReferenceSnapshot, CapturedOrigin, ConversationBranchId,
    ConversationCutoff, PreparedReference, ReferenceBudget, ReferenceOmissions, ReferenceRequest,
    ReferenceSnapshot, ResolvedConversationBoundary, SourceCapture, SourceSelection,
};

use super::host::RuntimeHost;
use crate::flow_store::{FlowRef, FlowStore, source_capture::ReferencePlan};

impl RuntimeHost {
    /// Capture a bounded durable reference without executing its source or inheriting control state.
    /// The trusted adapter authenticates the caller; authority is rechecked in the owner and before delivery.
    pub async fn capture_reference(
        &self,
        request: ReferenceRequest,
        authorize: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<SourceCapture<ReferenceSnapshot>> {
        request.budget.validate()?;
        self.capture_reference_with(request.flow.clone(), authorize, move |store| {
            Ok((store.capture_reference_plan(&request)?, ()))
        })
        .await
        .map(|(capture, ())| capture)
    }

    /// Resolve a stable turn boundary and capture its bounded reference in one owner request.
    pub async fn capture_reference_at_boundary(
        &self,
        request: BoundaryReferenceRequest,
        authorize: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<SourceCapture<BoundaryReferenceSnapshot>> {
        request.budget.validate()?;
        let (capture, boundary) = self
            .capture_reference_with(request.observed.flow.clone(), authorize, move |store| {
                store.capture_reference_boundary_plan(&request)
            })
            .await?;
        Ok(SourceCapture {
            source: capture.source,
            origin: capture.origin,
            product: BoundaryReferenceSnapshot {
                boundary,
                reference: capture.product,
            },
        })
    }

    async fn capture_reference_with<Metadata: Send + 'static>(
        &self,
        flow: FlowRef,
        authorize: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + 'static,
        prepare: impl FnOnce(&FlowStore) -> io::Result<(ReferencePlan, Metadata)> + Send + 'static,
    ) -> io::Result<(SourceCapture<ReferenceSnapshot>, Metadata)> {
        authorize(&flow)?;
        let session = self.session(flow.session_id.0)?;
        let retention = session.retain_source().await?;
        let catalog = session.snapshot().await?;
        let worker = session.loaded_flow(&flow.instance_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "open the source Flow before capturing a reference",
            )
        })?;
        let authorize = Arc::new(authorize);
        let owner_check = authorize.clone();
        let (plan, metadata) = worker
            .request(move |store| {
                owner_check(store.state().address())?;
                prepare(store)
            })
            .await?;
        let origin = CapturedOrigin {
            catalog: catalog.cursor,
            session_project: catalog.catalog.project,
            flow_project: plan.flow_project.clone(),
            configuration: plan.configuration.clone(),
            execution: plan.execution.clone(),
        };
        crate::read_work::capture(move || {
            let result = plan.prepare().and_then(|(source, product)| {
                authorize(&source.flow)?;
                Ok((
                    SourceCapture {
                        source,
                        origin,
                        product,
                    },
                    metadata,
                ))
            });
            // Source deletion cannot race artifact reads, but returned data owns no source lease.
            drop(retention);
            result
        })
        .await
        .map_err(io::Error::other)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        flow_store::{
            EntrySnapshot, FlowDescriptor, FlowInstanceId, RuntimeStore, SessionDescriptor,
            SessionRemoval,
        },
        source_program::RevisionBundle,
    };
    use std::collections::BTreeMap;
    use uuid::Uuid;

    fn session_descriptor() -> SessionDescriptor {
        SessionDescriptor {
            configuration: Default::default(),
            id: Uuid::new_v4(),
            project: None,
            title: "Source".into(),
        }
    }

    #[tokio::test]
    async fn cold_reference_capture_does_not_open_an_owner_or_append_facts() {
        let dir = tempfile::tempdir().unwrap();
        let root = RuntimeStore::open(dir.path()).unwrap();
        let info = session_descriptor();
        let mut store = root.create_session(info.clone()).unwrap();
        let address = FlowRef {
            session_id: info.id.into(),
            instance_id: FlowInstanceId::now(),
        };
        let flow = store
            .create_flow(FlowDescriptor {
                address: address.clone(),
                creation_request: Uuid::new_v4(),
                created_by: None,
                content: EntrySnapshot {
                    name: "main".into(),
                    path: "/missing/project/main.at".into(),
                    bundle: RevisionBundle {
                        entry: "entry:main.at".into(),
                        sources: BTreeMap::from([(
                            "entry:main.at".into(),
                            "flow main() -> int { return 7 }".into(),
                        )]),
                        edges: BTreeMap::new(),
                    },
                }
                .into(),
            })
            .unwrap();
        let path = flow.dir().unwrap().join("events.jsonl");
        let before = std::fs::read(&path).unwrap();
        let cursor = flow.cursor().require_durable().unwrap().clone();
        drop(flow);
        let host = RuntimeHost::default();
        let session = host.attach_session(store).unwrap();
        let result = host
            .capture_reference(ReferenceRequest::current(address.clone()), |_| Ok(()))
            .await;
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::NotFound));
        let result = host
            .capture_reference_at_boundary(
                BoundaryReferenceRequest {
                    observed: SourceSelection {
                        flow: address.clone(),
                        branch: ConversationBranchId::initial(&address, cursor.journal_id),
                        cursor: cursor.clone(),
                    },
                    cutoff: ConversationCutoff::AfterCompletedTurn {
                        lease: atman_rt::TurnLease {
                            flow: address.clone(),
                            turn: atman_rt::TurnId::now(),
                            run: atman_rt::RunId::now(),
                            epoch: 1,
                        },
                        receipt: cursor,
                    },
                    budget: ReferenceBudget::default(),
                },
                |_| Ok(()),
            )
            .await;
        assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::NotFound));
        assert!(session.loaded_flow(&address.instance_id).is_none());
        assert_eq!(std::fs::read(path).unwrap(), before);
        host.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn source_retention_fences_deletion_without_blocking_other_session_opens() {
        let dir = tempfile::tempdir().unwrap();
        let host = RuntimeHost::default();
        let info = session_descriptor();
        let session = host.create_session(dir.path(), info.clone()).await.unwrap();
        let retention = session.retain_source().await.unwrap();
        let request = SessionRemoval {
            session_id: info.id,
            expected: session.snapshot().await.unwrap().cursor,
        };
        let deleting = host.clone();
        let data = dir.path().to_owned();
        let mut task = tokio::spawn(async move { deleting.delete_session(&data, request).await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut task)
                .await
                .is_err()
        );
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            host.create_session(dir.path(), session_descriptor()),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(session.snapshot().await.is_ok());
        drop(retention);
        tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(session.retain_source().await.is_err());
        host.shutdown().await.unwrap();
    }
}
