use atman_dsl::ast::{Expr, Node};
use atman_dsl::parse::parse_file;
use atman_dsl::print::print_file;

fn parse_bind_expr(src: &str) -> Expr {
    let file = parse_file(&format!("flow t() -> string {{ x = {src} }}")).expect("parse");
    match &file.flows[0].body[0] {
        atman_dsl::ast::Stmt::Bind { value, .. } => value.clone(),
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
            op: atman_dsl::ast::BinOp::Or,
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
fn lambda_body_with_pipe() {
    let src = r#"flow t() -> string { x = |f| f |> len() return "ok" }"#;
    parse_file(src).expect("should parse");
}

#[test]
fn dynamic_fanout_parses_all() {
    let src = r#"flow t() -> string { r = fanout tasks { |t| t } collect: all return "ok" }"#;
    let file = parse_file(src).expect("parse");
    match &file.flows[0].body[0] {
        atman_dsl::ast::Stmt::Bind { value, .. } => match value {
            Expr::Node(Node::DynamicFanout { collect, .. }) => {
                assert!(matches!(collect, atman_dsl::ast::FanoutCollect::All));
            }
            other => panic!("expected DynamicFanout, got {other:?}"),
        },
        _ => panic!("expected bind"),
    }
}

#[test]
fn dynamic_fanout_parses_first() {
    let src = r#"flow t() -> string { r = fanout tasks { |t| t } collect: first return "ok" }"#;
    let file = parse_file(src).expect("parse");
    match &file.flows[0].body[0] {
        atman_dsl::ast::Stmt::Bind { value, .. } => match value {
            Expr::Node(Node::DynamicFanout { collect, .. }) => {
                assert!(matches!(collect, atman_dsl::ast::FanoutCollect::First));
            }
            other => panic!("expected DynamicFanout, got {other:?}"),
        },
        _ => panic!("expected bind"),
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
    let src = r#"flow t() -> string { r = fanout tasks { |t| t } collect: all return "ok" }"#;
    let f1 = parse_file(src).expect("parse");
    let printed = print_file(&f1);
    let _f2 = parse_file(&printed).expect("re-parse");
}
