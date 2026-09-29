mod oneshot;

use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
    task::{Context, Poll, Wake, Waker},
    thread::{self, ThreadId},
};

use atman_rt::{
    EvalError, Source, SourceResolver, StatementOutcome, ToolRouter, Value, Vm, VmDelegates,
    resource::WithResources,
};
use mirui::{
    app::App,
    ecs::Entity,
    prelude::Color,
    surface::{FramebufferAccess, framebuf::FramebufSurface},
    types::PhysicalRect,
    ui::Style,
};

type Payload = WithResources<()>;
type HeadlessApp = App<FramebufSurface<fn(&[u8], PhysicalRect)>>;

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

enum Command {
    Load {
        caller: ThreadId,
        name: String,
        lease_proxy: UiProxy,
        reply: oneshot::Sender<Result<SurfaceLease, EvalError>>,
    },
    Frame {
        caller: ThreadId,
        reply: oneshot::Sender<i64>,
    },
    Draw {
        caller: ThreadId,
        surface: u64,
        frame: i64,
    },
    Release {
        caller: ThreadId,
        surface: u64,
    },
}

#[derive(Clone)]
struct UiProxy {
    commands: Sender<Command>,
}

impl UiProxy {
    fn send(&self, command: Command) -> Result<(), EvalError> {
        self.commands.send(command).map_err(|_| broken_bridge())
    }
}

#[atman_rt::resource]
struct SurfaceLease {
    id: u64,
    proxy: UiProxy,
    armed: bool,
}

impl SurfaceLease {
    fn new(id: u64, proxy: UiProxy) -> Self {
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

    /// Advances the owner-thread frame counter.
    #[tool]
    async fn frame(&self) -> Result<i64, EvalError> {
        let (reply, response) = oneshot::channel();
        self.proxy.send(Command::Frame {
            caller: thread::current().id(),
            reply,
        })?;
        response.await.map_err(|_| broken_bridge())
    }

    /// Changes the leased entity and renders the headless framebuffer.
    #[tool]
    fn draw(&self, surface: &SurfaceLease, frame: i64) -> Result<(), EvalError> {
        self.proxy.send(Command::Draw {
            caller: thread::current().id(),
            surface: surface.id,
            frame,
        })
    }
}

fn broken_bridge() -> EvalError {
    EvalError::TypeMismatch {
        expected: "live mirui owner-thread bridge".into(),
        actual: "bridge disconnected".into(),
    }
}

struct MiruiOwner {
    owner: ThreadId,
    app: HeadlessApp,
    resources: BTreeMap<u64, Entity>,
    next_resource: u64,
    frame: i64,
    initial_pixels: u64,
    frame_pixels: Vec<u64>,
}

impl MiruiOwner {
    fn new() -> Result<Self, String> {
        let mut app: HeadlessApp = App::headless(32, 24);
        app.with_default_widgets().with_default_systems();
        app.spawn_root()
            .bg_color(Color::rgb(8, 12, 18))
            .ignore_safe_area()
            .id();
        app.render()
            .map_err(|error| format!("initial render: {error:?}"))?;
        let initial_pixels = framebuffer_hash(&mut app);
        Ok(Self {
            owner: thread::current().id(),
            app,
            resources: BTreeMap::new(),
            next_resource: 1,
            frame: 0,
            initial_pixels,
            frame_pixels: Vec::new(),
        })
    }

    fn handle(&mut self, command: Command) -> Result<(), String> {
        if thread::current().id() != self.owner {
            return Err("mirui App left its owner thread".into());
        }
        let caller = match &command {
            Command::Load { caller, .. }
            | Command::Frame { caller, .. }
            | Command::Draw { caller, .. }
            | Command::Release { caller, .. } => *caller,
        };
        if caller == self.owner {
            return Err("VM command unexpectedly originated on the mirui thread".into());
        }

        match command {
            Command::Load {
                name,
                lease_proxy,
                reply,
                ..
            } => {
                if name != "root-surface" {
                    reply
                        .send(Err(EvalError::TypeMismatch {
                            expected: "root-surface".into(),
                            actual: name,
                        }))
                        .map_err(|_| "load reply receiver closed".to_string())?;
                    return Ok(());
                }
                let entity = self
                    .app
                    .root
                    .ok_or_else(|| "mirui root missing".to_string())?;
                let id = self.next_resource;
                self.next_resource += 1;
                self.resources.insert(id, entity);
                let lease = SurfaceLease::new(id, lease_proxy);
                if let Err(Ok(lease)) = reply.send(Ok(lease)) {
                    self.release_surface(lease.disarm())?;
                }
            }
            Command::Frame { reply, .. } => {
                self.frame += 1;
                reply
                    .send(self.frame)
                    .map_err(|_| "frame reply receiver closed".to_string())?;
            }
            Command::Draw { surface, frame, .. } => {
                if frame != self.frame || !(1..=3).contains(&frame) {
                    return Err(format!(
                        "draw order mismatch: owner frame {}, command frame {frame}",
                        self.frame
                    ));
                }
                let entity = *self
                    .resources
                    .get(&surface)
                    .ok_or_else(|| format!("draw used released surface {surface}"))?;
                let color = match frame {
                    1 => Color::rgb(220, 40, 60),
                    2 => Color::rgb(30, 200, 90),
                    3 => Color::rgb(35, 90, 230),
                    _ => unreachable!(),
                };
                self.app
                    .world
                    .get_mut::<Style>(entity)
                    .ok_or_else(|| "root style missing".to_string())?
                    .set_bg_color(color);
                self.app.world.mark_subtree_dirty(entity);
                self.app
                    .render()
                    .map_err(|error| format!("frame {frame} render: {error:?}"))?;
                self.frame_pixels.push(framebuffer_hash(&mut self.app));
            }
            Command::Release { surface, .. } => {
                self.release_surface(surface)?;
            }
        }
        Ok(())
    }

    fn release_surface(&mut self, surface: u64) -> Result<(), String> {
        self.resources
            .remove(&surface)
            .ok_or_else(|| format!("surface {surface} released twice"))?;
        Ok(())
    }
}

fn framebuffer_hash(app: &mut HeadlessApp) -> u64 {
    app.backend
        .framebuffer()
        .buf
        .as_slice()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

fn vm_worker(commands: Sender<Command>) -> Result<i64, String> {
    let vm = Vm::compile(Source::new("demo.at", include_str!("demo.at")), &NoSources)
        .map_err(|error| format!("invalid fixture program: {error}"))?;
    let mut tools = ToolRouter::<Payload, EvalError>::new();
    tools
        .mount(
            UiHost {
                proxy: UiProxy { commands },
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

fn run_demo() -> Result<Vec<u64>, String> {
    let (commands, receiver): (Sender<Command>, Receiver<Command>) = mpsc::channel();
    let worker = thread::spawn(move || vm_worker(commands));
    let mut owner = MiruiOwner::new()?;
    while let Ok(command) = receiver.recv() {
        owner.handle(command)?;
    }
    let frames = worker
        .join()
        .map_err(|_| "VM worker panicked".to_string())??;
    if frames != 3 {
        return Err(format!("VM rendered {frames} frames instead of 3"));
    }
    if !owner.resources.is_empty() {
        return Err("VM resource release did not reach the mirui owner".into());
    }
    if owner.frame_pixels.len() != 3
        || owner.frame_pixels.contains(&owner.initial_pixels)
        || owner.frame_pixels[0] == owner.frame_pixels[1]
        || owner.frame_pixels[1] == owner.frame_pixels[2]
        || owner.frame_pixels[0] == owner.frame_pixels[2]
    {
        return Err(format!(
            "headless framebuffer did not change across frames: initial={}, frames={:?}",
            owner.initial_pixels, owner.frame_pixels
        ));
    }
    Ok(owner.frame_pixels)
}

fn main() {
    match run_demo() {
        Ok(hashes) => println!("mirui headless pixels changed across three VM frames: {hashes:?}"),
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
    fn vm_commands_change_headless_mirui_pixels() {
        let hashes = run_demo().unwrap();
        assert_eq!(hashes.len(), 3);
        assert_ne!(hashes[0], hashes[1]);
        assert_ne!(hashes[1], hashes[2]);
    }

    #[test]
    fn cancelled_surface_replies_release_owner_map_once() {
        let mut owner = MiruiOwner::new().unwrap();
        let caller = another_thread_id();

        let (closed_commands, closed_receiver) = mpsc::channel();
        let (closed_reply, closed_response) = oneshot::channel::<Result<SurfaceLease, EvalError>>();
        drop(closed_response);
        owner
            .handle(Command::Load {
                caller,
                name: "root-surface".into(),
                lease_proxy: UiProxy {
                    commands: closed_commands,
                },
                reply: closed_reply,
            })
            .unwrap();
        assert!(owner.resources.is_empty());
        assert!(matches!(
            closed_receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));

        let (unread_commands, unread_receiver) = mpsc::channel();
        let (unread_reply, unread_response) = oneshot::channel::<Result<SurfaceLease, EvalError>>();
        owner
            .handle(Command::Load {
                caller,
                name: "root-surface".into(),
                lease_proxy: UiProxy {
                    commands: unread_commands,
                },
                reply: unread_reply,
            })
            .unwrap();
        assert_eq!(owner.resources.len(), 1);

        thread::spawn(move || drop(unread_response)).join().unwrap();
        owner.handle(unread_receiver.recv().unwrap()).unwrap();
        assert!(owner.resources.is_empty());
        assert!(matches!(
            unread_receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));
    }

    fn another_thread_id() -> ThreadId {
        thread::spawn(|| thread::current().id()).join().unwrap()
    }
}
