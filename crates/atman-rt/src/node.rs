//! Portable descriptions for executable VM statements.

use alloc::{
    format,
    string::{String, ToString},
    vec::Vec,
};

use serde::{Deserialize, Serialize};

use crate::ast::{Arg, BinOp, Expr, FlowRef, Literal, MessageRole, Node, Stmt};

/// Host-facing description of a VM statement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VmNode {
    pub kind: VmNodeKind,
    pub label: String,
}

impl VmNode {
    pub fn from_stmt(stmt: &Stmt) -> Self {
        match stmt {
            Stmt::Bind { value, .. } | Stmt::Expr(value) => Self::from_expr(value),
            Stmt::Return { .. } => Self::new(VmNodeKind::Return, "return"),
            Stmt::When { cond, .. } => {
                let condition_preview = format_expr_short(cond);
                Self::new(
                    VmNodeKind::When {
                        condition_preview: condition_preview.clone(),
                    },
                    format!("when {condition_preview}"),
                )
            }
            Stmt::Watch(_) => Self::new(VmNodeKind::Return, "watch"),
            Stmt::Loop { .. } => Self::new(VmNodeKind::Loop, "loop"),
            Stmt::Break => Self::new(VmNodeKind::Return, "break"),
            Stmt::Continue => Self::new(VmNodeKind::Return, "continue"),
            Stmt::Yield => Self::new(VmNodeKind::Yield, "yield"),
        }
    }

    pub fn from_expr(expr: &Expr) -> Self {
        if !matches!(expr, Expr::Await { .. } | Expr::Node(_))
            && let Some(effect) = first_effect_expr(expr)
        {
            return Self::from_expr(effect);
        }

        match expr {
            Expr::Await { value } => match value.as_ref() {
                Expr::Node(Node::FlowCall { name, .. }) => Self::new(
                    VmNodeKind::Subflow {
                        name: name.display_name(),
                    },
                    format_expr_short(expr),
                ),
                _ => Self::new(
                    VmNodeKind::FlowAwait {
                        target: format_expr_short(value),
                    },
                    format_expr_short(expr),
                ),
            },
            Expr::Node(Node::ToolCall { path, args })
                if path.len() == 2 && path[0].name == "llm" && path[1].name == "call" =>
            {
                Self::new(
                    VmNodeKind::Llm {
                        model: literal_named_string(args, "model"),
                    },
                    "llm.call",
                )
            }
            Expr::Node(Node::ToolCall { path, .. }) => {
                let path = path
                    .iter()
                    .map(|part| part.name.clone())
                    .collect::<Vec<_>>()
                    .join(".");
                Self::new(
                    VmNodeKind::ToolCall { path: path.clone() },
                    format!("⟶ {path}"),
                )
            }
            Expr::Node(Node::Fanout { source }) => {
                let label = match source.as_ref() {
                    Expr::List(items) => format!("fanout ×{}", items.len()),
                    _ => "fanout".into(),
                };
                Self::new(VmNodeKind::Fanout, label)
            }
            Expr::Node(Node::DynamicFanout { .. }) => {
                Self::new(VmNodeKind::Fanout, "fanout (dynamic)")
            }
            Expr::Node(Node::UserConfirm { .. }) => {
                Self::new(VmNodeKind::UserConfirm, "user_confirm")
            }
            Expr::Node(Node::FlowCall { name, args }) => Self::new(
                VmNodeKind::FlowFuture {
                    name: name.display_name(),
                },
                flow_call_label(name, args),
            ),
            Expr::Node(Node::Message { role, .. }) => {
                let role = message_role_name(*role);
                Self::new(
                    VmNodeKind::Message { role: role.into() },
                    format!("{role}_msg"),
                )
            }
            Expr::Node(Node::FixUntilTestPasses { .. }) => {
                Self::new(VmNodeKind::FixUntilTest, "fix_until_test")
            }
            _ => Self::new(VmNodeKind::Return, "expr"),
        }
    }

    fn new(kind: VmNodeKind, label: impl Into<String>) -> Self {
        Self {
            kind,
            label: label.into(),
        }
    }
}

impl From<&Stmt> for VmNode {
    fn from(stmt: &Stmt) -> Self {
        Self::from_stmt(stmt)
    }
}

impl From<&Expr> for VmNode {
    fn from(expr: &Expr) -> Self {
        Self::from_expr(expr)
    }
}

/// Stable node categories exposed to VM hosts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum VmNodeKind {
    Llm { model: Option<String> },
    ToolCall { path: String },
    Fanout,
    UserConfirm,
    FlowFuture { name: String },
    FlowAwait { target: String },
    Subflow { name: String },
    Message { role: String },
    FixUntilTest,
    When { condition_preview: String },
    Loop,
    Yield,
    Return,
}

/// Formats the compact expression labels used in VM node events.
pub fn format_expr_short(expr: &Expr) -> String {
    match expr {
        Expr::Await { value } => format!("{}.await", format_expr_short(value)),
        Expr::Index { base, index } => {
            format!("{}[{}]", format_expr_short(base), format_expr_short(index))
        }
        Expr::Node(Node::FlowCall { name, args }) => flow_call_label(name, args),
        Expr::Literal(Literal::Bool(value)) => value.to_string(),
        Expr::Literal(Literal::Str(value)) => format!("\"{value}\""),
        Expr::Literal(Literal::Int(value)) => value.to_string(),
        Expr::Literal(Literal::Float(value)) => value.to_string(),
        Expr::Ident(ident) => ident.name.clone(),
        Expr::Member { base, field } => format!("{}.{}", format_expr_short(base), field.name),
        Expr::Binary { op, left, right } => format!(
            "{} {} {}",
            format_expr_short(left),
            binary_symbol(*op),
            format_expr_short(right)
        ),
        _ => "…".into(),
    }
}

fn first_effect_expr(expr: &Expr) -> Option<&Expr> {
    match expr {
        Expr::Node(_) | Expr::Await { .. } => Some(expr),
        Expr::Member { base, .. }
        | Expr::Unary { operand: base, .. }
        | Expr::Annotated { expr: base, .. } => first_effect_expr(base),
        Expr::Index { base, index }
        | Expr::Binary {
            left: base,
            right: index,
            ..
        } => first_effect_expr(base).or_else(|| first_effect_expr(index)),
        Expr::Call { args, .. } | Expr::List(args) => args.iter().find_map(first_effect_expr),
        Expr::Struct(fields) => fields
            .iter()
            .find_map(|(_, value)| first_effect_expr(value)),
        Expr::Literal(_) | Expr::Ident(_) | Expr::FileRef(_) | Expr::Lambda { .. } => None,
    }
}

fn literal_named_string(args: &[Arg], key: &str) -> Option<String> {
    args.iter().find_map(|arg| match arg {
        Arg::Named {
            name,
            value: Expr::Literal(Literal::Str(value)),
        } if name.name == key => Some(value.clone()),
        _ => None,
    })
}

fn flow_call_label(name: &FlowRef, args: &[Arg]) -> String {
    format!(
        "{}({})",
        name.display_name(),
        if args.is_empty() { "" } else { "…" }
    )
}

fn message_role_name(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Assistant => "assistant",
        MessageRole::System => "system",
        MessageRole::Tool => "tool",
    }
}

fn binary_symbol(op: BinOp) -> &'static str {
    match op {
        BinOp::Eq => "==",
        BinOp::Ne => "!=",
        BinOp::Lt => "<",
        BinOp::Le => "<=",
        BinOp::Gt => ">",
        BinOp::Ge => ">=",
        BinOp::And => "and",
        BinOp::Or => "or",
        BinOp::Add => "+",
        BinOp::Sub => "-",
        BinOp::Mul => "*",
        BinOp::Div => "/",
        BinOp::Mod => "%",
    }
}

#[cfg(test)]
mod tests {
    use alloc::{boxed::Box, vec};

    use super::*;
    use crate::ast::{Ident, Span};

    fn ident(name: &str) -> Ident {
        Ident::new(name, Span::default())
    }

    fn flow_call(name: &str) -> Expr {
        Expr::Node(Node::FlowCall {
            name: FlowRef::Local(ident(name)),
            args: vec![],
        })
    }

    #[test]
    fn describes_flow_execution_modes() {
        let cold = flow_call("worker");
        assert_eq!(
            VmNode::from_expr(&cold),
            VmNode::new(
                VmNodeKind::FlowFuture {
                    name: "worker".into()
                },
                "worker()"
            )
        );

        let awaited = Expr::Await {
            value: Box::new(cold),
        };
        assert_eq!(
            VmNode::from_expr(&awaited),
            VmNode::new(
                VmNodeKind::Subflow {
                    name: "worker".into()
                },
                "worker().await"
            )
        );

        let handle = Expr::Await {
            value: Box::new(Expr::Ident(ident("pending"))),
        };
        assert_eq!(
            VmNode::from_expr(&handle),
            VmNode::new(
                VmNodeKind::FlowAwait {
                    target: "pending".into()
                },
                "pending.await"
            )
        );
    }

    #[test]
    fn describes_tools_fanout_and_builtin_nodes() {
        let llm = Expr::Node(Node::ToolCall {
            path: vec![ident("llm"), ident("call")],
            args: vec![Arg::Named {
                name: ident("model"),
                value: Expr::Literal(Literal::Str("reasoner".into())),
            }],
        });
        assert_eq!(
            VmNode::from_expr(&llm).kind,
            VmNodeKind::Llm {
                model: Some("reasoner".into())
            }
        );

        let tool = Expr::Node(Node::ToolCall {
            path: vec![ident("fs"), ident("read")],
            args: vec![],
        });
        assert_eq!(
            VmNode::from_expr(&tool),
            VmNode::new(
                VmNodeKind::ToolCall {
                    path: "fs.read".into()
                },
                "⟶ fs.read"
            )
        );

        let fanout = Expr::Node(Node::Fanout {
            source: Box::new(Expr::List(vec![flow_call("a"), flow_call("b")])),
        });
        assert_eq!(VmNode::from_expr(&fanout).label, "fanout ×2");

        let dynamic = Expr::Node(Node::DynamicFanout {
            source: Box::new(Expr::Ident(ident("items"))),
            lambda: Box::new(Expr::Lambda {
                params: vec![ident("item")],
                body: Box::new(flow_call("worker")),
            }),
        });
        assert_eq!(
            VmNode::from_expr(&dynamic),
            VmNode::new(VmNodeKind::Fanout, "fanout (dynamic)")
        );

        let message = Expr::Node(Node::Message {
            role: MessageRole::Assistant,
            args: vec![],
        });
        assert_eq!(
            VmNode::from_expr(&message),
            VmNode::new(
                VmNodeKind::Message {
                    role: "assistant".into()
                },
                "assistant_msg"
            )
        );

        let confirmation = Expr::Node(Node::UserConfirm {
            msg: Box::new(Expr::Literal(Literal::Str("continue?".into()))),
        });
        assert_eq!(
            VmNode::from_expr(&confirmation),
            VmNode::new(VmNodeKind::UserConfirm, "user_confirm")
        );

        let fix = Expr::Node(Node::FixUntilTestPasses { kwargs: vec![] });
        assert_eq!(
            VmNode::from_expr(&fix),
            VmNode::new(VmNodeKind::FixUntilTest, "fix_until_test")
        );
    }

    #[test]
    fn nested_expressions_use_the_first_effect() {
        let tool = Expr::Node(Node::ToolCall {
            path: vec![ident("cursor"), ident("next")],
            args: vec![],
        });
        let nested = Expr::Binary {
            op: BinOp::Add,
            left: Box::new(Expr::Index {
                base: Box::new(Expr::Member {
                    base: Box::new(tool),
                    field: ident("items"),
                }),
                index: Box::new(Expr::Literal(Literal::Int(0))),
            }),
            right: Box::new(flow_call("later")),
        };

        assert_eq!(
            VmNode::from_expr(&nested),
            VmNode::new(
                VmNodeKind::ToolCall {
                    path: "cursor.next".into()
                },
                "⟶ cursor.next"
            )
        );
    }

    #[test]
    fn describes_control_statements() {
        let when = Stmt::When {
            cond: Expr::Binary {
                op: BinOp::Ge,
                left: Box::new(Expr::Ident(ident("attempt"))),
                right: Box::new(Expr::Literal(Literal::Int(2))),
            },
            body: vec![],
        };
        assert_eq!(
            VmNode::from_stmt(&when),
            VmNode::new(
                VmNodeKind::When {
                    condition_preview: "attempt >= 2".into()
                },
                "when attempt >= 2"
            )
        );
        assert_eq!(
            VmNode::from_stmt(&Stmt::Loop { body: vec![] }),
            VmNode::new(VmNodeKind::Loop, "loop")
        );
        assert_eq!(
            VmNode::from_stmt(&Stmt::Return {
                value: flow_call("hidden")
            }),
            VmNode::new(VmNodeKind::Return, "return")
        );
        assert_eq!(
            VmNode::from_stmt(&Stmt::Yield),
            VmNode::new(VmNodeKind::Yield, "yield")
        );
    }
}
