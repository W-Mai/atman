use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::routing::get;

fn atman_bin() -> &'static str {
    env!("CARGO_BIN_EXE_atman")
}

async fn spawn_mock(status: axum::http::StatusCode) -> (u16, tokio_util::sync::CancellationToken) {
    let app: Router = Router::new().route("/", get(move || async move { (status, "ok") }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let shutdown = tokio_util::sync::CancellationToken::new();
    let sh_clone = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { sh_clone.cancelled().await })
            .await
            .ok();
    });
    tokio::time::sleep(Duration::from_millis(80)).await;
    (port, shutdown)
}

fn run_doctor(env: &[(&str, &str)]) -> (String, String, i32) {
    run_doctor_with_config(env, None)
}

fn run_doctor_with_config(env: &[(&str, &str)], config: Option<&str>) -> (String, String, i32) {
    let tmp = tempfile::tempdir().unwrap();
    let config_dir = tmp.path().join("config");
    if let Some(config) = config {
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("config.toml"), config).unwrap();
    }
    let mut cmd = Command::new(atman_bin());
    cmd.arg("doctor")
        .env("ATMAN_DATA_DIR", tmp.path().join("data").to_str().unwrap())
        .env("ATMAN_CONFIG_DIR", config_dir.to_str().unwrap())
        .env("HOME", tmp.path().to_str().unwrap())
        // strip host env leaking into tests
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .env_remove("ATMAN_TEST_GLM_KEY")
        .env_remove("ANTHROPIC_BASE_URL")
        .env_remove("OPENAI_BASE_URL")
        .env_remove("ATMAN_TEST_GLM_BASE_URL");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("spawn atman doctor");
    (
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
        out.status.code().unwrap_or(-1),
    )
}

#[test]
fn doctor_without_keys_shows_skipped_for_every_provider() {
    let (out, err, code) = run_doctor(&[]);
    assert_eq!(code, 0, "exit: stderr={err}");
    let anthropic_line = out
        .lines()
        .find(|l| l.contains("$ANTHROPIC_API_KEY"))
        .expect("want anthropic row");
    let openai_line = out
        .lines()
        .find(|l| l.contains("$OPENAI_API_KEY"))
        .expect("want openai row");
    assert!(
        anthropic_line.contains("skipped: no api key"),
        "anthropic row missing skip hint: {anthropic_line}"
    );
    assert!(
        openai_line.contains("skipped: no api key"),
        "openai row missing skip hint: {openai_line}"
    );
    assert!(
        !out.contains("reachable"),
        "no probe should fire without a key, got: {out}"
    );
}

#[test]
fn doctor_lists_models_and_aliases_from_config_hub_projection() {
    let (out, err, code) = run_doctor_with_config(
        &[],
        Some(
            "[models.fast]\nmodel = \"gpt-4o-mini\"\ncontext_budget = 8192\n[alias.default]\nmodel = \"fast\"\n",
        ),
    );
    assert_eq!(code, 0, "exit: stderr={err}");
    assert!(out.contains("models:"), "stdout: {out}");
    assert!(out.contains("fast"), "stdout: {out}");
    assert!(out.contains("budget=8192"), "stdout: {out}");
    assert!(out.contains("aliases:"), "stdout: {out}");
    assert!(out.contains("default"), "stdout: {out}");
}

#[test]
fn doctor_ignores_invalid_model_config_without_failing() {
    let (out, err, code) = run_doctor_with_config(&[], Some("[models\n"));
    assert_eq!(code, 0, "exit: stderr={err}");
    assert!(!out.contains("models:"), "stdout: {out}");
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_probes_provider_url_and_reports_reachable() {
    let (port, shutdown) = spawn_mock(axum::http::StatusCode::OK).await;
    let base = format!("http://127.0.0.1:{port}");
    let (out, err, code) = run_doctor(&[
        ("ANTHROPIC_API_KEY", "fake-token"),
        ("ANTHROPIC_BASE_URL", base.as_str()),
    ]);
    shutdown.cancel();
    assert_eq!(code, 0, "exit: stderr={err}");
    let anthropic_line = out
        .lines()
        .find(|l| l.contains("$ANTHROPIC_API_KEY"))
        .expect("want anthropic row");
    assert!(
        anthropic_line.contains("reachable (HTTP 200)"),
        "want reachable HTTP 200: {anthropic_line}"
    );
    assert!(
        anthropic_line.contains(&base),
        "want base URL: {anthropic_line}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn doctor_reports_unreachable_when_probe_times_out() {
    // 127.0.0.1 with port 1 is a reserved system port; connect refused / no route
    let (out, err, code) = run_doctor(&[
        ("OPENAI_API_KEY", "fake"),
        ("OPENAI_BASE_URL", "http://127.0.0.1:1"),
    ]);
    let _ = err;
    assert_eq!(code, 0);
    let openai_line = out
        .lines()
        .find(|l| l.contains("$OPENAI_API_KEY"))
        .expect("want openai row");
    assert!(
        openai_line.contains("unreachable"),
        "want unreachable, got: {openai_line}"
    );
}

// axum + tower-http prelude used only to hush deadcode
#[allow(dead_code)]
fn _link() {
    let _ = Arc::new(());
}
