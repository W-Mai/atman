use std::{
    future::Future,
    pin::pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

use atman_rt::{
    EvalError, ToolArgs, ToolRegisterError, ToolRouter, Value,
    binding::Factory,
    resource::{ResourceRegistry, WithResources},
};

type Payload = WithResources<()>;

#[atman_rt::resource]
struct SharedResource {
    value: i64,
    drops: Arc<AtomicUsize>,
}

impl Drop for SharedResource {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

struct Producer {
    drops: Arc<AtomicUsize>,
}

#[atman_rt::tools(namespace = "producer")]
impl Producer {
    #[tool]
    fn make(&self, value: i64) -> SharedResource {
        SharedResource {
            value,
            drops: Arc::clone(&self.drops),
        }
    }
}

struct Consumer;

#[atman_rt::tools(namespace = "consumer")]
impl Consumer {
    #[tool]
    fn inspect(&self, resource: &SharedResource) -> i64 {
        resource.value
    }
}

fn ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut Context::from_waker(Waker::noop()))
    {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("test tool should complete synchronously"),
    }
}

fn args(positional: Vec<Value<Payload, EvalError>>) -> ToolArgs<Payload, EvalError> {
    ToolArgs {
        positional,
        named: Vec::new(),
    }
}

fn assert_make_inspect_release(
    producer: &ToolRouter<Payload, EvalError>,
    consumer: &ToolRouter<Payload, EvalError>,
    drops: &AtomicUsize,
) {
    let resource = ready(producer.dispatch("producer.make", args(vec![Value::Int(42)])));
    let stale = resource.clone();

    assert!(matches!(
        ready(consumer.dispatch("consumer.inspect", args(vec![resource.clone()]))),
        Value::Int(42)
    ));
    assert!(matches!(
        ready(consumer.dispatch("consumer.release", args(vec![resource]))),
        Value::Unit
    ));
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert!(matches!(
        ready(consumer.dispatch("consumer.inspect", args(vec![stale]))),
        Value::Err(_)
    ));
}

#[test]
fn mounted_bindings_share_resources_for_borrow_and_release() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRouter::<Payload, EvalError>::new();
    tools
        .mount(
            Producer {
                drops: Arc::clone(&drops),
            }
            .into_atman_binding(),
        )
        .unwrap();
    tools.mount(Consumer.into_atman_binding()).unwrap();

    assert_make_inspect_release(&tools, &tools, &drops);
}

#[test]
fn router_clones_keep_one_resource_lineage_across_later_mounts() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut producer = ToolRouter::<Payload, EvalError>::new();
    let mut consumer = producer.clone();
    producer
        .mount(
            Producer {
                drops: Arc::clone(&drops),
            }
            .into_atman_binding(),
        )
        .unwrap();
    consumer.mount(Consumer.into_atman_binding()).unwrap();

    assert_make_inspect_release(&producer, &consumer, &drops);
}

struct CaptureFactory {
    names: &'static [&'static str],
    registries: Arc<Mutex<Vec<Arc<ResourceRegistry>>>>,
}

struct ReentrantFactory {
    sibling: ToolRouter<Payload, EvalError>,
}

impl Factory<Payload, EvalError> for ReentrantFactory {
    fn build(
        self,
        _resources: Arc<ResourceRegistry>,
    ) -> Result<ToolRouter<Payload, EvalError>, ToolRegisterError> {
        let mut sibling = self.sibling;
        sibling.mount(CaptureFactory {
            names: &["nested"],
            registries: Arc::new(Mutex::new(Vec::new())),
        })?;
        Ok(ToolRouter::new())
    }
}

impl Factory<Payload, EvalError> for CaptureFactory {
    fn build(
        self,
        resources: Arc<ResourceRegistry>,
    ) -> Result<ToolRouter<Payload, EvalError>, ToolRegisterError> {
        self.registries.lock().unwrap().push(resources);
        let mut router = ToolRouter::new();
        for name in self.names {
            router.register_sync(*name, |_| Ok(Value::Unit))?;
        }
        Ok(router)
    }
}

#[test]
fn failed_initial_mount_does_not_commit_its_resource_registry() {
    let registries = Arc::new(Mutex::new(Vec::new()));
    let mut tools = ToolRouter::<Payload, EvalError>::new();
    tools.register_sync("taken", |_| Ok(Value::Unit)).unwrap();

    assert!(matches!(
        tools.mount(CaptureFactory {
            names: &["candidate", "taken"],
            registries: Arc::clone(&registries),
        }),
        Err(ToolRegisterError::DuplicateName(name)) if name == "taken"
    ));
    assert!(!tools.contains("candidate"));

    tools
        .mount(CaptureFactory {
            names: &["mounted"],
            registries: Arc::clone(&registries),
        })
        .unwrap();

    let registries = registries.lock().unwrap();
    assert_eq!(registries.len(), 2);
    assert!(!Arc::ptr_eq(&registries[0], &registries[1]));
}

#[test]
fn reentrant_initial_mount_returns_without_waiting_for_its_own_lineage_lock() {
    let mut tools = ToolRouter::<Payload, EvalError>::new();
    let sibling = tools.clone();

    assert!(matches!(
        tools.mount(ReentrantFactory { sibling }),
        Err(ToolRegisterError::MountInProgress)
    ));

    tools
        .mount(CaptureFactory {
            names: &["mounted"],
            registries: Arc::new(Mutex::new(Vec::new())),
        })
        .unwrap();
    assert!(tools.contains("mounted"));
}
