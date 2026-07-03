use syn::parse::{Parse, ParseStream};
use syn::{
    LitBool, LitFloat, LitInt, LitStr, Result, Token, braced, bracketed, parenthesized, token,
};

use crate::ast::*;

mod kw {
    syn::custom_keyword!(flow);
    syn::custom_keyword!(when);
    syn::custom_keyword!(llm);
    syn::custom_keyword!(fanout);
    syn::custom_keyword!(collect);
    syn::custom_keyword!(all);
    syn::custom_keyword!(first);
    syn::custom_keyword!(user_confirm);
    syn::custom_keyword!(contract);
    syn::custom_keyword!(subflow);
    syn::custom_keyword!(user_msg);
    syn::custom_keyword!(assistant_msg);
    syn::custom_keyword!(system_msg);
    syn::custom_keyword!(tool_result);
    syn::custom_keyword!(watch);
    syn::custom_keyword!(on);
    syn::custom_keyword!(token);
    syn::custom_keyword!(elapsed);
    syn::custom_keyword!(tokens_consumed);

    syn::custom_keyword!(abort);
    syn::custom_keyword!(warn);
    syn::custom_keyword!(ms);
    syn::custom_keyword!(s);
}

fn to_ident(id: syn::Ident) -> Ident {
    Ident {
        name: id.to_string(),
        span: id.span(),
    }
}

impl Parse for File {
    fn parse(input: ParseStream) -> Result<Self> {
        let mut flows = Vec::new();
        while !input.is_empty() {
            if input.peek(kw::flow) {
                flows.push(input.parse::<FlowDecl>()?);
            } else {
                return Err(input.error("expected `flow` declaration at top level"));
            }
        }
        Ok(File { flows })
    }
}

impl Parse for FlowDecl {
    fn parse(input: ParseStream) -> Result<Self> {
        input.parse::<kw::flow>()?;
        let name = to_ident(input.parse::<syn::Ident>()?);

        let params_content;
        parenthesized!(params_content in input);
        let params = parse_params(&params_content)?;

        let ret = if input.peek(Token![->]) {
            input.parse::<Token![->]>()?;
            Some(parse_type(input)?)
        } else {
            None
        };

        let body_content;
        braced!(body_content in input);

        let contract = if body_content.peek(kw::contract) {
            Some(parse_contract(&body_content)?)
        } else {
            None
        };

        let body = parse_stmts(&body_content)?;

        Ok(FlowDecl {
            name,
            params,
            ret,
            contract,
            body,
        })
    }
}

fn parse_contract(input: ParseStream) -> Result<Contract> {
    input.parse::<kw::contract>()?;
    let content;
    braced!(content in input);
    let mut blocks = Vec::new();
    while !content.is_empty() {
        let name = to_ident(content.parse::<syn::Ident>()?);
        let kwargs_content;
        braced!(kwargs_content in content);
        let mut kwargs = Vec::new();
        while !kwargs_content.is_empty() {
            let k = to_ident(kwargs_content.parse::<syn::Ident>()?);
            kwargs_content.parse::<Token![:]>()?;
            let v = parse_expr(&kwargs_content)?;
            kwargs.push((k, v));
            if kwargs_content.peek(Token![,]) {
                kwargs_content.parse::<Token![,]>()?;
            }
        }
        blocks.push(ContractBlock { name, kwargs });
    }
    Ok(Contract { blocks })
}

fn parse_params(input: ParseStream) -> Result<Vec<(Ident, TypeExpr)>> {
    let mut params = Vec::new();
    while !input.is_empty() {
        let name = to_ident(input.parse::<syn::Ident>()?);
        input.parse::<Token![:]>()?;
        let ty = parse_type(input)?;
        params.push((name, ty));
        if input.is_empty() {
            break;
        }
        input.parse::<Token![,]>()?;
    }
    Ok(params)
}

fn parse_type(input: ParseStream) -> Result<TypeExpr> {
    if input.peek(token::Bracket) {
        let content;
        bracketed!(content in input);
        let inner = parse_type(&content)?;
        return Ok(TypeExpr::List(Box::new(inner)));
    }
    if input.peek(token::Brace) {
        let content;
        braced!(content in input);
        let mut fields = Vec::new();
        while !content.is_empty() {
            let name = to_ident(content.parse::<syn::Ident>()?);
            content.parse::<Token![:]>()?;
            let ty = parse_type(&content)?;
            fields.push((name, ty));
            if content.is_empty() {
                break;
            }
            content.parse::<Token![,]>()?;
        }
        return Ok(TypeExpr::Struct(fields));
    }
    let id = input.parse::<syn::Ident>()?;
    Ok(TypeExpr::Named(to_ident(id)))
}

fn parse_stmts(input: ParseStream) -> Result<Vec<Stmt>> {
    let mut stmts = Vec::new();
    while !input.is_empty() {
        stmts.push(parse_stmt(input)?);
        if input.peek(Token![,]) {
            input.parse::<Token![,]>()?;
        } else if input.peek(Token![;]) {
            input.parse::<Token![;]>()?;
        }
    }
    Ok(stmts)
}

fn parse_stmt(input: ParseStream) -> Result<Stmt> {
    if input.peek(Token![return]) {
        input.parse::<Token![return]>()?;
        let value = parse_expr(input)?;
        return Ok(Stmt::Return { value });
    }
    if input.peek(kw::watch) {
        return Ok(Stmt::Watch(parse_watch(input)?));
    }
    if input.peek(kw::when) {
        input.parse::<kw::when>()?;
        let cond = parse_expr(input)?;
        let content;
        braced!(content in input);
        let body = parse_stmts(&content)?;
        return Ok(Stmt::When { cond, body });
    }
    if input.peek(syn::Ident) && input.peek2(Token![=]) {
        let name = to_ident(input.parse::<syn::Ident>()?);
        input.parse::<Token![=]>()?;
        let value = parse_expr(input)?;
        return Ok(Stmt::Bind { name, value });
    }
    let expr = parse_expr(input)?;
    Ok(Stmt::Expr(expr))
}

fn parse_expr(input: ParseStream) -> Result<Expr> {
    parse_expr_bp(input, 0)
}

fn parse_expr_bp(input: ParseStream, min_bp: u8) -> Result<Expr> {
    let mut lhs = parse_expr_primary(input)?;
    loop {
        let op = peek_binop(input);
        let Some((op, bp)) = op else { break };
        if bp < min_bp {
            break;
        }
        consume_binop(input, op)?;
        let rhs = parse_expr_bp(input, bp + 1)?;
        lhs = Expr::Binary {
            op,
            left: Box::new(lhs),
            right: Box::new(rhs),
        };
    }
    Ok(lhs)
}

fn peek_binop(input: ParseStream) -> Option<(BinOp, u8)> {
    if input.peek(Token![||]) {
        Some((BinOp::Or, 1))
    } else if input.peek(Token![&&]) {
        Some((BinOp::And, 2))
    } else if input.peek(Token![==]) {
        Some((BinOp::Eq, 3))
    } else if input.peek(Token![!=]) {
        Some((BinOp::Ne, 3))
    } else if input.peek(Token![<=]) {
        Some((BinOp::Le, 3))
    } else if input.peek(Token![>=]) {
        Some((BinOp::Ge, 3))
    } else if input.peek(Token![<]) {
        Some((BinOp::Lt, 3))
    } else if input.peek(Token![>]) {
        Some((BinOp::Gt, 3))
    } else if input.peek(Token![+]) {
        Some((BinOp::Add, 4))
    } else {
        None
    }
}

fn consume_binop(input: ParseStream, op: BinOp) -> Result<()> {
    match op {
        BinOp::Or => {
            input.parse::<Token![||]>()?;
        }
        BinOp::And => {
            input.parse::<Token![&&]>()?;
        }
        BinOp::Eq => {
            input.parse::<Token![==]>()?;
        }
        BinOp::Ne => {
            input.parse::<Token![!=]>()?;
        }
        BinOp::Le => {
            input.parse::<Token![<=]>()?;
        }
        BinOp::Ge => {
            input.parse::<Token![>=]>()?;
        }
        BinOp::Lt => {
            input.parse::<Token![<]>()?;
        }
        BinOp::Gt => {
            input.parse::<Token![>]>()?;
        }
        BinOp::Add => {
            input.parse::<Token![+]>()?;
        }
    }
    Ok(())
}

fn parse_expr_primary(input: ParseStream) -> Result<Expr> {
    if input.peek(Token![@]) {
        input.parse::<Token![@]>()?;
        let s: LitStr = input.parse()?;
        return Ok(Expr::FileRef(FileRef { path: s.value() }));
    }

    if input.peek(kw::llm) {
        return Ok(Expr::Node(parse_llm(input)?));
    }
    if input.peek(kw::fanout) {
        return Ok(Expr::Node(parse_fanout(input)?));
    }
    if input.peek(kw::user_confirm) {
        return Ok(Expr::Node(parse_user_confirm(input)?));
    }
    if input.peek(kw::subflow) {
        return Ok(Expr::Node(parse_subflow(input)?));
    }
    if input.peek(kw::user_msg) {
        input.parse::<kw::user_msg>()?;
        return Ok(Expr::Node(parse_message_args(input, MessageRole::User)?));
    }
    if input.peek(kw::assistant_msg) {
        input.parse::<kw::assistant_msg>()?;
        return Ok(Expr::Node(parse_message_args(
            input,
            MessageRole::Assistant,
        )?));
    }
    if input.peek(kw::system_msg) {
        input.parse::<kw::system_msg>()?;
        return Ok(Expr::Node(parse_message_args(input, MessageRole::System)?));
    }
    if input.peek(kw::tool_result) {
        input.parse::<kw::tool_result>()?;
        return Ok(Expr::Node(parse_message_args(input, MessageRole::Tool)?));
    }

    if input.peek(LitStr) {
        let s: LitStr = input.parse()?;
        return Ok(Expr::Literal(Literal::Str(s.value())));
    }
    if input.peek(LitBool) {
        let b: LitBool = input.parse()?;
        return Ok(Expr::Literal(Literal::Bool(b.value)));
    }
    if input.peek(LitInt) {
        let n: LitInt = input.parse()?;
        let v: i64 = n.base10_parse()?;
        return Ok(Expr::Literal(Literal::Int(v)));
    }
    if input.peek(LitFloat) {
        let n: LitFloat = input.parse()?;
        let v: f64 = n.base10_parse()?;
        return Ok(Expr::Literal(Literal::Float(v)));
    }

    if input.peek(token::Brace) {
        return parse_struct_literal(input);
    }

    if input.peek(token::Bracket) {
        let content;
        bracketed!(content in input);
        let mut items = Vec::new();
        while !content.is_empty() {
            items.push(parse_expr(&content)?);
            if content.is_empty() {
                break;
            }
            content.parse::<Token![,]>()?;
        }
        return Ok(Expr::List(items));
    }

    if input.peek(syn::Ident) {
        return parse_ident_expr(input);
    }

    Err(input.error("expected expression"))
}

fn parse_struct_literal(input: ParseStream) -> Result<Expr> {
    let content;
    braced!(content in input);
    let mut fields = Vec::new();
    while !content.is_empty() {
        let name = to_ident(content.parse::<syn::Ident>()?);
        content.parse::<Token![:]>()?;
        let value = parse_expr(&content)?;
        fields.push((name, value));
        if content.is_empty() {
            break;
        }
        content.parse::<Token![,]>()?;
    }
    Ok(Expr::Struct(fields))
}

fn parse_ident_expr(input: ParseStream) -> Result<Expr> {
    let first = to_ident(input.parse::<syn::Ident>()?);

    if input.peek(Token![.]) {
        let mut path = vec![first];
        while input.peek(Token![.]) {
            input.parse::<Token![.]>()?;
            let seg = to_ident(input.parse::<syn::Ident>()?);
            path.push(seg);
        }
        if input.peek(token::Paren) {
            let content;
            parenthesized!(content in input);
            let args = parse_call_args(&content)?;
            return Ok(Expr::Node(Node::ToolCall { path, args }));
        }
        let mut iter = path.into_iter();
        let base_ident = iter.next().unwrap();
        let mut expr = Expr::Ident(base_ident);
        for field in iter {
            expr = Expr::Member {
                base: Box::new(expr),
                field,
            };
        }
        return Ok(expr);
    }

    if input.peek(token::Paren) {
        let content;
        parenthesized!(content in input);
        let args = parse_call_args(&content)?;
        return Ok(Expr::Node(Node::ToolCall {
            path: vec![first],
            args,
        }));
    }

    Ok(Expr::Ident(first))
}

fn parse_call_args(input: ParseStream) -> Result<Vec<Arg>> {
    let mut args = Vec::new();
    while !input.is_empty() {
        if (input.peek(syn::Ident) || peek_any_ident(input)) && input.peek2(Token![:]) {
            let name = to_ident(<syn::Ident as syn::ext::IdentExt>::parse_any(input)?);
            input.parse::<Token![:]>()?;
            let value = parse_expr(input)?;
            args.push(Arg::Named { name, value });
        } else {
            args.push(Arg::Positional(parse_expr(input)?));
        }
        if input.is_empty() {
            break;
        }
        input.parse::<Token![,]>()?;
    }
    Ok(args)
}

fn peek_any_ident(input: ParseStream) -> bool {
    input
        .fork()
        .call(<syn::Ident as syn::ext::IdentExt>::parse_any)
        .is_ok()
}

fn parse_watch(input: ParseStream) -> Result<WatchDecl> {
    input.parse::<kw::watch>()?;
    let target = to_ident(input.parse::<syn::Ident>()?);
    let content;
    braced!(content in input);
    let mut on_blocks = Vec::new();
    while !content.is_empty() {
        on_blocks.push(parse_on_block(&content)?);
    }
    Ok(WatchDecl { target, on_blocks })
}

fn parse_on_block(input: ParseStream) -> Result<OnBlock> {
    input.parse::<kw::on>()?;
    let event = parse_watch_event(input)?;
    let body;
    braced!(body in input);
    let mut actions = Vec::new();
    while !body.is_empty() {
        actions.push(parse_watch_action(&body)?);
    }
    Ok(OnBlock { event, actions })
}

fn parse_watch_event(input: ParseStream) -> Result<WatchEvent> {
    if input.peek(kw::token) {
        input.parse::<kw::token>()?;
        let inner;
        parenthesized!(inner in input);
        let label = <syn::Ident as syn::ext::IdentExt>::parse_any(&inner)?;
        if label != "match" {
            return Err(syn::Error::new(label.span(), "expected `match:`"));
        }
        inner.parse::<Token![:]>()?;
        let mut patterns = Vec::new();
        patterns.push(inner.parse::<LitStr>()?.value());
        while inner.peek(Token![|]) {
            inner.parse::<Token![|]>()?;
            patterns.push(inner.parse::<LitStr>()?.value());
        }
        return Ok(WatchEvent::Token { patterns });
    }
    if input.peek(kw::elapsed) {
        input.parse::<kw::elapsed>()?;
        let inner;
        parenthesized!(inner in input);
        let cmp = parse_cmp(&inner)?;
        let n = inner.parse::<LitInt>()?.base10_parse::<u64>()?;
        let ms = if inner.peek(kw::ms) {
            inner.parse::<kw::ms>()?;
            n
        } else if inner.peek(kw::s) {
            inner.parse::<kw::s>()?;
            n.saturating_mul(1000)
        } else {
            return Err(inner.error("expected `ms` or `s` unit"));
        };
        return Ok(WatchEvent::Elapsed {
            cmp,
            duration_ms: ms,
        });
    }
    if input.peek(kw::tokens_consumed) {
        input.parse::<kw::tokens_consumed>()?;
        let inner;
        parenthesized!(inner in input);
        let cmp = parse_cmp(&inner)?;
        let value = inner.parse::<LitInt>()?.base10_parse::<u64>()?;
        return Ok(WatchEvent::TokensConsumed { cmp, value });
    }
    Err(input.error("expected `token`, `elapsed`, or `tokens_consumed`"))
}

fn parse_cmp(input: ParseStream) -> Result<CmpOp> {
    if input.peek(Token![>=]) {
        input.parse::<Token![>=]>()?;
        Ok(CmpOp::Ge)
    } else if input.peek(Token![>]) {
        input.parse::<Token![>]>()?;
        Ok(CmpOp::Gt)
    } else if input.peek(Token![<=]) {
        input.parse::<Token![<=]>()?;
        Ok(CmpOp::Le)
    } else if input.peek(Token![<]) {
        input.parse::<Token![<]>()?;
        Ok(CmpOp::Lt)
    } else {
        Err(input.error("expected `>`, `>=`, `<`, or `<=`"))
    }
}

fn parse_watch_action(input: ParseStream) -> Result<WatchAction> {
    let kind = if input.peek(kw::abort) {
        input.parse::<kw::abort>()?;
        "abort"
    } else if input.peek(kw::warn) {
        input.parse::<kw::warn>()?;
        "warn"
    } else {
        return Err(input.error("expected `abort` or `warn`"));
    };
    let inner;
    parenthesized!(inner in input);
    let msg = if inner.is_empty() {
        None
    } else {
        Some(parse_expr(&inner)?)
    };
    Ok(match kind {
        "abort" => WatchAction::Abort { msg },
        "warn" => WatchAction::Warn { msg },
        _ => unreachable!(),
    })
}

fn parse_kwargs(input: ParseStream) -> Result<Kwargs> {
    let content;
    braced!(content in input);
    let mut kwargs = Vec::new();
    while !content.is_empty() {
        let name = to_ident(content.parse::<syn::Ident>()?);
        content.parse::<Token![:]>()?;
        let value = parse_expr(&content)?;
        kwargs.push((name, value));
        if content.peek(Token![,]) {
            content.parse::<Token![,]>()?;
        }
    }
    Ok(kwargs)
}

fn parse_llm(input: ParseStream) -> Result<Node> {
    input.parse::<kw::llm>()?;
    let kwargs = parse_kwargs(input)?;
    Ok(Node::Llm { kwargs })
}

fn parse_subflow(input: ParseStream) -> Result<Node> {
    input.parse::<kw::subflow>()?;
    let content;
    parenthesized!(content in input);
    let name = to_ident(content.parse::<syn::Ident>()?);
    let args = if content.peek(Token![,]) {
        content.parse::<Token![,]>()?;
        parse_call_args(&content)?
    } else if content.is_empty() {
        Vec::new()
    } else {
        return Err(content.error("expected `,` after subflow name"));
    };
    Ok(Node::Subflow { name, args })
}

fn parse_user_confirm(input: ParseStream) -> Result<Node> {
    input.parse::<kw::user_confirm>()?;
    let content;
    parenthesized!(content in input);
    let msg = parse_expr(&content)?;
    Ok(Node::UserConfirm { msg: Box::new(msg) })
}

fn parse_message_args(input: ParseStream, role: MessageRole) -> Result<Node> {
    let content;
    parenthesized!(content in input);
    let args = parse_call_args(&content)?;
    Ok(Node::Message { role, args })
}

fn parse_fanout(input: ParseStream) -> Result<Node> {
    input.parse::<kw::fanout>()?;
    let content;
    bracketed!(content in input);
    let mut items = Vec::new();
    while !content.is_empty() {
        items.push(parse_expr(&content)?);
        if content.is_empty() {
            break;
        }
        content.parse::<Token![,]>()?;
    }
    input.parse::<kw::collect>()?;
    input.parse::<Token![:]>()?;
    let collect = if input.peek(kw::all) {
        input.parse::<kw::all>()?;
        FanoutCollect::All
    } else if input.peek(kw::first) {
        input.parse::<kw::first>()?;
        FanoutCollect::First
    } else {
        return Err(input.error("expected `all` or `first` after `collect:`"));
    };
    Ok(Node::Fanout { items, collect })
}

pub fn parse_file(src: &str) -> Result<File> {
    syn::parse_str::<File>(src)
}
