use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use atman_client::{Client, SessionClient};
use atman_proto::{
    FlowRunId, InterjectionLevel, MessagePart, MessageRole, RunLifecycle, RunProjection,
    TranscriptItem,
};

pub(crate) async fn run(resume: Option<String>) -> Result<()> {
    let client = crate::daemon_tui::connect_local_daemon().await?;
    let session = match resume {
        Some(prefix) => {
            let id = crate::daemon_tui::resolve_session_prefix(&client, &prefix).await?;
            client.attach_session(id).await?
        }
        None => {
            let project_root = std::env::current_dir()?.to_string_lossy().into_owned();
            client.create_session(Some(project_root), None).await?
        }
    };
    run_boot_flow(&session).await;

    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::task::spawn_blocking(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines().map_while(Result::ok) {
            if input_tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut pushback = VecDeque::new();
    let mut attachments = Vec::new();
    loop {
        let line = if let Some(line) = pushback.pop_front() {
            Some(line)
        } else {
            input_rx.recv().await
        };
        let Some(line) = line else {
            break;
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(command) = trimmed.strip_prefix(':') {
            if handle_meta(
                &client,
                &session,
                command.trim(),
                &mut attachments,
                &mut input_rx,
            )
            .await?
            {
                break;
            }
            continue;
        }
        submit(
            &session,
            trimmed,
            &mut attachments,
            &mut input_rx,
            &mut pushback,
        )
        .await;
    }
    Ok(())
}

async fn run_boot_flow(session: &SessionClient) {
    let Ok(config_dir) = atman_runtime::storage::config_dir() else {
        return;
    };
    let path = config_dir.join("on_session_start.at");
    if !path.is_file() {
        return;
    }
    let response = session
        .start_run(
            path.to_string_lossy().into_owned(),
            None,
            serde_json::Map::new(),
            None,
            Vec::new(),
        )
        .await;
    match response {
        Ok(response) => match wait_run(session, &response.run_id).await {
            Ok(run) => print_run(&run),
            Err(error) => eprintln!("[atman] boot flow error: {error:#}"),
        },
        Err(error) => eprintln!("[atman] boot flow error: {error}"),
    }
}

async fn submit(
    session: &SessionClient,
    line: &str,
    attachments: &mut Vec<PathBuf>,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    pushback: &mut VecDeque<String>,
) {
    let (text, inline) = crate::extract_at_paths(line);
    let mut paths = attachments.clone();
    paths.extend(inline);
    let images = match crate::inline_images(paths) {
        Ok(images) => images,
        Err(error) => {
            eprintln!("error: {error:#}");
            return;
        }
    };
    match session.send_message(text, None, images).await {
        Ok(response) => {
            attachments.clear();
            match wait_run_with_input(session, &response.run_id, input_rx, pushback).await {
                Ok(run) => print_run(&run),
                Err(error) => eprintln!("error: {error:#}"),
            }
        }
        Err(error) => {
            let message = error.to_string();
            if message.contains("no route matched") {
                println!("[atman] {message}");
            } else {
                eprintln!("error: {message}");
            }
        }
    }
}

async fn wait_run_with_input(
    session: &SessionClient,
    run_id: &FlowRunId,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    pushback: &mut VecDeque<String>,
) -> Result<RunProjection> {
    loop {
        if let Some(run) = terminal_run(session, run_id) {
            return Ok(run);
        }
        tokio::select! {
            line = input_rx.recv(), if !input_rx.is_closed() => {
                if let Some(line) = line {
                    if !handle_interjection(session, run_id, line.trim()).await {
                        pushback.push_back(line);
                    }
                }
            }
            _ = tokio::time::sleep(std::time::Duration::from_millis(50)) => {
                session
                    .refresh_until_current()
                    .await
                    .context("refresh daemon session")?;
            }
        }
    }
}

async fn handle_interjection(session: &SessionClient, run_id: &FlowRunId, input: &str) -> bool {
    let (level, text, redirect_target) = if input == "!stop" {
        (InterjectionLevel::HardStop, "stop", None)
    } else if let Some(text) = input.strip_prefix("!course-correct ") {
        (InterjectionLevel::CourseCorrect, text.trim(), None)
    } else if let Some(target) = input.strip_prefix("!redirect ") {
        let target = target.trim();
        (InterjectionLevel::Redirect, target, Some(target.to_owned()))
    } else if let Some(text) = input.strip_prefix("!nudge ") {
        (InterjectionLevel::Nudge, text.trim(), None)
    } else if let Some(text) = input.strip_prefix('!') {
        (InterjectionLevel::Nudge, text.trim(), None)
    } else {
        return false;
    };
    if text.is_empty() {
        eprintln!("[atman] interjection text must not be empty");
        return true;
    }
    match session
        .interject(run_id.clone(), text, level, redirect_target)
        .await
    {
        Ok(_) => println!("[atman] interjection queued"),
        Err(error) => eprintln!("[atman] interjection rejected: {error}"),
    }
    true
}

async fn wait_run(session: &SessionClient, run_id: &FlowRunId) -> Result<RunProjection> {
    loop {
        if let Some(run) = terminal_run(session, run_id) {
            return Ok(run);
        }
        session
            .refresh_until_current()
            .await
            .context("refresh daemon session")?;
        if terminal_run(session, run_id).is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

fn terminal_run(session: &SessionClient, run_id: &FlowRunId) -> Option<RunProjection> {
    session
        .current()
        .projection()
        .runs
        .iter()
        .find(|run| {
            &run.id == run_id
                && matches!(
                    run.state,
                    RunLifecycle::Cancelled
                        | RunLifecycle::Succeeded
                        | RunLifecycle::Failed
                        | RunLifecycle::Lost
                )
        })
        .cloned()
}

fn print_run(run: &RunProjection) {
    match run.state {
        RunLifecycle::Succeeded => {
            if let Some(output) = run.output.as_deref()
                && !output.is_empty()
            {
                println!("{output}");
            }
        }
        RunLifecycle::Failed => eprintln!(
            "error: {}",
            run.error
                .as_deref()
                .unwrap_or("flow failed without details")
        ),
        RunLifecycle::Cancelled => eprintln!("error: flow cancelled"),
        RunLifecycle::Lost => eprintln!("error: flow lost after daemon restart"),
        _ => {}
    }
}

async fn handle_meta(
    client: &Client,
    session: &SessionClient,
    command: &str,
    attachments: &mut Vec<PathBuf>,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
) -> Result<bool> {
    let Some(meta) = atman_runtime::meta_commands::match_command(command) else {
        eprintln!("unknown `:{command}` — try `:help`");
        return Ok(false);
    };
    match meta.name {
        "exit" => return Ok(true),
        "help" => {
            for line in atman_runtime::meta_commands::help_lines() {
                println!("{line}");
            }
        }
        "session" => println!("session_id: {}", session.session_id()),
        "sessions" => print_sessions(client).await?,
        "rename" => rename(session, command).await,
        "compact" => match session.compact().await {
            Ok(_) => println!("[atman] compaction requested"),
            Err(error) => eprintln!("[atman] :compact failed: {error}"),
        },
        "attach" => attach(command, attachments),
        "copy" => copy(session, command),
        "goal" => goal(session, command).await,
        "todo" => todo(session, command).await,
        "cost" => print_cost(session),
        "model" => print_model(session),
        "mode" | "mode-theme" | "sidebar" => {
            println!("[atman] :{} is available in TUI mode", meta.name)
        }
        "suggest" => suggest(session, input_rx).await,
        _ => {}
    }
    Ok(false)
}

async fn suggest(
    session: &SessionClient,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
) {
    let response = match session.suggest_flow().await {
        Ok(response) => response,
        Err(error) => {
            eprintln!("[atman] :suggest failed: {error}");
            return;
        }
    };
    let (flow_name, source, has_shell) = match response.result {
        atman_proto::SuggestFlowStatus::NoSuggestion => {
            println!("[atman] no reusable pattern found in recent turns");
            return;
        }
        atman_proto::SuggestFlowStatus::Invalid { reason } => {
            eprintln!("[atman] suggestion was rejected: {reason}");
            return;
        }
        atman_proto::SuggestFlowStatus::Proposal {
            flow_name,
            source,
            has_shell,
        } => (flow_name, source, has_shell),
    };
    println!("[atman] suggested flow `{flow_name}`:\n---\n{source}\n---");
    if has_shell {
        println!("[atman] this flow executes shell tools; review it before accepting");
    }
    println!("[atman] install this flow? [y/N]");
    let accepted = input_rx
        .recv()
        .await
        .is_some_and(|answer| matches!(answer.trim(), "y" | "Y" | "yes"));
    if !accepted {
        println!("[atman] suggested flow discarded");
        return;
    }
    match session.install_suggested_flow(flow_name, source).await {
        Ok(response) => println!("[atman] installed suggested flow `{}`", response.flow_name),
        Err(error) => eprintln!("[atman] could not install suggested flow: {error}"),
    }
}

async fn print_sessions(client: &Client) -> Result<()> {
    let rows = client.list_sessions(None, None, Some(20)).await?;
    if rows.is_empty() {
        println!("[atman] no sessions yet");
        return Ok(());
    }
    for row in rows {
        println!("{}  {}  {} messages", row.id, row.title, row.message_count);
    }
    Ok(())
}

async fn rename(session: &SessionClient, command: &str) {
    let arg = command.strip_prefix("rename").unwrap_or("").trim();
    let result = match arg {
        "" => {
            println!(
                "[atman] session title: {}",
                session.current().projection().metadata.title
            );
            return;
        }
        "clear" => session.clear_title().await,
        title => session.rename(title).await,
    };
    match result {
        Ok(response) => println!("[atman] session title: {}", response.session.title),
        Err(error) => eprintln!("[atman] :rename failed: {error}"),
    }
}

fn attach(command: &str, attachments: &mut Vec<PathBuf>) {
    let arg = command.strip_prefix("attach").unwrap_or("").trim();
    match arg {
        "" => eprintln!(":attach <path>  |  :attach clear  |  :attach list"),
        "clear" => {
            attachments.clear();
            println!("[atman] pending attachments cleared");
        }
        "list" if attachments.is_empty() => println!("[atman] no pending attachments"),
        "list" => {
            for (index, path) in attachments.iter().enumerate() {
                println!("  {index}: {}", path.display());
            }
        }
        path => {
            let path = PathBuf::from(path);
            if !path.is_file() {
                eprintln!(":attach: file not found: {}", path.display());
                return;
            }
            attachments.push(path.clone());
            println!(
                "[atman] attached {} (pending count: {})",
                path.display(),
                attachments.len()
            );
        }
    }
}

fn copy(session: &SessionClient, command: &str) {
    let target = command.strip_prefix("copy").unwrap_or("").trim();
    let target = if target.is_empty() {
        "last-message"
    } else {
        target
    };
    let state = session.current();
    let payload = match target {
        "last-message" | "last" => projected_part(&state.projection().transcript, true),
        "last-tool" => projected_part(&state.projection().transcript, false),
        other => {
            eprintln!(":copy: unknown target `{other}` — use last-message | last-tool");
            return;
        }
    };
    let Some(payload) = payload else {
        println!(":copy: nothing to copy for {target}");
        return;
    };
    write_osc52(&payload);
    println!(
        ":copy: pushed {} chars to terminal clipboard (OSC 52)",
        payload.chars().count()
    );
}

fn projected_part(items: &[TranscriptItem], assistant: bool) -> Option<String> {
    items.iter().rev().find_map(|item| {
        let TranscriptItem::Message { message, .. } = item else {
            return None;
        };
        if assistant && message.role != MessageRole::Assistant {
            return None;
        }
        message.parts.iter().rev().find_map(|part| match part {
            MessagePart::Text { text } if assistant => Some(text.clone()),
            MessagePart::ToolResult { content, .. } if !assistant => Some(content.clone()),
            _ => None,
        })
    })
}

fn write_osc52(payload: &str) {
    use base64::Engine;
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload.as_bytes());
    let _ = std::io::stderr().write_all(format!("\x1b]52;c;{encoded}\x07").as_bytes());
    let _ = std::io::stderr().flush();
}

async fn goal(session: &SessionClient, command: &str) {
    let arg = command.strip_prefix("goal").unwrap_or("").trim();
    if arg.is_empty() {
        match session.current().projection().goal.as_deref() {
            Some(goal) => println!("[atman] goal: {goal}"),
            None => println!("[atman] no session goal set"),
        }
        return;
    }
    let goal = (arg != "clear").then(|| arg.to_owned());
    match session.set_goal(goal.clone()).await {
        Ok(_) if goal.is_some() => println!("[atman] goal set: {arg}"),
        Ok(_) => println!("[atman] goal cleared"),
        Err(error) => eprintln!("[atman] :goal failed: {error}"),
    }
}

async fn todo(session: &SessionClient, command: &str) {
    let arg = command.strip_prefix("todo").unwrap_or("").trim();
    if matches!(arg, "" | "list") {
        print_todos(session);
        return;
    }
    let mutation = if arg == "clear" {
        atman_proto::TodoMutation::Clear
    } else if let Some(id) = arg.strip_prefix("done ") {
        let Some(id) = resolve_todo_id(session, id.trim()) else {
            return;
        };
        atman_proto::TodoMutation::SetState {
            id,
            state: atman_proto::TodoState::Done,
        }
    } else if let Some(id) = arg.strip_prefix("cancel ") {
        let Some(id) = resolve_todo_id(session, id.trim()) else {
            return;
        };
        atman_proto::TodoMutation::SetState {
            id,
            state: atman_proto::TodoState::Cancelled,
        }
    } else {
        eprintln!("[atman] :todo: unknown `{arg}` (try: list / done <id> / cancel <id> / clear)");
        return;
    };
    match session.update_todos(mutation).await {
        Ok(_) if arg == "clear" => println!("[atman] todos cleared"),
        Ok(_) => println!("[atman] todo updated"),
        Err(error) => eprintln!("[atman] :todo failed: {error}"),
    }
}

fn resolve_todo_id(session: &SessionClient, input: &str) -> Option<String> {
    if uuid::Uuid::parse_str(input).is_ok() {
        return Some(input.to_owned());
    }
    let Ok(index) = input.parse::<usize>() else {
        eprintln!("[atman] bad todo id `{input}` (use uuid or list index)");
        return None;
    };
    let state = session.current();
    match state.projection().todos.get(index) {
        Some(todo) => Some(todo.id.clone()),
        None => {
            eprintln!("[atman] todo index {index} out of range");
            None
        }
    }
}

fn print_todos(session: &SessionClient) {
    let state = session.current();
    if state.projection().todos.is_empty() {
        println!("[atman] no todos yet");
        return;
    }
    for (index, todo) in state.projection().todos.iter().enumerate() {
        let glyph = match todo.state {
            atman_proto::TodoState::Pending => "○",
            atman_proto::TodoState::InProgress => "⚡",
            atman_proto::TodoState::Done => "✓",
            atman_proto::TodoState::Cancelled => "✗",
        };
        println!("  {index:>2}  {glyph} {}  ({})", todo.where_, todo.id);
    }
}

fn print_cost(session: &SessionClient) {
    let state = session.current();
    let usage = &state.projection().usage;
    println!(
        "total llm_calls: {} · input: {} · output: {}",
        usage.llm_calls, usage.input_tokens, usage.output_tokens
    );
}

fn print_model(session: &SessionClient) {
    let state = session.current();
    let model = state
        .projection()
        .runs
        .iter()
        .rev()
        .find_map(|run| run.model.as_deref())
        .unwrap_or("(none)");
    println!("current model: {model}");
}
