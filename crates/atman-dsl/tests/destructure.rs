use atman_dsl::ast::{Pattern, Stmt};
use atman_dsl::{parse::parse_file, print::print_file};

fn parse_flow(body: &str) -> atman_dsl::ast::File {
    let src = format!("flow f() {{\n{body}\n}}\n");
    parse_file(&src).unwrap_or_else(|e| panic!("parse: {e}\n---\n{src}"))
}

#[test]
fn destructure_bind_pulls_two_fields_out_of_struct() {
    let file = parse_flow("    { status, body } = { status: 200, body: \"ok\" }");
    let stmt = &file.flows[0].body[0];
    let Stmt::Bind { name, .. } = stmt else {
        panic!("expected bind, got {stmt:?}");
    };
    let Pattern::Struct { fields } = name else {
        panic!("expected struct pattern, got {name:?}");
    };
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].source.name, "status");
    assert!(fields[0].rename.is_none());
    assert_eq!(fields[1].source.name, "body");
    assert!(fields[1].rename.is_none());
}

#[test]
fn destructure_bind_supports_rename() {
    let file = parse_flow("    { error: err } = { error: \"oops\" }");
    let Stmt::Bind {
        name: Pattern::Struct { fields },
        ..
    } = &file.flows[0].body[0]
    else {
        panic!("expected destructure bind");
    };
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].source.name, "error");
    assert_eq!(fields[0].rename.as_ref().unwrap().name, "err");
}

#[test]
fn plain_ident_bind_still_produces_ident_pattern() {
    let file = parse_flow("    x = 1");
    let Stmt::Bind {
        name: Pattern::Ident(id),
        ..
    } = &file.flows[0].body[0]
    else {
        panic!("expected ident pattern");
    };
    assert_eq!(id.name, "x");
}

#[test]
fn destructure_bind_round_trip_preserves_shape() {
    let file1 = parse_flow("    { status, body } = { status: 200, body: \"ok\" }");
    let printed = print_file(&file1);
    let file2 = parse_file(&printed).unwrap_or_else(|e| panic!("reparse: {e}\n---\n{printed}"));
    let a = strip_spans(&format!("{:#?}", file1));
    let b = strip_spans(&format!("{:#?}", file2));
    assert_eq!(a, b, "destructure diverged after roundtrip");
    assert!(
        printed.contains("{ status, body } = "),
        "printed lost destructure shape:\n{printed}"
    );
}

#[test]
fn destructure_bind_rename_round_trip() {
    let file1 = parse_flow("    { error: err } = { error: \"x\" }");
    let printed = print_file(&file1);
    let file2 = parse_file(&printed).unwrap_or_else(|e| panic!("reparse: {e}"));
    let a = strip_spans(&format!("{:#?}", file1));
    let b = strip_spans(&format!("{:#?}", file2));
    assert_eq!(a, b);
    assert!(
        printed.contains("{ error: err }"),
        "rename lost after roundtrip:\n{printed}"
    );
}

fn strip_spans(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '#' && chars.peek() == Some(&'0') {
            for c in chars.by_ref() {
                if c == ')' {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}
