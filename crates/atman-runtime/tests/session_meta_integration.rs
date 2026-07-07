use atman_runtime::Session;
use atman_runtime::session_meta::{SessionMeta, find_project_root, fingerprint_from_root};

#[tokio::test]
async fn open_writes_meta_json_next_to_events() {
    let tmp = tempfile::tempdir().unwrap();
    let session = Session::open(tmp.path()).unwrap();
    let meta_path = session.dir().join("meta.json");
    assert!(
        meta_path.exists(),
        "expected meta.json at {}",
        meta_path.display()
    );
    let meta = SessionMeta::load(session.dir()).unwrap();
    assert!(meta.created_at.is_some());
}

#[tokio::test]
async fn meta_records_project_root_when_launched_inside_project() {
    let tmp = tempfile::tempdir().unwrap();
    let project = tmp.path().join("proj");
    std::fs::create_dir_all(&project).unwrap();
    std::fs::create_dir(project.join(".git")).unwrap();
    let subdir = project.join("nested");
    std::fs::create_dir(&subdir).unwrap();
    let previous_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&subdir).unwrap();
    let sessions_root = tmp.path().join("sessions_root");
    let session = Session::open(&sessions_root).unwrap();
    std::env::set_current_dir(previous_cwd).unwrap();
    let meta = SessionMeta::load(session.dir()).unwrap();
    let recorded = meta
        .project_root
        .expect("project_root must be Some inside a project");
    assert_eq!(
        recorded.canonicalize().unwrap(),
        project.canonicalize().unwrap()
    );
    let fp = meta.project_fingerprint.expect("fingerprint present");
    assert_eq!(fp, fingerprint_from_root(&project));
}

#[test]
fn find_project_root_climbs_from_deeply_nested_path() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir(tmp.path().join(".atman")).unwrap();
    let deep = tmp.path().join("a").join("b").join("c").join("d");
    std::fs::create_dir_all(&deep).unwrap();
    let root = find_project_root(&deep).unwrap();
    assert_eq!(
        root.canonicalize().unwrap(),
        tmp.path().canonicalize().unwrap()
    );
}
