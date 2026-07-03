use std::sync::Arc;

use atman_dsl::parse::parse_file;
use atman_runtime::memory::spec::SpecStore;
use atman_runtime::{Executor, Value, tools};

#[tokio::test]
async fn spec_workflow_from_status_through_update_to_deviate() {
    let dir = tempfile::tempdir().unwrap();
    let spec_store = Arc::new(SpecStore::new(dir.path().to_path_buf()));

    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_spec_memory(&mut ex.tools, spec_store.clone());

    let src = r#"
flow init(feature: string) -> string {
    st = memory.spec.status(feature: feature)
    return st.phase
}

flow start_research(feature: string, content: string) -> string {
    r = memory.spec.update(feature: feature, phase: "research", content: content)
    return r.phase
}

flow advance_to_design(feature: string, content: string) -> string {
    r = memory.spec.update(feature: feature, phase: "design", content: content)
    return r.phase
}

flow record_deviation(feature: string, section: string, delta: string, reason: string) -> string {
    d = memory.spec.deviate(feature: feature, section: section, delta: delta, reason: reason)
    return d.section
}

flow full_status(feature: string) -> int {
    st = memory.spec.status(feature: feature)
    return st.deviation_count
}
"#;
    let file = parse_file(src).unwrap();

    let out = ex
        .run(
            &file,
            "init",
            vec![("feature".into(), Value::Str("demo".into()))],
        )
        .await
        .unwrap();
    assert!(matches!(&out, Value::Str(s) if s == "not_started"));

    let out = ex
        .run(
            &file,
            "start_research",
            vec![
                ("feature".into(), Value::Str("demo".into())),
                ("content".into(), Value::Str("research notes".into())),
            ],
        )
        .await
        .unwrap();
    assert!(matches!(&out, Value::Str(s) if s == "research"));

    let out = ex
        .run(
            &file,
            "advance_to_design",
            vec![
                ("feature".into(), Value::Str("demo".into())),
                ("content".into(), Value::Str("design doc".into())),
            ],
        )
        .await
        .unwrap();
    assert!(matches!(&out, Value::Str(s) if s == "design"));

    let out = ex
        .run(
            &file,
            "record_deviation",
            vec![
                ("feature".into(), Value::Str("demo".into())),
                ("section".into(), Value::Str("data".into())),
                ("delta".into(), Value::Str("added field X".into())),
                ("reason".into(), Value::Str("need for tail-latency".into())),
            ],
        )
        .await
        .unwrap();
    assert!(matches!(&out, Value::Str(s) if s == "data"));

    let out = ex
        .run(
            &file,
            "full_status",
            vec![("feature".into(), Value::Str("demo".into()))],
        )
        .await
        .unwrap();
    assert!(matches!(&out, Value::Int(1)));
}

#[tokio::test]
async fn spec_workflow_phase_gate_rejects_skip_from_flow() {
    let dir = tempfile::tempdir().unwrap();
    let spec_store = Arc::new(SpecStore::new(dir.path().to_path_buf()));
    let mut ex = Executor::new();
    tools::register_tier_zero(&mut ex.tools);
    tools::register_spec_memory(&mut ex.tools, spec_store);

    let src = r#"flow skip() -> string {
    r = memory.spec.update(feature: "x", phase: "implementation", content: "premature")
    return r.phase
}"#;
    let file = parse_file(src).unwrap();
    let err = ex.run(&file, "skip", vec![]).await.unwrap_err();
    assert!(format!("{err}").contains("phase gate"), "err: {err}");
}
