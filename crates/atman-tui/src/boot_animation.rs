use anyhow::Result;
use atman_runtime::event::TurnId;
use atman_runtime::workflow::{
    NodeStatus, Parallelism, WorkflowGraph, WorkflowNode, WorkflowNodeKind,
};
use crossterm::event::{Event as CtEvent, KeyCode, KeyModifiers};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::widgets::Paragraph;
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootStepId {
    OpenSession,
    BuildExecutor,
    RegisterProviders,
    AttachMcp,
    AttachMemory,
    LoadTodos,
    Ready,
}

impl BootStepId {
    pub const ALL: &'static [BootStepId] = &[
        BootStepId::OpenSession,
        BootStepId::BuildExecutor,
        BootStepId::RegisterProviders,
        BootStepId::AttachMcp,
        BootStepId::AttachMemory,
        BootStepId::LoadTodos,
        BootStepId::Ready,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Self::OpenSession => "open session",
            Self::BuildExecutor => "build executor",
            Self::RegisterProviders => "register providers",
            Self::AttachMcp => "attach mcp servers",
            Self::AttachMemory => "attach memory stores",
            Self::LoadTodos => "load todos + plans",
            Self::Ready => "ready",
        }
    }

    pub fn id(self) -> String {
        format!("boot::{self:?}")
    }
}

#[derive(Debug, Clone)]
pub enum BootProgress {
    Start(BootStepId),
    Finish(BootStepId, NodeStatus),
}

pub fn build_boot_graph() -> WorkflowGraph {
    let mut root = Vec::with_capacity(BootStepId::ALL.len());
    for step in BootStepId::ALL {
        root.push(WorkflowNode {
            id: step.id(),
            kind: WorkflowNodeKind::Stmt {
                node_kind: atman_runtime::nodegraph::NodeKind::Message {
                    role: "boot".into(),
                },
            },
            label: step.label().into(),
            status: NodeStatus::Pending,
            started_at: None,
            ended_at: None,
            output_preview: None,
            children: Vec::new(),
            parallelism: Parallelism::Serial,
            approval: None,
            llm_stats: None,
        });
    }
    WorkflowGraph {
        turn_id: TurnId::now(),
        root,
    }
}

pub fn apply_progress(graph: &mut WorkflowGraph, event: &BootProgress) {
    let step = match event {
        BootProgress::Start(s) | BootProgress::Finish(s, _) => *s,
    };
    let now = chrono::Utc::now();
    let target = graph.root.iter_mut().find(|n| n.id == step.id());
    if let Some(node) = target {
        match event {
            BootProgress::Start(_) => {
                node.status = NodeStatus::Running;
                node.started_at = Some(now);
            }
            BootProgress::Finish(_, status) => {
                node.status = *status;
                node.ended_at = Some(now);
                if node.started_at.is_none() {
                    node.started_at = Some(now);
                }
            }
        }
    }
}

pub async fn run_boot_animation(
    mut rx: mpsc::UnboundedReceiver<BootProgress>,
    version: String,
    recent: Vec<crate::app::StartupSessionEntry>,
    toast_buf: std::sync::Arc<std::sync::Mutex<Vec<atman_runtime::notify::Notification>>>,
) -> Result<(
    Terminal<CrosstermBackend<std::io::Stdout>>,
    Vec<atman_runtime::notify::Notification>,
)> {
    use std::io::stdout;
    let backend = CrosstermBackend::new(stdout());
    let mut terminal = Terminal::new(backend)?;
    let mut graph = build_boot_graph();
    let mut spinner_tick = tokio::time::interval(std::time::Duration::from_millis(80));
    spinner_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut frame: u32 = 0;
    let mut ready_at: Option<std::time::Instant> = None;
    let hold_after_ready = std::time::Duration::from_millis(50);
    let min_step = std::time::Duration::from_millis(50);
    let mut current_step_started: Option<(BootStepId, std::time::Instant)> = None;
    let reveal_start = std::time::Instant::now();
    let reveal_ms_per_row: u128 = 60;
    let mut toasts: Vec<atman_runtime::notify::Notification> = Vec::new();
    let mut channel_closed = false;

    // Consume input events to prevent tty input buffer exhaustion during
    // boot animation. Without this, mouse events pile up in the OS buffer
    // until stdout writes block, causing a CPU-0% deadlock.
    let (mut key_events, reader_shutdown) = crate::spawn_event_reader();
    let mut sigterm = crate::build_sigterm_stream();

    let result: Result<(
        Terminal<CrosstermBackend<std::io::Stdout>>,
        Vec<atman_runtime::notify::Notification>,
    )> = loop {
        let reveal_count = ((reveal_start.elapsed().as_millis() / reveal_ms_per_row) as usize + 1)
            .min(recent.len());
        let settled = ready_at.is_some();
        // Drain new toasts.
        if let Ok(mut buf) = toast_buf.lock() {
            toasts.append(&mut buf);
        }
        // Expire old toasts based on TTL.
        let now = std::time::Instant::now();
        toasts.retain(|t| {
            let ttl = match t.lifecycle {
                atman_runtime::notify::NotifyLifecycle::Ttl(d) => d,
                _ => std::time::Duration::from_secs(10),
            };
            now.duration_since(t.created_at) < ttl
        });
        // Keep only the most recent 3.
        while toasts.len() > 3 {
            toasts.remove(0);
        }
        terminal.draw(|f| {
            render(
                f,
                &graph,
                frame,
                &version,
                &recent,
                reveal_count,
                settled,
                &toasts,
            )
        })?;
        tokio::select! {
            _ = spinner_tick.tick() => {
                frame = frame.wrapping_add(1);
            }
            ev = rx.recv() => {
                if channel_closed {
                    // Channel already closed; avoid tight loop.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
                match ev {
                    Some(progress) => {
                        if let BootProgress::Finish(step, _) = &progress {
                            if let Some((started_step, started_at)) = current_step_started {
                                if started_step == *step {
                                    let elapsed = started_at.elapsed();
                                    if elapsed < min_step {
                                        tokio::time::sleep(min_step - elapsed).await;
                                    }
                                }
                            }
                            current_step_started = None;
                        }
                        apply_progress(&mut graph, &progress);
                        if let BootProgress::Start(step) = progress {
                            current_step_started = Some((step, std::time::Instant::now()));
                        }
                        if let BootProgress::Finish(BootStepId::Ready, _) = progress {
                            ready_at = Some(std::time::Instant::now());
                        }
                    }
                    None => {
                        channel_closed = true;
                    }
                }
            }
            // Drain and discard all input events to keep the tty input
            // buffer empty. Ctrl+C / Esc aborts the boot animation early.
            ev = key_events.recv() => {
                if let Some(Ok(CtEvent::Key(ke))) = ev {
                    if ke.code == KeyCode::Char('c')
                        && ke.modifiers.contains(KeyModifiers::CONTROL)
                    {
                        break Ok((terminal, toasts));
                    }
                    if ke.code == KeyCode::Esc {
                        break Ok((terminal, toasts));
                    }
                }
            }
            _ = crate::wait_sigterm(sigterm.as_mut()) => {
                break Ok((terminal, toasts));
            }
        }
        let can_exit = ready_at
            .map(|t| t.elapsed() >= hold_after_ready)
            .unwrap_or(false)
            || channel_closed;
        if can_exit {
            break Ok((terminal, toasts));
        }
    };
    reader_shutdown.store(true, std::sync::atomic::Ordering::SeqCst);
    result
}

#[allow(clippy::too_many_arguments)]
fn render(
    f: &mut ratatui::Frame,
    graph: &WorkflowGraph,
    frame: u32,
    version: &str,
    recent: &[crate::app::StartupSessionEntry],
    reveal_count: usize,
    settled: bool,
    toasts: &[atman_runtime::notify::Notification],
) {
    let area = f.area();
    if area.width < 30 || area.height < 8 {
        return;
    }
    let l = crate::layout::compute_ex(area, 1);
    f.render_widget(
        crate::status::render_bar(crate::status::StatusInputs {
            session_id: "········",
            goal: None,
            streaming: false,
            waiting_for_llm: false,
            status_notes: &std::collections::HashMap::new(),
        }),
        l.status,
    );
    let splash = crate::output::compute_startup_overlay(l.transcript, recent);
    crate::output::render_startup_overlay(f, splash.area, version, recent, false, reveal_count);
    f.render_widget(
        crate::input::input_paragraph(
            "",
            0,
            crate::theme::theme().subtle_fg.into(),
            0,
            0,
            &atman_runtime::trust::TrustConfig::default(),
        ),
        splash.input_slot,
    );
    if settled {
        // Convert Notifications to ToastNotes and delegate to shared renderer.
        let toast_notes: Vec<crate::app::ToastNote> = toasts
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let level = match n.level {
                    atman_runtime::notify::NotifyLevel::Error => crate::app::NoteLevel::Error,
                    atman_runtime::notify::NotifyLevel::Warn => crate::app::NoteLevel::Warn,
                    atman_runtime::notify::NotifyLevel::Success => crate::app::NoteLevel::Success,
                    _ => crate::app::NoteLevel::Info,
                };
                crate::app::ToastNote {
                    id: format!("boot-{i}", i = i),
                    level,
                    message: n.message.clone(),
                    ttl: std::time::Duration::from_secs(10),
                    created: n.created_at,
                    position: crate::app::ToastPosition::TopRight,
                    fading: false,
                    fade_started: None,
                }
            })
            .collect();
        crate::render_toast_notes(f, area, &toast_notes);
        return;
    }
    let (mut lines, _) = crate::output::render_workflow_panel_with_regions(
        graph,
        &std::collections::HashSet::new(),
        false,
        false,
        frame,
        splash.banner_rect.width,
        10,
    );
    let cap = splash.banner_rect.height as usize;
    if lines.len() > cap {
        lines.truncate(cap);
    }
    while lines.len() < cap {
        lines.push(ratatui::text::Line::from(""));
    }
    f.render_widget(ratatui::widgets::Clear, splash.banner_rect);
    f.render_widget(Paragraph::new(lines), splash.banner_rect);
    // Convert Notifications to ToastNotes and delegate to shared renderer.
    let toast_notes: Vec<crate::app::ToastNote> = toasts
        .iter()
        .enumerate()
        .map(|(i, n)| {
            let level = match n.level {
                atman_runtime::notify::NotifyLevel::Error => crate::app::NoteLevel::Error,
                atman_runtime::notify::NotifyLevel::Warn => crate::app::NoteLevel::Warn,
                atman_runtime::notify::NotifyLevel::Success => crate::app::NoteLevel::Success,
                _ => crate::app::NoteLevel::Info,
            };
            crate::app::ToastNote {
                id: format!("boot-{i}", i = i),
                level,
                message: n.message.clone(),
                ttl: std::time::Duration::from_secs(10),
                created: n.created_at,
                position: crate::app::ToastPosition::TopRight,
                fading: false,
                fade_started: None,
            }
        })
        .collect();
    crate::render_toast_notes(f, area, &toast_notes);
}
