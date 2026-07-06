use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::memory::confession::ConfessionStore;
use atman_runtime::memory::spec::SpecStore;
use atman_runtime::memory::todo::TodoStore;
use atman_runtime::{Executor, tools, validate};
use tempfile::TempDir;

fn build_executor_with_memory(dir: &TempDir) -> Executor {
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    let todo = Arc::new(TodoStore::at(dir.path()));
    let confession = Arc::new(ConfessionStore::at(dir.path()));
    let goal = Arc::new(atman_runtime::memory::GoalStore::at(dir.path()));
    let spec = Arc::new(SpecStore::new(dir.path().to_path_buf()));
    tools::register_memory(&mut ex.tools, todo, confession, goal);
    tools::register_spec_memory(&mut ex.tools, spec);
    ex
}

#[test]
fn validate_accepts_registered_memory_tools() {
    let dir = TempDir::new().unwrap();
    let ex = build_executor_with_memory(&dir);
    let src = r#"flow t() -> string {
    memory.confess(trigger: "typo", rule_violated: "no", what_i_did: "x", why: "y", mitigation: "z")
    return "ok"
}
"#;
    let file = parse_file(src).unwrap();
    validate::validate(&file.flows[0], &ex.tools).expect("valid memory flow");
}

#[test]
fn validate_rejects_typo_in_memory_tool_name() {
    let dir = TempDir::new().unwrap();
    let ex = build_executor_with_memory(&dir);
    let src = r#"flow t() -> string {
    memory.confes(body: "typo")
    return "ok"
}
"#;
    let file = parse_file(src).unwrap();
    let errs = validate::validate(&file.flows[0], &ex.tools).unwrap_err();
    assert!(
        errs.iter().any(|e| matches!(
            e,
            validate::ValidationError::UndefinedTool(name) if name == "memory.confes"
        )),
        "expected UndefinedTool(memory.confes) in {errs:?}"
    );
}

#[test]
fn validate_rejects_unknown_memory_family() {
    let dir = TempDir::new().unwrap();
    let ex = build_executor_with_memory(&dir);
    let src = r#"flow t() -> string {
    memory.dream(body: "not a thing")
    return "ok"
}
"#;
    let file = parse_file(src).unwrap();
    let errs = validate::validate(&file.flows[0], &ex.tools).unwrap_err();
    assert!(
        errs.iter().any(|e| matches!(
            e,
            validate::ValidationError::UndefinedTool(name) if name == "memory.dream"
        )),
        "expected UndefinedTool(memory.dream) in {errs:?}"
    );
}

#[test]
fn validate_accepts_all_registered_memory_spec_tools() {
    let dir = TempDir::new().unwrap();
    let ex = build_executor_with_memory(&dir);
    for tool in &[
        "memory.spec.status",
        "memory.spec.update",
        "memory.spec.deviate",
        "memory.todo.set",
        "memory.todo.done",
        "memory.fetch_confessions",
    ] {
        assert!(
            ex.tools.has(tool),
            "expected tool `{tool}` to be registered"
        );
    }
}
