use atman_dsl::ast::{CmpOp, OnBlock, Stmt, WatchAction, WatchEvent};
use atman_dsl::parse::parse_file;
use atman_dsl::print::print_file;

#[test]
fn parses_watch_token_abort_block() {
    let src = r#"flow t() -> Int {
    primary = 1
    watch primary {
        on token(match: "as any" | "@ts-ignore") {
            abort("type-safety")
        }
    }
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let Stmt::Watch(w) = &file.flows[0].body[1] else {
        panic!("expected Watch");
    };
    assert_eq!(w.target.name, "primary");
    assert_eq!(w.on_blocks.len(), 1);
    let OnBlock { event, actions } = &w.on_blocks[0];
    match event {
        WatchEvent::Token { patterns } => {
            assert_eq!(
                patterns,
                &vec!["as any".to_string(), "@ts-ignore".to_string()]
            );
        }
        _ => panic!("expected Token event"),
    }
    assert_eq!(actions.len(), 1);
    matches!(&actions[0], WatchAction::Abort { msg: Some(_) });
}

#[test]
fn parses_watch_elapsed_and_tokens_consumed() {
    let src = r#"flow t() -> Int {
    primary = 1
    watch primary {
        on elapsed(> 30 s) {
            warn("slow")
        }
        on tokens_consumed(>= 10000) {
            abort("budget")
        }
    }
    return primary
}
"#;
    let file = parse_file(src).unwrap();
    let Stmt::Watch(w) = &file.flows[0].body[1] else {
        panic!("expected Watch");
    };
    assert_eq!(w.on_blocks.len(), 2);
    match &w.on_blocks[0].event {
        WatchEvent::Elapsed { cmp, duration_ms } => {
            assert_eq!(*cmp, CmpOp::Gt);
            assert_eq!(*duration_ms, 30_000);
        }
        _ => panic!("expected Elapsed"),
    }
    match &w.on_blocks[1].event {
        WatchEvent::TokensConsumed { cmp, value } => {
            assert_eq!(*cmp, CmpOp::Ge);
            assert_eq!(*value, 10_000);
        }
        _ => panic!("expected TokensConsumed"),
    }
}

#[test]
fn watch_roundtrips() {
    let src = r#"flow t() -> Int {
    primary = 1
    watch primary {
        on token(match: "as any") {
            abort("type-safety")
        }
        on elapsed(> 30 s) {
            warn("slow")
        }
    }
    return primary
}
"#;
    let file1 = parse_file(src).unwrap();
    let printed = print_file(&file1);
    let file2 = parse_file(&printed)
        .unwrap_or_else(|e| panic!("re-parse failed:\n{printed}\n\nerror: {e}"));
    let a = strip_spans(&format!("{:#?}", file1));
    let b = strip_spans(&format!("{:#?}", file2));
    assert_eq!(a, b, "watch AST diverged after roundtrip");
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
