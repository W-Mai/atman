use anyhow::{Context, Result, bail};
use atman_dsl::parse::parse_file;
use atman_runtime::{Executor, Session, Value};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

mod daemon_tui;
mod init;
mod mcp_templates;
mod migrate_source;
mod oauth_login;
mod repl_completer;
mod suggest;
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
        None => cmd_repl(cli.r#continue).await,
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
    let cfg_path = atman_daemon::config::default_config_path()?;
    let cfg = atman_runtime::config_hub::ConfigHub::from_daemon_config_path(&cfg_path)
        .load_or_init_daemon_config()?;
    let base = format!("http://127.0.0.1:{port}");
    let client = atman_client::Client::connect(
        atman_client::HttpTransport::new(&base, &cfg.auth_token)?,
        atman_client::ClientIdentity::new("atman-cli", env!("CARGO_PKG_VERSION")),
    )
    .await
    .with_context(|| format!("connect to {base} (is atman-daemon running?)"))?;

    let abs = if file.is_absolute() {
        file.clone()
    } else {
        std::env::current_dir()?.join(&file)
    };

    let images = images
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
        .collect::<Result<Vec<_>>>()?;
    let run = client
        .run_flow(
            abs.to_string_lossy().into_owned(),
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

async fn follow_daemon_run(
    client: &atman_client::Client,
    run: &atman_proto::RunFlowResponse,
) -> Result<()> {
    use futures::StreamExt;

    let session = client
        .attach_session(run.session_id.clone())
        .await
        .context("attach daemon session for follow")?;
    if run_projection_finished(&session.current().projection().runs, &run.run_id) {
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
        if run_projection_finished(&session.current().projection().runs, &run.run_id) {
            return Ok(());
        }
    }
    bail!(
        "session event stream ended before run {} reached a terminal state",
        run.run_id
    )
}

fn run_projection_finished(
    runs: &[atman_proto::RunProjection],
    run_id: &atman_proto::FlowRunId,
) -> bool {
    runs.iter().any(|run| {
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

    let target_flow = parsed
        .flows
        .iter()
        .find(|f| f.name.name == flow_name)
        .ok_or_else(|| anyhow::anyhow!("flow `{flow_name}` not found in {}", file.display()))?;
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
            .map(|(k, v)| format!("{k}={}", render_value(v)))
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
            println!("{}", render_value(&v));
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
    let mut cursor = None;
    loop {
        let page = client.get_events(session_id.clone(), cursor).await?;
        for envelope in &page.events {
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
        if !page.has_more {
            break;
        }
        cursor = Some(page.next_cursor.0);
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

    let events = atman_runtime::event_log::reader::read_event_envelopes(&events_path)?;
    let findings = atman_runtime::attachment_store::sanitize_findings(&events)?;

    if findings.is_empty() {
        println!("sanitize: no attachment problems found");
        return Ok(());
    }
    println!("sanitize: found {} attachment issue(s)", findings.len());
    for finding in &findings {
        println!(
            "  {} part_id={} {} → {}",
            finding.context,
            match finding.patch.target {
                atman_runtime::message::AttachmentTarget::Part { part_id } => part_id.0,
                atman_runtime::message::AttachmentTarget::Legacy { .. } => {
                    unreachable!("sanitize only emits stable part identities")
                }
            },
            finding.patch.file_basename,
            finding.patch.reason
        );
    }
    if dry_run {
        println!("sanitize: dry-run, no events written");
        return Ok(());
    }

    let session =
        atman_runtime::Session::open_existing_with_trust(&root, &sid, load_global_trust_config()?)
            .with_context(|| format!("open session {sid}"))?;
    let session = std::sync::Arc::new(session);
    for finding in &findings {
        atman_runtime::attachment_store::emit_sanitize_finding(session.sink(), finding);
    }
    match std::sync::Arc::try_unwrap(session) {
        Ok(s) => s.shutdown().await,
        Err(_) => atman_runtime::notify!(
            warn,
            location = Log,
            stack = dedupe("sanitize.refs_at_shutdown", 60_000),
            "sanitize: session still had refs at shutdown"
        ),
    }
    println!("sanitize: wrote {} degrade event(s)", findings.len());
    Ok(())
}

enum RouteOutcome {
    Handled(Value),
    HandledErr(anyhow::Error),
}

async fn route_input_in_turn(
    route: &atman_runtime::routing::RouteMatch,
    executor: &Executor,
    session: std::sync::Arc<Session>,
    turn_id: atman_runtime::event::TurnId,
    invocation_env: atman_runtime::InvocationEnv,
) -> RouteOutcome {
    match run_slash_command_in_turn(
        &route.slash_call(),
        executor,
        session,
        turn_id,
        invocation_env,
    )
    .await
    {
        Ok(v) => RouteOutcome::Handled(v),
        Err(e) => RouteOutcome::HandledErr(e),
    }
}

fn resolve_route(line: &str) -> Result<Option<atman_runtime::routing::RouteMatch>> {
    let hub = atman_runtime::config_hub::ConfigHub::global()?;
    let program = atman_runtime::routing::RouteProgram::load(&hub)?;
    Ok(program.resolve(line))
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
    let mut executor = executor.clone();
    executor.source_dir = path.parent().map(|p| p.to_path_buf());
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
    session: std::sync::Arc<Session>,
    turn_id: atman_runtime::event::TurnId,
    invocation_env: atman_runtime::InvocationEnv,
) -> Result<Value> {
    let (parsed, flow_name, kv, source_dir) = resolve_slash_command(line)?;
    let mut executor = executor.clone();
    executor.source_dir = source_dir;
    executor
        .run_in_turn_with_env(
            &parsed,
            &flow_name,
            kv,
            Some(turn_id),
            Some(session),
            invocation_env,
        )
        .await
        .map_err(Into::into)
}

type SlashCommandParsed = (
    atman_dsl::ast::File,
    String,
    Vec<(String, Value)>,
    Option<PathBuf>,
);

fn resolve_slash_command(line: &str) -> Result<SlashCommandParsed> {
    let hub = atman_runtime::config_hub::ConfigHub::global()?;
    let resolved = atman_runtime::routing::resolve_command_call(&hub, line)?;
    Ok((
        resolved.file,
        resolved.flow_name,
        resolved.args,
        resolved.source_dir,
    ))
}

struct PrebuiltSession {
    session: std::sync::Arc<atman_runtime::Session>,
    initial_transcript: Vec<atman_runtime::TranscriptEntry>,
    executor: Executor,
    is_fresh: bool,
    root: PathBuf,
    intro: Option<atman_tui::app::StartupIntro>,
    /// Notifications collected during boot, for seamless toast continuity.
    boot_notifications: Vec<atman_runtime::notify::Notification>,
    provider_catalog_refresh_plan: Vec<String>,
    mcp_shutdown_tx: Option<tokio::sync::oneshot::Sender<()>>,
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
    load_model_config_from_disk();
    let root = data_dir()?;
    let redactor = atman_daemon::bootstrap::build_redactor(config_dir().ok().as_deref());
    let is_fresh = resume_sid.is_none();
    let project_index = open_current_project_index()?;
    let global_trust = load_global_trust_config()?;
    let mut initial_transcript = Vec::new();
    let session = std::sync::Arc::new(match resume_sid {
        Some(sid) => {
            let resolved_sid = resolve_session_prefix(&root, &sid)?;
            let session = if tui_mode_requested() {
                let mut observer = |entry| initial_transcript.push(entry);
                Session::open_existing_with_replay_observer(
                    &root,
                    &resolved_sid,
                    redactor.clone(),
                    project_index.clone(),
                    global_trust.clone(),
                    &mut observer,
                )
            } else {
                Session::open_existing_with_context_and_trust(
                    &root,
                    &resolved_sid,
                    redactor.clone(),
                    project_index.clone(),
                    global_trust.clone(),
                )
            };
            session.with_context(|| {
                format!("resuming session {resolved_sid} under {}", root.display())
            })?
        }
        None => Session::open_with_context_and_trust(
            &root,
            redactor.clone(),
            project_index.clone(),
            global_trust,
        )
        .with_context(|| format!("opening session under {}", root.display()))?,
    });
    apply_session_config(&session);
    emit(BootStepId::OpenSession, false, true);

    emit(BootStepId::BuildExecutor, true, false);
    let atman_daemon::bootstrap::BootstrapOutcome {
        mut executor,
        provider_catalog_refresh_plan,
        ..
    } = atman_daemon::bootstrap::build_executor(bootstrap_opts(session.sink().clone(), false)?)
        .await?;
    emit(BootStepId::BuildExecutor, false, true);

    emit(BootStepId::RegisterProviders, true, false);
    emit(BootStepId::RegisterProviders, false, true);

    emit(BootStepId::AttachMcp, true, false);
    let mcp_shutdown_tx = atman_daemon::bootstrap::spawn_mcp_boot(
        executor.clone(),
        session.clone(),
        config_dir().ok().as_deref(),
    );
    emit(BootStepId::AttachMcp, false, true);

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
        initial_transcript,
        executor,
        is_fresh,
        root,
        intro,
        boot_notifications: Vec::new(),
        provider_catalog_refresh_plan,
        mcp_shutdown_tx,
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

    // Replace CliSink with ToastCollector during boot so codex/other
    // startup logs don't leak to the raw terminal.
    let (toast_sink, toast_buf) = atman_runtime::notify::ToastCollector::new();
    let _boot_sink =
        atman_runtime::notify::ScopedSink::replace_with(std::sync::Arc::new(toast_sink));

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let prebuild = tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("boot animation prebuild runtime init")?;
        rt.block_on(prebuild_session(resume_sid, None, Some(tx)))
    });
    let animation = atman_tui::boot_animation::run_boot_animation(rx, version, recent, toast_buf);
    let (anim_result, prebuild_result) = tokio::join!(animation, prebuild);
    let (terminal, boot_notifications) = anim_result?;
    match prebuild_result {
        Ok(Ok(mut session)) => {
            session.boot_notifications = boot_notifications;
            Ok((session, Some(terminal)))
        }
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
    // Suppress stderr while TUI is in raw mode — spans all session switches.
    let _sink_guard: Option<atman_runtime::notify::ScopedSink> = if tui_mode_requested() {
        Some(atman_runtime::notify::ScopedSink::tui())
    } else {
        None
    };
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
        mut initial_transcript,
        mut executor,
        is_fresh: is_fresh_session,
        root,
        intro,
        boot_notifications,
        mut provider_catalog_refresh_plan,
        mcp_shutdown_tx,
    } = prebuilt;

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
    if !use_tui {
        let provider_lifecycle = executor
            .provider_lifecycle()
            .context("provider lifecycle unavailable")?;
        consume_provider_catalog_refresh_plan(
            std::mem::take(&mut provider_catalog_refresh_plan),
            move |provider_id| {
                let lifecycle = provider_lifecycle.clone();
                async move { lifecycle.refresh_models_if_stale(&provider_id).await }
            },
        )
        .await;
    }
    lifecycles
        .fire(&executor, atman_dsl::ast::LifecycleEvent::SessionStart)
        .await;

    let classifier = build_interjection_classifier();

    if let Err(e) = run_boot_flow(&executor, &reporter).await {
        reporter.error(format!("[atman] boot flow error: {e}"));
    }

    let flow_names = discover_flow_names();
    let (input_tx, mut input_rx) = mpsc::unbounded_channel::<ReplInput>();
    let (tui_task, tui_shutdown, ctrl_task, cmd_tx_for_repl) = if use_tui {
        // The control task is the sole TUI-side owner of the REPL input sender.
        // Dropping the TUI closes ctrl_rx, which drops this sink and wakes input_rx.
        let tui_input_sink = TuiControlInputSink::new(input_tx, session.clone());
        let (sh_tx, sh_rx) = tokio::sync::oneshot::channel::<()>();
        let sh_tx_shared: std::sync::Arc<
            std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
        > = std::sync::Arc::new(std::sync::Mutex::new(Some(sh_tx)));
        let sh_tx_for_ctrl = sh_tx_shared.clone();
        session.flush_writer().await;
        initial_transcript.extend(session.transcript_since_open());
        let output_store =
            atman_runtime::tools::tool_output::OutputStore::at(session.dir().to_path_buf());
        let mut initial_items = atman_tui::history::flatten_transcript_with_output_store(
            &initial_transcript,
            &output_store,
        );
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
        let cmd_tx_for_models = cmd_tx.clone();
        let session_for_ctrl = std::sync::Arc::clone(&session);
        let session_for_ctrl_term_registry = executor.tool_ctx.term_registry.clone();
        let switch_target_for_ctrl = switch_target.clone();
        let providers_for_ctrl = executor.providers.clone();
        let provider_lifecycle_for_ctrl = executor
            .provider_lifecycle()
            .context("provider lifecycle unavailable")?;
        let data_root_for_ctrl = root.clone();
        let executor_for_ctrl = executor.clone();
        let reporter_for_ctrl = reporter.clone();
        let mut mcp_shutdown_tx = mcp_shutdown_tx;
        let ctrl_task = tokio::spawn(async move {
            let mut session_moves =
                tokio::task::JoinSet::<atman_runtime::form::FormSubmission>::new();
            let mut provider_mutations = tokio::task::JoinSet::new();
            let mut provider_catalog_refreshes = tokio::task::JoinSet::new();
            let mut trust_update_error: Option<String> = None;
            for provider_id in provider_catalog_refresh_plan {
                let lifecycle = provider_lifecycle_for_ctrl.clone();
                provider_catalog_refreshes.spawn(async move {
                    let result = match lifecycle.refresh_models_if_stale(&provider_id).await {
                        Ok(outcome) => Ok(outcome),
                        Err(
                            atman_runtime::ProviderLifecycleError::ProviderNotFound { .. }
                            | atman_runtime::ProviderLifecycleError::ProviderDisabled { .. }
                            | atman_runtime::ProviderLifecycleError::Stale { .. },
                        ) => Ok(
                            atman_runtime::provider_lifecycle::ProviderCatalogRefreshOutcome::NotNeeded,
                        ),
                        Err(error) => Err(error.to_string()),
                    };
                    (provider_id, result)
                });
            }
            loop {
                let msg = tokio::select! {
                    message = ctrl_rx.recv() => {
                        let Some(message) = message else {
                            break;
                        };
                        message
                    }
                    completed = session_moves.join_next(), if !session_moves.is_empty() => {
                        match completed {
                            Some(Ok(atman_runtime::form::FormSubmission::Submitted { answers })) => {
                                if let Some(atman_runtime::form::FormAnswer::TextEntered { text }) = answers.first() {
                                    let cwd = std::path::PathBuf::from(text);
                                    let mut meta = atman_runtime::session_meta::SessionMeta::load(
                                        session_for_ctrl.dir(),
                                    ).unwrap_or_default();
                                    meta.rebase(&cwd);
                                    if let Err(error) = meta.save(session_for_ctrl.dir()) {
                                        reporter_for_ctrl.error(format!("move session failed: {error}"));
                                    }
                                }
                            }
                            Some(Err(error)) => reporter_for_ctrl.error(format!("move session failed: {error}")),
                            _ => {}
                        }
                        continue;
                    }
                    completed = provider_mutations.join_next(), if !provider_mutations.is_empty() => {
                        match completed {
                            Some(Ok((request, result))) => {
                                let _ = cmd_tx_for_models.send(
                                    atman_tui::TuiCommand::ProviderMutationResult {
                                        request,
                                        result,
                                    },
                                );
                            }
                            Some(Err(error)) => {
                                session_for_ctrl.cancel_flow();
                                if let Some(tx) = sh_tx_for_ctrl.lock().unwrap().take() {
                                    let _ = tx.send(());
                                }
                                provider_mutations.shutdown().await;
                                provider_catalog_refreshes.shutdown().await;
                                if error.is_panic() {
                                    std::panic::resume_unwind(error.into_panic());
                                }
                                panic!("provider mutation task failed: {error}");
                            }
                            None => {}
                        }
                        continue;
                    }
                    completed = provider_catalog_refreshes.join_next(), if !provider_catalog_refreshes.is_empty() => {
                        match completed {
                            Some(Ok((provider_id, result))) => {
                                let _ = cmd_tx_for_models.send(
                                    atman_tui::TuiCommand::ProviderCatalogRefreshResult {
                                        provider_id,
                                        result,
                                    },
                                );
                            }
                            Some(Err(error)) => {
                                session_for_ctrl.cancel_flow();
                                if let Some(tx) = sh_tx_for_ctrl.lock().unwrap().take() {
                                    let _ = tx.send(());
                                }
                                provider_mutations.shutdown().await;
                                provider_catalog_refreshes.shutdown().await;
                                if error.is_panic() {
                                    std::panic::resume_unwind(error.into_panic());
                                }
                                panic!("provider catalog refresh task failed: {error}");
                            }
                            None => {}
                        }
                        continue;
                    }
                };
                if let atman_tui::TuiControl::Domain(command) = msg {
                    match command {
                        atman_tui::TuiDomainCommand::Submit(submission) => {
                            if let Some(error) = trust_update_error.take() {
                                if !submission.images.is_empty() {
                                    session_for_ctrl.restore_pending_images(submission.images);
                                }
                                reporter_for_ctrl.error(format!(
                                    "input not submitted because the trust policy update failed: {error}"
                                ));
                                continue;
                            }
                            tui_input_sink.send(submission);
                        }
                        atman_tui::TuiDomainCommand::UpdateTrust(mut trust) => {
                            trust.theme = session_for_ctrl.trust_config().theme;
                            let result = session_for_ctrl.update_trust(trust, |config| {
                                let hub = atman_runtime::config_hub::ConfigHub::global()
                                    .map_err(|error| std::io::Error::other(error.to_string()))?;
                                hub.set_trust_config(config)
                                    .map_err(|error| std::io::Error::other(error.to_string()))
                            });
                            match result {
                                Ok(()) => trust_update_error = None,
                                Err(error) => {
                                    let error = error.to_string();
                                    trust_update_error = Some(error.clone());
                                    reporter_for_ctrl
                                        .error(format!("failed to update trust policy: {error}"));
                                }
                            }
                        }
                        atman_tui::TuiDomainCommand::CancelFlow => session_for_ctrl.cancel_flow(),
                        atman_tui::TuiDomainCommand::HardStop => {
                            session_for_ctrl.cancel_flow();
                            let _ = session_for_ctrl.enqueue_injection_with_level(
                                "stop",
                                atman_runtime::injection::InjectionLevel::L4HardStop,
                                None,
                            );
                        }
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
                                    session_for_ctrl.permission_broker().user_resolve(
                                        &session_for_ctrl.id().to_string(),
                                        Some("local-tui-user".into()),
                                        ids,
                                        &expected,
                                        None,
                                        action,
                                        grant_scope,
                                        reason,
                                    )
                                }
                                atman_runtime::permission::PermissionSelector::Group(group_id) => {
                                    session_for_ctrl.permission_broker().user_resolve(
                                        &session_for_ctrl.id().to_string(),
                                        Some("local-tui-user".into()),
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
                                reporter_for_ctrl
                                    .error(format!("permission decision rejected: {error}"));
                            }
                        }
                        atman_tui::TuiDomainCommand::CompactNow => {
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
                                        atman_runtime::notify!(
                                            error,
                                            "compact runtime init failed: {e}"
                                        );
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
                        atman_tui::TuiDomainCommand::CompactReviewAccept { review_id, edited } => {
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
                        atman_tui::TuiDomainCommand::CompactReviewReject { review_id } => {
                            session_for_ctrl
                                .compact_reviews()
                                .decide(&review_id, atman_runtime::CompactReviewDecision::Reject);
                        }
                        atman_tui::TuiDomainCommand::DeleteSession(sid) => {
                            delete_session_dir(&data_root_for_ctrl, &sid);
                            if let Some(idx) = session_for_ctrl.project_index() {
                                let _ = idx.delete_events_for_session(&sid);
                            }
                        }
                        atman_tui::TuiDomainCommand::RenameSession { session_id, title } => {
                            let dir = data_root_for_ctrl.join("sessions").join(&session_id);
                            match atman_runtime::session_meta::SessionMeta::set_title(
                                &dir,
                                title.clone(),
                            ) {
                                Ok(()) => {
                                    if session_id == session_for_ctrl.id().to_string()
                                        && let Some(name) = title
                                    {
                                        let _ = cmd_tx_for_models
                                            .send(atman_tui::TuiCommand::SessionNameUpdated(name));
                                    }
                                }
                                Err(e) => {
                                    atman_runtime::notify!(error, "rename {session_id} failed: {e}")
                                }
                            }
                        }
                        atman_tui::TuiDomainCommand::FormSubmit {
                            form_id,
                            submission,
                        } => {
                            let _ = session_for_ctrl.forms().submit(&form_id, submission);
                        }
                        atman_tui::TuiDomainCommand::TermResize {
                            resource_id,
                            rows,
                            cols,
                        } => {
                            let task = resource_id
                                .task_id()
                                .map(atman_runtime::TaskId)
                                .and_then(|id| {
                                    executor_for_ctrl
                                        .tool_ctx
                                        .task_registry
                                        .as_ref()
                                        .and_then(|registry| registry.lookup(&id))
                                })
                                .filter(|task| {
                                    task.session_id == session_for_ctrl.id().to_string()
                                        && task.kind == atman_runtime::TaskKind::Terminal
                                });
                            if let (Some(registry), Some(task)) =
                                (&session_for_ctrl_term_registry, task)
                                && let Ok(entry) = registry
                                    .lookup(&task.source_handle, &session_for_ctrl.id().to_string())
                            {
                                let _ = entry.resize(rows, cols);
                            }
                        }
                        _ => {
                            atman_runtime::notify!(
                                error,
                                "TUI session command is unsupported by this host"
                            );
                        }
                    }
                    continue;
                }
                match msg {
                    atman_tui::TuiControl::AutoNameSession => {
                        match atman_runtime::session_naming::force_generate_session_name(
                            &executor_for_ctrl,
                            &session_for_ctrl,
                        )
                        .await
                        {
                            Ok(true) => {
                                if let Some(name) = atman_runtime::session_meta::SessionMeta::load(
                                    session_for_ctrl.dir(),
                                )
                                .and_then(|meta| meta.title)
                                {
                                    let _ = cmd_tx_for_models
                                        .send(atman_tui::TuiCommand::SessionNameUpdated(name));
                                }
                                atman_runtime::notify!(success, "session name generated");
                            }
                            Ok(false) => {
                                atman_runtime::notify!(warn, "session name was not changed")
                            }
                            Err(error) => {
                                atman_runtime::notify!(
                                    error,
                                    "session name generation failed: {error}"
                                )
                            }
                        }
                    }
                    atman_tui::TuiControl::SwitchSession { sid, intro } => {
                        // spawn_blocking + fresh current_thread runtime because MCP registration
                        // futures aren't Send, so plain tokio::spawn can't take them.
                        let handle = tokio::task::spawn_blocking(move || {
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .context("prebuild runtime init")?;
                            rt.block_on(prebuild_session(Some(sid), Some(intro), None))
                        });
                        *switch_target_for_ctrl.lock().unwrap() = Some(handle);
                        session_for_ctrl.cancel_flow();
                        if let Some(tx) = sh_tx_for_ctrl.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        break;
                    }
                    atman_tui::TuiControl::NewSession => {
                        let handle = tokio::task::spawn_blocking(move || {
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .context("prebuild runtime init")?;
                            rt.block_on(prebuild_session(None, None, None))
                        });
                        *switch_target_for_ctrl.lock().unwrap() = Some(handle);
                        session_for_ctrl.cancel_flow();
                        if let Some(tx) = sh_tx_for_ctrl.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        break;
                    }
                    atman_tui::TuiControl::MoveSession => {
                        if session_moves.is_empty() {
                            let kind = atman_runtime::form::FormKind::Text {
                                prompt: "New working directory:".into(),
                                placeholder: Some("/path/to/project".into()),
                                multiline: false,
                            };
                            let response = session_for_ctrl.forms().request(
                                atman_runtime::form::PendingForm {
                                    form_id: uuid::Uuid::now_v7().to_string(),
                                    run_id: atman_runtime::event::FlowRunId::now(),
                                    tool_use_id: "session_move_path".into(),
                                    kind: kind.clone(),
                                    form: atman_runtime::form::CompositeForm {
                                        questions: vec![atman_runtime::form::FormQuestion {
                                            id: "question".into(),
                                            kind,
                                        }],
                                    },
                                    emitted_at: chrono::Utc::now(),
                                },
                            );
                            session_moves.spawn(async move {
                                response
                                    .await
                                    .unwrap_or(atman_runtime::form::FormSubmission::Rejected)
                            });
                        }
                    }
                    atman_tui::TuiControl::MutateProvider(request) => {
                        let lifecycle = provider_lifecycle_for_ctrl.clone();
                        let action = request.action.clone();
                        spawn_provider_mutation_task(
                            &mut provider_mutations,
                            request,
                            async move { execute_provider_mutation(&lifecycle, action).await },
                        );
                    }
                    atman_tui::TuiControl::UpsertConfigModel {
                        old_name,
                        name,
                        model,
                        provider,
                        context_budget,
                        reasoning,
                        max_tokens,
                        enabled,
                    } => {
                        match atman_runtime::config_hub::ConfigHub::global().and_then(|hub| {
                            hub.upsert_model(atman_runtime::model_registry::ModelConfigUpdate {
                                old_name: old_name.as_deref(),
                                name: &name,
                                model: &model,
                                provider: provider.as_deref(),
                                context_budget,
                                reasoning,
                                capabilities: None,
                                image_detail: None,
                                max_tokens,
                                enabled,
                            })
                        }) {
                            Ok(()) => {
                                let _ = cmd_tx_for_models.send(
                                    atman_tui::TuiCommand::ProviderCatalogChanged {
                                        added_provider: None,
                                    },
                                );
                                atman_runtime::notify!(success, "Model \"{name}\" saved");
                            }
                            Err(e) => {
                                atman_runtime::notify!(error, "Model \"{name}\" save failed: {e}");
                            }
                        }
                    }
                    atman_tui::TuiControl::OpenAliasManager { .. } => {
                        // handled internally in the TUI — no-op here
                    }
                    atman_tui::TuiControl::OnboardingInit => {
                        if let Ok(dir) = config_dir() {
                            let _ = crate::init::init_config_dir_with_mode(&dir, None);
                        }
                    }
                    atman_tui::TuiControl::SwitchModel { request_id, model } => {
                        let result = atman_runtime::config_hub::ConfigHub::global()
                            .map_err(|error| error.to_string())
                            .and_then(|hub| {
                                switch_smart_model(
                                    &hub,
                                    &providers_for_ctrl,
                                    &session_for_ctrl,
                                    &model,
                                )
                            });
                        let _ = cmd_tx_for_models.send(atman_tui::TuiCommand::ModelSwitchResult {
                            request_id,
                            model,
                            result,
                        });
                    }
                    atman_tui::TuiControl::TestProvider { name, entry } => {
                        let tx = cmd_tx_for_models.clone();
                        tokio::spawn(async move {
                            let (msg, ok) = test_provider_endpoint(&name, &entry).await;
                            let _ = tx.send(atman_tui::TuiCommand::ProviderTestResult((msg, ok)));
                        });
                    }
                    atman_tui::TuiControl::McpTest { name } => {
                        let tx = cmd_tx_for_models.clone();
                        let name = name.clone();
                        tokio::spawn(async move {
                            let configs = load_mcp_configs();
                            let Some(cfg) = configs.into_iter().find(|c| c.name == name) else {
                                let _ = tx.send(atman_tui::TuiCommand::McpTestResult {
                                    name,
                                    message: "not found in config".into(),
                                    ok: false,
                                });
                                return;
                            };
                            let probe = atman_runtime::ToolRegistry::new();
                            let results =
                                atman_runtime::mcp::register_from_configs(&probe, &[cfg]).await;
                            let (msg, ok) = match &results[0] {
                                Ok(s) => (format!("{} tools discovered", s.tool_count), true),
                                Err(e) => (e.error.to_string(), false),
                            };
                            let _ = tx.send(atman_tui::TuiCommand::McpTestResult {
                                name,
                                message: msg,
                                ok,
                            });
                        });
                    }
                    atman_tui::TuiControl::McpListResources { name } => {
                        let tx = cmd_tx_for_models.clone();
                        let name = name.clone();
                        tokio::spawn(async move {
                            let configs = load_mcp_configs();
                            let Some(cfg) = configs.into_iter().find(|c| c.name == name) else {
                                let _ = tx.send(atman_tui::TuiCommand::McpResourcesResult {
                                    name,
                                    resources: Vec::new(),
                                });
                                return;
                            };
                            let client = match cfg.transport {
                                atman_runtime::mcp::TransportKind::Stdio => {
                                    atman_runtime::mcp::McpClient::connect_stdio(
                                        &cfg.name,
                                        &cfg.command,
                                        &cfg.args,
                                        &cfg.env,
                                        cfg.timeout_ms,
                                    )
                                    .await
                                }
                                _ => match cfg.url.as_deref() {
                                    Some(url) => {
                                        atman_runtime::mcp::McpClient::connect_http(
                                            &cfg.name,
                                            url,
                                            cfg.auth_token.clone(),
                                            cfg.timeout_ms,
                                        )
                                        .await
                                    }
                                    None => Err(atman_runtime::mcp::McpError::Protocol(
                                        "transport requires url".into(),
                                    )),
                                },
                            };
                            let resources = match client {
                                Ok(c) => c.list_resources().await.unwrap_or_default(),
                                Err(_) => Vec::new(),
                            };
                            let _ = tx.send(atman_tui::TuiCommand::McpResourcesResult {
                                name,
                                resources,
                            });
                        });
                    }
                    atman_tui::TuiControl::McpListPrompts { name } => {
                        let tx = cmd_tx_for_models.clone();
                        let name = name.clone();
                        tokio::spawn(async move {
                            let configs = load_mcp_configs();
                            let Some(cfg) = configs.into_iter().find(|c| c.name == name) else {
                                let _ = tx.send(atman_tui::TuiCommand::McpPromptsResult {
                                    name,
                                    prompts: Vec::new(),
                                });
                                return;
                            };
                            let client = match cfg.transport {
                                atman_runtime::mcp::TransportKind::Stdio => {
                                    atman_runtime::mcp::McpClient::connect_stdio(
                                        &cfg.name,
                                        &cfg.command,
                                        &cfg.args,
                                        &cfg.env,
                                        cfg.timeout_ms,
                                    )
                                    .await
                                }
                                _ => match cfg.url.as_deref() {
                                    Some(url) => {
                                        atman_runtime::mcp::McpClient::connect_http(
                                            &cfg.name,
                                            url,
                                            cfg.auth_token.clone(),
                                            cfg.timeout_ms,
                                        )
                                        .await
                                    }
                                    None => Err(atman_runtime::mcp::McpError::Protocol(
                                        "transport requires url".into(),
                                    )),
                                },
                            };
                            let prompts = match client {
                                Ok(c) => c.list_prompts().await.unwrap_or_default(),
                                Err(_) => Vec::new(),
                            };
                            let _ =
                                tx.send(atman_tui::TuiCommand::McpPromptsResult { name, prompts });
                        });
                    }
                    atman_tui::TuiControl::McpReload => {
                        if let Some(tx) = mcp_shutdown_tx.take() {
                            let _ = tx.send(());
                        }
                        mcp_shutdown_tx = atman_daemon::bootstrap::spawn_mcp_boot(
                            executor_for_ctrl.clone(),
                            session_for_ctrl.clone(),
                            crate::config_dir().ok().as_deref(),
                        );
                        let _ = cmd_tx_for_models
                            .send(atman_tui::TuiCommand::McpReloaded { active_runs: None });
                    }
                    _ => {
                        atman_runtime::notify!(
                            error,
                            "TUI control request is unsupported by this host"
                        );
                    }
                }
            }
            session_moves.shutdown().await;
            provider_mutations.shutdown().await;
            provider_catalog_refreshes.shutdown().await;
        });
        let session_meta =
            atman_runtime::session_meta::SessionMeta::load(session.dir()).unwrap_or_default();
        let handle = atman_tui::TuiHandle {
            session_id: session.id().to_string(),
            session_dir: session.dir().to_string_lossy().to_string(),
            session_name: session_meta.title,
            project_root: session_meta
                .project_root
                .map(|path| path.display().to_string()),
            goal: session.goal(),
            stream_rx: Some(session.stream_subscribe()),
            task_event_rx: executor
                .tool_ctx
                .task_registry
                .as_ref()
                .map(|tr| tr.subscribe()),
            submit_tx: None,
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
            trust_rx: Some(session.subscribe_trust()),
            compact_review_rx: Some(session.compact_reviews().subscribe()),
            form_rx: Some(session.forms().subscribe()),
            injection_rx: Some(session.subscribe_injections()),
            daemon_state_rx: None,
            daemon_updates_rx: None,
            flow_names: flow_names.clone(),
            session: Some(std::sync::Arc::clone(&session)),
            startup_intro: intro.clone(),
            onboarding_recommended: atman_runtime::model_registry::is_first_run(),
            trust: session.trust_config(),
            task_registry: executor.tool_ctx.task_registry.clone(),
            permission_client: Some(session.permission_broker().register_client()),
            boot_toasts: boot_notifications
                .into_iter()
                .map(|n| {
                    let level = match n.level {
                        atman_runtime::notify::NotifyLevel::Error => {
                            atman_tui::app::NoteLevel::Error
                        }
                        atman_runtime::notify::NotifyLevel::Warn => atman_tui::app::NoteLevel::Warn,
                        atman_runtime::notify::NotifyLevel::Success => {
                            atman_tui::app::NoteLevel::Success
                        }
                        _ => atman_tui::app::NoteLevel::Info,
                    };
                    atman_tui::app::ToastNote {
                        id: format!("boot-{}", n.message.len()),
                        level,
                        message: n.message,
                        ttl: std::time::Duration::from_secs(10),
                        created: n.created_at,
                        position: atman_tui::app::ToastPosition::TopRight,
                        fading: false,
                        fade_started: None,
                    }
                })
                .collect(),
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
    let mut pushback: VecDeque<ReplInput> = VecDeque::new();
    let sid = session.id().to_string();

    loop {
        let mut line = if let Some(l) = pushback.pop_front() {
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
            line.restore_images(&session);
            continue;
        }
        if line.starts_with(':') {
            line.restore_images(&session);
            let rest = line.strip_prefix(':').unwrap_or_default();
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
                "model" => {
                    if let Some(tx) = cmd_tx_for_repl.as_ref() {
                        let _ = tx.send(atman_tui::TuiCommand::OpenModelPicker);
                    } else {
                        reporter.info(format!("current model: {}", session.last_model()));
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
                    handle_compact_builtin(&session, &reporter, &executor.providers);
                }
                "attach" => {
                    let arg = trimmed.strip_prefix("attach").unwrap_or("").trim();
                    handle_attach_builtin(arg, &session, &reporter);
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
            let trimmed = line.trim().to_string();
            let route = match resolve_route(&trimmed) {
                Ok(Some(route)) => route,
                Ok(None) => {
                    line.restore_images(&session);
                    reporter.info(
                        "[atman] no route matched. add a route to ~/.config/atman/routes.at, or use `/name args...`.",
                    );
                    continue;
                }
                Err(error) => {
                    line.restore_images(&session);
                    reporter.error(format!("error: {error}"));
                    continue;
                }
            };
            (trimmed, TurnKind::Bare(route))
        };
        run_turn_with_interjection(
            session.clone(),
            &executor,
            &lifecycles,
            classifier.as_ref(),
            &text,
            line.images.take(),
            line.invocation_env.clone(),
            atman_runtime::message::MessageOrigin::User,
            kind,
            &mut input_rx,
            &mut pushback,
            &reporter,
        )
        .await;

        while session.watch_hub.has_active_watchers() {
            let timeout = std::time::Duration::from_secs(300);
            tokio::select! {
                biased;
                Some(line) = input_rx.recv() => {
                    pushback.push_back(line);
                    break;
                }
                evt = session.watch_hub.wait_for_event(timeout) => {
                    if let Some(evt) = evt {
                        let event_text = atman_runtime::watch::format_watch_event_text(&evt);
                        reporter.info(&event_text);
                        match resolve_route(&event_text) {
                            Ok(Some(route)) => {
                                run_turn_with_interjection(
                                    session.clone(),
                                    &executor,
                                    &lifecycles,
                                    classifier.as_ref(),
                                    &event_text,
                                    None,
                                    atman_runtime::InvocationEnv::default(),
                                    atman_runtime::message::MessageOrigin::Watcher,
                                    TurnKind::Bare(route),
                                    &mut input_rx,
                                    &mut pushback,
                                    &reporter,
                                )
                                .await;
                            }
                            Ok(None) => reporter.info(
                                "[atman] no route matched. add a route to ~/.config/atman/routes.at, or use `/name args...`.",
                            ),
                            Err(error) => reporter.error(format!("error: {error}")),
                        }
                    } else {
                        break;
                    }
                }
            }
        }
    }

    lifecycles
        .fire(&executor, atman_dsl::ast::LifecycleEvent::SessionEnd)
        .await;

    if let Some(tr) = &executor.tool_ctx.term_registry {
        tr.kill_all();
    }
    if let Some(br) = &executor.tool_ctx.bg_registry {
        br.kill_all();
    }
    drop(executor);
    if let Some(sh) = tui_shutdown
        && let Some(tx) = sh.lock().unwrap().take()
    {
        let _ = tx.send(());
    }
    if let Some(handle) = tui_task {
        match handle.await {
            Ok(Ok(())) | Err(_) => {}
            Ok(Err(e)) => atman_runtime::notify!(error, "tui exited with error: {e}"),
        }
    }
    if let Some(ct) = ctrl_task {
        match ct.await {
            Ok(()) => {}
            Err(error) if error.is_panic() => {
                std::panic::resume_unwind(error.into_panic());
            }
            Err(error) => return Err(anyhow::anyhow!("TUI control task failed: {error}")),
        }
    }
    let user_msg_count = session.user_message_count();
    let goal = session.goal();
    let meta = atman_runtime::session_meta::SessionMeta::load(session.dir()).unwrap_or_default();
    let session_name = meta.title;
    let project_root = meta.project_root.map(|path| path.display().to_string());
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
    let activity = session.activity_summary();
    session.shutdown().await;
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
            name: session_name,
            project_root,
            goal,
            todos,
            plans,
            activity,
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
    name: Option<String>,
    project_root: Option<String>,
    msg_count: usize,
    goal: Option<String>,
    todos: Vec<atman_runtime::memory::todo::Todo>,
    plans: Vec<atman_runtime::memory::plan::Plan>,
    activity: atman_runtime::activity::ActivitySummary,
}

pub fn flush_pending_summary() {
    SUMMARY_PENDING.with(|cell| {
        if let Some(s) = cell.borrow_mut().take() {
            print_session_summary(&s);
        }
    });
}

fn print_session_summary(summary: &SessionSummary) {
    let sid_short = &summary.sid;
    let goal_line = summary.goal.as_deref().unwrap_or("(none)");
    let pending = summary
        .todos
        .iter()
        .filter(|t| matches!(t.status, atman_runtime::memory::todo::TodoStatus::Pending))
        .count();
    let done = summary
        .todos
        .iter()
        .filter(|t| matches!(t.status, atman_runtime::memory::todo::TodoStatus::Done))
        .count();
    let plan_line = summary
        .plans
        .iter()
        .max_by_key(|p| p.updated_at)
        .map(|p| {
            let (step_done, step_total) = p.progress();
            format!("{} ({step_done}/{step_total})", truncate_str(&p.title, 40))
        })
        .unwrap_or_else(|| "(none)".to_string());

    let lines = vec![
        " ∴ ATMAN".to_string(),
        format!(
            " name      {}",
            truncate_str(summary.name.as_deref().unwrap_or("Untitled session"), 60)
        ),
        format!(
            " project   {}",
            truncate_str(summary.project_root.as_deref().unwrap_or("-"), 80)
        ),
        format!(" session   {sid_short}"),
        format!(" messages  {}", summary.msg_count),
        format!(" goal      {}", truncate_str(goal_line, 50)),
        format!(" plan      {plan_line}"),
        format!(" todos     {done} done · {pending} pending"),
        format!(
            " activity  {} tools · {} files · +{} −{}",
            summary.activity.attempted_calls,
            summary.activity.files,
            summary.activity.insertions,
            summary.activity.deletions
        ),
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

fn spawn_provider_mutation_task<F>(
    tasks: &mut tokio::task::JoinSet<(
        atman_tui::ProviderMutationRequest,
        Result<atman_tui::ProviderMutationSuccess, String>,
    )>,
    request: atman_tui::ProviderMutationRequest,
    future: F,
) where
    F: std::future::Future<Output = Result<atman_tui::ProviderMutationSuccess>> + Send + 'static,
{
    tasks.spawn(async move {
        let result = future.await.map_err(|error| format!("{error:#}"));
        (request, result)
    });
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

async fn execute_provider_mutation(
    lifecycle: &atman_runtime::provider_lifecycle::ProviderLifecycle,
    action: atman_tui::ProviderMutation,
) -> Result<atman_tui::ProviderMutationSuccess> {
    match action {
        atman_tui::ProviderMutation::Login { kind, name } => {
            if kind != atman_runtime::auth_store::ProviderKind::Codex {
                bail!("OAuth login for {kind:?} is not supported");
            }
            let (provider, delta) = crate::oauth_login::oauth_login::<
                atman_runtime::providers::codex::CodexProvider,
            >(lifecycle, &name)
            .await?;
            Ok(atman_tui::ProviderMutationSuccess::Installed {
                provider_id: provider.id,
                name: provider.name,
                kind: provider.kind,
                delta,
            })
        }
        atman_tui::ProviderMutation::SetEnabled {
            provider_id,
            enabled,
        } => {
            if !enabled {
                let change = lifecycle.disable_provider(&provider_id)?;
                return Ok(atman_tui::ProviderMutationSuccess::StateChanged {
                    provider_id,
                    enabled: Some(false),
                    change,
                    catalog: None,
                });
            }

            let provider = lifecycle
                .config_hub()
                .load_auth()?
                .providers
                .into_iter()
                .find(|provider| provider.id == provider_id)
                .with_context(|| format!("auth provider `{provider_id}` does not exist"))?;
            let expected_kind = provider.kind.clone();
            let live = atman_runtime::oauth::create_supported_managed_oauth_provider(
                &provider,
                lifecycle.config_hub().clone(),
            )?;
            let outcome = lifecycle
                .enable_provider(&provider_id, expected_kind, live)
                .await?;
            Ok(atman_tui::ProviderMutationSuccess::StateChanged {
                provider_id,
                enabled: Some(true),
                change: outcome.state,
                catalog: outcome.catalog,
            })
        }
        atman_tui::ProviderMutation::Remove { provider_id } => {
            let change = lifecycle.remove_provider(&provider_id)?;
            Ok(atman_tui::ProviderMutationSuccess::StateChanged {
                provider_id,
                enabled: None,
                change,
                catalog: None,
            })
        }
        atman_tui::ProviderMutation::Refresh { provider_id } => {
            let delta = lifecycle.refresh_models(&provider_id).await?;
            Ok(atman_tui::ProviderMutationSuccess::Refreshed { provider_id, delta })
        }
        atman_tui::ProviderMutation::UpsertConfig {
            name,
            kind,
            api_key,
            api_key_env,
            base_url,
            max_tokens,
            reasoning_format,
            enabled,
            create,
        } => {
            if !atman_runtime::model_registry::config_provider_types().contains(&kind.as_str()) {
                bail!("config provider kind `{kind}` is not supported");
            }
            let reasoning_format = if reasoning_format.trim().is_empty() {
                None
            } else {
                Some(
                    reasoning_format
                        .parse()
                        .map_err(|error: String| anyhow::anyhow!(error))?,
                )
            };
            let update = atman_runtime::config_hub::ProviderConfigUpdate {
                name: &name,
                kind: &kind,
                api_key: (!api_key.is_empty()).then_some(api_key.as_str()),
                api_key_env: (!api_key_env.is_empty()).then_some(api_key_env.as_str()),
                base_url: (!base_url.is_empty()).then_some(base_url.as_str()),
                max_tokens,
                reasoning_format,
                prompt_cache_key: None,
                enabled,
            };
            if create {
                lifecycle.create_config_provider(update)?
            } else {
                lifecycle.update_config_provider(update)?
            }
            Ok(atman_tui::ProviderMutationSuccess::ConfigSaved {
                name,
                created: create,
            })
        }
        _ => bail!("provider mutation is not supported by this host"),
    }
}

fn switch_smart_model(
    hub: &atman_runtime::config_hub::ConfigHub,
    providers: &atman_runtime::provider::ProviderRegistry,
    session: &atman_runtime::Session,
    requested_model: &str,
) -> Result<String, String> {
    let info = atman_runtime::model_registry::model_info(requested_model);
    if info.context_budget == 0 {
        return Err("model or provider is disabled".into());
    }
    let active_model = info.name;
    if providers.resolve(&active_model).is_none() {
        return Err("provider is not available in this process".into());
    }
    hub.update_alias(Some("smart"), "smart", &active_model)
        .map_err(|error| error.to_string())?;
    session.set_current_model(active_model.clone());
    Ok(active_model)
}

fn delete_session_dir(data_root: &std::path::Path, sid: &str) {
    let dir = data_root.join("sessions").join(sid);
    if !dir.exists() {
        return;
    }
    if let Err(e) = std::fs::remove_dir_all(&dir) {
        atman_runtime::notify!(error, "failed to delete session {}: {e}", dir.display());
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
            Self::Stdout => atman_runtime::notify!(info, "{}", strip_atman_tag(&text)),
            Self::Tui(tx) => {
                let _ = tx.send(atman_tui::TuiNote::Info(strip_atman_tag(&text).to_string()));
            }
        }
    }

    #[allow(dead_code)]
    fn warn(&self, text: impl Into<String>) {
        let text = text.into();
        match self {
            Self::Stdout => atman_runtime::notify!(warn, "{}", strip_atman_tag(&text)),
            Self::Tui(tx) => {
                let _ = tx.send(atman_tui::TuiNote::Warn(strip_atman_tag(&text).to_string()));
            }
        }
    }

    fn error(&self, text: impl Into<String>) {
        let text = text.into();
        match self {
            Self::Stdout => atman_runtime::notify!(error, "{}", strip_atman_tag(&text)),
            Self::Tui(tx) => {
                let _ = tx.send(atman_tui::TuiNote::Error(
                    strip_atman_tag(&text).to_string(),
                ));
            }
        }
    }

    #[allow(dead_code)]
    fn success(&self, text: impl Into<String>) {
        let text = text.into();
        match self {
            Self::Stdout => atman_runtime::notify!(success, "{}", strip_atman_tag(&text)),
            Self::Tui(tx) => {
                let _ = tx.send(atman_tui::TuiNote::Info(strip_atman_tag(&text).to_string()));
            }
        }
    }
}

fn strip_atman_tag(s: &str) -> &str {
    s.strip_prefix("[atman] ").unwrap_or(s)
}

type ExternalPrinter = Box<dyn rustyline::ExternalPrinter + Send>;

#[derive(Debug)]
struct ReplInput {
    text: String,
    images: Option<Vec<atman_runtime::message::ImageSource>>,
    invocation_env: atman_runtime::InvocationEnv,
}

impl ReplInput {
    fn text(text: String) -> Self {
        Self {
            text,
            images: None,
            invocation_env: atman_runtime::InvocationEnv::default(),
        }
    }

    fn from_tui(submission: atman_tui::TuiSubmission) -> Self {
        let invocation_env = submission
            .reasoning
            .map(|selection| {
                atman_runtime::InvocationEnv::single("effort", Value::Str(selection.to_string()))
            })
            .unwrap_or_default();
        Self {
            text: submission.text,
            images: Some(submission.images),
            invocation_env,
        }
    }

    fn has_images(&self) -> bool {
        self.images
            .as_ref()
            .is_some_and(|images| !images.is_empty())
    }

    fn restore_images(&mut self, session: &Session) {
        if let Some(images) = self.images.take()
            && !images.is_empty()
        {
            session.restore_pending_images(images);
        }
    }
}

impl std::ops::Deref for ReplInput {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.text
    }
}

struct TuiControlInputSink {
    input_tx: tokio::sync::mpsc::UnboundedSender<ReplInput>,
    session: std::sync::Arc<Session>,
}

impl TuiControlInputSink {
    fn new(
        input_tx: tokio::sync::mpsc::UnboundedSender<ReplInput>,
        session: std::sync::Arc<Session>,
    ) -> Self {
        Self { input_tx, session }
    }

    fn send(&self, submission: atman_tui::TuiSubmission) {
        if let Err(mut error) = self.input_tx.send(ReplInput::from_tui(submission)) {
            error.0.restore_images(&self.session);
        }
    }
}

fn spawn_stdin_reader(
    tx: tokio::sync::mpsc::UnboundedSender<ReplInput>,
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
                if tx.send(ReplInput::text(l)).is_err() {
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
                        atman_runtime::notify!(error, "rustyline init failed: {e}");
                        let _ = printer_tx.send(None);
                        return;
                    }
                };
            editor.set_helper(Some(completer));
            let printer: Option<ExternalPrinter> = match editor.create_external_printer() {
                Ok(p) => Some(Box::new(p)),
                Err(e) => {
                    atman_runtime::notify!(warn, "external printer unavailable: {e}");
                    None
                }
            };
            let _ = printer_tx.send(printer);
            loop {
                match editor.readline("atman> ") {
                    Ok(l) => {
                        if tx.send(ReplInput::text(l)).is_err() {
                            break;
                        }
                    }
                    Err(ReadlineError::Eof) | Err(ReadlineError::Interrupted) => break,
                    Err(e) => {
                        atman_runtime::notify!(error, "readline error: {e}");
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
        StreamFrame::Notification(frame) => render_note(printer, frame.message),
        StreamFrame::FlowGraph { .. }
        | StreamFrame::TurnStarted { .. }
        | StreamFrame::TurnEnded { .. }
        | StreamFrame::FlowStart { .. }
        | StreamFrame::FlowNodeStart { .. }
        | StreamFrame::FlowNodeEnd { .. }
        | StreamFrame::FlowDone { .. }
        | StreamFrame::ToolNode { .. }
        | StreamFrame::ThinkingChunk { .. }
        | StreamFrame::ToolCallDraft { .. }
        | StreamFrame::LlmCallStats { .. }
        | StreamFrame::AssistantMsg { .. }
        | StreamFrame::ToolResultMsg { .. }
        | StreamFrame::ToolPendingApproval { .. }
        | StreamFrame::ToolApproved { .. }
        | StreamFrame::ToolDenied { .. }
        | StreamFrame::PermissionRequestCreated { .. }
        | StreamFrame::PermissionRequestTargeted { .. }
        | StreamFrame::PermissionRequestDeferred { .. }
        | StreamFrame::PermissionRequestApproved { .. }
        | StreamFrame::PermissionRequestDenied { .. }
        | StreamFrame::PermissionRequestCancelled { .. }
        | StreamFrame::PermissionGroupCreated { .. }
        | StreamFrame::PermissionGroupUpdated { .. }
        | StreamFrame::PermissionGroupResolved { .. }
        | StreamFrame::PermissionGrantCreated { .. }
        | StreamFrame::PermissionGrantExpired { .. }
        | StreamFrame::UnrestrictedExecution { .. }
        | StreamFrame::TerminalChunk { .. }
        | StreamFrame::TerminalExited { .. }
        | StreamFrame::BashChunk { .. }
        | StreamFrame::BashExited { .. }
        | StreamFrame::DiffPreview { .. }
        | StreamFrame::FileEditApplied { .. }
        | StreamFrame::CompactionSummary { .. }
        | StreamFrame::CompactionDelta { .. }
        | StreamFrame::MermaidDiagram { .. }
        | StreamFrame::SubAgentStarted { .. }
        | StreamFrame::SubAgentDone { .. }
        | StreamFrame::LlmRetry { .. }
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
    Bare(atman_runtime::routing::RouteMatch),
}

#[allow(clippy::too_many_arguments)]
async fn run_turn_with_interjection(
    session: std::sync::Arc<Session>,
    executor: &Executor,
    lifecycles: &atman_runtime::lifecycle::LifecycleRunner,
    classifier: Option<
        &std::sync::Arc<dyn atman_runtime::injection_classifier::InjectionClassifier>,
    >,
    raw_line: &str,
    submitted_images: Option<Vec<atman_runtime::message::ImageSource>>,
    invocation_env: atman_runtime::InvocationEnv,
    origin: atman_runtime::message::MessageOrigin,
    kind: TurnKind,
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ReplInput>,
    pushback: &mut std::collections::VecDeque<ReplInput>,
    reporter: &Reporter,
) {
    let (text, inline_attachments) = extract_at_paths(raw_line);
    let turn_id = atman_runtime::event::TurnId::now();
    let user_msg = match build_user_message(
        &session,
        &text,
        &inline_attachments,
        submitted_images.as_deref(),
        turn_id.clone(),
        origin,
    ) {
        Ok(message) => message,
        Err(error) => {
            if let Some(images) = submitted_images {
                session.restore_pending_images(images);
            }
            reporter.error(format!("[atman] {error}"));
            return;
        }
    };
    {
        let _compact_guard = session.acquire_compact_lock().await;
        session.begin_turn(user_msg);
    }
    lifecycles
        .fire(executor, atman_dsl::ast::LifecycleEvent::TurnStart)
        .await;

    let flow_fut = async {
        match kind {
            TurnKind::Slash => {
                run_slash_command_in_turn(
                    &text,
                    executor,
                    session.clone(),
                    turn_id.clone(),
                    invocation_env,
                )
                .await
            }
            TurnKind::Bare(route) => {
                match route_input_in_turn(
                    &route,
                    executor,
                    session.clone(),
                    turn_id.clone(),
                    invocation_env,
                )
                .await
                {
                    RouteOutcome::Handled(v) => Ok(v),
                    RouteOutcome::HandledErr(e) => Err(e),
                }
            }
        }
    };
    tokio::pin!(flow_fut);

    let result = loop {
        tokio::select! {
            biased;
            r = &mut flow_fut => break r,
            Some(line) = input_rx.recv() => {
                if line.has_images()
                    || !consume_interjection_input(&line, &session, classifier, reporter).await
                {
                    pushback.push_back(line);
                }
            }
        }
    };
    let streamed = session.take_streamed_flag(&turn_id);
    let succeeded = result.is_ok();
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
    session.end_turn(&turn_id);
    if succeeded && session.record_successful_flow().is_some() {
        let _ =
            atman_runtime::session_naming::maybe_generate_session_name(executor, &session).await;
    }
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
    session: &Session,
    text: &str,
    attachments: &[std::path::PathBuf],
    submitted_images: Option<&[atman_runtime::message::ImageSource]>,
    turn_id: atman_runtime::event::TurnId,
    origin: atman_runtime::message::MessageOrigin,
) -> Result<atman_runtime::message::Message, atman_runtime::RuntimeError> {
    use atman_runtime::message::{Message, MessagePart, MessageRole};
    let mut imported = Vec::with_capacity(attachments.len());
    for path in attachments {
        imported.push(MessagePart::Image {
            id: None,
            source: session.import_image_path(path)?,
        });
    }
    let pending = submitted_images
        .map(<[atman_runtime::message::ImageSource]>::to_vec)
        .unwrap_or_else(|| session.take_pending_images());
    let mut parts: Vec<MessagePart> = pending
        .into_iter()
        .map(|source| MessagePart::Image { source, id: None })
        .collect();
    parts.extend(imported);
    if !text.is_empty() {
        parts.push(MessagePart::Text { text: text.into() });
    }
    Ok(Message {
        role: MessageRole::User,
        parts,
        turn_id,
        origin,
    })
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
            let event_count = atman_runtime::session_meta::SessionStats::load_or_rebuild(
                &root.join("sessions").join(&r.sid),
            )
            .map(|stats| stats.event_count)
            .unwrap_or(0);
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

fn handle_attach_builtin(arg: &str, session: &Session, reporter: &Reporter) {
    match arg {
        "" => {
            reporter.error(":attach <path>  |  :attach clear  |  :attach list");
        }
        "clear" => {
            session.clear_pending_images();
            reporter.info("[atman] pending attachments cleared");
        }
        "list" => {
            let names = session.pending_image_names();
            if names.is_empty() {
                reporter.info("[atman] no pending attachments");
            } else {
                for (i, name) in names.iter().enumerate() {
                    reporter.info(format!("  {i}: {name}"));
                }
            }
        }
        path => {
            let expanded = std::path::PathBuf::from(path);
            if !expanded.exists() {
                reporter.error(format!(":attach: file not found: {}", expanded.display()));
                return;
            }
            match session.queue_image_path(&expanded) {
                Ok(count) => reporter.info(format!(
                    "[atman] attached {} (pending count: {count})",
                    expanded.display(),
                )),
                Err(error) => {
                    reporter.error(format!(":attach: {error}"));
                }
            }
        }
    }
}

fn handle_compact_builtin(
    session: &Arc<Session>,
    reporter: &Reporter,
    providers: &atman_runtime::provider::ProviderRegistry,
) {
    let model = session.last_model();
    let model = if model.is_empty() {
        "claude-opus-4.7".to_string()
    } else {
        model
    };
    session.request_manual_compact();
    let before = session.messages().len();
    let tok_before = atman_runtime::compaction::estimate_tokens_for_messages(&session.messages());
    reporter.info(format!(
        ":compact — running LLM summary on {before} messages ({tok_before} tokens)…"
    ));
    let session_for_compact = Arc::clone(session);
    let providers_for_compact = providers.clone();
    let reporter_for_compact = reporter.clone();
    tokio::task::spawn_blocking(move || {
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                reporter_for_compact.error(format!(":compact — runtime init failed: {e}"));
                return;
            }
        };
        rt.block_on(async {
            atman_runtime::compaction::maybe_auto_compact(
                &session_for_compact,
                &model,
                &providers_for_compact,
            )
            .await;
        });
        let after = session_for_compact.messages().len();
        let tok_after = atman_runtime::compaction::estimate_tokens_for_messages(
            &session_for_compact.messages(),
        );
        reporter_for_compact.info(format!(
            ":compact — {before} → {after} messages · {tok_before} → {tok_after} tokens"
        ));
    });
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
    input_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ReplInput>,
    reporter: &Reporter,
) -> Result<()> {
    let events = session
        .events_path()
        .context("session has no events path (dry-run?)")?;
    let transcript = suggest::read_recent_events(&events, suggest::recent_turns_limit())?;
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
        "[atman] :suggest — reading {} turns, asking `{model}`…",
        suggest::recent_turns_limit()
    ));

    // Stream tokens to the reporter in real-time.
    let (token_tx, mut token_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let stream_task = tokio::spawn(async move {
        let mut buf = String::new();
        while let Some(t) = token_rx.recv().await {
            buf.push_str(&t);
        }
        buf
    });
    let reply = suggest::generate_suggestion(provider, &model, &transcript, Some(token_tx)).await?;
    let streamed = stream_task.await.unwrap_or_default();
    if !streamed.is_empty() {
        reporter.info(format!("---\n{streamed}\n---"));
    }

    if reply.trim() == "NO_SUGGESTION" {
        reporter.info("[atman] :suggest — model saw no reusable pattern.");
        return Ok(());
    }
    let Some(dsl_src) = suggest::extract_code_block(&reply) else {
        reporter.info("[atman] :suggest — model reply did not contain a fenced code block.");
        return Ok(());
    };

    let flow_name = match suggest::extract_flow_name(&dsl_src) {
        Ok(n) => n,
        Err(e) => {
            reporter.info(format!(
                "[atman] :suggest — suggested flow is not valid: {e}"
            ));
            return Ok(());
        }
    };
    let parsed = parse_file(&dsl_src)?;
    if let Err(errs) = atman_runtime::validate::validate(&parsed.flows[0], &executor.tools) {
        reporter.info("[atman] :suggest — validation rejected the suggestion:");
        for e in errs {
            reporter.info(format!("  · {e:?}"));
        }
        return Ok(());
    }

    let has_shell = dsl_src.contains("bash.spawn")
        || dsl_src.contains("term.spawn")
        || dsl_src.contains("shell.");
    reporter.info(format!("[atman] suggested flow `{flow_name}`:"));
    reporter.info(format!("---\n{dsl_src}\n---"));
    if has_shell {
        reporter.info("[atman] note: this flow calls shell tools — accept only if you trust it.");
    }

    // Accept/reject: use form modal in TUI, CLI prompt otherwise.
    let choice = if reporter.is_tui() {
        let form = atman_runtime::form::PendingForm {
            form_id: uuid::Uuid::now_v7().to_string(),
            run_id: atman_runtime::event::FlowRunId::now(),
            tool_use_id: "suggest_confirm".to_string(),
            form: atman_runtime::form::CompositeForm {
                questions: vec![atman_runtime::form::FormQuestion {
                    id: "question".into(),
                    kind: atman_runtime::form::FormKind::Confirm {
                        prompt: format!("accept suggested flow `{flow_name}`?"),
                    },
                }],
            },
            kind: atman_runtime::form::FormKind::Confirm {
                prompt: format!("accept suggested flow `{flow_name}`?"),
            },
            emitted_at: chrono::Utc::now(),
        };
        let rx = session.forms().request(form);
        match rx.await {
            Ok(atman_runtime::form::FormSubmission::Submitted { answers })
                if matches!(
                    answers.first(),
                    Some(atman_runtime::form::FormAnswer::Confirmed { value: true })
                ) =>
            {
                'y'
            }
            _ => 'n',
        }
    } else {
        reporter.info(
            "[atman] accept? [y] yes / [n] no / [e] print path so you can edit the buffered draft",
        );
        loop {
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
        }
    };

    if choice == 'n' {
        reporter.info("[atman] :suggest — discarded.");
        return Ok(());
    }

    let cfg = config_dir()?;
    let (final_name, target) = install_suggested_flow(&cfg, &flow_name, &dsl_src)
        .with_context(|| "install suggested flow")?;
    let trigger = format!("{final_name} ");

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

fn install_suggested_flow(
    config_dir: &std::path::Path,
    flow_name: &str,
    dsl_src: &str,
) -> anyhow::Result<(String, std::path::PathBuf)> {
    use std::io::Write;

    let cmd_dir = config_dir.join("commands");
    std::fs::create_dir_all(&cmd_dir)?;
    let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(config_dir);
    let mut suffix = 1usize;
    loop {
        let final_name = if suffix == 1 {
            flow_name.to_string()
        } else {
            format!("{flow_name}_v{suffix}")
        };
        let target = cmd_dir.join(format!("{final_name}.at"));
        let final_src = dsl_src;
        let mut file = match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                suffix += 1;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let write_result = file
            .write_all(format!("{final_src}\n").as_bytes())
            .and_then(|_| file.sync_all());
        drop(file);
        if let Err(error) = write_result {
            let _ = std::fs::remove_file(&target);
            return Err(error.into());
        }

        let trigger = format!("{final_name} ");
        if let Err(error) = hub.append_dsl_route(&final_name, &trigger) {
            let _ = std::fs::remove_file(&target);
            return Err(anyhow::anyhow!(error));
        }
        return Ok((final_name, target));
    }
}

fn apply_session_config(session: &atman_runtime::Session) {
    if let Some(mode) = atman_runtime::config_hub::ConfigHub::global()
        .and_then(|hub| hub.compact_review_mode())
        .ok()
        .flatten()
    {
        session.set_compact_review_mode(mode);
    }
    let env_mode = std::env::var("ATMAN_FS_ACCESS")
        .ok()
        .and_then(|s| s.parse::<atman_runtime::fs_access::FsAccessMode>().ok());
    let config_mode = atman_runtime::config_hub::ConfigHub::global()
        .and_then(|hub| hub.fs_access_mode())
        .ok()
        .flatten();
    if let Some(mode) = select_fs_access_mode(env_mode, config_mode) {
        session.set_fs_access_mode(mode);
    }
}

pub fn load_model_config_from_disk() {
    let Ok(hub) = atman_runtime::config_hub::ConfigHub::global() else {
        return;
    };
    match hub.migrate_and_reload_models() {
        Ok(atman_runtime::model_registry::ModelMigrationOutcome::Migrated { backup }) => {
            atman_runtime::notify!(
                info,
                "config.toml migrated to v2 format (backup at {})",
                backup.display()
            );
        }
        Ok(atman_runtime::model_registry::ModelMigrationOutcome::NotNeeded) => {}
        Err(error) => atman_runtime::notify!(
            error,
            "config.toml migration/reload failed; disk migration may already be committed: {error}"
        ),
    }
}

fn select_fs_access_mode(
    env_mode: Option<atman_runtime::fs_access::FsAccessMode>,
    config_mode: Option<atman_runtime::fs_access::FsAccessMode>,
) -> Option<atman_runtime::fs_access::FsAccessMode> {
    env_mode.or(config_mode)
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

fn load_suggest_model() -> String {
    let configured = atman_runtime::config_hub::ConfigHub::global()
        .and_then(|hub| hub.suggest_model())
        .ok()
        .flatten();
    atman_runtime::model_registry::resolve_alias(&select_suggest_model(configured))
}

fn select_suggest_model(configured: Option<String>) -> String {
    configured.unwrap_or_else(|| "gpt-4o-mini".to_string())
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
        let stats = atman_runtime::session_meta::SessionStats::load_or_rebuild(&entry.path())
            .unwrap_or_default();
        out.push((
            id.clone(),
            serde_json::json!({
                "id": id,
                "event_count": stats.event_count,
                "first_ts": stats.first_ts,
            }),
        ));
    }
    out.sort_by(|a, b| b.0.cmp(&a.0));
    out.into_iter().map(|(_, v)| v).collect()
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

fn build_interjection_classifier()
-> Option<std::sync::Arc<dyn atman_runtime::injection_classifier::InjectionClassifier>> {
    let mode = atman_runtime::config_hub::ConfigHub::global()
        .and_then(|hub| hub.interjection_mode())
        .ok()
        .flatten();
    classifier_for_interjection_mode(mode)
}

fn classifier_for_interjection_mode(
    mode: Option<atman_runtime::config_hub::InterjectionMode>,
) -> Option<std::sync::Arc<dyn atman_runtime::injection_classifier::InjectionClassifier>> {
    use atman_runtime::config_hub::InterjectionMode;
    use atman_runtime::injection_classifier::{ComposedClassifier, RuleClassifier};

    match mode {
        Some(InterjectionMode::Off) => None,
        Some(InterjectionMode::Rule) | None => Some(std::sync::Arc::new(RuleClassifier::default())),
        Some(InterjectionMode::Llm) => Some(std::sync::Arc::new(ComposedClassifier::new(
            RuleClassifier::default(),
        ))),
        Some(InterjectionMode::Unknown(value)) => {
            atman_runtime::notify!(
                warn,
                "unknown [interjection] classifier = `{value}` — falling back to rule"
            );
            Some(std::sync::Arc::new(RuleClassifier::default()))
        }
    }
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
            let root = data_dir()?;
            let session = Session::open_with_trust(&root, load_global_trust_config()?)
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
                origin: atman_runtime::message::MessageOrigin::User,
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
    let root = data_dir()?;
    let sid = match session_id {
        Some(s) => s,
        None => latest_session(&root)?
            .with_context(|| format!("no sessions found under {}", root.display()))?,
    };
    let cfg_path = atman_daemon::config::default_config_path()?;
    let cfg = atman_runtime::config_hub::ConfigHub::from_daemon_config_path(&cfg_path)
        .load_or_init_daemon_config()?;
    let base = format!("http://127.0.0.1:{port}");
    let client = reqwest::Client::new();
    atman_runtime::notify!(
        info,
        "streaming events for session {sid} from {base}/events"
    );
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
        atman_runtime::notify!(warn, "--follow not yet implemented");
    }
    Ok(())
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

async fn cmd_mcp(action: McpAction) -> anyhow::Result<()> {
    use std::io::Write as _;
    match action {
        McpAction::List => {
            let configs = load_mcp_configs();
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
                let transport = match cfg.transport {
                    atman_runtime::mcp::TransportKind::Stdio => "stdio",
                    atman_runtime::mcp::TransportKind::Http => "http",
                    atman_runtime::mcp::TransportKind::Sse => "sse",
                };
                let source = if cfg.command.is_empty() {
                    cfg.url.as_deref().unwrap_or("<missing url>").to_string()
                } else {
                    let mut s = format!("{} {}", cfg.command, cfg.args.join(" "));
                    if !cfg.env.is_empty() {
                        s.push_str(&format!(" (env: {} keys)", cfg.env.len()));
                    }
                    s
                };
                let tier = match cfg.tier {
                    atman_runtime::Tier::Zero => 0,
                    atman_runtime::Tier::One => 1,
                    atman_runtime::Tier::Two => 2,
                    atman_runtime::Tier::Three => 3,
                    atman_runtime::Tier::Four => 4,
                };
                println!(
                    "  {:<20} {:<8} {:<30} {}{}",
                    cfg.name,
                    transport,
                    source.chars().take(30).collect::<String>(),
                    tier,
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
                atman_runtime::config_hub::ConfigHub::global()?.upsert_mcp(config)?;
                println!("✓ Added MCP server \"{}\" to mcp_servers.json", name);
            } else {
                cmd_mcp_add_interactive()?;
            }
        }
        McpAction::Remove { name } => {
            atman_runtime::config_hub::ConfigHub::global()?.remove_mcp(&name)?;
            println!("✓ Removed MCP server \"{}\"", name);
        }
        McpAction::Test { name } => {
            let configs = load_mcp_configs();
            let Some(cfg) = configs.iter().find(|c| c.name == name) else {
                anyhow::bail!("MCP server \"{}\" not found", name);
            };
            print!("Connecting to {}... ", name);
            std::io::stdout().flush()?;
            let probe_registry = atman_runtime::ToolRegistry::new();
            let statuses = atman_runtime::mcp::register_from_configs(
                &probe_registry,
                std::slice::from_ref(cfg),
            )
            .await;
            match &statuses[0] {
                Ok(s) => {
                    println!("✓ {} tools discovered", s.tool_count);
                    let names = probe_registry.names();
                    for name in &names {
                        println!("  - {}", name);
                    }
                }
                Err(e) => {
                    println!("✗ {}", e.error);
                }
            }
        }
        McpAction::Tools { name } => {
            let configs = load_mcp_configs();
            let Some(cfg) = configs.iter().find(|c| c.name == name) else {
                anyhow::bail!("MCP server \"{}\" not found", name);
            };
            let client = connect_mcp_client(cfg).await?;
            let snapshot = client.tool_snapshot();
            println!("Tools from {} ({}):", name, snapshot.tools.len());
            for t in snapshot.tools.iter() {
                println!(
                    "  - {} — {}",
                    t.name,
                    t.description.as_deref().unwrap_or("(no description)")
                );
            }
        }
        McpAction::Resources { name } => {
            let configs = load_mcp_configs();
            let Some(cfg) = configs.iter().find(|c| c.name == name) else {
                anyhow::bail!("MCP server \"{}\" not found", name);
            };
            let client = connect_mcp_client(cfg).await?;
            match client.list_resources().await {
                Ok(resources) => {
                    println!("Resources from {} ({}):", name, resources.len());
                    for r in &resources {
                        println!("  - {} ({})", r.uri, r.name);
                        if let Some(desc) = &r.description {
                            println!("      {}", desc);
                        }
                    }
                }
                Err(e) => println!("  (resources not supported: {e})"),
            }
        }
        McpAction::Prompts { name } => {
            let configs = load_mcp_configs();
            let Some(cfg) = configs.iter().find(|c| c.name == name) else {
                anyhow::bail!("MCP server \"{}\" not found", name);
            };
            let client = connect_mcp_client(cfg).await?;
            match client.list_prompts().await {
                Ok(prompts) => {
                    println!("Prompts from {} ({}):", name, prompts.len());
                    for p in &prompts {
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
            }
        }
        McpAction::Import { file } => {
            let text = std::fs::read_to_string(&file)
                .map_err(|e| anyhow::anyhow!("read {}: {e}", file.display()))?;
            let servers = atman_runtime::mcp_config::parse_mcp_json(&text);
            if servers.is_empty() {
                println!("No MCP servers found in {}", file.display());
                return Ok(());
            }
            let imported = servers.len();
            let hub = atman_runtime::config_hub::ConfigHub::global()?;
            for server in servers {
                println!("✓ Imported \"{}\"", server.name);
                hub.upsert_mcp(server)?;
            }
            println!("Imported {imported} servers");
        }
    }
    Ok(())
}

async fn connect_mcp_client(
    cfg: &atman_runtime::mcp::McpServerConfig,
) -> anyhow::Result<std::sync::Arc<atman_runtime::mcp::McpClient>> {
    let client = match cfg.transport {
        atman_runtime::mcp::TransportKind::Stdio => {
            atman_runtime::mcp::McpClient::connect_stdio(
                &cfg.name,
                &cfg.command,
                &cfg.args,
                &cfg.env,
                cfg.timeout_ms,
            )
            .await
        }
        atman_runtime::mcp::TransportKind::Http | atman_runtime::mcp::TransportKind::Sse => {
            let url = cfg
                .url
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("missing url"))?;
            atman_runtime::mcp::McpClient::connect_http(
                &cfg.name,
                url,
                cfg.auth_token.clone(),
                cfg.timeout_ms,
            )
            .await
        }
    };
    client
        .map(std::sync::Arc::new)
        .map_err(|e| anyhow::anyhow!("{e}"))
}

fn cmd_mcp_add_interactive() -> anyhow::Result<()> {
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

    atman_runtime::config_hub::ConfigHub::global()?.upsert_mcp(server)?;
    println!("✓ Added MCP server \"{}\" to mcp_servers.json", name);
    println!("  Restart atman to apply.");
    Ok(())
}

async fn test_provider_endpoint(
    name: &str,
    entry: &atman_runtime::model_registry::ProviderEntry,
) -> (String, bool) {
    let provider = match atman_runtime::config_provider::build_config_provider(name, entry) {
        Ok(provider) => provider,
        Err(availability) => {
            let reason = match availability {
                atman_runtime::config_provider::ConfigProviderAvailability::Disabled => {
                    "is disabled"
                }
                atman_runtime::config_provider::ConfigProviderAvailability::MissingCredential => {
                    "has no available credential"
                }
                atman_runtime::config_provider::ConfigProviderAvailability::UnsupportedKind => {
                    "uses an unsupported provider kind"
                }
                _ => "is unavailable",
            };
            return (format!("\"{name}\" {reason}"), false);
        }
    };
    match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        provider.test_connection(),
    )
    .await
    {
        Ok(Ok(msg)) => (msg, true),
        Ok(Err(msg)) => (format!("\"{name}\" {msg}"), false),
        Err(_) => (format!("\"{name}\" timed out after 15s"), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use atman_runtime::fs_access::FsAccessMode;

    struct DropSignal(Option<tokio::sync::oneshot::Sender<()>>);

    impl Drop for DropSignal {
        fn drop(&mut self) {
            if let Some(sender) = self.0.take() {
                let _ = sender.send(());
            }
        }
    }

    const PNG_BYTES: &[u8] = &[
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d,
    ];

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
            assert!(run_projection_finished(&runs, &target));
        }

        let running = vec![run_projection(
            target.clone(),
            atman_proto::RunLifecycle::Running,
        )];
        assert!(!run_projection_finished(&running, &target));

        let other = vec![run_projection(
            atman_proto::FlowRunId(uuid::Uuid::now_v7()),
            atman_proto::RunLifecycle::Succeeded,
        )];
        assert!(!run_projection_finished(&other, &target));
    }

    async fn panic_provider_mutation() -> Result<atman_tui::ProviderMutationSuccess> {
        panic!("provider mutation panic fixture")
    }

    #[tokio::test]
    async fn dropping_tui_input_sink_closes_repl_input() {
        let session = std::sync::Arc::new(Session::open_ephemeral());
        let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
        let tui_input_sink = TuiControlInputSink::new(input_tx, session);

        drop(tui_input_sink);

        let received = tokio::time::timeout(std::time::Duration::from_secs(1), input_rx.recv())
            .await
            .expect("TUI input sink kept the REPL input channel open");
        assert!(received.is_none());
    }

    #[tokio::test]
    async fn consecutive_tui_submissions_keep_invocation_effort_isolated() {
        let session = std::sync::Arc::new(Session::open_ephemeral());
        let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
        let tui_input_sink = TuiControlInputSink::new(input_tx, session);

        tui_input_sink.send(atman_tui::TuiSubmission {
            text: "first".into(),
            images: Vec::new(),
            reasoning: Some(atman_runtime::provider::ReasoningSelection::Effort {
                effort: atman_runtime::provider::ReasoningEffort::High,
                execution_mode: None,
            }),
        });
        tui_input_sink.send(atman_tui::TuiSubmission {
            text: "second".into(),
            images: Vec::new(),
            reasoning: None,
        });

        let first = input_rx.recv().await.unwrap();
        let second = input_rx.recv().await.unwrap();
        assert!(matches!(
            first.invocation_env.get("effort"),
            Some(Value::Str(value)) if value == "high"
        ));
        assert!(second.invocation_env.get("effort").is_none());
    }

    #[tokio::test]
    async fn provider_mutation_panic_remains_fatal() {
        let request = atman_tui::ProviderMutationRequest::new(
            7,
            atman_tui::ProviderMutation::Refresh {
                provider_id: "provider-id".into(),
            },
        );
        let mut tasks = tokio::task::JoinSet::new();

        spawn_provider_mutation_task(&mut tasks, request, panic_provider_mutation());

        let error = tasks.join_next().await.unwrap().unwrap_err();
        assert!(error.is_panic());
    }

    #[tokio::test]
    async fn provider_mutation_shutdown_drops_in_flight_work() {
        let request = atman_tui::ProviderMutationRequest::new(
            8,
            atman_tui::ProviderMutation::Refresh {
                provider_id: "provider-id".into(),
            },
        );
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let mut tasks = tokio::task::JoinSet::new();
        spawn_provider_mutation_task(&mut tasks, request, async move {
            let _drop_signal = DropSignal(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<Result<atman_tui::ProviderMutationSuccess>>().await
        });
        started_rx.await.unwrap();

        tasks.shutdown().await;

        tokio::time::timeout(std::time::Duration::from_secs(1), dropped_rx)
            .await
            .expect("provider mutation future was not dropped")
            .unwrap();
    }

    #[tokio::test]
    async fn plain_repl_refresh_helper_consumes_entire_plan() {
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
            .block_on(execute_provider_mutation(
                &lifecycle,
                atman_tui::ProviderMutation::Login {
                    kind: atman_runtime::auth_store::ProviderKind::GitHubCopilot,
                    name: "Unsupported".into(),
                },
            ))
            .unwrap_err();
        assert!(login_error.to_string().contains("not supported"));

        let enable_error = runtime
            .block_on(execute_provider_mutation(
                &lifecycle,
                atman_tui::ProviderMutation::SetEnabled {
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
            .block_on(execute_provider_mutation(
                &lifecycle,
                atman_tui::ProviderMutation::Refresh {
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

        let create = atman_tui::ProviderMutation::UpsertConfig {
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
                .block_on(execute_provider_mutation(&lifecycle, create.clone()))
                .unwrap(),
            atman_tui::ProviderMutationSuccess::ConfigSaved {
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
                .block_on(execute_provider_mutation(&lifecycle, create))
                .unwrap_err()
                .to_string()
                .contains("already exists")
        );
        assert_eq!(hub.read_config_toml().unwrap(), before_duplicate);

        let disable = atman_tui::ProviderMutation::UpsertConfig {
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
            .block_on(execute_provider_mutation(&lifecycle, disable))
            .unwrap();
        assert!(!lifecycle.provider_registry().contains("config:gateway"));
        assert_eq!(
            hub.model_config().unwrap().unwrap().providers["gateway"].max_tokens,
            Some(16_384)
        );
    }

    #[test]
    fn switch_smart_model_commits_a_concrete_target_before_success() {
        let _registry = atman_runtime::model_registry::MODEL_CONFIG_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = tempfile::tempdir().unwrap();
        let hub = atman_runtime::config_hub::ConfigHub::from_config_dir(dir.path());
        hub.upsert_provider(atman_runtime::config_hub::ProviderConfigUpdate {
            name: "gateway",
            kind: "openai-compat",
            api_key: Some("test-key"),
            api_key_env: None,
            base_url: Some("http://localhost/v1"),
            max_tokens: None,
            reasoning_format: None,
            prompt_cache_key: None,
            enabled: true,
        })
        .unwrap();
        for (name, enabled) in [("old", true), ("new", true), ("disabled", false)] {
            hub.upsert_model(atman_runtime::model_registry::ModelConfigUpdate {
                old_name: None,
                name,
                model: name,
                provider: Some("gateway"),
                context_budget: 128_000,
                reasoning: atman_runtime::provider::ReasoningSelection::ProviderDefault,
                capabilities: None,
                image_detail: None,
                max_tokens: None,
                enabled,
            })
            .unwrap();
        }
        hub.add_alias("smart", "old").unwrap();
        hub.add_alias("cheap", "smart").unwrap();
        let session = Session::open_ephemeral();
        session.set_current_model("old");
        let providers = atman_runtime::provider::ProviderRegistry::new();

        let before = hub.read_config_toml().unwrap();
        assert!(switch_smart_model(&hub, &providers, &session, "new").is_err());
        assert_eq!(hub.read_config_toml().unwrap(), before);
        assert_eq!(session.last_model(), "old");

        providers.register(std::sync::Arc::new(
            atman_runtime::providers::mock::MockProvider::new("config:gateway"),
        ));
        assert_eq!(
            switch_smart_model(&hub, &providers, &session, "cheap").unwrap(),
            "old"
        );
        let config =
            atman_runtime::model_registry::parse_config(&hub.read_config_toml().unwrap()).unwrap();
        assert_eq!(config.aliases["smart"].model, "old");

        assert_eq!(
            switch_smart_model(&hub, &providers, &session, "new").unwrap(),
            "new"
        );
        assert_eq!(atman_runtime::model_registry::resolve_alias("smart"), "new");
        assert_eq!(session.last_model(), "new");

        let before = hub.read_config_toml().unwrap();
        assert!(switch_smart_model(&hub, &providers, &session, "disabled").is_err());
        assert_eq!(hub.read_config_toml().unwrap(), before);
        assert_eq!(session.last_model(), "new");

        atman_runtime::model_registry::set_provider_config(Default::default());
    }

    #[test]
    fn submitted_image_snapshot_does_not_drain_new_pending_images() {
        let session = Session::open_ephemeral();
        let submitted = session
            .import_image_bytes(PNG_BYTES, Some("submitted.png"))
            .unwrap();
        session
            .queue_image_bytes(&[PNG_BYTES, &[0x01]].concat(), Some("next.png"))
            .unwrap();

        let message = build_user_message(
            &session,
            "inspect",
            &[],
            Some(std::slice::from_ref(&submitted)),
            atman_runtime::event::TurnId::now(),
            atman_runtime::message::MessageOrigin::User,
        )
        .unwrap();

        assert!(matches!(
            &message.parts[0],
            atman_runtime::message::MessagePart::Image { source, .. } if source == &submitted
        ));
        assert_eq!(session.pending_image_count(), 1);
    }

    #[test]
    fn restoring_repl_images_keeps_them_before_new_pending_images() {
        let session = Session::open_ephemeral();
        let first = session
            .import_image_bytes(PNG_BYTES, Some("first.png"))
            .unwrap();
        let mut input = ReplInput {
            text: "/agent inspect".into(),
            images: Some(vec![first.clone()]),
            invocation_env: atman_runtime::InvocationEnv::default(),
        };
        session
            .queue_image_bytes(&[PNG_BYTES, &[0x01]].concat(), Some("second.png"))
            .unwrap();

        input.restore_images(&session);

        assert_eq!(session.take_pending_images()[0], first);
    }

    #[test]
    fn interjection_modes_build_expected_classifier() {
        use atman_runtime::config_hub::InterjectionMode;

        assert!(classifier_for_interjection_mode(Some(InterjectionMode::Off)).is_none());
        assert_eq!(
            classifier_for_interjection_mode(None).unwrap().kind(),
            "rule"
        );
        assert_eq!(
            classifier_for_interjection_mode(Some(InterjectionMode::Rule))
                .unwrap()
                .kind(),
            "rule"
        );
        assert_eq!(
            classifier_for_interjection_mode(Some(InterjectionMode::Llm))
                .unwrap()
                .kind(),
            "composed"
        );
        assert_eq!(
            classifier_for_interjection_mode(Some(InterjectionMode::Unknown("custom".into())))
                .unwrap()
                .kind(),
            "rule"
        );
    }

    #[test]
    fn suggest_model_uses_configured_value_or_default() {
        assert_eq!(select_suggest_model(Some("smart".into())), "smart");
        assert_eq!(select_suggest_model(Some(String::new())), "");
        assert_eq!(select_suggest_model(None), "gpt-4o-mini");
    }

    #[test]
    fn install_suggested_flow_retries_all_filename_collisions_without_overwriting() {
        let config = tempfile::tempdir().unwrap();
        let commands = config.path().join("commands");
        std::fs::create_dir(&commands).unwrap();
        for name in ["review", "review_v2", "review_v3"] {
            std::fs::write(commands.join(format!("{name}.at")), format!("old {name}")).unwrap();
        }

        let (name, path) = install_suggested_flow(
            config.path(),
            "review",
            "flow review(input: string) -> string { return input }",
        )
        .unwrap();

        assert_eq!(name, "review_v4");
        assert_eq!(path, commands.join("review_v4.at"));
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("flow review(")
        );
        for name in ["review", "review_v2", "review_v3"] {
            assert_eq!(
                std::fs::read_to_string(commands.join(format!("{name}.at"))).unwrap(),
                format!("old {name}")
            );
        }
        let routes = std::fs::read_to_string(config.path().join("routes.at")).unwrap();
        assert!(routes.contains("route \"review_v4 \" { flow: review_v4 }"));
    }

    #[test]
    fn install_suggested_flow_removes_new_command_when_route_append_fails() {
        let config = tempfile::tempdir().unwrap();
        std::fs::write(config.path().join("routes.at"), "route invalid").unwrap();

        let error = install_suggested_flow(
            config.path(),
            "review",
            "flow review(input: string) -> string { return input }",
        )
        .unwrap_err();

        assert!(error.to_string().contains("parse existing routes.at"));
        assert!(!config.path().join("commands/review.at").exists());
        assert_eq!(
            std::fs::read_to_string(config.path().join("routes.at")).unwrap(),
            "route invalid"
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

    #[test]
    fn fs_access_env_mode_overrides_config_mode() {
        assert_eq!(
            select_fs_access_mode(
                Some(FsAccessMode::ReadOnly),
                Some(FsAccessMode::DangerFullAccess),
            ),
            Some(FsAccessMode::ReadOnly)
        );
    }

    #[test]
    fn fs_access_config_mode_is_used_without_env_mode() {
        assert_eq!(
            select_fs_access_mode(None, Some(FsAccessMode::WorkspaceWrite)),
            Some(FsAccessMode::WorkspaceWrite)
        );
    }

    #[test]
    fn fs_access_mode_is_none_without_env_or_config() {
        assert_eq!(select_fs_access_mode(None, None), None);
    }
}
