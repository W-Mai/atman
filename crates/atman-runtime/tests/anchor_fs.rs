use std::fs;
use std::path::Path;

use atman_runtime::tools::anchor_fs;

#[test]
fn anchor_v1_fixed_vectors_and_probe_stride() {
    assert_eq!(anchor_fs::line_hash(""), 0x02cc5d05 >> 14);
    assert_eq!(anchor_fs::anchor_for_hash(0), "AAA");
    assert_eq!(anchor_fs::anchor_for_hash(3906), "BBA");
    assert_eq!(anchor_fs::anchor_for_hash(238_328), "AAA");
}

#[test]
fn probe_allocates_distinct_anchors_for_collisions() {
    let lines = vec!["same".to_owned(), "same".to_owned()];
    let anchors = anchor_fs::anchors_for_lines(&lines, None).unwrap();
    assert_ne!(anchors[0], anchors[1]);
    assert_eq!(
        anchors[1],
        anchor_fs::anchor_for_hash(anchor_fs::line_hash("same") + 3907)
    );
}

#[test]
fn sqlite_store_recovers_from_corruption() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("anchor-state.sqlite3"), b"not sqlite").unwrap();
    let snapshot = anchor_fs::make_snapshot(Path::new("sample.txt"), "one\n", None).unwrap();
    let store = anchor_fs::StateStore::new(root.path());
    store.put_snapshot(snapshot.clone()).unwrap();
    assert_eq!(
        store.snapshot(Path::new("sample.txt")).unwrap(),
        Some(snapshot)
    );
    assert!(root.path().read_dir().unwrap().any(|e| {
        e.unwrap()
            .file_name()
            .to_string_lossy()
            .contains("corrupt-")
    }));
}

#[test]
fn strict_undo_checks_current_after_hash() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sample.txt");
    let change = anchor_fs::ChangeRecord {
        change_id: "c1".into(),
        path: path.to_string_lossy().into(),
        before_hash: anchor_fs::file_hash(b"before"),
        after_hash: anchor_fs::file_hash(b"after"),
        before_content: "before".into(),
        after_content: "after".into(),
        parent_change_id: None,
        created_at: 1,
    };
    let store = anchor_fs::StateStore::new(root.path());
    let err = store.undo_strict(&path, &change, b"other").unwrap_err();
    assert!(err.to_string().contains("strict undo refused"));
}

fn endpoint(snapshot: &anchor_fs::Snapshot, index: usize) -> String {
    snapshot.anchors[index].clone()
}

#[test]
fn mutation_core_resolves_ranges_and_supports_insert_remove() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sample.txt");
    fs::write(&path, "one\ntwo\nthree\n").unwrap();
    let store = anchor_fs::StateStore::new(root.path().join("state"));
    let rendered = anchor_fs::read_anchor_text(&path, &store).unwrap();
    assert_eq!(rendered.lines().count(), 3);
    assert!(rendered.lines().all(|line| line.contains('│')));
    let read_endpoint = rendered
        .lines()
        .nth(1)
        .and_then(|line| line.split_once('│'))
        .and_then(|(line, _)| line.split_once(':'))
        .map(|(_, anchor)| anchor)
        .unwrap()
        .to_owned();
    let inserted = anchor_fs::edit_by_anchor(
        &path,
        "insert",
        None,
        None,
        None,
        Some(&read_endpoint),
        Some("before"),
        Some("new\n"),
        &store,
    )
    .unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "one\nnew\ntwo\nthree\n");
    let current = store.snapshot(&path).unwrap().unwrap();
    let removed = anchor_fs::edit_by_anchor(
        &path,
        "remove",
        Some(&endpoint(&current, 1)),
        None,
        None,
        None,
        None,
        None,
        &store,
    )
    .unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\nthree\n");
    assert_ne!(inserted.change_id, removed.change_id);
}

#[test]
fn mutation_core_rejects_stale_ambiguous_and_reversed_ranges() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sample.txt");
    fs::write(&path, "same\nsame\nlast\n").unwrap();
    let store = anchor_fs::StateStore::new(root.path().join("state"));
    let snapshot =
        anchor_fs::make_snapshot(&path, &fs::read_to_string(&path).unwrap(), None).unwrap();
    store.put_snapshot(snapshot.clone()).unwrap();
    let stale = format!("{}:deadbeef", snapshot.anchors[2]);
    let stale_error = anchor_fs::edit_by_anchor(
        &path,
        "remove",
        Some(&stale),
        None,
        None,
        None,
        None,
        None,
        &store,
    )
    .unwrap_err();
    assert!(matches!(
        stale_error,
        anchor_fs::AnchorError::Resolve { .. }
    ));
    assert!(stale_error.to_string().contains("invalid endpoint"));
    let ambiguous = anchor_fs::anchor_for_hash(anchor_fs::line_hash("same"));
    assert!(matches!(
        anchor_fs::edit_by_anchor(
            &path,
            "remove",
            Some(&ambiguous),
            None,
            None,
            None,
            None,
            None,
            &store
        ),
        Err(anchor_fs::AnchorError::Resolve { .. })
    ));
    assert!(matches!(
        anchor_fs::edit_by_anchor(
            &path,
            "replace",
            None,
            Some(&endpoint(&snapshot, 2)),
            Some(&endpoint(&snapshot, 0)),
            None,
            None,
            Some("x\n"),
            &store
        ),
        Err(anchor_fs::AnchorError::Resolve { .. })
    ));
}

#[test]
fn mutation_core_undo_is_strict_and_restores_content() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sample.txt");
    fs::write(&path, "one\n").unwrap();
    let store = anchor_fs::StateStore::new(root.path().join("state"));
    let change =
        anchor_fs::overwrite_with_hash(&path, &anchor_fs::file_hash(b"one\n"), "two\n", &store)
            .unwrap();
    anchor_fs::undo_change(&change.change_id, &store).unwrap();
    assert_eq!(fs::read_to_string(&path).unwrap(), "one\n");
    fs::write(&path, "external\n").unwrap();
    assert!(matches!(
        anchor_fs::undo_change(&change.change_id, &store),
        Err(anchor_fs::AnchorError::UndoConflict { .. })
    ));
}
