//! Host ownership and persistence boundaries for temporary conversation execution.

use super::*;
use crate::flow_runtime::launch::EntryExecution;
use crate::flow_store::launch::{EntryBinding, EntryRequest, EntryStart, LaunchInput};
use crate::flow_store::{EntrySnapshot, FlowRef};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

fn files(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, dir: &Path, files: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect(root, &path, files);
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().into(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    collect(root, root, &mut result);
    result
}

#[tokio::test]
async fn transient_owner_reconciles_executes_and_discards_without_catalog_or_files() {
    let data = tempfile::tempdir().unwrap();
    let host = RuntimeHost::default();
    let id = Uuid::now_v7();
    let session = host
        .create_session(
            data.path(),
            SessionDescriptor {
                id,
                configuration: Default::default(),
                project: None,
                title: "Temporary".into(),
            },
        )
        .await
        .unwrap();
    let descriptor = FlowDescriptor {
        address: FlowRef {
            session_id: id.into(),
            instance_id: FlowInstanceId::now(),
        },
        creation_request: Uuid::now_v7(),
        created_by: None,
        content: EntrySnapshot {
            name: "main".into(),
            path: "main.at".into(),
            bundle: crate::source_program::RevisionBundle {
                entry: "entry:main.at".into(),
                edges: Default::default(),
                sources: [(
                    "entry:main.at".into(),
                    "flow main() -> int { return 42 }".into(),
                )]
                .into(),
            },
        }
        .into(),
    };
    let mut source = descriptor.clone();
    source.address.instance_id = FlowInstanceId::now();
    source.creation_request = Uuid::now_v7();
    let source = session.create_flow(source).await.unwrap();
    let catalog = session.snapshot().await.unwrap().cursor;
    let authority =
        crate::flow_authority::EffectiveAuthority::root(&Default::default(), false, None)
            .read_only();
    let input = LaunchInput {
        name: None,
        display_label: None,
        scope: None,
        workspace: None,
        initial_goal: None,
        inherited_context: vec![],
        inherited_outputs: vec![],
        continued_from: None,
        result_route: None,
        request: EntryRequest {
            request_id: descriptor.creation_request,
            recipient: descriptor.address.clone(),
            sender: crate::flow_store::inbox::SenderIdentity::User(Uuid::now_v7()),
            arguments: Default::default(),
            invocation: Default::default(),
            start: EntryStart::Invoke { user: None },
        },
        execution: Some(EntryBinding {
            user_routes: false,
            session_start: false,
            first_turn: crate::event::TurnId::now(),
            first_run: crate::event::FlowRunId::now(),
            authority: authority.clone(),
        }),
    };
    let execution = || EntryExecution {
        executor: crate::Executor::new(),
        authority: authority.clone(),
        trust: Default::default(),
        yield_interval: std::num::NonZeroUsize::new(16).unwrap(),
        catalog: catalog.clone(),
    };
    let before = files(data.path());
    let launched = host
        .launch_transient_entry(
            descriptor.clone(),
            input.clone(),
            execution(),
            Arc::default(),
            |_, _| Ok(()),
        )
        .await
        .unwrap();
    assert!(matches!(
        launched.record.receipt,
        FlowPosition::Transient { .. }
    ));
    assert!(launched.worker.context().storage_directory().is_none());
    let mut changes = launched.worker.subscribe_commits();
    let completion = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            changes.borrow_and_update();
            if let Some(completion) = launched.worker.completion(None).await.unwrap() {
                break completion;
            }
            changes.changed().await.unwrap();
        }
    })
    .await
    .unwrap();
    assert!(matches!(completion.receipt, FlowPosition::Transient { .. }));
    assert_eq!(
        completion.completion.result,
        crate::flow_store::completion::InvocationResult::Returned(
            crate::flow_store::completion::StoredValue::Int(42)
        )
    );
    let retry = host
        .launch_transient_entry(
            descriptor.clone(),
            input.clone(),
            execution(),
            Arc::default(),
            |_, _| Ok(()),
        )
        .await
        .unwrap();
    assert_eq!(retry.record.receipt, launched.record.receipt);
    assert_eq!(
        retry
            .worker
            .completion(None)
            .await
            .unwrap()
            .unwrap()
            .receipt,
        completion.receipt
    );
    drop(retry);
    assert_eq!(session.snapshot().await.unwrap().cursor, catalog);
    let inventory = session.snapshot().await.unwrap().catalog.flows;
    assert_eq!(inventory.len(), 1);
    assert_eq!(inventory[0].id, source.address().instance_id);
    assert_eq!(files(data.path()), before);

    let expected = launched.worker.subscribe_commits().borrow().clone();
    let mut foreign = expected.clone();
    if let FlowPosition::Transient { transient } = &mut foreign {
        transient.host_generation = Uuid::now_v7();
    }
    assert!(
        host.discard_transient(descriptor.address.clone(), foreign, |_| Ok(()))
            .await
            .is_err()
    );
    assert!(
        session
            .loaded_flow(&descriptor.address.instance_id)
            .is_some()
    );
    host.discard_transient(descriptor.address.clone(), expected.clone(), |_| Ok(()))
        .await
        .unwrap();
    assert!(
        session
            .loaded_flow(&descriptor.address.instance_id)
            .is_none()
    );
    assert!(!source.is_stopped());
    host.discard_transient(descriptor.address.clone(), expected, |_| Ok(()))
        .await
        .unwrap();
    assert!(
        host.launch_transient_entry(
            descriptor,
            input,
            execution(),
            Arc::default(),
            |_, _| Ok(())
        )
        .await
        .is_err()
    );
    assert_eq!(files(data.path()), before);
    drop(launched);
    host.shutdown().await.unwrap();
    assert_eq!(host.directory.transient_budget.retained_bytes(), 0);
}
