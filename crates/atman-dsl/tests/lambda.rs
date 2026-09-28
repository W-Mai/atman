use atman_dsl::parse::parse_file;
use atman_dsl::print::print_file;
use atman_rt::ast::{Expr, Node};

fn parse_bind_expr(src: &str) -> Expr {
    let file = parse_file(&format!("flow t() -> string {{ x = {src} }}")).expect("parse");
    match &file.flows[0].body[0] {
        atman_rt::ast::Stmt::Bind { value, .. } => value.clone(),
        _ => panic!("expected bind stmt"),
    }
}

// === Parser tests ===

#[test]
fn parse_single_param_lambda() {
    let expr = parse_bind_expr(r#"|a| a + 1"#);
    match expr {
        Expr::Lambda { params, body } => {
            assert_eq!(params.len(), 1);
            assert_eq!(params[0].name, "a");
            assert!(matches!(*body, Expr::Binary { .. }));
        }
        other => panic!("expected Lambda, got {other:?}"),
    }
}

#[test]
fn parse_multi_param_lambda() {
    let expr = parse_bind_expr(r#"|x, y| x + y"#);
    match expr {
        Expr::Lambda { params, .. } => {
            assert_eq!(params.len(), 2);
            assert_eq!(params[0].name, "x");
            assert_eq!(params[1].name, "y");
        }
        other => panic!("expected Lambda, got {other:?}"),
    }
}

#[test]
fn parse_three_param_lambda() {
    let expr = parse_bind_expr(r#"|x, y, z| x"#);
    match expr {
        Expr::Lambda { params, .. } => assert_eq!(params.len(), 3),
        other => panic!("expected Lambda, got {other:?}"),
    }
}

#[test]
fn logical_or_not_lambda() {
    let expr = parse_bind_expr("a || b");
    match expr {
        Expr::Binary {
            op: atman_rt::ast::BinOp::Or,
            ..
        } => {}
        other => panic!("expected Binary Or, got {other:?}"),
    }
}

#[test]
fn lambda_as_tool_arg() {
    let src = r#"flow t() -> string { x = list.map(items, |n| n + 1) return "ok" }"#;
    parse_file(src).expect("should parse");
}

#[test]
fn lambda_in_struct() {
    let expr = parse_bind_expr(r#"{ func: |x| x + 1 }"#);
    match expr {
        Expr::Struct(fields) => {
            assert_eq!(fields.len(), 1);
            assert!(matches!(&fields[0].1, Expr::Lambda { .. }));
        }
        other => panic!("expected Struct, got {other:?}"),
    }
}

#[test]
fn lambda_in_list() {
    let expr = parse_bind_expr(r#"[|x| x + 1, |y| y * 2]"#);
    match expr {
        Expr::List(items) => {
            assert_eq!(items.len(), 2);
            assert!(matches!(items[0], Expr::Lambda { .. }));
            assert!(matches!(items[1], Expr::Lambda { .. }));
        }
        other => panic!("expected List, got {other:?}"),
    }
}

#[test]
fn lambda_bound_to_var() {
    let expr = parse_bind_expr(r#"|x| x + 1"#);
    assert!(matches!(expr, Expr::Lambda { .. }));
}

#[test]
fn lambda_body_with_tool_call() {
    let src = r#"flow t() -> string { x = |f| len(f) return "ok" }"#;
    parse_file(src).expect("should parse");
}

#[test]
fn dynamic_fanout_parses() {
    let src = r#"flow t() -> string { r = fanout tasks { |t| t } return "ok" }"#;
    let file = parse_file(src).expect("parse");
    match &file.flows[0].body[0] {
        atman_rt::ast::Stmt::Bind { value, .. } => match value {
            Expr::Node(Node::DynamicFanout { .. }) => {}
            other => panic!("expected DynamicFanout, got {other:?}"),
        },
        _ => panic!("expected bind"),
    }
}

#[test]
fn static_fanout_parses() {
    let src = r#"flow t() -> string { r = fanout [a, b] return "ok" }"#;
    let file = parse_file(src).expect("parse");
    match &file.flows[0].body[0] {
        atman_rt::ast::Stmt::Bind { value, .. } => match value {
            Expr::Node(Node::Fanout { source }) => {
                assert!(matches!(source.as_ref(), Expr::List(items) if items.len() == 2));
            }
            other => panic!("expected Fanout, got {other:?}"),
        },
        _ => panic!("expected bind"),
    }
}

#[test]
fn fanout_accepts_array_variable() {
    let expr = parse_bind_expr("fanout pending");
    assert!(matches!(
        expr,
        Expr::Node(Node::Fanout { source }) if matches!(source.as_ref(), Expr::Ident(id) if id.name == "pending")
    ));
}

#[test]
fn fanout_keeps_outer_comparison() {
    let compared = parse_bind_expr("fanout [a, b] == expected");
    assert!(matches!(
        compared,
        Expr::Binary { left, .. } if matches!(left.as_ref(), Expr::Node(Node::Fanout { .. }))
    ));
}

#[test]
fn fanout_condition_keeps_when_body() {
    let source = "flow t() -> bool { when fanout [1] == [1] { return true } return false }";
    let file = parse_file(source).expect("when body must not become a fanout mapping");
    assert!(matches!(
        &file.flows[0].body[0],
        atman_rt::ast::Stmt::When {
            cond: Expr::Binary { left, .. },
            body,
        } if matches!(left.as_ref(), Expr::Node(Node::Fanout { .. })) && body.len() == 1
    ));
}

#[test]
fn dynamic_fanout_accepts_expression_source() {
    let expr = parse_bind_expr("fanout left + right { |item| item }");
    assert!(matches!(
        expr,
        Expr::Node(Node::DynamicFanout { source, .. })
            if matches!(source.as_ref(), Expr::Binary { .. })
    ));
}

#[test]
fn fanout_rejects_collect_clause() {
    for fanout in ["fanout [a, b]", "fanout tasks { |t| t }"] {
        for mode in ["all", "first"] {
            let src =
                format!("flow t() -> string {{ r = {fanout} collect: {mode} return \"ok\" }}");
            assert!(parse_file(&src).is_err(), "accepted removed syntax: {src}");
        }
    }
}

// === Roundtrip tests ===

#[test]
fn roundtrip_simple_lambda() {
    let src = r#"flow t() -> string { x = |a| a + 1 return "ok" }"#;
    let f1 = parse_file(src).expect("parse");
    let printed = print_file(&f1);
    let f2 = parse_file(&printed).expect("re-parse");
    assert_eq!(f1.flows[0].name.name, f2.flows[0].name.name);
}

#[test]
fn roundtrip_multi_param_lambda() {
    let src = r#"flow t() -> string { x = |x, y| x + y return "ok" }"#;
    let f1 = parse_file(src).expect("parse");
    let printed = print_file(&f1);
    let _f2 = parse_file(&printed).expect("re-parse");
}

#[test]
fn roundtrip_combinator() {
    let src = r#"flow t() -> string { x = list.map(items, |n| n * 2) return "ok" }"#;
    let f1 = parse_file(src).expect("parse");
    let printed = print_file(&f1);
    let _f2 = parse_file(&printed).expect("re-parse");
}

#[test]
fn roundtrip_dynamic_fanout() {
    let src = r#"flow t() -> string { r = fanout tasks { |t| t } return "ok" }"#;
    let f1 = parse_file(src).expect("parse");
    let printed = print_file(&f1);
    assert!(!printed.contains("collect:"));
    let _f2 = parse_file(&printed).expect("re-parse");
}

#[test]
fn roundtrip_static_fanout() {
    let src = r#"flow t() -> string { r = fanout [a, b] return "ok" }"#;
    let f1 = parse_file(src).expect("parse");
    let printed = print_file(&f1);
    assert!(!printed.contains("collect:"));
    let _f2 = parse_file(&printed).expect("re-parse");
}

#[test]
fn roundtrip_array_variable_fanout() {
    let src = r#"flow t() -> string { pending = [a, b] return to_json_string(fanout pending) }"#;
    let file = parse_file(src).expect("parse");
    let printed = print_file(&file);
    assert!(printed.contains("fanout pending"), "{printed}");
    parse_file(&printed).expect("re-parse");
}
