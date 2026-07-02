use atman_dsl::{parse::parse_file, print::print_file};

const SRC: &str = include_str!("../../../examples/review_code.at");

#[test]
fn parses_canonical_example() {
    let file = parse_file(SRC).unwrap_or_else(|e| panic!("parse failed: {e}"));
    assert_eq!(file.flows.len(), 1);
    assert_eq!(file.flows[0].name.name, "review_code");
}

#[test]
fn roundtrip_ast_equivalence() {
    let file1 = parse_file(SRC).expect("first parse");
    let printed = print_file(&file1);
    let file2 = parse_file(&printed)
        .unwrap_or_else(|e| panic!("re-parse of printed output failed:\n{printed}\n\nerror: {e}"));

    let a = strip_spans(&format!("{:#?}", file1));
    let b = strip_spans(&format!("{:#?}", file2));
    assert_eq!(a, b, "AST diverged after roundtrip");
}

// Ident spans encode byte ranges that drift after reprinting; strip them so
// Debug-format equality survives the roundtrip.
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
