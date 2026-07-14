use anyhow::{Context, Result, bail};
use atman_dsl::parse::parse_file;
use atman_runtime::{Executor, Session, Value};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::Arc;

mod init;
// meta_commands now in atman_runtime
mod migrate_source;
mod repl_completer;
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
    #[arg(long, value_name = "SID", global = true)]
    r#continue: Option<String>,
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
        #[arg(long, conflicts_with = "session_id")]
        all: bool,
    },
    Doctor {
        #[arg(long)]
        fix: bool,
    },
    Init {
        #[arg(long, value_name = "MODE")]
        sandbox: Option<String>,
    },
    RebuildIndex,
    TuiPreview {
        scene: Option<String>,
    },
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
        session_id: Option<String>,
        #[arg(long, default_value = "opencode")]
        from: String,
        #[arg(long)]
        storage: Option<PathBuf>,
        #[arg(long, conflicts_with = "into")]
        out: Option<PathBuf>,
        #[arg(long, conflicts_with = "out", value_parser = ["new"])]
        into: Option<String>,
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
        to: Option<PathBuf>,
        #[arg(long)]
        yes: bool,
    },
    Lint {
        path: PathBuf,
    },
    Test {
        path: PathBuf,
        #[arg(long)]
        bless: bool,
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
    Stream {
        session_id: Option<String>,
        #[arg(long, default_value_t = 65099)]
        port: u16,
        #[arg(long)]
        since_seq: Option<u64>,
    },
}

#[derive(Subcommand, Debug)]
enum SessionAction {
    List {
        #[arg(
            long,
            help = "Show sessions from every project (default: only current project)"
        )]
        all: bool,
        #[arg(long, help = "Filter by an explicit project root path")]
        project: Option<PathBuf>,
    },
    Show {
        session_id: String,
    },
    Search {
        query: String,
        #[arg(long, help = "Restrict search to a single session id")]
        session: Option<String>,
        #[arg(
            long,
            help = "Search sessions from every project (default: current project)"
        )]
        all: bool,
        #[arg(long, help = "Search sessions under an explicit project root")]
        project: Option<PathBuf>,
        #[arg(long, default_value_t = 20, help = "Maximum results returned")]
        limit: usize,
    },
    Gc,
    Sanitize {
        session_id: String,
        #[arg(long)]
        dry_run: bool,
    },
}

// Any failure is logged to stderr and startup continues — a broken
// migration must never block launching atman.
fn run_startup_config_migration() {
    let (Ok(cfg), Ok(data)) = (config_dir(), data_dir()) else {
        return;
    };
    match atman_runtime::config_migration::migrate_legacy_config_if_needed(&cfg, &data) {
        Ok(Some(rep)) => {
            eprintln!(
                "[atman] moved {} config item(s) from {} to {}",
                rep.moved.len(),
                rep.from.display(),
                rep.to.display(),
            );
            for name in &rep.moved {
                eprintln!("  moved: {name}");
            }
            for name in &rep.skipped_conflicts {
                eprintln!("  skipped (already at destination): {name}");
            }
        }
        Ok(None) => {}
        Err(e) => eprintln!("[atman] config migration skipped: {e:#}"),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    run_startup_config_migration();
    // Probe theme before raw mode so OSC 11 reply can't leak into KeyEvents.
    let _ = atman_tui::theme::theme();
    match cli.cmd {
        None => cmd_repl(cli.r#continue).await,
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
        Some(Cmd::Logs {
            action:
                LogsAction::Stream {
                    session_id,
                    port,
                    since_seq,
                },
        }) => cmd_logs_stream(session_id, port, since_seq).await,
        Some(Cmd::Session {
            action: SessionAction::List { all, project },
        }) => cmd_session_list(all, project).await,
        Some(Cmd::Session {
            action: SessionAction::Show { session_id },
        }) => cmd_session_show(session_id).await,
        Some(Cmd::Session {
            action:
                SessionAction::Search {
                    query,
                    session,
                    all,
                    project,
                    limit,
                },
        }) => cmd_session_search(query, session, all, project, limit).await,
        Some(Cmd::Session {
            action: SessionAction::Gc,
        }) => cmd_session_gc().await,
        Some(Cmd::Session {
            action:
                SessionAction::Sanitize {
                    session_id,
                    dry_run,
                },
        }) => cmd_session_sanitize(session_id, dry_run).await,
        Some(Cmd::Cost { session_id, all }) => cmd_cost(session_id, all).await,
        Some(Cmd::Doctor { fix }) => cmd_doctor(fix).await,
        Some(Cmd::Init { sandbox }) => cmd_init(sandbox).await,
        Some(Cmd::RebuildIndex) => cmd_rebuild_index().await,
        Some(Cmd::TuiPreview { scene }) => cmd_tui_preview(scene).await,
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
    stream_daemon_events(&client, &base, &cfg.auth_token, &sid, None, true).await?;
    Ok(())
}

async fn stream_daemon_events(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    sid: &str,
    since_seq: Option<u64>,
    stop_on_flow_end: bool,
) -> Result<()> {
    let mut url = format!("{base}/events?session_id={sid}");
    if let Some(seq) = since_seq {
        url.push_str(&format!("&since_seq={seq}"));
    }
    let sse = client
        .get(&url)
        .bearer_auth(token)
        .send()
        .await
        .with_context(|| format!("GET {url} (is atman-daemon running?)"))?;
    if !sse.status().is_success() {
        bail!("daemon SSE returned HTTP {}", sse.status());
    }
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
                if stop_on_flow_end && data.contains("\"flow_end\"") {
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
        let project_index = open_current_project_index()?;
        Session::open_with_context(&root, redactor.clone(), project_index)
            .with_context(|| format!("opening session under {}", root.display()))?
    };

    if let Some(path) = session.events_path() {
        eprintln!("[atman] session={} events={}", session.id(), path.display());
    }

    load_model_config_from_disk();

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
    attach_memory_stores(&mut executor, &session, ephemeral)?;
    executor.tool_ctx.prompt_resolver = Some(std::sync::Arc::new(
        atman_runtime::rendezvous::AutoResolveResolver {
            default: serde_json::json!({ "hunks": [] }),
        },
    ));

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
    session
        .approval()
        .set_auto_ceiling(atman_runtime::tool::ApprovalLevel::Dangerous);
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

async fn cmd_session_list(all: bool, project: Option<PathBuf>) -> Result<()> {
    let root = data_dir()?;
    let sessions = root.join("sessions");
    if !sessions.exists() {
        return Ok(());
    }
    let filter = resolve_session_list_filter(all, project.as_deref())?;
    let mut rows: Vec<(std::time::SystemTime, String, u64, usize, String)> = Vec::new();
    for entry in std::fs::read_dir(&sessions)? {
        let entry = entry?;
        if !entry.path().is_dir() {
            continue;
        }
        let sid = entry.file_name().to_string_lossy().to_string();
        let meta = atman_runtime::session_meta::SessionMeta::load(&entry.path());
        let fingerprint = meta.as_ref().and_then(|m| m.project_fingerprint.clone());
        if let SessionListFilter::Project(ref want) = filter {
            if fingerprint.as_deref() != Some(want.as_str()) {
                continue;
            }
        }
        let project_label = meta
            .as_ref()
            .and_then(|m| m.project_root.as_deref())
            .map(short_project_path)
            .unwrap_or_else(|| "-".into());
        let events_path = entry.path().join("events.jsonl");
        let (bytes, events) = match std::fs::metadata(&events_path) {
            Ok(m) => (m.len(), count_lines(&events_path)),
            Err(_) => (0, 0),
        };
        let modified = entry
            .metadata()
            .and_then(|m| m.modified())
            .unwrap_or(std::time::UNIX_EPOCH);
        rows.push((modified, sid, bytes, events, project_label));
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));
    match &filter {
        SessionListFilter::All => {}
        SessionListFilter::Project(fp) if rows.is_empty() => {
            println!(
                "no sessions match project fingerprint {fp}. Use --all to list every session."
            );
            return Ok(());
        }
        SessionListFilter::Project(_) => {}
    }
    let header_sid = "session_id";
    let header_events = "events";
    let header_bytes = "bytes";
    let header_project = "project";
    println!("{header_sid:<38} {header_events:>8} {header_bytes:>10} {header_project}");
    for (_, sid, bytes, events, project_label) in rows {
        println!("{sid:<38} {events:>8} {bytes:>10} {project_label}");
    }
    Ok(())
}

enum SessionListFilter {
    All,
    Project(String),
}

fn resolve_session_list_filter(all: bool, project: Option<&Path>) -> Result<SessionListFilter> {
    if all {
        return Ok(SessionListFilter::All);
    }
    if let Some(path) = project {
        if !path.exists() {
            bail!("--project path does not exist: {}", path.display());
        }
        return Ok(SessionListFilter::Project(
            atman_runtime::session_meta::fingerprint_from_root(path),
        ));
    }
    let cwd = std::env::current_dir().context("reading cwd")?;
    match atman_runtime::session_meta::find_project_root(&cwd) {
        Some(root) => Ok(SessionListFilter::Project(
            atman_runtime::session_meta::fingerprint_from_root(&root),
        )),
        None => bail!(
            "cwd is not inside any project (looked for .atman/ or .git/). Use --all or --project PATH."
        ),
    }
}

fn short_project_path(path: &Path) -> String {
    let s = path.display().to_string();
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy().to_string();
        if let Some(rest) = s.strip_prefix(&home) {
            return format!("~{rest}");
        }
    }
    s
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

async fn cmd_session_search(
    query: String,
    session: Option<String>,
    all: bool,
    project: Option<PathBuf>,
    limit: usize,
) -> Result<()> {
    if query.trim().is_empty() {
        bail!("empty search query");
    }
    if limit == 0 {
        bail!("--limit must be >= 1");
    }
    let root = data_dir()?;
    let sessions_root = root.join("sessions");
    if !sessions_root.exists() {
        return Ok(());
    }
    let filter = if session.is_some() {
        SessionListFilter::All
    } else {
        resolve_session_list_filter(all, project.as_deref())?
    };
    let mut project_dirs: Vec<PathBuf> = Vec::new();
    let mut session_filter: Option<String> = None;
    if let Some(sid) = session {
        let dir = sessions_root.join(&sid);
        if !dir.is_dir() {
            bail!("session not found: {}", dir.display());
        }
        let meta = atman_runtime::session_meta::SessionMeta::load(&dir);
        let project_root = meta.as_ref().and_then(|m| m.project_root.clone());
        let scope = match project_root {
            Some(pr) => atman_runtime::storage::resolve_project_scope_for(&pr)
                .with_context(|| format!("resolve scope for {}", pr.display()))?,
            None => atman_runtime::storage::resolve_current_project_scope()?,
        };
        project_dirs.push(scope);
        session_filter = Some(sid);
    } else {
        match &filter {
            SessionListFilter::Project(fp) => {
                let scope = root.join("projects").join(fp);
                if scope.is_dir() {
                    project_dirs.push(scope);
                }
            }
            SessionListFilter::All => {
                let projects_root = root.join("projects");
                if projects_root.is_dir() {
                    for entry in std::fs::read_dir(&projects_root)? {
                        let path = entry?.path();
                        if path.is_dir() {
                            project_dirs.push(path);
                        }
                    }
                } else {
                    project_dirs.push(atman_runtime::storage::resolve_current_project_scope()?);
                }
            }
        }
    }
    let mut hits: Vec<(String, String, u64, String, String)> = Vec::new();
    for scope in project_dirs {
        let idx = match atman_runtime::index::AnchorIndex::open_project(&scope) {
            Ok(i) => i,
            Err(_) => continue,
        };
        let rows = match idx.fts_search_project_events(&query, session_filter.as_deref(), limit) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for row in rows {
            let snippet = extract_snippet(&row.payload, &query);
            hits.push((row.ts, row.session_id, row.seq, row.kind, snippet));
        }
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0));
    hits.truncate(limit);
    if hits.is_empty() {
        println!("(no matches)");
        return Ok(());
    }
    let hdr_sid = "session";
    let hdr_seq = "seq";
    let hdr_kind = "kind";
    let hdr_ts = "ts";
    println!("{hdr_sid:<12} {hdr_seq:>6} {hdr_kind:<20} {hdr_ts:<20} snippet");
    for (ts, sid, seq, kind, snippet) in hits {
        let short_sid: String = sid.chars().take(8).collect();
        let short_ts = ts.chars().take(19).collect::<String>();
        let short_kind: String = kind.chars().take(20).collect();
        println!("{short_sid:<12} {seq:>6} {short_kind:<20} {short_ts:<20} {snippet}");
    }
    Ok(())
}

fn extract_snippet(payload: &str, query: &str) -> String {
    let needle_lower = query.trim_matches('"').to_lowercase();
    let payload_lower = payload.to_lowercase();
    let idx = payload_lower.find(&needle_lower).unwrap_or(0);
    let start = payload
        .char_indices()
        .rev()
        .find(|(i, _)| *i <= idx.saturating_sub(40))
        .map(|(i, _)| i)
        .unwrap_or(0);
    let end = (idx + 160).min(payload.len());
    let chunk: String = payload
        .chars()
        .skip(start)
        .take(200)
        .collect::<String>()
        .replace('\n', " ");
    let _ = end;
    chunk
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

async fn cmd_session_sanitize(sid: String, dry_run: bool) -> Result<()> {
    use atman_runtime::message::{ImageData, MessagePart};

    let root = data_dir()?;
    let dir = root.join("sessions").join(&sid);
    if !dir.is_dir() {
        bail!("session not found: {}", dir.display());
    }
    let events_path = dir.join("events.jsonl");
    if !events_path.exists() {
        println!("no events.jsonl in session");
        return Ok(());
    }

    let text = tokio::fs::read_to_string(&events_path).await?;
    let mut already_degraded: std::collections::HashSet<(u64, usize)> =
        std::collections::HashSet::new();
    let mut findings: Vec<(u64, usize, String, String)> = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["type"].as_str() == Some("attachment_degraded") {
            if let (Some(seq), Some(idx)) = (v["message_seq"].as_u64(), v["part_index"].as_u64()) {
                already_degraded.insert((seq, idx as usize));
            }
            continue;
        }
        if v["type"].as_str() != Some("user_msg") {
            continue;
        }
        let Some(seq) = v["seq"].as_u64() else {
            continue;
        };
        let Some(m) = v.get("message") else { continue };
        let Ok(msg) = serde_json::from_value::<atman_runtime::message::Message>(m.clone()) else {
            continue;
        };
        for (idx, part) in msg.parts.iter().enumerate() {
            let MessagePart::Image { source } = part else {
                continue;
            };
            if already_degraded.contains(&(seq, idx)) {
                continue;
            }
            match &source.data {
                ImageData::Path { path } => {
                    let (basename, reason) = match std::fs::metadata(path) {
                        Ok(meta) if meta.is_file() => {
                            let bn = path
                                .file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("unknown")
                                .to_string();
                            let ext = path
                                .extension()
                                .and_then(|s| s.to_str())
                                .map(str::to_ascii_lowercase)
                                .unwrap_or_default();
                            let mime = &source.media_type;
                            let mime_ok = match ext.as_str() {
                                "png" => mime == "image/png",
                                "jpg" | "jpeg" => mime == "image/jpeg",
                                "gif" => mime == "image/gif",
                                "webp" => mime == "image/webp",
                                _ => true,
                            };
                            if !mime_ok {
                                (bn, "sanitize:mime_mismatch".to_string())
                            } else {
                                continue;
                            }
                        }
                        Ok(_) => (
                            path.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("unknown")
                                .to_string(),
                            "sanitize:not_a_file".to_string(),
                        ),
                        Err(_) => (
                            path.file_name()
                                .and_then(|n| n.to_str())
                                .unwrap_or("unknown")
                                .to_string(),
                            "sanitize:file_not_found".to_string(),
                        ),
                    };
                    findings.push((seq, idx, basename, reason));
                }
                ImageData::Base64 { .. } => {}
            }
        }
    }

    if findings.is_empty() {
        println!("sanitize: no attachment problems found");
        return Ok(());
    }
    println!("sanitize: found {} attachment issue(s)", findings.len());
    for (seq, idx, basename, reason) in &findings {
        println!("  msg_seq={seq} part_index={idx} {basename} → {reason}");
    }
    if dry_run {
        println!("sanitize: dry-run, no events written");
        return Ok(());
    }

    let session = atman_runtime::Session::open_existing(&root, &sid)
        .with_context(|| format!("open session {sid}"))?;
    let session = std::sync::Arc::new(session);
    for (seq, idx, basename, reason) in &findings {
        session.emit_attachment_degrade(*seq, *idx, basename.clone(), reason.clone());
    }
    match std::sync::Arc::try_unwrap(session) {
        Ok(s) => s.shutdown().await,
        Err(_) => eprintln!("sanitize: session still had refs at shutdown"),
    }
    println!("sanitize: wrote {} degrade event(s)", findings.len());
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

async fn run_boot_flow(executor: &Executor, reporter: &Reporter) -> Result<()> {
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
        // Route through Reporter so the boot flow's greeting lands as a
        // TUI system note (inside the alternate screen) instead of a
        // raw println that would sit above the freshly-cleared frame.
        reporter.info(rendered);
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
    let trimmed_line = line.trim();
    let (name_full, rest_raw) = match trimmed_line.split_once(char::is_whitespace) {
        Some((n, r)) => (n, r.trim_start()),
        None => (trimmed_line, ""),
    };
    if name_full.is_empty() {
        bail!("empty slash command");
    }
    let name = name_full.strip_prefix('/').unwrap_or(name_full);
    let cfg = config_dir()?;
    let path = cfg.join("commands").join(format!("{name}.at"));
    if !path.exists() {
        bail!("no such command: {} (looked for {})", name, path.display());
    }
    let source =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let parsed = parse_file(&source).with_context(|| format!("parsing {}", path.display()))?;
    if parsed.flows.is_empty() {
        bail!("{} declares no flows", path.display());
    }
    let flow = parsed
        .flows
        .iter()
        .find(|f| f.name.name == name)
        .or_else(|| {
            if parsed.flows.len() == 1 {
                parsed.flows.first()
            } else {
                None
            }
        })
        .ok_or_else(|| {
            let names: Vec<&str> = parsed.flows.iter().map(|f| f.name.name.as_str()).collect();
            anyhow::anyhow!(
                "{} has {} flows but none is named `{name}` — declare a `flow {name}(...)` entry or invoke one of: {}",
                path.display(),
                parsed.flows.len(),
                names.join(", ")
            )
        })?;
    let flow_name = flow.name.name.clone();
    let params: Vec<String> = flow.params.iter().map(|(id, _)| id.name.clone()).collect();

    let mut kv: Vec<(String, Value)> = Vec::new();
    let tokens = split_quoted_args(rest_raw);

    let single_string_param = params.len() == 1
        && !rest_raw.is_empty()
        && !tokens
            .iter()
            .any(|t| t.contains('=') && !t.starts_with('='));
    if single_string_param {
        kv.push((params[0].clone(), Value::Str(rest_raw.to_string())));
        return Ok((parsed, flow_name, kv));
    }

    let mut positional_index = 0usize;
    for tok in tokens {
        if let Some((k, v)) = tok.split_once('=') {
            kv.push((k.to_string(), Value::Str(v.to_string())));
        } else if positional_index < params.len() {
            kv.push((
                params[positional_index].clone(),
                Value::Str(tok.to_string()),
            ));
            positional_index += 1;
        } else {
            kv.push((format!("_extra{positional_index}"), Value::Str(tok)));
            positional_index += 1;
        }
    }
    Ok((parsed, flow_name, kv))
}

fn split_quoted_args(input: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = input.chars().peekable();
    let mut in_single = false;
    let mut in_double = false;
    while let Some(c) = chars.next() {
        match c {
            '"' if !in_single => {
                in_double = !in_double;
            }
            '\'' if !in_double => {
                in_single = !in_single;
            }
            '\\' if in_double => {
                if let Some(&next) = chars.peek() {
                    cur.push(next);
                    chars.next();
                }
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() || in_single || in_double {
        out.push(cur);
    }
    out
}

#[derive(Default)]
struct PendingUserMessage {
    attachments: Vec<std::path::PathBuf>,
}

struct PrebuiltSession {
    session: std::sync::Arc<atman_runtime::Session>,
    executor: Executor,
    mcp_status: Vec<Result<atman_runtime::mcp::McpClientStatus, String>>,
    is_fresh: bool,
    root: PathBuf,
    intro: Option<atman_tui::app::StartupIntro>,
}

async fn prebuild_session(
    resume_sid: Option<String>,
    intro: Option<atman_tui::app::StartupIntro>,
    progress: Option<tokio::sync::mpsc::UnboundedSender<atman_tui::boot_animation::BootProgress>>,
) -> Result<PrebuiltSession> {
    use atman_runtime::workflow::NodeStatus;
    use atman_tui::boot_animation::{BootProgress, BootStepId};
    let emit = |step: BootStepId, start: bool, ok: bool| {
        if let Some(tx) = progress.as_ref() {
            let msg = if start {
                BootProgress::Start(step)
            } else {
                BootProgress::Finish(step, if ok { NodeStatus::Ok } else { NodeStatus::Err })
            };
            let _ = tx.send(msg);
        }
    };

    emit(BootStepId::OpenSession, true, false);
    let root = data_dir()?;
    let redactor = atman_daemon::bootstrap::build_redactor(config_dir().ok().as_deref());
    let is_fresh = resume_sid.is_none();
    let project_index = open_current_project_index()?;
    let session = std::sync::Arc::new(match resume_sid {
        Some(sid) => {
            let resolved_sid = resolve_session_prefix(&root, &sid)?;
            Session::open_existing_with_context(
                &root,
                &resolved_sid,
                redactor.clone(),
                project_index.clone(),
            )
            .with_context(|| format!("resuming session {resolved_sid} under {}", root.display()))?
        }
        None => Session::open_with_context(&root, redactor.clone(), project_index.clone())
            .with_context(|| format!("opening session under {}", root.display()))?,
    });
    apply_session_config(&session);
    emit(BootStepId::OpenSession, false, true);

    emit(BootStepId::BuildExecutor, true, false);
    let atman_daemon::bootstrap::BootstrapOutcome {
        mut executor,
        mcp_status,
    } = atman_daemon::bootstrap::build_executor(bootstrap_opts(session.sink().clone(), false)?)
        .await?;
    emit(BootStepId::BuildExecutor, false, true);

    emit(BootStepId::RegisterProviders, true, false);
    emit(BootStepId::RegisterProviders, false, true);

    emit(BootStepId::AttachMcp, true, false);
    let mcp_ok = mcp_status.iter().all(|r| r.is_ok());
    emit(BootStepId::AttachMcp, false, mcp_ok);

    emit(BootStepId::AttachMemory, true, false);
    attach_memory_stores(&mut executor, &session, false)?;
    emit(BootStepId::AttachMemory, false, true);

    emit(BootStepId::LoadTodos, true, false);
    session.refresh_todos_from_store_async().await;
    session.refresh_plans_from_store_async().await;
    emit(BootStepId::LoadTodos, false, true);

    emit(BootStepId::Ready, true, false);
    emit(BootStepId::Ready, false, true);

    Ok(PrebuiltSession {
        session,
        executor,
        mcp_status,
        is_fresh,
        root,
        intro,
    })
}

type PrebuildHandle = tokio::task::JoinHandle<Result<PrebuiltSession>>;

async fn boot_first_session(
    resume_sid: Option<String>,
) -> Result<(PrebuiltSession, Option<atman_tui::InheritedTerminal>)> {
    if !tui_mode_requested() {
        return Ok((prebuild_session(resume_sid, None, None).await?, None));
    }
    let version = env!("CARGO_PKG_VERSION").to_string();
    let recent = build_startup_recent(&data_dir()?, "", 5);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let prebuild = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("boot animation prebuild runtime init")?;
        rt.block_on(prebuild_session(resume_sid, None, Some(tx)))
    });
    let animation = atman_tui::boot_animation::run_boot_animation(rx, version, recent);
    let (anim_result, prebuild_result) = tokio::join!(animation, prebuild);
    let terminal = anim_result?;
    match prebuild_result {
        Ok(Ok(session)) => Ok((session, Some(terminal))),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(anyhow::anyhow!("prebuild join failed: {e}")),
    }
}

async fn cmd_repl(resume_sid: Option<String>) -> Result<()> {
    // Hold the terminal guard across every session switch so the
    // alternate screen stays alive between one cmd_repl_once and the
    // next. Without this, each SwitchSession would call
    // LeaveAlternateScreen and briefly show the user's shell.
    let _terminal_guard = if tui_mode_requested() {
        Some(atman_tui::terminal_guard::TerminalGuard::install()?)
    } else {
        None
    };
    let (first, boot_terminal) = boot_first_session(resume_sid).await?;
    let mut current = first;
    let mut inherited_terminal = boot_terminal;
    loop {
        let switch_target: std::sync::Arc<std::sync::Mutex<Option<PrebuildHandle>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        cmd_repl_once(current, switch_target.clone(), inherited_terminal.take()).await?;
        let next_handle = switch_target.lock().unwrap().take();
        match next_handle {
            Some(handle) => match handle.await {
                Ok(Ok(next)) => current = next,
                Ok(Err(e)) => return Err(e),
                Err(e) => return Err(anyhow::anyhow!("prebuild task join failed: {e}")),
            },
            None => {
                drop(_terminal_guard);
                flush_pending_summary();
                return Ok(());
            }
        }
    }
}

async fn cmd_repl_once(
    prebuilt: PrebuiltSession,
    switch_target: std::sync::Arc<std::sync::Mutex<Option<PrebuildHandle>>>,
    inherited_terminal: Option<atman_tui::InheritedTerminal>,
) -> Result<()> {
    use std::collections::VecDeque;
    use tokio::sync::mpsc;

    let use_tui = tui_mode_requested();
    let (note_tx, note_rx) = mpsc::unbounded_channel::<atman_tui::TuiNote>();
    let reporter = Reporter::new(use_tui, note_tx);

    let PrebuiltSession {
        session,
        mut executor,
        mcp_status,
        is_fresh: is_fresh_session,
        root,
        intro,
    } = prebuilt;
    for outcome in &mcp_status {
        match outcome {
            Ok(s) => reporter.info(format!(
                "[atman] mcp `{}` connected via {} ({} tools)",
                s.name, s.transport, s.tool_count
            )),
            Err(e) => reporter.error(format!("[atman] mcp boot: {e}")),
        }
    }

    let lifecycles = match config_dir() {
        Ok(cfg) => atman_runtime::lifecycle::LifecycleRunner::from_dir(&cfg),
        Err(_) => atman_runtime::lifecycle::LifecycleRunner::new(),
    };
    session.refresh_todos_from_store_async().await;
    session.refresh_plans_from_store_async().await;
    let (lifecycle_tx, mut lifecycle_rx) =
        mpsc::unbounded_channel::<atman_dsl::ast::LifecycleEvent>();
    executor.tool_ctx.lifecycle_fire_tx = Some(lifecycle_tx);
    if use_tui {
        let resolver = std::sync::Arc::new(atman_tui::prompt_resolver::TuiPromptResolver::new(
            session.forms(),
        ));
        executor.tool_ctx.prompt_resolver = Some(resolver);
    }
    lifecycles
        .fire(&executor, atman_dsl::ast::LifecycleEvent::SessionStart)
        .await;

    let classifier = build_interjection_classifier();

    if let Err(e) = run_boot_flow(&executor, &reporter).await {
        reporter.error(format!("[atman] boot flow error: {e}"));
    }

    let flow_names = discover_flow_names();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<String>();
    let (tui_task, tui_shutdown, ctrl_task, cmd_tx_for_repl) = if use_tui {
        let (sh_tx, sh_rx) = tokio::sync::oneshot::channel::<()>();
        let sh_tx_shared: std::sync::Arc<
            std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(Some(sh_tx)));
        let sh_tx_for_ctrl = sh_tx_shared.clone();
        session.flush_writer().await;
        let mut initial_items =
            atman_tui::history::flatten_transcript(&session.transcript_replay());
        if is_fresh_session {
            let recent = build_startup_recent(&root, &session.id().to_string(), 5);
            initial_items.insert(
                0,
                atman_tui::app::OutputItem::StartupCard {
                    version: env!("CARGO_PKG_VERSION").into(),
                    recent,
                },
            );
        }
        let (ctrl_tx, mut ctrl_rx) = mpsc::unbounded_channel::<atman_tui::TuiControl>();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<atman_tui::TuiCommand>();
        let session_for_ctrl = std::sync::Arc::clone(&session);
        let switch_target_for_ctrl = switch_target.clone();
        let providers_for_ctrl = executor.providers.clone();
        let data_root_for_ctrl = root.clone();
        let ctrl_task = tokio::spawn(async move {
            while let Some(msg) = ctrl_rx.recv().await {
                match msg {
                    atman_tui::TuiControl::CancelFlow => session_for_ctrl.cancel_flow(),
                    atman_tui::TuiControl::ApproveTool(id) => {
                        session_for_ctrl
                            .approval()
                            .decide(&id, atman_runtime::session::ApprovalDecision::Approve);
                    }
                    atman_tui::TuiControl::DenyTool {
                        tool_use_id,
                        reason,
                    } => {
                        session_for_ctrl.approval().decide(
                            &tool_use_id,
                            atman_runtime::session::ApprovalDecision::Deny { reason },
                        );
                    }
                    atman_tui::TuiControl::ApproveAllPending => {
                        session_for_ctrl
                            .approval()
                            .decide_all(atman_runtime::session::ApprovalDecision::Approve);
                    }
                    atman_tui::TuiControl::DenyAllPending { reason } => {
                        session_for_ctrl
                            .approval()
                            .decide_all(atman_runtime::session::ApprovalDecision::Deny { reason });
                    }
                    atman_tui::TuiControl::CompactNow => {
                        session_for_ctrl.request_manual_compact();
                        let session_for_compact = std::sync::Arc::clone(&session_for_ctrl);
                        let providers_for_compact = providers_for_ctrl.clone();
                        tokio::task::spawn_blocking(move || {
                            let rt = match tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                            {
                                Ok(rt) => rt,
                                Err(e) => {
                                    eprintln!("[atman] compact runtime init failed: {e}");
                                    return;
                                }
                            };
                            rt.block_on(async {
                                let model = session_for_compact.last_model();
                                atman_runtime::compaction::maybe_auto_compact(
                                    &session_for_compact,
                                    &model,
                                    &providers_for_compact,
                                )
                                .await;
                            });
                        });
                    }
                    atman_tui::TuiControl::CompactReviewAccept { review_id, edited } => {
                        let decision = match edited {
                            Some(summary) => {
                                atman_runtime::CompactReviewDecision::AcceptEdited { summary }
                            }
                            None => atman_runtime::CompactReviewDecision::AcceptAsIs,
                        };
                        session_for_ctrl
                            .compact_reviews()
                            .decide(&review_id, decision);
                    }
                    atman_tui::TuiControl::CompactReviewReject { review_id } => {
                        session_for_ctrl
                            .compact_reviews()
                            .decide(&review_id, atman_runtime::CompactReviewDecision::Reject);
                    }
                    atman_tui::TuiControl::SwitchSession { sid, intro } => {
                        // spawn_blocking + fresh current_thread runtime because MCP registration
                        // futures aren't Send, so plain tokio::spawn can't take them.
                        let handle = tokio::task::spawn_blocking(move || {
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .context("prebuild runtime init")?;
                            rt.block_on(prebuild_session(Some(sid), intro, None))
                        });
                        *switch_target_for_ctrl.lock().unwrap() = Some(handle);
                        session_for_ctrl.cancel_flow();
                        if let Some(tx) = sh_tx_for_ctrl.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        break;
                    }
                    atman_tui::TuiControl::DeleteSession(sid) => {
                        delete_session_dir(&data_root_for_ctrl, &sid);
                        if let Some(idx) = session_for_ctrl.project_index() {
                            let _ = idx.delete_events_for_session(&sid);
                        }
                    }
                    atman_tui::TuiControl::RenameSession { session_id, title } => {
                        let dir = data_root_for_ctrl.join("sessions").join(&session_id);
                        if let Err(e) =
                            atman_runtime::session_meta::SessionMeta::set_title(&dir, title)
                        {
                            eprintln!("[atman] rename {session_id} failed: {e}");
                        }
                    }
                    atman_tui::TuiControl::FormSubmit { form_id, answer } => {
                        let _ = session_for_ctrl.forms().submit(&form_id, answer);
                    }
                }
            }
        });
        let handle = atman_tui::TuiHandle {
            session_id: session.id().to_string(),
            session_dir: session.dir().to_string_lossy().to_string(),
            goal: session.goal(),
            stream_rx: session.stream_subscribe(),
            submit_tx: Some(input_tx),
            note_rx: Some(note_rx),
            shutdown_rx: Some(sh_rx),
            control_tx: Some(ctrl_tx),
            cmd_rx: Some(cmd_rx),
            initial_items,
            goal_rx: Some(session.subscribe_goal()),
            context_rx: Some(session.subscribe_context()),
            attach_rx: Some(session.subscribe_attach()),
            todos_rx: Some(session.subscribe_todos()),
            plans_rx: Some(session.subscribe_plans()),
            approvals_rx: Some(session.subscribe_pending_approvals()),
            compact_review_rx: Some(session.compact_reviews().subscribe()),
            form_rx: Some(session.forms().subscribe()),
            flow_names: flow_names.clone(),
            session: Some(std::sync::Arc::clone(&session)),
            startup_intro: intro.clone(),
            trust: atman_daemon::bootstrap::load_trust_config(config_dir().ok().as_deref()),
        };
        (
            Some(tokio::spawn(atman_tui::run_tui_ex(
                handle,
                inherited_terminal,
            ))),
            Some(sh_tx_shared),
            Some(ctrl_task),
            Some(cmd_tx),
        )
    } else {
        drop(note_rx);
        let (printer_tx, printer_rx) = tokio::sync::oneshot::channel::<Option<ExternalPrinter>>();
        spawn_stdin_reader(input_tx, printer_tx);
        let printer = printer_rx.await.unwrap_or(None);
        spawn_stream_consumer(&session, printer).await;
        (None, None, None, None)
    };
    let mut pending = PendingUserMessage::default();
    let mut pushback: VecDeque<String> = VecDeque::new();
    let sid = session.id().to_string();

    loop {
        let line = if let Some(l) = pushback.pop_front() {
            l
        } else {
            tokio::select! {
                l = input_rx.recv() => match l {
                    Some(l) => l,
                    None => break,
                },
                Some(ev) = lifecycle_rx.recv() => {
                    lifecycles.fire(&executor, ev).await;
                    continue;
                }
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix(':') {
            let trimmed = rest.trim();
            let Some(mc) = atman_runtime::meta_commands::match_command(trimmed) else {
                reporter.error(format!("unknown `:{trimmed}` — try `:help`"));
                continue;
            };
            match mc.name {
                "mode" => {
                    if let Some(tx) = cmd_tx_for_repl.as_ref() {
                        let _ = tx.send(atman_tui::TuiCommand::OpenTrustModePicker);
                    } else {
                        reporter.info("[atman] :mode — switch trust level (available in TUI mode)");
                    }
                }
                "mode-theme" => {
                    if let Some(tx) = cmd_tx_for_repl.as_ref() {
                        let _ = tx.send(atman_tui::TuiCommand::OpenThemePicker);
                    } else {
                        reporter.info(
                            "[atman] :mode-theme — switch display theme (available in TUI mode)",
                        );
                    }
                }
                "outside" => {
                    if let Some(tx) = cmd_tx_for_repl.as_ref() {
                        let _ = tx.send(atman_tui::TuiCommand::CycleOutside);
                    } else {
                        reporter.info(
                            "[atman] :outside — cycle outside behavior (available in TUI mode)",
                        );
                    }
                }
                "suggest" => {
                    if let Err(e) =
                        handle_suggest(&executor, &session, &mut input_rx, &reporter).await
                    {
                        reporter.error(format!("[atman] :suggest: {e}"));
                    }
                }
                "goal" => {
                    handle_goal_builtin(trimmed, &session, &reporter);
                }
                "sessions" => {
                    if let Some(tx) = cmd_tx_for_repl.as_ref() {
                        let _ = tx.send(atman_tui::TuiCommand::OpenSessionSwitcher);
                    } else {
                        match list_recent_sessions(&data_dir()?, 20) {
                            Ok(rows) => print_sessions_table(&rows, &reporter),
                            Err(e) => reporter.error(format!("[atman] :sessions: {e}")),
                        }
                    }
                }
                "sidebar" => {
                    let arg = trimmed.strip_prefix("sidebar").unwrap_or("").trim();
                    handle_sidebar_builtin(arg, cmd_tx_for_repl.as_ref(), &reporter);
                }
                "todo" => {
                    let arg = trimmed.strip_prefix("todo").unwrap_or("").trim();
                    handle_todo_builtin(arg, &session, &reporter).await;
                }
                "rename" => {
                    let arg = trimmed.strip_prefix("rename").unwrap_or("").trim();
                    handle_rename_builtin(arg, &session, &reporter);
                }
                "help" => {
                    for line in atman_runtime::meta_commands::help_lines() {
                        reporter.info(line);
                    }
                }
                "exit" => break,
                "session" => {
                    reporter.info(format!("session_id: {sid}"));
                }
                "cost" => {
                    reporter.error(format!(
                        "(hint) run `atman cost {sid}` in another shell for now"
                    ));
                }
                "compact" => {
                    handle_compact_builtin(&session, &reporter);
                }
                "attach" => {
                    let arg = trimmed.strip_prefix("attach").unwrap_or("").trim();
                    handle_attach_builtin(arg, &mut pending, &session, &reporter);
                }
                "copy" => {
                    let arg = trimmed.strip_prefix("copy").unwrap_or("").trim();
                    handle_copy_builtin(arg, &session, &reporter);
                }
                _ => {}
            }
            continue;
        }
        let (text, kind) = if let Some(rest) = line.strip_prefix('/') {
            (rest.trim().to_string(), TurnKind::Slash)
        } else {
            let trimmed = line.trim();
            if resolve_route_call(trimmed).is_none() {
                reporter.info(
                    "[atman] no route matched. add `\"prefix\" -> command` to ~/.config/atman/routes.toml, or use `/name args...`.",
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
            &reporter,
        )
        .await;
    }

    lifecycles
        .fire(&executor, atman_dsl::ast::LifecycleEvent::SessionEnd)
        .await;

    drop(executor);
    if let Some(sh) = tui_shutdown
        && let Some(tx) = sh.lock().unwrap().take()
    {
        let _ = tx.send(());
    }
    if let Some(handle) = tui_task {
        match handle.await {
            Ok(Ok(())) | Err(_) => {}
            Ok(Err(e)) => eprintln!("[atman] tui exited with error: {e}"),
        }
    }
    if let Some(ct) = ctrl_task {
        let _ = ct.await;
    }
    let user_msg_count = session.user_message_count();
    let goal = session.goal();
    let todos: Vec<atman_runtime::memory::todo::Todo> = {
        let store = atman_runtime::memory::todo::TodoStore::at(session.dir());
        store.list().await.unwrap_or_default()
    };
    let plans: Vec<atman_runtime::memory::plan::Plan> = {
        let store = atman_runtime::memory::plan::PlanStore::at(session.dir());
        store.list().await.unwrap_or_default()
    };
    let session_id = session.id().to_string();
    let session_dir = session.dir().to_path_buf();
    match std::sync::Arc::try_unwrap(session) {
        Ok(s) => s.shutdown().await,
        Err(_) => eprintln!("[atman] session still had refs at shutdown; skipping graceful close"),
    }
    if is_fresh_session
        && user_msg_count == 0
        && !session_dir.as_os_str().is_empty()
        && session_dir_is_disposable(&session_dir)
    {
        let _ = std::fs::remove_dir_all(&session_dir);
    }
    SUMMARY_PENDING.with(|cell| {
        *cell.borrow_mut() = Some(SessionSummary {
            sid: session_id,
            msg_count: user_msg_count,
            goal,
            todos,
            plans,
        });
    });
    Ok(())
}

thread_local! {
    static SUMMARY_PENDING: std::cell::RefCell<Option<SessionSummary>> =
        const { std::cell::RefCell::new(None) };
}

struct SessionSummary {
    sid: String,
    msg_count: usize,
    goal: Option<String>,
    todos: Vec<atman_runtime::memory::todo::Todo>,
    plans: Vec<atman_runtime::memory::plan::Plan>,
}

pub fn flush_pending_summary() {
    SUMMARY_PENDING.with(|cell| {
        if let Some(s) = cell.borrow_mut().take() {
            print_session_summary(&s.sid, s.msg_count, s.goal.as_deref(), &s.todos, &s.plans);
        }
    });
}

fn print_session_summary(
    sid: &str,
    msg_count: usize,
    goal: Option<&str>,
    todos: &[atman_runtime::memory::todo::Todo],
    plans: &[atman_runtime::memory::plan::Plan],
) {
    let sid_short = sid;
    let goal_line = goal.unwrap_or("(none)");
    let pending = todos
        .iter()
        .filter(|t| matches!(t.status, atman_runtime::memory::todo::TodoStatus::Pending))
        .count();
    let done = todos
        .iter()
        .filter(|t| matches!(t.status, atman_runtime::memory::todo::TodoStatus::Done))
        .count();
    let plan_line = plans
        .iter()
        .max_by_key(|p| p.updated_at)
        .map(|p| {
            let (step_done, step_total) = p.progress();
            format!("{} ({step_done}/{step_total})", truncate_str(&p.title, 40))
        })
        .unwrap_or_else(|| "(none)".to_string());

    let lines = vec![
        format!(" session   {sid_short}"),
        format!(" messages  {msg_count}"),
        format!(" goal      {}", truncate_str(goal_line, 50)),
        format!(" plan      {plan_line}"),
        format!(" todos     {done} done · {pending} pending"),
        String::new(),
        format!(" resume    atman --continue {sid_short}"),
    ];

    let max_w = lines
        .iter()
        .map(|l| unicode_width::UnicodeWidthStr::width(l.as_str()))
        .max()
        .unwrap_or(0)
        .max(40);
    let inner_w = max_w + 4;
    let top = format!("╭{}╮", "─".repeat(inner_w));
    let bot = format!("╰{}╯", "─".repeat(inner_w));
    let pad = |l: &str| {
        let visible = unicode_width::UnicodeWidthStr::width(l);
        let trail = max_w.saturating_sub(visible);
        format!("│  {}{}  │", l, " ".repeat(trail))
    };

    println!();
    println!("{}", top);
    for l in &lines {
        println!("{}", pad(l));
    }
    println!("{}", bot);
    println!();
}

fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max).collect();
        out.push('…');
        out
    }
}

fn delete_session_dir(data_root: &std::path::Path, sid: &str) {
    let dir = data_root.join("sessions").join(sid);
    if !dir.exists() {
        return;
    }
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        eprintln!("[atman] failed to delete session {}: {e}", dir.display());
    }
}

fn session_dir_is_disposable(dir: &std::path::Path) -> bool {
    const SIDE_EFFECT_FILES: &[&str] = &[
        "todos.jsonl",
        "plans.jsonl",
        "goal.txt",
        "confessions.jsonl",
    ];
    !SIDE_EFFECT_FILES.iter().any(|name| dir.join(name).exists())
}

fn tui_mode_requested() -> bool {
    if let Ok(v) = std::env::var("ATMAN_NO_TUI") {
        if matches!(v.as_str(), "1" | "true" | "yes" | "on") {
            return false;
        }
    }
    if let Ok(v) = std::env::var("ATMAN_TUI") {
        if matches!(v.as_str(), "0" | "false" | "no" | "off") {
            return false;
        }
    }
    std::env::var("ATMAN_REPL_NON_INTERACTIVE").is_err()
}

#[derive(Clone)]
enum Reporter {
    Stdout,
    Tui(tokio::sync::mpsc::UnboundedSender<atman_tui::TuiNote>),
}

impl Reporter {
    fn new(tui: bool, tx: tokio::sync::mpsc::UnboundedSender<atman_tui::TuiNote>) -> Self {
        if tui { Self::Tui(tx) } else { Self::Stdout }
    }

    fn is_tui(&self) -> bool {
        matches!(self, Self::Tui(_))
    }

    fn info(&self, text: impl Into<String>) {
        let text = text.into();
        match self {
            Self::Stdout => println!("{text}"),
            Self::Tui(tx) => {
                let _ = tx.send(atman_tui::TuiNote::Info(strip_atman_tag(&text).to_string()));
            }
        }
    }

    fn error(&self, text: impl Into<String>) {
        let text = text.into();
        match self {
            Self::Stdout => eprintln!("{text}"),
            Self::Tui(tx) => {
                let _ = tx.send(atman_tui::TuiNote::Error(
                    strip_atman_tag(&text).to_string(),
                ));
            }
        }
    }
}

fn strip_atman_tag(s: &str) -> &str {
    s.strip_prefix("[atman] ").unwrap_or(s)
}

type ExternalPrinter = Box<dyn rustyline::ExternalPrinter + Send>;

fn spawn_stdin_reader(
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    printer_tx: tokio::sync::oneshot::Sender<Option<ExternalPrinter>>,
) {
    let non_interactive = std::env::var("ATMAN_REPL_NON_INTERACTIVE").is_ok();
    tokio::task::spawn_blocking(move || {
        if non_interactive {
            let _ = printer_tx.send(None);
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
            use rustyline::history::DefaultHistory;
            use rustyline::{Config, Editor};
            let config = Config::builder().auto_add_history(true).build();
            let completer = repl_completer::AtmanCompleter::new(config_dir().ok());
            let mut editor: Editor<repl_completer::AtmanCompleter, DefaultHistory> =
                match Editor::with_config(config) {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("[atman] rustyline init failed: {e}");
                        let _ = printer_tx.send(None);
                        return;
                    }
                };
            editor.set_helper(Some(completer));
            let printer: Option<ExternalPrinter> = match editor.create_external_printer() {
                Ok(p) => Some(Box::new(p)),
                Err(e) => {
                    eprintln!("[atman] external printer unavailable: {e}");
                    None
                }
            };
            let _ = printer_tx.send(printer);
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

async fn spawn_stream_consumer(session: &atman_runtime::Session, printer: Option<ExternalPrinter>) {
    let mut rx = session.stream_subscribe();
    tokio::spawn(async move {
        let mut printer = printer;
        let mut pending_line = String::new();
        loop {
            match rx.recv().await {
                Ok(frame) => {
                    render_stream_frame(&mut printer, &mut pending_line, frame);
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    render_note(&mut printer, format!("(dropped {n} stream frames)"));
                }
                Err(_) => break,
            }
        }
    });
}

fn render_stream_frame(
    printer: &mut Option<ExternalPrinter>,
    pending: &mut String,
    frame: atman_runtime::stream::StreamFrame,
) {
    use atman_runtime::stream::StreamFrame;
    match frame {
        StreamFrame::LlmChunk { text, .. } => {
            pending.push_str(&text);
            emit(printer, text);
        }
        StreamFrame::LlmDone { .. } => {
            if !pending.ends_with('\n') {
                emit(printer, "\n".into());
            }
            pending.clear();
        }
        StreamFrame::ToolUseStart {
            tool, args_preview, ..
        } => {
            emit(printer, format!("  ⟶ {tool}({args_preview})\n"));
        }
        StreamFrame::ToolUseDone {
            tool, ok, preview, ..
        } => {
            let mark = if ok { '✓' } else { '✗' };
            emit(printer, format!("  {mark} {tool} → {preview}\n"));
        }
        StreamFrame::Note(s) => render_note(printer, s),
        StreamFrame::FlowGraph { .. }
        | StreamFrame::FlowStart { .. }
        | StreamFrame::FlowNodeStart { .. }
        | StreamFrame::FlowNodeEnd { .. }
        | StreamFrame::FlowDone { .. }
        | StreamFrame::ToolNode { .. }
        | StreamFrame::ThinkingChunk { .. }
        | StreamFrame::LlmCallStats { .. }
        | StreamFrame::AssistantMsg { .. }
        | StreamFrame::ToolResultMsg { .. }
        | StreamFrame::ToolPendingApproval { .. }
        | StreamFrame::ToolApproved { .. }
        | StreamFrame::ToolDenied { .. }
        | StreamFrame::Unknown => {}
    }
}

fn render_note(printer: &mut Option<ExternalPrinter>, s: String) {
    emit(printer, format!("[atman] {s}\n"));
}

fn emit(printer: &mut Option<ExternalPrinter>, s: String) {
    match printer.as_mut() {
        Some(p) => {
            let _ = p.print(s);
        }
        None => {
            print!("{s}");
            use std::io::Write;
            let _ = std::io::stdout().flush();
        }
    }
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
    reporter: &Reporter,
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
                if !consume_interjection_input(&line, session, classifier, reporter).await {
                    pushback.push_back(line);
                }
            }
        }
    };

    let streamed = session.take_streamed_flag();
    match result {
        Ok(v) => {
            if !(reporter.is_tui() && streamed) {
                let rendered = render_value(&v);
                if !rendered.is_empty() {
                    reporter.info(rendered);
                }
            }
        }
        Err(e) => reporter.error(format!("error: {e}")),
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
    reporter: &Reporter,
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
        reporter.info("[atman] stop requested; flow will abort at next node boundary");
        return true;
    }
    if let Some(text) = trimmed.strip_prefix("!course-correct ") {
        let text = text.trim();
        if text.is_empty() {
            reporter.error("[atman] usage: !course-correct <text>");
            return true;
        }
        match session.enqueue_injection_with_level(text, InjectionLevel::L2CourseCorrect, None) {
            Ok(id) => reporter.info(format!(
                "[atman] course-correct queued ({id}) — llm restarts at next chunk boundary"
            )),
            Err(e) => reporter.error(format!("[atman] course-correct rejected: {e}")),
        }
        return true;
    }
    if let Some(target) = trimmed.strip_prefix("!redirect ") {
        let target = target.trim();
        if target.is_empty() {
            reporter.error("[atman] usage: !redirect <flow_name>");
            return true;
        }
        match session.enqueue_injection_with_level(
            target,
            InjectionLevel::L3Redirect,
            Some(target.to_string()),
        ) {
            Ok(id) => reporter.info(format!("[atman] redirect queued ({id}) → {target}")),
            Err(e) => reporter.error(format!("[atman] redirect rejected: {e}")),
        }
        return true;
    }
    if let Some(text) = trimmed.strip_prefix("!nudge ") {
        let text = text.trim();
        if text.is_empty() {
            reporter.error("[atman] usage: !nudge <text>");
            return true;
        }
        match session.enqueue_injection(text) {
            Ok(id) => reporter.info(format!(
                "[atman] nudge queued ({id}) — will inject at next llm node"
            )),
            Err(e) => reporter.error(format!("[atman] nudge rejected: {e}")),
        }
        return true;
    }
    if let Some(text) = trimmed.strip_prefix('!') {
        let text = text.trim();
        if text.is_empty() {
            reporter.error(
                "[atman] usage while flow runs: !nudge <text> | !course-correct <text> | !redirect <flow> | !stop",
            );
            return true;
        }
        match session.enqueue_injection(text) {
            Ok(id) => reporter.info(format!(
                "[atman] nudge queued ({id}) — will inject at next llm node"
            )),
            Err(e) => reporter.error(format!("[atman] nudge rejected: {e}")),
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
            reporter.info(format!("[atman] L4 stop queued ({source}): {trimmed}"));
        }
        InjectionLevel::L3Redirect => {
            let target = cls.redirect_target.clone();
            match session.enqueue_injection_with_level(
                trimmed,
                InjectionLevel::L3Redirect,
                target.clone(),
            ) {
                Ok(id) => reporter.info(format!(
                    "[atman] L3 redirect queued ({id}, {source}) → {}",
                    target.as_deref().unwrap_or("<no target>")
                )),
                Err(e) => reporter.error(format!("[atman] L3 redirect rejected: {e}")),
            }
        }
        InjectionLevel::L2CourseCorrect => {
            match session.enqueue_injection_with_level(
                trimmed,
                InjectionLevel::L2CourseCorrect,
                None,
            ) {
                Ok(id) => reporter.info(format!(
                    "[atman] L2 course-correct queued ({id}, {source}): {trimmed}"
                )),
                Err(e) => reporter.error(format!("[atman] L2 course-correct rejected: {e}")),
            }
        }
        InjectionLevel::L1Nudge => match session.enqueue_injection(trimmed) {
            Ok(id) => reporter.info(format!(
                "[atman] L1 nudge queued ({id}, {source}): {trimmed}"
            )),
            Err(e) => reporter.error(format!("[atman] L1 nudge rejected: {e}")),
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

struct SessionRow {
    sid: String,
    mtime: std::time::SystemTime,
    events_bytes: u64,
    goal: Option<String>,
    project: Option<String>,
}

fn build_startup_recent(
    root: &Path,
    exclude_sid: &str,
    cap: usize,
) -> Vec<atman_tui::app::StartupSessionEntry> {
    let Ok(rows) = list_recent_sessions(root, cap.saturating_add(1)) else {
        return Vec::new();
    };
    let now = std::time::SystemTime::now();
    rows.into_iter()
        .filter(|r| r.sid != exclude_sid)
        .take(cap)
        .map(|r| {
            let age_secs = now
                .duration_since(r.mtime)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let event_count = count_events_lines(root, &r.sid);
            let short_id: String = r.sid.chars().take(8).collect();
            atman_tui::app::StartupSessionEntry {
                session_id: r.sid,
                short_id,
                goal: r.goal,
                project: r.project,
                age_label: format_age(age_secs),
                event_count,
            }
        })
        .collect()
}

fn count_events_lines(root: &Path, sid: &str) -> u64 {
    let path = root.join("sessions").join(sid).join("events.jsonl");
    let Ok(file) = std::fs::File::open(&path) else {
        return 0;
    };
    use std::io::BufRead;
    std::io::BufReader::new(file).lines().count() as u64
}

fn list_recent_sessions(root: &Path, cap: usize) -> Result<Vec<SessionRow>> {
    let sessions_dir = root.join("sessions");
    if !sessions_dir.exists() {
        return Ok(Vec::new());
    }
    let mut rows: Vec<SessionRow> = Vec::new();
    for entry in std::fs::read_dir(&sessions_dir)? {
        let e = entry?;
        let path = e.path();
        if !path.is_dir() {
            continue;
        }
        let sid = e.file_name().to_string_lossy().to_string();
        let events = path.join("events.jsonl");
        let (mtime, events_bytes) = match events.metadata() {
            Ok(m) => (m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len()),
            Err(_) => continue,
        };
        let goal_path = path.join("goal.txt");
        let goal = std::fs::read_to_string(&goal_path)
            .ok()
            .map(|s| s.trim_end().to_string())
            .filter(|s| !s.is_empty());
        let project = atman_runtime::session_meta::SessionMeta::load(&path)
            .and_then(|m| m.project_root)
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().to_string()));
        rows.push(SessionRow {
            sid,
            mtime,
            events_bytes,
            goal,
            project,
        });
    }
    rows.sort_by_key(|r| std::cmp::Reverse(r.mtime));
    rows.truncate(cap);
    Ok(rows)
}

fn print_sessions_table(rows: &[SessionRow], reporter: &Reporter) {
    if rows.is_empty() {
        reporter.info("[atman] no sessions on disk yet");
        return;
    }
    reporter.info(format!(
        "{:<40} {:>10} {:>8}  goal",
        "session_id", "events(B)", "age"
    ));
    let now = std::time::SystemTime::now();
    for r in rows {
        let age = now
            .duration_since(r.mtime)
            .map(|d| format_age(d.as_secs()))
            .unwrap_or_else(|_| "?".into());
        let goal_col = r.goal.as_deref().unwrap_or("");
        reporter.info(format!(
            "{:<40} {:>10} {:>8}  {}",
            r.sid, r.events_bytes, age, goal_col
        ));
    }
    reporter.info("[atman] resume with: atman --continue <session_id>");
}

fn format_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86400)
    }
}

fn discover_flow_names() -> Vec<(String, String)> {
    let Ok(cfg) = config_dir() else {
        return Vec::new();
    };
    let dir = cfg.join("commands");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|s| s.to_str()) != Some("at") {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        out.push((name.to_string(), format!("commands/{name}.at")));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn handle_rename_builtin(arg: &str, session: &Session, reporter: &Reporter) {
    let dir = session.dir();
    if dir.as_os_str().is_empty() {
        reporter.info("[atman] :rename only works on a persisted session");
        return;
    }
    let title = match arg {
        "" => {
            let current = atman_runtime::session_meta::SessionMeta::load(dir)
                .and_then(|m| m.title)
                .unwrap_or_else(|| "(none)".into());
            reporter.info(format!(
                "[atman] session title: {current}  (:rename <text> to set, :rename clear to remove)"
            ));
            return;
        }
        "clear" => None,
        s => Some(s.to_string()),
    };
    match atman_runtime::session_meta::SessionMeta::set_title(dir, title.clone()) {
        Ok(()) => match title {
            Some(t) => reporter.info(format!("[atman] renamed session → {t}")),
            None => reporter.info("[atman] session title cleared"),
        },
        Err(e) => reporter.error(format!("[atman] :rename failed: {e}")),
    }
}

fn handle_sidebar_builtin(
    arg: &str,
    cmd_tx: Option<&tokio::sync::mpsc::UnboundedSender<atman_tui::TuiCommand>>,
    reporter: &Reporter,
) {
    let Some(tx) = cmd_tx else {
        reporter.info("[atman] :sidebar only works in TUI mode");
        return;
    };
    let mode = match arg {
        "" | "toggle" => {
            reporter.info("[atman] :sidebar toggle | on | off");
            return;
        }
        "on" | "open" => atman_tui::sidebar::SidebarMode::Open,
        "off" | "close" | "closed" => atman_tui::sidebar::SidebarMode::Closed,
        other => {
            reporter.error(format!(
                "[atman] :sidebar: unknown arg `{other}` (try on/off)"
            ));
            return;
        }
    };
    let _ = tx.send(atman_tui::TuiCommand::SetSidebar(mode));
    reporter.info(format!("[atman] sidebar mode: {arg}"));
}

async fn handle_todo_builtin(arg: &str, session: &Session, reporter: &Reporter) {
    use atman_runtime::memory::todo::{TodoStatus, TodoStore};
    let store = TodoStore::at(session.dir());
    let trimmed = arg.trim();
    match trimmed {
        "" | "list" => match store.list().await {
            Ok(list) if list.is_empty() => reporter.info("[atman] no todos yet"),
            Ok(list) => {
                for (i, t) in list.iter().enumerate() {
                    let glyph = match t.status {
                        TodoStatus::Pending => "○",
                        TodoStatus::InProgress => "⚡",
                        TodoStatus::Done => "✓",
                        TodoStatus::Cancelled => "✗",
                    };
                    reporter.info(format!(
                        "  {i:>2}  {glyph} {}  ({})",
                        t.where_, t.id
                    ));
                }
            }
            Err(e) => reporter.error(format!("[atman] :todo list: {e}")),
        },
        "clear" => {
            match tokio::fs::remove_file(store.path()).await {
                Ok(()) | Err(_) => {}
            }
            session.refresh_todos_from_store_async().await;
            reporter.info("[atman] todos cleared");
        }
        s if s.starts_with("done ") => {
            let id_str = s[5..].trim();
            match parse_todo_id(id_str, &store).await {
                Ok(id) => match store.set_status(&id, TodoStatus::Done).await {
                    Ok(()) => {
                        session.refresh_todos_from_store_async().await;
                        reporter.info(format!("[atman] todo {id} → done"));
                    }
                    Err(e) => reporter.error(format!("[atman] :todo done: {e}")),
                },
                Err(e) => reporter.error(format!("[atman] :todo done: {e}")),
            }
        }
        s if s.starts_with("cancel ") => {
            let id_str = s[7..].trim();
            match parse_todo_id(id_str, &store).await {
                Ok(id) => match store.set_status(&id, TodoStatus::Cancelled).await {
                    Ok(()) => {
                        session.refresh_todos_from_store_async().await;
                        reporter.info(format!("[atman] todo {id} → cancelled"));
                    }
                    Err(e) => reporter.error(format!("[atman] :todo cancel: {e}")),
                },
                Err(e) => reporter.error(format!("[atman] :todo cancel: {e}")),
            }
        }
        other => reporter.error(format!(
            "[atman] :todo: unknown `{other}` (try: list / done <id> / cancel <id> / clear). To add todos, ask the agent — it uses memory.todo.set."
        )),
    }
}

async fn parse_todo_id(
    s: &str,
    store: &atman_runtime::memory::todo::TodoStore,
) -> Result<atman_runtime::memory::MemoryId, String> {
    if let Ok(id) = atman_runtime::memory::MemoryId::parse(s) {
        return Ok(id);
    }
    if let Ok(idx) = s.parse::<usize>() {
        let list = store
            .list()
            .await
            .map_err(|e| format!("list failed: {e}"))?;
        if let Some(t) = list.get(idx) {
            return Ok(t.id.clone());
        }
        return Err(format!("index {idx} out of range"));
    }
    Err(format!("bad todo id `{s}` (use uuid or list index)"))
}

fn handle_goal_builtin(cmd: &str, session: &Session, reporter: &Reporter) {
    let store = atman_runtime::memory::goal::GoalStore::at(session.dir());
    let rest = cmd.strip_prefix("goal").unwrap_or(cmd).trim();
    if rest.is_empty() {
        match store.get() {
            Ok(s) if s.is_empty() => reporter.info("[atman] no session goal set"),
            Ok(s) => reporter.info(format!("[atman] goal: {s}")),
            Err(e) => reporter.error(format!("[atman] :goal: read failed: {e}")),
        }
        return;
    }
    if rest == "clear" {
        match store.clear() {
            Ok(()) => {
                session.set_goal(None);
                reporter.info("[atman] goal cleared");
            }
            Err(e) => reporter.error(format!("[atman] :goal clear: {e}")),
        }
        return;
    }
    match store.set(rest) {
        Ok(()) => {
            session.set_goal(Some(rest.to_string()));
            reporter.info(format!("[atman] goal set: {rest}"));
        }
        Err(e) => reporter.error(format!("[atman] :goal set: {e}")),
    }
}

fn handle_attach_builtin(
    arg: &str,
    pending: &mut PendingUserMessage,
    session: &Session,
    reporter: &Reporter,
) {
    match arg {
        "" => {
            reporter.error(":attach <path>  |  :attach clear  |  :attach list");
        }
        "clear" => {
            pending.attachments.clear();
            session.set_attach_count(0);
            reporter.info("[atman] pending attachments cleared");
        }
        "list" => {
            if pending.attachments.is_empty() {
                reporter.info("[atman] no pending attachments");
            } else {
                for (i, p) in pending.attachments.iter().enumerate() {
                    reporter.info(format!("  {i}: {}", p.display()));
                }
            }
        }
        path => {
            let expanded = std::path::PathBuf::from(path);
            if !expanded.exists() {
                reporter.error(format!(":attach: file not found: {}", expanded.display()));
                return;
            }
            pending.attachments.push(expanded.clone());
            session.set_attach_count(pending.attachments.len());
            reporter.info(format!(
                "[atman] attached {} (pending count: {})",
                expanded.display(),
                pending.attachments.len()
            ));
        }
    }
}

fn handle_compact_builtin(session: &Session, reporter: &Reporter) {
    let model = session.last_model();
    let model = if model.is_empty() {
        "claude-opus-4.7".to_string()
    } else {
        model
    };
    let summary = format!("manual :compact (model {model})");
    match session.compact_messages(summary) {
        Some(result) => {
            reporter.info(format!(
                ":compact — {}..{} · {} → {} tokens",
                result.compacted_start,
                result.compacted_end,
                result.before_tokens,
                result.after_tokens
            ));
        }
        None => {
            reporter.info(":compact — nothing to compact (history too short or already compacted)");
        }
    }
}

fn handle_copy_builtin(arg: &str, session: &Session, reporter: &Reporter) {
    use atman_runtime::message::{MessagePart, MessageRole};
    let target = if arg.is_empty() { "last-message" } else { arg };
    let msgs = session.messages();
    let text = match target {
        "last-message" | "last" => msgs
            .iter()
            .rev()
            .find(|m| matches!(m.role, MessageRole::Assistant))
            .and_then(|m| {
                let mut buf = String::new();
                for part in &m.parts {
                    if let MessagePart::Text { text } = part {
                        buf.push_str(text);
                    }
                }
                if buf.is_empty() { None } else { Some(buf) }
            }),
        "last-tool" => msgs.iter().rev().find_map(|m| {
            m.parts.iter().rev().find_map(|part| match part {
                MessagePart::ToolResult { content, .. } => Some(content.clone()),
                _ => None,
            })
        }),
        other => {
            reporter.error(format!(
                ":copy: unknown target `{other}` — use last-message | last-tool"
            ));
            return;
        }
    };
    let Some(payload) = text else {
        reporter.info(format!(":copy: nothing to copy for {target}"));
        return;
    };
    write_osc52(&payload);
    reporter.info(format!(
        ":copy: pushed {} chars to terminal clipboard (OSC 52)",
        payload.chars().count()
    ));
}

fn write_osc52(payload: &str) {
    use base64::Engine;
    use std::io::Write;
    let encoded = base64::engine::general_purpose::STANDARD.encode(payload.as_bytes());
    let seq = format!("\x1b]52;c;{encoded}\x07");
    let _ = std::io::stderr().write_all(seq.as_bytes());
    let _ = std::io::stderr().flush();
}

async fn handle_suggest(
    executor: &Executor,
    session: &Session,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<String>,
    reporter: &Reporter,
) -> Result<()> {
    let events = session
        .events_path()
        .context("session has no events path (dry-run?)")?;
    let transcript = suggest::read_recent_events(events, suggest::recent_turns_limit())?;
    if transcript.trim().is_empty() {
        reporter.info("[atman] :suggest — no recent turns yet; talk a bit first.");
        return Ok(());
    }

    let model = load_suggest_model();
    let provider = executor
        .providers
        .resolve(&model)
        .with_context(|| format!("no provider resolves model `{model}` — configure one first"))?;

    reporter.info(format!(
        "[atman] :suggest — asking `{model}` to spot a reusable pattern…"
    ));
    let reply = suggest::generate_suggestion(provider, &model, &transcript).await?;
    if reply.trim() == "NO_SUGGESTION" {
        reporter.info("[atman] :suggest — model saw no reusable pattern.");
        return Ok(());
    }
    let Some(dsl_src) = suggest::extract_code_block(&reply) else {
        reporter.info("[atman] :suggest — model reply did not contain a fenced code block:");
        reporter.info(reply.trim().to_string());
        return Ok(());
    };

    let flow_name = match suggest::extract_flow_name(&dsl_src) {
        Ok(n) => n,
        Err(e) => {
            reporter.info(format!(
                "[atman] :suggest — suggested flow is not valid: {e}"
            ));
            reporter.info(format!("---\n{dsl_src}\n---"));
            return Ok(());
        }
    };
    let parsed = parse_file(&dsl_src)?;
    if let Err(errs) = atman_runtime::validate::validate(&parsed.flows[0], &executor.tools) {
        reporter.info("[atman] :suggest — validation rejected the suggestion:");
        for e in errs {
            reporter.info(format!("  · {e:?}"));
        }
        reporter.info(format!("---\n{dsl_src}\n---"));
        return Ok(());
    }

    let has_shell = dsl_src.contains("bash.exec") || dsl_src.contains("shell.");
    reporter.info(format!("[atman] suggested flow `{flow_name}`:"));
    reporter.info(format!("---\n{dsl_src}\n---"));
    if has_shell {
        reporter.info("[atman] note: this flow calls shell tools — accept only if you trust it.");
    }

    if reporter.is_tui() {
        reporter.info(
            "[atman] :suggest — TUI is read-only for this flow. Exit and re-run with ATMAN_NO_TUI=1 to accept.",
        );
        return Ok(());
    }

    reporter.info(
        "[atman] accept? [y] yes / [n] no / [e] print path so you can edit the buffered draft",
    );

    let choice = loop {
        let Some(line) = input_rx.recv().await else {
            reporter.info("[atman] :suggest — input closed, discarding.");
            return Ok(());
        };
        match line.trim() {
            "y" | "Y" | "yes" => break 'y',
            "n" | "N" | "no" | "" => break 'n',
            "e" | "E" | "edit" => break 'e',
            other => {
                reporter.info(format!("[atman] answer with y / n / e (got `{other}`)"));
            }
        }
    };

    if choice == 'n' {
        reporter.info("[atman] :suggest — discarded.");
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
        reporter.info(format!(
            "[atman] :suggest — `{flow_name}.at` exists; writing as `{final_name}.at` instead"
        ));
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

    reporter.info(format!(
        "[atman] :suggest — accepted. wrote {} and appended route \"{}\" → {}",
        target.display(),
        trigger,
        final_name
    ));
    if choice == 'e' {
        reporter.info(format!(
            "[atman] :suggest — open {} to edit.",
            target.display()
        ));
    }
    Ok(())
}

fn apply_session_config(session: &atman_runtime::Session) {
    let Ok(cfg) = config_dir() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(cfg.join("config.toml")) else {
        return;
    };
    if let Some(mode) = parse_compact_review_mode(&text) {
        session.set_compact_review_mode(mode);
    }
    let env_mode = std::env::var("ATMAN_FS_ACCESS")
        .ok()
        .and_then(|s| s.parse::<atman_runtime::fs_access::FsAccessMode>().ok());
    let cfg_mode = parse_fs_access_mode(&text);
    if let Some(mode) = env_mode.or(cfg_mode) {
        session.set_fs_access_mode(mode);
    }
    if let Some(mc) = parse_model_config(&text) {
        atman_runtime::model_registry::set_model_config(mc);
    }
}

pub fn load_model_config_from_disk() {
    let Ok(cfg) = config_dir() else {
        return;
    };
    let Ok(text) = std::fs::read_to_string(cfg.join("config.toml")) else {
        return;
    };
    if let Some(mc) = parse_model_config(&text) {
        atman_runtime::model_registry::set_model_config(mc);
    }
}

pub fn parse_model_config(text: &str) -> Option<atman_runtime::model_registry::ModelConfig> {
    use atman_runtime::model_registry::{AliasEntry, ModelConfig, ModelEntry};
    #[derive(serde::Deserialize, Default)]
    struct RawModel {
        #[serde(default)]
        model: Option<String>,
        #[serde(default)]
        provider: Option<String>,
        #[serde(default)]
        api_key: Option<String>,
        #[serde(default)]
        base_url: Option<String>,
        #[serde(default)]
        context_budget: Option<u64>,
        #[serde(default)]
        compact_threshold_ratio: Option<f64>,
        #[serde(default)]
        thinking: Option<bool>,
        #[serde(default)]
        max_tokens: Option<u32>,
    }
    #[derive(serde::Deserialize, Default)]
    struct RawAlias {
        model: String,
    }
    #[derive(serde::Deserialize, Default)]
    struct RawFile {
        #[serde(default)]
        models: std::collections::HashMap<String, RawModel>,
        #[serde(default)]
        alias: std::collections::HashMap<String, RawAlias>,
    }
    let raw: RawFile = toml::from_str(text).ok()?;
    let mut cfg = ModelConfig::default();
    for (name, m) in raw.models {
        cfg.models.insert(
            name,
            ModelEntry {
                model: m.model.unwrap_or_default(),
                provider: m.provider,
                api_key: m.api_key,
                base_url: m.base_url,
                context_budget: m.context_budget,
                compact_threshold_ratio: m.compact_threshold_ratio,
                thinking: m.thinking,
                max_tokens: m.max_tokens,
            },
        );
    }
    for (name, a) in raw.alias {
        cfg.aliases.insert(name, AliasEntry { model: a.model });
    }
    if cfg.models.is_empty() && cfg.aliases.is_empty() {
        return None;
    }
    Some(cfg)
}

// Reads [fs_access] mode = "..." out of the same config.toml we already
// scan for other tiny settings. Kept as a hand-rolled section walk so
// unrelated syntax errors elsewhere in the file don't kill boot.
pub fn parse_fs_access_mode(text: &str) -> Option<atman_runtime::fs_access::FsAccessMode> {
    use std::str::FromStr;
    let mut in_section = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[')
            && let Some(name) = rest.strip_suffix(']')
        {
            in_section = name.trim() == "fs_access";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == "mode" {
            let raw = v.trim().trim_matches('"');
            return atman_runtime::fs_access::FsAccessMode::from_str(raw).ok();
        }
    }
    None
}

fn parse_compact_review_mode(text: &str) -> Option<atman_runtime::CompactReviewMode> {
    let mut in_section = false;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some(rest) = line.strip_prefix('[')
            && let Some(name) = rest.strip_suffix(']')
        {
            in_section = name.trim() == "compaction";
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == "review" {
            let raw = v.trim().trim_matches('"');
            return atman_runtime::CompactReviewMode::parse(raw);
        }
    }
    None
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
        match registry.snapshot(name, source, &meta, Some(source_path)) {
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
    let raw = parse_suggest_model(&text).unwrap_or(default);
    atman_runtime::model_registry::resolve_alias(&raw)
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

async fn cmd_cost(session_id: Option<String>, all: bool) -> Result<()> {
    let root = data_dir()?;
    if all {
        return cmd_cost_all(&root).await;
    }
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
    let summary = aggregate_cost(&contents);
    print_cost_summary(&format!("session {sid}"), &summary);
    Ok(())
}

async fn cmd_cost_all(root: &Path) -> Result<()> {
    let sessions_dir = root.join("sessions");
    if !sessions_dir.exists() {
        bail!("no sessions under {}", sessions_dir.display());
    }
    let mut per_session: Vec<(String, CostSummary)> = Vec::new();
    let mut combined = CostSummary::default();
    let mut sessions_walked = 0u64;
    let mut entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&sessions_dir)
        .with_context(|| format!("read_dir {}", sessions_dir.display()))?
        .filter_map(|e| e.ok())
        .collect();
    entries.sort_by_key(|e| e.file_name());
    for entry in entries {
        let sid = entry.file_name().to_string_lossy().to_string();
        let events = entry.path().join("events.jsonl");
        if !events.exists() {
            continue;
        }
        let contents = match tokio::fs::read_to_string(&events).await {
            Ok(c) => c,
            Err(_) => continue,
        };
        let summary = aggregate_cost(&contents);
        if summary.total_calls == 0 {
            continue;
        }
        sessions_walked += 1;
        combined.merge(&summary);
        per_session.push((sid, summary));
    }
    if per_session.is_empty() {
        println!(
            "[atman] cost --all: no llm_call events found under {}",
            sessions_dir.display()
        );
        return Ok(());
    }
    println!("[atman] cost across {sessions_walked} session(s)");
    println!();
    print_cost_summary("all sessions", &combined);
    println!();
    println!("per-session totals (calls | in | cached | out | wall_ms):");
    for (sid, summary) in &per_session {
        let (calls, input, cached, output, wall) = summary.grand_totals();
        println!("  {sid:<40} {calls:>6} {input:>10} {cached:>10} {output:>10} {wall:>10}");
    }
    Ok(())
}

#[derive(Default)]
struct CostSummary {
    by_model: std::collections::BTreeMap<String, ModelTotals>,
    total_calls: u64,
}

#[derive(Default, Clone, Copy)]
struct ModelTotals {
    calls: u64,
    input: u64,
    cached: u64,
    output: u64,
    wall_ms: u64,
}

impl CostSummary {
    fn record(&mut self, model: String, input: u64, cached: u64, output: u64, wall_ms: u64) {
        let entry = self.by_model.entry(model).or_default();
        entry.calls += 1;
        entry.input += input;
        entry.cached += cached;
        entry.output += output;
        entry.wall_ms += wall_ms;
        self.total_calls += 1;
    }

    fn merge(&mut self, other: &CostSummary) {
        for (model, m) in &other.by_model {
            let entry = self.by_model.entry(model.clone()).or_default();
            entry.calls += m.calls;
            entry.input += m.input;
            entry.cached += m.cached;
            entry.output += m.output;
            entry.wall_ms += m.wall_ms;
        }
        self.total_calls += other.total_calls;
    }

    fn grand_totals(&self) -> (u64, u64, u64, u64, u64) {
        let mut acc = (0u64, 0u64, 0u64, 0u64, 0u64);
        for m in self.by_model.values() {
            acc.0 += m.calls;
            acc.1 += m.input;
            acc.2 += m.cached;
            acc.3 += m.output;
            acc.4 += m.wall_ms;
        }
        acc
    }
}

fn aggregate_cost(events_jsonl: &str) -> CostSummary {
    let mut summary = CostSummary::default();
    for line in events_jsonl.lines() {
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
        summary.record(model, input, cached, output, wall);
    }
    summary
}

fn print_cost_summary(header: &str, summary: &CostSummary) {
    println!("{header}");
    println!("total llm_calls: {}", summary.total_calls);
    println!();
    println!(
        "{:<32} {:>6} {:>10} {:>10} {:>10} {:>10}",
        "model", "calls", "in", "cached", "out", "wall_ms"
    );
    for (model, m) in &summary.by_model {
        println!(
            "{:<32} {:>6} {:>10} {:>10} {:>10} {:>10}",
            model, m.calls, m.input, m.cached, m.output, m.wall_ms
        );
    }
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
    let cwd = std::env::current_dir()?;
    let project_root =
        atman_runtime::session_meta::find_project_root(&cwd).unwrap_or_else(|| cwd.clone());
    let scope = atman_runtime::storage::resolve_project_scope_for(&project_root)
        .with_context(|| format!("resolve scope for {}", project_root.display()))?;
    let fingerprint = atman_runtime::session_meta::fingerprint_from_root(&project_root);
    let idx = atman_runtime::index::AnchorIndex::open_project(&scope)
        .with_context(|| format!("open project index at {}", scope.display()))?;
    let stats = idx
        .rebuild_events_from_sessions(&sessions_root, &fingerprint)
        .with_context(|| format!("rebuild events for fingerprint {fingerprint}"))?;
    println!(
        "project {} fingerprint={fingerprint}\n  scope: {}\n  rebuilt: {} events (skipped {})",
        project_root.display(),
        scope.display(),
        stats.rebuilt,
        stats.skipped
    );
    Ok(())
}

enum ProviderHealth {
    Reachable(u16),
    Unreachable(String),
}

async fn probe_provider(base_url: &str, timeout_ms: u64) -> ProviderHealth {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_millis(timeout_ms))
        .build()
    {
        Ok(c) => c,
        Err(e) => return ProviderHealth::Unreachable(format!("client init: {e}")),
    };
    match client.get(base_url).send().await {
        Ok(resp) => ProviderHealth::Reachable(resp.status().as_u16()),
        Err(e) => {
            let msg = if e.is_timeout() {
                format!("timeout after {timeout_ms}ms")
            } else if e.is_connect() {
                format!("connect: {e}")
            } else {
                e.to_string()
            };
            ProviderHealth::Unreachable(msg)
        }
    }
}

async fn cmd_init(sandbox: Option<String>) -> Result<()> {
    use std::str::FromStr;
    let cfg = config_dir()?;
    let fs_access = match sandbox.as_deref() {
        Some(raw) => Some(
            atman_runtime::fs_access::FsAccessMode::from_str(raw)
                .map_err(|e| anyhow::anyhow!("invalid --sandbox value: {e}"))?,
        ),
        None => None,
    };
    let rep = init::init_config_dir_with_mode(&cfg, fs_access)?;
    if rep.written.is_empty() {
        println!(
            "[atman] init: {} already fully populated ({} file(s) preserved)",
            rep.config_dir.display(),
            rep.skipped.len()
        );
    } else {
        println!(
            "[atman] init: wrote {} template(s) under {}",
            rep.written.len(),
            rep.config_dir.display()
        );
        for p in &rep.written {
            if let Ok(rel) = p.strip_prefix(&rep.config_dir) {
                println!("  + {}", rel.display());
            } else {
                println!("  + {}", p.display());
            }
        }
        if !rep.skipped.is_empty() {
            println!(
                "  {} file(s) already existed, left untouched",
                rep.skipped.len()
            );
        }
    }
    println!();
    println!("next steps:");
    println!("  1. export an api key:  export ANTHROPIC_API_KEY=...");
    println!("  2. sanity check:       atman doctor          (add --fix to auto-repair)");
    println!("  3. start REPL:         atman");
    println!("     · plain text goes to the code agent (see commands/agent.at)");
    println!("     · /hello runs commands/hello.at");
    println!("     · :goal <text> anchors the session (never evicted from context)");
    println!("     · the agent auto-tracks todos + the last 10 turns of history");
    println!("  4. see docs/quickstart.md for a walkthrough,");
    println!("     docs/context-strategy.md for how goal / todos / recent_turns compose.");
    println!();
    println!("fs access policy:");
    match fs_access {
        Some(mode) => println!(
            "  [fs_access] mode = \"{}\" persisted to config.toml.",
            mode.as_str()
        ),
        None => println!("  default is workspace-write — atman may create / edit files inside",),
    }
    println!("  this repo and the system tempdir, writes outside are blocked.");
    println!(
        "  Override per run with ATMAN_FS_ACCESS=read-only|workspace-write|danger-full-access."
    );
    Ok(())
}

const TUI_PREVIEW_SCENES: &[(&str, &str)] = &[
    ("chat", "streaming markdown + tool call round-trip"),
    ("approval-single", "one pending approval"),
    ("approval-multi", "several pending approvals stacked"),
    ("form-confirm", "yes/no confirm modal"),
    ("form-text", "single-line text input"),
    ("form-multiline", "multi-line text input"),
    ("form-single-select", "radio-style single select"),
    ("form-multi-select", "checkbox-style multi select"),
    ("form-sequence", "three forms chained one after another"),
    ("notes", "info / warn / error system notes"),
    ("workflow-running", "long-lived workflow with nested nodes"),
    ("workflow-cancelled", "workflow ended with Err cascade"),
    ("compact-review", "post-compaction summary review modal"),
];

async fn cmd_tui_preview(scene: Option<String>) -> Result<()> {
    let scene = scene.unwrap_or_else(|| "chat".into());
    if scene == "list" || scene == "help" || scene == "?" {
        println!("available scenes:");
        for (name, desc) in TUI_PREVIEW_SCENES {
            println!("  {name:<22} {desc}");
        }
        return Ok(());
    }
    let session = std::sync::Arc::new(Session::open_ephemeral());
    let (ctrl_tx, mut ctrl_rx) = tokio::sync::mpsc::unbounded_channel::<atman_tui::TuiControl>();
    let mut handle = atman_tui::TuiHandle::from_session(session.clone());
    handle.control_tx = Some(ctrl_tx);
    let ctrl_session = session.clone();
    let ctrl_task = tokio::spawn(async move {
        use atman_runtime::session::ApprovalDecision;
        while let Some(msg) = ctrl_rx.recv().await {
            match msg {
                atman_tui::TuiControl::ApproveTool(id) => {
                    ctrl_session
                        .approval()
                        .decide(&id, ApprovalDecision::Approve);
                }
                atman_tui::TuiControl::DenyTool {
                    tool_use_id,
                    reason,
                } => {
                    ctrl_session
                        .approval()
                        .decide(&tool_use_id, ApprovalDecision::Deny { reason });
                }
                atman_tui::TuiControl::ApproveAllPending => {
                    ctrl_session
                        .approval()
                        .decide_all(ApprovalDecision::Approve);
                }
                atman_tui::TuiControl::DenyAllPending { reason } => {
                    ctrl_session
                        .approval()
                        .decide_all(ApprovalDecision::Deny { reason });
                }
                atman_tui::TuiControl::FormSubmit { form_id, answer } => {
                    ctrl_session.forms().submit(&form_id, answer);
                }
                atman_tui::TuiControl::CompactReviewAccept { review_id, edited } => {
                    let decision = match edited {
                        Some(summary) => {
                            atman_runtime::CompactReviewDecision::AcceptEdited { summary }
                        }
                        None => atman_runtime::CompactReviewDecision::AcceptAsIs,
                    };
                    ctrl_session.compact_reviews().decide(&review_id, decision);
                }
                atman_tui::TuiControl::CompactReviewReject { review_id } => {
                    ctrl_session
                        .compact_reviews()
                        .decide(&review_id, atman_runtime::CompactReviewDecision::Reject);
                }
                _ => {}
            }
        }
    });
    let feeder = spawn_preview_scene(&scene, session.clone())?;
    let result = atman_tui::run_tui(handle).await;
    feeder.abort();
    ctrl_task.abort();
    result
}

fn spawn_preview_scene(
    scene: &str,
    session: std::sync::Arc<Session>,
) -> Result<tokio::task::JoinHandle<()>> {
    let scene = scene.to_string();
    Ok(tokio::spawn(async move {
        let ok = match scene.as_str() {
            "chat" => {
                preview_scene_chat(session).await;
                true
            }
            "approval-single" => {
                preview_scene_approval(session, 1).await;
                true
            }
            "approval-multi" => {
                preview_scene_approval(session, 4).await;
                true
            }
            "form-confirm" => {
                preview_scene_form(
                    session,
                    atman_runtime::form::FormKind::Confirm {
                        prompt: "Delete `~/tmp/scratch`? This cannot be undone.".into(),
                    },
                )
                .await;
                true
            }
            "form-text" => {
                preview_scene_form(
                    session,
                    atman_runtime::form::FormKind::Text {
                        prompt: "New branch name".into(),
                        placeholder: Some("feature/…".into()),
                        multiline: false,
                    },
                )
                .await;
                true
            }
            "form-multiline" => {
                preview_scene_form(
                    session,
                    atman_runtime::form::FormKind::Text {
                        prompt: "Commit message".into(),
                        placeholder: Some("Describe the change…".into()),
                        multiline: true,
                    },
                )
                .await;
                true
            }
            "form-single-select" => {
                preview_scene_form(
                    session,
                    atman_runtime::form::FormKind::SingleSelect {
                        prompt: "Which model should reply?".into(),
                        options: vec![
                            "claude-opus-4".into(),
                            "claude-sonnet-4".into(),
                            "gpt-4o".into(),
                            "deepseek-v3".into(),
                        ],
                    },
                )
                .await;
                true
            }
            "form-multi-select" => {
                preview_scene_form(
                    session,
                    atman_runtime::form::FormKind::MultiSelect {
                        prompt: "Which files should the agent read?".into(),
                        options: vec![
                            "src/lib.rs".into(),
                            "src/app.rs".into(),
                            "src/output.rs".into(),
                            "src/input.rs".into(),
                        ],
                        min: Some(1),
                        max: None,
                    },
                )
                .await;
                true
            }
            "form-sequence" => {
                preview_scene_form_sequence(session).await;
                true
            }
            "notes" => {
                preview_scene_notes(session).await;
                true
            }
            "workflow-running" => {
                preview_scene_workflow(session, false).await;
                true
            }
            "workflow-cancelled" => {
                preview_scene_workflow(session, true).await;
                true
            }
            "compact-review" => {
                preview_scene_compact_review(session).await;
                true
            }
            _ => false,
        };
        if !ok {
            eprintln!("[atman] unknown tui-preview scene: {scene}. try `atman tui-preview list`.");
        }
    }))
}

async fn preview_scene_chat(session: std::sync::Arc<Session>) {
    use atman_runtime::stream::StreamFrame;
    let tx = session.stream_tx();
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let _ = tx.send(StreamFrame::LlmChunk {
        text: "# Hello from atman\n\n".into(),
        model: "demo".into(),
    });
    for word in [
        "atman ",
        "is ",
        "a ",
        "Rust ",
        "code-agent ",
        "runtime.\n\n",
    ] {
        let _ = tx.send(StreamFrame::LlmChunk {
            text: word.into(),
            model: "demo".into(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
    }
    let demo_id = "demo_tool_1".to_string();
    let _ = tx.send(StreamFrame::ToolUseStart {
        tool: "fs.list".into(),
        args_preview: "path=\"examples\"".into(),
        id: demo_id.clone(),
    });
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let _ = tx.send(StreamFrame::ToolUseDone {
        tool: "fs.list".into(),
        ok: true,
        preview: "9 entries".into(),
        id: demo_id,
    });
    for word in ["Found ", "`agent.at`, ", "`hello.at`, ", "and ", "more.\n"] {
        let _ = tx.send(StreamFrame::LlmChunk {
            text: word.into(),
            model: "demo".into(),
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let _ = tx.send(StreamFrame::LlmDone { total_tokens: 48 });
}

async fn preview_scene_approval(session: std::sync::Arc<Session>, count: usize) {
    use atman_runtime::event::FlowRunId;
    use atman_runtime::nodegraph::NodeKind;
    use atman_runtime::session::PendingApproval;
    use atman_runtime::stream::StreamFrame;
    use atman_runtime::tool::ApprovalLevel;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let tx = session.stream_tx();
    let run_id = FlowRunId::now().0.to_string();
    let _ = tx.send(StreamFrame::FlowStart {
        run_id: run_id.clone(),
        flow_name: "demo".into(),
        parent_run_id: None,
        parent_node_id: None,
    });
    let reg = session.approval();
    let demos: &[(&str, &str, &str)] = &[
        (
            "bash",
            "cmd=\"rm -rf ./target\"",
            "will remove build artifacts (~2.4 GB)",
        ),
        (
            "fs.write",
            "path=\"/etc/hosts\"",
            "root-owned file, requires sudo",
        ),
        (
            "http.request",
            "url=\"https://example.com/api/users\", method=\"DELETE\"",
            "external network call",
        ),
        (
            "shell",
            "cmd=\"git push --force origin main\"",
            "force push to protected branch",
        ),
    ];
    for (i, (tool, args, preview)) in demos.iter().take(count).enumerate() {
        let node_id = format!("stmt_{i}");
        let tool_use_id = format!("preview_tool_{i}");
        let _ = tx.send(StreamFrame::FlowNodeStart {
            run_id: run_id.clone(),
            node_id: node_id.clone(),
            kind: NodeKind::ToolCall {
                path: (*tool).into(),
            },
            label: (*tool).into(),
            parent_node_id: None,
        });
        let _ = tx.send(StreamFrame::ToolNode {
            run_id: run_id.clone(),
            parent_node_id: node_id.clone(),
            tool_use_id: tool_use_id.clone(),
            tool: (*tool).into(),
            args_preview: (*args).into(),
        });
        let _ = tx.send(StreamFrame::ToolPendingApproval {
            run_id: run_id.clone(),
            tool_use_id: tool_use_id.clone(),
            tool_name: (*tool).into(),
            args_preview: (*args).into(),
            level: "dangerous".into(),
            preview: Some((*preview).into()),
        });
        drop(reg.request(PendingApproval {
            tool_use_id,
            tool_name: (*tool).into(),
            args_preview: (*args).into(),
            preview: Some((*preview).into()),
            level: ApprovalLevel::Dangerous,
            run_id: FlowRunId::now(),
            emitted_at: chrono::Utc::now(),
            bypass_auto_ceiling: false,
        }));
    }
}

async fn preview_scene_form(session: std::sync::Arc<Session>, kind: atman_runtime::form::FormKind) {
    use atman_runtime::event::FlowRunId;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let _rx = session.forms().request(atman_runtime::form::PendingForm {
        form_id: "preview_form".into(),
        run_id: FlowRunId::now(),
        tool_use_id: "preview_form_tool".into(),
        kind,
        emitted_at: chrono::Utc::now(),
    });
    std::mem::forget(_rx);
}

async fn preview_scene_form_sequence(session: std::sync::Arc<Session>) {
    use atman_runtime::event::FlowRunId;
    use atman_runtime::form::{FormKind, PendingForm};
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let forms = session.forms();
    let steps: Vec<(&str, FormKind)> = vec![
        (
            "step-1",
            FormKind::Confirm {
                prompt: "Ready to configure your agent?".into(),
            },
        ),
        (
            "step-2",
            FormKind::SingleSelect {
                prompt: "Which model?".into(),
                options: vec![
                    "claude-opus-4".into(),
                    "claude-sonnet-4".into(),
                    "gpt-4o".into(),
                ],
            },
        ),
        (
            "step-3",
            FormKind::Text {
                prompt: "Give this agent a name.".into(),
                placeholder: Some("e.g. release-buddy".into()),
                multiline: false,
            },
        ),
    ];
    let receivers: Vec<_> = steps
        .into_iter()
        .map(|(id, kind)| {
            forms.request(PendingForm {
                form_id: id.into(),
                run_id: FlowRunId::now(),
                tool_use_id: format!("preview_seq_{id}"),
                kind,
                emitted_at: chrono::Utc::now(),
            })
        })
        .collect();
    for rx in receivers {
        if rx.await.is_err() {
            break;
        }
    }
}

async fn preview_scene_notes(session: std::sync::Arc<Session>) {
    use atman_runtime::stream::StreamFrame;
    let tx = session.stream_tx();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let _ = tx.send(StreamFrame::Note("connected to demo provider".into()));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let _ = tx.send(StreamFrame::Note(
        "rate limit approaching (48/50 rpm)".into(),
    ));
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let _ = tx.send(StreamFrame::Note("provider returned 500 — retrying".into()));
}

async fn preview_scene_workflow(session: std::sync::Arc<Session>, cancel_midway: bool) {
    use atman_runtime::event::{FlowNodeStatus, FlowRunId};
    use atman_runtime::nodegraph::NodeKind;
    use atman_runtime::stream::StreamFrame;
    let tx = session.stream_tx();
    let run_id = FlowRunId::now().0.to_string();
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let _ = tx.send(StreamFrame::FlowStart {
        run_id: run_id.clone(),
        flow_name: "demo".into(),
        parent_run_id: None,
        parent_node_id: None,
    });
    let steps: &[(&str, NodeKind)] = &[
        (
            "plan",
            NodeKind::Message {
                role: "assistant".into(),
            },
        ),
        (
            "search",
            NodeKind::ToolCall {
                path: "fs.grep".into(),
            },
        ),
        (
            "write patch",
            NodeKind::ToolCall {
                path: "fs.write".into(),
            },
        ),
    ];
    for (i, (label, kind)) in steps.iter().enumerate() {
        let _ = tx.send(StreamFrame::FlowNodeStart {
            run_id: run_id.clone(),
            node_id: format!("stmt_{i}"),
            kind: kind.clone(),
            label: (*label).into(),
            parent_node_id: None,
        });
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if i == 0 {
            let _ = tx.send(StreamFrame::FlowNodeEnd {
                run_id: run_id.clone(),
                node_id: format!("stmt_{i}"),
                status: FlowNodeStatus::Ok,
                output_preview: Some("ready".into()),
                parent_node_id: None,
            });
        } else if cancel_midway && i == 1 {
            break;
        }
    }
    if cancel_midway {
        tokio::time::sleep(std::time::Duration::from_millis(400)).await;
        let _ = tx.send(StreamFrame::FlowDone {
            run_id,
            flow_name: "demo".into(),
            ok: false,
        });
    }
}

async fn preview_scene_compact_review(session: std::sync::Arc<Session>) {
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let _rx = session
        .compact_reviews()
        .request(atman_runtime::session::PendingCompactReview {
            review_id: "preview_review".into(),
            summary: "The user asked for a Rust CLI that lists sessions and prints their titles. \
The assistant designed the storage layout, wrote the JSON parsing, added a `session list` \
subcommand, and wired it into the daemon. All tests pass."
                .into(),
            slice_preview: "user: build a session list\nassistant: sure, here's the plan…".into(),
            slice_count: 24,
            range_start: 1,
            range_end: 24,
            tokens_before: 12800,
            emitted_at: chrono::Utc::now(),
        });
    std::mem::forget(_rx);
}

async fn cmd_doctor(fix: bool) -> Result<()> {
    let data = data_dir()?;
    let cfg = config_dir()?;
    let sessions = data.join("sessions");
    let commands = cfg.join("commands");

    let mut fixes_applied = 0usize;
    let mut fixes_hinted = 0usize;

    if !cfg.exists() {
        if fix {
            match std::fs::create_dir_all(&cfg) {
                Ok(()) => {
                    println!("  [fixed] created config dir {}", cfg.display());
                    fixes_applied += 1;
                }
                Err(e) => println!("  [fail]  create {}: {}", cfg.display(), e),
            }
        } else {
            println!(
                "  [hint]  config dir {} missing — run `atman doctor --fix` or `atman init`",
                cfg.display()
            );
            fixes_hinted += 1;
        }
    }

    let cfg_file = cfg.join("config.toml");
    if cfg.exists() && !cfg_file.exists() {
        if fix {
            match std::fs::write(&cfg_file, init::CONFIG_TOML) {
                Ok(()) => {
                    println!("  [fixed] wrote default {}", cfg_file.display());
                    fixes_applied += 1;
                }
                Err(e) => println!("  [fail]  write {}: {}", cfg_file.display(), e),
            }
        } else {
            println!(
                "  [hint]  {} missing — run `atman doctor --fix` or `atman init`",
                cfg_file.display()
            );
            fixes_hinted += 1;
        }
    }

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
    println!("project storage:");
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let project_root =
        atman_runtime::session_meta::find_project_root(&cwd).unwrap_or_else(|| cwd.clone());
    let fingerprint = atman_runtime::session_meta::fingerprint_from_root(&project_root);
    match atman_runtime::storage::resolve_project_scope_for(&project_root) {
        Ok(scope) => {
            println!(
                "  root:        {} (fingerprint={fingerprint})",
                project_root.display()
            );
            println!("  scope:       {}", scope.display());
            let probe = scope.join(".doctor-write-probe");
            match std::fs::write(&probe, b"ok").and_then(|_| std::fs::remove_file(&probe)) {
                Ok(()) => println!("  [✓] writable"),
                Err(e) => println!("  [✗] not writable: {e}"),
            }
            match atman_runtime::index::AnchorIndex::open_project(&scope) {
                Ok(idx) => {
                    let conn = idx.conn();
                    let has_events: bool = conn
                        .query_row(
                            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='events'",
                            [],
                            |r| r.get::<_, i64>(0).map(|n| n > 0),
                        )
                        .unwrap_or(false);
                    let has_fts: bool = conn
                        .query_row(
                            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='events_fts'",
                            [],
                            |r| r.get::<_, i64>(0).map(|n| n > 0),
                        )
                        .unwrap_or(false);
                    let event_count: i64 = conn
                        .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
                        .unwrap_or(0);
                    let mark = if has_events && has_fts { "✓" } else { "✗" };
                    println!(
                        "  [{mark}] index.db schema (events={has_events} events_fts={has_fts}) — {event_count} row(s)"
                    );
                }
                Err(e) => println!("  [✗] open index.db failed: {e}"),
            }
        }
        Err(e) => println!("  [✗] resolve scope failed: {e}"),
    }
    println!();
    println!("providers:");
    let probes = [
        (
            "anthropic",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "https://api.anthropic.com",
        ),
        (
            "openai",
            "OPENAI_API_KEY",
            "OPENAI_BASE_URL",
            "https://api.openai.com/v1",
        ),
        (
            "glm (anthropic compat)",
            "ATMAN_TEST_GLM_KEY",
            "ATMAN_TEST_GLM_BASE_URL",
            "https://open.bigmodel.cn/api/anthropic",
        ),
    ];
    for (name, env, base_env, default_base) in probes {
        let key_set = std::env::var(env).is_ok();
        let base = std::env::var(base_env).unwrap_or_else(|_| default_base.to_string());
        let key_mark = if key_set { "✓" } else { "✗" };
        if key_set {
            let health = probe_provider(&base, 3000).await;
            let health_mark = match &health {
                ProviderHealth::Reachable(status) => format!("reachable (HTTP {status})"),
                ProviderHealth::Unreachable(reason) => format!("unreachable: {reason}"),
            };
            println!("  [{key_mark}] {name:<28} ${env}  → {base}  [{health_mark}]");
        } else {
            println!("  [{key_mark}] {name:<28} ${env}  → {base}  [skipped: no api key]");
        }
    }
    println!();

    let cfg_text = std::fs::read_to_string(config_dir().unwrap_or_default().join("config.toml"))
        .unwrap_or_default();
    if let Some(mc) = parse_model_config(&cfg_text) {
        println!("models:");
        for (name, entry) in &mc.models {
            let budget = entry
                .context_budget
                .map(|b| b.to_string())
                .unwrap_or_else(|| "builtin".into());
            let thinking = if entry.thinking.unwrap_or(false) {
                " thinking"
            } else {
                ""
            };
            println!("  {name:<28} budget={budget}{thinking}");
        }
        if !mc.aliases.is_empty() {
            println!("aliases:");
            for (name, entry) in &mc.aliases {
                println!("  {name:<28} → {model}", model = entry.model);
            }
        }
        println!();
    }

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
    println!();
    if fix {
        println!(
            "Applied {fixes_applied} fix(es). Re-run `atman doctor` to verify remaining items."
        );
    } else if fixes_hinted > 0 {
        println!("{fixes_hinted} item(s) can be auto-repaired. Run `atman doctor --fix` to apply.");
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

fn open_current_project_index() -> Result<Option<std::sync::Arc<atman_runtime::index::AnchorIndex>>>
{
    let scope = atman_runtime::storage::resolve_current_project_scope()
        .context("resolve project storage scope")?;
    match atman_runtime::index::AnchorIndex::open_project(&scope) {
        Ok(idx) => Ok(Some(std::sync::Arc::new(idx))),
        Err(e) => {
            eprintln!(
                "[atman] project index unavailable at {} — history search disabled: {e}",
                scope.display()
            );
            Ok(None)
        }
    }
}

fn attach_memory_stores(
    executor: &mut atman_runtime::Executor,
    session: &atman_runtime::Session,
    ephemeral: bool,
) -> Result<()> {
    let session_dir = session.dir();
    let (session_scope, scope_root, project_index) = if ephemeral {
        let scratch = data_dir()?.join("ephemeral");
        std::fs::create_dir_all(&scratch).ok();
        (scratch.clone(), scratch, None)
    } else {
        let scope = atman_runtime::storage::resolve_current_project_scope()?;
        (
            session_dir.to_path_buf(),
            scope,
            open_current_project_index()?,
        )
    };
    let redactor = atman_daemon::bootstrap::build_redactor(config_dir().ok().as_deref());
    atman_daemon::bootstrap::attach_memory_stores_with_redactor(
        executor,
        &session_scope,
        &scope_root,
        redactor,
        project_index,
        session.goal_watch().clone(),
        session.todos_watch().clone(),
        session.plans_watch().clone(),
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
            into,
        } => {
            if out.is_none() && into.is_none() {
                bail!("migrate import: pass either --out <path> or --into new");
            }
            let source = build_migration_source(&from, storage)?;
            let resolved_id = match session_id {
                Some(id) => id,
                None => pick_session_interactively(source.as_ref(), &from)?,
            };
            let messages = source.load_messages(&resolved_id)?;
            if messages.is_empty() {
                bail!("session {resolved_id} loaded 0 messages — nothing to import");
            }
            if let Some(out_path) = out {
                if let Some(parent) = out_path.parent()
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
                std::fs::write(&out_path, body)
                    .with_context(|| format!("write {}", out_path.display()))?;
                println!(
                    "[atman] migrate: wrote {} messages from {from}/{resolved_id} to {}",
                    messages.len(),
                    out_path.display()
                );
                return Ok(());
            }
            let root = data_dir()?;
            let session = Session::open(&root)
                .with_context(|| format!("open a fresh atman session under {}", root.display()))?;
            let sid = session.id().to_string();
            let events = session.events_path().map(|p| p.display().to_string());
            replay_messages_into(&session, source.source_tag(), &messages);
            session.shutdown().await;
            println!(
                "[atman] migrate: replayed {} messages from {from}/{resolved_id} into new session {sid}",
                messages.len()
            );
            if let Some(p) = events {
                println!("[atman] migrate: events → {p}");
            }
            Ok(())
        }
    }
}

fn pick_session_interactively(
    source: &dyn migrate_source::MigrationSource,
    from: &str,
) -> Result<String> {
    let sessions = source.discover_sessions()?;
    if sessions.is_empty() {
        bail!("migrate import: no sessions in {from} storage — nothing to pick from");
    }
    eprintln!("[atman] {from} sessions (newest first):");
    for (i, s) in sessions.iter().enumerate() {
        eprintln!("  {:>3}. {}  ms={}  {}", i + 1, s.id, s.created_ms, s.title);
    }
    eprint!("[atman] pick number 1-{} (blank cancels): ", sessions.len());
    use std::io::{BufRead, Write};
    let _ = std::io::stderr().flush();
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line)? == 0 {
        bail!("migrate import: stdin closed before a pick");
    }
    let trimmed = line.trim();
    if trimmed.is_empty() {
        bail!("migrate import: no pick given, aborted");
    }
    let idx: usize = trimmed
        .parse()
        .with_context(|| format!("`{trimmed}` is not a number"))?;
    if idx == 0 || idx > sessions.len() {
        bail!(
            "migrate import: pick {idx} out of range 1..={}",
            sessions.len()
        );
    }
    Ok(sessions[idx - 1].id.clone())
}

fn replay_messages_into(
    session: &Session,
    source_tag: &str,
    messages: &[migrate_source::ImportedMessage],
) {
    for m in messages {
        let turn_id = atman_runtime::event::TurnId::now();
        let text = if let Some(agent) = &m.agent {
            format!("[migrated from {source_tag}, agent={agent}]\n{}", m.text)
        } else {
            format!("[migrated from {source_tag}]\n{}", m.text)
        };
        let msg = match m.role {
            migrate_source::MessageRole::User => {
                atman_runtime::message::Message::user_text(turn_id, text)
            }
            migrate_source::MessageRole::Assistant => {
                atman_runtime::message::Message::assistant_text(turn_id, text)
            }
            migrate_source::MessageRole::System => {
                atman_runtime::message::Message::system_text(turn_id, text)
            }
            migrate_source::MessageRole::Tool => atman_runtime::message::Message {
                role: atman_runtime::message::MessageRole::Tool,
                parts: vec![atman_runtime::message::MessagePart::Text { text }],
                turn_id,
            },
        };
        session.append_message(msg, None);
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
        "kiro-cli" => {
            let root = match storage {
                Some(p) => p,
                None => migrate_source::KiroCliSource::default_root()?,
            };
            Ok(Box::new(migrate_source::KiroCliSource::new(root)))
        }
        other => bail!("unknown migration source `{other}` (want: opencode | kiro-cli)"),
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
    match &action {
        FlowAction::Lint { path } => return cmd_flow_lint(path),
        FlowAction::Test { path, bless } => return cmd_flow_test(path, *bless).await,
        _ => {}
    }
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
        } => cmd_flow_rollback(&registry, &flow_name, &version, to.as_deref(), yes),
        FlowAction::Lint { .. } | FlowAction::Test { .. } => {
            unreachable!("handled above")
        }
    }
}

async fn cmd_flow_test(path: &Path, bless: bool) -> Result<()> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let file = atman_dsl::parse::parse_file(&source)
        .with_context(|| format!("parse {}", path.display()))?;
    let cases: Vec<&atman_dsl::ast::FlowDecl> =
        file.flows.iter().filter(|f| f.params.is_empty()).collect();
    let skipped: Vec<String> = file
        .flows
        .iter()
        .filter(|f| !f.params.is_empty())
        .map(|f| f.name.name.clone())
        .collect();
    if cases.is_empty() {
        println!(
            "[atman] flow test: {} has no 0-param flows; nothing to run",
            path.display()
        );
        if !skipped.is_empty() {
            println!("  skipped flows requiring args: {}", skipped.join(", "));
        }
        return Ok(());
    }

    let mut ex = atman_runtime::Executor::new();
    atman_runtime::tools::register_tier_zero(&mut ex.tools);
    ex.providers.register(std::sync::Arc::new(
        atman_runtime::providers::mock::MockProvider::new("mock")
            .with_fallback(atman_runtime::Value::Str("[mock reply]".into())),
    ));

    let mut recorded: std::collections::BTreeMap<String, serde_json::Value> =
        std::collections::BTreeMap::new();
    let mut errors: Vec<(String, String)> = Vec::new();
    for flow in &cases {
        match ex.run(&file, flow.name.name.as_str(), vec![]).await {
            Ok(v) => {
                recorded.insert(flow.name.name.clone(), v.to_json());
            }
            Err(e) => errors.push((flow.name.name.clone(), format!("{e}"))),
        }
    }
    if !errors.is_empty() {
        for (name, msg) in &errors {
            eprintln!("[atman] flow test: {name} raised {msg}");
        }
        bail!("flow test: {} flow(s) errored", errors.len());
    }

    let snap_path = snap_path_for(path);
    let existing = if snap_path.exists() {
        Some(load_snapshot(&snap_path)?)
    } else {
        None
    };
    match (existing, bless) {
        (None, _) => {
            write_snapshot(&snap_path, &recorded)?;
            println!(
                "[atman] flow test: wrote fresh snapshot {} ({} case(s))",
                snap_path.display(),
                recorded.len()
            );
        }
        (Some(_), true) => {
            write_snapshot(&snap_path, &recorded)?;
            println!(
                "[atman] flow test: refreshed snapshot {} ({} case(s))",
                snap_path.display(),
                recorded.len()
            );
        }
        (Some(prev), false) => {
            let mut mismatches: Vec<String> = Vec::new();
            let mut prev_names: std::collections::BTreeSet<&String> = prev.keys().collect();
            for (name, cur) in &recorded {
                match prev.get(name) {
                    Some(old) if old == cur => {
                        prev_names.remove(name);
                    }
                    Some(_) => {
                        prev_names.remove(name);
                        mismatches.push(name.clone());
                    }
                    None => mismatches.push(format!("{name} (new)")),
                }
            }
            for orphan in prev_names {
                mismatches.push(format!("{orphan} (removed)"));
            }
            if mismatches.is_empty() {
                println!(
                    "[atman] flow test: {} case(s) match {}",
                    recorded.len(),
                    snap_path.display()
                );
            } else {
                for name in &mismatches {
                    println!("[atman] flow test drift: {name}");
                }
                bail!(
                    "flow test: {} case(s) drifted — re-run with --bless to accept",
                    mismatches.len()
                );
            }
        }
    }
    if !skipped.is_empty() {
        println!(
            "[atman] flow test: skipped flows requiring args: {}",
            skipped.join(", ")
        );
    }
    Ok(())
}

fn snap_path_for(flow_path: &Path) -> PathBuf {
    let name = flow_path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "flow".to_string());
    flow_path.with_file_name(format!("{name}.snap.json"))
}

fn load_snapshot(path: &Path) -> Result<std::collections::BTreeMap<String, serde_json::Value>> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let map: std::collections::BTreeMap<String, serde_json::Value> =
        serde_json::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(map)
}

fn write_snapshot(
    path: &Path,
    snap: &std::collections::BTreeMap<String, serde_json::Value>,
) -> Result<()> {
    let text = serde_json::to_string_pretty(snap)?;
    std::fs::write(path, format!("{text}\n")).with_context(|| format!("write {}", path.display()))
}

fn cmd_flow_lint(path: &Path) -> Result<()> {
    let source =
        std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let file = atman_dsl::parse::parse_file(&source)
        .with_context(|| format!("parse {}", path.display()))?;
    let hits = atman_runtime::flow_lint::lint_file(&file);
    if hits.is_empty() {
        println!("[atman] flow lint: {} — clean", path.display());
        return Ok(());
    }
    for hit in &hits {
        println!(
            "{}:{}:{}: {}",
            path.display(),
            hit.flow,
            hit.rule.slug(),
            hit.message
        );
    }
    bail!("flow lint: {} hit(s)", hits.len());
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
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let outcome = registry.snapshot(&name, &content, &meta, Some(canonical.as_path()))?;
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
    target: Option<&Path>,
    assume_yes: bool,
) -> Result<()> {
    let rev = registry
        .find_by_version(flow_name, version)?
        .with_context(|| format!("no revision matches `{version}` for `{flow_name}`"))?;
    let (target_buf, target_source) = match target {
        Some(t) => (t.to_path_buf(), "--to"),
        None => {
            let origin = rev.origin_path.as_deref().with_context(|| {
                format!(
                    "no --to given and revision {} for `{flow_name}` has no stored origin path — pass --to <file>",
                    rev.version
                )
            })?;
            println!(
                "[atman] no --to given; using stored origin {} from revision id={}",
                origin, rev.id
            );
            (PathBuf::from(origin), "origin")
        }
    };
    let target_path = target_buf.as_path();
    if target_path.is_dir() {
        bail!(
            "{target_source} {} is a directory (want a file path)",
            target_path.display()
        );
    }
    if let Some(git_root) = git_root_containing(target_path) {
        eprintln!(
            "[atman] note: {} lives inside git repo at {}. `git checkout <sha> -- {}` may be a safer rollback path.",
            target_path.display(),
            git_root.display(),
            target_path.display()
        );
        if !assume_yes {
            bail!(
                "rollback aborted — re-run with --yes to overwrite {} anyway",
                target_path.display()
            );
        }
    }
    if target_path.exists() && !assume_yes {
        eprintln!(
            "[atman] refusing to overwrite {} without --yes (would replace with {} @ {}, id={})",
            target_path.display(),
            flow_name,
            rev.version,
            rev.id
        );
        bail!("rollback aborted");
    }
    if let Some(parent) = target_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    std::fs::write(target_path, &rev.content)
        .with_context(|| format!("write {}", target_path.display()))?;
    println!(
        "[atman] rolled back {} to {} (id={}) at {}",
        flow_name,
        rev.version,
        rev.id,
        target_path.display()
    );
    Ok(())
}

fn git_root_containing(target: &Path) -> Option<PathBuf> {
    let probe_dir = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    atman_runtime::git::discover_toplevel(probe_dir).ok()
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

async fn cmd_logs_stream(
    session_id: Option<String>,
    port: u16,
    since_seq: Option<u64>,
) -> Result<()> {
    let root = data_dir()?;
    let sid = match session_id {
        Some(s) => s,
        None => latest_session(&root)?
            .with_context(|| format!("no sessions found under {}", root.display()))?,
    };
    let cfg_path = atman_daemon::config::default_config_path()?;
    let cfg = atman_daemon::config::DaemonConfig::load_or_init(&cfg_path)?;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    eprintln!("[atman] streaming events for session {sid} from {base}/events");
    stream_daemon_events(&client, &base, &cfg.auth_token, &sid, since_seq, false).await
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
    atman_runtime::storage::data_dir()
}

fn config_dir() -> Result<PathBuf> {
    atman_runtime::storage::config_dir()
}

fn resolve_session_prefix(root: &std::path::Path, sid: &str) -> Result<String> {
    if uuid::Uuid::parse_str(sid).is_ok() {
        return Ok(sid.to_string());
    }
    let sessions = root.join("sessions");
    let mut matches: Vec<String> = Vec::new();
    if sessions.exists() {
        if let Ok(entries) = std::fs::read_dir(&sessions) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(sid) {
                    matches.push(name);
                }
            }
        }
    }
    match matches.len() {
        0 => bail!("no session found matching prefix `{sid}`"),
        1 => Ok(matches[0].clone()),
        _ => {
            bail!(
                "ambiguous session prefix `{sid}` matches {} sessions: {}",
                matches.len(),
                matches
                    .iter()
                    .take(5)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        }
    }
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
