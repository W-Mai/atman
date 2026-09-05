use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub struct TestDaemon {
    child: std::process::Child,
    _config: Option<tempfile::TempDir>,
}

impl Drop for TestDaemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[allow(dead_code)]
pub fn spawn_test_daemon(data_dir: &Path) -> TestDaemon {
    let config = tempfile::tempdir().unwrap();
    let config_path = config.path().to_path_buf();
    spawn_test_daemon_at(data_dir, &config_path, Some(config))
}

#[allow(dead_code)]
pub fn spawn_test_daemon_with_config(data_dir: &Path, config_dir: &Path) -> TestDaemon {
    spawn_test_daemon_at(data_dir, config_dir, None)
}

fn spawn_test_daemon_at(
    data_dir: &Path,
    config_dir: &Path,
    config: Option<tempfile::TempDir>,
) -> TestDaemon {
    let child = Command::new(env!("CARGO_BIN_EXE_atman"))
        .env("ATMAN_CONFIG_DIR", config_dir)
        .env("ATMAN_DATA_DIR", data_dir)
        .env("ATMAN_DAEMON_PORT", "0")
        .args(["daemon", "serve"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn test daemon");
    let socket_path = data_dir.join("run/atman.sock");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !socket_path.exists() {
        assert!(
            Instant::now() < deadline,
            "test daemon did not create {}",
            socket_path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    TestDaemon {
        child,
        _config: config,
    }
}
