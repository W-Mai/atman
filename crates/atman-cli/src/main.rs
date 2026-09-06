use anyhow::{Context, Result, bail};
use atman_dsl::parse::parse_file;
use atman_runtime::{Session, Value};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

mod daemon_repl;
mod daemon_tui;
mod mcp_templates;
mod migrate_source;
mod monitor;
mod repl_completer;
mod sync;
mod upgrade;

#[derive(Parser, Debug)]
#[command(name = "atman", version, about = "atman witnesses; code exists")]
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
        #[arg(long, value_name = "LEVEL")]
        reasoning: Option<String>,
        #[arg(long = "image", value_name = "PATH")]
        images: Vec<PathBuf>,
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
    /// Update atman with the official installer from atman.run.
    Upgrade {
        /// Skip confirmation when the current executable is not the installer target.
        #[arg(long)]
        yes: bool,
        /// Pass verbose output through to the official installer.
        #[arg(long)]
        verbose: bool,
        /// Prevent the installer from editing shell profile PATH entries.
        #[arg(long)]
        no_modify_path: bool,
    },
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
    /// Manage MCP (Model Context Protocol) servers.
    Mcp {
        #[command(subcommand)]
        action: McpAction,
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
enum McpAction {
    /// List all configured MCP servers.
    List,
    /// Add a new MCP server interactively.
    Add {
        #[arg(long)]
        template: Option<String>,
    },
    /// Remove an MCP server.
    Remove { name: String },
    /// Test connection to an MCP server.
    Test { name: String },
    /// List tools provided by an MCP server.
    Tools { name: String },
    /// List resources provided by an MCP server.
    Resources { name: String },
    /// List prompts provided by an MCP server.
    Prompts { name: String },
    /// Import MCP servers from a JSON file (Claude Desktop / Cursor / Cline format).
    Import { file: PathBuf },
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
    #[command(hide = true)]
    Serve,
    Run {
        file: PathBuf,
        #[arg(long)]
        follow: bool,
        #[arg(long, default_value_t = 65099)]
        port: u16,
        #[arg(long, value_name = "LEVEL")]
        reasoning: Option<String>,
        #[arg(long = "image", value_name = "PATH")]
        images: Vec<PathBuf>,
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
    New,
    Move {
        session_id: String,
        /// New working directory for the session.
        cwd: PathBuf,
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
            atman_runtime::notify!(
                info,
                "moved {} config item(s) from {} to {}",
                rep.moved.len(),
                rep.from.display(),
                rep.to.display()
            );
            for name in &rep.moved {
                atman_runtime::notify!(info, "  moved: {name}");
            }
            for name in &rep.skipped_conflicts {
                atman_runtime::notify!(info, "  skipped (already at destination): {name}");
            }
            for item in &rep.artifacts {
                if matches!(
                    item.kind,
                    atman_runtime::config_migration::ArtifactOutcomeKind::RejectedFileType
                        | atman_runtime::config_migration::ArtifactOutcomeKind::FailedBeforePublish
                        | atman_runtime::config_migration::ArtifactOutcomeKind::CommittedSourceRetained
                ) {
                    atman_runtime::notify!(
                        warn,
                        "  legacy config {}: {:?}{}",
                        item.path,
                        item.kind,
                        item.error
                            .as_deref()
                            .map(|error| format!(": {error}"))
                            .unwrap_or_default()
                    );
                }
            }
        }
        Ok(None) => {}
        Err(e) => atman_runtime::notify!(error, "config migration skipped: {e:#}"),
    }
}

fn main() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(16 * 1024 * 1024)
        .build()?;
    runtime.block_on(async_main())
}

async fn async_main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(Cmd::Upgrade {
        yes,
        verbose,
        no_modify_path,
    }) = &cli.cmd
    {
        return upgrade::run(upgrade::UpgradeOptions {
            yes: *yes,
            verbose: *verbose,
            no_modify_path: *no_modify_path,
        })
        .await;
    }
    run_startup_config_migration();
    // Install default notifier (TUI replaces with its own sink on boot).
    atman_runtime::notify::install(std::sync::Arc::new(atman_runtime::notify::CliSink));
    if let Ok(dd) = data_dir() {
        atman_runtime::notify::install_log(std::sync::Arc::new(
            atman_runtime::notify::LogSink::new(dd),
        ));
    }
    // Probe theme before raw mode so OSC 11 reply can't leak into KeyEvents.
    let _ = atman_tui::theme::theme();
    match cli.cmd {
        None if tui_mode_requested() => daemon_tui::run(cli.r#continue).await,
        None => daemon_repl::run(cli.r#continue).await,
        Some(Cmd::Version) => {
            println!("atman v{}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(Cmd::Upgrade { .. }) => {
            unreachable!("upgrade is dispatched before notifier and theme initialization")
        }
        Some(Cmd::Run {
            file,
            flow,
            mock,
            ephemeral,
            reasoning,
            images,
            args,
        }) => cmd_run(file, flow, mock, ephemeral, reasoning, images, args).await,
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
            action: SessionAction::New,
        }) => cmd_session_new().await,
        Some(Cmd::Session {
            action: SessionAction::Move { session_id, cwd },
        }) => cmd_session_move(&session_id, &cwd).await,
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
        Some(Cmd::Monitor { port }) => monitor::run(port).await,
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
        Some(Cmd::Daemon {
            action: DaemonAction::Serve,
        }) => atman_daemon::server::serve().await,
        Some(Cmd::Flow { action }) => cmd_flow(action).await,
        Some(Cmd::Sync { action }) => cmd_sync(action).await,
        Some(Cmd::Migrate { action }) => cmd_migrate(action).await,
        Some(Cmd::Mcp { action }) => cmd_mcp(action).await,
        Some(Cmd::Daemon {
            action:
                DaemonAction::Run {
                    file,
                    follow,
                    port,
                    reasoning,
                    images,
                },
        }) => cmd_daemon_run(file, follow, port, reasoning, images).await,
    }
}

async fn cmd_daemon_run(
    file: PathBuf,
    follow: bool,
    port: u16,
    reasoning: Option<String>,
    images: Vec<PathBuf>,
) -> Result<()> {
    let client = connect_http_daemon(port).await?;

    let abs = if file.is_absolute() {
        file.clone()
    } else {
        std::env::current_dir()?.join(&file)
    };

    let images = inline_images(images)?;
    let run = client
        .run_flow(
            abs.to_string_lossy().into_owned(),
            None,
            Some(std::env::current_dir()?.to_string_lossy().into_owned()),
            serde_json::Map::new(),
            reasoning,
            images,
        )
        .await
        .context("start daemon flow")?;
    println!("session_id: {}", run.session_id);
    println!("run_id:     {}", run.run_id);

    if !follow {
        return Ok(());
    }
    follow_daemon_run(&client, &run).await?;
    Ok(())
}

pub(crate) fn inline_images(images: Vec<PathBuf>) -> Result<Vec<atman_proto::InlineImage>> {
    images
        .into_iter()
        .map(|path| {
            let source = atman_runtime::attachment_store::AttachmentStore::at("")
                .import_path(&path)
                .with_context(|| format!("reading image {}", path.display()))?;
            Ok(atman_proto::InlineImage {
                data_base64: atman_runtime::attachment_store::image_base64(&source, None)?,
                name: path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .map(str::to_owned),
            })
        })
        .collect()
}

async fn follow_daemon_run(
    client: &atman_client::Client,
    run: &atman_proto::RunFlowResponse,
) -> Result<()> {
    use futures::StreamExt;

    let session = client
        .attach_session(run.session_id.clone())
        .await
        .context("attach daemon session for follow")?;
    if let Some(completed) =
        terminal_run_projection(&session.current().projection().runs, &run.run_id).cloned()
    {
        wait_for_durable_run_events(client, &run.session_id, &run.run_id, &completed).await?;
        return Ok(());
    }
    let Some(mut events) = client
        .session_events(run.session_id.clone(), session.current().cursor())
        .await?
    else {
        bail!("daemon transport does not support session event streaming");
    };
    while let Some(event) = events.next().await {
        let event = event?;
        println!("{}", serde_json::to_string(&event)?);
        session.apply_event(event).await?;
        if let Some(completed) =
            terminal_run_projection(&session.current().projection().runs, &run.run_id).cloned()
        {
            wait_for_durable_run_events(client, &run.session_id, &run.run_id, &completed).await?;
            return Ok(());
        }
    }
    bail!(
        "session event stream ended before run {} reached a terminal state",
        run.run_id
    )
}

fn terminal_run_projection<'a>(
    runs: &'a [atman_proto::RunProjection],
    run_id: &atman_proto::FlowRunId,
) -> Option<&'a atman_proto::RunProjection> {
    runs.iter().find(|run| {
        &run.id == run_id
            && matches!(
                run.state,
                atman_proto::RunLifecycle::Cancelled
                    | atman_proto::RunLifecycle::Succeeded
                    | atman_proto::RunLifecycle::Failed
                    | atman_proto::RunLifecycle::Lost
            )
    })
}

async fn wait_daemon_run(
    client: &atman_client::Client,
    run: &atman_proto::RunFlowResponse,
) -> Result<atman_proto::RunProjection> {
    let session = client
        .attach_session(run.session_id.clone())
        .await
        .context("attach daemon session for run")?;
    loop {
        if let Some(projection) =
            terminal_run_projection(&session.current().projection().runs, &run.run_id)
        {
            let completed = projection.clone();
            wait_for_durable_run_events(client, &run.session_id, &run.run_id, &completed).await?;
            return Ok(completed);
        }
        session
            .refresh_until_current()
            .await
            .context("refresh daemon run state")?;
        if terminal_run_projection(&session.current().projection().runs, &run.run_id).is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
}

pub(crate) async fn wait_for_durable_run_events(
    client: &atman_client::Client,
    session_id: &atman_proto::SessionId,
    run_id: &atman_proto::FlowRunId,
    completed: &atman_proto::RunProjection,
) -> Result<()> {
    let run_id = run_id.to_string();
    let turn_id = completed
        .turn_id
        .as_ref()
        .map(|turn_id| turn_id.0.to_string());
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    let mut cursor = 0;
    let mut saw_flow_end = false;
    let mut saw_turn_end = turn_id.is_none();
    loop {
        let page = client
            .get_events(session_id.clone(), Some(cursor))
            .await
            .context("wait for durable daemon run events")?;
        if page.has_more && page.next_cursor.0 <= cursor {
            bail!("daemon event page did not advance beyond cursor {cursor}");
        }
        cursor = page.next_cursor.0;
        for envelope in page.events {
            let event = envelope.event;
            saw_flow_end |= event["type"] == "flow_end" && event["run_id"] == run_id;
            saw_turn_end |= event["type"] == "turn_end"
                && turn_id
                    .as_deref()
                    .is_some_and(|turn_id| event["turn_id"] == turn_id);
        }
        if saw_flow_end && saw_turn_end {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            bail!("daemon run {run_id} reached terminal state before its events became durable");
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

async fn cmd_daemon_start() -> Result<()> {
    let pid_path = atman_daemon::pidfile::default_pid_path()?;
    if let Some(pid) = atman_daemon::pidfile::read_pid(&pid_path)?
        && atman_daemon::pidfile::is_alive(pid)
    {
        println!("atman-daemon already running (pid={pid})");
        return Ok(());
    }
    let child = spawn_daemon_process()?;
    println!("atman-daemon spawned (pid={})", child.id());
    println!("pid file: {}", pid_path.display());
    Ok(())
}

pub(crate) fn spawn_daemon_process() -> Result<std::process::Child> {
    let current = std::env::current_exe().context("resolve current executable")?;
    let mut command = daemon_process_command(&current);
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawn atman daemon")
}

fn daemon_process_command(current: &Path) -> std::process::Command {
    let sibling = current
        .parent()
        .map(|directory| directory.join("atman-daemon"));
    match sibling.filter(|path| path.exists()) {
        Some(binary) => std::process::Command::new(binary),
        None => {
            let mut command = std::process::Command::new(current);
            command.args(["daemon", "serve"]);
            command
        }
    }
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
    let cfg = atman_runtime::config_hub::ConfigHub::from_daemon_config_path(&cfg_path)
        .rotate_daemon_config()?;
    println!("{}", cfg.auth_token);
    atman_runtime::notify!(
        success,
        location = Log,
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
    reasoning: Option<String>,
    images: Vec<PathBuf>,
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
    let target_flow = parsed
        .flows
        .iter()
        .find(|flow| flow.name.name == flow_name)
        .ok_or_else(|| anyhow::anyhow!("flow `{flow_name}` not found in {}", file.display()))?;

    if !mock && !ephemeral {
        if load_auto_snapshot() {
            auto_snapshot_flows(&file, &source, &parsed);
        }
        let flow_path = if file.is_absolute() {
            file
        } else {
            std::env::current_dir()?.join(file)
        };
        let args = args
            .into_iter()
            .map(|(name, value)| (name, value.to_json()))
            .collect();
        let client = daemon_tui::connect_local_daemon().await?;
        let project_root = std::env::current_dir()?.to_string_lossy().into_owned();
        let run = client
            .run_flow(
                flow_path.to_string_lossy().into_owned(),
                Some(flow_name),
                Some(project_root),
                args,
                reasoning,
                inline_images(images)?,
            )
            .await
            .context("start daemon flow")?;
        let completed = wait_daemon_run(&client, &run).await?;
        return match completed.state {
            atman_proto::RunLifecycle::Succeeded => {
                println!("{}", completed.output.unwrap_or_default());
                Ok(())
            }
            atman_proto::RunLifecycle::Failed => {
                bail!(
                    "flow error: {}",
                    completed.error.as_deref().unwrap_or("unknown error")
                )
            }
            atman_proto::RunLifecycle::Cancelled => bail!("flow cancelled"),
            atman_proto::RunLifecycle::Lost => bail!("flow lost after daemon restart"),
            _ => unreachable!("wait_daemon_run returned a non-terminal run"),
        };
    }

    let invocation_env = match reasoning {
        Some(value) => {
            let selection: atman_runtime::provider::ReasoningSelection = value
                .parse()
                .map_err(|error: String| anyhow::anyhow!("invalid --reasoning value: {error}"))?;
            atman_runtime::InvocationEnv::single("effort", Value::Str(selection.to_string()))
        }
        None => atman_runtime::InvocationEnv::default(),
    };

    let redactor = atman_daemon::bootstrap::build_redactor(config_dir().ok().as_deref());
    let session = std::sync::Arc::new(if ephemeral {
        Session::open_ephemeral()
    } else {
        let root = data_dir()?;
        let project_index = open_current_project_index()?;
        Session::open_with_context_and_trust(
            &root,
            redactor.clone(),
            project_index,
            load_global_trust_config()?,
        )
        .with_context(|| format!("opening session under {}", root.display()))?
    });
    if let Some(path) = session.events_path() {
        atman_runtime::notify!(
            info,
            location = Log,
            "session={} events={}",
            session.id(),
            path.display()
        );
    }

    load_model_config_from_disk();

    let atman_daemon::bootstrap::BootstrapOutcome {
        mut executor,
        provider_catalog_refresh_plan,
        ..
    } = atman_daemon::bootstrap::build_executor(bootstrap_opts(session.sink().clone(), mock)?)
        .await?;
    if !mock && let Some(lifecycle) = executor.provider_lifecycle() {
        consume_provider_catalog_refresh_plan(provider_catalog_refresh_plan, move |provider_id| {
            let lifecycle = lifecycle.clone();
            async move { lifecycle.refresh_models_if_stale(&provider_id).await }
        })
        .await;
    }
    executor.source_dir = file.parent().map(|p| p.to_path_buf());
    let _mcp_shutdown = atman_daemon::bootstrap::spawn_mcp_boot(
        executor.clone(),
        session.clone(),
        config_dir().ok().as_deref(),
    );
    attach_memory_stores(&mut executor, &session, ephemeral)?;
    executor.tool_ctx.prompt_resolver = Some(std::sync::Arc::new(
        atman_runtime::rendezvous::AutoResolveResolver {
            default: serde_json::json!({ "hunks": [] }),
        },
    ));

    if let Err(errs) = atman_runtime::validate::validate(target_flow, &executor.tools) {
        for e in &errs {
            atman_runtime::notify!(warn, "validation: {e}");
        }
        bail!("flow validation failed with {} error(s)", errs.len());
    }

    if load_auto_snapshot() {
        auto_snapshot_flows(&file, &source, &parsed);
    }

    let turn_id = atman_runtime::event::TurnId::now();
    let user_text = if args.is_empty() {
        flow_name.clone()
    } else {
        args.iter()
            .map(|(k, v)| format!("{k}={}", v.render_text()))
            .collect::<Vec<_>>()
            .join(" ")
    };
    let mut parts = Vec::with_capacity(images.len() + 1);
    for image in images {
        parts.push(atman_runtime::message::MessagePart::Image {
            id: None,
            source: session.import_image_path(image)?,
        });
    }
    parts.push(atman_runtime::message::MessagePart::Text {
        text: user_text.clone(),
    });
    let user_msg = atman_runtime::message::Message {
        role: atman_runtime::message::MessageRole::User,
        parts,
        turn_id: turn_id.clone(),
        origin: atman_runtime::message::MessageOrigin::User,
    };
    {
        let _compact_guard = session.acquire_compact_lock().await;
        session.begin_turn(user_msg);
    }
    let outcome = executor
        .run_in_turn_with_env(
            &parsed,
            &flow_name,
            args,
            Some(turn_id.clone()),
            Some(session.clone()),
            invocation_env,
        )
        .await;
    session.end_turn(&turn_id);
    if outcome.is_ok() && session.record_successful_flow().is_some() {
        let _ =
            atman_runtime::session_naming::maybe_generate_session_name(&executor, &session).await;
    }
    session.shutdown().await;

    match outcome {
        Ok(v) => {
            println!("{}", v.render_text());
            Ok(())
        }
        Err(e) => {
            atman_runtime::notify!(error, "flow error: {e}");
            std::process::exit(1);
        }
    }
}

async fn cmd_session_list(all: bool, project: Option<PathBuf>) -> Result<()> {
    let filter = resolve_session_list_filter(all, project.as_deref())?;
    let project_root = match &filter {
        SessionListFilter::All => None,
        SessionListFilter::Project { canonical_root, .. } => {
            Some(canonical_root.to_string_lossy().into_owned())
        }
    };
    let client = daemon_tui::connect_local_daemon().await?;
    let rows = client.list_sessions(project_root, None, None).await?;
    match &filter {
        SessionListFilter::All => {}
        SessionListFilter::Project { .. } if rows.is_empty() => {
            println!("no sessions match this project. Use --all to list every session.");
            return Ok(());
        }
        SessionListFilter::Project { .. } => {}
    }
    let header_sid = "session_id";
    let header_events = "events";
    let header_messages = "messages";
    let header_status = "status";
    let header_where = "where";
    println!(
        "{header_sid:<38} {header_events:>8} {header_messages:>9} {header_status:<9} {header_where}"
    );
    for summary in rows {
        let where_label = summary
            .project_root
            .as_deref()
            .map(Path::new)
            .map(short_project_path)
            .unwrap_or_else(|| "-".into());
        println!(
            "{:<38} {:>8} {:>9} {:<9} {}",
            summary.id,
            summary.event_count,
            summary.message_count,
            format!("{:?}", summary.status).to_lowercase(),
            where_label
        );
    }
    Ok(())
}

enum SessionListFilter {
    All,
    Project {
        fingerprint: String,
        canonical_root: PathBuf,
    },
}

fn resolve_session_list_filter(all: bool, project: Option<&Path>) -> Result<SessionListFilter> {
    if all {
        return Ok(SessionListFilter::All);
    }
    if let Some(path) = project {
        if !path.exists() {
            bail!("--project path does not exist: {}", path.display());
        }
        return Ok(SessionListFilter::Project {
            fingerprint: atman_runtime::session_meta::fingerprint_from_root(path),
            canonical_root: atman_runtime::session_meta::canonical_root(path),
        });
    }
    let cwd = std::env::current_dir().context("reading cwd")?;
    Ok(SessionListFilter::Project {
        fingerprint: atman_runtime::session_meta::fingerprint_from_root(&cwd),
        canonical_root: atman_runtime::session_meta::canonical_root(&cwd),
    })
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
    let client = daemon_tui::connect_local_daemon().await?;
    let session_id = daemon_tui::resolve_session_prefix(&client, &sid).await?;
    let session = client.attach_session(session_id.clone()).await?;
    let state = session.current();
    let projection = state.projection();
    let summary = client
        .list_sessions(None, Some(session_id.to_string()), None)
        .await?
        .into_iter()
        .find(|summary| summary.id == session_id);
    let mut flow_start = 0;
    let mut flow_end = 0;
    let mut llm_call = 0;
    let events = get_all_session_events(&client, &session_id).await?;
    for envelope in &events {
        match envelope
            .event
            .get("type")
            .and_then(serde_json::Value::as_str)
        {
            Some("flow_start") => flow_start += 1,
            Some("flow_end") => flow_end += 1,
            Some("llm_call") => llm_call += 1,
            _ => {}
        }
    }
    println!("session_id: {session_id}");
    println!("title:      {}", projection.metadata.title);
    println!(
        "project:    {}",
        projection.metadata.project_root.as_deref().unwrap_or("-")
    );
    println!("revision:   {}", projection.revision.0);
    println!("cursor:     {}", state.cursor().0);
    println!("messages:   {}", projection.transcript.len());
    println!("runs:       {}", projection.runs.len());
    println!("resources:  {}", projection.resources.len());
    if let Some(summary) = summary {
        println!("events:     {}", summary.event_count);
    }
    println!("flow_start: {flow_start}");
    println!("flow_end:   {flow_end}");
    println!("llm_call:   {llm_call}");
    Ok(())
}

async fn cmd_session_new() -> Result<()> {
    let client = daemon_tui::connect_local_daemon().await?;
    let project_root = std::env::current_dir()?.to_string_lossy().into_owned();
    let session = client.create_session(Some(project_root), None).await?;
    println!("{}", session.session_id());
    Ok(())
}

async fn cmd_session_move(sid: &str, new_cwd: &Path) -> Result<()> {
    if !new_cwd.is_dir() {
        bail!("not a directory: {}", new_cwd.display());
    }
    let abs_cwd = new_cwd
        .canonicalize()
        .with_context(|| format!("resolve path {}", new_cwd.display()))?;
    let client = daemon_tui::connect_local_daemon().await?;
    let session_id = daemon_tui::resolve_session_prefix(&client, sid).await?;
    let session = client.attach_session(session_id.clone()).await?;
    let moved = session
        .move_to(abs_cwd.to_string_lossy().into_owned())
        .await?;
    println!(
        "session {} moved to {}",
        session_id,
        moved.session.project_root.as_deref().unwrap_or("-")
    );
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
            SessionListFilter::Project { fingerprint, .. } => {
                let scope = root.join("projects").join(fingerprint);
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
        let short_ts = chrono::DateTime::parse_from_rfc3339(&ts)
            .ok()
            .map(|dt| {
                dt.with_timezone(&chrono::Local)
                    .format("%Y-%m-%d %H:%M:%S")
                    .to_string()
            })
            .unwrap_or_else(|| ts.chars().take(19).collect::<String>());
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
    let client = daemon_tui::connect_local_daemon().await?;
    let mut removed = 0usize;
    let mut skipped = 0usize;
    for session in client.list_sessions(None, None, None).await? {
        if session.message_count != 0 {
            continue;
        }
        match client.delete_session(session.id).await?.status {
            atman_proto::SessionDeleteStatus::Deleted
            | atman_proto::SessionDeleteStatus::NotFound => removed += 1,
            atman_proto::SessionDeleteStatus::Busy
            | atman_proto::SessionDeleteStatus::UnsafeResources => skipped += 1,
        }
    }
    println!("gc removed {removed} empty session(s), skipped {skipped} active session(s)");
    Ok(())
}

async fn cmd_session_sanitize(sid: String, dry_run: bool) -> Result<()> {
    let client = daemon_tui::connect_local_daemon().await?;
    let session_id = daemon_tui::resolve_session_prefix(&client, &sid).await?;
    let response = client
        .sanitize_session_attachments(session_id, dry_run)
        .await?;
    if response.issues.is_empty() {
        println!("sanitize: no attachment problems found");
        return Ok(());
    }
    println!(
        "sanitize: found {} attachment issue(s)",
        response.issues.len()
    );
    for issue in &response.issues {
        println!(
            "  {} part_id={} {} → {}",
            issue.context, issue.part_id, issue.file_basename, issue.reason
        );
    }
    if dry_run {
        println!("sanitize: dry-run, no events written");
    } else {
        println!("sanitize: wrote {} degrade event(s)", response.repaired);
    }
    Ok(())
}

async fn consume_provider_catalog_refresh_plan<F, Fut>(plan: Vec<String>, mut refresh_provider: F)
where
    F: FnMut(String) -> Fut,
    Fut: std::future::Future<
            Output = Result<
                atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome,
                atman_runtime::ProviderLifecycleError,
            >,
        >,
{
    for provider_id in plan {
        match refresh_provider(provider_id.clone()).await {
            Ok(
                atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome::NotNeeded
                | atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome::AlreadyInFlight,
            ) => {}
            Ok(
                atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome::CatalogUpdated(
                    delta,
                ),
            ) => atman_runtime::notify!(
                info,
                location = Log,
                "provider catalog `{provider_id}` refreshed: +{} ~{} -{} ({} total)",
                delta.added,
                delta.updated,
                delta.removed,
                delta.total
            ),
            Ok(_) => {}
            Err(
                atman_runtime::ProviderLifecycleError::ProviderNotFound { .. }
                | atman_runtime::ProviderLifecycleError::ProviderDisabled { .. }
                | atman_runtime::ProviderLifecycleError::Stale { .. },
            ) => {}
            Err(error) => atman_runtime::notify!(
                warn,
                location = Log,
                "provider catalog `{provider_id}` refresh failed: {error}"
            ),
        }
    }
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

pub(crate) fn extract_at_paths(line: &str) -> (String, Vec<std::path::PathBuf>) {
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

pub fn load_model_config_from_disk() {
    let Ok(hub) = atman_runtime::config_hub::ConfigHub::global() else {
        return;
    };
    match hub.model_config() {
        Ok(config) => {
            atman_runtime::model_registry::set_provider_config(config.unwrap_or_default())
        }
        Err(error) => {
            atman_runtime::notify!(error, "config.toml reload failed: {error}");
        }
    }
}

fn load_auto_snapshot() -> bool {
    let env_value = std::env::var("ATMAN_AUTO_SNAPSHOT").ok();
    if select_auto_snapshot(env_value.as_deref(), None) {
        return true;
    }
    atman_runtime::config_hub::ConfigHub::global()
        .and_then(|hub| hub.auto_snapshot())
        .ok()
        .flatten()
        .unwrap_or(false)
}

fn select_auto_snapshot(env_value: Option<&str>, config_value: Option<bool>) -> bool {
    env_value.is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
        || config_value.unwrap_or(false)
}

fn auto_snapshot_flows(source_path: &Path, source: &str, parsed: &atman_dsl::ast::File) {
    let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let registry = match atman_runtime::flow_registry::FlowRegistry::open(&project_root) {
        Ok(r) => r,
        Err(e) => {
            atman_runtime::notify!(error, "auto_snapshot: open registry failed: {e}");
            return;
        }
    };
    let meta = match atman_runtime::flow_meta::FlowMeta::from_source(source_path, source) {
        Ok(m) => m,
        Err(e) => {
            atman_runtime::notify!(error, "auto_snapshot: read meta failed: {e}");
            return;
        }
    };
    for flow in &parsed.flows {
        let name = &flow.name.name;
        match registry.snapshot(name, source, &meta, Some(source_path)) {
            Ok(atman_runtime::flow_registry::SnapshotOutcome::Inserted(rev)) => {
                atman_runtime::notify!(
                    debug,
                    "auto_snapshot: {name} @ {} (id={})",
                    rev.version,
                    rev.id
                )
            }
            Ok(atman_runtime::flow_registry::SnapshotOutcome::UnchangedFromLatest(_)) => {}
            Err(e) => atman_runtime::notify!(error, "auto_snapshot: {name}: {e}"),
        }
    }
}

async fn cmd_cost(session_id: Option<String>, all: bool) -> Result<()> {
    let client = daemon_tui::connect_local_daemon().await?;
    if all {
        return cmd_cost_all(&client).await;
    }
    let sid = resolve_daemon_session(&client, session_id).await?;
    let events = get_all_session_events(&client, &sid).await?;
    let summary = aggregate_cost_events(events.iter().map(|event| &event.event));
    print_cost_summary(&format!("session {sid}"), &summary);
    Ok(())
}

async fn cmd_cost_all(client: &atman_client::Client) -> Result<()> {
    let mut per_session: Vec<(String, CostSummary)> = Vec::new();
    let mut combined = CostSummary::default();
    let mut sessions_walked = 0u64;
    for session in client.list_sessions(None, None, None).await? {
        let sid = session.id.to_string();
        let events = get_all_session_events(client, &session.id).await?;
        let summary = aggregate_cost_events(events.iter().map(|event| &event.event));
        if summary.total_calls == 0 {
            continue;
        }
        sessions_walked += 1;
        combined.merge(&summary);
        per_session.push((sid, summary));
    }
    if per_session.is_empty() {
        println!("[atman] cost --all: no llm_call events found");
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

fn aggregate_cost_events<'a>(
    events: impl IntoIterator<Item = &'a serde_json::Value>,
) -> CostSummary {
    let mut summary = CostSummary::default();
    for v in events {
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
    let rep = atman_runtime::config_init::init_config_dir_with_mode(&cfg, fs_access)?;
    if rep.written.is_empty() && rep.skipped.len() == 4 {
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
                let tag = if rep.managed.contains(p) {
                    "  *"
                } else {
                    "  +"
                };
                println!("{tag} {}", rel.display());
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
    println!(
        "Note: commands/agent.at is managed by atman and refreshed when bundled content changes. Do not edit it."
    );
    println!("To customize behavior, create your own .at file and route to it from routes.at.");
    println!();
    println!("next steps:");
    let cfg_dir = config_dir().ok();
    let cfg_file = cfg_dir
        .as_ref()
        .map(|d| d.join("config.toml").display().to_string())
        .unwrap_or_else(|| "<config_dir>/config.toml".to_string());
    println!("  1. set an api key:     export ANTHROPIC_API_KEY=...");
    println!("     (or put api_key = \"sk-...\" in {})", cfg_file);
    println!("  2. verify setup:       atman doctor");
    println!("  3. launch:             atman");
    println!();
    println!("config: {}", cfg_file);
    println!("docs:   https://atman.run");
    if fs_access.is_none() {
        println!();
        println!("fs access: workspace-write (default). override with ATMAN_FS_ACCESS.");
    }
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
    ("notify", "all 5 levels: inline + toast + status demo"),
    ("workflow-running", "long-lived workflow with nested nodes"),
    ("workflow-cancelled", "workflow ended with Err cascade"),
    ("compact-review", "post-compaction summary review modal"),
    (
        "floating-panel",
        "floating panels: bash + terminal + history",
    ),
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
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<atman_tui::TuiCommand>();
    let mut handle = atman_tui::TuiHandle::from_session(session.clone());
    handle.control_tx = Some(ctrl_tx);
    handle.cmd_rx = Some(cmd_rx);
    let ctrl_session = session.clone();
    let ctrl_task = tokio::spawn(async move {
        while let Some(msg) = ctrl_rx.recv().await {
            match msg {
                atman_tui::TuiControl::Domain(command) => match command {
                    atman_tui::TuiDomainCommand::ResolvePermission {
                        selector,
                        expected_revision,
                        action,
                        grant_scope,
                        reason,
                    } => {
                        let result = match selector {
                            atman_runtime::permission::PermissionSelector::RequestIds(ids) => {
                                let expected = ids
                                    .iter()
                                    .cloned()
                                    .map(|id| (id, expected_revision))
                                    .collect();
                                ctrl_session.permission_broker().user_resolve(
                                    &ctrl_session.id().to_string(),
                                    Some("local-tui-preview".into()),
                                    ids,
                                    &expected,
                                    None,
                                    action,
                                    grant_scope,
                                    reason,
                                )
                            }
                            atman_runtime::permission::PermissionSelector::Group(group_id) => {
                                ctrl_session.permission_broker().user_resolve(
                                    &ctrl_session.id().to_string(),
                                    Some("local-tui-preview".into()),
                                    Vec::new(),
                                    &std::collections::HashMap::new(),
                                    Some((group_id, expected_revision)),
                                    action,
                                    grant_scope,
                                    reason,
                                )
                            }
                            _ => Err(
                                atman_runtime::permission::PermissionError::GroupRevisionConflict,
                            ),
                        };
                        if let Err(error) = result {
                            atman_runtime::notify!(error, "permission decision rejected: {error}");
                        }
                    }
                    atman_tui::TuiDomainCommand::FormSubmit {
                        form_id,
                        submission,
                    } => {
                        ctrl_session.forms().submit(&form_id, submission);
                    }
                    atman_tui::TuiDomainCommand::CompactReviewAccept { review_id, edited } => {
                        let decision = match edited {
                            Some(summary) => {
                                atman_runtime::CompactReviewDecision::AcceptEdited { summary }
                            }
                            None => atman_runtime::CompactReviewDecision::AcceptAsIs,
                        };
                        ctrl_session.compact_reviews().decide(&review_id, decision);
                    }
                    atman_tui::TuiDomainCommand::CompactReviewReject { review_id } => {
                        ctrl_session
                            .compact_reviews()
                            .decide(&review_id, atman_runtime::CompactReviewDecision::Reject);
                    }
                    _ => {}
                },
                atman_tui::TuiControl::MutateProvider(request) => {
                    let _ = cmd_tx.send(atman_tui::TuiCommand::ProviderMutationResult {
                        request,
                        result: Err("provider mutation is unavailable in TUI preview".into()),
                    });
                }
                atman_tui::TuiControl::SwitchModel { request_id, model } => {
                    let _ = cmd_tx.send(atman_tui::TuiCommand::ModelSwitchResult {
                        request_id,
                        model,
                        result: Err("model switching is unavailable in TUI preview".into()),
                    });
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
            "notify" => {
                preview_scene_notify(session).await;
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
            "floating-panel" => {
                preview_scene_floating_panel(session).await;
                true
            }
            _ => false,
        };
        if !ok {
            atman_runtime::notify!(
                warn,
                "unknown tui-preview scene: {scene}. try `atman tui-preview list`."
            );
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
        run_id: None,
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
            run_id: None,
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
            run_id: None,
        });
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let _ = tx.send(StreamFrame::LlmDone {
        total_tokens: 48,
        run_id: None,
    });
}

async fn preview_scene_approval(session: std::sync::Arc<Session>, count: usize) {
    use atman_runtime::event::FlowRunId;
    use atman_runtime::nodegraph::NodeKind;
    use atman_runtime::permission::PermissionRequestId;
    use atman_runtime::permission_audit::{
        PermissionAuditTarget, PermissionPolicyReference, PermissionProvenanceSummary,
        PermissionRequestAudit,
    };
    use atman_runtime::stream::StreamFrame;
    use atman_runtime::tool::Tier;
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let tx = session.stream_tx();
    let run_id = FlowRunId::now().0.to_string();
    let _ = tx.send(StreamFrame::FlowStart {
        run_id: run_id.clone(),
        flow_name: "demo".into(),
        parent_run_id: None,
        parent_node_id: None,
    });
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
            call_intent: None,
        });
        let flow_run_id = FlowRunId(uuid::Uuid::parse_str(&run_id).unwrap());
        let _ = tx.send(StreamFrame::PermissionRequestCreated {
            run_id: run_id.clone(),
            payload: PermissionRequestAudit {
                request_id: Some(PermissionRequestId::now()),
                revision: 1,
                session_id: session.id().to_string(),
                requesting_run_id: flow_run_id.clone(),
                parent_run_id: None,
                root_run_id: flow_run_id,
                tool_use_id,
                tool: (*tool).into(),
                call_intent: None,
                tier: Tier::Four,
                execution_boundary: Default::default(),
                provenance: PermissionProvenanceSummary {
                    targets: vec![(*args).into()],
                    ..PermissionProvenanceSummary::default()
                },
                target: PermissionAuditTarget::User,
                group_ids: Vec::new(),
                policy: PermissionPolicyReference {
                    snapshot_id: "preview".into(),
                    rule_id: "preview".into(),
                },
                escalation_path: Vec::new(),
                decision_id: None,
                actor: None,
                scope: None,
                reason: Some((*preview).into()),
                at: chrono::Utc::now(),
            },
        });
    }
}

async fn preview_scene_form(session: std::sync::Arc<Session>, kind: atman_runtime::form::FormKind) {
    use atman_runtime::event::FlowRunId;
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let form = atman_runtime::form::CompositeForm {
        questions: vec![atman_runtime::form::FormQuestion {
            id: "question".into(),
            kind: kind.clone(),
        }],
    };
    let _rx = session.forms().request(atman_runtime::form::PendingForm {
        form_id: "preview_form".into(),
        run_id: FlowRunId::now(),
        tool_use_id: "preview_form_tool".into(),
        form,
        kind,
        emitted_at: chrono::Utc::now(),
    });
    let _ = _rx.await;
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
                form: atman_runtime::form::CompositeForm {
                    questions: vec![atman_runtime::form::FormQuestion {
                        id: "question".into(),
                        kind: kind.clone(),
                    }],
                },
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

async fn preview_scene_notify(session: std::sync::Arc<Session>) {
    use atman_runtime::notify::{NotifyLevel, NotifyLifecycle, NotifyLocation};
    use atman_runtime::stream::{NotificationFrame, StreamFrame};
    let tx = session.stream_tx();

    let send = |level, location, message: &str| {
        let _ = tx.send(StreamFrame::Notification(NotificationFrame {
            run_id: None,
            level,
            location,
            lifecycle: NotifyLifecycle::Persistent,
            stack: atman_runtime::notify::NotifyStack::Append,
            message: message.into(),
        }));
    };

    // Inline notes — all 5 levels
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    send(
        NotifyLevel::Error,
        NotifyLocation::Inline,
        "connection to provider lost — retrying in 3s…",
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    send(
        NotifyLevel::Warn,
        NotifyLocation::Inline,
        "project index unavailable — history search disabled",
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    send(
        NotifyLevel::Info,
        NotifyLocation::Inline,
        "requested transcript compaction (847 → 142 messages)",
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    send(
        NotifyLevel::Success,
        NotifyLocation::Inline,
        "copied 247 chars to clipboard",
    );
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    send(
        NotifyLevel::Debug,
        NotifyLocation::Inline,
        "auto_snapshot: agent @ 1.2.0 (id=a3f8b1c2)",
    );

    // Toast notifications
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let _ = tx.send(StreamFrame::Notification(NotificationFrame {
        run_id: None,
        level: NotifyLevel::Success,
        location: NotifyLocation::Toast,
        lifecycle: NotifyLifecycle::Ttl(std::time::Duration::from_secs(3)),
        stack: atman_runtime::notify::NotifyStack::Append,
        message: "todo marked done: add notify module".into(),
    }));
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let _ = tx.send(StreamFrame::Notification(NotificationFrame {
        run_id: None,
        level: NotifyLevel::Info,
        location: NotifyLocation::Toast,
        lifecycle: NotifyLifecycle::Ttl(std::time::Duration::from_secs(3)),
        stack: atman_runtime::notify::NotifyStack::Append,
        message: "checkpoint saved at seq 1842".into(),
    }));
    tokio::time::sleep(std::time::Duration::from_millis(400)).await;
    let _ = tx.send(StreamFrame::Notification(NotificationFrame {
        run_id: None,
        level: NotifyLevel::Warn,
        location: NotifyLocation::Toast,
        lifecycle: NotifyLifecycle::Ttl(std::time::Duration::from_secs(5)),
        stack: atman_runtime::notify::NotifyStack::Dedupe {
            key: "rate-limit".into(),
            window: std::time::Duration::from_secs(30),
        },
        message: "rate limit approaching (48/50 rpm)".into(),
    }));

    // Status bar notes — appear in bottom bar
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let _ = tx.send(StreamFrame::Notification(NotificationFrame {
        run_id: None,
        level: NotifyLevel::Info,
        location: NotifyLocation::Status,
        lifecycle: NotifyLifecycle::UntilReplaced,
        stack: atman_runtime::notify::NotifyStack::Replace {
            key: "daemon".into(),
        },
        message: "daemon connected".into(),
    }));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let _ = tx.send(StreamFrame::Notification(NotificationFrame {
        run_id: None,
        level: NotifyLevel::Warn,
        location: NotifyLocation::Status,
        lifecycle: NotifyLifecycle::UntilReplaced,
        stack: atman_runtime::notify::NotifyStack::Replace {
            key: "compact".into(),
        },
        message: "compacting… 847 → 142 messages".into(),
    }));

    // Modal — critical error inline
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let _ = tx.send(StreamFrame::Notification(NotificationFrame {
        run_id: None,
        level: NotifyLevel::Error,
        location: NotifyLocation::Modal,
        lifecycle: NotifyLifecycle::Persistent,
        stack: atman_runtime::notify::NotifyStack::Append,
        message: "authentication failed — Codex token expired".into(),
    }));
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
            cancelled: false,
            suicide: false,
        });
    }
}

async fn preview_scene_compact_review(session: std::sync::Arc<Session>) {
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let _ = session
        .compact_reviews()
        .request(atman_runtime::session::PendingCompactReview {
            review_id: "preview_review".into(),
            context_id: session.context().context_id().cloned(),
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
        })
        .await;
}

async fn preview_scene_floating_panel(session: std::sync::Arc<Session>) {
    use atman_runtime::event::{FlowNodeStatus, FlowRunId};
    use atman_runtime::nodegraph::NodeKind;
    use atman_runtime::stream::StreamFrame;
    use atman_runtime::tools::term::{TerminalCell, TerminalColor, TerminalScreen};

    let tx = session.stream_tx();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;

    // 1. Bash task with output
    let bash_handle = "bg_s_demo_1".to_string();
    let _ = tx.send(StreamFrame::BashChunk {
        handle: bash_handle.clone(),
        kind: "stdout".into(),
        line: "$ cargo test --workspace\n".into(),
        call_intent: None,
        tool_use_id: None,
        run_id: None,
    });
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let _ = tx.send(StreamFrame::BashChunk {
        handle: bash_handle.clone(),
        kind: "stdout".into(),
        line: "    Finished test [unoptimized + debuginfo] target(s) in 0.52s\n".into(),
        call_intent: None,
        tool_use_id: None,
        run_id: None,
    });
    let _ = tx.send(StreamFrame::BashChunk {
        handle: bash_handle.clone(),
        kind: "stdout".into(),
        line: "     Running unittests src/lib.rs\n".into(),
        call_intent: None,
        tool_use_id: None,
        run_id: None,
    });
    let _ = tx.send(StreamFrame::BashChunk {
        handle: bash_handle.clone(),
        kind: "stdout".into(),
        line: "running 258 tests\ntest result: ok. 258 passed; 0 failed\n".into(),
        call_intent: None,
        tool_use_id: None,
        run_id: None,
    });

    // 2. Terminal task with screen
    let term_handle = "term_s_demo_1".to_string();
    let cols = 80u16;
    let rows = 24u16;
    let cells: Vec<TerminalCell> = (0..(rows as usize * cols as usize))
        .map(|i| {
            let row = i / cols as usize;
            let col = i % cols as usize;
            let chars = if row == 0 && col < 4 {
                "vim ".to_string()
            } else if (2..22).contains(&row) && col < 1 {
                "~".to_string()
            } else if row == 22 && col < 20 {
                "test.sh - 1 line 1:1".to_string()
            } else {
                " ".to_string()
            };
            TerminalCell {
                chars,
                fg: TerminalColor::Default,
                bg: TerminalColor::Default,
                bold: false,
                italic: false,
                underline: false,
                inverse: false,
                dim: false,
                wide: false,
                wide_continuation: false,
            }
        })
        .collect();
    let screen = TerminalScreen {
        rows,
        cols,
        cells,
        cursor: Some((0, 0)),
        alt_screen: false,
    };
    let _ = tx.send(StreamFrame::TerminalChunk {
        handle: term_handle.clone(),
        bytes: vec![],
        screen: Some(screen),
        state: atman_runtime::tools::term::TermStateSnapshot::Running,
        call_intent: None,
        tool_use_id: None,
        run_id: None,
    });

    // 3. Flow with activity nodes
    let run_id = FlowRunId::now().0.to_string();
    let _ = tx.send(StreamFrame::FlowStart {
        run_id: run_id.clone(),
        flow_name: "agent".into(),
        parent_run_id: None,
        parent_node_id: None,
    });
    let _ = tx.send(StreamFrame::FlowNodeStart {
        run_id: run_id.clone(),
        node_id: "stmt_0".into(),
        kind: NodeKind::Llm {
            model: Some("demo".into()),
        },
        label: "analyze task".into(),
        parent_node_id: None,
    });
    let _ = tx.send(StreamFrame::FlowNodeEnd {
        run_id: run_id.clone(),
        node_id: "stmt_0".into(),
        status: FlowNodeStatus::Ok,
        output_preview: Some("done".into()),
        parent_node_id: None,
    });
    let _ = tx.send(StreamFrame::FlowNodeStart {
        run_id: run_id.clone(),
        node_id: "stmt_1".into(),
        kind: NodeKind::ToolCall {
            path: "fs.read".into(),
        },
        label: "read src/lib.rs".into(),
        parent_node_id: None,
    });

    // 4. System note
    let _ = tx.send(StreamFrame::Note(
        "Click any task in the left panel to open a floating panel. Try dragging, resizing, and the buttons.".into()
    ));

    // keep alive
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    }
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
            match std::fs::write(&cfg_file, atman_runtime::config_init::CONFIG_TOML) {
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

    let model_config = atman_runtime::config_hub::ConfigHub::global()
        .and_then(|hub| hub.model_config())
        .ok()
        .flatten();
    if let Some(mc) = model_config {
        println!("models:");
        for (name, entry) in &mc.models {
            let budget = entry
                .context_budget
                .map(|b| b.to_string())
                .unwrap_or_else(|| "builtin".into());
            let reasoning = atman_runtime::model_registry::model_info(name)
                .reasoning
                .to_string();
            println!("  {name:<28} budget={budget} reasoning={reasoning}");
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
            let skill_count = rules.iter().filter(|r| r.source_tool == "skill").count();
            for r in &rules {
                let scope = match r.scope {
                    atman_runtime::migration::RuleScope::Project => "project",
                    atman_runtime::migration::RuleScope::Global => "global ",
                };
                let desc = r
                    .description
                    .as_deref()
                    .map(|d| format!(" — {d}"))
                    .unwrap_or_default();
                println!(
                    "  [✓] {:<30} [{:<8}] {} — {}{}",
                    r.name,
                    r.source_tool,
                    scope,
                    r.source_path.display(),
                    desc,
                );
            }
            println!("  skills: {skill_count} referenced rule(s) from ~/.claude/skills/");
        }
    } else {
        println!("  (HOME env not set)");
    }
    println!();
    println!("confessions:");
    match atman_runtime::storage::resolve_project_scope_for(&project_root) {
        Ok(scope) => {
            let conf_dir = scope.join("confessions");
            if conf_dir.exists() {
                let count = std::fs::read_dir(&conf_dir)
                    .map(|it| {
                        it.filter_map(|e| e.ok())
                            .filter(|e| {
                                e.path()
                                    .extension()
                                    .and_then(|s| s.to_str())
                                    .map(|s| s == "md")
                                    .unwrap_or(false)
                            })
                            .count()
                    })
                    .unwrap_or(0);
                println!("  [✓] {} — {count} confession(s)", conf_dir.display());
            } else {
                println!("  (none — {} missing)", conf_dir.display());
            }
        }
        Err(e) => println!("  [✗] resolve scope failed: {e}"),
    }
    println!();
    println!("mcp:");
    let mcp_configs = load_mcp_configs();
    if mcp_configs.is_empty() {
        println!("  (none configured — add [[mcp]] blocks to config.toml)");
    } else {
        let probe_registry = atman_runtime::ToolRegistry::new();
        let statuses =
            atman_runtime::mcp::register_from_configs(&probe_registry, &mcp_configs).await;
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
                atman_runtime::mcp::TransportKind::Sse => {
                    format!("sse: {}", cfg.url.as_deref().unwrap_or("<missing url>"))
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
    static WORKSPACE_GENERATION: OnceLock<String> = OnceLock::new();

    let project_root = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let home_dir = std::env::var("HOME").ok().map(std::path::PathBuf::from);
    let config_dir = config_dir().ok();
    let workspace_generation = WORKSPACE_GENERATION
        .get_or_init(|| uuid::Uuid::now_v7().to_string())
        .clone();
    Ok(atman_daemon::bootstrap::BootstrapOptions {
        events,
        task_registry: atman_runtime::TaskRegistry::new(),
        mock,
        config_dir,
        project_root,
        home_dir,
        workspace_generation,
    })
}

fn open_project_index(
    scope: &Path,
) -> Result<Option<std::sync::Arc<atman_runtime::index::AnchorIndex>>> {
    match atman_runtime::index::AnchorIndex::open_project(scope) {
        Ok(idx) => Ok(Some(std::sync::Arc::new(idx))),
        Err(e) => {
            atman_runtime::notify!(
                warn,
                "project index unavailable at {} — history search disabled: {e}",
                scope.display()
            );
            Ok(None)
        }
    }
}

fn open_current_project_index() -> Result<Option<std::sync::Arc<atman_runtime::index::AnchorIndex>>>
{
    let scope = atman_runtime::storage::resolve_current_project_scope()
        .context("resolve project storage scope")?;
    open_project_index(&scope)
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
            scope.clone(),
            open_project_index(&scope)?,
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

fn load_mcp_configs() -> Vec<atman_runtime::mcp::McpServerConfig> {
    atman_runtime::config_hub::ConfigHub::global()
        .map(|hub| hub.load_mcp())
        .unwrap_or_default()
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
            let client = daemon_tui::connect_local_daemon().await?;
            let session = client
                .create_session(
                    Some(std::env::current_dir()?.to_string_lossy().into_owned()),
                    None,
                )
                .await?;
            let sid = session.session_id().clone();
            let imported = messages
                .iter()
                .map(|message| {
                    let text = if let Some(agent) = &message.agent {
                        format!(
                            "[migrated from {}, agent={agent}]\n{}",
                            source.source_tag(),
                            message.text
                        )
                    } else {
                        format!("[migrated from {}]\n{}", source.source_tag(), message.text)
                    };
                    let role = match message.role {
                        migrate_source::MessageRole::User => atman_proto::MessageRole::User,
                        migrate_source::MessageRole::Assistant => {
                            atman_proto::MessageRole::Assistant
                        }
                        migrate_source::MessageRole::System => atman_proto::MessageRole::System,
                        migrate_source::MessageRole::Tool => atman_proto::MessageRole::Tool,
                    };
                    atman_proto::ImportedMessage { role, text }
                })
                .collect();
            client
                .import_session_messages(sid.clone(), imported)
                .await?;
            println!(
                "[atman] migrate: replayed {} messages from {from}/{resolved_id} into new session {sid}",
                messages.len()
            );
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

    let ex = atman_runtime::Executor::new();
    atman_runtime::tools::register_tier_zero(&ex.tools);
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
            atman_runtime::notify!(error, "flow test: {name} raised {msg}");
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
        atman_runtime::notify!(
            info,
            "note: {} lives inside git repo at {}. `git checkout <sha> -- {}` may be a safer rollback path.",
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
        atman_runtime::notify!(
            error,
            "refusing to overwrite {} without --yes (would replace with {} @ {}, id={})",
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
    let client = connect_http_daemon(port).await?;
    let base = format!("http://127.0.0.1:{port}");
    let sid = resolve_daemon_session(&client, session_id).await?;
    atman_runtime::notify!(info, "streaming events for session {sid} from {base}");
    let mut cursor = since_seq.unwrap_or(0);
    loop {
        let page = client.get_events(sid.clone(), Some(cursor)).await?;
        if page.has_more && page.next_cursor.0 <= cursor {
            bail!("daemon event page did not advance beyond cursor {cursor}");
        }
        cursor = page.next_cursor.0;
        if page.events.is_empty() {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            continue;
        }
        for event in page.events {
            println!("{}", serde_json::to_string(&event)?);
        }
    }
}

async fn connect_http_daemon(port: u16) -> Result<atman_client::Client> {
    let cfg_path = atman_daemon::config::default_config_path()?;
    let cfg = atman_runtime::config_hub::ConfigHub::from_daemon_config_path(&cfg_path)
        .load_or_init_daemon_config()?;
    let base = format!("http://127.0.0.1:{port}");
    atman_client::Client::connect(
        atman_client::HttpTransport::new(&base, &cfg.auth_token)?,
        atman_client::ClientIdentity::new("atman-cli", env!("CARGO_PKG_VERSION")),
    )
    .await
    .with_context(|| format!("connect to {base} (is atman-daemon running?)"))
}

async fn cmd_logs_tail(session_id: Option<String>, n: usize, follow: bool) -> Result<()> {
    let client = daemon_tui::connect_local_daemon().await?;
    let sid = resolve_daemon_session(&client, session_id).await?;
    let events = get_all_session_events(&client, &sid).await?;
    let start = events.len().saturating_sub(n);
    for event in &events[start..] {
        println!("{}", serde_json::to_string(&event.event)?);
    }

    let mut cursor = events.last().map(|event| event.cursor.0).unwrap_or(0);
    if follow {
        loop {
            let page = client.get_events(sid.clone(), Some(cursor)).await?;
            if page.events.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
            for event in page.events {
                cursor = event.cursor.0;
                println!("{}", serde_json::to_string(&event.event)?);
            }
        }
    }
    Ok(())
}

async fn resolve_daemon_session(
    client: &atman_client::Client,
    session_id: Option<String>,
) -> Result<atman_proto::SessionId> {
    match session_id {
        Some(session_id) => daemon_tui::resolve_session_prefix(client, &session_id).await,
        None => client
            .list_sessions(None, None, Some(1))
            .await?
            .into_iter()
            .next()
            .map(|session| session.id)
            .context("no sessions found"),
    }
}

async fn get_all_session_events(
    client: &atman_client::Client,
    session_id: &atman_proto::SessionId,
) -> Result<Vec<atman_proto::ServerEventEnvelope>> {
    let mut events = Vec::new();
    let mut cursor = 0;
    loop {
        let page = client.get_events(session_id.clone(), Some(cursor)).await?;
        if page.has_more && page.next_cursor.0 <= cursor {
            bail!("daemon event page did not advance beyond cursor {cursor}");
        }
        cursor = page.next_cursor.0;
        events.extend(page.events);
        if !page.has_more {
            return Ok(events);
        }
    }
}

fn data_dir() -> Result<PathBuf> {
    atman_runtime::storage::data_dir()
}

fn config_dir() -> Result<PathBuf> {
    atman_runtime::storage::config_dir()
}

fn load_global_trust_config() -> Result<atman_runtime::trust::TrustConfig> {
    atman_runtime::config_hub::ConfigHub::global()?
        .trust_config()
        .context("load global trust config")
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

async fn cmd_mcp(action: McpAction) -> anyhow::Result<()> {
    use std::io::Write as _;
    let client = daemon_tui::connect_local_daemon().await?;
    match action {
        McpAction::List => {
            let configs = client.list_mcp_servers().await?.servers;
            if configs.is_empty() {
                println!("  (no MCP servers configured)");
                println!();
                println!("  Add servers with:");
                println!("    atman mcp add");
                println!("    atman mcp add --template filesystem");
                return Ok(());
            }
            println!(
                "  {:<20} {:<8} {:<30} tier",
                "name", "transport", "command / url"
            );
            println!("  {}", "─".repeat(70));
            for cfg in &configs {
                let source = if cfg.command.is_empty() {
                    cfg.url.as_deref().unwrap_or("<missing url>").to_string()
                } else {
                    let mut s = format!("{} {}", cfg.command, cfg.args.join(" "));
                    if cfg.env_count != 0 {
                        s.push_str(&format!(" (env: {} keys)", cfg.env_count));
                    }
                    s
                };
                println!(
                    "  {:<20} {:<8} {:<30} {}{}",
                    cfg.name,
                    cfg.transport,
                    source.chars().take(30).collect::<String>(),
                    cfg.tier,
                    if cfg.disabled { " (disabled)" } else { "" }
                );
            }
        }
        McpAction::Add { template } => {
            if let Some(tmpl_name) = template {
                let Some(tmpl) = mcp_templates::find(&tmpl_name) else {
                    anyhow::bail!(
                        "unknown template '{}'; available: {}",
                        tmpl_name,
                        mcp_templates::TEMPLATES
                            .iter()
                            .map(|t| t.name)
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                };
                println!("Template: {} — {}", tmpl.name, tmpl.description);
                let mut args: Vec<String> = tmpl.args.iter().map(|s| s.to_string()).collect();
                if let Some(placeholder) = tmpl.path_placeholder {
                    print!("Path for {}: ", placeholder);
                    std::io::stdout().flush()?;
                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    let path = input.trim();
                    if !path.is_empty() {
                        args.push(path.to_string());
                    }
                }
                let mut env: Vec<(String, String)> = Vec::new();
                for key in tmpl.env_keys {
                    print!("Enter value for {key} (required): ");
                    std::io::stdout().flush()?;
                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    let val = input.trim();
                    if val.is_empty() {
                        anyhow::bail!("{key} is required");
                    }
                    env.push((key.to_string(), val.to_string()));
                }
                let name = tmpl.name.to_string();
                let mut config = atman_runtime::mcp::McpServerConfig::stdio(
                    &name,
                    tmpl.command,
                    args,
                    atman_runtime::Tier::Three,
                    30_000,
                );
                config.env = env;
                client.upsert_mcp_server(mcp_server_input(config)).await?;
                println!("✓ Added MCP server \"{}\" to mcp_servers.json", name);
            } else {
                let server = prompt_mcp_server()?;
                let name = server.name.clone();
                client.upsert_mcp_server(mcp_server_input(server)).await?;
                println!("✓ Added MCP server \"{}\" to mcp_servers.json", name);
                println!("  Reload MCP in active sessions to apply.");
            }
        }
        McpAction::Remove { name } => {
            client.remove_mcp_server(name.clone()).await?;
            println!("✓ Removed MCP server \"{}\"", name);
        }
        McpAction::Test { name } => {
            print!("Connecting to {}... ", name);
            std::io::stdout().flush()?;
            let result = client.probe_mcp(name.clone()).await?;
            if result.ok {
                println!("✓ {}", result.message);
                if let Ok(response) = client.list_mcp_tools(name).await {
                    for tool in response.tools {
                        println!("  - {}", tool.name);
                    }
                }
            } else {
                println!("✗ {}", result.message);
            }
        }
        McpAction::Tools { name } => {
            let tools = client.list_mcp_tools(name.clone()).await?.tools;
            println!("Tools from {} ({}):", name, tools.len());
            for tool in tools {
                println!(
                    "  - {} — {}",
                    tool.name,
                    tool.description.as_deref().unwrap_or("(no description)")
                );
            }
        }
        McpAction::Resources { name } => match client.list_mcp_resources(name.clone()).await {
            Ok(resources) => {
                println!("Resources from {} ({}):", name, resources.resources.len());
                for r in &resources.resources {
                    println!("  - {} ({})", r.uri, r.name);
                    if let Some(desc) = &r.description {
                        println!("      {}", desc);
                    }
                }
            }
            Err(e) => println!("  (resources not supported: {e})"),
        },
        McpAction::Prompts { name } => match client.list_mcp_prompts(name.clone()).await {
            Ok(prompts) => {
                println!("Prompts from {} ({}):", name, prompts.prompts.len());
                for p in &prompts.prompts {
                    println!(
                        "  - {} — {}",
                        p.name,
                        p.description.as_deref().unwrap_or("(no description)")
                    );
                    for arg in &p.arguments {
                        println!(
                            "      arg: {}{} — {}",
                            arg.name,
                            if arg.required { " (required)" } else { "" },
                            arg.description.as_deref().unwrap_or("")
                        );
                    }
                }
            }
            Err(e) => println!("  (prompts not supported: {e})"),
        },
        McpAction::Import { file } => {
            let text = std::fs::read_to_string(&file)
                .map_err(|e| anyhow::anyhow!("read {}: {e}", file.display()))?;
            let servers = atman_runtime::mcp_config::parse_mcp_json(&text);
            if servers.is_empty() {
                println!("No MCP servers found in {}", file.display());
                return Ok(());
            }
            let imported = servers.len();
            for server in servers {
                println!("✓ Imported \"{}\"", server.name);
                client.upsert_mcp_server(mcp_server_input(server)).await?;
            }
            println!("Imported {imported} servers");
        }
    }
    Ok(())
}

fn prompt_mcp_server() -> anyhow::Result<atman_runtime::mcp::McpServerConfig> {
    use std::io::Write;
    print!("Server name: ");
    std::io::stdout().flush()?;
    let mut name = String::new();
    std::io::stdin().read_line(&mut name)?;
    let name = name.trim().to_string();
    if name.is_empty() {
        anyhow::bail!("name is required");
    }

    print!("Transport (stdio/http/sse) [stdio]: ");
    std::io::stdout().flush()?;
    let mut transport = String::new();
    std::io::stdin().read_line(&mut transport)?;
    let transport = transport.trim();
    let transport = if transport.is_empty() {
        "stdio"
    } else {
        transport
    };

    let server = match transport {
        "stdio" => {
            print!("Command: ");
            std::io::stdout().flush()?;
            let mut command = String::new();
            std::io::stdin().read_line(&mut command)?;
            let command = command.trim().to_string();
            if command.is_empty() {
                anyhow::bail!("command is required");
            }

            print!("Args (space-separated): ");
            std::io::stdout().flush()?;
            let mut args = String::new();
            std::io::stdin().read_line(&mut args)?;

            print!("Env vars (KEY=value, comma-separated, optional): ");
            std::io::stdout().flush()?;
            let mut env_input = String::new();
            std::io::stdin().read_line(&mut env_input)?;
            let env = env_input
                .split(',')
                .filter_map(|pair| pair.trim().split_once('='))
                .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
                .collect();
            let mut config = atman_runtime::mcp::McpServerConfig::stdio(
                &name,
                command,
                args.split_whitespace().map(String::from).collect(),
                atman_runtime::Tier::Three,
                30_000,
            );
            config.env = env;
            config
        }
        "http" | "sse" => {
            print!("URL: ");
            std::io::stdout().flush()?;
            let mut url = String::new();
            std::io::stdin().read_line(&mut url)?;
            let url = url.trim().to_string();
            if url.is_empty() {
                anyhow::bail!("url is required");
            }

            print!("Auth token (optional): ");
            std::io::stdout().flush()?;
            let mut token = String::new();
            std::io::stdin().read_line(&mut token)?;
            let token = (!token.trim().is_empty()).then(|| token.trim().to_string());
            if transport == "http" {
                atman_runtime::mcp::McpServerConfig::http(
                    &name,
                    url,
                    token,
                    atman_runtime::Tier::Three,
                    30_000,
                )
            } else {
                atman_runtime::mcp::McpServerConfig::sse(
                    &name,
                    url,
                    token,
                    atman_runtime::Tier::Three,
                    30_000,
                )
            }
        }
        other => anyhow::bail!("unknown transport '{other}'; use stdio/http/sse"),
    };

    Ok(server)
}

fn mcp_server_input(config: atman_runtime::mcp::McpServerConfig) -> atman_proto::McpServerInput {
    let transport = match config.transport {
        atman_runtime::mcp::TransportKind::Stdio => "stdio",
        atman_runtime::mcp::TransportKind::Http => "http",
        atman_runtime::mcp::TransportKind::Sse => "sse",
    };
    let tier = match config.tier {
        atman_runtime::Tier::Zero => 0,
        atman_runtime::Tier::One => 1,
        atman_runtime::Tier::Two => 2,
        atman_runtime::Tier::Three => 3,
        atman_runtime::Tier::Four => 4,
    };
    let key_values = |values: Vec<(String, String)>| {
        values
            .into_iter()
            .map(|(name, value)| atman_proto::McpKeyValue { name, value })
            .collect()
    };
    atman_proto::McpServerInput {
        name: config.name,
        transport: transport.into(),
        command: config.command,
        args: config.args,
        env: key_values(config.env),
        url: config.url,
        auth_token: config.auth_token,
        headers: key_values(config.headers),
        tier,
        timeout_ms: config.timeout_ms,
        disabled: config.disabled,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn daemon_command_uses_self_hosted_service_without_a_sibling_binary() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("atman");

        let command = daemon_process_command(&current);

        assert_eq!(command.get_program(), current.as_os_str());
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                std::ffi::OsStr::new("daemon"),
                std::ffi::OsStr::new("serve")
            ]
        );
    }

    #[test]
    fn daemon_command_prefers_the_dedicated_sibling_binary() {
        let temp = tempfile::tempdir().unwrap();
        let current = temp.path().join("atman");
        let sibling = temp.path().join("atman-daemon");
        std::fs::write(&sibling, []).unwrap();

        let command = daemon_process_command(&current);

        assert_eq!(command.get_program(), sibling.as_os_str());
        assert_eq!(command.get_args().count(), 0);
    }

    fn run_projection(
        id: atman_proto::FlowRunId,
        state: atman_proto::RunLifecycle,
    ) -> atman_proto::RunProjection {
        atman_proto::RunProjection {
            turn_id: None,
            id,
            flow_name: "test".into(),
            model: None,
            provider: None,
            parent_run_id: None,
            parent_node_id: None,
            state,
            started_at: chrono::Utc::now(),
            finished_at: None,
            error: None,
            output: None,
        }
    }

    #[test]
    fn daemon_follow_stops_only_for_the_target_run_terminal_state() {
        let target = atman_proto::FlowRunId(uuid::Uuid::now_v7());
        for state in [
            atman_proto::RunLifecycle::Cancelled,
            atman_proto::RunLifecycle::Succeeded,
            atman_proto::RunLifecycle::Failed,
            atman_proto::RunLifecycle::Lost,
        ] {
            let runs = vec![run_projection(target.clone(), state)];
            assert!(terminal_run_projection(&runs, &target).is_some());
        }

        let running = vec![run_projection(
            target.clone(),
            atman_proto::RunLifecycle::Running,
        )];
        assert!(terminal_run_projection(&running, &target).is_none());

        let other = vec![run_projection(
            atman_proto::FlowRunId(uuid::Uuid::now_v7()),
            atman_proto::RunLifecycle::Succeeded,
        )];
        assert!(terminal_run_projection(&other, &target).is_none());
    }

    #[tokio::test]
    async fn provider_catalog_refresh_consumes_entire_plan() {
        use atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome;

        let plan = vec!["provider-a".into(), "provider-b".into()];
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_by_refresh = seen.clone();

        consume_provider_catalog_refresh_plan(plan, move |provider_id| {
            seen_by_refresh.lock().unwrap().push(provider_id);
            std::future::ready(Ok::<_, atman_runtime::ProviderLifecycleError>(
                ProviderCatalogRefreshOutcome::NotNeeded,
            ))
        })
        .await;

        assert_eq!(
            *seen.lock().unwrap(),
            vec!["provider-a".to_string(), "provider-b".to_string()]
        );
    }

    #[test]
    fn provider_mutations_fail_explicitly_without_partial_runtime_state() {
        let _registry = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let config = tempfile::tempdir().unwrap();
        let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(config.path());
        let lifecycle = atman_runtime::provider_lifecycle::ProviderLifecycle::new(
            hub.clone(),
            atman_runtime::provider::ProviderRegistry::new(),
        );
        hub.add_auth_provider(atman_runtime::auth_store::StoredProvider {
            id: "unsupported".into(),
            name: "Unsupported".into(),
            kind: atman_runtime::auth_store::ProviderKind::GitHubCopilot,
            access_token: "access".into(),
            refresh_token: None,
            expires_at: i64::MAX,
            account: None,
            enabled: false,
            model_cache: None,
        })
        .unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let login_error = runtime
            .block_on(atman_daemon::provider_config::mutate(
                &lifecycle,
                atman_proto::ProviderMutation::Login {
                    kind: atman_proto::ProviderKind::GitHubCopilot,
                    name: "Unsupported".into(),
                },
            ))
            .unwrap_err();
        assert!(login_error.to_string().contains("not supported"));

        let enable_error = runtime
            .block_on(atman_daemon::provider_config::mutate(
                &lifecycle,
                atman_proto::ProviderMutation::SetEnabled {
                    provider_id: "unsupported".into(),
                    enabled: true,
                },
            ))
            .unwrap_err();
        assert!(enable_error.to_string().contains("not supported"));
        let stored = hub.load_auth().unwrap();
        assert!(!stored.providers[0].enabled);
        assert!(!lifecycle.provider_registry().contains("unsupported"));

        let refresh_error = runtime
            .block_on(atman_daemon::provider_config::mutate(
                &lifecycle,
                atman_proto::ProviderMutation::Refresh {
                    provider_id: "missing".into(),
                },
            ))
            .unwrap_err();
        assert!(refresh_error.to_string().contains("does not exist"));
    }

    #[test]
    fn config_provider_mutation_commits_through_selected_hub_and_reconciles_live_state() {
        struct ConfigReset;

        impl Drop for ConfigReset {
            fn drop(&mut self) {
                atman_runtime::model_registry::set_provider_config(Default::default());
            }
        }

        let _registry = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _reset = ConfigReset;
        let config = tempfile::tempdir().unwrap();
        let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(config.path());
        let lifecycle = atman_runtime::provider_lifecycle::ProviderLifecycle::new(
            hub.clone(),
            atman_runtime::provider::ProviderRegistry::new(),
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        let create = atman_proto::ProviderMutation::UpsertConfig {
            name: "gateway".into(),
            kind: "openai-compat".into(),
            api_key: "test-key".into(),
            api_key_env: String::new(),
            base_url: "http://localhost/v1".into(),
            max_tokens: Some(16_384),
            reasoning_format: "reasoning-effort".into(),
            enabled: true,
            create: true,
        };
        assert_eq!(
            runtime
                .block_on(atman_daemon::provider_config::mutate(
                    &lifecycle,
                    create.clone(),
                ))
                .unwrap(),
            atman_proto::ProviderMutationResult::ConfigSaved {
                name: "gateway".into(),
                created: true,
            }
        );
        assert!(lifecycle.provider_registry().contains("config:gateway"));
        let entry = hub.model_config().unwrap().unwrap().providers["gateway"].clone();
        assert_eq!(entry.max_tokens, Some(16_384));
        assert_eq!(
            entry.reasoning_format,
            Some(atman_runtime::providers::openai::OpenAiReasoningFormat::Official)
        );

        let before_duplicate = hub.read_config_toml().unwrap();
        assert!(
            runtime
                .block_on(atman_daemon::provider_config::mutate(&lifecycle, create))
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert_eq!(hub.read_config_toml().unwrap(), before_duplicate);

        let disable = atman_proto::ProviderMutation::UpsertConfig {
            name: "gateway".into(),
            kind: "openai-compat".into(),
            api_key: "test-key".into(),
            api_key_env: String::new(),
            base_url: "http://localhost/v1".into(),
            max_tokens: Some(16_384),
            reasoning_format: "reasoning-effort".into(),
            enabled: false,
            create: false,
        };
        runtime
            .block_on(atman_daemon::provider_config::mutate(&lifecycle, disable))
            .unwrap();
        assert!(!lifecycle.provider_registry().contains("config:gateway"));
        assert_eq!(
            hub.model_config().unwrap().unwrap().providers["gateway"].max_tokens,
            Some(16_384)
        );
    }

    #[test]
    fn auto_snapshot_env_true_overrides_config_false() {
        assert!(select_auto_snapshot(Some(" yes "), Some(false)));
    }

    #[test]
    fn auto_snapshot_false_like_env_does_not_override_config_true() {
        assert!(select_auto_snapshot(Some("false"), Some(true)));
    }

    #[test]
    fn auto_snapshot_defaults_to_false_without_env_or_config() {
        assert!(!select_auto_snapshot(None, None));
    }
}
