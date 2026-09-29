use std::{
    future::Future,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    task::{Context, Poll, Waker},
};

use atman_rt::{
    EvalError, ToolArgs, ToolCallMode, ToolRouter, Value, catalog::TypeSpec,
    resource::WithResources,
};

type Payload = WithResources<()>;

#[atman_rt::value]
struct Point {
    x: i32,
    y: i32,
}

#[atman_rt::resource]
struct Texture(i64);

struct GraphicsHost {
    offset: i64,
    loads: Arc<AtomicUsize>,
}

#[atman_rt::tools(namespace = "gfx")]
impl GraphicsHost {
    /// Adds the host offset to a value.
    #[tool]
    fn add(&self, value: i64) -> i64 {
        self.offset + value
    }

    #[tool(name = "texture.draw")]
    fn draw(&self, texture: &Texture, point: Point) -> Result<i64, EvalError> {
        Ok(texture.0 + i64::from(point.x) + i64::from(point.y))
    }

    #[tool]
    async fn load(&self, id: i64) -> Texture {
        self.loads.fetch_add(1, Ordering::SeqCst);
        Texture(id)
    }

    #[tool]
    fn maybe(&self, present: bool) -> Option<Texture> {
        present.then_some(Texture(self.offset))
    }

    #[tool(name = "texture.release")]
    fn nested_release_name_is_not_reserved(&self) {}
}

fn args(positional: Vec<Value<Payload, EvalError>>) -> ToolArgs<Payload, EvalError> {
    ToolArgs {
        positional,
        named: Vec::new(),
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

#[test]
fn stateful_binding_keeps_specs_handlers_and_resources_in_sync() {
    let loads = Arc::new(AtomicUsize::new(0));
    let mut tools = ToolRouter::<Payload, EvalError>::new();
    tools
        .mount(
            GraphicsHost {
                offset: 4,
                loads: Arc::clone(&loads),
            }
            .into_atman_binding(),
        )
        .expect("mount stateful binding");

    let catalog = tools.catalog();
    let add = catalog.lookup("gfx.add").expect("add spec");
    assert_eq!(add.mode, ToolCallMode::Immediate);
    assert_eq!(
        add.spec.as_ref().expect("generated spec").description,
        "Adds the host offset to a value."
    );
    assert_eq!(
        catalog.lookup("gfx.load").expect("load spec").mode,
        ToolCallMode::Deferred
    );
    let draw = catalog
        .lookup("gfx.texture.draw")
        .and_then(|entry| entry.spec.as_ref())
        .expect("draw spec");
    assert!(matches!(
        &draw.params[0].ty,
        TypeSpec::Resource(resource) if resource.name.as_deref() == Some("Texture")
    ));
    let release = catalog
        .lookup("gfx.release")
        .and_then(|entry| entry.spec.as_ref())
        .expect("release spec");
    assert!(matches!(
        &release.params[0].ty,
        TypeSpec::Resource(resource) if resource.name.is_none()
    ));
    assert!(catalog.lookup("gfx.texture.release").is_some());

    assert!(matches!(
        ready(tools.dispatch("gfx.add", args(vec![Value::Int(3)]))),
        Value::Int(7)
    ));

    let load = tools.dispatch("gfx.load", args(vec![Value::Int(9)]));
    assert_eq!(loads.load(Ordering::SeqCst), 0);
    let texture = ready(load);
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    assert!(matches!(texture, Value::Host(_)));

    let point = Value::Struct(vec![
        ("x".into(), Value::Int(2)),
        ("y".into(), Value::Int(3)),
    ]);
    assert!(matches!(
        ready(tools.dispatch("gfx.texture.draw", args(vec![texture.clone(), point]),)),
        Value::Int(14)
    ));
    assert!(matches!(
        ready(tools.dispatch("gfx.release", args(vec![texture.clone()]))),
        Value::Unit
    ));
    assert!(matches!(
        ready(tools.dispatch(
            "gfx.texture.draw",
            args(vec![
                texture,
                Value::Struct(vec![
                    ("x".into(), Value::Int(0)),
                    ("y".into(), Value::Int(0)),
                ]),
            ]),
        )),
        Value::Err(_)
    ));

    assert!(matches!(
        ready(tools.dispatch("gfx.maybe", args(vec![Value::Bool(true)]))),
        Value::Host(_)
    ));
}
