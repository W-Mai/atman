use std::{
    future::Future,
    pin::Pin,
    sync::{Arc, mpsc::Sender},
    task::{Context, Poll, Wake, Waker},
    thread::{self, ThreadId},
};

use atman_rt::{
    EvalError, Source, SourceResolver, StatementOutcome, ToolRouter, Value, Vm, VmDelegates,
    resource::WithResources,
};

use crate::oneshot;

type Payload = WithResources<()>;

struct NoSources;

impl SourceResolver for NoSources {
    type Error = &'static str;

    fn resolve(&self, _importer_id: &str, _specifier: &str) -> Result<Source, Self::Error> {
        Err("the fixture has no imports")
    }
}

struct ThreadWake(thread::Thread);

impl Wake for ThreadWake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(ThreadWake(thread::current())));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match Pin::as_mut(&mut future).poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => thread::park(),
        }
    }
}

pub(crate) enum Command {
    Load {
        caller: ThreadId,
        name: String,
        lease_proxy: UiProxy,
        reply: oneshot::Sender<Result<SurfaceLease, EvalError>>,
    },
    Configure {
        caller: ThreadId,
        surface: u64,
        title: String,
        subtitle: String,
    },
    Status {
        caller: ThreadId,
        surface: u64,
        text: String,
        tone: String,
    },
    Metric {
        caller: ThreadId,
        surface: u64,
        slot: i64,
        label: String,
        value: String,
    },
    Stage {
        caller: ThreadId,
        surface: u64,
        index: i64,
        label: String,
        detail: String,
        accent: String,
        progress: f64,
    },
    Boundary {
        caller: ThreadId,
        surface: u64,
        state: String,
        note: String,
    },
    Present {
        caller: ThreadId,
        reply: oneshot::Sender<i64>,
    },
    Release {
        caller: ThreadId,
        surface: u64,
    },
}

impl Command {
    pub(crate) fn caller(&self) -> ThreadId {
        match self {
            Self::Load { caller, .. }
            | Self::Configure { caller, .. }
            | Self::Status { caller, .. }
            | Self::Metric { caller, .. }
            | Self::Stage { caller, .. }
            | Self::Boundary { caller, .. }
            | Self::Present { caller, .. }
            | Self::Release { caller, .. } => *caller,
        }
    }
}

#[derive(Clone)]
pub(crate) struct UiProxy {
    commands: Sender<Command>,
}

impl UiProxy {
    pub(crate) fn new(commands: Sender<Command>) -> Self {
        Self { commands }
    }

    fn send(&self, command: Command) -> Result<(), EvalError> {
        self.commands.send(command).map_err(|_| broken_bridge())
    }
}

#[atman_rt::resource]
pub(crate) struct SurfaceLease {
    id: u64,
    proxy: UiProxy,
    armed: bool,
}

impl SurfaceLease {
    pub(crate) fn new(id: u64, proxy: UiProxy) -> Self {
        Self {
            id,
            proxy,
            armed: true,
        }
    }

    pub(crate) fn disarm(mut self) -> u64 {
        self.armed = false;
        self.id
    }
}

impl Drop for SurfaceLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = self.proxy.commands.send(Command::Release {
            caller: thread::current().id(),
            surface: self.id,
        });
    }
}

struct UiHost {
    proxy: UiProxy,
}

#[atman_rt::tools(namespace = "ui")]
impl UiHost {
    /// Acquires a proxy lease for a main-thread mirui entity.
    #[tool]
    async fn load(&self, name: String) -> Result<SurfaceLease, EvalError> {
        let (reply, response) = oneshot::channel();
        self.proxy.send(Command::Load {
            caller: thread::current().id(),
            name,
            lease_proxy: self.proxy.clone(),
            reply,
        })?;
        response.await.map_err(|_| broken_bridge())?
    }

    /// Changes the dashboard title and subtitle.
    #[tool]
    fn configure(
        &self,
        surface: &SurfaceLease,
        title: String,
        subtitle: String,
    ) -> Result<(), EvalError> {
        self.proxy.send(Command::Configure {
            caller: thread::current().id(),
            surface: surface.id,
            title,
            subtitle,
        })
    }

    /// Changes the run status displayed in the header.
    #[tool]
    fn status(&self, surface: &SurfaceLease, text: String, tone: String) -> Result<(), EvalError> {
        if !matches!(tone.as_str(), "active" | "success" | "muted") {
            return Err(EvalError::TypeMismatch {
                expected: "active, success, or muted status tone".into(),
                actual: tone,
            });
        }
        self.proxy.send(Command::Status {
            caller: thread::current().id(),
            surface: surface.id,
            text,
            tone,
        })
    }

    /// Writes one of the three dashboard metrics.
    #[tool]
    fn metric(
        &self,
        surface: &SurfaceLease,
        slot: i64,
        label: String,
        value: String,
    ) -> Result<(), EvalError> {
        if !(1..=3).contains(&slot) {
            return Err(EvalError::TypeMismatch {
                expected: "metric slot 1..=3".into(),
                actual: slot.to_string(),
            });
        }
        self.proxy.send(Command::Metric {
            caller: thread::current().id(),
            surface: surface.id,
            slot,
            label,
            value,
        })
    }

    /// Applies one script-defined pipeline stage to the retained UI tree.
    #[tool]
    fn stage(
        &self,
        surface: &SurfaceLease,
        index: i64,
        label: String,
        detail: String,
        accent: String,
        progress: f64,
    ) -> Result<(), EvalError> {
        if !(1..=3).contains(&index) {
            return Err(EvalError::TypeMismatch {
                expected: "stage index 1..=3".into(),
                actual: index.to_string(),
            });
        }
        if !matches!(accent.as_str(), "sage" | "steel" | "mauve") {
            return Err(EvalError::TypeMismatch {
                expected: "sage, steel, or mauve accent".into(),
                actual: accent,
            });
        }
        if !(0.0..=1.0).contains(&progress) {
            return Err(EvalError::TypeMismatch {
                expected: "progress in 0.0..=1.0".into(),
                actual: progress.to_string(),
            });
        }
        self.proxy.send(Command::Stage {
            caller: thread::current().id(),
            surface: surface.id,
            index,
            label,
            detail,
            accent,
            progress,
        })
    }

    /// Changes the host-boundary state and explanatory note.
    #[tool]
    fn boundary(
        &self,
        surface: &SurfaceLease,
        state: String,
        note: String,
    ) -> Result<(), EvalError> {
        self.proxy.send(Command::Boundary {
            caller: thread::current().id(),
            surface: surface.id,
            state,
            note,
        })
    }

    /// Presents queued mutations and waits asynchronously for the frame slot.
    #[tool]
    async fn present(&self) -> Result<i64, EvalError> {
        let (reply, response) = oneshot::channel();
        self.proxy.send(Command::Present {
            caller: thread::current().id(),
            reply,
        })?;
        response.await.map_err(|_| broken_bridge())
    }
}

fn broken_bridge() -> EvalError {
    EvalError::TypeMismatch {
        expected: "live mirui owner-thread bridge".into(),
        actual: "bridge disconnected".into(),
    }
}

pub(crate) fn vm_worker(commands: Sender<Command>) -> Result<i64, String> {
    let vm = Vm::compile(Source::new("demo.at", include_str!("demo.at")), &NoSources)
        .map_err(|error| format!("invalid fixture program: {error}"))?;
    let mut tools = ToolRouter::<Payload, EvalError>::new();
    tools
        .mount(
            UiHost {
                proxy: UiProxy::new(commands),
            }
            .into_atman_binding(),
        )
        .map_err(|error| error.to_string())?;
    vm.validate_tools(&tools.catalog())
        .map_err(|error| error.to_string())?;

    match block_on(vm.run("render_frames", vec![], VmDelegates::new(tools))) {
        StatementOutcome::Return(Value::Int(frames)) => Ok(frames),
        StatementOutcome::Err(error) => Err(format!("render_frames failed: {error:?}")),
        StatementOutcome::Return(_) => Err("render_frames returned a non-integer".into()),
        _ => Err("render_frames did not return".into()),
    }
}
