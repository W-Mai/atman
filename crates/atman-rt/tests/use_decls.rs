use atman_rt::ast::{Expr, FlowRef, Node, Stmt, UseBinding};
use atman_rt::{parse_file, print_file};

#[test]
fn parses_use_bindings_public_flows_and_qualified_calls() {
    let source = r#"
use "./lib/text.at"::normalize
use "./lib/text.at"::tokenize as words
use "./lib/text.at"::{normalize as clean, tokenize}
use "./lib/text.at" as text

pub flow review(input: string) -> string {
    result = subflow(text.normalize, input)
    return subflow(clean, result)
}

flow helper(input: string) -> string {
    return input
}
"#;
    let file = parse_file(source).expect("parse source");
    assert_eq!(file.uses.len(), 4);
    assert_eq!(file.public_flows.len(), 1);
    assert_eq!(file.public_flows[0].name, "review");
    assert_eq!(file.flows.len(), 2);
    assert!(
        matches!(&file.uses[0].binding, UseBinding::Flows(flows) if flows.len() == 1 && flows[0].name.name == "normalize" && flows[0].alias.is_none())
    );
    assert!(
        matches!(&file.uses[1].binding, UseBinding::Flows(flows) if flows.len() == 1 && flows[0].alias.as_ref().is_some_and(|alias| alias.name == "words"))
    );
    assert!(
        matches!(&file.uses[2].binding, UseBinding::Flows(flows) if flows.len() == 2 && flows[0].alias.as_ref().is_some_and(|alias| alias.name == "clean") && flows[1].name.name == "tokenize")
    );
    assert!(matches!(&file.uses[3].binding, UseBinding::Module(alias) if alias.name == "text"));

    let Stmt::Bind {
        value: Expr::Node(Node::Subflow { name, .. }),
        ..
    } = &file.flows[0].body[0]
    else {
        panic!("expected qualified subflow");
    };
    assert!(
        matches!(name, FlowRef::Qualified { module, flow } if module.name == "text" && flow.name == "normalize")
    );
    assert_eq!(name.display_name(), "text.normalize");

    let printed = print_file(&file);
    let parsed_again = parse_file(&printed).expect("parse printed source");
    assert_eq!(format!("{file:#?}"), format!("{parsed_again:#?}"));
}

#[test]
fn escapes_use_source_when_printing() {
    let file = parse_file("use \"./lib/quo\\\"te\\\\x.at\"::f\nflow main() { return subflow(f) }")
        .expect("parse source with escaped path");
    let printed = print_file(&file);
    let parsed_again = parse_file(&printed).expect("parse printed escaped path");
    assert_eq!(parsed_again.uses[0].source, file.uses[0].source);
}

#[test]
fn rejects_pub_on_other_declarations_and_invalid_use_syntax() {
    assert!(parse_file("pub route \"/\" { flow: main }").is_err());
    assert!(parse_file("pub on session.start {}").is_err());
    assert!(parse_file("use \"./lib.at\"::*").is_err());
    assert!(parse_file("use \"./lib.at\"::{}").is_err());
    assert!(parse_file("use \"./lib.at\"").is_err());
}
