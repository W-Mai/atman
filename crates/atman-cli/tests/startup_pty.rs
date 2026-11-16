use std::{
    io::{Read, Write},
    path::Path,
    sync::mpsc,
    time::{Duration, Instant},
};

use atman_runtime::flow_store::RuntimeStore;
use portable_pty::{CommandBuilder, PtySize};

fn open_terminal(root: &Path, arguments: &[String], title: &str) -> uuid::Uuid {
    let pair = portable_pty::native_pty_system()
        .openpty(PtySize {
            rows: 36,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(env!("CARGO_BIN_EXE_atman"));
    command.args(arguments);
    command.cwd(root);
    command.env("ATMAN_DATA_DIR", root.join("data"));
    command.env("ATMAN_CONFIG_DIR", root.join("config"));
    command.env("ATMAN_DISABLE_MIGRATION", "1");
    command.env("ATMAN_TUI", "1");
    command.env("TERM", "xterm-256color");
    command.env("COLORTERM", "truecolor");
    for key in ["ATMAN_REPL_NON_INTERACTIVE", "ATMAN_NO_TUI", "NO_COLOR"] {
        command.env_remove(key);
    }
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut writer = pair.master.take_writer().unwrap();
    let mut reader = pair.master.try_clone_reader().unwrap();
    let (send, receive) = mpsc::channel();
    let reader = std::thread::spawn(move || {
        let mut buffer = [0; 8192];
        while let Ok(length) = reader.read(&mut buffer) {
            if length == 0 || send.send(buffer[..length].to_vec()).is_err() {
                break;
            }
        }
    });
    let mut parser = vt100::Parser::new(36, 120, 0);
    let queries: &[(&[u8], &[u8])] = &[
        (b"\x1b[6n", b"\x1b[1;1R"),
        (b"\x1b[c", b"\x1b[?1;2c"),
        (b"\x1b[?u", b"\x1b[?0u"),
        (b"\x1b]10;?\x07", b"\x1b]10;rgb:dddd/dddd/dddd\x07"),
        (b"\x1b]11;?\x07", b"\x1b]11;rgb:0000/0000/0000\x07"),
    ];
    let mut answered = vec![0; queries.len()];
    let mut output = Vec::new();
    let mut selected = None;
    let mut onboarding_skipped = false;
    let mut submitted = false;
    let mut exiting = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    let project_root = root.canonicalize().unwrap();
    let outcome = (|| -> Result<uuid::Uuid, String> {
        loop {
            if let Ok(bytes) = receive.recv_timeout(Duration::from_millis(25)) {
                parser.process(&bytes);
                output.extend(bytes);
                for (index, (query, reply)) in queries.iter().enumerate() {
                    while let Some(position) = output[answered[index]..]
                        .windows(query.len())
                        .position(|window| window == *query)
                    {
                        writer.write_all(reply).map_err(|error| error.to_string())?;
                        answered[index] += position + query.len();
                    }
                }
            }
            let screen = parser.screen().contents();
            if let Some(status) = child.try_wait().map_err(|error| error.to_string())? {
                return if status.success() && exiting {
                    selected.ok_or_else(|| "startup never installed a live Session".into())
                } else {
                    Err(format!(
                        "startup exited before acknowledgement: {status:?}\n{screen}"
                    ))
                };
            }
            if Instant::now() >= deadline || output.len() > 2_000_000 {
                return Err(format!(
                    "startup did not acknowledge terminal input\n{screen}"
                ));
            }
            if screen.contains("os error 33") || screen.contains("View disconnected") {
                return Err(screen);
            }
            if screen.contains("Welcome to atman") {
                if !onboarding_skipped {
                    writer.write_all(b"q").map_err(|error| error.to_string())?;
                    onboarding_skipped = true;
                }
                continue;
            }
            if selected.is_none() {
                for id in RuntimeStore::session_ids(&root.join("data"))
                    .map_err(|error| error.to_string())?
                {
                    let observation = RuntimeStore::inspect_session(&root.join("data"), id)
                        .map_err(|error| format!("startup snapshot failed: {error}\n{screen}"))?;
                    if observation
                        .projection
                        .project
                        .as_ref()
                        .is_some_and(|project| project.root() == project_root)
                    {
                        selected = Some(id);
                        break;
                    }
                }
            }
            let Some(id) = selected else { continue };
            // The boot animation uses a placeholder identity and cannot accept commands.
            if !submitted
                && (onboarding_skipped || !arguments.is_empty())
                && screen.contains("/ Flow ")
                && screen.contains(&id.to_string()[..8])
            {
                writer
                    .write_all(format!(":rename {title}\r").as_bytes())
                    .map_err(|error| error.to_string())?;
                submitted = true;
            }
            if submitted && !exiting {
                let observation = RuntimeStore::inspect_session(&root.join("data"), id)
                    .map_err(|error| format!("live snapshot failed: {error}\n{screen}"))?;
                if observation.projection.title.as_deref() == Some(title) {
                    writer
                        .write_all(b"\x04")
                        .map_err(|error| error.to_string())?;
                    exiting = true;
                }
            }
        }
    })();
    let _ = child.kill();
    drop(writer);
    drop(pair.master);
    let _ = child.wait();
    reader.join().unwrap();
    outcome.unwrap_or_else(|error| panic!("{error}"))
}

#[test]
fn fresh_profile_accepts_terminal_input_and_reopens_the_same_session() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path();
    let id = open_terminal(root, &[], "Startup first");
    let restored = open_terminal(
        root,
        &["--continue".into(), id.to_string()],
        "Startup restored",
    );
    assert_eq!(restored, id);
    let observation = RuntimeStore::inspect_session(&root.join("data"), id).unwrap();
    assert_eq!(
        observation.projection.title.as_deref(),
        Some("Startup restored")
    );
}
