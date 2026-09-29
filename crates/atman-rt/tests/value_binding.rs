use atman_rt::{
    Value,
    binding::{BindingErrorKind, Context, Input, Output},
    catalog::{EnumSpec, StructSpec, TypeSpec},
};

#[atman_rt::value]
#[derive(Debug, PartialEq)]
struct Vec2 {
    x: f32,
    y: f32,
}

#[atman_rt::value]
#[derive(Debug, PartialEq)]
enum BlendMode {
    Normal,
    Multiply,
}

#[atman_rt::value]
#[derive(Debug, PartialEq)]
struct Sprite {
    position: Vec2,
    label: Option<String>,
    tags: Vec<String>,
    mode: BlendMode,
}

type TestValue = Value<(), ()>;

#[test]
fn generated_struct_codec_round_trips_nested_values() {
    let context = Context::<(), ()>::value_only();
    let sprite = Sprite {
        position: Vec2 { x: 1.5, y: -2.0 },
        label: None,
        tags: vec!["hero".into(), "moving".into()],
        mode: BlendMode::Multiply,
    };

    let encoded = sprite.encode_output(&context).unwrap();
    let decoded = Sprite::decode_input(Some(encoded), &context).unwrap();

    assert_eq!(
        decoded,
        Sprite {
            position: Vec2 { x: 1.5, y: -2.0 },
            label: None,
            tags: vec!["hero".into(), "moving".into()],
            mode: BlendMode::Multiply,
        }
    );
}

#[test]
fn generated_struct_codec_ignores_extra_fields_and_accepts_missing_options() {
    let context = Context::<(), ()>::value_only();
    let value = TestValue::Struct(vec![
        (
            "position".into(),
            TestValue::Struct(vec![
                ("x".into(), TestValue::Float(4.0)),
                ("y".into(), TestValue::Float(5.0)),
            ]),
        ),
        ("tags".into(), TestValue::List(vec![])),
        ("mode".into(), TestValue::Str("Normal".into())),
        ("future_field".into(), TestValue::Bool(true)),
    ]);

    let decoded = Sprite::decode_input(Some(value), &context).unwrap();
    assert_eq!(decoded.label, None);
    assert_eq!(decoded.mode, BlendMode::Normal);
}

#[test]
fn generated_struct_codec_reports_nested_and_duplicate_paths() {
    let context = Context::<(), ()>::value_only();
    let nested = TestValue::Struct(vec![
        (
            "position".into(),
            TestValue::Struct(vec![
                ("x".into(), TestValue::Float(1.0)),
                ("y".into(), TestValue::Str("bad".into())),
            ]),
        ),
        ("tags".into(), TestValue::List(vec![])),
        ("mode".into(), TestValue::Str("Normal".into())),
    ]);
    let error = Sprite::decode_input(Some(nested), &context).unwrap_err();
    assert_eq!(error.path().to_string(), "position.y");
    assert!(matches!(
        error.kind(),
        BindingErrorKind::TypeMismatch { .. }
    ));

    let duplicate = TestValue::Struct(vec![
        ("x".into(), TestValue::Float(1.0)),
        ("x".into(), TestValue::Float(2.0)),
        ("y".into(), TestValue::Float(3.0)),
    ]);
    let error = Vec2::decode_input(Some(duplicate), &context).unwrap_err();
    assert_eq!(error.path().to_string(), "x");
    assert!(matches!(error.kind(), BindingErrorKind::DuplicateField));
}

#[test]
fn generated_type_specs_share_the_codec_shape() {
    let TypeSpec::Struct(StructSpec { name, fields }) = <Sprite as Input<(), ()>>::input_type()
    else {
        panic!("sprite must be a struct")
    };
    assert_eq!(name, "Sprite");
    assert_eq!(fields.len(), 4);

    let TypeSpec::Enum(EnumSpec { name, variants }) = <BlendMode as Output<(), ()>>::output_type()
    else {
        panic!("blend mode must be an enum")
    };
    assert_eq!(name, "BlendMode");
    assert_eq!(variants, ["Normal", "Multiply"]);
}
