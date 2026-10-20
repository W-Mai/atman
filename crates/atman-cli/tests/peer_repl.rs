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

#[test]
fn original_history_attachment_never_opens_an_execution_session() {
    use atman_runtime::message::{Message, MessageOrigin};
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let data = root.join("data");
    let id = uuid::Uuid::now_v7();
    let directory = data.join("sessions").join(id.to_string());
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::create_dir_all(root.join("config")).unwrap();
    let config = root.join("config/config.toml");
    std::fs::write(&config, "invalid [ config").unwrap();
    let turn = atman_rt::TurnId::now();
    let mut message = Message::assistant_text(turn.clone(), "Original reply\n\u{1b}[2Jretained");
    message.origin = MessageOrigin::FinalAnswer;
    let cases = [
        (
            format!(
                "{}\n",
                serde_json::json!({"seq":1,"type":"assistant_msg","turn_id":turn,"message":message})
            ),
            "Original reply",
        ),
        ("{broken record".into(), "Retained source record"),
        (String::new(), "read-only"),
    ];
    for (raw, expected) in cases {
        let journal = directory.join("events.jsonl");
        std::fs::write(&journal, &raw).unwrap();
        // Both full IDs and prefixes select the immutable source, without an
        // entry, valid configuration, metadata, or repairable journal tail.
        for selection in [id.to_string(), id.to_string()[..8].to_owned()] {
            let output = command(root)
                .args(["--continue", &selection])
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let text = String::from_utf8(output.stdout).unwrap();
            assert!(text.contains(expected), "{text}");
            assert!(text.contains("read-only"), "{text}");
            assert!(!text.contains('\u{1b}'), "{text}");
            assert_eq!(std::fs::read_to_string(&journal).unwrap(), raw);
            assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
            assert!(!data.join("runtime-v1").exists());
            assert_eq!(
                std::fs::read_to_string(&config).unwrap(),
                "invalid [ config"
            );
        }
    }
}

#[test]
fn archive_attachment_reads_without_configuration_or_journal_ownership() {
    use atman_runtime::{
        flow_store::{SessionDescriptor, migration},
        message::{Message, MessageOrigin},
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let data = root.join("data");
    let source = root.join("old.jsonl");
    let old = uuid::Uuid::now_v7();
    let run = uuid::Uuid::now_v7();
    let turn = atman_rt::TurnId::now();
    let mut message = Message::assistant_text(turn.clone(), "Archived reply\n\u{1b}[2Jretained");
    message.origin = MessageOrigin::FinalAnswer;
    let raw = [
        serde_json::json!({"seq":1,"type":"flow_start","run_id":run,"flow_name":"agent","spawned":false}),
        serde_json::json!({"seq":2,"type":"assistant_msg","turn_id":turn,"flow_run_id":run,"message":message}),
    ].map(|value| format!("{value}\n")).concat();
    std::fs::write(&source, raw).unwrap();
    let store = RuntimeStore::open(&data).unwrap();
    let id = migration::destination_id(&source, old).unwrap();
    store
        .archive_legacy(
            &source,
            old,
            SessionDescriptor {
                id,
                project: None,
                title: "Archive".into(),
                configuration: Default::default(),
            },
            true,
        )
        .unwrap();
    std::fs::remove_file(source).unwrap();
    std::fs::create_dir_all(root.join("config")).unwrap();
    std::fs::write(root.join("config/config.toml"), "invalid [ config").unwrap();
    // Neither a locked writer nor unusable execution configuration prevents inspection.
    let session = store.open_session(id).unwrap();
    let instance = session.projection().default_flow.as_ref().unwrap();
    let writer = session.open_flow(instance).unwrap();
    let journal = writer.dir().join("events.jsonl");
    let before = std::fs::read(&journal).unwrap();
    let output = command(root)
        .args(["--continue", &id.to_string()])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8(output.stdout).unwrap();
    assert!(text.contains("Archived reply"), "{text}");
    assert!(text.contains("\\u{1b}[2Jretained"), "{text}");
    assert!(!text.contains('\u{1b}'));
    assert_eq!(std::fs::read(journal).unwrap(), before);
    assert!(!data.join("runtime-v1/host").exists());
    assert!(!data.join("runtime-v1/.profile.lock").exists());
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
    resume_from(root, root, id, text)
}

fn resume_from(root: &Path, cwd: &Path, id: uuid::Uuid, text: &str) -> String {
    let mut child = command(root)
        .current_dir(cwd)
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

fn create_session(root: &Path) -> uuid::Uuid {
    let child = command(root)
        .args(["session", "new"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    String::from_utf8(finish(child).stdout)
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn terminal_todo_controls_share_tool_state_and_preserve_execution_across_restart() {
    use atman_runtime::memory::todo::TodoStatus;
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (sid, fid) = launch(
        root,
        r#"
        flow main(input: string) -> string {
            contract { invocation { user_message: input } }
            memory.todo.set(where: "first task", why: "check", how: "verify", expected_result: "saved")
            memory.todo.set(where: "second task", why: "check", how: "verify", expected_result: "saved")
            plan.write(id: "plan", title: "Keep plan", steps: ["verify"])
            return final.answer(message: input)
        }
    "#,
    );
    let store = RuntimeStore::open(&root.join("data")).unwrap();
    let session = store.open_session(sid).unwrap();
    let flow = session.open_flow(&fid).unwrap();
    let second = flow.planning().todos[1].id.clone();
    let original_plans = flow.planning().plans.clone();
    drop(flow);
    drop(session);
    let output = resume(
        root,
        sid,
        &format!(":pause\n:todo list\n:todo done 0\n:todo cancel {second}\n:todo done 99\n"),
    );
    assert!(output.contains("first task"), "{output}");
    assert!(output.contains("index 99 out of range"), "{output}");
    let session = store.open_session(sid).unwrap();
    let flow = session.open_flow(&fid).unwrap();
    assert_eq!(flow.planning().todos[0].status, TodoStatus::Done);
    assert_eq!(flow.planning().todos[1].status, TodoStatus::Cancelled);
    assert_eq!(flow.planning().plans, original_plans);
    let state = flow.state().clone();
    drop(flow);
    drop(session);
    let output = resume(root, sid, ":todo list\n:todo clear\n:todo list\n");
    assert!(output.contains("✓ first task"), "{output}");
    assert!(output.contains("✗ second task"), "{output}");
    assert!(output.contains("No todos yet"), "{output}");
    let session = store.open_session(sid).unwrap();
    let flow = session.open_flow(&fid).unwrap();
    assert!(flow.planning().todos.is_empty());
    assert_eq!(flow.planning().plans, original_plans);
    assert_eq!(flow.state(), &state);
    assert_eq!(answers(&updates(root, sid, &fid)), ["seed"]);
    assert!(!flow.dir().join("todos.jsonl").exists());
}

#[test]
fn terminal_captures_inline_images_in_the_selected_flow_before_acceptance() {
    use atman_runtime::flow_store::{inbox::InboxPayload, view::AcceptedInput};
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (sid, fid) = launch(
        root,
        "flow main(input: string) -> string { contract { invocation { user_message: input } } return final.answer(message: input) }",
    );
    let bytes = b"\x89PNG\r\n\x1a\n";
    std::fs::write(root.join("sample.png"), bytes).unwrap();
    let elsewhere = root.join("another-project");
    std::fs::create_dir(&elsewhere).unwrap();
    std::fs::write(
        elsewhere.join("sample.png"),
        [bytes.as_slice(), b"other"].concat(),
    )
    .unwrap();
    let output = resume_from(root, &elsewhere, sid, ":pause\nlook @./sample.png\n");
    assert!(output.contains("Input accepted"), "{output}");
    let image = updates(root, sid, &fid)
        .into_iter()
        .find_map(|update| {
            let FlowUpdate::InputAccepted(input) = update else {
                return None;
            };
            let AcceptedInput::Envelope(envelope) = *input else {
                return None;
            };
            let InboxPayload::UserSubmission { content, .. } = envelope.request.payload else {
                return None;
            };
            assert_eq!(content.text, "look");
            Some(content.images.into_iter().next().unwrap())
        })
        .unwrap();
    let atman_runtime::message::ImageData::Artifact { path, .. } = &image.data else {
        panic!("missing captured artifact");
    };
    assert!(
        path.starts_with(
            root.join("data/runtime-v1/sessions")
                .join(sid.to_string())
                .join("flows")
                .join(fid.to_string())
        )
    );
    std::fs::remove_file(root.join("sample.png")).unwrap();
    assert_eq!(
        atman_runtime::attachment_store::image_bytes(&image).unwrap(),
        bytes
    );
    resume(root, sid, ":resume\n");
    assert_eq!(answers(&updates(root, sid, &fid)), ["seed", "look"]);
    let prior = updates(root, sid, &fid).len();
    let output = resume(root, sid, "rejected @./missing.png\n");
    assert!(output.contains("cannot read"), "{output}");
    assert_eq!(updates(root, sid, &fid).len(), prior);
}

#[test]
fn created_session_retains_startup_without_execution_and_routes_input_after_attachment() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let config = root.join("config");
    std::fs::create_dir_all(config.join("commands")).unwrap();
    std::fs::write(config.join("routes.at"), "default_route { flow: echo }").unwrap();
    std::fs::write(
        config.join("commands/echo.at"),
        "flow echo(text: string) -> string { contract { invocation { user_message: text } } return final.answer(message: text) }",
    )
    .unwrap();
    let startup = config.join("on_session_start.at");
    std::fs::write(
        &startup,
        "flow startup() -> string { return final.answer(message: \"startup delivered\") }",
    )
    .unwrap();
    let sid = create_session(root);
    let ids = RuntimeStore::session_ids(&root.join("data")).unwrap();
    assert_eq!(ids, [sid]);
    let inspected = RuntimeStore::inspect_session(&root.join("data"), sid).unwrap();
    assert_eq!(
        inspected.projection.descriptor.project.unwrap().root(),
        root.canonicalize().unwrap()
    );
    let fid = inspected.projection.default_flow.unwrap();
    let prepared = updates(root, sid, &fid);
    assert!(
        prepared
            .iter()
            .any(|update| matches!(update, FlowUpdate::Activated { input: Some(_), .. }))
    );
    assert!(!prepared.iter().any(|update| matches!(
        update,
        FlowUpdate::InputCommitted(_)
            | FlowUpdate::TurnCompleted(_)
            | FlowUpdate::MessagePublished(_)
    )));
    // The first attachment uses the captured source, even after the file changes.
    std::fs::write(&startup, "flow invalid(").unwrap();
    let output = resume(root, sid, "first input\n/echo direct input\n");
    assert!(output.contains("startup delivered"), "{output}");
    assert!(output.contains("direct input"), "{output}");
    assert_eq!(
        answers(&updates(root, sid, &fid)),
        ["startup delivered", "first input", "direct input"]
    );
    // Reopening never rereads or recreates the startup command.
    resume(root, sid, "after restart\n");
    assert_eq!(RuntimeStore::session_ids(&root.join("data")).unwrap(), ids);
    assert_eq!(
        answers(&updates(root, sid, &fid)),
        [
            "startup delivered",
            "first input",
            "direct input",
            "after restart"
        ]
    );
    assert!(!root.join("data/sessions").exists());
}

#[test]
fn created_session_waits_without_synthetic_turns_and_rejects_bad_startup_before_creation() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("config")).unwrap();
    let sid = create_session(root);
    let ids = RuntimeStore::session_ids(&root.join("data")).unwrap();
    assert_eq!(ids, [sid]);
    let inspected = RuntimeStore::inspect_session(&root.join("data"), sid).unwrap();
    let fid = inspected.projection.default_flow.unwrap();
    let initial = updates(root, sid, &fid);
    assert!(
        initial
            .iter()
            .any(|update| matches!(update, FlowUpdate::Activated { input: None, .. }))
    );
    assert!(!initial.iter().any(|update| matches!(
        update,
        FlowUpdate::InputCommitted(_)
            | FlowUpdate::TurnCompleted(_)
            | FlowUpdate::MessagePublished(_)
    )));
    resume(root, sid, "");
    assert_eq!(updates(root, sid, &fid).len(), initial.len());
    let output = resume(root, sid, ":rename Named session\n:rename\n");
    assert!(output.contains("Session title: Named session"), "{output}");
    let renamed = RuntimeStore::inspect_session(&root.join("data"), sid).unwrap();
    assert_eq!(renamed.projection.title.as_deref(), Some("Named session"));
    assert_eq!(
        renamed.projection.descriptor,
        inspected.projection.descriptor
    );
    let output = resume(root, sid, ":rename\n:rename clear\n");
    assert!(output.contains("Session title: Named session"), "{output}");
    assert!(
        RuntimeStore::inspect_session(&root.join("data"), sid)
            .unwrap()
            .projection
            .title
            .is_none()
    );
    assert_eq!(updates(root, sid, &fid).len(), initial.len());
    std::fs::write(root.join("config/on_session_start.at"), "flow invalid(").unwrap();
    let failed = command(root)
        .args(["session", "new"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!failed.status.success());
    assert!(
        String::from_utf8(failed.stderr)
            .unwrap()
            .contains("on_session_start.at")
    );
    assert_eq!(RuntimeStore::session_ids(&root.join("data")).unwrap(), ids);
    assert!(!root.join("data/sessions").exists());
}

#[test]
fn commands_capture_sources_and_share_the_instance_inbox_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let (sid, fid) = launch(
        root,
        r#"flow main(input: string) -> string {
        contract { invocation { user_message: input } }
        return final.answer(message: input)
    }"#,
    );
    let commands = root.join("config/commands");
    std::fs::create_dir_all(&commands).unwrap();
    std::fs::write(
        commands.join("helper.at"),
        "pub flow decorate(text: string) -> string { return \"command: \" + text }",
    )
    .unwrap();
    std::fs::write(
        commands.join("inspect.at"),
        r#"use "./helper.at"::decorate
        flow inspect(text: string) -> string {
            contract { invocation { user_message: text } }
            // Source-only sentinel must not enter read-only updates.
            return final.answer(message: decorate(text).await)
        }"#,
    )
    .unwrap();
    let rejected = resume(root, sid, ":pause\n/inspect\n/missing payload\n");
    assert!(rejected.contains("missing command argument"), "{rejected}");
    assert!(rejected.contains("no such command"), "{rejected}");
    let queued = resume(root, sid, "/inspect captured\n/inspect second\n");
    assert_eq!(queued.matches("Input accepted:").count(), 2, "{queued}");
    assert_eq!(answers(&updates(root, sid, &fid)), ["seed"]);
    let visible = serde_json::to_string(&updates(root, sid, &fid)).unwrap();
    assert!(visible.contains("input_accepted"));
    assert!(!visible.contains("Source-only sentinel"));
    std::fs::remove_dir_all(commands).unwrap();
    let continued = resume(root, sid, ":resume\n");
    assert!(continued.contains("command: captured"), "{continued}");
    assert_eq!(
        answers(&updates(root, sid, &fid)),
        ["seed", "command: captured", "command: second"]
    );
    resume(root, sid, "");
    assert_eq!(answers(&updates(root, sid, &fid)).len(), 3);
    resume(root, sid, ":goal set finish inspection\n:goal pause\n");
    resume(root, sid, ":goal resume\n");
    let resumed = answers(&updates(root, sid, &fid));
    assert_eq!(resumed.len(), 4);
    assert!(resumed[3].starts_with("command: "), "{resumed:?}");
    resume(root, sid, "ordinary input\n");
    assert_eq!(
        answers(&updates(root, sid, &fid)).last().unwrap(),
        "ordinary input"
    );
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
    let parent_image = b"\x89PNG\r\n\x1a\nparent";
    let child_image = b"\x89PNG\r\n\x1a\nchild";
    std::fs::write(root.join("parent image.png"), parent_image).unwrap();
    std::fs::write(root.join("child.png"), child_image).unwrap();
    let out = resume(
        root,
        sid,
        &format!(
            ":pause\n:attach parent image.png\n:focus {child}\n:attach list\n:attach child.png\n/missing rejected\n:attach missing.png\nchild input\n:attach list\n:attach child.png\n:attach clear\n:attach list\n:focus {parent}\nparent input\n"
        ),
    );
    assert!(out.contains("Pause acknowledged"), "{out}");
    assert_eq!(out.matches("No pending attachments").count(), 3, "{out}");
    assert!(out.contains("no such command"), "{out}");
    assert!(out.contains("cannot read"), "{out}");
    for (flow, expected) in [
        (&parent, parent_image.as_slice()),
        (&child, child_image.as_slice()),
    ] {
        let accepted = updates(root, sid, flow)
            .into_iter()
            .find_map(|update| {
                let FlowUpdate::InputAccepted(input) = update else {
                    return None;
                };
                let atman_runtime::flow_store::view::AcceptedInput::Envelope(envelope) = *input
                else {
                    return None;
                };
                let atman_runtime::flow_store::inbox::InboxPayload::UserSubmission {
                    content, ..
                } = envelope.request.payload
                else {
                    return None;
                };
                Some(content)
            })
            .unwrap();
        assert_eq!(accepted.images.len(), 1);
        assert_eq!(
            atman_runtime::attachment_store::image_bytes(&accepted.images[0]).unwrap(),
            expected
        );
    }
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
