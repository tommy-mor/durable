//! hopc v0 — compiles `.hop` source to Lua targeting the hoprt runtime.
//!
//! The v0 language is the smallest thing that exercises every runtime
//! semantic:
//!
//! ```text
//! item  := "server" "let" name "=" expr ";"        server-side global
//!        | "fn" name "(" params ")" block
//! stmt  := "let" name "=" expr ";"
//!        | "server" "!" "(" ")" ";"                placement mark
//!        | "browser" "!" "(" ")" ";"               placement mark (→ origin)
//!        | "cast" ("browsers"|"server"|"session" "(" expr ")") block
//!        | "spawn" call ";"                        start a flow (an "event")
//!        | "return" expr? ";"
//!        | "if" expr block ("else" (block|if))?
//!        | lvalue "=" expr ";"                     assignment
//!        | expr ";"
//! ```
//!
//! Compilation of a marked function: the body splits into segments at the
//! marks; segment 0 becomes the origin function; each later segment is
//! registered under a stable hop id (`name:i`) and chained via
//! `rt.at(target, "name:i", { live vars })`. What crosses each hop is
//! computed here, statically: the variables referenced by the remainder
//! that are in scope before the mark. Cast bodies compile the same way
//! under `name:cN` ids with their own captured-vars set.
//!
//! v0 restrictions (deliberate): marks only at the top level of a function
//! body; flows originate on the browser; no while/for; no try/catch
//! (errors still propagate — they surface at the flow origin).

use std::collections::BTreeSet;
use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Num(String),
    Str(String),
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    Comma,
    Semi,
    Bang,
    Dot,
    DotDot,
    Assign,
    Eq,
    Ne,
    Lt,
    Gt,
    Le,
    Ge,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    AndAnd,
    OrOr,
}

fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let cs: Vec<char> = src.chars().collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < cs.len() {
        let c = cs[i];
        let two = |ch: char| i + 1 < cs.len() && cs[i + 1] == ch;
        match c {
            ' ' | '\t' | '\r' | '\n' => i += 1,
            '/' if two('/') => {
                while i < cs.len() && cs[i] != '\n' {
                    i += 1;
                }
            }
            '(' => { out.push(Tok::LParen); i += 1 }
            ')' => { out.push(Tok::RParen); i += 1 }
            '{' => { out.push(Tok::LBrace); i += 1 }
            '}' => { out.push(Tok::RBrace); i += 1 }
            '[' => { out.push(Tok::LBracket); i += 1 }
            ']' => { out.push(Tok::RBracket); i += 1 }
            ',' => { out.push(Tok::Comma); i += 1 }
            ';' => { out.push(Tok::Semi); i += 1 }
            '+' => { out.push(Tok::Plus); i += 1 }
            '-' => { out.push(Tok::Minus); i += 1 }
            '*' => { out.push(Tok::Star); i += 1 }
            '/' => { out.push(Tok::Slash); i += 1 }
            '%' => { out.push(Tok::Percent); i += 1 }
            '.' if two('.') => { out.push(Tok::DotDot); i += 2 }
            '.' => { out.push(Tok::Dot); i += 1 }
            '=' if two('=') => { out.push(Tok::Eq); i += 2 }
            '=' => { out.push(Tok::Assign); i += 1 }
            '!' if two('=') => { out.push(Tok::Ne); i += 2 }
            '!' => { out.push(Tok::Bang); i += 1 }
            '<' if two('=') => { out.push(Tok::Le); i += 2 }
            '<' => { out.push(Tok::Lt); i += 1 }
            '>' if two('=') => { out.push(Tok::Ge); i += 2 }
            '>' => { out.push(Tok::Gt); i += 1 }
            '&' if two('&') => { out.push(Tok::AndAnd); i += 2 }
            '|' if two('|') => { out.push(Tok::OrOr); i += 2 }
            '"' => {
                i += 1;
                let mut s = String::new();
                while i < cs.len() && cs[i] != '"' {
                    if cs[i] == '\\' && i + 1 < cs.len() {
                        i += 1;
                    }
                    s.push(cs[i]);
                    i += 1;
                }
                if i >= cs.len() {
                    return Err("unterminated string".into());
                }
                i += 1;
                out.push(Tok::Str(s));
            }
            _ if c.is_ascii_digit() => {
                let start = i;
                while i < cs.len() && (cs[i].is_ascii_digit() || cs[i] == '.') {
                    // don't swallow `..` (concat)
                    if cs[i] == '.' && i + 1 < cs.len() && cs[i + 1] == '.' {
                        break;
                    }
                    i += 1;
                }
                out.push(Tok::Num(cs[start..i].iter().collect()));
            }
            _ if c.is_ascii_alphabetic() || c == '_' => {
                let start = i;
                while i < cs.len() && (cs[i].is_ascii_alphanumeric() || cs[i] == '_') {
                    i += 1;
                }
                out.push(Tok::Ident(cs[start..i].iter().collect()));
            }
            _ => return Err(format!("unexpected character: {c:?}")),
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Expr {
    Num(String),
    Str(String),
    Bool(bool),
    Nil,
    Ident(String),
    Field(Box<Expr>, String),
    Index(Box<Expr>, Box<Expr>),
    Call(Box<Expr>, Vec<Expr>),
    Unary(&'static str, Box<Expr>),
    Binary(&'static str, Box<Expr>, Box<Expr>),
    Table(Vec<(String, Expr)>),
    Array(Vec<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Side {
    Server,
    Browser,
}

#[derive(Debug, Clone)]
enum CastTarget {
    Browsers,
    Server,
    Session(Expr),
}

#[derive(Debug, Clone)]
enum Stmt {
    Let(String, Expr),
    Assign(Expr, Expr),
    Return(Option<Expr>),
    If(Expr, Vec<Stmt>, Option<Vec<Stmt>>),
    Mark(Side),
    Cast(CastTarget, Vec<Stmt>),
    Spawn(Expr),
    Expr(Expr),
}

#[derive(Debug)]
enum Item {
    ServerLet(String, Expr),
    Fn(String, Vec<String>, Vec<Stmt>),
}

// ---------------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------------

struct Parser {
    toks: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos)
    }

    fn peek2(&self) -> Option<&Tok> {
        self.toks.get(self.pos + 1)
    }

    fn next(&mut self) -> Result<Tok, String> {
        let t = self.toks.get(self.pos).cloned().ok_or("unexpected end of input")?;
        self.pos += 1;
        Ok(t)
    }

    fn expect(&mut self, t: Tok) -> Result<(), String> {
        let got = self.next()?;
        if got == t {
            Ok(())
        } else {
            Err(format!("expected {t:?}, got {got:?}"))
        }
    }

    fn at_kw(&self, kw: &str) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if s == kw)
    }

    fn eat_kw(&mut self, kw: &str) -> Result<(), String> {
        if self.at_kw(kw) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected keyword '{kw}', got {:?}", self.peek()))
        }
    }

    fn ident(&mut self) -> Result<String, String> {
        match self.next()? {
            Tok::Ident(s) => Ok(s),
            t => Err(format!("expected identifier, got {t:?}")),
        }
    }

    fn items(&mut self) -> Result<Vec<Item>, String> {
        let mut out = Vec::new();
        while self.peek().is_some() {
            if self.at_kw("server") {
                self.eat_kw("server")?;
                self.eat_kw("let")?;
                let name = self.ident()?;
                self.expect(Tok::Assign)?;
                let e = self.expr()?;
                self.expect(Tok::Semi)?;
                out.push(Item::ServerLet(name, e));
            } else if self.at_kw("fn") {
                self.eat_kw("fn")?;
                let name = self.ident()?;
                self.expect(Tok::LParen)?;
                let mut params = Vec::new();
                while !matches!(self.peek(), Some(Tok::RParen)) {
                    params.push(self.ident()?);
                    if matches!(self.peek(), Some(Tok::Comma)) {
                        self.pos += 1;
                    }
                }
                self.expect(Tok::RParen)?;
                let body = self.block()?;
                out.push(Item::Fn(name, params, body));
            } else {
                return Err(format!("expected item, got {:?}", self.peek()));
            }
        }
        Ok(out)
    }

    fn block(&mut self) -> Result<Vec<Stmt>, String> {
        self.expect(Tok::LBrace)?;
        let mut out = Vec::new();
        while !matches!(self.peek(), Some(Tok::RBrace)) {
            out.push(self.stmt()?);
        }
        self.expect(Tok::RBrace)?;
        Ok(out)
    }

    fn stmt(&mut self) -> Result<Stmt, String> {
        // placement marks: `server!();` / `browser!();`
        if (self.at_kw("server") || self.at_kw("browser"))
            && matches!(self.peek2(), Some(Tok::Bang))
        {
            let side = if self.at_kw("server") { Side::Server } else { Side::Browser };
            self.pos += 2; // ident + !
            self.expect(Tok::LParen)?;
            self.expect(Tok::RParen)?;
            self.expect(Tok::Semi)?;
            return Ok(Stmt::Mark(side));
        }
        if self.at_kw("let") {
            self.eat_kw("let")?;
            let name = self.ident()?;
            self.expect(Tok::Assign)?;
            let e = self.expr()?;
            self.expect(Tok::Semi)?;
            return Ok(Stmt::Let(name, e));
        }
        if self.at_kw("return") {
            self.eat_kw("return")?;
            if matches!(self.peek(), Some(Tok::Semi)) {
                self.pos += 1;
                return Ok(Stmt::Return(None));
            }
            let e = self.expr()?;
            self.expect(Tok::Semi)?;
            return Ok(Stmt::Return(Some(e)));
        }
        if self.at_kw("if") {
            return self.if_stmt();
        }
        if self.at_kw("cast") {
            self.eat_kw("cast")?;
            let target = if self.at_kw("browsers") {
                self.pos += 1;
                CastTarget::Browsers
            } else if self.at_kw("server") {
                self.pos += 1;
                CastTarget::Server
            } else if self.at_kw("session") {
                self.pos += 1;
                self.expect(Tok::LParen)?;
                let e = self.expr()?;
                self.expect(Tok::RParen)?;
                CastTarget::Session(e)
            } else {
                return Err(format!("expected cast target, got {:?}", self.peek()));
            };
            let body = self.block()?;
            return Ok(Stmt::Cast(target, body));
        }
        if self.at_kw("spawn") {
            self.eat_kw("spawn")?;
            let e = self.expr()?;
            if !matches!(e, Expr::Call(..)) {
                return Err("spawn expects a function call".into());
            }
            self.expect(Tok::Semi)?;
            return Ok(Stmt::Spawn(e));
        }
        // expression statement or assignment
        let e = self.expr()?;
        if matches!(self.peek(), Some(Tok::Assign)) {
            self.pos += 1;
            let rhs = self.expr()?;
            self.expect(Tok::Semi)?;
            match e {
                Expr::Ident(_) | Expr::Field(..) | Expr::Index(..) => {
                    return Ok(Stmt::Assign(e, rhs))
                }
                _ => return Err("invalid assignment target".into()),
            }
        }
        self.expect(Tok::Semi)?;
        Ok(Stmt::Expr(e))
    }

    fn if_stmt(&mut self) -> Result<Stmt, String> {
        self.eat_kw("if")?;
        let cond = self.expr()?;
        let then = self.block()?;
        let els = if self.at_kw("else") {
            self.eat_kw("else")?;
            if self.at_kw("if") {
                Some(vec![self.if_stmt()?])
            } else {
                Some(self.block()?)
            }
        } else {
            None
        };
        Ok(Stmt::If(cond, then, els))
    }

    // precedence: || < && < cmp < .. < +- < */% < unary < postfix
    fn expr(&mut self) -> Result<Expr, String> {
        self.or_expr()
    }

    fn or_expr(&mut self) -> Result<Expr, String> {
        let mut lhs = self.and_expr()?;
        while matches!(self.peek(), Some(Tok::OrOr)) {
            self.pos += 1;
            let rhs = self.and_expr()?;
            lhs = Expr::Binary("or", Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn and_expr(&mut self) -> Result<Expr, String> {
        let mut lhs = self.cmp_expr()?;
        while matches!(self.peek(), Some(Tok::AndAnd)) {
            self.pos += 1;
            let rhs = self.cmp_expr()?;
            lhs = Expr::Binary("and", Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn cmp_expr(&mut self) -> Result<Expr, String> {
        let lhs = self.concat_expr()?;
        let op = match self.peek() {
            Some(Tok::Eq) => "==",
            Some(Tok::Ne) => "~=",
            Some(Tok::Lt) => "<",
            Some(Tok::Gt) => ">",
            Some(Tok::Le) => "<=",
            Some(Tok::Ge) => ">=",
            _ => return Ok(lhs),
        };
        self.pos += 1;
        let rhs = self.concat_expr()?;
        Ok(Expr::Binary(op, Box::new(lhs), Box::new(rhs)))
    }

    fn concat_expr(&mut self) -> Result<Expr, String> {
        let lhs = self.add_expr()?;
        if matches!(self.peek(), Some(Tok::DotDot)) {
            self.pos += 1;
            let rhs = self.concat_expr()?; // right assoc
            return Ok(Expr::Binary("..", Box::new(lhs), Box::new(rhs)));
        }
        Ok(lhs)
    }

    fn add_expr(&mut self) -> Result<Expr, String> {
        let mut lhs = self.mul_expr()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Plus) => "+",
                Some(Tok::Minus) => "-",
                _ => break,
            };
            self.pos += 1;
            let rhs = self.mul_expr()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn mul_expr(&mut self) -> Result<Expr, String> {
        let mut lhs = self.unary_expr()?;
        loop {
            let op = match self.peek() {
                Some(Tok::Star) => "*",
                Some(Tok::Slash) => "/",
                Some(Tok::Percent) => "%",
                _ => break,
            };
            self.pos += 1;
            let rhs = self.unary_expr()?;
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs));
        }
        Ok(lhs)
    }

    fn unary_expr(&mut self) -> Result<Expr, String> {
        match self.peek() {
            Some(Tok::Bang) => {
                self.pos += 1;
                Ok(Expr::Unary("not", Box::new(self.unary_expr()?)))
            }
            Some(Tok::Minus) => {
                self.pos += 1;
                Ok(Expr::Unary("-", Box::new(self.unary_expr()?)))
            }
            _ => self.postfix_expr(),
        }
    }

    fn postfix_expr(&mut self) -> Result<Expr, String> {
        let mut e = self.primary()?;
        loop {
            match self.peek() {
                Some(Tok::LParen) => {
                    self.pos += 1;
                    let mut args = Vec::new();
                    while !matches!(self.peek(), Some(Tok::RParen)) {
                        args.push(self.expr()?);
                        if matches!(self.peek(), Some(Tok::Comma)) {
                            self.pos += 1;
                        }
                    }
                    self.expect(Tok::RParen)?;
                    e = Expr::Call(Box::new(e), args);
                }
                Some(Tok::Dot) => {
                    self.pos += 1;
                    let name = self.ident()?;
                    e = Expr::Field(Box::new(e), name);
                }
                Some(Tok::LBracket) => {
                    self.pos += 1;
                    let idx = self.expr()?;
                    self.expect(Tok::RBracket)?;
                    e = Expr::Index(Box::new(e), Box::new(idx));
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.next()? {
            Tok::Num(n) => Ok(Expr::Num(n)),
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::Ident(s) if s == "true" => Ok(Expr::Bool(true)),
            Tok::Ident(s) if s == "false" => Ok(Expr::Bool(false)),
            Tok::Ident(s) if s == "nil" => Ok(Expr::Nil),
            Tok::Ident(s) => Ok(Expr::Ident(s)),
            Tok::LParen => {
                let e = self.expr()?;
                self.expect(Tok::RParen)?;
                Ok(e)
            }
            Tok::LBrace => {
                // table literal: { name = expr, ... }
                let mut fields = Vec::new();
                while !matches!(self.peek(), Some(Tok::RBrace)) {
                    let name = self.ident()?;
                    self.expect(Tok::Assign)?;
                    let e = self.expr()?;
                    fields.push((name, e));
                    if matches!(self.peek(), Some(Tok::Comma)) {
                        self.pos += 1;
                    }
                }
                self.expect(Tok::RBrace)?;
                Ok(Expr::Table(fields))
            }
            Tok::LBracket => {
                let mut items = Vec::new();
                while !matches!(self.peek(), Some(Tok::RBracket)) {
                    items.push(self.expr()?);
                    if matches!(self.peek(), Some(Tok::Comma)) {
                        self.pos += 1;
                    }
                }
                self.expect(Tok::RBracket)?;
                Ok(Expr::Array(items))
            }
            t => Err(format!("unexpected token in expression: {t:?}")),
        }
    }
}

// ---------------------------------------------------------------------------
// Liveness: which identifiers does a region reference?
// ---------------------------------------------------------------------------

fn refs_expr(e: &Expr, out: &mut BTreeSet<String>) {
    match e {
        Expr::Ident(n) => {
            out.insert(n.clone());
        }
        Expr::Field(b, _) => refs_expr(b, out),
        Expr::Index(a, b) => {
            refs_expr(a, out);
            refs_expr(b, out);
        }
        Expr::Call(f, args) => {
            refs_expr(f, out);
            for a in args {
                refs_expr(a, out);
            }
        }
        Expr::Unary(_, a) => refs_expr(a, out),
        Expr::Binary(_, a, b) => {
            refs_expr(a, out);
            refs_expr(b, out);
        }
        Expr::Table(fs) => {
            for (_, v) in fs {
                refs_expr(v, out);
            }
        }
        Expr::Array(items) => {
            for v in items {
                refs_expr(v, out);
            }
        }
        _ => {}
    }
}

fn refs_stmts(ss: &[Stmt], out: &mut BTreeSet<String>) {
    for s in ss {
        match s {
            Stmt::Let(_, e) => refs_expr(e, out),
            Stmt::Assign(l, r) => {
                refs_expr(l, out);
                refs_expr(r, out);
            }
            Stmt::Return(Some(e)) => refs_expr(e, out),
            Stmt::Return(None) => {}
            Stmt::If(c, t, e) => {
                refs_expr(c, out);
                refs_stmts(t, out);
                if let Some(e) = e {
                    refs_stmts(e, out);
                }
            }
            Stmt::Mark(_) => {}
            Stmt::Cast(t, body) => {
                if let CastTarget::Session(e) = t {
                    refs_expr(e, out);
                }
                refs_stmts(body, out);
            }
            Stmt::Spawn(e) => refs_expr(e, out),
            Stmt::Expr(e) => refs_expr(e, out),
        }
    }
}

fn toplevel_lets(ss: &[Stmt]) -> Vec<String> {
    ss.iter()
        .filter_map(|s| match s {
            Stmt::Let(n, _) => Some(n.clone()),
            _ => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Codegen
// ---------------------------------------------------------------------------

struct Gen {
    /// rt.register(...) blocks, emitted after all function definitions.
    regs: Vec<String>,
    /// per-function cast counter for stable `name:cN` hop ids
    cast_n: u32,
    fn_name: String,
}

fn lua_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n"))
}

fn emit_expr(e: &Expr) -> String {
    match e {
        Expr::Num(n) => n.clone(),
        Expr::Str(s) => lua_str(s),
        Expr::Bool(true) => "true".into(),
        Expr::Bool(false) => "false".into(),
        Expr::Nil => "nil".into(),
        Expr::Ident(n) => n.clone(),
        Expr::Field(b, f) => format!("{}.{}", emit_expr(b), f),
        Expr::Index(a, b) => format!("{}[{}]", emit_expr(a), emit_expr(b)),
        Expr::Call(f, args) => {
            // session() is the runtime's identity primitive
            if matches!(f.as_ref(), Expr::Ident(n) if n == "session") && args.is_empty() {
                return "rt.session()".into();
            }
            let args: Vec<String> = args.iter().map(emit_expr).collect();
            format!("{}({})", emit_expr(f), args.join(", "))
        }
        Expr::Unary(op, a) => {
            if *op == "not" {
                format!("not ({})", emit_expr(a))
            } else {
                format!("{}({})", op, emit_expr(a))
            }
        }
        Expr::Binary(op, a, b) => format!("({} {} {})", emit_expr(a), op, emit_expr(b)),
        Expr::Table(fs) => {
            let fs: Vec<String> = fs.iter().map(|(k, v)| format!("{} = {}", k, emit_expr(v))).collect();
            format!("{{ {} }}", fs.join(", "))
        }
        Expr::Array(items) => {
            let items: Vec<String> = items.iter().map(emit_expr).collect();
            format!("{{ {} }}", items.join(", "))
        }
    }
}

fn vars_table(names: &BTreeSet<String>) -> String {
    let fields: Vec<String> = names.iter().map(|n| format!("{n} = {n}")).collect();
    format!("{{ {} }}", fields.join(", "))
}

fn hop_target(side: Side) -> &'static str {
    match side {
        Side::Server => "\"server\"",
        // browser!() returns to the flow's origin session
        Side::Browser => "rt.session()",
    }
}

impl Gen {
    /// Emit statements at `indent`, tracking `scope` (in-scope locals) so
    /// cast sites know what to capture.
    fn stmts(&mut self, ss: &[Stmt], scope: &mut Vec<String>, indent: usize, out: &mut String) {
        let pad = "  ".repeat(indent);
        for s in ss {
            match s {
                Stmt::Let(n, e) => {
                    let _ = writeln!(out, "{pad}local {} = {}", n, emit_expr(e));
                    scope.push(n.clone());
                }
                Stmt::Assign(l, r) => {
                    let _ = writeln!(out, "{pad}{} = {}", emit_expr(l), emit_expr(r));
                }
                Stmt::Return(Some(e)) => {
                    let _ = writeln!(out, "{pad}return {}", emit_expr(e));
                }
                Stmt::Return(None) => {
                    let _ = writeln!(out, "{pad}return");
                }
                Stmt::If(c, t, e) => {
                    let _ = writeln!(out, "{pad}if {} then", emit_expr(c));
                    let mut inner = scope.clone();
                    self.stmts(t, &mut inner, indent + 1, out);
                    if let Some(e) = e {
                        let _ = writeln!(out, "{pad}else");
                        let mut inner = scope.clone();
                        self.stmts(e, &mut inner, indent + 1, out);
                    }
                    let _ = writeln!(out, "{pad}end");
                }
                Stmt::Mark(_) => {
                    unreachable!("marks are split before emission; nested marks are rejected")
                }
                Stmt::Cast(target, body) => {
                    self.cast_n += 1;
                    let id = format!("{}:c{}", self.fn_name, self.cast_n);
                    // captured = referenced by body ∩ in scope here
                    let mut refs = BTreeSet::new();
                    refs_stmts(body, &mut refs);
                    let captured: BTreeSet<String> =
                        refs.into_iter().filter(|n| scope.contains(n)).collect();
                    // register the body as a segment
                    let mut reg = String::new();
                    let _ = writeln!(reg, "rt.register({}, function(__vars)", lua_str(&id));
                    let mut body_scope: Vec<String> = captured.iter().cloned().collect();
                    for n in &captured {
                        let _ = writeln!(reg, "  local {n} = __vars.{n}");
                    }
                    self.stmts(body, &mut body_scope, 1, &mut reg);
                    let _ = writeln!(reg, "end)");
                    self.regs.push(reg);
                    // the send site
                    let tgt = match target {
                        CastTarget::Browsers => "\"browsers\"".to_string(),
                        CastTarget::Server => "\"server\"".to_string(),
                        CastTarget::Session(e) => emit_expr(e),
                    };
                    let _ = writeln!(
                        out,
                        "{pad}rt.cast({}, {}, {})",
                        tgt,
                        lua_str(&id),
                        vars_table(&captured)
                    );
                }
                Stmt::Spawn(e) => {
                    let _ = writeln!(out, "{pad}rt.start_flow(function() {} end)", emit_expr(e));
                }
                Stmt::Expr(e) => {
                    let _ = writeln!(out, "{pad}{}", emit_expr(e));
                }
            }
        }
    }
}

fn check_no_nested_marks(ss: &[Stmt]) -> Result<(), String> {
    for s in ss {
        match s {
            Stmt::If(_, t, e) => {
                for inner in [Some(t), e.as_ref()].into_iter().flatten() {
                    for st in inner {
                        if matches!(st, Stmt::Mark(_)) {
                            return Err(
                                "v0: placement marks are only allowed at the top level of a function body"
                                    .into(),
                            );
                        }
                    }
                    check_no_nested_marks(inner)?;
                }
            }
            Stmt::Cast(_, body) => {
                for st in body {
                    if matches!(st, Stmt::Mark(_)) {
                        return Err("v0: placement marks are not allowed inside cast bodies".into());
                    }
                }
                check_no_nested_marks(body)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn emit_fn(name: &str, params: &[String], body: &[Stmt], out: &mut String, regs: &mut Vec<String>) -> Result<(), String> {
    check_no_nested_marks(body)?;

    // split at top-level marks; flows originate on the browser
    let mut segs: Vec<(Side, Vec<Stmt>)> = vec![(Side::Browser, Vec::new())];
    for s in body {
        if let Stmt::Mark(side) = s {
            let (cur, _) = segs.last().unwrap();
            if *cur == *side {
                return Err(format!("fn {name}: mark to the side already executing"));
            }
            segs.push((*side, Vec::new()));
        } else {
            segs.last_mut().unwrap().1.push(s.clone());
        }
    }

    let mut gen = Gen { regs: Vec::new(), cast_n: 0, fn_name: name.to_string() };

    if segs.len() == 1 {
        // unmarked: a plain, location-agnostic function
        let mut code = String::new();
        let _ = writeln!(code, "function {}({})", name, params.join(", "));
        let mut scope: Vec<String> = params.to_vec();
        gen.stmts(&segs[0].1, &mut scope, 1, &mut code);
        let _ = writeln!(code, "end");
        out.push_str(&code);
        out.push('\n');
        regs.append(&mut gen.regs);
        return Ok(());
    }

    // ship set for each hop i (into segment i):
    //   refs(segments i..end) ∩ scope at the end of segment i-1
    let n = segs.len();
    let mut ship: Vec<BTreeSet<String>> = vec![BTreeSet::new(); n];
    let mut scope_end: Vec<Vec<String>> = vec![Vec::new(); n];
    scope_end[0] = params.to_vec();
    scope_end[0].extend(toplevel_lets(&segs[0].1));
    for i in 1..n {
        let mut refs = BTreeSet::new();
        for (_, seg) in &segs[i..] {
            refs_stmts(seg, &mut refs);
        }
        ship[i] = refs.into_iter().filter(|r| scope_end[i - 1].contains(r)).collect();
        scope_end[i] = ship[i].iter().cloned().collect();
        scope_end[i].extend(toplevel_lets(&segs[i].1));
    }

    // origin function: segment 0, then chain to segment 1
    let mut code = String::new();
    let _ = writeln!(code, "function {}({})", name, params.join(", "));
    let mut scope: Vec<String> = params.to_vec();
    gen.stmts(&segs[0].1, &mut scope, 1, &mut code);
    let _ = writeln!(
        code,
        "  return rt.at({}, {}, {})",
        hop_target(segs[1].0),
        lua_str(&format!("{name}:1")),
        vars_table(&ship[1])
    );
    let _ = writeln!(code, "end");
    out.push_str(&code);
    out.push('\n');

    // registered segments 1..n-1
    for i in 1..n {
        let mut reg = String::new();
        let _ = writeln!(reg, "rt.register({}, function(__vars)", lua_str(&format!("{name}:{i}")));
        for v in &ship[i] {
            let _ = writeln!(reg, "  local {v} = __vars.{v}");
        }
        let mut scope: Vec<String> = ship[i].iter().cloned().collect();
        gen.stmts(&segs[i].1, &mut scope, 1, &mut reg);
        if i + 1 < n {
            let _ = writeln!(
                reg,
                "  return rt.at({}, {}, {})",
                hop_target(segs[i + 1].0),
                lua_str(&format!("{name}:{}", i + 1)),
                vars_table(&ship[i + 1])
            );
        }
        let _ = writeln!(reg, "end)");
        gen.regs.push(reg);
    }

    regs.append(&mut gen.regs);
    Ok(())
}

/// Compile `.hop` source to a Lua chunk targeting the hoprt runtime.
pub fn compile(src: &str) -> Result<String, String> {
    let toks = lex(src)?;
    let mut parser = Parser { toks, pos: 0 };
    let items = parser.items()?;

    let mut out = String::from(
        "-- generated by hopc; do not edit.\n-- segments registered below correspond to placement marks in the source.\n\n",
    );
    let mut regs: Vec<String> = Vec::new();

    for item in &items {
        match item {
            Item::ServerLet(name, e) => {
                let _ = writeln!(
                    out,
                    "if SIDE == \"server\" then\n  {} = {}\nend\n",
                    name,
                    emit_expr(e)
                );
            }
            Item::Fn(name, params, body) => emit_fn(name, params, body, &mut out, &mut regs)?,
        }
    }

    if !regs.is_empty() {
        out.push_str("-- hop segments (same table on every VM; the wire carries ids + vars)\n");
        for r in regs {
            out.push_str(&r);
        }
    }
    Ok(out)
}
