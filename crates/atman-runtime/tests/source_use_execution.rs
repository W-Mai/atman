use std::fs;

use atman_runtime::source_program::{SourceRoots, load_program};
use atman_runtime::value::AtmanValue as Value;
use atman_runtime::{Executor, RootInvocation};

#[tokio::test]
async fn named_and_namespace_uses_execute_with_dependency_source_dir() {
    let root = tempfile::tempdir().unwrap();
    let lib = root.path().join("lib");
    fs::create_dir(&lib).unwrap();
    fs::write(lib.join("payload.txt"), "from dependency").unwrap();
    fs::write(
        lib.join("text.at"),
        r#"
flow read_private() -> string { return @"payload.txt" }
pub flow read() -> string { return subflow(read_private) }
pub flow defaulted(input: string = @"payload.txt") -> string { return input }
"#,
    )
    .unwrap();
    let entry = root.path().join("main.at");
    fs::write(
        &entry,
        r#"
use "./lib/text.at"::read as direct
use "./lib/text.at" as text

flow named() -> string { return subflow(direct) }
flow namespaced() -> string { return subflow(text.read) }
flow with_default() -> string { return subflow(text.defaulted) }
"#,
    )
    .unwrap();

    let program = load_program(&entry, &SourceRoots::default()).unwrap();
    let executor = Executor::new();
    for flow in ["named", "namespaced", "with_default"] {
        let actual = executor
            .run_linked_with_invocation(&program, flow, vec![], RootInvocation::default())
            .await
            .unwrap();
        assert!(matches!(actual, Value::Str(text) if text == "from dependency"));
    }
}
