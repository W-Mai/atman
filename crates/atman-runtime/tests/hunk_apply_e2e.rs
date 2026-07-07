use atman_dsl::parse::parse_file;
use atman_runtime::{Executor, Value, tools};

fn base_flow() -> &'static str {
    r#"flow apply_all(file: path, new_content: string) -> HunkResult {
    contract { scope { read: [project_root] write: [project_root] } }
    proposal = hunk.plan_edit(file, new_content)
    return hunk.apply(proposal, hunks: "all")
}

flow apply_none(file: path, new_content: string) -> HunkResult {
    contract { scope { read: [project_root] write: [project_root] } }
    proposal = hunk.plan_edit(file, new_content)
    return hunk.apply(proposal, hunks: "none")
}

flow apply_first_only(file: path, new_content: string) -> HunkResult {
    contract { scope { read: [project_root] write: [project_root] } }
    proposal = hunk.plan_edit(file, new_content)
    return hunk.apply(proposal, hunks: [1])
}
"#
}

#[tokio::test]
async fn hunk_all_writes_full_proposed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.txt");
    std::fs::write(&path, "a\nb\nc\n").unwrap();

    let file = parse_file(base_flow()).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    let out = ex
        .run(
            &file,
            "apply_all",
            vec![
                ("file".into(), Value::Path(path.clone())),
                ("new_content".into(), Value::Str("a\nB\nc\n".into())),
            ],
        )
        .await
        .unwrap();
    let Value::Struct(fields) = out else {
        panic!("expected struct");
    };
    let f = |k: &str| fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    assert!(matches!(f("status"), Some(Value::Str(s)) if s == "applied"));
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nB\nc\n");
}

#[tokio::test]
async fn hunk_none_leaves_file_original() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.txt");
    std::fs::write(&path, "a\nb\nc\n").unwrap();

    let file = parse_file(base_flow()).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    ex.run(
        &file,
        "apply_none",
        vec![
            ("file".into(), Value::Path(path.clone())),
            ("new_content".into(), Value::Str("a\nB\nc\n".into())),
        ],
    )
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "a\nb\nc\n");
}

#[tokio::test]
async fn hunk_list_selection_applies_only_id_1() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("x.txt");
    let original: String = (0..20).map(|i| format!("l{i}\n")).collect();
    std::fs::write(&path, &original).unwrap();
    let mut proposed = original.clone();
    proposed = proposed.replace("l3\n", "L3\n");
    proposed = proposed.replace("l15\n", "L15\n");

    let file = parse_file(base_flow()).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    ex.run(
        &file,
        "apply_first_only",
        vec![
            ("file".into(), Value::Path(path.clone())),
            ("new_content".into(), Value::Str(proposed)),
        ],
    )
    .await
    .unwrap();
    let on_disk = std::fs::read_to_string(&path).unwrap();
    assert!(on_disk.contains("L3\n"), "hunk 1 must be applied");
    assert!(!on_disk.contains("L15\n"), "hunk 2 must NOT be applied");
    assert!(on_disk.contains("l15\n"), "hunk 2 original must remain");
}

#[test]
fn examples_hunk_review_at_parses() {
    let src = std::fs::read_to_string("../../examples/hunk_review.at").unwrap();
    let file = parse_file(&src).unwrap();
    assert_eq!(file.flows.len(), 1);
    assert_eq!(file.flows[0].name.name, "hunk_review");
}
