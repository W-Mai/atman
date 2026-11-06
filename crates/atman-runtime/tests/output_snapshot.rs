use atman_runtime::tools::tool_output::{OutputStore, ToolOutputBudget};

fn envelope(id: &str, full: &str) -> String {
    serde_json::json!({
        "content": full, "output_id": id, "total_bytes": full.len(),
        "next": { "mode": "bytes", "offset": full.len(), "has_more": false }
    })
    .to_string()
}

#[test]
fn captured_references_are_immutable_and_have_no_disk_or_write_fallback() {
    let directory = tempfile::tempdir().unwrap();
    let store = OutputStore::at(directory.path());
    let full = "source 汉字\nsecond line\n";
    let id = store.register("source", full).unwrap();
    let unrequested = store.register("unrequested", "not captured").unwrap();
    let reference = envelope(&id, full);
    let snapshot = store.snapshot_references([reference.as_str(), reference.as_str()]);
    let second = snapshot.snapshot_references([reference.as_str()]);
    drop(directory);
    for captured in [&snapshot, &second] {
        assert_eq!(captured.retained_content(&reference), Some(full));
        assert!(captured.validates_pagination_envelope(&reference));
        assert_eq!(
            captured
                .read_bytes(&id, 0, 1024, ToolOutputBudget::default())
                .unwrap()
                .content,
            full
        );
        assert!(
            captured
                .read_bytes(&unrequested, 0, 1024, ToolOutputBudget::default())
                .is_err()
        );
        assert!(
            captured
                .register("forbidden", "no write directory")
                .is_none()
        );
    }
}

#[test]
fn captured_references_preserve_validation_and_ignore_missing_or_untrusted_inputs() {
    let directory = tempfile::tempdir().unwrap();
    let store = OutputStore::at(directory.path());
    let id = store.register("source", "actual content").unwrap();
    let valid = envelope(&id, "actual content");
    let forged = envelope(&id, "wrong size");
    let missing = envelope("out_00000000000000000000000000000000", "missing");
    let traversal = envelope("../outside", "not a registered id");
    let snapshot = store.snapshot_references([
        forged.as_str(),
        missing.as_str(),
        traversal.as_str(),
        "{\"output_id\":\"not-a-reference\"}",
        valid.as_str(),
    ]);
    assert_eq!(snapshot.retained_content(&valid), Some("actual content"));
    assert!(snapshot.retained_content(&forged).is_none());
    assert!(snapshot.retained_content(&missing).is_none());
    assert!(snapshot.retained_content(&traversal).is_none());
}
