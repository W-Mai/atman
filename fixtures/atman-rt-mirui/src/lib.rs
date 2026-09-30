mod bridge;
mod dashboard;
mod oneshot;

use std::{
    collections::BTreeMap,
    sync::mpsc::{self, Receiver, Sender},
    thread::{self, JoinHandle, ThreadId},
};

#[cfg(feature = "sdl")]
use std::time::{Duration, Instant};

use bridge::{Command, SurfaceLease, UiProxy};
use dashboard::DashboardNodes;
use mirui::{
    app::App,
    ecs::Entity,
    prelude::ColorToken,
    surface::{FramebufferAccess, Surface, framebuf::FramebufSurface},
    types::PhysicalRect,
};

type HeadlessBackend = FramebufSurface<fn(&[u8], PhysicalRect)>;
type HeadlessOwner = MiruiOwner<HeadlessBackend>;

#[cfg(feature = "sdl")]
struct PendingFrame {
    deadline: Instant,
    value: i64,
    reply: oneshot::Sender<i64>,
}

#[cfg(feature = "sdl")]
pub enum SdlRun {
    Completed(Vec<u64>),
    ClosedEarly { rendered_frames: usize },
}

struct MiruiOwner<B>
where
    B: Surface + FramebufferAccess,
{
    owner: ThreadId,
    app: App<B>,
    nodes: DashboardNodes,
    resources: BTreeMap<u64, Entity>,
    next_resource: u64,
    frame: i64,
    initial_pixels: u64,
    frame_pixels: Vec<u64>,
    #[cfg(feature = "sdl")]
    frame_delay: Duration,
    #[cfg(feature = "sdl")]
    pending_frame: Option<PendingFrame>,
}

impl<B> MiruiOwner<B>
where
    B: Surface + FramebufferAccess,
{
    fn new(mut app: App<B>) -> Result<Self, String> {
        app.with_default_widgets()
            .with_default_systems()
            .with_theme(dashboard::theme());
        let root = app
            .spawn_root()
            .bg_color(ColorToken::Surface)
            .ignore_safe_area()
            .id();
        let nodes = DashboardNodes::build(&mut app.world, root)?;
        app.render()
            .map_err(|error| format!("initial render: {error:?}"))?;
        let initial_pixels = framebuffer_hash(&mut app);
        Ok(Self {
            owner: thread::current().id(),
            app,
            nodes,
            resources: BTreeMap::new(),
            next_resource: 1,
            frame: 0,
            initial_pixels,
            frame_pixels: Vec::new(),
            #[cfg(feature = "sdl")]
            frame_delay: Duration::ZERO,
            #[cfg(feature = "sdl")]
            pending_frame: None,
        })
    }

    #[cfg(feature = "sdl")]
    fn with_frame_delay(mut self, delay: Duration) -> Self {
        self.frame_delay = delay;
        self
    }

    fn handle(&mut self, command: Command) -> Result<(), String> {
        if thread::current().id() != self.owner {
            return Err("mirui App left its owner thread".into());
        }
        if command.caller() == self.owner {
            return Err("VM command unexpectedly originated on the mirui thread".into());
        }

        match command {
            Command::Load {
                name,
                lease_proxy,
                reply,
                ..
            } => self.load_surface(name, lease_proxy, reply)?,
            Command::Configure {
                surface,
                title,
                subtitle,
                ..
            } => {
                self.ensure_surface(surface)?;
                self.nodes.configure(&mut self.app.world, title, subtitle);
            }
            Command::Status {
                surface,
                text,
                tone,
                ..
            } => {
                self.ensure_surface(surface)?;
                self.nodes.status(&mut self.app.world, text, tone)?;
            }
            Command::Metric {
                surface,
                slot,
                label,
                value,
                ..
            } => {
                self.ensure_surface(surface)?;
                self.nodes.metric(&mut self.app.world, slot, label, value)?;
            }
            Command::Stage {
                surface,
                index,
                label,
                detail,
                accent,
                progress,
                ..
            } => {
                self.ensure_surface(surface)?;
                self.nodes
                    .stage(&mut self.app.world, index, label, detail, accent, progress)?;
            }
            Command::Boundary {
                surface,
                state,
                note,
                ..
            } => {
                self.ensure_surface(surface)?;
                self.nodes.boundary(&mut self.app.world, state, note);
            }
            Command::Present { reply, .. } => self.present(reply)?,
            Command::Release { surface, .. } => self.release_surface(surface)?,
        }
        Ok(())
    }

    fn load_surface(
        &mut self,
        name: String,
        lease_proxy: UiProxy,
        reply: oneshot::Sender<Result<SurfaceLease, atman_rt::EvalError>>,
    ) -> Result<(), String> {
        if name != "runtime-canvas" {
            reply
                .send(Err(atman_rt::EvalError::TypeMismatch {
                    expected: "runtime-canvas".into(),
                    actual: name,
                }))
                .map_err(|_| "load reply receiver closed".to_string())?;
            return Ok(());
        }

        let id = self.next_resource;
        self.next_resource += 1;
        self.resources.insert(id, self.nodes.canvas);

        let lease = SurfaceLease::new(id, lease_proxy);
        if let Err(Ok(lease)) = reply.send(Ok(lease)) {
            self.release_surface(lease.disarm())?;
        }
        Ok(())
    }

    fn present(&mut self, reply: oneshot::Sender<i64>) -> Result<(), String> {
        if self.resources.is_empty() {
            return Err("present requires an active surface lease".into());
        }
        #[cfg(feature = "sdl")]
        if self.pending_frame.is_some() {
            return Err("received overlapping present requests".into());
        }

        let value = self.frame + 1;
        self.frame = value;
        self.render(&format!("frame {value}"))?;
        self.frame_pixels.push(framebuffer_hash(&mut self.app));

        #[cfg(feature = "sdl")]
        if !self.frame_delay.is_zero() {
            self.pending_frame = Some(PendingFrame {
                deadline: Instant::now() + self.frame_delay,
                value,
                reply,
            });
            return Ok(());
        }

        reply
            .send(value)
            .map_err(|_| "frame reply receiver closed".to_string())
    }

    fn ensure_surface(&self, surface: u64) -> Result<Entity, String> {
        let entity = *self
            .resources
            .get(&surface)
            .ok_or_else(|| format!("command used released surface {surface}"))?;
        if entity != self.nodes.canvas {
            return Err(format!("surface {surface} is not the runtime canvas"));
        }
        Ok(entity)
    }

    fn release_surface(&mut self, surface: u64) -> Result<(), String> {
        self.resources
            .remove(&surface)
            .ok_or_else(|| format!("surface {surface} released twice"))?;
        self.render("surface release")
    }

    fn render(&mut self, context: &str) -> Result<(), String> {
        self.app
            .render()
            .map_err(|error| format!("{context} render: {error:?}"))
    }

    #[cfg(feature = "sdl")]
    fn advance_frame_timer(&mut self) -> Result<(), String> {
        let ready = self
            .pending_frame
            .as_ref()
            .is_some_and(|pending| Instant::now() >= pending.deadline);
        if !ready {
            return Ok(());
        }
        let pending = self.pending_frame.take().expect("ready frame exists");
        self.frame = pending.value;
        pending
            .reply
            .send(pending.value)
            .map_err(|_| "frame reply receiver closed".to_string())
    }

    #[cfg(feature = "sdl")]
    fn next_wait(&self) -> Duration {
        let tick = Duration::from_millis(16);
        self.pending_frame
            .as_ref()
            .map(|pending| {
                pending
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .min(tick)
            })
            .unwrap_or(tick)
    }

    #[cfg(feature = "sdl")]
    fn poll_quit(&mut self) -> bool {
        use mirui::surface::InputEvent;

        while let Some(event) = self.app.poll_event() {
            if matches!(event, InputEvent::Quit) {
                return true;
            }
        }
        false
    }
}

fn framebuffer_hash<B>(app: &mut App<B>) -> u64
where
    B: Surface + FramebufferAccess,
{
    app.backend
        .framebuffer()
        .buf
        .as_slice()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

fn start_worker() -> (Receiver<Command>, JoinHandle<Result<i64, String>>) {
    let (commands, receiver): (Sender<Command>, Receiver<Command>) = mpsc::channel();
    let worker = thread::spawn(move || bridge::vm_worker(commands));
    (receiver, worker)
}

fn validate_completed_demo<B>(owner: &MiruiOwner<B>, frames: i64) -> Result<(), String>
where
    B: Surface + FramebufferAccess,
{
    validate_completed_state(
        owner.resources.is_empty(),
        owner.frame,
        owner.initial_pixels,
        &owner.frame_pixels,
        frames,
    )
}

fn validate_completed_state(
    resources_empty: bool,
    rendered_frames: i64,
    initial_pixels: u64,
    frame_pixels: &[u64],
    frames: i64,
) -> Result<(), String> {
    let expected_frames = usize::try_from(frames)
        .ok()
        .filter(|frames| *frames > 0)
        .ok_or_else(|| format!("VM returned invalid frame count {frames}"))?;
    if rendered_frames != frames {
        return Err(format!(
            "VM returned {frames} frames after the owner rendered {rendered_frames}"
        ));
    }
    if !resources_empty {
        return Err("VM resource release did not reach the mirui owner".into());
    }
    if frame_pixels.len() != expected_frames
        || frame_pixels.contains(&initial_pixels)
        || frame_pixels.windows(2).any(|frames| frames[0] == frames[1])
    {
        return Err(format!(
            "framebuffer did not change across frames: initial={}, frames={:?}",
            initial_pixels, frame_pixels
        ));
    }
    Ok(())
}

fn new_headless_owner() -> Result<HeadlessOwner, String> {
    MiruiOwner::new(App::headless(800, 480))
}

pub fn run_headless_demo() -> Result<Vec<u64>, String> {
    let (receiver, worker) = start_worker();
    let mut owner = new_headless_owner()?;
    while let Ok(command) = receiver.recv() {
        owner.handle(command)?;
    }
    let frames = worker
        .join()
        .map_err(|_| "VM worker panicked".to_string())??;
    validate_completed_demo(&owner, frames)?;
    Ok(owner.frame_pixels)
}

#[cfg(feature = "sdl")]
pub fn run_sdl_demo() -> Result<SdlRun, String> {
    use mirui::surface::sdl::SdlSurface;

    let auto_close = std::env::var_os("ATMAN_MIRUI_AUTOCLOSE").is_some();
    let (receiver, worker) = start_worker();
    let app = App::new(SdlSurface::new("Atman Runtime / mirui", 900, 560));
    let mut owner = MiruiOwner::new(app)?.with_frame_delay(Duration::from_millis(620));
    let mut worker = Some(worker);
    let mut completed = false;
    let mut user_closed = false;

    loop {
        if worker.is_some() {
            match receiver.recv_timeout(owner.next_wait()) {
                Ok(command) => owner.handle(command)?,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    let frames = worker
                        .take()
                        .expect("worker is present")
                        .join()
                        .map_err(|_| "VM worker panicked".to_string())??;
                    validate_completed_demo(&owner, frames)?;
                    completed = true;
                    eprintln!("VM run complete. Press Esc or close the window to exit.");
                }
            }
        } else {
            thread::sleep(owner.next_wait());
        }

        owner.advance_frame_timer()?;
        if owner.poll_quit() {
            user_closed = true;
            break;
        }
        if auto_close && completed {
            break;
        }
    }

    let resources_empty = owner.resources.is_empty();
    let rendered_frames = owner.frame;
    let initial_pixels = owner.initial_pixels;
    let hashes = owner.frame_pixels.clone();
    drop(receiver);
    drop(owner);
    let late_result = worker
        .map(|worker| worker.join().map_err(|_| "VM worker panicked".to_string()))
        .transpose()?;
    if user_closed && !completed {
        match late_result {
            Some(Ok(frames))
                if validate_completed_state(
                    resources_empty,
                    rendered_frames,
                    initial_pixels,
                    &hashes,
                    frames,
                )
                .is_ok() =>
            {
                return Ok(SdlRun::Completed(hashes));
            }
            _ => {}
        }
        Ok(SdlRun::ClosedEarly {
            rendered_frames: hashes.len(),
        })
    } else {
        if let Some(result) = late_result {
            result?;
        }
        Ok(SdlRun::Completed(hashes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vm_commands_change_headless_mirui_pixels() {
        let hashes = run_headless_demo().unwrap();
        assert!(!hashes.is_empty());
        assert!(hashes.windows(2).all(|frames| frames[0] != frames[1]));
    }

    #[test]
    fn cancelled_surface_replies_release_owner_map_once() {
        let mut owner = new_headless_owner().unwrap();
        let caller = another_thread_id();

        let (closed_commands, closed_receiver) = mpsc::channel();
        let (closed_reply, closed_response) =
            oneshot::channel::<Result<SurfaceLease, atman_rt::EvalError>>();
        drop(closed_response);
        owner
            .handle(Command::Load {
                caller,
                name: "runtime-canvas".into(),
                lease_proxy: UiProxy::new(closed_commands),
                reply: closed_reply,
            })
            .unwrap();
        assert!(owner.resources.is_empty());
        assert!(matches!(
            closed_receiver.try_recv(),
            Err(mpsc::TryRecvError::Disconnected)
        ));

        let (unread_commands, unread_receiver) = mpsc::channel();
        let (unread_reply, unread_response) =
            oneshot::channel::<Result<SurfaceLease, atman_rt::EvalError>>();
        owner
            .handle(Command::Load {
                caller,
                name: "runtime-canvas".into(),
                lease_proxy: UiProxy::new(unread_commands),
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
