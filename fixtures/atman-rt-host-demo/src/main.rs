mod oneshot;

use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, Sender},
    },
    task::{Context, Poll, Wake, Waker},
    thread::{self, ThreadId},
};

use atman_rt::{
    CancellationDelegate, EvalError, HostFuture, Source, SourceResolver, StatementOutcome,
    ToolRouter, Value, Vm, VmContext, VmDelegates, resource::WithResources,
};

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

#[derive(Default)]
struct CancellationSignal {
    cancelled: AtomicBool,
    waiter: Mutex<Option<Waker>>,
}

impl CancellationSignal {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        let waiter = self.waiter.lock().unwrap().take();
        if let Some(waiter) = waiter {
            waiter.wake();
        }
    }
}

#[derive(Clone)]
struct WakeableCancellation {
    signal: Arc<CancellationSignal>,
}

impl CancellationDelegate<EvalError> for WakeableCancellation {
    fn cancellation_error(&self, _context: &VmContext) -> Option<EvalError> {
        self.signal
            .cancelled
            .load(Ordering::SeqCst)
            .then(cancelled_error)
    }

    fn cancelled<'a>(&'a self, _context: &'a VmContext) -> HostFuture<'a, EvalError> {
        Box::pin(std::future::poll_fn(move |context| {
            if self.signal.cancelled.load(Ordering::SeqCst) {
                return Poll::Ready(cancelled_error());
            }
            let mut waiter = self.signal.waiter.lock().unwrap();
            if self.signal.cancelled.load(Ordering::SeqCst) {
                Poll::Ready(cancelled_error())
            } else {
                if !waiter
                    .as_ref()
                    .is_some_and(|waiter| waiter.will_wake(context.waker()))
                {
                    *waiter = Some(context.waker().clone());
                }
                Poll::Pending
            }
        }))
    }

    fn is_cancellation(&self, error: &EvalError) -> bool {
        is_fixture_cancellation(error)
    }
}

fn cancelled_error() -> EvalError {
    EvalError::MissingArgument("owner reply cancelled".into())
}

fn is_fixture_cancellation(error: &EvalError) -> bool {
    matches!(error, EvalError::MissingArgument(message) if message == "owner reply cancelled")
}

enum Command {
    Load {
        caller: ThreadId,
        name: String,
        lease_proxy: GraphicsProxy,
        reply: oneshot::Sender<Result<TextureLease, EvalError>>,
    },
    Frame {
        caller: ThreadId,
        reply: oneshot::Sender<i64>,
    },
    Draw {
        caller: ThreadId,
        texture: u64,
        frame: i64,
        index: i64,
    },
    Release {
        caller: ThreadId,
        texture: u64,
    },
    CancelAfterLoad {
        caller: ThreadId,
        name: String,
    },
}

#[derive(Clone)]
struct GraphicsProxy {
    commands: Sender<Command>,
}

impl GraphicsProxy {
    fn send(&self, command: Command) -> Result<(), EvalError> {
        self.commands.send(command).map_err(|_| broken_bridge())
    }
}

#[atman_rt::resource]
struct TextureLease {
    id: u64,
    proxy: GraphicsProxy,
    armed: bool,
}

impl TextureLease {
    fn new(id: u64, proxy: GraphicsProxy) -> Self {
        Self {
            id,
            proxy,
            armed: true,
        }
    }

    fn disarm(mut self) -> u64 {
        self.armed = false;
        self.id
    }
}

impl Drop for TextureLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let _ = self.proxy.commands.send(Command::Release {
            caller: thread::current().id(),
            texture: self.id,
        });
    }
}

struct GraphicsHost {
    proxy: GraphicsProxy,
}

#[atman_rt::tools(namespace = "demo")]
impl GraphicsHost {
    /// Loads one owner-thread texture and returns its VM-side lease.
    #[tool]
    async fn load(&self, name: String) -> Result<TextureLease, EvalError> {
        let cancel_after_load = (name == "cancelled-owner-reply").then(|| name.clone());
        let (reply, response) = oneshot::channel::<Result<TextureLease, EvalError>>();
        self.proxy.send(Command::Load {
            caller: thread::current().id(),
            name,
            lease_proxy: self.proxy.clone(),
            reply,
        })?;
        let lease = response.await.map_err(|_| broken_bridge())??;
        if let Some(name) = cancel_after_load {
            self.proxy.send(Command::CancelAfterLoad {
                caller: thread::current().id(),
                name,
            })?;
            std::future::pending::<()>().await;
        }
        Ok(lease)
    }

    /// Advances the owner-thread frame clock.
    #[tool]
    async fn frame(&self) -> Result<i64, EvalError> {
        let (reply, response) = oneshot::channel();
        self.proxy.send(Command::Frame {
            caller: thread::current().id(),
            reply,
        })?;
        response.await.map_err(|_| broken_bridge())
    }

    /// Queues one ordered draw against a borrowed resource.
    #[tool]
    fn draw(&self, texture: &TextureLease, frame: i64, index: i64) -> Result<(), EvalError> {
        self.proxy.send(Command::Draw {
            caller: thread::current().id(),
            texture: texture.id,
            frame,
            index,
        })
    }
}

fn broken_bridge() -> EvalError {
    EvalError::TypeMismatch {
        expected: "live owner-thread bridge".into(),
        actual: "bridge disconnected".into(),
    }
}

#[derive(Debug)]
struct FakeTexture {
    name: String,
    pixels: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq)]
enum AuditEvent {
    Load {
        texture: u64,
        name: String,
    },
    Frame(i64),
    Draw {
        texture: u64,
        frame: i64,
        index: i64,
    },
    Release(u64),
    PendingLoad(String),
}

struct FakeWorld {
    owner: ThreadId,
    next_texture: u64,
    frame: i64,
    textures: BTreeMap<u64, FakeTexture>,
    audit: Vec<AuditEvent>,
    cancellation: Arc<CancellationSignal>,
}

impl FakeWorld {
    fn new(cancellation: Arc<CancellationSignal>) -> Self {
        Self {
            owner: thread::current().id(),
            next_texture: 1,
            frame: 0,
            textures: BTreeMap::new(),
            audit: Vec::new(),
            cancellation,
        }
    }

    fn handle(&mut self, command: Command) -> Result<(), String> {
        if thread::current().id() != self.owner {
            return Err("fake world left its owner thread".into());
        }
        let caller = match &command {
            Command::Load { caller, .. }
            | Command::Frame { caller, .. }
            | Command::Draw { caller, .. }
            | Command::Release { caller, .. }
            | Command::CancelAfterLoad { caller, .. } => *caller,
        };
        if caller == self.owner {
            return Err("VM command unexpectedly originated on the owner thread".into());
        }

        match command {
            Command::Load {
                name,
                lease_proxy,
                reply,
                ..
            } => {
                let id = self.next_texture;
                self.next_texture += 1;
                self.textures.insert(
                    id,
                    FakeTexture {
                        name: name.clone(),
                        pixels: vec![0; 16],
                    },
                );
                self.audit.push(AuditEvent::Load { texture: id, name });
                let lease = TextureLease::new(id, lease_proxy);
                if let Err(Ok(lease)) = reply.send(Ok(lease)) {
                    self.release_texture(lease.disarm())?;
                }
            }
            Command::Frame { reply, .. } => {
                self.frame += 1;
                self.audit.push(AuditEvent::Frame(self.frame));
                reply
                    .send(self.frame)
                    .map_err(|_| "frame reply receiver closed".to_string())?;
            }
            Command::Draw {
                texture,
                frame,
                index,
                ..
            } => {
                if frame != self.frame || index != frame - 1 {
                    return Err(format!(
                        "draw order mismatch: owner frame {}, command frame {frame}, index {index}",
                        self.frame
                    ));
                }
                let target = self
                    .textures
                    .get_mut(&texture)
                    .ok_or_else(|| format!("draw used missing texture {texture}"))?;
                target.pixels[index as usize] = frame as u8;
                self.audit.push(AuditEvent::Draw {
                    texture,
                    frame,
                    index,
                });
            }
            Command::Release { texture, .. } => {
                self.release_texture(texture)?;
            }
            Command::CancelAfterLoad { name, .. } => {
                self.audit.push(AuditEvent::PendingLoad(name));
                self.cancellation.cancel();
            }
        }
        Ok(())
    }

    fn release_texture(&mut self, texture: u64) -> Result<(), String> {
        let released = self
            .textures
            .remove(&texture)
            .ok_or_else(|| format!("texture {texture} released twice"))?;
        if released.name == "checkerboard" && released.pixels[..3] != [1, 2, 3] {
            return Err("checkerboard was released before all frames were drawn".into());
        }
        self.audit.push(AuditEvent::Release(texture));
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct WorkerReport {
    cold_result: i64,
    rendered_frames: i64,
    stale_error: String,
    cancelled_wait: bool,
}

fn vm_worker(
    commands: Sender<Command>,
    cancellation: Arc<CancellationSignal>,
) -> Result<WorkerReport, String> {
    let vm = Vm::compile(Source::new("demo.at", include_str!("demo.at")), &NoSources)
        .map_err(|error| format!("invalid fixture program: {error}"))?;
    let host = GraphicsHost {
        proxy: GraphicsProxy { commands },
    };
    let mut tools = ToolRouter::<Payload, EvalError>::new();
    tools
        .mount(host.into_atman_binding())
        .map_err(|error| error.to_string())?;
    vm.validate_tools(&tools.catalog())
        .map_err(|error| error.to_string())?;

    let cold_result = match block_on(vm.run("cold_async", vec![], VmDelegates::new(tools.clone())))
    {
        StatementOutcome::Return(Value::Int(value)) => value,
        StatementOutcome::Err(error) => return Err(format!("cold_async failed: {error:?}")),
        StatementOutcome::Return(_) => return Err("cold_async returned a non-integer".into()),
        _ => return Err("cold_async did not return".into()),
    };
    let rendered_frames =
        match block_on(vm.run("render_frames", vec![], VmDelegates::new(tools.clone()))) {
            StatementOutcome::Return(Value::Int(value)) => value,
            StatementOutcome::Err(error) => {
                return Err(format!("render_frames failed: {error:?}"));
            }
            StatementOutcome::Return(_) => {
                return Err("render_frames returned a non-integer".into());
            }
            _ => return Err("render_frames did not return".into()),
        };
    let stale_error =
        match block_on(vm.run("stale_handle", vec![], VmDelegates::new(tools.clone()))) {
            StatementOutcome::Err(error) => format!("{error:?}"),
            StatementOutcome::Return(_) => return Err("stale_handle unexpectedly returned".into()),
            _ => return Err("stale_handle did not fail".into()),
        };
    if !stale_error.contains("stale") {
        return Err(format!(
            "stale handle diagnostic was not preserved: {stale_error}"
        ));
    }
    let cancelled_wait = match block_on(vm.run(
        "cancelled_owner_reply",
        vec![],
        VmDelegates::new(tools).with_cancellation(WakeableCancellation {
            signal: cancellation,
        }),
    )) {
        StatementOutcome::Err(error) if is_fixture_cancellation(&error) => true,
        StatementOutcome::Err(error) => {
            return Err(format!("cancelled_owner_reply failed: {error:?}"));
        }
        _ => return Err("cancelled_owner_reply was not cancelled".into()),
    };

    Ok(WorkerReport {
        cold_result,
        rendered_frames,
        stale_error,
        cancelled_wait,
    })
}

fn run_demo() -> Result<(WorkerReport, Vec<AuditEvent>), String> {
    let (commands, receiver): (Sender<Command>, Receiver<Command>) = mpsc::channel();
    let cancellation = Arc::new(CancellationSignal::default());
    let worker_cancellation = Arc::clone(&cancellation);
    let worker = thread::spawn(move || vm_worker(commands, worker_cancellation));
    let mut world = FakeWorld::new(cancellation);
    while let Ok(command) = receiver.recv() {
        world.handle(command)?;
    }
    let report = worker
        .join()
        .map_err(|_| "VM worker panicked".to_string())??;

    if report.cold_result != 1 || report.rendered_frames != 3 || !report.cancelled_wait {
        return Err(format!("unexpected VM report: {report:?}"));
    }
    if world
        .audit
        .iter()
        .any(|event| matches!(event, AuditEvent::Load { name, .. } if name == "never-driven"))
    {
        return Err("cold async load ran without being awaited".into());
    }
    if !world.textures.is_empty() {
        return Err(format!("owner leaked {} textures", world.textures.len()));
    }
    let expected = [
        AuditEvent::Load {
            texture: 1,
            name: "checkerboard".into(),
        },
        AuditEvent::Frame(1),
        AuditEvent::Draw {
            texture: 1,
            frame: 1,
            index: 0,
        },
        AuditEvent::Frame(2),
        AuditEvent::Draw {
            texture: 1,
            frame: 2,
            index: 1,
        },
        AuditEvent::Frame(3),
        AuditEvent::Draw {
            texture: 1,
            frame: 3,
            index: 2,
        },
        AuditEvent::Release(1),
        AuditEvent::Load {
            texture: 2,
            name: "stale-probe".into(),
        },
        AuditEvent::Release(2),
        AuditEvent::Load {
            texture: 3,
            name: "cancelled-owner-reply".into(),
        },
        AuditEvent::PendingLoad("cancelled-owner-reply".into()),
        AuditEvent::Release(3),
    ];
    if world.audit != expected {
        return Err(format!("unexpected owner audit: {:?}", world.audit));
    }
    Ok((report, world.audit))
}

fn main() {
    match run_demo() {
        Ok((report, audit)) => println!(
            "owner-thread bridge ok: {} frames, {} owner audit events, cold async stayed idle, cancellation released its owner resource, {}",
            report.rendered_frames,
            audit.len(),
            report.stale_error
        ),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exercises_owner_thread_resources_cancellation_and_cold_tools() {
        let (report, audit) = run_demo().unwrap();
        assert_eq!(report.cold_result, 1);
        assert_eq!(report.rendered_frames, 3);
        assert!(report.stale_error.contains("stale"));
        assert!(report.cancelled_wait);
        assert_eq!(audit.len(), 13);
        assert_eq!(audit.last(), Some(&AuditEvent::Release(3)));
    }

    #[test]
    fn closed_resource_reply_is_removed_without_queuing_release() {
        let (commands, receiver) = mpsc::channel();
        let cancellation = Arc::new(CancellationSignal::default());
        let mut world = FakeWorld::new(cancellation);
        let (reply, response) = oneshot::channel::<Result<TextureLease, EvalError>>();
        drop(response);

        world
            .handle(Command::Load {
                caller: another_thread_id(),
                name: "closed-reply".into(),
                lease_proxy: GraphicsProxy { commands },
                reply,
            })
            .unwrap();

        assert!(world.textures.is_empty());
        assert_eq!(
            world.audit,
            [
                AuditEvent::Load {
                    texture: 1,
                    name: "closed-reply".into(),
                },
                AuditEvent::Release(1),
            ]
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn unread_resource_reply_releases_owner_map_once() {
        let (commands, receiver) = mpsc::channel();
        let cancellation = Arc::new(CancellationSignal::default());
        let mut world = FakeWorld::new(cancellation);
        let (reply, response) = oneshot::channel::<Result<TextureLease, EvalError>>();

        world
            .handle(Command::Load {
                caller: another_thread_id(),
                name: "unread-reply".into(),
                lease_proxy: GraphicsProxy { commands },
                reply,
            })
            .unwrap();
        assert_eq!(world.textures.len(), 1);

        thread::spawn(move || drop(response)).join().unwrap();
        world.handle(receiver.recv().unwrap()).unwrap();

        assert!(world.textures.is_empty());
        assert_eq!(
            world.audit,
            [
                AuditEvent::Load {
                    texture: 1,
                    name: "unread-reply".into(),
                },
                AuditEvent::Release(1),
            ]
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    fn another_thread_id() -> ThreadId {
        thread::spawn(|| thread::current().id()).join().unwrap()
    }
}
