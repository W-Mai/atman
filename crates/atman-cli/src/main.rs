use anyhow::{Context, Result, bail};
use atman_dsl::parse::parse_file;
use atman_runtime::{Executor, Session, Value};
use clap::{Parser, Subcommand};
use directories::ProjectDirs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod migrate_source;
mod suggest;
mod sync;

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
    RebuildIndex,
    Version,
    Monitor {
        #[arg(long, default_value_t = 65098)]
        port: u16,
    },
    Daemon {
        #[command(subcommand)]
        action: DaemonAction,
    },
    Flow {
        #[command(subcommand)]
        action: FlowAction,
    },
    Sync {
        #[command(subcommand)]
        action: SyncAction,
    },
    Migrate {
        #[command(subcommand)]
        action: MigrateAction,
    },
}

#[derive(Subcommand, Debug)]
enum MigrateAction {
    List {
        #[arg(long, default_value = "opencode")]
        from: String,
        #[arg(long)]
        storage: Option<PathBuf>,
    },
    Import {
        session_id: String,
        #[arg(long, default_value = "opencode")]
        from: String,
        #[arg(long)]
        storage: Option<PathBuf>,
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Subcommand, Debug)]
enum SyncAction {
    Init {
        url: String,
        #[arg(long)]
        branch: Option<String>,
    },
    Push {
        #[arg(long)]
        message: Option<String>,
    },
    Pull,
    Status,
}

#[derive(Subcommand, Debug)]
enum FlowAction {
    Snapshot {
        path: PathBuf,
        #[arg(long)]
        author: Option<String>,
    },
    Versions {
        flow_name: String,
    },
    Diff {
        flow_name: String,
        from: String,
        to: String,
    },
    Rollback {
        flow_name: String,
        version: String,
        #[arg(long)]
        to: PathBuf,
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Subcommand, Debug)]
enum DaemonAction {
    Start,
    Stop,
    Status,
    RotateToken,
    Run {
        file: PathBuf,
        #[arg(long)]
        follow: bool,
        #[arg(long, default_value_t = 65099)]
        port: u16,
    },
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
        Some(Cmd::RebuildIndex) => cmd_rebuild_index().await,
        Some(Cmd::Monitor { port }) => cmd_monitor(port).await,
        Some(Cmd::Daemon {
            action: DaemonAction::Start,
        }) => cmd_daemon_start().await,
        Some(Cmd::Daemon {
            action: DaemonAction::Stop,
        }) => cmd_daemon_stop().await,
        Some(Cmd::Daemon {
            action: DaemonAction::Status,
        }) => cmd_daemon_status().await,
        Some(Cmd::Daemon {
            action: DaemonAction::RotateToken,
        }) => cmd_daemon_rotate_token().await,
        Some(Cmd::Flow { action }) => cmd_flow(action).await,
        Some(Cmd::Sync { action }) => cmd_sync(action).await,
        Some(Cmd::Migrate { action }) => cmd_migrate(action).await,
        Some(Cmd::Daemon {
            action: DaemonAction::Run { file, follow, port },
        }) => cmd_daemon_run(file, follow, port).await,
    }
}

async fn cmd_daemon_run(file: PathBuf, follow: bool, port: u16) -> Result<()> {
    let cfg_path = atman_daemon::config::default_config_path()?;
    let cfg = atman_daemon::config::DaemonConfig::load_or_init(&cfg_path)?;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();

    let abs = if file.is_absolute() {
        file.clone()
    } else {
        std::env::current_dir()?.join(&file)
    };

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "run_flow",
        "params": {"flow_path": abs.to_string_lossy()}
    });
    let resp = client
        .post(format!("{base}/rpc"))
        .bearer_auth(&cfg.auth_token)
        .json(&body)
        .send()
        .await
        .with_context(|| format!("POST {base}/rpc (is atman-daemon running?)"))?;
    if !resp.status().is_success() {
        bail!("daemon returned HTTP {}", resp.status());
    }
    let out: serde_json::Value = resp.json().await?;
    if let Some(err) = out.get("error") {
        bail!("daemon rpc error: {err}");
    }
    let sid = out["result"]["session_id"]
        .as_str()
        .context("no session_id in response")?
        .to_string();
    let rid = out["result"]["run_id"].as_str().unwrap_or("");
    println!("session_id: {sid}");
    println!("run_id:     {rid}");

    if !follow {
        return Ok(());
    }
    let sse = client
        .get(format!("{base}/events?session_id={sid}"))
        .bearer_auth(&cfg.auth_token)
        .send()
        .await?;
    use futures::StreamExt;
    let mut stream = sse.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        buf.extend_from_slice(&chunk);
        while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
            let line = buf.drain(..=nl).collect::<Vec<u8>>();
            let text = String::from_utf8_lossy(&line).trim().to_string();
            if let Some(data) = text.strip_prefix("data: ") {
                println!("{data}");
                if data.contains("\"flow_end\"") {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

async fn cmd_daemon_start() -> Result<()> {
    let pid_path = atman_daemon::pidfile::default_pid_path()?;
    if let Some(pid) = atman_daemon::pidfile::read_pid(&pid_path)?
        && atman_daemon::pidfile::is_alive(pid)
    {
        println!("atman-daemon already running (pid={pid})");
        return Ok(());
    }
    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("atman-daemon")))
        .filter(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("atman-daemon"));
    let child = std::process::Command::new(&bin)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .with_context(|| format!("spawning {}", bin.display()))?;
    println!("atman-daemon spawned (pid={})", child.id());
    println!("pid file: {}", pid_path.display());
    Ok(())
}

async fn cmd_daemon_stop() -> Result<()> {
    let pid_path = atman_daemon::pidfile::default_pid_path()?;
    let Some(pid) = atman_daemon::pidfile::read_pid(&pid_path)? else {
        println!(
            "no atman-daemon running (no pid file at {})",
            pid_path.display()
        );
        return Ok(());
    };
    if !atman_daemon::pidfile::is_alive(pid) {
        println!("stale pid file (pid={pid} not alive), removing");
        atman_daemon::pidfile::remove_pid(&pid_path);
        return Ok(());
    }
    let rc = unsafe { libc::kill(pid as libc::pid_t, libc::SIGTERM) };
    if rc != 0 {
        anyhow::bail!(
            "kill(pid={pid}, SIGTERM) failed: errno={}",
            std::io::Error::last_os_error()
        );
    }
    println!("sent SIGTERM to atman-daemon (pid={pid})");
    Ok(())
}

async fn cmd_daemon_status() -> Result<()> {
    let pid_path = atman_daemon::pidfile::default_pid_path()?;
    match atman_daemon::pidfile::read_pid(&pid_path)? {
        Some(pid) if atman_daemon::pidfile::is_alive(pid) => {
            println!("atman-daemon running (pid={pid})");
        }
        Some(pid) => {
            println!("atman-daemon pid file stale (pid={pid} not alive)");
        }
        None => {
            println!("atman-daemon not running");
        }
    }
    Ok(())
}

async fn cmd_daemon_rotate_token() -> Result<()> {
    let pid_path = atman_daemon::pidfile::default_pid_path()?;
    if let Some(pid) = atman_daemon::pidfile::read_pid(&pid_path)?
        && atman_daemon::pidfile::is_alive(pid)
    {
        anyhow::bail!("atman-daemon is running (pid={pid}). Stop it first: `atman daemon stop`");
    }
    let cfg_path = atman_daemon::config::default_config_path()?;
    let cfg = atman_daemon::config::DaemonConfig::rotate(&cfg_path)?;
    println!("{}", cfg.auth_token);
    eprintln!(
        "new token written to {}. restart daemon with `atman daemon start`.",
        cfg_path.display()
    );
    Ok(())
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

    let redactor = atman_daemon::bootstrap::build_redactor(config_dir().ok().as_deref());
    let session = if ephemeral {
        Session::open_ephemeral()
    } else {
        let root = data_dir()?;
        Session::open_with_redactor(&root, redactor.clone())
            .with_context(|| format!("opening session under {}", root.display()))?
    };

    if let Some(path) = session.events_path() {
        eprintln!("[atman] session={} events={}", session.id(), path.display());
    }

    let atman_daemon::bootstrap::BootstrapOutcome {
        mut executor,
        mcp_status,
    } = atman_daemon::bootstrap::build_executor(bootstrap_opts(session.sink().clone(), mock)?)
        .await?;
    for outcome in &mcp_status {
        match outcome {
            Ok(s) => eprintln!(
                "[atman] mcp `{}` connected via {} ({} tools)",
                s.name, s.transport, s.tool_count
            ),
            Err(e) => eprintln!("[atman] mcp boot: {e}"),
        }
    }
    attach_memory_stores(&mut executor, session.dir(), ephemeral)?;

    let target_flow = parsed
        .flows
        .iter()
        .find(|f| f.name.name == flow_name)
        .ok_or_else(|| anyhow::anyhow!("flow `{flow_name}` not found in {}", file.display()))?;
    if let Err(errs) = atman_runtime::validate::validate(target_flow, &executor.tools) {
        for e in &errs {
            eprintln!("[atman] validation: {e}");
        }
        bail!("flow validation failed with {} error(s)", errs.len());
    }

    if load_auto_snapshot() {
        auto_snapshot_flows(&file, &source, &parsed);
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
    if let Some(call) = resolve_dsl_route_call(line) {
        return Some(call);
    }
    resolve_toml_route_call(line)
}

fn resolve_dsl_route_call(line: &str) -> Option<String> {
    let cfg = config_dir().ok()?;
    let routes_at = cfg.join("routes.at");
    if !routes_at.exists() {
        return None;
    }
    let contents = std::fs::read_to_string(&routes_at).ok()?;
    let parsed = parse_file(&contents).ok()?;
    for r in &parsed.routes {
        if let Some(rest) = line.strip_prefix(&r.pattern) {
            let rest = rest.trim();
            let cmd = format!("/{}", r.flow.name);
            let call = if rest.is_empty() {
                cmd
            } else {
                format!("{cmd} {rest}")
            };
            return Some(call);
        }
    }
    if let Some(dr) = &parsed.default_route {
        let cmd = format!("/{}", dr.flow.name);
        let call = if line.trim().is_empty() {
            cmd
        } else {
            format!("{cmd} {}", line.trim())
        };
        return Some(call);
    }
    None
}

fn resolve_toml_route_call(line: &str) -> Option<String> {
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
    let name_full = parts.next().context("empty slash command")?;
    let name = name_full.strip_prefix('/').unwrap_or(name_full);
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
    let redactor = atman_daemon::bootstrap::build_redactor(config_dir().ok().as_deref());
    let session = Session::open_with_redactor(&root, redactor.clone())
        .with_context(|| format!("opening session under {}", root.display()))?;
    if let Some(path) = session.events_path() {
        println!("[atman] session={} events={}", session.id(), path.display());
    }

    let atman_daemon::bootstrap::BootstrapOutcome {
        mut executor,
        mcp_status,
    } = atman_daemon::bootstrap::build_executor(bootstrap_opts(session.sink().clone(), false)?)
        .await?;
    for outcome in &mcp_status {
        match outcome {
            Ok(s) => println!(
                "[atman] mcp `{}` connected via {} ({} tools)",
                s.name, s.transport, s.tool_count
            ),
            Err(e) => eprintln!("[atman] mcp boot: {e}"),
        }
    }
    attach_memory_stores(&mut executor, session.dir(), false)?;

    let lifecycles = match config_dir() {
        Ok(cfg) => atman_runtime::lifecycle::LifecycleRunner::from_dir(&cfg),
        Err(_) => atman_runtime::lifecycle::LifecycleRunner::new(),
    };
    lifecycles
        .fire(&executor, atman_dsl::ast::LifecycleEvent::SessionStart)
        .await;

    let classifier = build_interjection_classifier();

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
            let trimmed = rest.trim();
            if trimmed == "suggest" || trimmed.starts_with("suggest ") {
                if let Err(e) = handle_suggest(&executor, &session, &mut input_rx).await {
                    eprintln!("[atman] :suggest: {e}");
                }
                continue;
            }
            if !handle_builtin(trimmed, sid.as_str(), &mut pending) {
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
            &lifecycles,
            classifier.as_ref(),
            &text,
            &mut pending,
            kind,
            &mut input_rx,
            &mut pushback,
        )
        .await;
    }

    lifecycles
        .fire(&executor, atman_dsl::ast::LifecycleEvent::SessionEnd)
        .await;

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
    lifecycles: &atman_runtime::lifecycle::LifecycleRunner,
    classifier: Option<
        &std::sync::Arc<dyn atman_runtime::injection_classifier::InjectionClassifier>,
    >,
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
    lifecycles
        .fire(executor, atman_dsl::ast::LifecycleEvent::TurnStart)
        .await;

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
                if !consume_interjection_input(&line, session, classifier).await {
                    pushback.push_back(line);
                }
            }
        }
    };

    match result {
        Ok(v) => println!("{}", render_value(&v)),
        Err(e) => eprintln!("error: {e}"),
    }
    lifecycles
        .fire(executor, atman_dsl::ast::LifecycleEvent::TurnEnd)
        .await;
    session.end_turn();
}

/// Returns true if the line was fully consumed as an interjection (`!nudge` / `!course-correct` /
/// `!redirect` / `!stop`) or reported as a busy-warning, false if it should be pushed back for the
/// main loop (e.g. `:exit` or a normal command arriving before the current flow finishes).
async fn consume_interjection_input(
    line: &str,
    session: &Session,
    classifier: Option<
        &std::sync::Arc<dyn atman_runtime::injection_classifier::InjectionClassifier>,
    >,
) -> bool {
    use atman_runtime::injection::InjectionLevel;
    use atman_runtime::injection_classifier::{ClassifierSource, source_tag};
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
    if let Some(text) = trimmed.strip_prefix("!nudge ") {
        let text = text.trim();
        if text.is_empty() {
            eprintln!("[atman] usage: !nudge <text>");
            return true;
        }
        match session.enqueue_injection(text) {
            Ok(id) => println!("[atman] nudge queued ({id}) — will inject at next llm node"),
            Err(e) => eprintln!("[atman] nudge rejected: {e}"),
        }
        return true;
    }
    if let Some(text) = trimmed.strip_prefix('!') {
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
    let Some(classifier) = classifier else {
        return false;
    };
    let cls = classifier.classify(trimmed).await;
    let source = source_tag(cls.source);
    match cls.level {
        InjectionLevel::L4HardStop => {
            session.cancel_flow();
            let _ = session.enqueue_injection_with_level(
                trimmed,
                InjectionLevel::L4HardStop,
                cls.redirect_target,
            );
            println!("[atman] L4 stop queued ({source}): {trimmed}");
        }
        InjectionLevel::L3Redirect => {
            let target = cls.redirect_target.clone();
            match session.enqueue_injection_with_level(
                trimmed,
                InjectionLevel::L3Redirect,
                target.clone(),
            ) {
                Ok(id) => println!(
                    "[atman] L3 redirect queued ({id}, {source}) → {}",
                    target.as_deref().unwrap_or("<no target>")
                ),
                Err(e) => eprintln!("[atman] L3 redirect rejected: {e}"),
            }
        }
        InjectionLevel::L2CourseCorrect => {
            match session.enqueue_injection_with_level(
                trimmed,
                InjectionLevel::L2CourseCorrect,
                None,
            ) {
                Ok(id) => {
                    println!("[atman] L2 course-correct queued ({id}, {source}): {trimmed}")
                }
                Err(e) => eprintln!("[atman] L2 course-correct rejected: {e}"),
            }
        }
        InjectionLevel::L1Nudge => match session.enqueue_injection(trimmed) {
            Ok(id) => println!("[atman] L1 nudge queued ({id}, {source}): {trimmed}"),
            Err(e) => eprintln!("[atman] L1 nudge rejected: {e}"),
        },
    }
    let _ = ClassifierSource::Default;
    true
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
            println!(":suggest             — ask meta-LLM for a reusable flow from recent turns");
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

async fn handle_suggest(
    executor: &Executor,
    session: &Session,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
) -> Result<()> {
    let events = session
        .events_path()
        .context("session has no events path (dry-run?)")?;
    let transcript = suggest::read_recent_events(events, suggest::recent_turns_limit())?;
    if transcript.trim().is_empty() {
        println!("[atman] :suggest — no recent turns yet; talk a bit first.");
        return Ok(());
    }

    let model = load_suggest_model();
    let provider = executor
        .providers
        .resolve(&model)
        .with_context(|| format!("no provider resolves model `{model}` — configure one first"))?;

    println!("[atman] :suggest — asking `{model}` to spot a reusable pattern…");
    let reply = suggest::generate_suggestion(provider, &model, &transcript).await?;
    if reply.trim() == "NO_SUGGESTION" {
        println!("[atman] :suggest — model saw no reusable pattern.");
        return Ok(());
    }
    let Some(dsl_src) = suggest::extract_code_block(&reply) else {
        println!("[atman] :suggest — model reply did not contain a fenced code block:");
        println!("{}", reply.trim());
        return Ok(());
    };

    let flow_name = match suggest::extract_flow_name(&dsl_src) {
        Ok(n) => n,
        Err(e) => {
            println!("[atman] :suggest — suggested flow is not valid: {e}");
            println!("---\n{dsl_src}\n---");
            return Ok(());
        }
    };
    let parsed = parse_file(&dsl_src)?;
    if let Err(errs) = atman_runtime::validate::validate(&parsed.flows[0], &executor.tools) {
        println!("[atman] :suggest — validation rejected the suggestion:");
        for e in errs {
            println!("  · {e:?}");
        }
        println!("---\n{dsl_src}\n---");
        return Ok(());
    }

    let has_shell = dsl_src.contains("bash.exec") || dsl_src.contains("shell.");
    println!("[atman] suggested flow `{flow_name}`:");
    println!("---\n{dsl_src}\n---");
    if has_shell {
        println!("[atman] note: this flow calls shell tools — accept only if you trust it.");
    }
    println!(
        "[atman] accept? [y] yes / [n] no / [e] print path so you can edit the buffered draft"
    );

    let choice = loop {
        let Some(line) = input_rx.recv().await else {
            println!("[atman] :suggest — input closed, discarding.");
            return Ok(());
        };
        match line.trim() {
            "y" | "Y" | "yes" => break 'y',
            "n" | "N" | "no" | "" => break 'n',
            "e" | "E" | "edit" => break 'e',
            other => {
                println!("[atman] answer with y / n / e (got `{other}`)");
            }
        }
    };

    if choice == 'n' {
        println!("[atman] :suggest — discarded.");
        return Ok(());
    }

    let cfg = config_dir()?;
    let cmd_dir = cfg.join("commands");
    std::fs::create_dir_all(&cmd_dir)?;
    let mut final_name = flow_name.clone();
    let mut target = cmd_dir.join(format!("{final_name}.at"));
    if target.exists() {
        final_name = format!("{flow_name}_v2");
        target = cmd_dir.join(format!("{final_name}.at"));
        println!(
            "[atman] :suggest — `{flow_name}.at` exists; writing as `{final_name}.at` instead"
        );
    }
    let final_src = if final_name == flow_name {
        dsl_src.clone()
    } else {
        dsl_src.replacen(
            &format!("flow {flow_name}"),
            &format!("flow {final_name}"),
            1,
        )
    };
    std::fs::write(&target, format!("{final_src}\n"))
        .with_context(|| format!("write {}", target.display()))?;

    let routes_at = cfg.join("routes.at");
    let mut routes_body = std::fs::read_to_string(&routes_at).unwrap_or_default();
    if !routes_body.ends_with('\n') && !routes_body.is_empty() {
        routes_body.push('\n');
    }
    let trigger = format!("{final_name} ");
    routes_body.push_str(&suggest::route_line(&final_name, &trigger));
    std::fs::write(&routes_at, routes_body)
        .with_context(|| format!("append route to {}", routes_at.display()))?;

    println!(
        "[atman] :suggest — accepted. wrote {} and appended route \"{}\" → {}",
        target.display(),
        trigger,
        final_name
    );
    if choice == 'e' {
        println!("[atman] :suggest — open {} to edit.", target.display());
    }
    Ok(())
}

fn load_auto_snapshot() -> bool {
    if let Ok(v) = std::env::var("ATMAN_AUTO_SNAPSHOT")
        && matches!(v.trim(), "1" | "true" | "yes" | "on")
    {
        return true;
    }
    let Ok(cfg) = config_dir() else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(cfg.join("config.toml")) else {
        return false;
    };
    parse_auto_snapshot(&text).unwrap_or(false)
}

fn parse_auto_snapshot(text: &str) -> Option<bool> {
    let mut in_section = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[')
            && let Some(name) = rest.strip_suffix(']')
        {
            in_section = name.trim() == "registry";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == "auto_snapshot" {
            return Some(matches!(v.trim(), "true" | "1" | "\"true\"" | "yes"));
        }
    }
    None
}

fn auto_snapshot_flows(source_path: &Path, source: &str, parsed: &atman_dsl::ast::File) {
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry = match atman_runtime::flow_registry::FlowRegistry::open(&project_root) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[atman] auto_snapshot: open registry failed: {e}");
            return;
        }
    };
    let meta = match atman_runtime::flow_meta::FlowMeta::from_source(source_path, source) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[atman] auto_snapshot: read meta failed: {e}");
            return;
        }
    };
    for flow in &parsed.flows {
        let name = &flow.name.name;
        match registry.snapshot(name, source, &meta) {
            Ok(atman_runtime::flow_registry::SnapshotOutcome::Inserted(rev)) => eprintln!(
                "[atman] auto_snapshot: {name} @ {} (id={})",
                rev.version, rev.id
            ),
            Ok(atman_runtime::flow_registry::SnapshotOutcome::UnchangedFromLatest(_)) => {}
            Err(e) => eprintln!("[atman] auto_snapshot: {name}: {e}"),
        }
    }
}

fn load_suggest_model() -> String {
    let default = "gpt-4o-mini".to_string();
    let Ok(cfg) = config_dir() else {
        return default;
    };
    let Ok(text) = std::fs::read_to_string(cfg.join("config.toml")) else {
        return default;
    };
    parse_suggest_model(&text).unwrap_or(default)
}

fn parse_suggest_model(text: &str) -> Option<String> {
    let mut in_section = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[')
            && let Some(name) = rest.strip_suffix(']')
        {
            in_section = name.trim() == "suggest";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == "model" {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
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

const MONITOR_HTML: &str = r##"<!doctype html>
<html><head><meta charset="utf-8"><title>atman monitor</title>
<style>
body{font:14px/1.4 -apple-system,BlinkMacSystemFont,'Segoe UI',sans-serif;margin:0;padding:16px;background:#0e1116;color:#e6edf3}
h1{margin:0 0 16px;font-size:16px;color:#7ee787}
.row{display:flex;gap:16px}
.pane{flex:1;background:#151b23;border:1px solid #30363d;border-radius:6px;padding:12px;overflow:auto;max-height:80vh}
.sess{padding:6px 8px;border-radius:4px;cursor:pointer;font-family:monospace;font-size:12px;color:#7d8590}
.sess:hover{background:#1f2530}
.sess.active{background:#1f2f4a;color:#79c0ff}
pre{white-space:pre-wrap;word-break:break-all;margin:0;font-family:'SF Mono',Menlo,monospace;font-size:11px}
.event{padding:6px 8px;margin-bottom:4px;border-radius:4px;background:#1c2430;border-left:3px solid #30363d}
.event.flow_start{border-left-color:#7ee787}
.event.flow_end{border-left-color:#79c0ff}
.event.llm_call{border-left-color:#f0883e}
.event.user_msg{border-left-color:#d2a8ff}
.event.assistant_msg{border-left-color:#7ee787}
.event.error{border-left-color:#f85149}
.type{color:#79c0ff;font-weight:600}
.ts{color:#6e7681;font-size:10px}
.pill{display:inline-block;margin-left:8px;padding:2px 8px;border-radius:10px;font-size:11px;font-weight:600;vertical-align:middle}
.pill.hidden{display:none}
.pill.connecting{background:#5a4a1a;color:#f0c674}
.pill.connected{background:#1a4a2a;color:#7ee787}
.pill.disconnected{background:#4a1a1a;color:#f85149}
</style></head><body>
<h1>atman monitor · <span id="hint">select a session</span> <small id="mode" style="color:#6e7681;font-weight:400;font-size:12px"></small><span id="ssePill" class="pill hidden"></span></h1>
<div class="row">
  <div class="pane" style="flex:0 0 260px" id="sessions"><em>loading sessions…</em></div>
  <div class="pane" id="events"><em>← pick a session on the left</em></div>
</div>
<script>
const params = new URLSearchParams(location.search);
const daemonBase = params.get('daemon') || '';
const daemonToken = params.get('token') || '';
const useSse = daemonBase.length > 0;
document.getElementById('mode').textContent = useSse ? '· sse mode via ' + daemonBase : '· file-tail mode (poll 5s)';
let activeSse = null;

async function fetchJson(url){const r=await fetch(url);if(!r.ok)throw new Error(r.status);return r.json();}
function esc(s){return String(s).replace(/[&<>]/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;'}[c]));}
function eventBlock(e){return `<div class="event ${esc(e.type||'')}"><span class="type">${esc(e.type||'?')}</span> <span class="ts">${esc(e.ts||'')}</span><pre>${esc(JSON.stringify(e,null,2))}</pre></div>`;}
function setSseState(state){
  const pill=document.getElementById('ssePill');
  if(!state){pill.className='pill hidden';pill.textContent='';return;}
  const label={connecting:'SSE: connecting…',connected:'SSE: live',disconnected:'SSE: reconnecting…'}[state]||state;
  pill.className='pill '+state;pill.textContent=label;
}

async function loadSessions(){
  const list=await fetchJson('/api/sessions');
  const el=document.getElementById('sessions');
  if(!list.length){el.innerHTML='<em>no sessions</em>';return;}
  el.innerHTML=list.map(s=>`<div class="sess" data-id="${esc(s.id)}">${esc(s.id)}<br><span class="ts">${s.event_count} events · ${esc(s.first_ts||'?')}</span></div>`).join('');
  el.querySelectorAll('.sess').forEach(node=>node.onclick=()=>loadEvents(node.dataset.id));
}
async function loadEvents(sid){
  document.getElementById('hint').textContent=sid;
  document.querySelectorAll('.sess').forEach(n=>n.classList.toggle('active',n.dataset.id===sid));
  const box=document.getElementById('events');
  if(activeSse){activeSse.close();activeSse=null;}
  if(useSse){
    const url = daemonBase + '/events?session_id=' + encodeURIComponent(sid) + (daemonToken?'&token='+encodeURIComponent(daemonToken):'');
    box.innerHTML='<em>connecting sse…</em>';
    setSseState('connecting');
    const es = new EventSource(url);
    activeSse = es;
    let first = true;
    es.onopen = () => { setSseState('connected'); };
    es.addEventListener('event', ev => {
      try {
        const e = JSON.parse(ev.data);
        if(first){box.innerHTML='';first=false;}
        box.insertAdjacentHTML('beforeend', eventBlock(e));
        box.scrollTop = box.scrollHeight;
      }catch(_){}
    });
    es.onerror = () => { setSseState('disconnected'); };
  } else {
    setSseState(null);
    const ev = await fetchJson('/api/sessions/'+encodeURIComponent(sid)+'/events');
    if(!ev.length){box.innerHTML='<em>empty session</em>';return;}
    box.innerHTML=ev.map(eventBlock).join('');
  }
}
loadSessions();
setInterval(loadSessions,5000);
</script></body></html>
"##;

async fn cmd_monitor(port: u16) -> Result<()> {
    use axum::Router;
    use axum::extract::Path;
    use axum::response::{Html, IntoResponse, Json};
    use axum::routing::get;
    use std::net::SocketAddr;

    let data = data_dir()?;
    let sessions_dir = data.join("sessions");
    let state = Arc::new(sessions_dir);

    let app = Router::new()
        .route("/", get(|| async { Html(MONITOR_HTML) }))
        .route(
            "/api/sessions",
            get({
                let state = state.clone();
                move || {
                    let state = state.clone();
                    async move { Json(list_sessions_summary(&state).await) }
                }
            }),
        )
        .route(
            "/api/sessions/{sid}/events",
            get({
                let state = state.clone();
                move |Path(sid): Path<String>| {
                    let state = state.clone();
                    async move { Json(read_session_events(&state, &sid).await).into_response() }
                }
            }),
        )
        .with_state(());

    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    println!("[atman] monitor listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn list_sessions_summary(sessions_dir: &std::path::Path) -> Vec<serde_json::Value> {
    let Ok(entries) = std::fs::read_dir(sessions_dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, serde_json::Value)> = Vec::new();
    for entry in entries.flatten() {
        let id = entry.file_name().to_string_lossy().to_string();
        let events_path = entry.path().join("events.jsonl");
        let (count, first_ts) = summarize_events_file(&events_path);
        out.push((
            id.clone(),
            serde_json::json!({
                "id": id,
                "event_count": count,
                "first_ts": first_ts,
            }),
        ));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|(_, v)| v).collect()
}

fn summarize_events_file(path: &std::path::Path) -> (usize, Option<String>) {
    let Ok(contents) = std::fs::read_to_string(path) else {
        return (0, None);
    };
    let mut count = 0usize;
    let mut first_ts: Option<String> = None;
    for line in contents.lines() {
        if line.trim().is_empty() {
            continue;
        }
        count += 1;
        if first_ts.is_none()
            && let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
            && let Some(ts) = v.get("ts").and_then(|t| t.as_str())
        {
            first_ts = Some(ts.into());
        }
    }
    (count, first_ts)
}

async fn read_session_events(sessions_dir: &std::path::Path, sid: &str) -> Vec<serde_json::Value> {
    let path = sessions_dir.join(sid).join("events.jsonl");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .collect()
}

async fn cmd_rebuild_index() -> Result<()> {
    let data = data_dir()?;
    let sessions_root = data.join("sessions");
    if !sessions_root.exists() {
        println!("no sessions directory at {}", sessions_root.display());
        return Ok(());
    }
    let mut total_rebuilt = 0usize;
    let mut total_skipped = 0usize;
    let mut session_count = 0usize;
    for entry in std::fs::read_dir(&sessions_root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let jsonl = path.join("events.jsonl");
        if !jsonl.exists() {
            continue;
        }
        match atman_runtime::index::AnchorIndex::open_session(&path) {
            Ok(idx) => match idx.rebuild_events_from_jsonl(&jsonl) {
                Ok(stats) => {
                    println!(
                        "  session {}: rebuilt {} events (skipped {})",
                        path.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                        stats.rebuilt,
                        stats.skipped
                    );
                    total_rebuilt += stats.rebuilt;
                    total_skipped += stats.skipped;
                    session_count += 1;
                }
                Err(e) => eprintln!("  session {}: rebuild failed: {e}", path.display()),
            },
            Err(e) => eprintln!("  session {}: open failed: {e}", path.display()),
        }
    }
    println!(
        "rebuilt {total_rebuilt} events across {session_count} sessions (skipped {total_skipped})"
    );
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
            let source = match cfg.transport {
                atman_runtime::mcp::TransportKind::Stdio => {
                    format!("stdio: {} {}", cfg.command, cfg.args.join(" "))
                        .trim()
                        .to_string()
                }
                atman_runtime::mcp::TransportKind::Http => {
                    format!("http: {}", cfg.url.as_deref().unwrap_or("<missing url>"))
                }
            };
            match status {
                Ok(s) => println!("  [✓] {:<20} {} tools · {source}", cfg.name, s.tool_count),
                Err(e) => println!("  [✗] {:<20} {} · {}", cfg.name, e.error, source),
            }
        }
    }
    Ok(())
}

fn bootstrap_opts(
    events: atman_runtime::event::EventSink,
    mock: bool,
) -> Result<atman_daemon::bootstrap::BootstrapOptions> {
    let project_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let home_dir = std::env::var("HOME").ok().map(std::path::PathBuf::from);
    let config_dir = config_dir().ok();
    Ok(atman_daemon::bootstrap::BootstrapOptions {
        events,
        mock,
        config_dir,
        project_root,
        home_dir,
    })
}

fn attach_memory_stores(
    executor: &mut atman_runtime::Executor,
    session_dir: &std::path::Path,
    ephemeral: bool,
) -> Result<()> {
    // Ephemeral runs must not touch project on-disk state (confessions.jsonl / .atman/).
    // Route everything to XDG data ephemeral scratch so `atman run --ephemeral` never pollutes cwd.
    let (session_scope, confession_root, spec_root) = if ephemeral {
        let scratch = data_dir()?.join("ephemeral");
        std::fs::create_dir_all(&scratch).ok();
        (scratch.clone(), scratch.clone(), scratch)
    } else {
        let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let confession_root = project_root.join(".atman");
        std::fs::create_dir_all(&confession_root).ok();
        let spec_root = data_dir()?.join("specs");
        std::fs::create_dir_all(&spec_root).ok();
        (session_dir.to_path_buf(), confession_root, spec_root)
    };
    let redactor = atman_daemon::bootstrap::build_redactor(config_dir().ok().as_deref());
    atman_daemon::bootstrap::attach_memory_stores_with_redactor(
        executor,
        &session_scope,
        &confession_root,
        &spec_root,
        redactor,
    );
    Ok(())
}

fn load_preview_config() -> atman_runtime::tools::preview::PreviewConfig {
    atman_daemon::bootstrap::load_preview_config(config_dir().ok().as_deref())
}

fn build_interjection_classifier()
-> Option<std::sync::Arc<dyn atman_runtime::injection_classifier::InjectionClassifier>> {
    let cfg_dir = config_dir().ok()?;
    let text = std::fs::read_to_string(cfg_dir.join("config.toml")).ok()?;
    let mode = parse_interjection_mode(&text);
    match mode.as_deref() {
        Some("off") => None,
        Some("rule") | None => Some(std::sync::Arc::new(
            atman_runtime::injection_classifier::RuleClassifier::default(),
        )),
        Some("llm") => Some(std::sync::Arc::new(
            atman_runtime::injection_classifier::ComposedClassifier::new(
                atman_runtime::injection_classifier::RuleClassifier::default(),
            ),
        )),
        Some(other) => {
            eprintln!(
                "[atman] unknown [interjection] classifier = `{other}` — falling back to rule"
            );
            Some(std::sync::Arc::new(
                atman_runtime::injection_classifier::RuleClassifier::default(),
            ))
        }
    }
}

fn parse_interjection_mode(text: &str) -> Option<String> {
    let mut in_section = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[')
            && let Some(name) = rest.strip_suffix(']')
        {
            in_section = name.trim() == "interjection";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == "classifier" {
            return Some(v.trim().trim_matches('"').to_string());
        }
    }
    None
}

fn load_mcp_configs() -> Vec<atman_runtime::mcp::McpServerConfig> {
    atman_daemon::bootstrap::load_mcp_configs(config_dir().ok().as_deref())
}

async fn cmd_migrate(action: MigrateAction) -> Result<()> {
    match action {
        MigrateAction::List { from, storage } => {
            let source = build_migration_source(&from, storage)?;
            let sessions = source.discover_sessions()?;
            if sessions.is_empty() {
                println!(
                    "[atman] migrate --from {from}: no sessions found (storage empty or unreadable)"
                );
                return Ok(());
            }
            println!("[atman] {from} sessions (newest first):");
            for (i, s) in sessions.iter().enumerate() {
                let when = format!("ms={}", s.created_ms);
                println!("  {:>3}. {}  {}  {}", i + 1, s.id, when, s.title);
            }
            Ok(())
        }
        MigrateAction::Import {
            session_id,
            from,
            storage,
            out,
        } => {
            let source = build_migration_source(&from, storage)?;
            let messages = source.load_messages(&session_id)?;
            if messages.is_empty() {
                bail!("session {session_id} loaded 0 messages — nothing to import");
            }
            if let Some(parent) = out.parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("mkdir {}", parent.display()))?;
            }
            let mut lines = Vec::with_capacity(messages.len());
            for m in &messages {
                let record = serde_json::json!({
                    "role": m.role.as_str(),
                    "text": m.text,
                    "agent": m.agent,
                    "model": m.model,
                    "created_ms": m.created_ms,
                    "source": source.source_tag(),
                });
                lines.push(record.to_string());
            }
            let body = lines.join("\n") + "\n";
            std::fs::write(&out, body).with_context(|| format!("write {}", out.display()))?;
            println!(
                "[atman] migrate: wrote {} messages from {from}/{session_id} to {}",
                messages.len(),
                out.display()
            );
            Ok(())
        }
    }
}

fn build_migration_source(
    kind: &str,
    storage: Option<PathBuf>,
) -> Result<Box<dyn migrate_source::MigrationSource>> {
    match kind {
        "opencode" => {
            let root = match storage {
                Some(p) => p,
                None => migrate_source::OpencodeSource::default_root()?,
            };
            Ok(Box::new(migrate_source::OpencodeSource::new(root)))
        }
        other => bail!("unknown migration source `{other}` (want: opencode)"),
    }
}

async fn cmd_sync(action: SyncAction) -> Result<()> {
    let env = sync::SyncEnv::discover()?;
    match action {
        SyncAction::Init { url, branch } => {
            let report = sync::init(&env, &url, branch.as_deref())?;
            if report.already_initialised {
                println!(
                    "[atman] sync: {} was already a git repo — remote reset to {}, branch {}",
                    env.memory_root.display(),
                    report.remote_url,
                    report.branch
                );
            } else {
                println!(
                    "[atman] sync: initialised {} @ branch {} → {}",
                    env.memory_root.display(),
                    report.branch,
                    report.remote_url
                );
            }
            if report.wrote_gitignore {
                println!("[atman] sync: wrote .gitignore");
            }
            Ok(())
        }
        SyncAction::Push { message } => {
            let report = sync::push(&env, message.as_deref())?;
            if report.committed {
                println!("[atman] sync: committed local changes");
            } else {
                println!("[atman] sync: nothing to commit — pushing existing branch");
            }
            println!("[atman] sync: pushed {} to origin", report.branch);
            if !report.stderr_tail.is_empty() {
                println!("[atman] sync: {}", report.stderr_tail);
            }
            Ok(())
        }
        SyncAction::Pull => {
            let out = sync::pull(&env)?;
            print!("{out}");
            Ok(())
        }
        SyncAction::Status => {
            let report = sync::status(&env)?;
            if !report.initialised {
                println!(
                    "[atman] sync: {} is not a memory repo yet — run `atman sync init <url>`",
                    env.memory_root.display()
                );
                return Ok(());
            }
            if let Some(b) = &report.branch {
                println!("[atman] sync: branch {b}");
            }
            if report.porcelain.trim().is_empty() {
                println!("[atman] sync: working tree clean");
            } else {
                print!("{}", report.porcelain);
            }
            Ok(())
        }
    }
}

async fn cmd_flow(action: FlowAction) -> Result<()> {
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry = atman_runtime::flow_registry::FlowRegistry::open(&project_root)
        .with_context(|| format!("open flow registry under {}", project_root.display()))?;
    match action {
        FlowAction::Snapshot { path, author } => cmd_flow_snapshot(&registry, &path, author),
        FlowAction::Versions { flow_name } => cmd_flow_versions(&registry, &flow_name),
        FlowAction::Diff {
            flow_name,
            from,
            to,
        } => cmd_flow_diff(&registry, &flow_name, &from, &to),
        FlowAction::Rollback {
            flow_name,
            version,
            to,
            yes,
        } => cmd_flow_rollback(&registry, &flow_name, &version, &to, yes),
    }
}

fn cmd_flow_snapshot(
    registry: &atman_runtime::flow_registry::FlowRegistry,
    path: &Path,
    author_override: Option<String>,
) -> Result<()> {
    let content =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut meta = atman_runtime::flow_meta::FlowMeta::from_source(path, &content)?;
    if let Some(a) = author_override {
        meta.author = Some(a);
    }
    let name = flow_name_from_source_or_path(&content, path);
    let outcome = registry.snapshot(&name, &content, &meta)?;
    match outcome {
        atman_runtime::flow_registry::SnapshotOutcome::Inserted(rev) => println!(
            "[atman] snapshot ok: {} @ {} (id={}) — source={}",
            rev.flow_name, rev.version, rev.id, rev.source_tag
        ),
        atman_runtime::flow_registry::SnapshotOutcome::UnchangedFromLatest(rev) => println!(
            "[atman] snapshot skipped: {} unchanged since {} (id={})",
            rev.flow_name, rev.version, rev.id
        ),
    }
    println!("[atman] registry: {}", registry.path().display());
    Ok(())
}

fn cmd_flow_versions(
    registry: &atman_runtime::flow_registry::FlowRegistry,
    flow_name: &str,
) -> Result<()> {
    let versions = registry.list_versions(flow_name)?;
    if versions.is_empty() {
        println!("[atman] no revisions for `{flow_name}` (run `atman flow snapshot <path>` first)");
        return Ok(());
    }
    println!(
        "{:>4}  {:<20}  {:<10}  {:<25}  hash",
        "id", "version", "source", "timestamp"
    );
    for r in versions {
        println!(
            "{:>4}  {:<20}  {:<10}  {:<25}  {}",
            r.id,
            r.version,
            r.source_tag,
            r.ts.to_rfc3339(),
            r.content_hash
        );
    }
    Ok(())
}

fn cmd_flow_diff(
    registry: &atman_runtime::flow_registry::FlowRegistry,
    flow_name: &str,
    from: &str,
    to: &str,
) -> Result<()> {
    let from_rev = registry
        .find_by_version(flow_name, from)?
        .with_context(|| format!("no revision matches `{from}` for `{flow_name}`"))?;
    let to_rev = registry
        .find_by_version(flow_name, to)?
        .with_context(|| format!("no revision matches `{to}` for `{flow_name}`"))?;
    println!(
        "--- {flow_name} @ {} (id={})",
        from_rev.version, from_rev.id
    );
    println!("+++ {flow_name} @ {} (id={})", to_rev.version, to_rev.id);
    let diff = similar::TextDiff::from_lines(&from_rev.content, &to_rev.content);
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            similar::ChangeTag::Delete => "-",
            similar::ChangeTag::Insert => "+",
            similar::ChangeTag::Equal => " ",
        };
        print!("{sign}{change}");
    }
    Ok(())
}

fn cmd_flow_rollback(
    registry: &atman_runtime::flow_registry::FlowRegistry,
    flow_name: &str,
    version: &str,
    target: &Path,
    assume_yes: bool,
) -> Result<()> {
    let rev = registry
        .find_by_version(flow_name, version)?
        .with_context(|| format!("no revision matches `{version}` for `{flow_name}`"))?;
    if target.is_dir() {
        bail!(
            "--to {} is a directory (want a file path)",
            target.display()
        );
    }
    if let Some(git_root) = git_root_containing(target) {
        eprintln!(
            "[atman] note: {} lives inside git repo at {}. `git checkout <sha> -- {}` may be a safer rollback path.",
            target.display(),
            git_root.display(),
            target.display()
        );
        if !assume_yes {
            bail!(
                "rollback aborted — re-run with --yes to overwrite {} anyway",
                target.display()
            );
        }
    }
    if target.exists() && !assume_yes {
        eprintln!(
            "[atman] refusing to overwrite {} without --yes (would replace with {} @ {}, id={})",
            target.display(),
            flow_name,
            rev.version,
            rev.id
        );
        bail!("rollback aborted");
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    std::fs::write(target, &rev.content).with_context(|| format!("write {}", target.display()))?;
    println!(
        "[atman] rolled back {} to {} (id={}) at {}",
        flow_name,
        rev.version,
        rev.id,
        target.display()
    );
    Ok(())
}

fn git_root_containing(target: &Path) -> Option<PathBuf> {
    let probe_dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(probe_dir)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if line.is_empty() {
        None
    } else {
        Some(PathBuf::from(line))
    }
}

fn flow_name_from_source_or_path(source: &str, path: &Path) -> String {
    if let Ok(file) = atman_dsl::parse::parse_file(source)
        && let Some(first) = file.flows.first()
    {
        return first.name.name.clone();
    }
    path.file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
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
