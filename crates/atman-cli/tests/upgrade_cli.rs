use std::process::{Command, Stdio};

fn atman_bin() -> &'static str {
    env!("CARGO_BIN_EXE_atman")
}

#[test]
fn upgrade_help_documents_supported_controls() {
    let output = Command::new(atman_bin())
        .args(["upgrade", "--help"])
        .output()
        .expect("run atman upgrade --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("--yes"), "stdout: {stdout}");
    assert!(stdout.contains("--verbose"), "stdout: {stdout}");
    assert!(stdout.contains("--no-modify-path"), "stdout: {stdout}");
    assert!(stdout.contains("official installer"), "stdout: {stdout}");
}

#[cfg(unix)]
#[test]
fn rejecting_source_change_exits_before_network_or_installation() {
    let home = tempfile::tempdir().unwrap();
    let legacy_config = home.path().join(".config/atman/config.toml");
    std::fs::create_dir_all(legacy_config.parent().unwrap()).unwrap();
    std::fs::write(&legacy_config, "legacy = true\n").unwrap();
    let data_dir = home.path().join("data");

    let output = Command::new(atman_bin())
        .arg("upgrade")
        .env("HOME", home.path())
        .env("ATMAN_DATA_DIR", &data_dir)
        .env_remove("CARGO_HOME")
        .stdin(Stdio::null())
        .output()
        .expect("run atman upgrade");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Official installer target:"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("does not upgrade Homebrew"),
        "stderr: {stderr}"
    );
    assert!(
        stderr.contains("upgrade cancelled before download"),
        "stderr: {stderr}"
    );
    assert!(!home.path().join(".cargo/bin/atman").exists());
    assert!(
        legacy_config.exists(),
        "upgrade must not migrate user config"
    );
    assert!(
        !data_dir.join("config/config.toml").exists(),
        "upgrade must not write migrated config"
    );
}
