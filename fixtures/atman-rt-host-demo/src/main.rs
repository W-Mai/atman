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

type Payload = WithResources<()>;

struct NoSources;

impl SourceResolver for NoSources {
    type Error = &'static str;

    fn resolve(&self, _importer_id: &str, _specifier: &str) -> Result<Source, Self::Error> {
        Err("the fixture has no imports")
    }
}

struct NoopWake;

impl Wake for NoopWake {
    fn wake(self: Arc<Self>) {}
}

fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(NoopWake));
    let mut context = Context::from_waker(&waker);
    let mut future = Box::pin(future);
    loop {
        match Pin::as_mut(&mut future).poll(&mut context) {
            Poll::Ready(value) => return value,
            Poll::Pending => thread::yield_now(),
        }
    }
}

enum Command {
    Load {
        caller: ThreadId,
        name: String,
        reply: Sender<Result<u64, EvalError>>,
    },
    Frame {
        caller: ThreadId,
        reply: Sender<i64>,
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
}

impl Drop for TextureLease {
    fn drop(&mut self) {
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
        let (reply, response) = mpsc::channel();
        self.proxy.send(Command::Load {
            caller: thread::current().id(),
            name,
            reply,
        })?;
        let id = response.recv().map_err(|_| broken_bridge())??;
        Ok(TextureLease {
            id,
            proxy: self.proxy.clone(),
        })
    }

    /// Advances the owner-thread frame clock.
    #[tool]
    async fn frame(&self) -> Result<i64, EvalError> {
        let (reply, response) = mpsc::channel();
        self.proxy.send(Command::Frame {
            caller: thread::current().id(),
            reply,
        })?;
        response.recv().map_err(|_| broken_bridge())
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
}

struct FakeWorld {
    owner: ThreadId,
    next_texture: u64,
    frame: i64,
    textures: BTreeMap<u64, FakeTexture>,
    audit: Vec<AuditEvent>,
}

impl FakeWorld {
    fn new() -> Self {
        Self {
            owner: thread::current().id(),
            next_texture: 1,
            frame: 0,
            textures: BTreeMap::new(),
            audit: Vec::new(),
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
            | Command::Release { caller, .. } => *caller,
        };
        if caller == self.owner {
            return Err("VM command unexpectedly originated on the owner thread".into());
        }

        match command {
            Command::Load { name, reply, .. } => {
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
                reply
                    .send(Ok(id))
                    .map_err(|_| "load reply receiver closed".to_string())?;
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
                let released = self
                    .textures
                    .remove(&texture)
                    .ok_or_else(|| format!("texture {texture} released twice"))?;
                if released.name == "checkerboard" && released.pixels[..3] != [1, 2, 3] {
                    return Err("checkerboard was released before all frames were drawn".into());
                }
                self.audit.push(AuditEvent::Release(texture));
            }
        }
        Ok(())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct WorkerReport {
    cold_result: i64,
    rendered_frames: i64,
    stale_error: String,
}

fn vm_worker(commands: Sender<Command>) -> Result<WorkerReport, String> {
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
    let stale_error = match block_on(vm.run("stale_handle", vec![], VmDelegates::new(tools))) {
        StatementOutcome::Err(error) => format!("{error:?}"),
        StatementOutcome::Return(_) => return Err("stale_handle unexpectedly returned".into()),
        _ => return Err("stale_handle did not fail".into()),
    };
    if !stale_error.contains("stale") {
        return Err(format!(
            "stale handle diagnostic was not preserved: {stale_error}"
        ));
    }

    Ok(WorkerReport {
        cold_result,
        rendered_frames,
        stale_error,
    })
}

fn run_demo() -> Result<(WorkerReport, Vec<AuditEvent>), String> {
    let (commands, receiver): (Sender<Command>, Receiver<Command>) = mpsc::channel();
    let worker = thread::spawn(move || vm_worker(commands));
    let mut world = FakeWorld::new();
    while let Ok(command) = receiver.recv() {
        world.handle(command)?;
    }
    let report = worker
        .join()
        .map_err(|_| "VM worker panicked".to_string())??;

    if report.cold_result != 1 || report.rendered_frames != 3 {
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
    ];
    if world.audit != expected {
        return Err(format!("unexpected owner audit: {:?}", world.audit));
    }
    Ok((report, world.audit))
}

fn main() {
    match run_demo() {
        Ok((report, audit)) => println!(
            "owner-thread bridge ok: {} frames, {} commands, cold async stayed idle, {}",
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
    fn exercises_owner_thread_resources_and_cold_tools() {
        let (report, audit) = run_demo().unwrap();
        assert_eq!(report.cold_result, 1);
        assert_eq!(report.rendered_frames, 3);
        assert!(report.stale_error.contains("stale"));
        assert_eq!(audit.len(), 10);
    }
}
