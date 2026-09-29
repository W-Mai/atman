//! Opt-in validation of linked host tool calls against a portable catalog.

use alloc::{
    collections::BTreeSet,
    format,
    string::{String, ToString},
    vec::Vec,
};
use core::fmt;

use crate::{
    ast::{Arg, Expr, LifecycleEvent, Literal, Node, Span, Stmt, WatchAction},
    catalog::{ToolCatalog, ToolSpec, TypeSpec},
    expr::ToolCallMode,
    list::ListIntrinsic,
    program::LinkedProgram,
};

/// One catalog validation diagnostic tied to its linked source location.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolValidationError {
    pub source: String,
    pub flow: String,
    pub span: Span,
    pub message: String,
}

impl fmt::Display for ToolValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{}:{}: flow `{}`: {}",
            self.source, self.span, self.flow, self.message
        )
    }
}

impl core::error::Error for ToolValidationError {}

/// All host tool diagnostics found in one linked program.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolValidationReport {
    pub errors: Vec<ToolValidationError>,
}

impl ToolValidationReport {
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }
}

impl fmt::Display for ToolValidationReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, error) in self.errors.iter().enumerate() {
            if index > 0 {
                formatter.write_str("\n")?;
            }
            error.fmt(formatter)?;
        }
        Ok(())
    }
}

impl core::error::Error for ToolValidationReport {}

/// Validates every statically linked host tool call against `catalog`.
///
/// Normal compilation remains catalog-free. Callers opt into this pass when
/// their complete host tool set is known before execution.
pub fn validate_tools(
    program: &LinkedProgram,
    catalog: &ToolCatalog,
) -> Result<(), ToolValidationReport> {
    let mut errors = Vec::new();

    for (_, module) in program.modules() {
        for flow in &module.file.flows {
            let mut walker = Walker {
                catalog,
                source: &module.source_id,
                flow: &flow.name.name,
                errors: &mut errors,
            };
            for param in &flow.params {
                if let Some(default) = &param.default {
                    walker.walk_expr(default);
                }
            }
            if let Some(contract) = &flow.contract {
                for block in &contract.blocks {
                    for (_, value) in &block.kwargs {
                        walker.walk_expr(value);
                    }
                }
            }
            walker.walk_stmts(&flow.body);
        }
    }

    let entry = program
        .module(program.entry_module())
        .expect("a linked program always contains its entry module");
    for lifecycle in &entry.file.lifecycles {
        let flow = lifecycle_name(lifecycle.event);
        Walker {
            catalog,
            source: &entry.source_id,
            flow: &flow,
            errors: &mut errors,
        }
        .walk_stmts(&lifecycle.body);
    }

    let report = ToolValidationReport { errors };
    if report.is_empty() {
        Ok(())
    } else {
        Err(report)
    }
}

fn lifecycle_name(event: LifecycleEvent) -> String {
    let event = match event {
        LifecycleEvent::SessionStart => "session.start",
        LifecycleEvent::SessionEnd => "session.end",
        LifecycleEvent::TurnStart => "turn.start",
        LifecycleEvent::TurnEnd => "turn.end",
        LifecycleEvent::ContextCompact => "session.context_compact",
    };
    format!("on {event}")
}

struct Walker<'catalog, 'context, 'errors> {
    catalog: &'catalog ToolCatalog,
    source: &'context str,
    flow: &'context str,
    errors: &'errors mut Vec<ToolValidationError>,
}

impl Walker<'_, '_, '_> {
    fn walk_stmts(&mut self, statements: &[Stmt]) {
        for statement in statements {
            match statement {
                Stmt::Bind { value, .. } | Stmt::Return { value } | Stmt::Expr(value) => {
                    self.walk_expr(value);
                }
                Stmt::When { cond, body } => {
                    self.walk_expr(cond);
                    self.walk_stmts(body);
                }
                Stmt::Watch(watch) => {
                    for block in &watch.on_blocks {
                        for action in &block.actions {
                            match action {
                                WatchAction::Abort { msg: Some(value) }
                                | WatchAction::Warn { msg: Some(value) } => self.walk_expr(value),
                                WatchAction::Abort { msg: None }
                                | WatchAction::Warn { msg: None } => {}
                            }
                        }
                    }
                }
                Stmt::Loop { body } => self.walk_stmts(body),
                Stmt::Break | Stmt::Continue | Stmt::Yield => {}
            }
        }
    }

    fn walk_expr(&mut self, expression: &Expr) {
        match expression {
            Expr::Literal(_) | Expr::Ident(_) | Expr::FileRef(_) => {}
            Expr::Member { base, .. }
            | Expr::Unary { operand: base, .. }
            | Expr::Annotated { expr: base, .. } => self.walk_expr(base),
            Expr::Await { value } => {
                if let Expr::Node(Node::ToolCall { path, .. }) = value.as_ref()
                    && ListIntrinsic::from_path(path).is_none()
                {
                    let name = tool_name(path);
                    if let Some(entry) = self.catalog.lookup(&name)
                        && entry.mode == ToolCallMode::Immediate
                    {
                        self.push_error(
                            tool_span(path),
                            format!("immediate tool `{name}` cannot be awaited"),
                        );
                    }
                }
                self.walk_expr(value);
            }
            Expr::Binary { left, right, .. } => {
                self.walk_expr(left);
                self.walk_expr(right);
            }
            Expr::Index { base, index } => {
                self.walk_expr(base);
                self.walk_expr(index);
            }
            Expr::Call { args, .. } | Expr::List(args) => {
                for argument in args {
                    self.walk_expr(argument);
                }
            }
            Expr::Struct(fields) => {
                for (_, value) in fields {
                    self.walk_expr(value);
                }
            }
            Expr::Lambda { body, .. } => self.walk_expr(body),
            Expr::Node(node) => self.walk_node(node),
        }
    }

    fn walk_node(&mut self, node: &Node) {
        match node {
            Node::ToolCall { path, args } => {
                if ListIntrinsic::from_path(path).is_none() {
                    self.validate_tool_call(path, args);
                }
                self.walk_args(args);
            }
            Node::FlowCall { args, .. } | Node::Message { args, .. } => self.walk_args(args),
            Node::Fanout { source } | Node::UserConfirm { msg: source } => {
                self.walk_expr(source);
            }
            Node::DynamicFanout { source, lambda } => {
                self.walk_expr(source);
                self.walk_expr(lambda);
            }
            Node::FixUntilTestPasses { kwargs } => {
                for (_, value) in kwargs {
                    self.walk_expr(value);
                }
            }
        }
    }

    fn walk_args(&mut self, args: &[Arg]) {
        for argument in args {
            match argument {
                Arg::Positional(value) | Arg::Named { value, .. } => self.walk_expr(value),
            }
        }
    }

    fn validate_tool_call(&mut self, path: &[crate::ast::Ident], args: &[Arg]) {
        let name = tool_name(path);
        let span = tool_span(path);
        let Some(entry) = self.catalog.lookup(&name) else {
            self.push_error(span, format!("unknown tool `{name}`"));
            return;
        };
        let Some(spec) = &entry.spec else {
            return;
        };
        validate_signature(spec, &name, args, span, self.source, self.flow, self.errors);
    }

    fn push_error(&mut self, span: Span, message: String) {
        push_error(self.errors, self.source, self.flow, span, message);
    }
}

fn tool_name(path: &[crate::ast::Ident]) -> String {
    path.iter()
        .map(|part| part.name.as_str())
        .collect::<Vec<_>>()
        .join(".")
}

fn tool_span(path: &[crate::ast::Ident]) -> Span {
    path.first().map(|part| part.span).unwrap_or_default()
}

fn validate_signature(
    spec: &ToolSpec,
    tool: &str,
    args: &[Arg],
    call_span: Span,
    source: &str,
    flow: &str,
    errors: &mut Vec<ToolValidationError>,
) {
    let positional_count = args
        .iter()
        .filter(|argument| matches!(argument, Arg::Positional(_)))
        .count();
    let positional_arity = spec
        .params
        .iter()
        .map(|param| param.position)
        .max()
        .map_or(0, |position| position.saturating_add(1));
    if positional_count > positional_arity {
        push_error(
            errors,
            source,
            flow,
            call_span,
            format!(
                "tool `{tool}` accepts at most {positional_arity} positional arguments, but got {positional_count}"
            ),
        );
    }

    let mut named = BTreeSet::new();
    for argument in args {
        let Arg::Named { name, value } = argument else {
            continue;
        };
        if !named.insert(name.name.clone()) {
            push_error(
                errors,
                source,
                flow,
                name.span,
                format!(
                    "tool `{tool}` receives named parameter `{}` more than once",
                    name.name
                ),
            );
            continue;
        }
        let Some(param) = spec.params.iter().find(|param| param.name == name.name) else {
            push_error(
                errors,
                source,
                flow,
                name.span,
                format!("tool `{tool}` has no parameter `{}`", name.name),
            );
            continue;
        };
        validate_known_type(
            value,
            &param.ty,
            tool,
            &param.name,
            name.span,
            source,
            flow,
            errors,
        );
    }

    let mut position = 0usize;
    for argument in args {
        let Arg::Positional(value) = argument else {
            continue;
        };
        if let Some(param) = spec.params.iter().find(|param| param.position == position)
            && !named.contains(&param.name)
        {
            validate_known_type(
                value,
                &param.ty,
                tool,
                &param.name,
                call_span,
                source,
                flow,
                errors,
            );
        }
        position = position.saturating_add(1);
    }

    for param in spec.params.iter().filter(|param| param.required) {
        if param.position >= positional_count && !named.contains(&param.name) {
            push_error(
                errors,
                source,
                flow,
                call_span,
                format!(
                    "tool `{tool}` is missing required parameter `{}`",
                    param.name
                ),
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_known_type(
    expression: &Expr,
    expected: &TypeSpec,
    tool: &str,
    path: &str,
    span: Span,
    source: &str,
    flow: &str,
    errors: &mut Vec<ToolValidationError>,
) {
    match expected {
        TypeSpec::Any => return,
        TypeSpec::Option(inner) => {
            validate_known_type(expression, inner, tool, path, span, source, flow, errors);
            return;
        }
        _ => {}
    }

    match expression {
        Expr::Literal(literal) => {
            validate_literal(literal, expected, tool, path, span, source, flow, errors)
        }
        Expr::Struct(fields) => {
            let TypeSpec::Struct(spec) = expected else {
                push_type_mismatch(errors, source, flow, span, tool, path, expected, "struct");
                return;
            };
            let mut seen = BTreeSet::new();
            for (name, value) in fields {
                let field_path = format!("{path}.{}", name.name);
                if !seen.insert(name.name.clone()) {
                    push_error(
                        errors,
                        source,
                        flow,
                        name.span,
                        format!(
                            "tool `{tool}` parameter `{field_path}` is specified more than once"
                        ),
                    );
                    continue;
                }
                if let Some(field) = spec.fields.iter().find(|field| field.name == name.name) {
                    validate_known_type(
                        value,
                        &field.ty,
                        tool,
                        &field_path,
                        name.span,
                        source,
                        flow,
                        errors,
                    );
                }
            }
            for field in &spec.fields {
                if !matches!(field.ty, TypeSpec::Option(_)) && !seen.contains(&field.name) {
                    push_error(
                        errors,
                        source,
                        flow,
                        span,
                        format!(
                            "tool `{tool}` parameter `{path}` is missing required field `{}`",
                            field.name
                        ),
                    );
                }
            }
        }
        Expr::List(items) => {
            let TypeSpec::List(item_type) = expected else {
                push_type_mismatch(errors, source, flow, span, tool, path, expected, "list");
                return;
            };
            for (index, item) in items.iter().enumerate() {
                validate_known_type(
                    item,
                    item_type,
                    tool,
                    &format!("{path}[{index}]"),
                    span,
                    source,
                    flow,
                    errors,
                );
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_literal(
    literal: &Literal,
    expected: &TypeSpec,
    tool: &str,
    path: &str,
    span: Span,
    source: &str,
    flow: &str,
    errors: &mut Vec<ToolValidationError>,
) {
    match (literal, expected) {
        (Literal::Str(_), TypeSpec::String) | (Literal::Bool(_), TypeSpec::Bool) => {}
        (Literal::Str(value), TypeSpec::Enum(spec)) => {
            if !spec.variants.iter().any(|variant| variant == value) {
                push_error(
                    errors,
                    source,
                    flow,
                    span,
                    format!(
                        "tool `{tool}` parameter `{path}` expected {}, but `{value}` is not a declared variant",
                        expected
                    ),
                );
            }
        }
        (Literal::Int(value), TypeSpec::Int { min, max }) => {
            let value = i128::from(*value);
            if value < *min || value > *max {
                push_error(
                    errors,
                    source,
                    flow,
                    span,
                    format!(
                        "tool `{tool}` parameter `{path}` expected int in {min}..={max}, got {value}"
                    ),
                );
            }
        }
        (Literal::Float(value), TypeSpec::Float { bits }) => {
            if *bits <= 32
                && value.is_finite()
                && !(f32::MIN as f64..=f32::MAX as f64).contains(value)
            {
                push_error(
                    errors,
                    source,
                    flow,
                    span,
                    format!(
                        "tool `{tool}` parameter `{path}` expected a {bits}-bit float, got {value}"
                    ),
                );
            }
        }
        (literal, expected) => push_type_mismatch(
            errors,
            source,
            flow,
            span,
            tool,
            path,
            expected,
            literal_kind(literal),
        ),
    }
}

fn literal_kind(literal: &Literal) -> &'static str {
    match literal {
        Literal::Str(_) => "string",
        Literal::Int(_) => "int",
        Literal::Float(_) => "float",
        Literal::Bool(_) => "bool",
    }
}

#[allow(clippy::too_many_arguments)]
fn push_type_mismatch(
    errors: &mut Vec<ToolValidationError>,
    source: &str,
    flow: &str,
    span: Span,
    tool: &str,
    path: &str,
    expected: &TypeSpec,
    actual: &str,
) {
    push_error(
        errors,
        source,
        flow,
        span,
        format!("tool `{tool}` parameter `{path}` expected {expected}, got {actual}"),
    );
}

fn push_error(
    errors: &mut Vec<ToolValidationError>,
    source: &str,
    flow: &str,
    span: Span,
    message: String,
) {
    errors.push(ToolValidationError {
        source: source.to_string(),
        flow: flow.to_string(),
        span,
        message,
    });
}

#[cfg(all(test, feature = "syntax"))]
mod tests {
    use alloc::{boxed::Box, collections::BTreeMap, string::ToString, vec};

    use super::*;
    use crate::{
        ast::{Contract, ContractBlock, File, FlowDecl, Ident, LifecycleDecl, ParamDecl, TypeExpr},
        catalog::{CatalogEntry, FieldSpec, StructSpec, ToolParamSpec},
        program::{ModuleId, ModuleInput},
    };

    fn ident(name: &str) -> Ident {
        Ident::new(name, Span { line: 4, column: 2 })
    }

    fn call(name: &str, args: Vec<Arg>) -> Expr {
        Expr::Node(Node::ToolCall {
            path: name.split('.').map(ident).collect(),
            args,
        })
    }

    fn linked(file: File) -> LinkedProgram {
        LinkedProgram::link(
            vec![ModuleInput {
                source_id: "entry.at".to_string(),
                display_name: "entry.at".to_string(),
                source: String::new(),
                file,
                dependencies: BTreeMap::new(),
            }],
            ModuleId(0),
        )
        .unwrap()
    }

    #[test]
    fn traverses_defaults_contracts_nested_statements_watch_actions_and_lifecycle() {
        let unknown = |name| call(name, vec![]);
        let flow = FlowDecl {
            name: ident("main"),
            params: vec![ParamDecl {
                name: ident("value"),
                ty: TypeExpr::Named(ident("int")),
                default: Some(unknown("host.default")),
            }],
            ret: None,
            contract: Some(Contract {
                blocks: vec![ContractBlock {
                    name: ident("rule"),
                    kwargs: vec![(ident("check"), unknown("host.contract"))],
                }],
            }),
            body: vec![Stmt::When {
                cond: unknown("host.when"),
                body: vec![Stmt::Loop {
                    body: vec![Stmt::Watch(crate::ast::WatchDecl {
                        target: ident("value"),
                        on_blocks: vec![crate::ast::OnBlock {
                            event: crate::ast::WatchEvent::Elapsed {
                                cmp: crate::ast::CmpOp::Ge,
                                duration_ms: 1,
                            },
                            actions: vec![WatchAction::Warn {
                                msg: Some(unknown("host.watch")),
                            }],
                        }],
                    })],
                }],
            }],
        };
        let program = linked(File {
            flows: vec![flow],
            lifecycles: vec![LifecycleDecl {
                event: LifecycleEvent::SessionStart,
                body: vec![Stmt::Expr(unknown("host.lifecycle"))],
                span: Span::default(),
            }],
            ..File::default()
        });

        let report = validate_tools(&program, &ToolCatalog::default()).unwrap_err();
        assert_eq!(report.errors.len(), 5);
        assert_eq!(report.errors[0].source, "entry.at");
        assert_eq!(report.errors[0].flow, "main");
        assert_eq!(report.errors[4].flow, "on session.start");
    }

    #[test]
    fn validates_signature_known_shapes_and_immediate_await() {
        let config = TypeSpec::Struct(StructSpec {
            name: "Config".to_string(),
            fields: vec![
                FieldSpec {
                    name: "title".to_string(),
                    ty: TypeSpec::String,
                },
                FieldSpec {
                    name: "retries".to_string(),
                    ty: TypeSpec::Option(Box::new(TypeSpec::Int { min: 0, max: 3 })),
                },
            ],
        });
        let spec = ToolSpec {
            name: "host.paint".to_string(),
            namespace: "host".to_string(),
            description: String::new(),
            mode: ToolCallMode::Immediate,
            params: vec![
                ToolParamSpec {
                    name: "config".to_string(),
                    position: 0,
                    required: true,
                    ty: config,
                },
                ToolParamSpec {
                    name: "tags".to_string(),
                    position: 1,
                    required: false,
                    ty: TypeSpec::List(Box::new(TypeSpec::String)),
                },
            ],
            result: TypeSpec::Unit,
        };
        let catalog = ToolCatalog::new(vec![CatalogEntry {
            name: "host.paint".to_string(),
            mode: ToolCallMode::Immediate,
            spec: Some(spec),
        }]);
        let expression = Expr::Await {
            value: Box::new(call(
                "host.paint",
                vec![
                    Arg::Positional(Expr::Struct(vec![(
                        ident("retries"),
                        Expr::Literal(Literal::Int(9)),
                    )])),
                    Arg::Named {
                        name: ident("tags"),
                        value: Expr::List(vec![
                            Expr::Literal(Literal::Str("ok".to_string())),
                            Expr::Literal(Literal::Int(1)),
                        ]),
                    },
                ],
            )),
        };
        let program = linked(File {
            flows: vec![FlowDecl {
                name: ident("main"),
                params: vec![],
                ret: None,
                contract: None,
                body: vec![Stmt::Expr(expression)],
            }],
            ..File::default()
        });

        let report = crate::Vm::new(program)
            .validate_tools(&catalog)
            .unwrap_err();
        let messages = report
            .errors
            .iter()
            .map(|error| error.message.as_str())
            .collect::<Vec<_>>();
        assert!(
            messages
                .iter()
                .any(|message| message.contains("cannot be awaited"))
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("missing required field `title`"))
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("expected int in 0..=3"))
        );
        assert!(
            messages
                .iter()
                .any(|message| message.contains("parameter `tags[1]` expected string"))
        );
    }

    #[test]
    fn entries_without_specs_skip_argument_schema_checks() {
        let catalog = ToolCatalog::new(vec![CatalogEntry {
            name: "manual".to_string(),
            mode: ToolCallMode::Immediate,
            spec: None,
        }]);
        let program = linked(File {
            flows: vec![FlowDecl {
                name: ident("main"),
                params: vec![],
                ret: None,
                contract: None,
                body: vec![Stmt::Expr(call(
                    "manual",
                    vec![Arg::Named {
                        name: ident("anything"),
                        value: Expr::Literal(Literal::Bool(true)),
                    }],
                ))],
            }],
            ..File::default()
        });

        assert!(validate_tools(&program, &catalog).is_ok());
    }

    #[test]
    fn immediate_mode_is_checked_without_a_signature() {
        let catalog = ToolCatalog::new(vec![CatalogEntry {
            name: "manual".to_string(),
            mode: ToolCallMode::Immediate,
            spec: None,
        }]);
        let program = linked(File {
            flows: vec![FlowDecl {
                name: ident("main"),
                params: vec![],
                ret: None,
                contract: None,
                body: vec![Stmt::Expr(Expr::Await {
                    value: Box::new(call("manual", vec![])),
                })],
            }],
            ..File::default()
        });

        let report = validate_tools(&program, &catalog).unwrap_err();
        assert_eq!(report.errors.len(), 1);
        assert!(report.errors[0].message.contains("cannot be awaited"));
    }

    #[test]
    fn linked_flow_calls_and_list_intrinsics_do_not_require_catalog_entries() {
        let program = linked(File {
            flows: vec![
                FlowDecl {
                    name: ident("child"),
                    params: vec![],
                    ret: None,
                    contract: None,
                    body: vec![],
                },
                FlowDecl {
                    name: ident("main"),
                    params: vec![],
                    ret: None,
                    contract: None,
                    body: vec![
                        Stmt::Expr(call("child", vec![])),
                        Stmt::Expr(call("list.len", vec![Arg::Positional(Expr::List(vec![]))])),
                    ],
                },
            ],
            ..File::default()
        });

        assert!(validate_tools(&program, &ToolCatalog::default()).is_ok());
    }
}
