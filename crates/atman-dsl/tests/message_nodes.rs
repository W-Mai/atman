use atman_dsl::ast::{Arg, Expr, Literal, MessageRole, Node};
use atman_dsl::parse::parse_file;
use atman_dsl::print::print_file;

fn only_return_expr(src: &str) -> Expr {
    let file = parse_file(src).unwrap_or_else(|e| panic!("parse: {e}"));
    let body = &file.flows[0].body;
    if let atman_dsl::ast::Stmt::Return { value } = &body[0] {
        return value.clone();
    }
    panic!("expected return stmt");
}

#[test]
fn user_msg_positional_text_parses() {
    let src = r#"flow t() -> Message { return user_msg("hello") }"#;
    let expr = only_return_expr(src);
    match expr {
        Expr::Node(Node::Message { role, args }) => {
            assert_eq!(role, MessageRole::User);
            assert_eq!(args.len(), 1);
            let Arg::Positional(Expr::Literal(Literal::Str(s))) = &args[0] else {
                panic!("expected string arg, got {:?}", args[0]);
            };
            assert_eq!(s, "hello");
        }
        other => panic!("expected user_msg node, got {other:?}"),
    }
}

#[test]
fn user_msg_with_attachments_parses() {
    let src = r#"flow t() -> Message {
    return user_msg("describe", attachments: [@"pic.png", @"other.jpg"])
}"#;
    let expr = only_return_expr(src);
    let Expr::Node(Node::Message { role, args }) = expr else {
        panic!("expected message node");
    };
    assert_eq!(role, MessageRole::User);
    assert_eq!(args.len(), 2);
    let Arg::Named { name, value } = &args[1] else {
        panic!("expected named");
    };
    assert_eq!(name.name, "attachments");
    let Expr::List(items) = value else {
        panic!("expected list");
    };
    assert_eq!(items.len(), 2);
}

#[test]
fn assistant_and_system_and_tool_result_parse() {
    let src = r#"flow t() -> Message {
    a = assistant_msg("done")
    s = system_msg("you are a reviewer")
    t = tool_result("toolu_1", "output", is_error: false)
    return a
}"#;
    let file = parse_file(src).unwrap_or_else(|e| panic!("parse: {e}"));
    let body = &file.flows[0].body;
    let roles: Vec<MessageRole> = body
        .iter()
        .filter_map(|stmt| {
            if let atman_dsl::ast::Stmt::Bind { value, .. } = stmt
                && let Expr::Node(Node::Message { role, .. }) = value
            {
                return Some(*role);
            }
            None
        })
        .collect();
    assert_eq!(
        roles,
        vec![
            MessageRole::Assistant,
            MessageRole::System,
            MessageRole::Tool
        ]
    );
}

#[test]
fn message_nodes_roundtrip_through_print() {
    let src = r#"flow t() -> Message {
    return user_msg("hi", attachments: [@"a.png"])
}
"#;
    let file1 = parse_file(src).unwrap();
    let printed = print_file(&file1);
    let file2 =
        parse_file(&printed).unwrap_or_else(|e| panic!("reparse: {e}\n---printed---\n{printed}"));

    let strip = |s: String| -> String {
        let mut out = String::with_capacity(s.len());
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '#' && it.peek() == Some(&'0') {
                for c in it.by_ref() {
                    if c == ')' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    };
    assert_eq!(
        strip(format!("{:#?}", file1)),
        strip(format!("{:#?}", file2)),
        "AST diverged after roundtrip"
    );
}
