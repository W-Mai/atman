use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, RuntimeStore, SessionDescriptor,
    access::{AccessTarget, FlowAction, FlowGrant},
    profile::{ManagementBinding, ProfileStore, ReplaceManagement},
};
use atman_runtime::source_program::RevisionBundle;
use uuid::Uuid;

#[tokio::test]
async fn control_owner_retains_abandoned_requests_without_enabling_invalid_management() {
    use atman_runtime::flow_runtime::host::RuntimeHost;
    use atman_runtime::flow_store::{
        control::{ControlAction, ControlRequest},
        inbox::SenderIdentity,
        profile::{ControlBatchRequest, ControlBatchUpdate, ControlProgress},
    };
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let mut store = ProfileStore::open(&root).unwrap();
    let missing = store
        .bootstrap(Default::default())
        .unwrap()
        .management
        .unwrap()
        .session
        .id;
    let original = root.root().join("sessions").join(missing.to_string());
    std::fs::rename(&original, data.path().join("missing-session")).unwrap();
    let host = RuntimeHost::default();
    let profile = host.attach_profile(store).unwrap();
    let unavailable = profile.snapshot().await.unwrap();
    assert!(unavailable.unavailable.is_some());
    let sender = SenderIdentity::User(Uuid::new_v4());
    let request = ControlBatchRequest {
        id: Uuid::new_v4(),
        sender: sender.clone(),
        commands: vec![ControlRequest {
            id: Uuid::new_v4(),
            sender: sender.clone(),
            target: FlowRef {
                session_id: Uuid::new_v4().into(),
                instance_id: FlowInstanceId::now(),
            },
            expected_revision: 1,
            pause_key: Uuid::new_v4().into(),
            action: ControlAction::Pause,
        }],
    };
    let (sent, received) = tokio::sync::oneshot::channel();
    let (release, gate) = std::sync::mpsc::channel();
    let submitted = request.clone();
    let owner = profile.clone();
    let waiting = tokio::spawn(async move {
        owner
            .submit_controls(submitted, move |_| {
                let _ = sent.send(());
                gate.recv().map_err(std::io::Error::other)?;
                Ok(())
            })
            .await
    });
    received.await.unwrap();
    waiting.abort();
    let _ = waiting.await;
    release.send(()).unwrap();
    let accepted = profile
        .control_batch(sender.clone(), request.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        profile.pending_controls().await.unwrap().as_slice(),
        std::slice::from_ref(&accepted)
    );
    let after = profile.snapshot().await.unwrap();
    assert_eq!(after.unavailable, unavailable.unavailable);
    assert_eq!(after.profile.management.unwrap().session.id, missing);
    assert!(after.profile.cursor.sequence > unavailable.profile.cursor.sequence);
    assert!(
        profile
            .submit_controls(request.clone(), |_| Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "revoked"
            )))
            .await
            .is_err()
    );
    assert_eq!(
        profile.snapshot().await.unwrap().profile.cursor,
        after.profile.cursor
    );
    assert_eq!(
        profile
            .submit_controls(request.clone(), |_| Ok(()))
            .await
            .unwrap(),
        accepted
    );
    let update = ControlBatchUpdate {
        batch_id: request.id,
        sender: sender.clone(),
        target: request.commands[0].target.clone(),
        progress: ControlProgress::Failed {
            reason: "target is unavailable".into(),
        },
    };
    let receipt = profile.advance_control(update.clone()).await.unwrap();
    assert_eq!(profile.advance_control(update).await.unwrap(), receipt);
    assert!(profile.pending_controls().await.unwrap().is_empty());
    assert_eq!(
        profile.snapshot().await.unwrap().unavailable,
        unavailable.unavailable
    );
    let finished = profile
        .control_batch(sender.clone(), request.id)
        .await
        .unwrap()
        .unwrap();
    host.shutdown().await.unwrap();
    assert!(
        profile
            .control_batch(sender.clone(), request.id)
            .await
            .is_err()
    );
    let store = ProfileStore::open(&root).unwrap();
    assert_eq!(store.control_batch(&sender, request.id).unwrap(), &finished);
    assert!(store.pending_controls().is_empty());
    assert!(
        !original.exists(),
        "control progress must not repair missing management storage"
    );
}

#[test]
fn control_batches_reconcile_partial_delivery_and_preserve_terminal_receipts() {
    use atman_runtime::flow_store::{
        FlowTransition,
        control::{ControlAction, ControlRequest},
        inbox::SenderIdentity,
        profile::{ControlBatchRequest, ControlBatchUpdate, ControlProgress},
    };
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let (a_session, a) = session(&root);
    let (b_session, b) = session(&root);
    let mut first = a_session.open_flow(&a.instance_id).unwrap();
    let mut second = b_session.open_flow(&b.instance_id).unwrap();
    for flow in [&mut first, &mut second] {
        flow.transition(flow.state().revision(), FlowTransition::Activated)
            .unwrap();
    }
    let sender = SenderIdentity::User(Uuid::new_v4());
    let key = Uuid::new_v4().into();
    let commands = [&first, &second]
        .into_iter()
        .zip([ControlAction::Pause, ControlAction::Close])
        .map(|(flow, action)| ControlRequest {
            id: Uuid::new_v4(),
            sender: sender.clone(),
            target: flow.state().address().clone(),
            expected_revision: flow.state().revision(),
            pause_key: key,
            action,
        })
        .collect();
    let request = ControlBatchRequest {
        id: Uuid::new_v4(),
        sender: sender.clone(),
        commands,
    };
    let progress = |target: FlowRef, progress| ControlBatchUpdate {
        batch_id: request.id,
        sender: sender.clone(),
        target,
        progress,
    };
    let mut profile = ProfileStore::open(&root).unwrap();
    let batch = profile.submit_controls(request.clone()).unwrap();
    assert!(batch.items.iter().all(|item| item.history.is_empty()));
    assert!(
        profile.snapshot().management.is_none(),
        "host controls must not bootstrap a management conversation"
    );
    let paused = first.control(request.commands[0].clone()).unwrap();
    let accepted_a = progress(
        a.clone(),
        ControlProgress::Accepted {
            receipt: paused.receipt.clone(),
        },
    );
    let a_receipt = profile.advance_control(accepted_a.clone()).unwrap();
    profile
        .advance_control(progress(
            a.clone(),
            ControlProgress::Applied {
                receipt: paused.receipt.clone(),
            },
        ))
        .unwrap();

    // The target commits closure, but the host loses its acknowledgement before recording it.
    let closing = second.control(request.commands[1].clone()).unwrap();
    drop(profile);
    drop(second);
    drop(first);
    let mut profile = ProfileStore::open(&root).unwrap();
    assert_eq!(profile.pending_controls().len(), 1);
    let pending = profile.pending_controls()[0];
    assert_eq!(pending.submitted, batch.submitted);
    assert!(pending.items[0].is_terminal());
    assert!(pending.items[1].history.is_empty());
    let mut second = b_session.open_flow(&b.instance_id).unwrap();
    assert_eq!(
        second.control(request.commands[1].clone()).unwrap(),
        closing
    );
    let accepted_b = progress(
        b.clone(),
        ControlProgress::Accepted {
            receipt: closing.receipt,
        },
    );
    let b_receipt = profile.advance_control(accepted_b.clone()).unwrap();
    assert!(!profile.pending_controls()[0].items[1].is_terminal());
    let closed = second
        .transition(second.state().revision(), FlowTransition::Closed)
        .unwrap();
    profile
        .advance_control(progress(b, ControlProgress::Applied { receipt: closed }))
        .unwrap();
    let cursor = profile.snapshot().cursor.clone();
    assert_eq!(profile.advance_control(accepted_a).unwrap(), a_receipt);
    assert_eq!(profile.advance_control(accepted_b).unwrap(), b_receipt);
    assert_eq!(profile.snapshot().cursor, cursor);
    assert!(profile.pending_controls().is_empty());
    let finished = profile.submit_controls(request.clone()).unwrap();
    assert_eq!(finished.submitted, batch.submitted);
    assert!(finished.items.iter().all(|item| item.is_terminal()));
    drop(profile);
    let mut profile = ProfileStore::open(&root).unwrap();
    assert_eq!(profile.submit_controls(request).unwrap(), finished);
    assert_eq!(profile.snapshot().cursor, cursor);
}

#[test]
fn control_batches_reject_conflicts_and_keep_independent_target_failures() {
    use atman_runtime::flow_store::{
        control::{ControlAction, ControlRequest},
        inbox::SenderIdentity,
        profile::{ControlBatchRequest, ControlBatchUpdate, ControlProgress},
    };
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let (_a_session, a) = session(&root);
    let (_b_session, b) = session(&root);
    let sender = SenderIdentity::User(Uuid::new_v4());
    let request = ControlBatchRequest {
        id: Uuid::new_v4(),
        sender: sender.clone(),
        commands: [a.clone(), b.clone()]
            .into_iter()
            .map(|target| ControlRequest {
                id: Uuid::new_v4(),
                sender: sender.clone(),
                target,
                expected_revision: 1,
                pause_key: Uuid::new_v4().into(),
                action: ControlAction::Pause,
            })
            .collect(),
    };
    let mut profile = ProfileStore::open(&root).unwrap();
    let before = profile.snapshot().cursor.clone();
    let mut duplicate = request.clone();
    duplicate.commands.push(duplicate.commands[0].clone());
    assert!(profile.submit_controls(duplicate).is_err());
    let mut wrong_actor = request.clone();
    wrong_actor.commands[0].sender = SenderIdentity::User(Uuid::new_v4());
    assert!(profile.submit_controls(wrong_actor).is_err());
    let mut nil_actor = request.clone();
    nil_actor.sender = SenderIdentity::User(Uuid::nil());
    for command in &mut nil_actor.commands {
        command.sender = nil_actor.sender.clone();
    }
    assert!(profile.submit_controls(nil_actor).is_err());
    assert_eq!(profile.snapshot().cursor, before);
    profile.submit_controls(request.clone()).unwrap();
    let mut conflict = request.clone();
    conflict.commands[0].action = ControlAction::Close;
    assert!(profile.submit_controls(conflict).is_err());
    let mut update = ControlBatchUpdate {
        batch_id: request.id,
        sender: sender.clone(),
        target: a,
        progress: ControlProgress::Failed {
            reason: "target revision changed".into(),
        },
    };
    let failed = profile.advance_control(update.clone()).unwrap();
    let saved = profile.control_batch(&sender, request.id).unwrap().clone();
    assert!(saved.items[0].is_terminal());
    assert!(!saved.items[1].is_terminal());
    update.progress = ControlProgress::Failed {
        reason: "different error".into(),
    };
    assert!(profile.advance_control(update).is_err());
    let mut update = ControlBatchUpdate {
        batch_id: request.id,
        sender: sender.clone(),
        target: b.clone(),
        progress: ControlProgress::Applied {
            receipt: before.clone(),
        },
    };
    assert!(profile.advance_control(update.clone()).is_err());
    let mut target_receipt = before;
    target_receipt.journal_id = b.instance_id.0.0;
    update.progress = ControlProgress::Applied {
        receipt: target_receipt.clone(),
    };
    assert!(
        profile.advance_control(update.clone()).is_err(),
        "application needs recorded target acceptance"
    );
    update.progress = ControlProgress::Accepted {
        receipt: target_receipt,
    };
    profile.advance_control(update.clone()).unwrap();
    update.progress = ControlProgress::Failed {
        reason: "cleanup failed".into(),
    };
    profile.advance_control(update).unwrap();
    let finished = profile.control_batch(&sender, request.id).unwrap().clone();
    assert_eq!(finished.items[0], saved.items[0]);
    assert_eq!(
        finished.items[1].history.len(),
        2,
        "failure retains accepted effects"
    );
    assert!(profile.pending_controls().is_empty());
    assert_eq!(finished.items[0].history[0].recorded, failed);
    let cursor = profile.snapshot().cursor.clone();
    let journal = root.root().join("host/events.jsonl");
    let bytes = std::fs::read(&journal).unwrap();
    assert!(
        profile
            .submit_controls(ControlBatchRequest {
                commands: Vec::new(),
                ..request.clone()
            })
            .is_err()
    );
    assert_eq!(profile.snapshot().cursor, cursor);
    assert_eq!(std::fs::read(journal).unwrap(), bytes);
    drop(profile);
    let mut profile = ProfileStore::open(&root).unwrap();
    assert_eq!(
        profile.control_batch(&sender, finished.id).unwrap(),
        &finished
    );
    let mut other_actor = request;
    other_actor.sender = SenderIdentity::User(Uuid::new_v4());
    for command in &mut other_actor.commands {
        command.sender = other_actor.sender.clone();
    }
    let separate = profile.submit_controls(other_actor).unwrap();
    assert_eq!(separate.id, finished.id);
    assert_eq!(profile.pending_controls(), [&separate]);
    assert_eq!(
        profile.control_batch(&sender, finished.id).unwrap(),
        &finished
    );
}

fn session(root: &RuntimeStore) -> (atman_runtime::flow_store::SessionStore, FlowRef) {
    let mut session = root
        .create_session(SessionDescriptor {
            id: Uuid::new_v4(),
            project: None,
            configuration: Default::default(),
            title: "Atman".into(),
        })
        .unwrap();
    let address = FlowRef {
        session_id: session.projection().descriptor.id.into(),
        instance_id: FlowInstanceId::now(),
    };
    session
        .create_flow(FlowDescriptor {
            address: address.clone(),
            creation_request: Uuid::new_v4(),
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: "main.at".into(),
                bundle: RevisionBundle {
                    entry: "entry:main.at".into(),
                    sources: [("entry:main.at".into(), "flow main() { return 1 }".into())].into(),
                    edges: Default::default(),
                },
            }
            .into(),
        })
        .unwrap();
    (session, address)
}

#[tokio::test]
async fn management_relation_inspection_uses_the_current_acknowledged_grants() {
    use atman_runtime::flow_runtime::catalog::{FlowCatalogEntry, OverviewExecution};
    use atman_runtime::flow_runtime::host::RuntimeHost;
    use atman_runtime::flow_store::relation::RelationKind;

    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let mut owner = ProfileStore::open(&root).unwrap();
    owner.bootstrap(Default::default()).unwrap();
    let (_source_owner, source) = session(&root);
    let (_target_owner, target) = session(&root);
    let grant = FlowGrant {
        id: Uuid::new_v4(),
        grantee: source.clone(),
        target: AccessTarget::Flow(target.clone()),
        actions: [
            FlowAction::ReadContext,
            FlowAction::ReadOutput,
            FlowAction::Watch,
        ]
        .into(),
    };
    owner
        .select(
            Uuid::new_v4(),
            owner.snapshot().cursor.clone(),
            source.session_id.0,
            vec![grant.clone()],
        )
        .unwrap();
    let host = RuntimeHost::default();
    let profile = host.attach_profile(owner).unwrap();
    let catalog = host.session_catalog();
    let relations = catalog.relations(source.session_id.0).await.unwrap();
    assert_eq!(relations.len(), 2);
    assert!(
        relations
            .iter()
            .any(|edge| edge.kind == RelationKind::Watch)
    );
    assert!(relations.iter().any(|edge| edge.kind == RelationKind::Read));
    assert!(
        relations.iter().all(|edge| edge.id == grant.id
            && edge.source == source
            && edge.target == grant.target)
    );
    assert!(
        catalog
            .relations(target.session_id.0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(host.sessions().is_empty());

    let overview = catalog
        .overview_page(None, 50, |_| Ok(true), |_| Ok(()))
        .await
        .unwrap();
    for entry in &overview.entries {
        let FlowCatalogEntry::Flow {
            owner,
            snapshot: Ok(snapshot),
            ..
        } = entry
        else {
            panic!("missing overview record")
        };
        assert!(matches!(snapshot.execution, OverviewExecution::Detached));
        if owner == &source {
            assert_eq!(snapshot.relations, relations);
        } else {
            assert!(snapshot.relations.is_empty());
        }
    }
    assert_eq!(overview.entries.len(), 2);

    profile
        .select(
            Uuid::new_v4(),
            profile.snapshot().await.unwrap().profile.cursor,
            source.session_id.0,
            Vec::new(),
        )
        .await
        .unwrap();
    assert!(
        catalog
            .relations(source.session_id.0)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(host.sessions().is_empty());
    let overview = catalog
        .overview_page(None, 50, |_| Ok(true), |_| Ok(()))
        .await
        .unwrap();
    assert!(overview.entries.iter().all(|entry| matches!(entry, FlowCatalogEntry::Flow { snapshot: Ok(snapshot), .. } if snapshot.relations.is_empty())));
    host.shutdown().await.unwrap();
}

#[test]
fn management_bootstrap_is_exclusive_projectless_and_does_not_create_work() {
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let mut owner = ProfileStore::open(&root).unwrap();
    assert!(ProfileStore::open(&root).is_err());
    let snapshot = owner.bootstrap(Default::default()).unwrap();
    let binding = snapshot.management.clone().unwrap();
    assert!(binding.grants.is_empty());
    assert!(binding.session.project.is_none());
    let catalog = root.open_session(binding.session.id).unwrap();
    assert!(catalog.projection().flows.is_empty());
    assert!(catalog.projection().default_flow.is_none());
    assert_eq!(
        owner.bootstrap(Default::default()).unwrap().cursor,
        snapshot.cursor
    );
    drop(owner);
    let mut owner = ProfileStore::open(&root).unwrap();
    let reopened = owner.bootstrap(Default::default()).unwrap();
    assert_eq!(reopened.profile_id, snapshot.profile_id);
    assert_eq!(reopened.management, Some(binding));
    assert_eq!(reopened.cursor, snapshot.cursor);
    assert_eq!(RuntimeStore::session_ids(data.path()).unwrap().len(), 1);
}

#[test]
fn management_replacement_changes_binding_and_grants_once_and_retries_original_receipt() {
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let mut owner = ProfileStore::open(&root).unwrap();
    owner.bootstrap(Default::default()).unwrap();
    let (catalog, source) = session(&root);
    let (_target_catalog, target) = session(&root);
    let grant = FlowGrant {
        id: Uuid::new_v4(),
        grantee: source.clone(),
        target: AccessTarget::AllSessions,
        actions: [FlowAction::Discover, FlowAction::Watch].into(),
    };
    let request = ReplaceManagement {
        request_id: Uuid::new_v4(),
        expected: owner.snapshot().cursor.clone(),
        binding: ManagementBinding {
            session: catalog.projection().descriptor.clone(),
            grants: vec![grant],
        },
    };
    let accepted = owner.replace(request.clone()).unwrap();
    let binding = owner.snapshot().management.as_ref().unwrap();
    assert!(binding.allows(&source, &target, FlowAction::Discover));
    assert!(binding.allows(&source, &target, FlowAction::Watch));
    assert!(!binding.allows(&source, &target, FlowAction::Control));
    assert!(!binding.allows(&target, &source, FlowAction::Discover));
    let mut unknown = request.clone();
    unknown.request_id = Uuid::new_v4();
    unknown.expected = accepted.clone();
    unknown.binding.grants[0].grantee.instance_id = FlowInstanceId::now();
    assert!(owner.replace(unknown).is_err());
    let mut replacement = request.clone();
    replacement.request_id = Uuid::new_v4();
    assert!(owner.replace(replacement.clone()).is_err());
    replacement.expected = accepted.clone();
    replacement.binding.grants.clear();
    let removed = owner.replace(replacement.clone()).unwrap();
    assert_eq!(owner.replace(request.clone()).unwrap(), accepted);
    assert_eq!(owner.snapshot().cursor, removed);
    assert!(!owner.snapshot().management.as_ref().unwrap().allows(
        &source,
        &target,
        FlowAction::Watch
    ));
    drop(owner);
    let mut owner = ProfileStore::open(&root).unwrap();
    assert_eq!(owner.replace(request.clone()).unwrap(), accepted);
    assert_eq!(owner.replace(replacement).unwrap(), removed);
    let mut conflict = request;
    conflict.binding.grants.clear();
    assert!(owner.replace(conflict).is_err());
    assert_eq!(owner.snapshot().cursor, removed);
    assert_eq!(
        root.open_session(target.session_id.0).err().unwrap().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn missing_management_container_requires_explicit_replacement() {
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let mut owner = ProfileStore::open(&root).unwrap();
    let bound = owner.bootstrap(Default::default()).unwrap();
    let original = bound.management.unwrap().session.id;
    std::fs::remove_dir_all(root.root().join("sessions").join(original.to_string())).unwrap();
    drop(owner);
    let mut owner = ProfileStore::open(&root).unwrap();
    assert!(owner.bootstrap(Default::default()).is_err());
    assert!(RuntimeStore::session_ids(data.path()).unwrap().is_empty());
    let (catalog, _) = session(&root);
    owner
        .replace(ReplaceManagement {
            request_id: Uuid::new_v4(),
            expected: owner.snapshot().cursor.clone(),
            binding: ManagementBinding {
                session: catalog.projection().descriptor.clone(),
                grants: Vec::new(),
            },
        })
        .unwrap();
    assert_ne!(
        owner
            .bootstrap(Default::default())
            .unwrap()
            .management
            .unwrap()
            .session
            .id,
        original
    );
}

#[test]
fn corrupt_profile_identity_or_journal_is_not_reinitialized() {
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let owner = ProfileStore::open(&root).unwrap();
    let id = owner.snapshot().profile_id;
    drop(owner);
    let identity = root.root().join("host/identity.json");
    let original = std::fs::read(&identity).unwrap();
    std::fs::write(
        &identity,
        format!(r#"{{"version":99,"profile_id":"{id}"}}"#),
    )
    .unwrap();
    assert!(ProfileStore::open(&root).is_err());
    std::fs::write(identity, original).unwrap();
    let journal = root.root().join("host/events.jsonl");
    std::fs::write(&journal, "broken\n").unwrap();
    assert!(ProfileStore::open(&root).is_err());
    assert_eq!(std::fs::read_to_string(journal).unwrap(), "broken\n");
}
