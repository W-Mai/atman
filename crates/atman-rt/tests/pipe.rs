use atman_rt::{parse_file, print_file};

#[test]
fn pipe_operator_is_rejected() {
    for body in [
        "fs.read(\"foo.txt\") |> len()",
        "a() |> b() |> c()",
        "1 + 2 |> plus(3)",
        "|f| f |> len()",
    ] {
        let source = format!("flow f() {{\n    x = {body}\n}}\n");
        assert!(
            parse_file(&source).is_err(),
            "accepted removed pipe syntax: {body}"
        );
    }
}

#[test]
fn explicit_tool_calls_roundtrip() {
    let source =
        "flow f() {\n    data = fs.read(\"foo.txt\")\n    size = len(data)\n    return size\n}\n";
    let file = parse_file(source).expect("parse explicit calls");
    let printed = print_file(&file);
    parse_file(&printed).expect("reparse explicit calls");
    assert!(!printed.contains("|>"), "{printed}");
}
