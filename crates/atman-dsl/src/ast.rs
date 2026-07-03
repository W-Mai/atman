use proc_macro2::Span;

#[derive(Debug, Clone)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

impl Ident {
    pub fn new(name: impl Into<String>, span: Span) -> Self {
        Self {
            name: name.into(),
            span,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Literal {
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

#[derive(Debug, Clone)]
pub struct FileRef {
    pub path: String,
}

#[derive(Debug, Clone)]
pub enum TypeExpr {
    Named(Ident),
    List(Box<TypeExpr>),
    Struct(Vec<(Ident, TypeExpr)>),
}

#[derive(Debug, Clone)]
pub enum Expr {
    Literal(Literal),
    Ident(Ident),
    FileRef(FileRef),
    Member {
        base: Box<Expr>,
        field: Ident,
    },
    Binary {
        op: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Call {
        func: Ident,
        args: Vec<Expr>,
    },
    Struct(Vec<(Ident, Expr)>),
    List(Vec<Expr>),
    Node(Node),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    Add,
}

#[derive(Debug, Clone)]
pub enum Node {
    Llm {
        kwargs: Kwargs,
    },
    ToolCall {
        path: Vec<Ident>,
        args: Vec<Arg>,
    },
    Fanout {
        items: Vec<Expr>,
        collect: FanoutCollect,
    },
    UserConfirm {
        msg: Box<Expr>,
    },
    Subflow {
        name: Ident,
        args: Vec<Arg>,
    },
}

#[derive(Debug, Clone)]
pub enum Arg {
    Positional(Expr),
    Named { name: Ident, value: Expr },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FanoutCollect {
    All,
    First,
}

pub type Kwargs = Vec<(Ident, Expr)>;

#[derive(Debug, Clone)]
pub enum Stmt {
    Bind { name: Ident, value: Expr },
    When { cond: Expr, body: Vec<Stmt> },
    Return { value: Expr },
    Expr(Expr),
    Watch(WatchDecl),
}

#[derive(Debug, Clone)]
pub struct WatchDecl {
    pub target: Ident,
    pub on_blocks: Vec<OnBlock>,
}

#[derive(Debug, Clone)]
pub struct OnBlock {
    pub event: WatchEvent,
    pub actions: Vec<WatchAction>,
}

#[derive(Debug, Clone)]
pub enum WatchEvent {
    Token { patterns: Vec<String> },
    Elapsed { cmp: CmpOp, duration_ms: u64 },
    TokensConsumed { cmp: CmpOp, value: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Gt,
    Ge,
    Lt,
    Le,
}

#[derive(Debug, Clone)]
pub enum WatchAction {
    Abort { msg: Option<Expr> },
    Warn { msg: Option<Expr> },
}

#[derive(Debug, Clone)]
pub struct FlowDecl {
    pub name: Ident,
    pub params: Vec<(Ident, TypeExpr)>,
    pub ret: Option<TypeExpr>,
    pub contract: Option<Contract>,
    pub body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
pub struct Contract {
    pub blocks: Vec<ContractBlock>,
}

#[derive(Debug, Clone)]
pub struct ContractBlock {
    pub name: Ident,
    pub kwargs: Kwargs,
}

#[derive(Debug, Clone)]
pub struct File {
    pub flows: Vec<FlowDecl>,
}
