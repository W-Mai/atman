use std::sync::Arc;

use atman_rt::{
    HostPayload, HostValueOps, Value,
    binding::{BindingErrorKind, Context, Output},
    catalog::TypeSpec,
    resource::{ResourcePayload, ResourceRegistry, ResourceType, WithResources},
};

#[atman_rt::resource]
#[derive(Debug)]
struct Texture(u32);

type Payload = WithResources<()>;

#[test]
fn generated_resource_output_inserts_and_resolves_the_owned_value() {
    let resources = Arc::new(ResourceRegistry::new().unwrap());
    let context = Context::<Payload, ()>::with_resources(Arc::clone(&resources));

    let TypeSpec::Resource(spec) = <Texture as Output<Payload, ()>>::output_type() else {
        panic!("texture must have a resource type")
    };
    assert_eq!(spec.name.as_deref(), Some("Texture"));
    assert_eq!(Texture::TYPE_NAME, "Texture");

    let Value::Host(payload) = Texture(42).encode_output(&context).unwrap() else {
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
    let error = Texture(1)
        .encode_output(&Context::<Payload, ()>::value_only())
        .unwrap_err();
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
