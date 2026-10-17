use std::{
    io::Write,
    path::Path,
    process::{Command, Output, Stdio},
    time::{Duration, Instant},
};

use atman_runtime::flow_store::{FlowInstanceId, RuntimeStore, view::FlowUpdate};

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_atman"));
    command
        .current_dir(root)
        .env("ATMAN_DATA_DIR", root.join("data"))
        .env("ATMAN_CONFIG_DIR", root.join("config"))
        .env("ATMAN_REPL_NON_INTERACTIVE", "1")
        .env("ATMAN_DISABLE_MIGRATION", "1");
    command
}

fn finish(mut child: std::process::Child) -> Output {
    let deadline = Instant::now() + Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if Instant::now() > deadline {
            child.kill().unwrap();
            let out = child.wait_with_output().unwrap();
            panic!(
                "terminal did not stop: {}\n{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let out = child.wait_with_output().unwrap();
    assert!(
        out.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn resume(root: &Path, id: uuid::Uuid, text: &str) -> String {
    let mut child = command(root)
        .args(["--continue", &id.to_string()[..8]])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(text.as_bytes())
        .unwrap();
    String::from_utf8(finish(child).stdout).unwrap()
}

fn updates(root: &Path, sid: uuid::Uuid, id: &FlowInstanceId) -> Vec<FlowUpdate> {
    let session = RuntimeStore::inspect_session(&root.join("data"), sid).unwrap();
    let mut result = vec![];
    session
        .inspect_flow_updates(id, |_, batch| {
            result.extend(batch);
            Ok(())
        })
        .unwrap();
    result
}

fn answers(updates: &[FlowUpdate]) -> Vec<String> {
    updates
        .iter()
        .filter_map(|update| match update {
            FlowUpdate::MessagePublished(message)
                if message.context_run.is_none()
                    && message.message.is_visible_assistant_reply() =>
            {
                Some(message.message.text_concat())
            }
            _ => None,
        })
        .collect()
}

fn launch(root: &Path, source: &str) -> (uuid::Uuid, FlowInstanceId) {
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::write(
        root.join("config/config.toml"),
        "[trust]\nmode = \"reckless\"\n",
    )
    .unwrap();
    std::fs::write(root.join("main.at"), source).unwrap();
    let child = command(root)
        .args(["run", "main.at", "input=seed"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    finish(child);
    let id = RuntimeStore::session_ids(&root.join("data")).unwrap()[0];
    let selected = RuntimeStore::inspect_session(&root.join("data"), id)
        .unwrap()
        .projection
        .default_flow
        .unwrap();
    (id, selected)
}

#[test]
fn terminal_preserves_inbox_gates_and_consumption_across_restarts() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (sid, fid) = launch(
        root,
        r#"flow main(input: string) -> string {
        contract { invocation { user_message: input } }
        return final.answer(message: input)
    }"#,
    );
    std::fs::remove_file(root.join("main.at")).unwrap();
    let paused = resume(root, sid, ":pause\nqueued text\n");
    assert!(paused.contains("Pause acknowledged"), "{paused}");
    assert!(paused.contains("Input accepted:"), "{paused}");
    assert_eq!(answers(&updates(root, sid, &fid)), ["seed"]);
    let continued = resume(root, sid, ":resume\n");
    assert!(continued.contains("queued text"), "{continued}");
    assert_eq!(answers(&updates(root, sid, &fid)), ["seed", "queued text"]);
    resume(root, sid, "");
    assert_eq!(answers(&updates(root, sid, &fid)), ["seed", "queued text"]);

    // Explicit exit must not wait for a blocking stdin read or close the instance.
    let mut child = command(root)
        .args(["--continue", &sid.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b":quit\n").unwrap();
    finish(child);
    drop(stdin);
    resume(root, sid, "after detach\n");
    assert_eq!(
        answers(&updates(root, sid, &fid)),
        ["seed", "queued text", "after detach"]
    );
    resume(
        root,
        sid,
        ":goal set old objective\n:goal pause\nnew request\n",
    );
    let facts = updates(root, sid, &fid);
    assert_eq!(answers(&facts).last().unwrap(), "new request");
    let store = RuntimeStore::open(&root.join("data")).unwrap();
    let session = store.open_session(sid).unwrap();
    let flow = session.open_flow(&fid).unwrap();
    assert_eq!(
        flow.goal().unwrap().status,
        atman_runtime::memory::goal::GoalStatus::Paused
    );
}

#[test]
fn terminal_requires_declared_text_mapping_and_releases_prompts_on_eof() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (sid, fid) = launch(
        root,
        r#"flow main(input: string) -> string {
        when input == "ask" {
            answer = form.ask(kind: "text", prompt: "A terminal question")
            return final.answer(message: answer.text)
        }
        return final.answer(message: input)
    }"#,
    );
    let result = resume(
        root,
        sid,
        "unmapped\n:insert unmapped\n:invoke {\"input\":\"typed\"}\n",
    );
    assert!(
        result.contains("entry has no invocation user_message parameter"),
        "{result}"
    );
    assert_eq!(answers(&updates(root, sid, &fid)), ["seed", "typed"]);
    let result = resume(root, sid, ":invoke {\"input\":\"ask\"}\n");
    assert!(result.contains("Turn failed:"), "{result}");
    assert_eq!(answers(&updates(root, sid, &fid)), ["seed", "typed"]);
    let mut child = command(root)
        .args(["--continue", &sid.to_string()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut input = child.stdin.take().unwrap();
    let output = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        use std::io::BufRead;
        for line in std::io::BufReader::new(output).lines() {
            if tx.send(line.unwrap()).is_err() {
                break;
            }
        }
    });
    input
        .write_all(b":recover\n:invoke {\"input\":\"ask\"}\n")
        .unwrap();
    let prompt = wait_line(&mut child, &rx, "[atman] Prompt ");
    let id = prompt.split_whitespace().nth(2).unwrap();
    writeln!(input, ":reply {id} {{\"Submitted\":{{\"answers\":[{{\"kind\":\"text_entered\",\"text\":\"terminal answer\"}}]}}}}").unwrap();
    wait_line(&mut child, &rx, "terminal answer");
    drop(input);
    finish(child);
    assert_eq!(
        answers(&updates(root, sid, &fid)),
        ["seed", "typed", "terminal answer"]
    );
}

fn wait_line(
    child: &mut std::process::Child,
    rx: &std::sync::mpsc::Receiver<String>,
    prefix: &str,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = Vec::new();
    while let Ok(line) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if line.starts_with(prefix) {
            return line;
        }
        seen.push(line);
    }
    let _ = child.kill();
    let _ = child.wait();
    panic!("missing {prefix:?}: {seen:?}");
}

#[test]
fn terminal_focus_routes_input_and_controls_to_equal_instances() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("child.at"),
        r#"flow child(input: string) -> string {
        contract { invocation { user_message: input } }
        return final.answer(message: input)
    }"#,
    )
    .unwrap();
    let (sid, parent) = launch(
        root,
        r#"flow main(input: string) -> string {
        contract { invocation { user_message: input } }
        when input == "seed" {
            inventory = flow.instances()
            flow.spawn(flow: "child.at", spawn_token: inventory.spawn_token, arguments: {input: "child seed"}, async: false)
        }
        return final.answer(message: input)
    }"#,
    );
    let session = RuntimeStore::inspect_session(&root.join("data"), sid).unwrap();
    let child = session
        .projection
        .flows
        .iter()
        .find(|flow| flow.id != parent)
        .unwrap()
        .id
        .clone();
    std::fs::remove_file(root.join("child.at")).unwrap();
    let out = resume(
        root,
        sid,
        &format!(":pause\n:focus {child}\nchild input\n:focus {parent}\nparent input\n"),
    );
    assert!(out.contains("Pause acknowledged"), "{out}");
    assert_eq!(
        answers(&updates(root, sid, &child)),
        ["child seed", "child input"]
    );
    assert_eq!(answers(&updates(root, sid, &parent)), ["seed"]);
    assert_eq!(
        RuntimeStore::inspect_session(&root.join("data"), sid)
            .unwrap()
            .projection
            .default_flow,
        Some(parent)
    );
}
