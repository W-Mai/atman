use super::*;
use crate::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowInstanceId, RuntimeStore, SessionDescriptor,
};
use crate::source_program::RevisionBundle;
use std::{sync::Arc, time::Duration};

async fn owner(root: &RuntimeStore, instance: FlowInstanceId) -> (FlowWorker, TaskScopeRef) {
    let id = Uuid::new_v4();
    let mut session = root
        .create_session(SessionDescriptor {
            configuration: Default::default(),
            id,
            project: None,
            title: "Scope transactions".into(),
        })
        .unwrap();
    let mut store = session
        .create_flow(FlowDescriptor {
            address: FlowRef {
                session_id: id.into(),
                instance_id: instance,
            },
            creation_request: Uuid::new_v4(),
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: "main.at".into(),
                bundle: RevisionBundle {
                    entry: "main.at".into(),
                    sources: [("main.at".into(), "flow main() { }".into())].into(),
                    edges: Default::default(),
                },
            }
            .into(),
        })
        .unwrap();
    store
        .transition(store.state().revision(), FlowTransition::Activated)
        .unwrap();
    let scope = TaskScopeRef {
        owner: store.state().address().clone(),
        id: Uuid::new_v4(),
    };
    store
        .create_task_scope(TaskScopeDefinition {
            id: scope.id,
            name: "Group".into(),
            goal: None,
        })
        .unwrap();
    (FlowWorker::start(store).await.unwrap(), scope)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reciprocal_scope_transactions_share_one_order_and_deduplicate_self_sources() {
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    // Equal instance UUIDs in different sessions must still acquire different gates.
    let instance = FlowInstanceId::now();
    let (left, left_scope) = owner(&root, instance.clone()).await;
    let (right, right_scope) = owner(&root, instance).await;
    let dependencies = ScopeAdmission(vec![
        (right_scope.clone(), right.clone()),
        (left_scope.clone(), left.clone()),
        (right_scope.clone(), right.clone()),
    ]);
    let inspected = dependencies.clone();
    let inspected_left = left.clone();
    tokio::task::spawn_blocking(move || {
        let transaction = ScopeTransaction::acquire(
            &inspected,
            inspected_left.address(),
            &inspected_left.scope_state,
        )
        .unwrap();
        assert_eq!(transaction.0.len(), 2);
        let addresses: Vec<_> = transaction
            .0
            .iter()
            .map(|(address, _)| (address.session_id.0, address.instance_id.0.0))
            .collect();
        assert!(addresses[0] < addresses[1]);
        assert_eq!(
            transaction
                .0
                .iter()
                .filter(|(_, gate)| matches!(gate, ScopeGuard::Write(_)))
                .count(),
            1
        );
        inspected.check(&transaction.snapshot(&inspected)).unwrap();
    })
    .await
    .unwrap();

    let start = Arc::new(tokio::sync::Barrier::new(2));
    let mut tasks = Vec::new();
    for worker in [left.clone(), right.clone()] {
        let start = start.clone();
        let dependencies = dependencies.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..12 {
                // Synchronize before enqueueing, never while a writer owns its transaction.
                start.wait().await;
                let check = dependencies.clone();
                worker
                    .request_scoped(dependencies.clone(), move |store, snapshot| {
                        check.check(snapshot)?;
                        store.create_task_scope(TaskScopeDefinition {
                            id: Uuid::new_v4(),
                            name: "Concurrent group".into(),
                            goal: None,
                        })
                    })
                    .await
                    .unwrap();
            }
        }));
    }
    tokio::time::timeout(Duration::from_secs(10), async {
        for task in tasks {
            task.await.unwrap();
        }
    })
    .await
    .unwrap();
    for worker in [left.clone(), right.clone()] {
        assert_eq!(worker.task_scopes(|_| Ok(())).await.unwrap().len(), 13);
    }

    // Dependencies prepared before cancellation must use the committed source state.
    left.cancel_task_scope(left_scope.id, |_| Ok(()))
        .await
        .unwrap();
    let before = right.snapshot().await.unwrap().cursor;
    let check = dependencies.clone();
    let error = right
        .request_scoped(dependencies, move |store, snapshot| {
            check.check(snapshot)?;
            store.create_task_scope(TaskScopeDefinition {
                id: Uuid::new_v4(),
                name: "Must not commit".into(),
                goal: None,
            })
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("cancelled"), "{error}");
    assert_eq!(right.snapshot().await.unwrap().cursor, before);
    left.shutdown().await.unwrap();
    right.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_failed_scope_source_rejects_only_the_dependent_transaction() {
    let data = tempfile::tempdir().unwrap();
    let root = RuntimeStore::open(data.path()).unwrap();
    let (source, scope) = owner(&root, FlowInstanceId::now()).await;
    let (target, _) = owner(&root, FlowInstanceId::now()).await;
    let state = source.scope_state.clone();
    tokio::task::spawn_blocking(move || {
        let _ = std::panic::catch_unwind(move || {
            let _guard = state.write().unwrap();
            panic!("failed scope source");
        });
    })
    .await
    .unwrap();
    let before = target.snapshot().await.unwrap().cursor;
    assert!(
        target
            .request_scoped(
                ScopeAdmission(vec![(scope, source.clone())]),
                |_, _| -> io::Result<()> {
                    panic!("failed dependencies must not invoke a storage operation")
                }
            )
            .await
            .is_err()
    );
    assert_eq!(target.snapshot().await.unwrap().cursor, before);
    target
        .create_task_scope(
            TaskScopeDefinition {
                id: Uuid::new_v4(),
                name: "Independent work".into(),
                goal: None,
            },
            |_| Ok(()),
        )
        .await
        .unwrap();
    source.shutdown().await.unwrap();
    target.shutdown().await.unwrap();
}
