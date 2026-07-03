use atman_dsl::parse::parse_file;
use atman_runtime::tools::stdlib::{compose_email_preview, shell_quote};

#[test]
fn shell_quote_smoke() {
    assert_eq!(shell_quote("hi"), "'hi'");
    assert_eq!(shell_quote("a'b"), "'a'\\''b'");
}

#[test]
fn compose_email_preview_smoke() {
    let preview = compose_email_preview("s", "b", &["x@a".into(), "y@a".into()]);
    assert_eq!(preview, "To: x@a, y@a\nSubject: s\n---\nb");
}

#[test]
fn examples_lark_mail_send_parses() {
    let src = std::fs::read_to_string("../../examples/lark_mail_send.at").unwrap();
    let file = parse_file(&src).unwrap();
    assert_eq!(file.flows.len(), 1);
    let flow = &file.flows[0];
    assert_eq!(flow.name.name, "lark_mail_send");
    let contract = flow
        .contract
        .as_ref()
        .expect("lark_mail_send must declare shell capability");
    assert!(
        contract
            .blocks
            .iter()
            .any(|b| b.name.name == "capabilities"),
        "capabilities block missing"
    );
}
