// Not source-preserving: comments and formatting are lost. Only guarantee
// is `parse(print(parse(x))) == parse(x)`, checked by the roundtrip test.

use alloc::{format, string::String};
use core::fmt::Write;

use crate::ast::*;

pub fn print_file(file: &File) -> String {
    let mut out = String::new();
    let mut first = true;
    for use_decl in &file.uses {
        if !first {
            out.push('\n');
        }
        first = false;
        write!(out, "use {:?}", use_decl.source).unwrap();
        match &use_decl.binding {
            UseBinding::Module(alias) => {
                write!(out, " as {}", alias.name).unwrap();
            }
            UseBinding::Flows(flows) if flows.len() == 1 => {
                write_use_flow(&mut out, &flows[0], "::");
            }
            UseBinding::Flows(flows) => {
                out.push_str("::{");
                for (index, flow) in flows.iter().enumerate() {
                    if index > 0 {
                        out.push_str(", ");
                    }
                    write_use_flow(&mut out, flow, "");
                }
                out.push('}');
            }
        }
        out.push('\n');
    }
    for r in &file.routes {
        if !first {
            out.push('\n');
        }
        first = false;
        writeln!(out, "route \"{}\" {{ flow: {} }}", r.pattern, r.flow.name).unwrap();
    }
    if let Some(dr) = &file.default_route {
        if !first {
            out.push('\n');
        }
        first = false;
        writeln!(out, "default_route {{ flow: {} }}", dr.flow.name).unwrap();
    }
    for lc in &file.lifecycles {
        if !first {
            out.push('\n');
        }
        first = false;
        let event_str = match lc.event {
            LifecycleEvent::SessionStart => "session.start",
            LifecycleEvent::SessionEnd => "session.end",
            LifecycleEvent::TurnStart => "turn.start",
            LifecycleEvent::TurnEnd => "turn.end",
            LifecycleEvent::ContextCompact => "session.context_compact",
        };
        writeln!(out, "on {event_str} {{").unwrap();
        write_stmts(&mut out, &lc.body, 1);
        out.push_str("}\n");
    }
    for flow in &file.flows {
        if !first {
            out.push('\n');
        }
        first = false;
        let is_public = file
            .public_flows
            .iter()
            .any(|name| name.name == flow.name.name);
        write_flow(&mut out, flow, is_public);
    }
    out
}

fn write_use_flow(out: &mut String, binding: &UseFlowBinding, prefix: &str) {
    write!(out, "{prefix}{}", binding.name.name).unwrap();
    if let Some(alias) = &binding.alias {
        write!(out, " as {}", alias.name).unwrap();
    }
}

fn write_flow(out: &mut String, flow: &FlowDecl, is_public: bool) {
    if is_public {
        out.push_str("pub ");
    }
    write!(out, "flow {}(", flow.name.name).unwrap();
    for (i, p) in flow.params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write!(out, "{}: ", p.name.name).unwrap();
        write_type(out, &p.ty);
        if let Some(default) = &p.default {
            out.push_str(" = ");
            write_expr(out, default, 0);
        }
    }
    out.push(')');
    if let Some(ret) = &flow.ret {
        out.push_str(" -> ");
        write_type(out, ret);
    }
    out.push_str(" {\n");
    if let Some(contract) = &flow.contract {
        write_contract(out, contract, 1);
    }
    for stmt in &flow.body {
        write_stmt(out, stmt, 1);
    }
    out.push_str("}\n");
}

fn write_contract(out: &mut String, contract: &Contract, indent: usize) {
    let outer_pad = "    ".repeat(indent);
    let inner_pad = "    ".repeat(indent + 1);
    let field_pad = "    ".repeat(indent + 2);
    writeln!(out, "{outer_pad}contract {{").unwrap();
    for block in &contract.blocks {
        writeln!(out, "{inner_pad}{} {{", block.name.name).unwrap();
        for (k, v) in &block.kwargs {
            write!(out, "{field_pad}{}: ", k.name).unwrap();
            write_expr(out, v, indent + 2);
            out.push('\n');
        }
        writeln!(out, "{inner_pad}}}").unwrap();
    }
    writeln!(out, "{outer_pad}}}").unwrap();
}

fn write_type(out: &mut String, ty: &TypeExpr) {
    match ty {
        TypeExpr::Named(id) => out.push_str(&id.name),
        TypeExpr::List(inner) => {
            out.push('[');
            write_type(out, inner);
            out.push(']');
        }
        TypeExpr::Struct(fields) => {
            out.push_str("{ ");
            for (i, (name, ty)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write!(out, "{}: ", name.name).unwrap();
                write_type(out, ty);
            }
            out.push_str(" }");
        }
    }
}

fn write_stmts(out: &mut String, stmts: &[Stmt], indent: usize) {
    for stmt in stmts {
        write_stmt(out, stmt, indent);
    }
}

fn write_stmt(out: &mut String, stmt: &Stmt, indent: usize) {
    let pad = "    ".repeat(indent);
    match stmt {
        Stmt::Bind { name, value } => {
            out.push_str(&pad);
            write_pattern(out, name);
            out.push_str(" = ");
            write_expr(out, value, indent);
            out.push('\n');
        }
        Stmt::When { cond, body } => {
            write!(out, "{pad}when ").unwrap();
            write_expr(out, cond, indent);
            out.push_str(" {\n");
            for s in body {
                write_stmt(out, s, indent + 1);
            }
            writeln!(out, "{pad}}}").unwrap();
        }
        Stmt::Return { value } => {
            write!(out, "{pad}return ").unwrap();
            write_expr(out, value, indent);
            out.push('\n');
        }
        Stmt::Expr(e) => {
            out.push_str(&pad);
            write_expr(out, e, indent);
            out.push('\n');
        }
        Stmt::Watch(w) => write_watch(out, w, indent),
        Stmt::Loop { body } => {
            out.push_str("loop {\n");
            write_stmts(out, body, indent + 1);
            let pad = "    ".repeat(indent);
            out.push_str(&format!("{pad}}}\n"));
        }
        Stmt::Break => {
            out.push_str("break\n");
        }
        Stmt::Continue => {
            out.push_str("continue\n");
        }
        Stmt::Yield => {
            writeln!(out, "{pad}yield").unwrap();
        }
    }
}

fn write_pattern(out: &mut String, pat: &Pattern) {
    match pat {
        Pattern::Ident(id) => out.push_str(&id.name),
        Pattern::Struct { fields } => {
            out.push_str("{ ");
            for (i, f) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&f.source.name);
                match &f.binding {
                    PatternFieldBinding::Same => {}
                    PatternFieldBinding::Rename(target) => {
                        write!(out, ": {}", target.name).unwrap();
                    }
                    PatternFieldBinding::Nested(inner) => {
                        out.push_str(": ");
                        write_pattern(out, inner);
                    }
                }
            }
            out.push_str(" }");
        }
    }
}

fn write_watch(out: &mut String, w: &WatchDecl, indent: usize) {
    let pad = "    ".repeat(indent);
    let inner_pad = "    ".repeat(indent + 1);
    let body_pad = "    ".repeat(indent + 2);
    writeln!(out, "{pad}watch {} {{", w.target.name).unwrap();
    for block in &w.on_blocks {
        write!(out, "{inner_pad}on ").unwrap();
        match &block.event {
            WatchEvent::Token { patterns } => {
                out.push_str("token(match: ");
                for (i, p) in patterns.iter().enumerate() {
                    if i > 0 {
                        out.push_str(" | ");
                    }
                    write!(out, "\"{}\"", p.replace('"', "\\\"")).unwrap();
                }
                out.push(')');
            }
            WatchEvent::Elapsed { cmp, duration_ms } => {
                let (n, unit) = if *duration_ms % 1000 == 0 {
                    (duration_ms / 1000, "s")
                } else {
                    (*duration_ms, "ms")
                };
                write!(out, "elapsed({} {n} {unit})", cmp_str(*cmp)).unwrap();
            }
            WatchEvent::TokensConsumed { cmp, value } => {
                write!(out, "tokens_consumed({} {value})", cmp_str(*cmp)).unwrap();
            }
        }
        out.push_str(" {\n");
        for action in &block.actions {
            write!(out, "{body_pad}").unwrap();
            match action {
                WatchAction::Abort { msg } => {
                    out.push_str("abort(");
                    if let Some(m) = msg {
                        write_expr(out, m, indent + 2);
                    }
                    out.push(')');
                }
                WatchAction::Warn { msg } => {
                    out.push_str("warn(");
                    if let Some(m) = msg {
                        write_expr(out, m, indent + 2);
                    }
                    out.push(')');
                }
            }
            out.push('\n');
        }
        writeln!(out, "{inner_pad}}}").unwrap();
    }
    writeln!(out, "{pad}}}").unwrap();
}

fn cmp_str(op: CmpOp) -> &'static str {
    match op {
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
    }
}

fn write_expr(out: &mut String, expr: &Expr, indent: usize) {
    match expr {
        Expr::Literal(l) => write_literal(out, l),
        Expr::Ident(id) => out.push_str(&id.name),
        Expr::FileRef(f) => write!(out, "@\"{}\"", f.path).unwrap(),
        Expr::Member { base, field } => {
            write_postfix_base(out, base, indent);
            write!(out, ".{}", field.name).unwrap();
        }
        Expr::Index { base, index } => {
            write_postfix_base(out, base, indent);
            out.push('[');
            write_expr(out, index, indent);
            out.push(']');
        }
        Expr::Await { value } => {
            write_postfix_base(out, value, indent);
            out.push_str(".await");
        }
        Expr::Binary { op, left, right } => {
            write_binary_operand(out, left, *op, false, indent);
            write!(out, " {} ", binop_str(*op)).unwrap();
            write_binary_operand(out, right, *op, true, indent);
        }
        Expr::Unary { op, operand } => {
            out.push_str(unop_str(*op));
            let grouped = matches!(
                operand.as_ref(),
                Expr::Binary { .. } | Expr::Annotated { .. }
            );
            if grouped {
                out.push('(');
            }
            write_expr(out, operand, indent);
            if grouped {
                out.push(')');
            }
        }
        Expr::Call { func, args } => {
            write!(out, "{}(", func.name).unwrap();
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_expr(out, a, indent);
            }
            out.push(')');
        }
        Expr::Struct(fields) => {
            out.push_str("{ ");
            for (i, (name, value)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write!(out, "{}: ", name.name).unwrap();
                write_expr(out, value, indent);
            }
            out.push_str(" }");
        }
        Expr::List(items) => {
            out.push('[');
            for (i, it) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write_expr(out, it, indent);
            }
            out.push(']');
        }
        Expr::Node(node) => write_node(out, node, indent),
        Expr::Annotated { expr, annotation } => {
            write_expr(out, expr, indent);
            out.push_str(" -- \"");
            out.push_str(&annotation.replace('\\', "\\\\").replace('"', "\\\""));
            out.push('"');
        }
        Expr::Lambda { params, body } => {
            out.push('|');
            for (i, p) in params.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&p.name);
            }
            out.push_str("| ");
            write_expr(out, body, indent);
        }
    }
}

fn write_postfix_base(out: &mut String, value: &Expr, indent: usize) {
    let grouped = matches!(
        value,
        Expr::Binary { .. }
            | Expr::Unary { .. }
            | Expr::Annotated { .. }
            | Expr::Lambda { .. }
            | Expr::Node(Node::Fanout { .. } | Node::DynamicFanout { .. })
    );
    if grouped {
        out.push('(');
    }
    write_expr(out, value, indent);
    if grouped {
        out.push(')');
    }
}

fn binary_precedence(op: BinOp) -> u8 {
    match op {
        BinOp::Or => 1,
        BinOp::And => 2,
        BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => 3,
        BinOp::Add | BinOp::Sub => 4,
        BinOp::Mul | BinOp::Div | BinOp::Mod => 5,
    }
}

fn write_binary_operand(out: &mut String, child: &Expr, parent: BinOp, right: bool, indent: usize) {
    let grouped = match child {
        Expr::Binary { op, .. } => {
            binary_precedence(*op) < binary_precedence(parent)
                || (right && binary_precedence(*op) == binary_precedence(parent))
        }
        Expr::Annotated { .. } | Expr::Lambda { .. } => true,
        _ => false,
    };
    if grouped {
        out.push('(');
    }
    write_expr(out, child, indent);
    if grouped {
        out.push(')');
    }
}

fn write_node(out: &mut String, node: &Node, indent: usize) {
    let pad = "    ".repeat(indent + 1);
    let outer_pad = "    ".repeat(indent);
    match node {
        Node::ToolCall { path, args } => {
            for (i, seg) in path.iter().enumerate() {
                if i > 0 {
                    out.push('.');
                }
                out.push_str(&seg.name);
            }
            out.push('(');
            write_args(out, args, indent);
            out.push(')');
        }
        Node::FlowCall { name, args } => {
            write!(out, "{}(", name.display_name()).unwrap();
            write_args(out, args, indent);
            out.push(')');
        }
        Node::Fanout { source } => {
            out.push_str("fanout ");
            if let Expr::List(items) = source.as_ref() {
                out.push_str("[\n");
                for it in items {
                    write!(out, "{pad}").unwrap();
                    write_expr(out, it, indent + 1);
                    out.push_str(",\n");
                }
                write!(out, "{outer_pad}]").unwrap();
            } else {
                write_expr(out, source, indent);
            }
        }
        Node::DynamicFanout { source, lambda } => {
            out.push_str("fanout ");
            write_expr(out, source, indent);
            out.push_str(" { ");
            write_expr(out, lambda, indent);
            out.push_str(" }");
        }
        Node::UserConfirm { msg } => {
            out.push_str("user_confirm(");
            write_expr(out, msg, indent);
            out.push(')');
        }
        Node::FixUntilTestPasses { kwargs } => {
            out.push_str("fix_until_test_passes {\n");
            for (name, value) in kwargs {
                write!(out, "{pad}{}: ", name.name).unwrap();
                write_expr(out, value, indent + 1);
                out.push('\n');
            }
            write!(out, "{outer_pad}}}").unwrap();
        }
        Node::Message { role, args } => {
            out.push_str(role.keyword());
            out.push('(');
            let mut first = true;
            for a in args {
                if !first {
                    out.push_str(", ");
                }
                first = false;
                match a {
                    Arg::Positional(e) => write_expr(out, e, indent),
                    Arg::Named { name, value } => {
                        write!(out, "{}: ", name.name).unwrap();
                        write_expr(out, value, indent);
                    }
                }
            }
            out.push(')');
        }
    }
}

fn write_args(out: &mut String, args: &[Arg], indent: usize) {
    for (index, arg) in args.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        match arg {
            Arg::Positional(value) => write_expr(out, value, indent),
            Arg::Named { name, value } => {
                write!(out, "{}: ", name.name).unwrap();
                write_expr(out, value, indent);
            }
        }
    }
}

fn write_literal(out: &mut String, lit: &Literal) {
    match lit {
        Literal::Str(s) => {
            write!(out, "\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")).unwrap()
        }
        Literal::Int(n) => write!(out, "{n}").unwrap(),
        Literal::Float(n) => write!(out, "{n}").unwrap(),
        Literal::Bool(b) => write!(out, "{b}").unwrap(),
    }
}

fn binop_str(op: BinOp) -> &'static str {
    match op {
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "&&",
        BinOp::Or => "||",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
    }
}

fn unop_str(op: UnOp) -> &'static str {
    match op {
        UnOp::Not => "!",
        UnOp::Neg => "-",
    }
}
