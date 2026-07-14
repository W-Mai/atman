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

    check = bash.spawn(block: true, cmd: verify_cmd)

    when check.exit_code != 0 {
        fs.write(file, original)
        return { status: "reverted", exit: check.exit_code }
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
    let bg = tools::register_bash_bg(&mut ex.tools);
    ex.tool_ctx = ex.tool_ctx.clone().with_bg_registry(bg).with_session_dir(
        std::env::temp_dir().join(format!("atman_edit_test_{}", uuid::Uuid::now_v7())),
    );

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

const FIX_LOOP_FLOW: &str = r#"flow demo(target: path, script: string) -> string {
    contract {
        scope { read: [project_root] write: [project_root] }
        capabilities { shell: true }
    }
    result = fix_until_test_passes {
        edit_flow: llm { model: "mock", prompt: "fix" }
        test: bash.spawn(block: true, cmd: script)
        target: target
        max_iters: 5
        on_giveup: { status: "gave_up" }
    }
    return result.status
}
"#;

#[tokio::test]
async fn fix_until_test_passes_iterates_until_bash_check_passes() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("subject.txt");
    std::fs::write(&target, "original\n").unwrap();
    let counter = dir.path().join("counter");
    let script = format!(
        "F={}; N=$(cat $F 2>/dev/null || echo 0); echo $((N+1)) > $F; if [ $N -lt 2 ]; then exit 1; else exit 0; fi",
        counter.display()
    );

    let file = parse_file(FIX_LOOP_FLOW).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    let bg = tools::register_bash_bg(&mut ex.tools);
    ex.tool_ctx = ex.tool_ctx.clone().with_bg_registry(bg).with_session_dir(
        std::env::temp_dir().join(format!("atman_edit_test_{}", uuid::Uuid::now_v7())),
    );
    let edited_value = Value::Struct(vec![
        ("new_content".into(), Value::Str("edited\n".into())),
        ("rationale".into(), Value::Str("try again".into())),
    ]);
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", edited_value),
    ));

    let out = ex
        .run(
            &file,
            "demo",
            vec![
                ("target".into(), Value::Path(target.clone())),
                ("script".into(), Value::Str(script)),
            ],
        )
        .await
        .unwrap();
    assert!(
        matches!(&out, Value::Str(s) if s == "passed"),
        "want passed after retries, got {out:?}"
    );
    let after = std::fs::read_to_string(&target).unwrap();
    assert_eq!(
        after, "original\n",
        "pristine base must remain since final pass triggers no revert but also no successful edit persistence in this shape"
    );
}

#[tokio::test]
async fn fix_until_test_passes_returns_gave_up_after_max_iters() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("subject.txt");
    std::fs::write(&target, "original\n").unwrap();

    let file = parse_file(FIX_LOOP_FLOW).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    let bg = tools::register_bash_bg(&mut ex.tools);
    ex.tool_ctx = ex.tool_ctx.clone().with_bg_registry(bg).with_session_dir(
        std::env::temp_dir().join(format!("atman_edit_test_{}", uuid::Uuid::now_v7())),
    );
    let edited_value = Value::Struct(vec![
        ("new_content".into(), Value::Str("attempt\n".into())),
        ("rationale".into(), Value::Str("hopeful".into())),
    ]);
    ex.providers.register(Arc::new(
        MockProvider::new("mock").with_model("mock", edited_value),
    ));

    let out = ex
        .run(
            &file,
            "demo",
            vec![
                ("target".into(), Value::Path(target.clone())),
                ("script".into(), Value::Str("false".into())),
            ],
        )
        .await
        .unwrap();
    assert!(matches!(&out, Value::Str(s) if s == "gave_up"));
    let after = std::fs::read_to_string(&target).unwrap();
    assert_eq!(
        after, "original\n",
        "target must be reverted to pristine after all iters fail"
    );
}

#[tokio::test]
async fn edit_and_verify_keeps_edit_when_check_passes() {
    let dir = tempfile::tempdir().unwrap();
    let file_path = dir.path().join("subject.txt");
    std::fs::write(&file_path, "hello\n").unwrap();

    let file = parse_file(EDIT_FLOW).unwrap();
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    let bg = tools::register_bash_bg(&mut ex.tools);
    ex.tool_ctx = ex.tool_ctx.clone().with_bg_registry(bg).with_session_dir(
        std::env::temp_dir().join(format!("atman_edit_test_{}", uuid::Uuid::now_v7())),
    );

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
