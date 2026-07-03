use anyhow::{Context, Result, bail};
use atman_dsl::parse::parse_file;
use atman_runtime::providers::anthropic::AnthropicProvider;
use atman_runtime::providers::mock::MockProvider;
use atman_runtime::providers::openai::OpenAiProvider;
use atman_runtime::{Executor, Session, Value, tools};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser, Debug)]
#[command(
    name = "atman",
    version,
    about = "atman — flow-driven code agent runtime"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    Run {
        file: PathBuf,
        #[arg(long)]
        flow: Option<String>,
        #[arg(long)]
        mock: bool,
        #[arg(long)]
        ephemeral: bool,
        args: Vec<String>,
    },
    Logs {
        #[command(subcommand)]
        action: LogsAction,
    },
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    Cost {
        session_id: Option<String>,
    },
    Doctor,
    Version,
}

#[derive(Subcommand, Debug)]
enum LogsAction {
    Tail {
        session_id: Option<String>,
        #[arg(long, default_value_t = 40)]
        n: usize,
        #[arg(long)]
        follow: bool,
    },
}

#[derive(Subcommand, Debug)]
enum SessionAction {
    List,
    Show { session_id: String },
    Gc,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        None => cmd_repl().await,
        Some(Cmd::Version) => {
            println!("atman v{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(Cmd::Run {
            file,
            flow,
            mock,
            ephemeral,
            args,
        }) => cmd_run(file, flow, mock, ephemeral, args).await,
        Some(Cmd::Logs {
            action:
                LogsAction::Tail {
                    session_id,
                    n,
                    follow,
                },
        }) => cmd_logs_tail(session_id, n, follow).await,
        Some(Cmd::Session {
            action: SessionAction::List,
        }) => cmd_session_list().await,
        Some(Cmd::Session {
            action: SessionAction::Show { session_id },
        }) => cmd_session_show(session_id).await,
        Some(Cmd::Session {
            action: SessionAction::Gc,
        }) => cmd_session_gc().await,
        Some(Cmd::Cost { session_id }) => cmd_cost(session_id).await,
        Some(Cmd::Doctor) => cmd_doctor().await,
    }
}

async fn cmd_run(
    file: PathBuf,
    flow_name: Option<String>,
    mock: bool,
    ephemeral: bool,
    raw_args: Vec<String>,
) -> Result<()> {
    let source =
        std::fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
    let parsed = parse_file(&source).with_context(|| format!("parsing {}", file.display()))?;

    let flow_name = match flow_name {
        Some(n) => n,
        None => {
            if parsed.flows.len() != 1 {
                bail!(
                    "{} has {} flows; pass --flow=<name> to disambiguate",
                    file.display(),
                    parsed.flows.len()
                );
            }
            parsed.flows[0].name.name.clone()
        }
    };

    let args = parse_args(&raw_args)?;

    let session = if ephemeral {
        Session::open_ephemeral()
    } else {
        let root = data_dir()?;
        Session::open(&root).with_context(|| format!("opening session under {}", root.display()))?
    };

    if let Some(path) = session.events_path() {
        eprintln!("[atman] session={} events={}", session.id(), path.display());
    }

    let mut executor = Executor::with_events(session.sink().clone());
    let fetch_rule = build_fetch_rule_with_migrations().await;
    tools::register_tier_zero_with_rules(&mut executor.tools, fetch_rule);
    tools::register_shell(&mut executor.tools);
    tools::register_preview(&mut executor.tools, load_preview_config());
    register_providers_from_env(&mut executor);
    let mcp_configs = load_mcp_configs();
    let mcp_status =
        atman_runtime::mcp::register_from_configs(&mut executor.tools, &mcp_configs).await;
    for outcome in &mcp_status {
        match outcome {
            Ok(s) => eprintln!(
                "[atman] mcp `{}` connected ({} tools)",
                s.name, s.tool_count
            ),
            Err(e) => eprintln!("[atman] mcp boot: {e}"),
        }
    }
    if mock {
        executor.providers.register(Arc::new(
            MockProvider::new("mock").with_fallback(Value::Str("[mock response]".into())),
        ));
    }

    let turn_id = atman_runtime::event::TurnId::now();
    let user_msg = atman_runtime::message::Message::user_text(
        turn_id.clone(),
        format!("atman run {} flow={flow_name}", file.display()),
    );
    session.begin_turn(user_msg);
    let outcome = executor
        .run_in_turn(&parsed, &flow_name, args, Some(turn_id), Some(&session))
        .await;
    session.end_turn();
    session.shutdown().await;

    match outcome {
        Ok(v) => {
            println!("{}", render_value(&v));
            Ok(())
        }
        Err(e) => {
            eprintln!("flow error: {e}");
            std::process::exit(1);
        }
    }
}

async fn cmd_session_list() -> Result<()> {
    let root = data_dir()?;
    let sessions = root.join("sessions");
    if !sessions.exists() {
        return Ok(());
    }
    let mut rows: Vec<(std::time::SystemTime, String, u64, usize)> = Vec::new();
    for entry in std::fs::read_dir(&sessions)? {
        let entry = entry?;
        if !entry.path().is_dir() {
            continue;
        }
        let sid = entry.file_name().to_string_lossy().to_string();
        let events_path = entry.path().join("events.jsonl");
        let (bytes, events) = match std::fs::metadata(&events_path) {
            Ok(m) => (m.len(), count_lines(&events_path)),
            Err(_) => (0, 0),
        };
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        rows.push((modified, sid, bytes, events));
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    println!("{:<38} {:>10} {:>10}", "session_id", "events", "bytes");
    for (_, sid, bytes, events) in rows {
        println!("{:<38} {:>10} {:>10}", sid, events, bytes);
    }
    Ok(())
}

async fn cmd_session_show(sid: String) -> Result<()> {
    let root = data_dir()?;
    let dir = root.join("sessions").join(&sid);
    if !dir.is_dir() {
        bail!("session not found: {}", dir.display());
    }
    let events_path = dir.join("events.jsonl");
    let mut flow_start = 0usize;
    let mut flow_end = 0usize;
    let mut llm_call = 0usize;
    if events_path.exists() {
        let contents = tokio::fs::read_to_string(&events_path).await?;
        for line in contents.lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line) {
                match v["type"].as_str() {
                    Some("flow_start") => flow_start += 1,
                    Some("flow_end") => flow_end += 1,
                    Some("llm_call") => llm_call += 1,
                    _ => {}
                }
            }
        }
    }
    let size = std::fs::metadata(&events_path)
        .map(|m| m.len())
        .unwrap_or(0);
    println!("session_id: {sid}");
    println!("dir:        {}", dir.display());
    println!("events:     {} bytes", size);
    println!("flow_start: {flow_start}");
    println!("flow_end:   {flow_end}");
    println!("llm_call:   {llm_call}");
    Ok(())
}

async fn cmd_session_gc() -> Result<()> {
    let root = data_dir()?;
    let sessions = root.join("sessions");
    if !sessions.exists() {
        return Ok(());
    }
    let mut removed = 0usize;
    for entry in std::fs::read_dir(&sessions)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let events_path = path.join("events.jsonl");
        let empty = match std::fs::metadata(&events_path) {
            Ok(m) => m.len() == 0,
            Err(_) => true,
        };
        if empty {
            std::fs::remove_dir_all(&path).with_context(|| format!("rm -r {}", path.display()))?;
            removed += 1;
        }
    }
    println!("gc removed {} empty session(s)", removed);
    Ok(())
}

fn count_lines(path: &std::path::Path) -> usize {
    match std::fs::read_to_string(path) {
        Ok(s) => s.lines().filter(|l| !l.trim().is_empty()).count(),
        Err(_) => 0,
    }
}

enum RouteOutcome {
    Handled(Value),
    HandledErr(anyhow::Error),
    Unmatched,
}

async fn route_input_in_turn(
    line: &str,
    executor: &Executor,
    session: &Session,
    turn_id: atman_runtime::event::TurnId,
) -> RouteOutcome {
    let Some(call) = resolve_route_call(line) else {
        return RouteOutcome::Unmatched;
    };
    match run_slash_command_in_turn(&call, executor, session, turn_id).await {
        Ok(v) => RouteOutcome::Handled(v),
        Err(e) => RouteOutcome::HandledErr(e),
    }
}

fn resolve_route_call(line: &str) -> Option<String> {
    let cfg = config_dir().ok()?;
    let routes_path = cfg.join("routes.toml");
    if !routes_path.exists() {
        return None;
    }
    let contents = std::fs::read_to_string(&routes_path).ok()?;
    for (i, raw_line) in contents.lines().enumerate() {
        let trimmed = raw_line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let Some((prefix, command)) = trimmed.split_once("->") else {
            eprintln!(
                "[atman] routes.toml:{}: expected `<prefix> -> <command>`",
                i + 1
            );
            continue;
        };
        let prefix = prefix.trim().trim_matches('"');
        let command = command.trim();
        if let Some(rest) = line.strip_prefix(prefix) {
            let rest = rest.trim();
            let call = if rest.is_empty() {
                command.to_string()
            } else {
                format!("{command} {rest}")
            };
            return Some(call);
        }
    }
    None
}

async fn run_boot_flow(executor: &Executor) -> Result<()> {
    let cfg = match config_dir() {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };
    let path = cfg.join("on_session_start.at");
    if !path.exists() {
        return Ok(());
    }
    let source =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let parsed = parse_file(&source).with_context(|| format!("parsing {}", path.display()))?;
    if parsed.flows.is_empty() {
        return Ok(());
    }
    let flow_name = parsed.flows[0].name.name.clone();
    let value = executor.run(&parsed, &flow_name, vec![]).await?;
    let rendered = render_value(&value);
    if !rendered.is_empty() {
        println!("{rendered}");
    }
    Ok(())
}

async fn run_slash_command_in_turn(
    line: &str,
    executor: &Executor,
    session: &Session,
    turn_id: atman_runtime::event::TurnId,
) -> Result<Value> {
    let (parsed, flow_name, kv) = resolve_slash_command(line)?;
    executor
        .run_in_turn(&parsed, &flow_name, kv, Some(turn_id), Some(session))
        .await
        .map_err(Into::into)
}

type SlashCommandParsed = (atman_dsl::ast::File, String, Vec<(String, Value)>);

fn resolve_slash_command(line: &str) -> Result<SlashCommandParsed> {
    let mut parts = line.split_whitespace();
    let name = parts.next().context("empty slash command")?;
    let cfg = config_dir()?;
    let path = cfg.join("commands").join(format!("{name}.at"));
    if !path.exists() {
        bail!("no such command: {} (looked for {})", name, path.display());
    }
    let source =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let parsed = parse_file(&source).with_context(|| format!("parsing {}", path.display()))?;
    if parsed.flows.len() != 1 {
        bail!("{} must contain exactly one flow", path.display());
    }
    let flow = &parsed.flows[0];
    let flow_name = flow.name.name.clone();
    let params: Vec<String> = flow.params.iter().map(|(id, _)| id.name.clone()).collect();

    let mut kv: Vec<(String, Value)> = Vec::new();
    let mut positional_index = 0usize;
    for tok in parts {
        if let Some((k, v)) = tok.split_once('=') {
            kv.push((k.to_string(), Value::Str(v.to_string())));
        } else if positional_index < params.len() {
            kv.push((
                params[positional_index].clone(),
                Value::Str(tok.to_string()),
            ));
            positional_index += 1;
        } else {
            bail!(
                "extra positional argument `{tok}` (flow expects {} params)",
                params.len()
            );
        }
    }
    Ok((parsed, flow_name, kv))
}

#[derive(Default)]
struct PendingUserMessage {
    attachments: Vec<std::path::PathBuf>,
}

async fn cmd_repl() -> Result<()> {
    use std::collections::VecDeque;
    use tokio::sync::mpsc;

    println!(
        "atman v{} — type `:help` for commands, `:exit` to leave, `!nudge <text>` or `!stop` while a flow is running",
        env!("CARGO_PKG_VERSION")
    );

    let root = data_dir()?;
    let session = Session::open(&root)
        .with_context(|| format!("opening session under {}", root.display()))?;
    if let Some(path) = session.events_path() {
        println!("[atman] session={} events={}", session.id(), path.display());
    }

    let mut executor = Executor::with_events(session.sink().clone());
    let fetch_rule = build_fetch_rule_with_migrations().await;
    tools::register_tier_zero_with_rules(&mut executor.tools, fetch_rule);
    tools::register_shell(&mut executor.tools);
    tools::register_preview(&mut executor.tools, load_preview_config());
    register_providers_from_env(&mut executor);
    let mcp_configs = load_mcp_configs();
    let mcp_status =
        atman_runtime::mcp::register_from_configs(&mut executor.tools, &mcp_configs).await;
    for outcome in &mcp_status {
        match outcome {
            Ok(s) => println!(
                "[atman] mcp `{}` connected ({} tools)",
                s.name, s.tool_count
            ),
            Err(e) => eprintln!("[atman] mcp boot: {e}"),
        }
    }

    if let Err(e) = run_boot_flow(&executor).await {
        eprintln!("[atman] boot flow error: {e}");
    }

    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<String>();
    spawn_stdin_reader(input_tx);
    let mut pending = PendingUserMessage::default();
    let mut pushback: VecDeque<String> = VecDeque::new();
    let sid = session.id().to_string();

    loop {
        let line = if let Some(l) = pushback.pop_front() {
            l
        } else {
            match input_rx.recv().await {
                Some(l) => l,
                None => break,
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(':') {
            if !handle_builtin(rest.trim(), sid.as_str(), &mut pending) {
                break;
            }
            continue;
        }
        let (text, kind) = if let Some(rest) = line.strip_prefix('/') {
            (rest.trim().to_string(), TurnKind::Slash)
        } else {
            let trimmed = line.trim();
            if resolve_route_call(trimmed).is_none() {
                println!(
                    "[atman] no route matched. add `\"prefix\" -> command` to ~/.config/atman/routes.toml, or use `/name args...`."
                );
                continue;
            }
            (trimmed.to_string(), TurnKind::Bare)
        };
        run_turn_with_interjection(
            &session,
            &executor,
            &text,
            &mut pending,
            kind,
            &mut input_rx,
            &mut pushback,
        )
        .await;
    }

    session.shutdown().await;
    drop(executor);
    Ok(())
}

fn spawn_stdin_reader(tx: tokio::sync::mpsc::UnboundedSender<String>) {
    let non_interactive = std::env::var("ATMAN_REPL_NON_INTERACTIVE").is_ok();
    tokio::task::spawn_blocking(move || {
        if non_interactive {
            use std::io::BufRead;
            let stdin = std::io::stdin();
            let locked = stdin.lock();
            for line in locked.lines() {
                let Ok(l) = line else { break };
                if tx.send(l).is_err() {
                    break;
                }
            }
        } else {
            use rustyline::error::ReadlineError;
            use rustyline::{Config, DefaultEditor};
            let config = Config::builder().auto_add_history(true).build();
            let mut editor: DefaultEditor = match DefaultEditor::with_config(config) {
                Ok(e) => e,
                Err(e) => {
                    eprintln!("[atman] rustyline init failed: {e}");
                    return;
                }
            };
            loop {
                match editor.readline("atman> ") {
                    Ok(l) => {
                        if tx.send(l).is_err() {
                            break;
                        }
                    }
                    Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => break,
                    Err(e) => {
                        eprintln!("[atman] readline error: {e}");
                        break;
                    }
                }
            }
        }
    });
}

enum TurnKind {
    Slash,
    Bare,
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_with_interjection(
    session: &Session,
    executor: &Executor,
    raw_line: &str,
    pending: &mut PendingUserMessage,
    kind: TurnKind,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    pushback: &mut std::collections::VecDeque<String>,
) {
    let (text, inline_attachments) = extract_at_paths(raw_line);
    let mut attachments = std::mem::take(&mut pending.attachments);
    attachments.extend(inline_attachments);
    let turn_id = atman_runtime::event::TurnId::now();
    let user_msg = build_user_message(&text, &attachments, turn_id.clone());
    session.begin_turn(user_msg);

    let flow_fut = async {
        match kind {
            TurnKind::Slash => run_slash_command_in_turn(&text, executor, session, turn_id).await,
            TurnKind::Bare => match route_input_in_turn(&text, executor, session, turn_id).await {
                RouteOutcome::Handled(v) => Ok(v),
                RouteOutcome::HandledErr(e) => Err(e),
                RouteOutcome::Unmatched => Err(anyhow::anyhow!(
                    "no route matched. add `\"prefix\" -> command` to ~/.config/atman/routes.toml, or use `/name args...`."
                )),
            },
        }
    };
    tokio::pin!(flow_fut);

    let result = loop {
        tokio::select! {
            biased;
            r = &mut flow_fut => break r,
            Some(line) = input_rx.recv() => {
                if !consume_interjection_input(&line, session) {
                    pushback.push_back(line);
                }
            }
        }
    };

    match result {
        Ok(v) => println!("{}", render_value(&v)),
        Err(e) => eprintln!("error: {e}"),
    }
    session.end_turn();
}

/// Returns true if the line was fully consumed as an interjection (`!nudge` / `!course-correct` /
/// `!redirect` / `!stop`) or reported as a busy-warning, false if it should be pushed back for the
/// main loop (e.g. `:exit` or a normal command arriving before the current flow finishes).
fn consume_interjection_input(line: &str, session: &Session) -> bool {
    use atman_runtime::injection::InjectionLevel;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return true;
    }
    if trimmed == "!stop" {
        session.cancel_flow();
        let _ = session.enqueue_injection_with_level("stop", InjectionLevel::L4HardStop, None);
        println!("[atman] stop requested; flow will abort at next node boundary");
        return true;
    }
    if let Some(text) = trimmed.strip_prefix("!course-correct ") {
        let text = text.trim();
        if text.is_empty() {
            eprintln!("[atman] usage: !course-correct <text>");
            return true;
        }
        match session.enqueue_injection_with_level(text, InjectionLevel::L2CourseCorrect, None) {
            Ok(id) => println!(
                "[atman] course-correct queued ({id}) — llm restarts at next chunk boundary"
            ),
            Err(e) => eprintln!("[atman] course-correct rejected: {e}"),
        }
        return true;
    }
    if let Some(target) = trimmed.strip_prefix("!redirect ") {
        let target = target.trim();
        if target.is_empty() {
            eprintln!("[atman] usage: !redirect <flow_name>");
            return true;
        }
        match session.enqueue_injection_with_level(
            target,
            InjectionLevel::L3Redirect,
            Some(target.to_string()),
        ) {
            Ok(id) => println!("[atman] redirect queued ({id}) → {target}"),
            Err(e) => eprintln!("[atman] redirect rejected: {e}"),
        }
        return true;
    }
    if let Some(text) = trimmed
        .strip_prefix("!nudge ")
        .or_else(|| trimmed.strip_prefix('!'))
    {
        let text = text.trim();
        if text.is_empty() {
            eprintln!(
                "[atman] usage while flow runs: !nudge <text> | !course-correct <text> | !redirect <flow> | !stop"
            );
            return true;
        }
        match session.enqueue_injection(text) {
            Ok(id) => println!("[atman] nudge queued ({id}) — will inject at next llm node"),
            Err(e) => eprintln!("[atman] nudge rejected: {e}"),
        }
        return true;
    }
    false
}

fn extract_at_paths(line: &str) -> (String, Vec<std::path::PathBuf>) {
    let mut text = String::with_capacity(line.len());
    let mut attachments = Vec::new();
    let mut first = true;
    for tok in line.split_whitespace() {
        if let Some(rest) = tok
            .strip_prefix("@./")
            .or_else(|| tok.strip_prefix("@../"))
            .or_else(|| tok.strip_prefix("@/"))
        {
            let prefix = if tok.starts_with("@./") {
                "./"
            } else if tok.starts_with("@../") {
                "../"
            } else {
                "/"
            };
            attachments.push(std::path::PathBuf::from(format!("{prefix}{rest}")));
        } else {
            if !first {
                text.push(' ');
            }
            text.push_str(tok);
            first = false;
        }
    }
    (text, attachments)
}

fn build_user_message(
    text: &str,
    attachments: &[std::path::PathBuf],
    turn_id: atman_runtime::event::TurnId,
) -> atman_runtime::message::Message {
    use atman_runtime::message::{ImageData, ImageSource, Message, MessagePart, MessageRole};
    let mut parts: Vec<MessagePart> = Vec::new();
    for path in attachments {
        let media_type = guess_image_mime(path).unwrap_or_else(|| "image/png".to_string());
        parts.push(MessagePart::Image {
            source: ImageSource {
                media_type,
                data: ImageData::Path { path: path.clone() },
            },
        });
    }
    if !text.is_empty() {
        parts.push(MessagePart::Text { text: text.into() });
    }
    Message {
        role: MessageRole::User,
        parts,
        turn_id,
    }
}

fn guess_image_mime(path: &std::path::Path) -> Option<String> {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())?
        .to_ascii_lowercase();
    Some(
        match ext.as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            _ => return None,
        }
        .to_string(),
    )
}

fn handle_builtin(cmd: &str, sid: &str, pending: &mut PendingUserMessage) -> bool {
    if let Some(rest) = cmd.strip_prefix("attach") {
        let arg = rest.trim();
        match arg {
            "" => {
                eprintln!(":attach <path>  |  :attach clear  |  :attach list");
                return true;
            }
            "clear" => {
                pending.attachments.clear();
                println!("[atman] pending attachments cleared");
                return true;
            }
            "list" => {
                if pending.attachments.is_empty() {
                    println!("[atman] no pending attachments");
                } else {
                    for (i, p) in pending.attachments.iter().enumerate() {
                        println!("  {i}: {}", p.display());
                    }
                }
                return true;
            }
            path => {
                let expanded = std::path::PathBuf::from(path);
                if !expanded.exists() {
                    eprintln!(":attach: file not found: {}", expanded.display());
                    return true;
                }
                pending.attachments.push(expanded.clone());
                println!(
                    "[atman] attached {} (pending count: {})",
                    expanded.display(),
                    pending.attachments.len()
                );
                return true;
            }
        }
    }
    match cmd {
        "help" => {
            println!(":help                — show this");
            println!(":exit | :quit        — leave REPL");
            println!(":session             — print current session id");
            println!(":cost                — cost summary for current session");
            println!(":attach <path>       — attach file to next turn");
            println!(":attach clear|list   — manage pending attachments");
            println!("@./path or @/abs     — inline attach in bare input");
            true
        }
        "exit" | "quit" => false,
        "session" => {
            println!("session_id: {sid}");
            true
        }
        "cost" => {
            eprintln!("(hint) run `atman cost {sid}` in another shell for now");
            true
        }
        other => {
            eprintln!("unknown builtin `:{other}` — try `:help`");
            true
        }
    }
}

async fn cmd_cost(session_id: Option<String>) -> Result<()> {
    use std::collections::BTreeMap;

    let root = data_dir()?;
    let sid = match session_id {
        Some(s) => s,
        None => latest_session(&root)?
            .with_context(|| format!("no sessions found under {}", root.display()))?,
    };
    let path = root.join("sessions").join(&sid).join("events.jsonl");
    if !path.exists() {
        bail!("events file not found: {}", path.display());
    }

    let contents = tokio::fs::read_to_string(&path).await?;
    let mut by_model: BTreeMap<String, (u64, u64, u64, u64, u64)> = BTreeMap::new();
    let mut total_calls = 0u64;
    for line in contents.lines() {
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if v["type"] != "llm_call" {
            continue;
        }
        let model = v["model"].as_str().unwrap_or("<unknown>").to_string();
        let input = v["usage"]["input"].as_u64().unwrap_or(0);
        let cached = v["usage"]["cached_input"].as_u64().unwrap_or(0);
        let output = v["usage"]["output"].as_u64().unwrap_or(0);
        let wall = v["wallclock_ms"].as_u64().unwrap_or(0);
        let entry = by_model.entry(model).or_insert((0, 0, 0, 0, 0));
        entry.0 += 1;
        entry.1 += input;
        entry.2 += cached;
        entry.3 += output;
        entry.4 += wall;
        total_calls += 1;
    }

    println!("session: {sid}");
    println!("total llm_calls: {total_calls}");
    println!();
    println!(
        "{:<32} {:>6} {:>10} {:>10} {:>10} {:>10}",
        "model", "calls", "in", "cached", "out", "wall_ms"
    );
    for (model, (calls, input, cached, output, wall)) in &by_model {
        println!(
            "{:<32} {:>6} {:>10} {:>10} {:>10} {:>10}",
            model, calls, input, cached, output, wall
        );
    }
    Ok(())
}

async fn cmd_doctor() -> Result<()> {
    let data = data_dir()?;
    let cfg = config_dir()?;
    let sessions = data.join("sessions");
    let commands = cfg.join("commands");

    let session_count = if sessions.exists() {
        std::fs::read_dir(&sessions)
            .map(|it| it.filter_map(|e| e.ok()).count())
            .unwrap_or(0)
    } else {
        0
    };
    let commands_count = if commands.exists() {
        std::fs::read_dir(&commands)
            .map(|it| {
                it.filter_map(|e| e.ok())
                    .filter(|e| {
                        e.path()
                            .extension()
                            .and_then(|s| s.to_str())
                            .map(|s| s == "at")
                            .unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0)
    } else {
        0
    };

    println!("atman v{}", env!("CARGO_PKG_VERSION"));
    println!("data_dir:   {}", data.display());
    println!(
        " sessions:  {} ({} entries)",
        sessions.display(),
        session_count
    );
    println!("config_dir: {}", cfg.display());
    println!(
        " commands:  {} ({} .at files)",
        commands.display(),
        commands_count
    );
    println!();
    println!("providers:");
    for (name, env) in [
        ("anthropic", "ANTHROPIC_API_KEY"),
        ("openai", "OPENAI_API_KEY"),
        ("glm (anthropic compat)", "ATMAN_TEST_GLM_KEY"),
    ] {
        let mark = if std::env::var(env).is_ok() {
            "✓"
        } else {
            "✗"
        };
        println!("  [{mark}] {name:<28} ${env}");
    }
    println!();
    let pcfg = load_preview_config();
    let ping = atman_runtime::tools::preview::ping(&pcfg.base_url, pcfg.timeout_ms).await;
    let (mark, note) = match &ping {
        atman_runtime::tools::preview::PingResult::Ok => ("✓", String::new()),
        atman_runtime::tools::preview::PingResult::Unavailable => (
            "✗",
            " (server not running; preview.push will return status=unavailable)".to_string(),
        ),
        atman_runtime::tools::preview::PingResult::Fail(msg) => ("✗", format!(" ({msg})")),
    };
    println!("preview:");
    println!("  [{mark}] {}{}", pcfg.base_url, note);
    println!();
    println!();
    println!("migrated rules:");
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    if let Ok(home) = std::env::var("HOME") {
        let rules =
            atman_runtime::migration::scan_migrated_rules(&cwd, std::path::Path::new(&home));
        if rules.is_empty() {
            println!("  (none detected in project or user home)");
        } else {
            for r in &rules {
                let scope = match r.scope {
                    atman_runtime::migration::RuleScope::Project => "project",
                    atman_runtime::migration::RuleScope::Global => "global ",
                };
                println!(
                    "  [✓] {:<30} [{:<8}] {} — {}",
                    r.name,
                    r.source_tool,
                    scope,
                    r.source_path.display()
                );
            }
        }
    } else {
        println!("  (HOME env not set)");
    }
    println!();
    println!("mcp:");
    let mcp_configs = load_mcp_configs();
    if mcp_configs.is_empty() {
        println!("  (none configured — add [[mcp]] blocks to config.toml)");
    } else {
        let mut probe_registry = atman_runtime::ToolRegistry::new();
        let statuses =
            atman_runtime::mcp::register_from_configs(&mut probe_registry, &mcp_configs).await;
        for (cfg, status) in mcp_configs.iter().zip(statuses.iter()) {
            match status {
                Ok(s) => println!(
                    "  [✓] {:<20} {} tools ({} {})",
                    cfg.name,
                    s.tool_count,
                    cfg.command,
                    cfg.args.join(" ")
                ),
                Err(e) => println!("  [✗] {:<20} {}", cfg.name, e.error),
            }
        }
    }
    Ok(())
}

async fn build_fetch_rule_with_migrations() -> atman_runtime::tools::memory_stubs::FetchRule {
    let fetch_rule = atman_runtime::tools::memory_stubs::FetchRule::new();
    if std::env::var("ATMAN_DISABLE_MIGRATION").is_ok() {
        return fetch_rule;
    }
    let project_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let home = match std::env::var("HOME") {
        Ok(h) => std::path::PathBuf::from(h),
        Err(_) => return fetch_rule,
    };
    let rules = atman_runtime::migration::scan_migrated_rules(&project_root, &home);
    fetch_rule.set_migrated(rules).await;
    fetch_rule
}

fn register_providers_from_env(executor: &mut Executor) {
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        let mut p = AnthropicProvider::new("anthropic", key);
        if let Ok(url) = std::env::var("ANTHROPIC_BASE_URL") {
            p = p.with_base_url(url);
        }
        executor.providers.register(Arc::new(p));
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        let mut p = OpenAiProvider::new("openai", key);
        if let Ok(url) = std::env::var("OPENAI_BASE_URL") {
            p = p.with_base_url(url);
        }
        executor.providers.register(Arc::new(p));
    }
}

fn load_preview_config() -> atman_runtime::tools::preview::PreviewConfig {
    let cfg = atman_runtime::tools::preview::PreviewConfig::default();
    let Ok(dir) = config_dir() else {
        return cfg;
    };
    let path = dir.join("config.toml");
    if !path.exists() {
        return cfg;
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return cfg;
    };
    parse_preview_config(&text, cfg)
}

fn load_mcp_configs() -> Vec<atman_runtime::mcp::McpServerConfig> {
    let Ok(dir) = config_dir() else {
        return Vec::new();
    };
    let path = dir.join("config.toml");
    if !path.exists() {
        return Vec::new();
    }
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    parse_mcp_configs(&text)
}

#[derive(Debug, serde::Deserialize)]
struct RawMcpConfigFile {
    #[serde(default)]
    mcp: Vec<RawMcpConfig>,
}

#[derive(Debug, serde::Deserialize)]
struct RawMcpConfig {
    name: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    tier: Option<u8>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

fn parse_mcp_configs(text: &str) -> Vec<atman_runtime::mcp::McpServerConfig> {
    let file: RawMcpConfigFile = match toml::from_str(text) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    file.mcp
        .into_iter()
        .map(|raw| atman_runtime::mcp::McpServerConfig {
            name: raw.name,
            command: raw.command,
            args: raw.args,
            tier: tier_from_int(raw.tier.unwrap_or(3)),
            timeout_ms: raw.timeout_ms.unwrap_or(30_000),
        })
        .collect()
}

fn tier_from_int(n: u8) -> atman_runtime::Tier {
    match n {
        0 => atman_runtime::Tier::Zero,
        1 => atman_runtime::Tier::One,
        2 => atman_runtime::Tier::Two,
        3 => atman_runtime::Tier::Three,
        _ => atman_runtime::Tier::Four,
    }
}

fn parse_preview_config(
    text: &str,
    mut cfg: atman_runtime::tools::preview::PreviewConfig,
) -> atman_runtime::tools::preview::PreviewConfig {
    let mut in_section = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[')
            && let Some(name) = rest.strip_suffix(']')
        {
            in_section = name.trim() == "preview";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let key = k.trim();
        let val = v.trim().trim_matches('"');
        match key {
            "base_url" => cfg.base_url = val.to_string(),
            "timeout_ms" => {
                if let Ok(n) = val.parse::<u64>() {
                    cfg.timeout_ms = n;
                }
            }
            "project_abs_path" => cfg.project_abs_path = val.to_string(),
            "project_hint_slug" => cfg.project_hint_slug = Some(val.to_string()),
            "max_body_bytes" => {
                if let Ok(n) = val.parse::<usize>() {
                    cfg.max_body_bytes = n;
                }
            }
            _ => {}
        }
    }
    cfg
}

async fn cmd_logs_tail(session_id: Option<String>, n: usize, follow: bool) -> Result<()> {
    let root = data_dir()?;
    let sid = match session_id {
        Some(s) => s,
        None => latest_session(&root)?
            .with_context(|| format!("no sessions found under {}", root.display()))?,
    };
    let path = root.join("sessions").join(&sid).join("events.jsonl");
    if !path.exists() {
        bail!("events file not found: {}", path.display());
    }

    let contents = tokio::fs::read_to_string(&path).await?;
    let lines: Vec<&str> = contents.lines().collect();
    let start = lines.len().saturating_sub(n);
    for line in &lines[start..] {
        println!("{line}");
    }

    if follow {
        eprintln!("[atman] --follow not yet implemented");
    }
    Ok(())
}

fn data_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("ATMAN_DATA_DIR") {
        return Ok(PathBuf::from(p));
    }
    let proj = ProjectDirs::from("", "", "atman")
        .context("could not determine XDG data dir; set ATMAN_DATA_DIR to override")?;
    Ok(proj.data_dir().to_path_buf())
}

fn config_dir() -> Result<PathBuf> {
    if let Ok(p) = std::env::var("ATMAN_CONFIG_DIR") {
        return Ok(PathBuf::from(p));
    }
    let proj = ProjectDirs::from("", "", "atman")
        .context("could not determine XDG config dir; set ATMAN_CONFIG_DIR to override")?;
    Ok(proj.config_dir().to_path_buf())
}

fn latest_session(root: &std::path::Path) -> Result<Option<String>> {
    let sessions = root.join("sessions");
    if !sessions.exists() {
        return Ok(None);
    }
    let mut best: Option<(std::time::SystemTime, String)> = None;
    for entry in std::fs::read_dir(&sessions)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        let name = entry.file_name().to_string_lossy().to_string();
        match &best {
            Some((t, _)) if *t >= modified => {}
            _ => best = Some((modified, name)),
        }
    }
    Ok(best.map(|(_, n)| n))
}

fn parse_args(raw: &[String]) -> Result<Vec<(String, Value)>> {
    let mut out = Vec::with_capacity(raw.len());
    for arg in raw {
        let (name, value) = arg
            .split_once('=')
            .with_context(|| format!("expected `name=value`, got `{arg}`"))?;
        out.push((name.to_string(), Value::Str(value.to_string())));
    }
    Ok(out)
}

fn render_value(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        Value::Int(n) => n.to_string(),
        Value::Float(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Unit => String::new(),
        other => format!("{other:?}"),
    }
}
