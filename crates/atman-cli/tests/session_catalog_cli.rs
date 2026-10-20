use std::{collections::BTreeMap, path::Path, process::Command};

use atman_runtime::{
    config_hub::ConfigHub,
    flow_store::{
        EntrySnapshot, FlowDescriptor, FlowInstanceId, FlowRef, FlowTransition, RuntimeStore,
        SessionDescriptor,
    },
    source_program::RevisionBundle,
    storage::ProjectContext,
};
use uuid::Uuid;

fn run(root: &Path, config: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_atman"))
        .env("ATMAN_DATA_DIR", root)
        .env("ATMAN_CONFIG_DIR", config)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn cli_inspects_live_catalogs_without_touching_execution_and_retains_legacy_views() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let config = temp.path().join("missing-config");
    let project = temp.path().join("removed-project");
    std::fs::create_dir(&project).unwrap();
    let binding =
        ProjectContext::resolve(&ConfigHub::from_config_dir(&config), &project, &data).unwrap();
    let root = RuntimeStore::open(&data).unwrap();
    let id = Uuid::new_v4();
    let mut owner = root
        .create_session(SessionDescriptor {
            id,
            project: Some(binding.clone()),
            configuration: Default::default(),
            title: "Preserved session".into(),
        })
        .unwrap();
    let address = FlowRef {
        session_id: id.into(),
        instance_id: FlowInstanceId::now(),
    };
    let mut flow = owner
        .create_flow(FlowDescriptor {
            address: address.clone(),
            creation_request: Uuid::new_v4(),
            created_by: None,
            content: EntrySnapshot {
                path: project.join("main.at"),
                name: "main".into(),
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
    owner
        .select_default(&owner.cursor().clone(), address.instance_id.clone())
        .unwrap();
    flow.transition(flow.state().revision(), FlowTransition::Activated)
        .unwrap();
    flow.transition(
        flow.state().revision(),
        FlowTransition::TurnStarted {
            turn: atman_rt::TurnId::now(),
            run: atman_rt::RunId::now(),
        },
    )
    .unwrap();
    std::fs::remove_dir(&project).unwrap();
    let flow_log = flow.dir().join("events.jsonl");
    let before = std::fs::read(&flow_log).unwrap();
    std::fs::write(owner.dir().join("session.json"), "bad cache").unwrap();
    let listed = run(
        &data,
        &config,
        &["session", "list", "--project", project.to_str().unwrap()],
    );
    assert!(
        listed.status.success(),
        "{}",
        String::from_utf8_lossy(&listed.stderr)
    );
    let text = String::from_utf8(listed.stdout).unwrap();
    assert!(text.contains("Preserved session"), "{text}");
    assert!(text.contains(&id.to_string()));
    let shown = run(&data, &config, &["session", "show", &id.to_string()]);
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    assert!(String::from_utf8_lossy(&shown.stdout).contains(&address.instance_id.to_string()));
    assert_eq!(std::fs::read(&flow_log).unwrap(), before);
    assert_eq!(
        std::fs::read_to_string(owner.dir().join("session.json")).unwrap(),
        "bad cache"
    );
    assert!(!config.exists());
    assert!(!project.exists());
    assert!(!data.join("sessions").exists());

    // Discovery uses current container membership while existing flow bindings stay fixed.
    let moved_path = temp.path().join("different-project");
    let moved =
        ProjectContext::resolve(&ConfigHub::from_config_dir(&config), &moved_path, &data).unwrap();
    owner
        .set_project(&owner.cursor().clone(), Some(moved))
        .unwrap();
    for (path, present) in [(&project, false), (&moved_path, true)] {
        let listed = run(
            &data,
            &config,
            &["session", "list", "--project", path.to_str().unwrap()],
        );
        assert!(
            listed.status.success(),
            "{}",
            String::from_utf8_lossy(&listed.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&listed.stdout).contains(&id.to_string()),
            present
        );
    }
    let shown = run(&data, &config, &["session", "show", &id.to_string()]);
    let text = String::from_utf8_lossy(&shown.stdout);
    assert!(
        shown.status.success() && text.contains(moved_path.to_str().unwrap()),
        "{text}"
    );
    assert!(!text.contains(project.to_str().unwrap()), "{text}");
    assert_eq!(owner.projection().flows[0].project.as_ref(), Some(&binding));
    assert_eq!(std::fs::read(&flow_log).unwrap(), before);
    assert!(!moved_path.exists() && !config.exists());
    owner.set_project(&owner.cursor().clone(), None).unwrap();
    let shown = run(&data, &config, &["session", "show", &id.to_string()]);
    assert!(shown.status.success());
    assert!(String::from_utf8_lossy(&shown.stdout).contains("project:    -"));
    let listed = run(
        &data,
        &config,
        &["session", "list", "--project", moved_path.to_str().unwrap()],
    );
    assert!(listed.status.success());
    assert!(!String::from_utf8_lossy(&listed.stdout).contains(&id.to_string()));

    // Both storage layouts remain inspectable even if their IDs coincide.
    let legacy = data.join("sessions").join(id.to_string());
    std::fs::create_dir_all(&legacy).unwrap();
    std::fs::write(legacy.join("events.jsonl"), "{\"type\":\"flow_start\"}\n").unwrap();
    let shown = run(&data, &config, &["session", "show", &id.to_string()]);
    let text = String::from_utf8(shown.stdout).unwrap();
    assert!(
        shown.status.success()
            && text.contains("Preserved session")
            && text.contains("flow_start: 1"),
        "{text}"
    );
    let listed = run(&data, &config, &["session", "list", "--all"]);
    let text = String::from_utf8(listed.stdout).unwrap();
    assert!(
        listed.status.success()
            && text.contains("Persistent sessions")
            && text.contains("Legacy sessions"),
        "{text}"
    );
    assert_eq!(text.matches(&id.to_string()).count(), 2);
}

#[test]
fn empty_inspection_creates_no_files_and_bad_catalogs_do_not_hide_healthy_sessions() {
    let temp = tempfile::tempdir().unwrap();
    let data = temp.path().join("data");
    let config = temp.path().join("config");
    let listed = run(&data, &config, &["session", "list", "--all"]);
    assert!(listed.status.success());
    assert!(!data.exists() && !config.exists());
    let root = RuntimeStore::open(&data).unwrap();
    let id = Uuid::new_v4();
    let owner = root
        .create_session(SessionDescriptor {
            id,
            project: None,
            configuration: Default::default(),
            title: "Healthy".into(),
        })
        .unwrap();
    let bad = Uuid::new_v4();
    let bad_dir = root.root().join("sessions").join(bad.to_string());
    std::fs::create_dir(&bad_dir).unwrap();
    std::fs::write(bad_dir.join("events.jsonl"), "broken\n").unwrap();
    let listed = run(&data, &config, &["session", "list", "--all"]);
    assert!(listed.status.success());
    assert!(String::from_utf8_lossy(&listed.stdout).contains("Healthy"));
    assert!(String::from_utf8_lossy(&listed.stderr).contains(&bad.to_string()));
    assert!(
        !run(&data, &config, &["session", "show", &bad.to_string()])
            .status
            .success()
    );
    drop(owner);
}
