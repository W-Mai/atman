use atman_runtime::flow_store::{
    EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, RuntimeStore, SessionDescriptor,
    access::{AccessTarget, FlowAction, FlowGrant},
    profile::{ManagementBinding, ProfileStore, ReplaceManagement},
};
use atman_runtime::source_program::RevisionBundle;
use uuid::Uuid;

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
