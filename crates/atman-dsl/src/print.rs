// Not source-preserving: comments and formatting are lost. Only guarantee
// is `parse(print(parse(x))) == parse(x)`, checked by the roundtrip test.

use std::fmt::Write;

use crate::ast::*;

pub fn print_file(file: &File) -> String {
    let mut out = String::new();
    for (i, flow) in file.flows.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        write_flow(&mut out, flow);
    }
    out
}

fn write_flow(out: &mut String, flow: &FlowDecl) {
    write!(out, "flow {}(", flow.name.name).unwrap();
    for (i, (name, ty)) in flow.params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        write!(out, "{}: ", name.name).unwrap();
        write_type(out, ty);
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

fn write_stmt(out: &mut String, stmt: &Stmt, indent: usize) {
    let pad = "    ".repeat(indent);
    match stmt {
        Stmt::Bind { name, value } => {
            write!(out, "{pad}{} = ", name.name).unwrap();
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
            write_expr(out, base, indent);
            write!(out, ".{}", field.name).unwrap();
        }
        Expr::Binary { op, left, right } => {
            write_expr(out, left, indent);
            write!(out, " {} ", binop_str(*op)).unwrap();
            write_expr(out, right, indent);
        }
        Expr::Unary { op, operand } => {
            out.push_str(unop_str(*op));
            write_expr(out, operand, indent);
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
    }
}

fn write_node(out: &mut String, node: &Node, indent: usize) {
    let pad = "    ".repeat(indent + 1);
    let outer_pad = "    ".repeat(indent);
    match node {
        Node::Llm { kwargs } => {
            out.push_str("llm {\n");
            for (name, value) in kwargs {
                write!(out, "{pad}{}: ", name.name).unwrap();
                write_expr(out, value, indent + 1);
                out.push('\n');
            }
            write!(out, "{outer_pad}}}").unwrap();
        }
        Node::ToolCall { path, args } => {
            for (i, seg) in path.iter().enumerate() {
                if i > 0 {
                    out.push('.');
                }
                out.push_str(&seg.name);
            }
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
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
        Node::Fanout { items, collect } => {
            out.push_str("fanout [\n");
            for it in items {
                write!(out, "{pad}").unwrap();
                write_expr(out, it, indent + 1);
                out.push_str(",\n");
            }
            let mode = match collect {
                FanoutCollect::All => "all",
                FanoutCollect::First => "first",
            };
            write!(out, "{outer_pad}] collect: {mode}").unwrap();
        }
        Node::UserConfirm { msg } => {
            out.push_str("user_confirm(");
            write_expr(out, msg, indent);
            out.push(')');
        }
        Node::Subflow { name, args } => {
            write!(out, "subflow({}", name.name).unwrap();
            for a in args {
                out.push_str(", ");
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
