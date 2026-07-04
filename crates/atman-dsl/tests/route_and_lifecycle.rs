use atman_dsl::ast::LifecycleEvent;
use atman_dsl::parse::parse_file;
use atman_dsl::print::print_file;

#[test]
fn parses_route_declaration() {
    let src = r#"route "review" { flow: review_code }

flow review_code(path: string) -> string {
    return path
}
"#;
    let file = parse_file(src).unwrap();
    assert_eq!(file.routes.len(), 1);
    assert_eq!(file.routes[0].pattern, "review");
    assert_eq!(file.routes[0].flow.name, "review_code");
}

#[test]
fn parses_default_route() {
    let src = r#"default_route { flow: chat }

flow chat() -> string {
    return "hi"
}
"#;
    let file = parse_file(src).unwrap();
    let dr = file.default_route.expect("default_route present");
    assert_eq!(dr.flow.name, "chat");
}

#[test]
fn parses_on_session_start() {
    let src = r#"on session.start {
    x = 1
}

flow t() -> Int {
    return 1
}
"#;
    let file = parse_file(src).unwrap();
    assert_eq!(file.lifecycles.len(), 1);
    assert!(matches!(
        file.lifecycles[0].event,
        LifecycleEvent::SessionStart
    ));
}

#[test]
fn parses_multiple_routes_and_lifecycles() {
    let src = r#"route "review" { flow: review_code }
route "edit" { flow: edit_and_verify }
default_route { flow: chat }

on session.start {
    x = 1
}
on turn.end {
    y = 2
}

flow review_code() -> string { return "" }
flow edit_and_verify() -> string { return "" }
flow chat() -> string { return "" }
"#;
    let file = parse_file(src).unwrap();
    assert_eq!(file.routes.len(), 2);
    assert_eq!(file.lifecycles.len(), 2);
    assert!(file.default_route.is_some());
    assert_eq!(file.flows.len(), 3);
}

#[test]
fn round_trip_route_default_route_and_lifecycle() {
    let src = r#"route "review" { flow: review_code }
default_route { flow: chat }
on session.start {
    x = 1
}

flow review_code() -> string {
    return ""
}

flow chat() -> string {
    return ""
}
"#;
    let file = parse_file(src).unwrap();
    let printed = print_file(&file);
    let reparsed = parse_file(&printed).expect("reparse");
    assert_eq!(reparsed.routes.len(), 1);
    assert_eq!(reparsed.routes[0].pattern, "review");
    assert!(reparsed.default_route.is_some());
    assert_eq!(reparsed.lifecycles.len(), 1);
    assert_eq!(reparsed.flows.len(), 2);
}

#[test]
fn parses_all_four_lifecycle_variants() {
    let src = r#"on session.start { x = 1 }
on session.end { x = 2 }
on turn.start { x = 3 }
on turn.end { x = 4 }

flow t() -> Int { return 0 }
"#;
    let file = parse_file(src).unwrap();
    assert_eq!(file.lifecycles.len(), 4);
    assert!(matches!(
        file.lifecycles[0].event,
        LifecycleEvent::SessionStart
    ));
    assert!(matches!(
        file.lifecycles[1].event,
        LifecycleEvent::SessionEnd
    ));
    assert!(matches!(
        file.lifecycles[2].event,
        LifecycleEvent::TurnStart
    ));
    assert!(matches!(file.lifecycles[3].event, LifecycleEvent::TurnEnd));
}

#[test]
fn unknown_lifecycle_event_rejected_with_helpful_message() {
    let src = r#"on flow.start { x = 1 }
flow t() -> Int { return 0 }
"#;
    let err = parse_file(src).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("unknown lifecycle") || msg.contains("flow.start"),
        "expected diagnostic mentioning lifecycle/flow.start, got: {msg}"
    );
}

#[test]
fn duplicate_default_route_rejected() {
    let src = r#"default_route { flow: a }
default_route { flow: b }

flow a() -> Int { return 1 }
flow b() -> Int { return 2 }
"#;
    let err = parse_file(src).unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("duplicate"),
        "expected duplicate error, got {msg}"
    );
}
