//! Source capture shares owner authorization while payload work stays on bounded read workers.

use std::{io, sync::Arc};

pub use crate::flow_store::source_capture::{
    BoundaryReferenceRequest, BoundaryReferenceSnapshot, CapturedOrigin, ConversationBranchId,
    ConversationCutoff, ForkBoundaryRequest, ForkReadBudget, PreparedReference, ReferenceBudget,
    ReferenceOmissions, ReferenceRequest, ReferenceSnapshot, ResolvedConversationBoundary,
    RetainedForkSnapshot, SourceCapture, SourceCaptureControl, SourceSelection,
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
        self.capture_source_with(
            request.flow.clone(),
            authorize,
            move |store| store.capture_reference_plan(&request),
            ReferencePlan::prepare,
        )
        .await
    }

    /// Resolve a stable turn boundary and capture its bounded reference in one owner request.
    pub async fn capture_reference_at_boundary(
        &self,
        request: BoundaryReferenceRequest,
        authorize: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<SourceCapture<BoundaryReferenceSnapshot>> {
        request.budget.validate()?;
        self.capture_source_with(
            request.observed.flow.clone(),
            authorize,
            move |store| store.capture_reference_boundary_plan(&request),
            |(plan, boundary)| {
                let (source, reference) = plan.prepare()?;
                Ok((
                    source,
                    BoundaryReferenceSnapshot {
                        boundary,
                        reference,
                    },
                ))
            },
        )
        .await
    }

    /// Prepare an independently owned Fork snapshot while the source Session remains retained. This does not accept or create a destination.
    pub async fn capture_fork_snapshot(
        &self,
        request: ForkBoundaryRequest,
        read_budget: ForkReadBudget,
        staging_parent: std::path::PathBuf,
        max_asset_bytes: usize,
        authorize: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<SourceCapture<RetainedForkSnapshot>> {
        self.capture_fork_snapshot_with_control(
            request,
            read_budget,
            staging_parent,
            max_asset_bytes,
            SourceCaptureControl::default(),
            authorize,
        )
        .await
    }

    /// Controlled Fork capture for native clients. The reservation fences concurrent staging and
    /// remains owned by the returned snapshot until its scratch directory is retired.
    #[allow(dead_code, clippy::too_many_arguments)]
    pub async fn capture_fork_snapshot_with_control(
        &self,
        request: ForkBoundaryRequest,
        read_budget: ForkReadBudget,
        staging_parent: std::path::PathBuf,
        max_asset_bytes: usize,
        control: SourceCaptureControl,
        authorize: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + 'static,
    ) -> io::Result<SourceCapture<RetainedForkSnapshot>> {
        if control.is_cancelled() {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "source capture cancelled",
            ));
        }
        let capacity = self.fork_staging_capacity(&staging_parent)?;
        let reservation = capacity.reserve(max_asset_bytes)?;
        let copy_control = control.copy_control(staging_parent.clone());
        self.capture_source_with(
            request.observed.flow.clone(),
            authorize,
            move |store| store.capture_fork_conversation(&request, read_budget, |_| Ok(())),
            move |plan| {
                let snapshot = plan.prepare_retained_with_control(
                    &staging_parent,
                    max_asset_bytes,
                    copy_control,
                    Some(reservation),
                )?;
                Ok((snapshot.data().boundary.source.clone(), snapshot))
            },
        )
        .await
    }

    async fn capture_source_with<Plan: Send + 'static, Product: Send + 'static>(
        &self,
        flow: FlowRef,
        authorize: impl Fn(&FlowRef) -> io::Result<()> + Send + Sync + 'static,
        select: impl FnOnce(&FlowStore) -> io::Result<Plan> + Send + 'static,
        prepare: impl FnOnce(Plan) -> io::Result<(SourceSelection, Product)> + Send + 'static,
    ) -> io::Result<SourceCapture<Product>> {
        authorize(&flow)?;
        let session = self.session(flow.session_id.0)?;
        let retention = session.retain_source().await?;
        let catalog = session.snapshot().await?;
        let worker = session.loaded_flow(&flow.instance_id).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "open the source Flow before capturing its conversation",
            )
        })?;
        let authorize = Arc::new(authorize);
        let owner_check = authorize.clone();
        let (plan, origin) = worker
            .request(move |store| {
                owner_check(store.state().address())?;
                let plan = select(store)?;
                let origin = store.source_capture_origin(catalog.cursor, catalog.catalog.project);
                Ok((plan, origin))
            })
            .await?;
        crate::read_work::capture(move || {
            let result = prepare(plan).and_then(|(source, product)| {
                authorize(&source.flow)?;
                Ok(SourceCapture {
                    source,
                    origin,
                    product,
                })
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

    fn flow_descriptor(address: FlowRef) -> FlowDescriptor {
        FlowDescriptor {
            address,
            creation_request: Uuid::now_v7(),
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
        }
    }

    #[tokio::test]
    async fn fork_capture_rechecks_authority_before_releasing_source_retention() {
        use crate::{
            attachment_store::{AttachmentStore, image_bytes},
            flow_store::{
                FlowTransition, completion::InvocationResult, publication::MessagePublication,
                source_capture::ForkCutoff,
            },
            message::{Message, MessagePart},
        };
        use std::sync::atomic::{AtomicUsize, Ordering};
        let data = tempfile::tempdir().unwrap();
        let staging = tempfile::tempdir().unwrap();
        let host = RuntimeHost::default();
        let descriptor = session_descriptor();
        let session = host
            .create_session(data.path(), descriptor.clone())
            .await
            .unwrap();
        let address = FlowRef {
            session_id: descriptor.id.into(),
            instance_id: FlowInstanceId::now(),
        };
        let worker = session
            .create_flow(flow_descriptor(address.clone()))
            .await
            .unwrap();
        let request = worker
            .request(|store| {
                store.transition(store.state().revision(), FlowTransition::Activated)?;
                store.transition(
                    store.state().revision(),
                    FlowTransition::TurnStarted {
                        turn: atman_rt::TurnId::now(),
                        run: atman_rt::RunId::now(),
                    },
                )?;
                let atman_rt::InstanceState::Open(atman_rt::InstanceActivity::Active(active)) =
                    store.state().state()
                else {
                    panic!("active")
                };
                let lease = active.lease.clone();
                let source = AttachmentStore::at(store.dir().unwrap())
                    .import_bytes(b"\x89PNG\r\n\x1a\n", None)
                    .unwrap();
                store.publish_message(MessagePublication {
                    id: Uuid::now_v7(),
                    lease: lease.clone(),
                    run: lease.run.clone(),
                    context_run: None,
                    message: Message {
                        parts: vec![MessagePart::Image { source }],
                        ..Message::assistant_text(lease.turn.clone(), "")
                    },
                })?;
                store.finish_turn(
                    lease,
                    InvocationResult::Unobserved(atman_rt::TurnOutcome::Completed),
                )?;
                Ok(ForkBoundaryRequest {
                    observed: SourceSelection {
                        flow: store.state().address().clone(),
                        branch: store.history_head().branch,
                        cursor: store.cursor().require_durable()?.clone(),
                    },
                    expected_head: store.history_head(),
                    cutoff: ForkCutoff::LatestSafe,
                })
            })
            .await
            .unwrap();
        let before = session.snapshot().await.unwrap().cursor;
        let (ready, observed) = tokio::sync::oneshot::channel();
        let ready = std::sync::Mutex::new(Some(ready));
        let (release, waiting) = std::sync::mpsc::channel();
        let waiting = std::sync::Mutex::new(waiting);
        let calls = Arc::new(AtomicUsize::new(0));
        let checked = calls.clone();
        let capturing = host.clone();
        let scratch = staging.path().to_owned();
        let captured_request = request.clone();
        let capture = tokio::spawn(async move {
            capturing
                .capture_fork_snapshot(
                    captured_request,
                    ForkReadBudget::default(),
                    scratch,
                    1024,
                    move |_| {
                        if checked.fetch_add(1, Ordering::SeqCst) == 2 {
                            let _ = ready.lock().unwrap().take().unwrap().send(());
                            waiting
                                .lock()
                                .unwrap()
                                .recv_timeout(std::time::Duration::from_secs(5))
                                .unwrap();
                        }
                        Ok(())
                    },
                )
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), observed)
            .await
            .unwrap()
            .unwrap();
        let deleting = host.clone();
        let source_data = data.path().to_owned();
        let mut deletion = tokio::spawn(async move {
            deleting
                .close_and_delete_session(
                    &source_data,
                    SessionRemoval {
                        session_id: descriptor.id,
                        expected: before,
                    },
                )
                .await
        });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut deletion)
                .await
                .is_err()
        );
        // The blocked publication check runs on a storage reader, not the UI/runtime or catalog owner.
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            host.create_session(data.path(), session_descriptor()),
        )
        .await
        .unwrap()
        .unwrap();
        release.send(()).unwrap();
        let captured = capture.await.unwrap().unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), deletion)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(captured.source, request.observed);
        let publication = captured
            .product
            .data()
            .records
            .iter()
            .find_map(|entry| match &entry.record {
                crate::projection::flow::FlowRecord::Publication(record) => Some(record),
                _ => None,
            })
            .unwrap();
        let MessagePart::Image { source } = &publication.message.parts[0] else {
            panic!("image")
        };
        assert_eq!(image_bytes(source).unwrap(), b"\x89PNG\r\n\x1a\n");
        host.shutdown().await.unwrap();
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
