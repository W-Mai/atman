use atman_dsl::parse::parse_file;
use atman_runtime::{AtmanRuntime, AtmanRuntimeOptions, AtmanValue, model_registry};

#[test]
fn atman_runtime_builds_and_runs_without_daemon() {
    let _registry_lock = model_registry::MODEL_CONFIG_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let project = tempfile::tempdir().unwrap();
            let config = tempfile::tempdir().unwrap();
            let runtime = AtmanRuntime::build(AtmanRuntimeOptions {
                events: atman_runtime::event::EventSink::new(),
                mock: true,
                config_dir: Some(config.path().to_path_buf()),
                project_root: project.path().to_path_buf(),
                home_dir: None,
                workspace_generation: "embedded-runtime-test".into(),
            })
            .await
            .unwrap();
            let file = parse_file("flow main(n: Int) -> Int { return n + 1 }").unwrap();
            let result = runtime
                .executor
                .run(&file, "main", vec![("n".into(), AtmanValue::Int(41))])
                .await
                .unwrap();
            assert!(matches!(result, AtmanValue::Int(42)));
        });
}
