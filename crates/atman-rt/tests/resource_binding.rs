use std::{
    future::Future,
    panic::{AssertUnwindSafe, catch_unwind},
    pin::pin,
    sync::{
        Arc, Barrier, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context as TaskContext, Poll, Waker},
};

use atman_rt::{
    EvalError, HostPayload, HostValueOps, ToolArgs, ToolRouter, Value,
    binding::{BindingError, BindingErrorKind, Context, Output},
    catalog::TypeSpec,
    resource::{
        ErasedHandle, ResourceError, ResourcePayload, ResourceRegistry, ResourceType, WithResources,
    },
};

#[atman_rt::resource]
#[derive(Debug)]
struct Texture(u32);

#[atman_rt::resource]
struct DropCounter {
    id: u32,
    drops: Arc<AtomicUsize>,
}

impl Drop for DropCounter {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::SeqCst);
    }
}

enum FallibleResource {
    Resource(DropCounter),
    Fail,
    Probe {
        resource: DropCounter,
        registry: Arc<ResourceRegistry>,
        published: Arc<Mutex<Option<ErasedHandle>>>,
        panic: bool,
    },
}

impl<P, E> Output<P, E> for FallibleResource
where
    P: ResourcePayload,
{
    fn output_type() -> TypeSpec {
        <DropCounter as Output<P, E>>::output_type()
    }

    fn encode_output(self, context: &mut Context<P, E>) -> Result<Value<P, E>, BindingError> {
        match self {
            Self::Resource(resource) => resource.encode_output(context),
            Self::Fail => Err(BindingError::missing_value()),
            Self::Probe {
                resource,
                registry,
                published,
                panic,
            } => {
                let encoded = resource.encode_output(context)?;
                let Value::Host(payload) = &encoded else {
                    unreachable!("resource output must use the host payload")
                };
                let handle = *payload.as_resource().expect("resource handle");
                *published.lock().unwrap() = Some(handle);
                assert!(matches!(
                    registry.try_borrow(handle.typed::<DropCounter>().unwrap()),
                    Err(ResourceError::StaleResource)
                ));
                assert!(matches!(
                    registry.release(handle),
                    Err(ResourceError::StaleResource)
                ));
                if panic {
                    panic!("probe output panic");
                }
                Err(BindingError::missing_value())
            }
        }
    }
}

enum PanickingPayload {}

impl HostPayload for PanickingPayload {
    fn kind_name(&self) -> &'static str {
        match *self {}
    }
}

impl HostValueOps for PanickingPayload {}

impl ResourcePayload for PanickingPayload {
    fn from_resource(_handle: ErasedHandle) -> Self {
        panic!("payload construction panic")
    }

    fn as_resource(&self) -> Option<&ErasedHandle> {
        match *self {}
    }
}

#[atman_rt::value]
struct NestedOutput {
    first: DropCounter,
    nested: Vec<Vec<FallibleResource>>,
}

type Payload = WithResources<()>;

fn drop_counter(id: u32, drops: &Arc<AtomicUsize>) -> DropCounter {
    DropCounter {
        id,
        drops: Arc::clone(drops),
    }
}

fn resource_handle(value: &Value<Payload, EvalError>) -> atman_rt::resource::ErasedHandle {
    let Value::Host(payload) = value else {
        panic!("resource output must use the host payload")
    };
    *payload.as_resource().expect("resource handle")
}

fn ready<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    match future
        .as_mut()
        .poll(&mut TaskContext::from_waker(Waker::noop()))
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

#[test]
fn generated_resource_output_inserts_and_resolves_the_owned_value() {
    let resources = Arc::new(ResourceRegistry::new().unwrap());
    let mut context = Context::<Payload, ()>::with_resources(Arc::clone(&resources));

    let TypeSpec::Resource(spec) = <Texture as Output<Payload, ()>>::output_type() else {
        panic!("texture must have a resource type")
    };
    assert_eq!(spec.name.as_deref(), Some("Texture"));
    assert_eq!(Texture::TYPE_NAME, "Texture");

    let Value::Host(payload) = Texture(42).encode_output(&mut context).unwrap() else {
        panic!("resource output must use the host payload")
    };
    assert_eq!(payload.kind_name(), "resource");
    let handle = *payload.as_resource().unwrap();
    let typed = handle.typed::<Texture>().unwrap();
    assert_eq!(resources.try_borrow(typed).unwrap().0, 42);

    resources.release(handle).unwrap();
    assert!(resources.try_borrow(typed).is_err());
}

#[test]
fn resource_output_requires_a_resource_backed_context() {
    let mut context = Context::<Payload, ()>::value_only();
    let error = Texture(1).encode_output(&mut context).unwrap_err();
    assert!(matches!(
        error.kind(),
        BindingErrorKind::ResourceContextUnavailable
    ));
}

#[test]
fn resource_payload_equality_uses_complete_handle_identity() {
    let resources = ResourceRegistry::new().unwrap();
    let first = resources.insert(Texture(1)).unwrap();
    let same = Payload::from_resource(first.erased());
    let same_copy = Payload::from_resource(first.erased());
    assert!(same.equals(&same_copy));

    resources.release(first.erased()).unwrap();
    let reused = resources.insert(Texture(2)).unwrap();
    let next_generation = Payload::from_resource(reused.erased());
    assert!(!same.equals(&next_generation));
    assert!(!same.equals(&Payload::Custom(())));
}

#[test]
fn successful_resource_list_keeps_every_handle_live() {
    let drops = Arc::new(AtomicUsize::new(0));
    let resources = Arc::new(ResourceRegistry::new().unwrap());
    let mut context = Context::<Payload, EvalError>::with_resources(Arc::clone(&resources));

    let Value::List(values) = vec![drop_counter(1, &drops), drop_counter(2, &drops)]
        .encode_output(&mut context)
        .unwrap()
    else {
        panic!("resource list must encode as a list")
    };
    assert_eq!(drops.load(Ordering::SeqCst), 0);

    let handles = values.iter().map(resource_handle).collect::<Vec<_>>();
    assert_eq!(
        resources
            .try_borrow(handles[0].typed::<DropCounter>().unwrap())
            .unwrap()
            .id,
        1
    );
    assert_eq!(
        resources
            .try_borrow(handles[1].typed::<DropCounter>().unwrap())
            .unwrap()
            .id,
        2
    );

    for handle in handles {
        resources.release(handle).unwrap();
    }
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[test]
fn failed_resource_list_rolls_back_every_inserted_resource() {
    let drops = Arc::new(AtomicUsize::new(0));
    let resources = Arc::new(ResourceRegistry::new().unwrap());
    let mut context = Context::<Payload, EvalError>::with_resources(resources);

    let error = vec![
        FallibleResource::Resource(drop_counter(1, &drops)),
        FallibleResource::Resource(drop_counter(2, &drops)),
        FallibleResource::Fail,
    ]
    .encode_output(&mut context)
    .unwrap_err();

    assert_eq!(error.path().to_string(), "[2]");
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[test]
fn pending_handle_cannot_be_borrowed_or_released_before_error_rollback() {
    let drops = Arc::new(AtomicUsize::new(0));
    let resources = Arc::new(ResourceRegistry::new().unwrap());
    let published = Arc::new(Mutex::new(None));
    let mut context = Context::<Payload, EvalError>::with_resources(Arc::clone(&resources));

    let error = vec![FallibleResource::Probe {
        resource: drop_counter(1, &drops),
        registry: Arc::clone(&resources),
        published: Arc::clone(&published),
        panic: false,
    }]
    .encode_output(&mut context)
    .unwrap_err();

    assert_eq!(error.path().to_string(), "[0]");
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let handle = published.lock().unwrap().expect("published pending handle");
    assert!(matches!(
        resources.try_borrow(handle.typed::<DropCounter>().unwrap()),
        Err(ResourceError::StaleResource)
    ));

    let reused = resources.insert(drop_counter(2, &drops)).unwrap();
    assert_eq!(reused.erased().slot(), handle.slot());
    assert_eq!(reused.erased().generation(), handle.generation() + 1);
    resources.release(reused.erased()).unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[test]
fn panic_rolls_back_pending_resource_and_leaves_context_reusable() {
    let drops = Arc::new(AtomicUsize::new(0));
    let resources = Arc::new(ResourceRegistry::new().unwrap());
    let published = Arc::new(Mutex::new(None));
    let mut context = Context::<Payload, EvalError>::with_resources(Arc::clone(&resources));

    let unwind = catch_unwind(AssertUnwindSafe(|| {
        let _ = vec![FallibleResource::Probe {
            resource: drop_counter(1, &drops),
            registry: Arc::clone(&resources),
            published: Arc::clone(&published),
            panic: true,
        }]
        .encode_output(&mut context);
    }));

    assert!(unwind.is_err());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let stale = published.lock().unwrap().expect("published pending handle");
    assert!(matches!(
        resources.try_borrow(stale.typed::<DropCounter>().unwrap()),
        Err(ResourceError::StaleResource)
    ));

    let Value::Host(payload) = drop_counter(2, &drops).encode_output(&mut context).unwrap() else {
        panic!("resource output must use the host payload")
    };
    let live = *payload.as_resource().unwrap();
    assert_eq!(
        resources
            .try_borrow(live.typed::<DropCounter>().unwrap())
            .unwrap()
            .id,
        2
    );
    resources.release(live).unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[test]
fn payload_construction_panic_rolls_back_registered_pending_resource() {
    let drops = Arc::new(AtomicUsize::new(0));
    let resources = Arc::new(ResourceRegistry::new().unwrap());
    let mut context = Context::<PanickingPayload, ()>::with_resources(Arc::clone(&resources));

    let unwind = catch_unwind(AssertUnwindSafe(|| {
        let _ = drop_counter(1, &drops).encode_output(&mut context);
    }));

    assert!(unwind.is_err());
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    let live = resources.insert(drop_counter(2, &drops)).unwrap();
    resources.release(live.erased()).unwrap();
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}

#[test]
fn nested_output_failure_rolls_back_outer_and_inner_resources() {
    let drops = Arc::new(AtomicUsize::new(0));
    let resources = Arc::new(ResourceRegistry::new().unwrap());
    let mut context = Context::<Payload, EvalError>::with_resources(resources);

    let error = NestedOutput {
        first: drop_counter(1, &drops),
        nested: vec![
            vec![FallibleResource::Resource(drop_counter(2, &drops))],
            vec![
                FallibleResource::Resource(drop_counter(3, &drops)),
                FallibleResource::Fail,
            ],
        ],
    }
    .encode_output(&mut context)
    .unwrap_err();

    assert_eq!(error.path().to_string(), "nested[1][1]");
    assert_eq!(drops.load(Ordering::SeqCst), 3);
}

struct ConcurrentOutput {
    fail: bool,
    coordination: Arc<Coordination>,
    drops: Arc<AtomicUsize>,
}

struct Coordination {
    first_inserted: Mutex<bool>,
    first_inserted_changed: Condvar,
    both_inserted: Barrier,
}

impl Coordination {
    fn new() -> Self {
        Self {
            first_inserted: Mutex::new(false),
            first_inserted_changed: Condvar::new(),
            both_inserted: Barrier::new(2),
        }
    }
}

impl<P, E> Output<P, E> for ConcurrentOutput
where
    P: ResourcePayload,
{
    fn output_type() -> TypeSpec {
        <DropCounter as Output<P, E>>::output_type()
    }

    fn encode_output(self, context: &mut Context<P, E>) -> Result<Value<P, E>, BindingError> {
        if !self.fail {
            let mut first_inserted = self.coordination.first_inserted.lock().unwrap();
            while !*first_inserted {
                first_inserted = self
                    .coordination
                    .first_inserted_changed
                    .wait(first_inserted)
                    .unwrap();
            }
        }

        let encoded = drop_counter(u32::from(self.fail), &self.drops).encode_output(context)?;
        if self.fail {
            *self.coordination.first_inserted.lock().unwrap() = true;
            self.coordination.first_inserted_changed.notify_one();
        }
        self.coordination.both_inserted.wait();

        if self.fail {
            Err(BindingError::missing_value())
        } else {
            Ok(encoded)
        }
    }
}

struct ConcurrentHost {
    coordination: Arc<Coordination>,
    drops: Arc<AtomicUsize>,
}

#[atman_rt::tools(namespace = "rollback")]
impl ConcurrentHost {
    #[tool]
    fn produce(&self, fail: bool) -> ConcurrentOutput {
        ConcurrentOutput {
            fail,
            coordination: Arc::clone(&self.coordination),
            drops: Arc::clone(&self.drops),
        }
    }
}

#[test]
fn concurrent_handlers_isolate_output_transactions() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRouter::<Payload, EvalError>::new();
    tools
        .mount(
            ConcurrentHost {
                coordination: Arc::new(Coordination::new()),
                drops: Arc::clone(&drops),
            }
            .into_atman_binding(),
        )
        .unwrap();

    let failing_tools = tools.clone();
    let failing = std::thread::spawn(move || {
        ready(failing_tools.dispatch("rollback.produce", args(vec![Value::Bool(true)])))
    });
    let successful_tools = tools.clone();
    let successful = std::thread::spawn(move || {
        ready(successful_tools.dispatch("rollback.produce", args(vec![Value::Bool(false)])))
    });

    assert!(matches!(failing.join().unwrap(), Value::Err(_)));
    let live = successful.join().unwrap();
    assert!(matches!(live, Value::Host(_)));
    assert_eq!(drops.load(Ordering::SeqCst), 1);

    assert!(matches!(
        ready(tools.dispatch("rollback.release", args(vec![live]))),
        Value::Unit
    ));
    assert_eq!(drops.load(Ordering::SeqCst), 2);
}
