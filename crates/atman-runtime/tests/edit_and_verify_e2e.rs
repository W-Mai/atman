use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::{Executor, Value, tools};

#[test]
fn examples_edit_and_verify_at_parses() {
    let src = std::fs::read_to_string("../../examples/edit_and_verify.at").unwrap();
    let file = parse_file(&src).unwrap();
    assert_eq!(file.flows.len(), 1);
    let flow = &file.flows[0];
    assert_eq!(flow.name.name, "edit_and_verify");
    let contract = flow.contract.as_ref().expect("contract required");
    assert!(
        contract
            .blocks
            .iter()
            .any(|b| b.name.name == "capabilities"),
        "shell capability required"
    );
}

// Inline variant: literal prompt + parameterised verify command so the test controls exit code.
const EDIT_FLOW: &str = r#"flow edit_and_verify(file: path, instruction: string, verify_cmd: string) -> EditResult {
    contract {
        scope { read: [project_root] write: [project_root] }
        capabilities { shell: true }
    }

    original = fs.read(file)

    edited = llm {
        model: "claude-opus-4.7"
        prompt: "edit"
        input: { file: file, original: original, instruction: instruction }
    }

    ok = user_confirm("apply?")

    when ok == false {
        return { status: "cancelled" }
    }

    fs.write(file, edited.new_content)

    check = bash.exec(verify_cmd)

    when check.exit != 0 {
        fs.write(file, original)
        return { status: "reverted", exit: check.exit }
    }

    return { status: "applied" }
}
"#;

#[tokio::test]
async fn edit_and_verify_reverts_file_when_check_fails() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("subject.txt");
    let original = "hello\n";
    std::fs::write(&file_path, original).unwrap();

    let file = parse_file(EDIT_FLOW).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_shell(&mut ex.tools);

    let edit = Value::Struct(vec![
        ("new_content".into(), Value::Str("goodbye\n".into())),
        ("rationale".into(), Value::Str("replace greeting".into())),
    ]);
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_model("claude-opus-4.7", edit),
    ));

    let out = ex
        .run(
            &file,
            "edit_and_verify",
            vec![
                ("file".into(), Value::Path(file_path.clone())),
                ("instruction".into(), Value::Str("swap greeting".into())),
                ("verify_cmd".into(), Value::Str("exit 1".into())),
            ],
        )
        .await
        .unwrap();

    let Value::Struct(fields) = out else {
        panic!("expected struct, got {out:?}");
    };
    let status = fields
        .iter()
        .find(|(k, _)| k == "status")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert!(matches!(status, Value::Str(s) if s == "reverted"));

    let after = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(after, original, "file must be restored to original content");
}

#[tokio::test]
async fn edit_and_verify_keeps_edit_when_check_passes() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("subject.txt");
    std::fs::write(&file_path, "hello\n").unwrap();

    let file = parse_file(EDIT_FLOW).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_shell(&mut ex.tools);

    let edit = Value::Struct(vec![
        ("new_content".into(), Value::Str("goodbye\n".into())),
        ("rationale".into(), Value::Str("replace greeting".into())),
    ]);
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_model("claude-opus-4.7", edit),
    ));

    let out = ex
        .run(
            &file,
            "edit_and_verify",
            vec![
                ("file".into(), Value::Path(file_path.clone())),
                ("instruction".into(), Value::Str("x".into())),
                ("verify_cmd".into(), Value::Str("true".into())),
            ],
        )
        .await
        .unwrap();

    let Value::Struct(fields) = out else {
        panic!("expected struct");
    };
    let status = fields
        .iter()
        .find(|(k, _)| k == "status")
        .map(|(_, v)| v.clone())
        .unwrap();
    assert!(matches!(status, Value::Str(s) if s == "applied"));

    let after = std::fs::read_to_string(&file_path).unwrap();
    assert_eq!(after, "goodbye\n");
}
