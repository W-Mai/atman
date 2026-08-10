use atman_dsl::ast::{Arg, Expr, Literal, Node, Stmt};
use atman_dsl::parse::parse_file;
use atman_dsl::print::print_file;

fn llm_call_args(value: &Expr) -> &[Arg] {
    match value {
        Expr::Node(Node::ToolCall { path, args })
            if path.len() == 2 && path[0].name == "llm" && path[1].name == "call" =>
        {
            args
        }
        other => panic!("expected llm.call tool call, got {other:?}"),
    }
}

fn named_arg<'a>(args: &'a [Arg], name: &str) -> &'a Expr {
    let arg = args
        .iter()
        .find(|a| matches!(a, Arg::Named { name: n, .. } if n.name == name))
        .unwrap_or_else(|| panic!("missing named arg `{name}`"));
    match arg {
        Arg::Named { value, .. } => value,
        _ => unreachable!(),
    }
}

#[test]
fn retry_is_a_regular_kwarg() {
    let src = r#"flow t() -> Int {
    primary = llm.call(model: "m", prompt: "hi", retry: 3)
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let Stmt::Bind { value, .. } = &file.flows[0].body[0] else {
        panic!();
    };
    let retry = named_arg(llm_call_args(value), "retry");
    assert!(matches!(retry, Expr::Literal(Literal::Int(3))));
}

#[test]
fn llm_call_roundtrips_through_print() {
    let src = r#"flow t() -> string {
    primary = llm.call(model: "opus", prompt: "hi", retry: 2)
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
