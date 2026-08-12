use atman_dsl::ast::Expr;
use atman_dsl::parse::parse_file;
use atman_dsl::print::print_file;

fn parse_expr_str(src: &str) -> Expr {
    let file = parse_file(&format!("flow t() -> string {{ x = {src} }}")).expect("parse");
    let stmts = &file.flows[0].body;
    match &stmts[0] {
        atman_dsl::ast::Stmt::Bind { value, .. } => value.clone(),
        _ => panic!("expected bind stmt"),
    }
}

#[test]
fn parse_bool_annotation() {
    let expr = parse_expr_str(r#"bool -- "is valid""#);
    match expr {
        Expr::Annotated { expr, annotation } => {
            assert_eq!(annotation, "is valid");
            match *expr {
                Expr::Ident(id) => assert_eq!(id.name, "bool"),
                other => panic!("expected Ident, got {other:?}"),
            }
        }
        other => panic!("expected Annotated, got {other:?}"),
    }
}

#[test]
fn parse_list_type_annotation() {
    let expr = parse_expr_str(r#"[string] -- "list of issues""#);
    match expr {
        Expr::Annotated { expr, annotation } => {
            assert_eq!(annotation, "list of issues");
            match *expr {
                Expr::List(items) => {
                    assert_eq!(items.len(), 1);
                    match &items[0] {
                        Expr::Ident(id) => assert_eq!(id.name, "string"),
                        other => panic!("expected Ident in list, got {other:?}"),
                    }
                }
                other => panic!("expected List, got {other:?}"),
            }
        }
        other => panic!("expected Annotated, got {other:?}"),
    }
}

#[test]
fn subtraction_not_annotation() {
    // `a - b` should be subtraction, not annotation
    let expr = parse_expr_str("a - b");
    match expr {
        Expr::Binary { op, .. } => assert_eq!(op, atman_dsl::ast::BinOp::Sub),
        other => panic!("expected Binary Sub, got {other:?}"),
    }
}

#[test]
fn double_negation_not_annotation() {
    // `a - -b` = `a - (-b)`, should be subtraction + unary neg
    let expr = parse_expr_str("a - -b");
    match expr {
        Expr::Binary {
            op: atman_dsl::ast::BinOp::Sub,
            right,
            ..
        } => match *right {
            Expr::Unary {
                op: atman_dsl::ast::UnOp::Neg,
                ..
            } => {}
            other => panic!("expected Unary Neg on rhs, got {other:?}"),
        },
        other => panic!("expected Binary Sub, got {other:?}"),
    }
}

#[test]
fn annotation_in_struct_field() {
    let src = r#"
flow t() -> string {
    x = { valid: bool -- "is valid" }
    return "ok"
}
"#;
    let file = parse_file(src).expect("parse");
    let stmts = &file.flows[0].body;
    match &stmts[0] {
        atman_dsl::ast::Stmt::Bind { value, .. } => match value {
            Expr::Struct(fields) => {
                assert_eq!(fields.len(), 1);
                assert_eq!(fields[0].0.name, "valid");
                match &fields[0].1 {
                    Expr::Annotated { annotation, .. } => {
                        assert_eq!(annotation, "is valid");
                    }
                    other => panic!("expected Annotated, got {other:?}"),
                }
            }
            other => panic!("expected Struct, got {other:?}"),
        },
        _ => panic!("expected bind stmt"),
    }
}

#[test]
fn roundtrip_annotation() {
    let src = r#"
flow t() -> string {
    x = bool -- "is valid"
    return "ok"
}
"#;
    let file1 = parse_file(src).expect("parse");
    let printed = print_file(&file1);
    let file2 = parse_file(&printed).expect("re-parse");
    assert_eq!(file1.flows.len(), file2.flows.len());
    assert_eq!(file1.flows[0].name.name, file2.flows[0].name.name);
}

#[test]
fn roundtrip_annotation_with_escaping() {
    let src = r#"
flow t() -> string {
    x = bool -- "she said \"hi\""
    return "ok"
}
"#;
    let file1 = parse_file(src).expect("parse");
    let printed = print_file(&file1);
    let _file2 = parse_file(&printed).expect("re-parse with escaping");
}
