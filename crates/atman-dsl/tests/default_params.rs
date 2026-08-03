use atman_dsl::parse::parse_file;

#[test]
fn parses_default_params() {
    let src = r#"flow f(a: string, b: int = 42, c: string = "hi") -> string {
    return a
}"#;
    let file = parse_file(src).unwrap();
    assert_eq!(file.flows.len(), 1);
    let f = &file.flows[0];
    assert_eq!(f.params.len(), 3);
    assert_eq!(f.params[0].name.name, "a");
    assert!(f.params[0].default.is_none());
    assert_eq!(f.params[1].name.name, "b");
    let Some(atman_dsl::ast::Expr::Literal(atman_dsl::ast::Literal::Int(42))) =
        &f.params[1].default
    else {
        panic!("expected Int(42) default for b");
    };
    assert_eq!(f.params[2].name.name, "c");
    let Some(atman_dsl::ast::Expr::Literal(atman_dsl::ast::Literal::Str(s))) = &f.params[2].default
    else {
        panic!("expected Str default for c");
    };
    assert_eq!(s, "hi");
}

#[test]
fn default_params_roundtrip() {
    use atman_dsl::print::print_file;
    let src = r#"flow f(a: string, b: int = 42) -> string {
    return a
}
"#;
    let file = parse_file(src).unwrap();
    let printed = print_file(&file);
    let file2 = parse_file(&printed).unwrap();
    assert_eq!(file2.flows[0].params.len(), 2);
    assert!(file2.flows[0].params[1].default.is_some());
}
