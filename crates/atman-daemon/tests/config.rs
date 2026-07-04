use atman_daemon::config::DaemonConfig;

#[test]
fn config_generates_token_on_first_load() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("daemon.toml");
    let cfg = DaemonConfig::load_or_init(&path).unwrap();
    assert_eq!(cfg.auth_token.len(), 64);
    assert!(cfg.auth_token.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(path.exists());
}

#[test]
fn config_reuses_existing_token() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("daemon.toml");
    let a = DaemonConfig::load_or_init(&path).unwrap();
    let b = DaemonConfig::load_or_init(&path).unwrap();
    assert_eq!(a.auth_token, b.auth_token);
}

#[test]
fn config_file_permissions_are_owner_only() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("daemon.toml");
    DaemonConfig::load_or_init(&path).unwrap();
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn rotate_generates_new_token_and_preserves_permissions() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("daemon.toml");
    let old = DaemonConfig::load_or_init(&path).unwrap();
    let new = DaemonConfig::rotate(&path).unwrap();
    assert_ne!(old.auth_token, new.auth_token);
    assert_eq!(new.auth_token.len(), 64);
    let reread = DaemonConfig::load_or_init(&path).unwrap();
    assert_eq!(reread.auth_token, new.auth_token);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

#[test]
fn rotate_errors_when_config_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("does-not-exist.toml");
    let err = DaemonConfig::rotate(&path).unwrap_err();
    assert!(err.to_string().contains("no daemon config"));
    assert!(!path.exists());
}
