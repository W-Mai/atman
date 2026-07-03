use atman_dsl::{parse::parse_file, print::print_file};

const SRC: &str = include_str!("../../../examples/review_code_with_contract.at");

#[test]
fn parses_contract_block() {
    let file = parse_file(SRC).unwrap_or_else(|e| panic!("parse failed: {e}"));
    let flow = &file.flows[0];
    let contract = flow.contract.as_ref().expect("contract block present");
    assert_eq!(contract.blocks.len(), 2);
    assert_eq!(contract.blocks[0].name.name, "scope");
    assert_eq!(contract.blocks[1].name.name, "interjection");
}

#[test]
fn scope_block_carries_read_and_write_kwargs() {
    let file = parse_file(SRC).unwrap();
    let scope = &file.flows[0].contract.as_ref().unwrap().blocks[0];
    let keys: Vec<_> = scope.kwargs.iter().map(|(k, _)| k.name.as_str()).collect();
    assert_eq!(keys, vec!["read", "write"]);
}

#[test]
fn contract_roundtrip_stable() {
    let file1 = parse_file(SRC).expect("first parse");
    let printed = print_file(&file1);
    let file2 = parse_file(&printed)
        .unwrap_or_else(|e| panic!("re-parse of printed output failed:\n{printed}\n\nerror: {e}"));
    let a = strip_spans(&format!("{:#?}", file1));
    let b = strip_spans(&format!("{:#?}", file2));
    assert_eq!(a, b, "contract AST diverged after roundtrip");
}

#[test]
fn flow_without_contract_still_parses() {
    let src = r#"flow trivial() -> Unit {
    return 0
}
"#;
    let file = parse_file(src).unwrap();
    assert!(file.flows[0].contract.is_none());
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
