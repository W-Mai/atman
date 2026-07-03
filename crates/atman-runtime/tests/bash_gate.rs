use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::tools;
use atman_runtime::tools::bash::BashExec;
use atman_runtime::{Executor, RuntimeError, Value};

#[tokio::test]
async fn bash_exec_with_shell_capability_runs_echo() {
    let src = r#"flow t() -> string {
    contract {
        capabilities {
            shell: true
        }
    }
    result = bash.exec("echo hello atman")
    return result.stdout
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_shell(&mut ex.tools);

    let out = ex.run(&file, "t", vec![]).await.unwrap();
    assert!(matches!(out, Value::Str(s) if s.trim() == "hello atman"));
}

#[tokio::test]
async fn bash_exec_without_shell_capability_is_rejected() {
    let src = r#"flow t() -> string {
    result = bash.exec("echo hello")
    return result.stdout
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    ex.tools.register(Arc::new(BashExec));

    let err = ex.run(&file, "t", vec![]).await.unwrap_err();
    match err {
        RuntimeError::ToolFailed(msg) => {
            assert!(msg.contains("Tier 4"));
            assert!(msg.contains("shell"));
        }
        other => panic!("expected ToolFailed, got {other:?}"),
    }
}

#[tokio::test]
async fn bash_exec_captures_exit_code() {
    let src = r#"flow t() -> Int {
    contract {
        capabilities {
            shell: true
        }
    }
    result = bash.exec("exit 3")
    return result.exit
}
"#;
    let file = parse_file(src).unwrap();
    let mut ex = Executor::new();
    tools::register_shell(&mut ex.tools);
    let out = ex.run(&file, "t", vec![]).await.unwrap();
    assert!(matches!(out, Value::Int(3)));
}
