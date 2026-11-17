use atman_daemon::pidfile;

#[test]
fn startup_lease_rejects_concurrent_owners_and_reuses_the_same_inode() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("daemon.pid");
    let lease = pidfile::PidLease::acquire(&path).unwrap();
    assert_eq!(pidfile::read_pid(&path).unwrap(), Some(std::process::id()));
    assert!(pidfile::PidLease::acquire(&path).is_err());
    drop(lease);
    assert_eq!(pidfile::read_pid(&path).unwrap(), None);
    assert!(tmp.path().join("daemon.pid.lock").is_file());
    let _next = pidfile::PidLease::acquire(&path).unwrap();
}

#[test]
fn write_read_roundtrip() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("run").join("atman-daemon.pid");
    pidfile::write_pid(&path, 12345).unwrap();
    assert_eq!(pidfile::read_pid(&path).unwrap(), Some(12345));
}

#[test]
fn missing_pid_returns_none() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("nope.pid");
    assert_eq!(pidfile::read_pid(&path).unwrap(), None);
}

#[test]
fn remove_pid_deletes_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("run").join("atman-daemon.pid");
    pidfile::write_pid(&path, 1).unwrap();
    pidfile::remove_pid(&path);
    assert!(!path.exists());
}

#[test]
fn is_alive_true_for_self() {
    assert!(pidfile::is_alive(std::process::id()));
}

#[test]
fn is_alive_false_for_pid_one_million() {
    // pid 1_000_000 is above the typical Linux pid_max (4_194_304 max, default 32768).
    // On macOS pid_max defaults to 99999. Either way this pid will not exist.
    assert!(!pidfile::is_alive(1_000_000));
}
