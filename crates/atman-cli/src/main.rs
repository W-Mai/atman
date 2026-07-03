use anyhow::{Context, Result, bail};
use atman_dsl::parse::parse_file;
use atman_runtime::providers::mock::MockProvider;
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
    tools::register_tier_zero(&mut executor.tools);
    if mock {
        executor.providers.register(Arc::new(
            MockProvider::new("mock").with_fallback(Value::Str("[mock response]".into())),
        ));
    }

    let outcome = executor.run(&parsed, &flow_name, args).await;
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

async fn route_input(line: &str, executor: &Executor) -> RouteOutcome {
    let cfg = match config_dir() {
        Ok(c) => c,
        Err(_) => return RouteOutcome::Unmatched,
    };
    let routes_path = cfg.join("routes.toml");
    if !routes_path.exists() {
        return RouteOutcome::Unmatched;
    }
    let contents = match std::fs::read_to_string(&routes_path) {
        Ok(c) => c,
        Err(_) => return RouteOutcome::Unmatched,
    };
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
            return match run_slash_command(&call, executor).await {
                Ok(v) => RouteOutcome::Handled(v),
                Err(e) => RouteOutcome::HandledErr(e),
            };
        }
    }
    RouteOutcome::Unmatched
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

async fn run_slash_command(line: &str, executor: &Executor) -> Result<Value> {
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

    executor
        .run(&parsed, &flow_name, kv)
        .await
        .map_err(Into::into)
}

async fn cmd_repl() -> Result<()> {
    use rustyline::error::ReadlineError;
    use rustyline::{Config, DefaultEditor};

    let non_interactive = std::env::var("ATMAN_REPL_NON_INTERACTIVE").is_ok();
    println!(
        "atman v{} — type `:help` for commands, `:exit` to leave",
        env!("CARGO_PKG_VERSION")
    );

    let root = data_dir()?;
    let session = Session::open(&root)
        .with_context(|| format!("opening session under {}", root.display()))?;
    if let Some(path) = session.events_path() {
        println!("[atman] session={} events={}", session.id(), path.display());
    }

    let mut executor = Executor::with_events(session.sink().clone());
    tools::register_tier_zero(&mut executor.tools);

    if let Err(e) = run_boot_flow(&executor).await {
        eprintln!("[atman] boot flow error: {e}");
    }

    let config = Config::builder().auto_add_history(true).build();
    let mut editor: DefaultEditor = DefaultEditor::with_config(config)?;

    loop {
        let line: String = if non_interactive {
            let mut buf = String::new();
            match std::io::stdin().read_line(&mut buf) {
                Ok(0) => break,
                Ok(_) => buf.trim_end().to_string(),
                Err(_) => break,
            }
        } else {
            match editor.readline("atman> ") {
                Ok(l) => l,
                Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => break,
                Err(e) => return Err(e.into()),
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(':') {
            if !handle_builtin(rest.trim(), session.id().to_string().as_str(), &executor) {
                break;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix('/') {
            match run_slash_command(rest.trim(), &executor).await {
                Ok(v) => println!("{}", render_value(&v)),
                Err(e) => eprintln!("error: {e}"),
            }
            continue;
        }
        match route_input(line.trim(), &executor).await {
            RouteOutcome::Handled(v) => println!("{}", render_value(&v)),
            RouteOutcome::HandledErr(e) => eprintln!("error: {e}"),
            RouteOutcome::Unmatched => {
                println!(
                    "[atman] no route matched. add `\"prefix\" -> command` to ~/.config/atman/routes.toml, or use `/name args...`."
                );
            }
        }
    }

    session.shutdown().await;
    drop(executor);
    Ok(())
}

fn handle_builtin(cmd: &str, sid: &str, executor: &Executor) -> bool {
    if let Some(rest) = cmd.strip_prefix("attach") {
        let path = rest.trim();
        if path.is_empty() {
            eprintln!(":attach <path> — path required");
            return true;
        }
        let expanded = std::path::PathBuf::from(path);
        if !expanded.exists() {
            eprintln!(":attach: file not found: {}", expanded.display());
            return true;
        }
        let kind = classify_attachment(&expanded);
        executor.push_attachment(atman_runtime::provider::Attachment {
            kind,
            path: expanded.clone(),
            mime: None,
        });
        println!(
            "[atman] attached {} (pending count: {})",
            expanded.display(),
            executor.pending_attachment_count()
        );
        return true;
    }
    match cmd {
        "help" => {
            println!(":help          — show this");
            println!(":exit | :quit  — leave REPL");
            println!(":session       — print current session id");
            println!(":cost          — cost summary for current session");
            println!(":attach <path> — attach a file to the next LLM call");
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

fn classify_attachment(path: &std::path::Path) -> atman_runtime::provider::AttachmentKind {
    let ext = path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp" => {
            atman_runtime::provider::AttachmentKind::Image
        }
        _ => atman_runtime::provider::AttachmentKind::File,
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
    Ok(())
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
