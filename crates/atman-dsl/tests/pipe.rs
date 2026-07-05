use atman_dsl::ast::{Arg, Expr, Node, Stmt};
use atman_dsl::{parse::parse_file, print::print_file};

fn parse_stmt_expr(body: &str) -> Expr {
    let src = format!("flow f() {{\n    x = {body}\n}}\n");
    let file = parse_file(&src).unwrap_or_else(|e| panic!("parse `{body}`: {e}"));
    let stmt = &file.flows[0].body[0];
    match stmt {
        Stmt::Bind { value, .. } => value.clone(),
        other => panic!("expected bind, got {other:?}"),
    }
}

fn roundtrip(body: &str) -> String {
    let src = format!("flow f() {{\n    x = {body}\n}}\n");
    let file = parse_file(&src).unwrap_or_else(|e| panic!("parse `{body}`: {e}"));
    print_file(&file)
}

#[test]
fn pipe_parses_into_pipe_variant() {
    let expr = parse_stmt_expr("fs.read(\"foo.txt\") |> len()");
    let Expr::Pipe { lhs, rhs } = expr else {
        panic!("expected Pipe, got {expr:?}");
    };
    assert!(matches!(*lhs, Expr::Node(Node::ToolCall { .. })));
    assert!(matches!(*rhs, Expr::Node(Node::ToolCall { .. })));
}

#[test]
fn pipe_is_left_associative() {
    let expr = parse_stmt_expr("a() |> b() |> c()");
    let Expr::Pipe { lhs, rhs } = expr else {
        panic!("expected outer Pipe, got {expr:?}");
    };
    assert!(
        matches!(*lhs, Expr::Pipe { .. }),
        "outer lhs should be pipe"
    );
    match *rhs {
        Expr::Node(Node::ToolCall { path, .. }) => {
            assert_eq!(path[0].name, "c");
        }
        other => panic!("outer rhs should be c(), got {other:?}"),
    }
}

#[test]
fn pipe_has_lower_precedence_than_arithmetic() {
    let expr = parse_stmt_expr("1 + 2 |> plus(3)");
    let Expr::Pipe { lhs, rhs } = expr else {
        panic!("expected top-level Pipe, got {expr:?}");
    };
    assert!(
        matches!(*lhs, Expr::Binary { .. }),
        "1 + 2 should stay a Binary on the lhs of |>"
    );
    match *rhs {
        Expr::Node(Node::ToolCall { path, args }) => {
            assert_eq!(path[0].name, "plus");
            assert_eq!(args.len(), 1);
            assert!(matches!(args[0], Arg::Positional(Expr::Literal(_))));
        }
        other => panic!("rhs should be plus(3), got {other:?}"),
    }
}

#[test]
fn pipe_round_trip_preserves_source() {
    let printed = roundtrip("fs.read(\"foo.txt\") |> len()");
    assert!(
        printed.contains("|>"),
        "expected pipe operator in printout, got:\n{printed}"
    );
    let reparsed = parse_file(&printed).unwrap_or_else(|e| panic!("reparse: {e}\n---\n{printed}"));
    assert!(matches!(
        reparsed.flows[0].body[0],
        Stmt::Bind {
            value: Expr::Pipe { .. },
            ..
        }
    ));
}

#[test]
fn pipe_survives_multi_stage_roundtrip() {
    let printed = roundtrip("fs.read(\"f\") |> len() |> stdlib.to_json_string()");
    let reparsed = parse_file(&printed).unwrap_or_else(|e| panic!("reparse: {e}\n---\n{printed}"));
    let Stmt::Bind {
        value: Expr::Pipe { lhs: outer_lhs, .. },
        ..
    } = &reparsed.flows[0].body[0]
    else {
        panic!("expected outer pipe binding");
    };
    assert!(
        matches!(**outer_lhs, Expr::Pipe { .. }),
        "left-associative pipe chain should nest"
    );
}
