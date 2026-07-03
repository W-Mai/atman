use atman_dsl::ast::{Expr, Node, Stmt};
use atman_dsl::parse::parse_file;
use atman_dsl::print::print_file;

fn strip_spans(s: String) -> String {
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
}

#[test]
fn fix_until_test_passes_parses_with_lazy_blocks() {
    let src = r#"flow attempt() -> string {
    return fix_until_test_passes {
        edit_flow: llm { model: "m", prompt: "fix it" }
        test: bash.exec("cargo test")
        max_iters: 3
    }
}
"#;
    let file = parse_file(src).unwrap();
    let body = &file.flows[0].body;
    let Stmt::Return { value } = &body[0] else {
        panic!("expected return");
    };
    let Expr::Node(Node::FixUntilTestPasses { kwargs }) = value else {
        panic!("expected fix_until_test_passes node, got {value:?}");
    };
    let names: Vec<&str> = kwargs.iter().map(|(k, _)| k.name.as_str()).collect();
    assert_eq!(names, vec!["edit_flow", "test", "max_iters"]);
}

#[test]
fn fix_until_test_passes_roundtrips_through_print() {
    let src = r#"flow attempt() -> string {
    return fix_until_test_passes {
        edit_flow: llm { model: "m", prompt: "fix" }
        test: bash.exec("cargo test")
        max_iters: 5
        on_giveup: user_confirm("gave up, continue?")
    }
}
"#;
    let file1 = parse_file(src).unwrap();
    let printed = print_file(&file1);
    let file2 =
        parse_file(&printed).unwrap_or_else(|e| panic!("reparse: {e}\n---printed---\n{printed}"));
    assert_eq!(
        strip_spans(format!("{:#?}", file1)),
        strip_spans(format!("{:#?}", file2)),
        "AST diverged after roundtrip"
    );
}
