use std::sync::Arc;

use atman_proto::{
    SessionId,
    capabilities::CapabilitiesRequest,
    flows::{FlowAddress, FlowCursor, InputStatusRequest, InvokeFlowRequest, SessionRequest},
    responses::{FlowEventBatch, FlowEventPage},
};
use atman_runtime::{
    flow_runtime::host::RuntimeHost,
    flow_store::{EntrySnapshot, FlowDescriptor, FlowTransition, RuntimeStore, SessionDescriptor},
    source_program::RevisionBundle,
};
use futures::StreamExt;
use uuid::Uuid;

use crate::{ClientError, EventSubscription, HttpClient, NativeClient};

fn batch(flow: &FlowAddress, sequence: u64) -> FlowEventBatch {
    FlowEventBatch {
        receipt: FlowCursor {
            journal_id: flow.instance_id,
            sequence,
            digest: format!("{sequence:064x}"),
        },
        facts: Vec::new(),
    }
}

fn observer(
    flow: &FlowAddress,
    after: Option<FlowCursor>,
    batches: Vec<FlowEventBatch>,
) -> EventSubscription {
    let page = FlowEventPage {
        next: batches.last().map(|batch| batch.receipt.clone()),
        batches,
        has_more: false,
    };
    EventSubscription::new(
        flow.clone(),
        after,
        futures::stream::iter([Ok(page)]).boxed(),
    )
}

#[tokio::test]
async fn reconnect_cursor_never_skips_an_undelivered_batch() {
    let flow = FlowAddress {
        session_id: SessionId(Uuid::now_v7()),
        instance_id: Uuid::now_v7(),
    };
    let mut stream = observer(&flow, None, vec![batch(&flow, 1), batch(&flow, 2)]);
    let first = stream.next().await.unwrap().unwrap();
    assert_eq!(stream.last(), Some(&first.receipt));
    let after = stream.last().cloned();
    drop(stream);
    let mut reconnected = observer(&flow, after, vec![batch(&flow, 1), batch(&flow, 2)]);
    assert_eq!(
        reconnected.next().await.unwrap().unwrap().receipt.sequence,
        2
    );
    assert!(reconnected.next().await.is_none());
}

#[tokio::test]
async fn foreign_source_is_rejected_before_advancing_a_cursor() {
    let flow = FlowAddress {
        session_id: SessionId(Uuid::now_v7()),
        instance_id: Uuid::now_v7(),
    };
    let other = FlowAddress {
        session_id: flow.session_id.clone(),
        instance_id: Uuid::now_v7(),
    };
    let mut stream = observer(&flow, None, vec![batch(&flow, 1), batch(&other, 2)]);
    assert!(matches!(
        stream.next().await,
        Some(Err(ClientError::EventSource))
    ));
    assert!(stream.last().is_none());
    assert!(stream.next().await.is_none());
}

#[tokio::test]
async fn native_client_borrows_the_original_owner_without_stopping_it() {
    let directory = tempfile::tempdir().unwrap();
    let store = RuntimeStore::open(directory.path()).unwrap();
    let id = Uuid::now_v7();
    let mut seeded = store
        .create_session(SessionDescriptor {
            id,
            title: "Shared owner".into(),
            project: None,
            configuration: Default::default(),
        })
        .unwrap();
    let address = atman_runtime::flow_store::FlowRef {
        session_id: id.into(),
        instance_id: atman_runtime::flow_store::FlowInstanceId::now(),
    };
    let mut flow = seeded
        .create_flow(FlowDescriptor {
            address: address.clone(),
            creation_request: Uuid::now_v7(),
            created_by: None,
            content: EntrySnapshot {
                name: "main".into(),
                path: directory.path().join("unavailable-entry.at"),
                bundle: RevisionBundle {
                    entry: "entry:main.at".into(),
                    sources: std::collections::BTreeMap::from([(
                        "entry:main.at".into(),
                        "flow main() -> int { return 1 }".into(),
                    )]),
                    edges: std::collections::BTreeMap::new(),
                },
            }
            .into(),
        })
        .unwrap();
    flow.transition(flow.state().revision(), FlowTransition::Activated)
        .unwrap();
    drop(flow);
    drop(seeded);
    let host = RuntimeHost::default();
    let original = host.open_session(directory.path(), id).await.unwrap();
    let client = NativeClient::new(
        host.clone(),
        directory.path().into(),
        Arc::<str>::from("original-host"),
    );
    let description = client
        .open_session(SessionRequest {
            session_id: SessionId(id),
        })
        .await
        .unwrap();
    assert_eq!(
        description.catalog.sequence,
        original.snapshot().await.unwrap().cursor.sequence
    );
    assert!(
        store.open_session(id).is_err(),
        "the original writer must remain exclusive"
    );
    let capabilities = client
        .capabilities(CapabilitiesRequest::default())
        .await
        .unwrap();
    assert_eq!(capabilities.host_generation, "original-host");
    assert!(!capabilities.methods.contains_key("flow.launch"));
    let rpc = NativeClient::legacy_rpc(host.clone(), directory.path().into(), "original-host");
    let flow = FlowAddress {
        session_id: SessionId(id),
        instance_id: address.instance_id.0.0,
    };
    let request_id = Uuid::new_v4();
    let request = || InvokeFlowRequest {
        flow: flow.clone(),
        request_id,
        arguments: Default::default(),
        invocation: Default::default(),
        user: None,
        expected_head: None,
    };
    let (native, legacy) = tokio::join!(client.invoke(request()), rpc.invoke(request()));
    let accepted = native.unwrap();
    assert_eq!(
        accepted.acceptance.message_id,
        legacy.unwrap().acceptance.message_id,
    );
    let observation = rpc
        .input_status(InputStatusRequest {
            flow: flow.clone(),
            request_id,
        })
        .await
        .unwrap()
        .observation
        .unwrap();
    assert_eq!(observation.accepted, accepted.acceptance.receipt);
    let mut conflicting = request();
    conflicting
        .arguments
        .insert("different".into(), true.into());
    assert!(rpc.invoke(conflicting).await.is_err());
    drop(rpc);
    drop(client);
    assert_eq!(original.snapshot().await.unwrap().catalog.descriptor.id, id);
    assert!(store.open_session(id).is_err());
    host.shutdown().await.unwrap();
}

#[test]
fn credentials_and_queries_cannot_be_embedded_in_an_endpoint() {
    for endpoint in [
        "http://user:secret@localhost/",
        "http://localhost/?token=secret",
        "http://localhost/#secret",
        "file:///tmp/host",
    ] {
        assert!(HttpClient::new(endpoint, "separate-token").is_err());
    }
}
