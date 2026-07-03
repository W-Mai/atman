use atman_dsl::ast::{Expr, Literal, Node, Stmt};
use atman_dsl::parse::parse_file;
use atman_dsl::print::print_file;

#[test]
fn retry_is_a_regular_kwarg() {
    let src = r#"flow t() -> Int {
    primary = llm {
        model: "m"
        prompt: "hi"
        retry: 3
    }
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let Stmt::Bind { value, .. } = &file.flows[0].body[0] else {
        panic!();
    };
    let Expr::Node(Node::Llm { kwargs }) = value else {
        panic!();
    };
    let retry = kwargs.iter().find(|(k, _)| k.name == "retry").unwrap();
    matches!(&retry.1, Expr::Literal(Literal::Int(3)));
}

#[test]
fn fallback_is_a_kwarg_holding_another_llm_node() {
    let src = r#"flow t() -> string {
    primary = llm {
        model: "opus"
        prompt: "hi"
        fallback: llm {
            model: "mini"
            prompt: "hi"
        }
    }
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let Stmt::Bind { value, .. } = &file.flows[0].body[0] else {
        panic!();
    };
    let Expr::Node(Node::Llm { kwargs }) = value else {
        panic!();
    };
    let fb = kwargs.iter().find(|(k, _)| k.name == "fallback").unwrap();
    assert!(matches!(&fb.1, Expr::Node(Node::Llm { .. })));
}

#[test]
fn retry_and_fallback_roundtrip() {
    let src = r#"flow t() -> string {
    primary = llm {
        model: "opus"
        prompt: "hi"
        retry: 2
        fallback: llm {
            model: "mini"
            prompt: "hi"
        }
    }
    return primary
}
"#;
    let file1 = parse_file(src).unwrap();
    let printed = print_file(&file1);
    let file2 = parse_file(&printed)
        .unwrap_or_else(|e| panic!("re-parse failed:\n{printed}\n\nerror: {e}"));
    assert_eq!(
        strip_spans(&format!("{:#?}", file1)),
        strip_spans(&format!("{:#?}", file2))
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
