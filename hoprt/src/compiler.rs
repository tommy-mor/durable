//! hopc — compiles `.hop` source to Hop IR (see ir.rs, docs/hop-ir.md).
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
//!        | "cast" ("browsers"|"server"|("session"|"user") "(" expr ")") block
//!        | "spawn" call ";"                        start a flow (an "event")
//!        | "return" expr? ";"
//!        | "if" expr block ("else" (block|if))?
//!        | "match" expr "{" arm* "}"               dispatch on expr.type
//!        | "for" k "," v "in" expr block           k = index (0-based) / key
//!        | "while" expr block
//!        | lvalue "=" expr ";"                     assignment
//!        | expr ";"
//! arm   := name ("{" field ("," field)* ","? "}")? "=>" block
//!        | "else" "=>" block                       optional, must be last
//! expr  := ... | ":name"                           keyword → string literal
//!        | "fn" "(" params ")" block               lambda; may contain marks
//! ```
//!
//! Compilation of a marked function: the body splits into segments at the
//! marks; segment 0 becomes the origin function; each later segment is
//! compiled as a separate IR function registered under a stable hop id
//! (`name:i`) and chained via the `At` instruction. What crosses each hop
//! is computed here, statically: the variables referenced by the remainder
//! that are in scope before the mark. Cast bodies compile the same way
//! under `name:cN` ids with their own captured-vars set.
//!
//! Lambdas may contain marks. A lambda's segment 0 becomes a closure whose
//! captures are copied from the enclosing frame at the `Closure`
//! instruction — closures never cross the wire. Only the marked remainder
//! ships, under `enclosing:lN:i` hop ids. This is what makes hiccup
//! attributes like `onclick = fn(e) { server!(); ... }` work.
//!
//! v0 restrictions (deliberate): marks only at the top level of a function
//! or lambda body; flows originate on the browser; no
//! try/catch (errors still propagate — they surface at the flow origin).

use std::collections::{BTreeSet, HashMap};

use crate::ir::{BinOp, Function, Instr, Program, UnOp};
use crate::value::Value;

// ---------------------------------------------------------------------------
// Lexer
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Num(String),
    Str(String),
    Keyword(String),
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
    FatArrow,
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
            '=' if two('>') => { out.push(Tok::FatArrow); i += 2 }
            '=' => { out.push(Tok::Assign); i += 1 }
            '!' if two('=') => { out.push(Tok::Ne); i += 2 }
            '!' => { out.push(Tok::Bang); i += 1 }
            '<' if two('=') => { out.push(Tok::Le); i += 2 }
            '<' => { out.push(Tok::Lt); i += 1 }
            '>' if two('=') => { out.push(Tok::Ge); i += 2 }
            '>' => { out.push(Tok::Gt); i += 1 }
            '&' if two('&') => { out.push(Tok::AndAnd); i += 2 }
            '|' if two('|') => { out.push(Tok::OrOr); i += 2 }
            ':' => {
                i += 1;
                let start = i;
                while i < cs.len() && (cs[i].is_ascii_alphanumeric() || cs[i] == '_' || cs[i] == '-') {
                    i += 1;
                }
                if i == start {
                    return Err("expected identifier after ':'".into());
                }
                out.push(Tok::Keyword(cs[start..i].iter().collect()));
            }
            '"' => {
                i += 1;
                let mut s = String::new();
                while i < cs.len() && cs[i] != '"' {
                    if cs[i] == '\\' && i + 1 < cs.len() {
                        i += 1;
                        s.push(match cs[i] {
                            'n' => '\n',
                            't' => '\t',
                            c => c,
                        });
                    } else {
                        s.push(cs[i]);
                    }
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
    /// `fn (params) { ... }` in expression position; body may contain marks
    Fn(Vec<String>, Vec<Stmt>),
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
    /// Every connected tab of one user (address "user:<uid>").
    User(Expr),
}

/// One `match` arm: `tag { field, ... } => block`. `tag == None` is the
/// `else` arm (no fields, always matches).
#[derive(Debug, Clone)]
struct MatchArm {
    tag: Option<String>,
    fields: Vec<String>,
    body: Vec<Stmt>,
}

#[derive(Debug, Clone)]
enum Stmt {
    Let(String, Expr),
    Assign(Expr, Expr),
    Return(Option<Expr>),
    If(Expr, Vec<Stmt>, Option<Vec<Stmt>>),
    /// `match expr { arm* }` — dispatch on `expr.type`; first match wins.
    Match(Expr, Vec<MatchArm>),
    /// `for k, v in expr { ... }` — arrays yield (0-based index, element),
    /// maps yield (key, value) in key order.
    For(String, String, Expr, Vec<Stmt>),
    /// `while expr { ... }` — condition re-evaluated each iteration.
    While(Expr, Vec<Stmt>),
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

    fn params(&mut self) -> Result<Vec<String>, String> {
        self.expect(Tok::LParen)?;
        let mut params = Vec::new();
        while !matches!(self.peek(), Some(Tok::RParen)) {
            params.push(self.ident()?);
            if matches!(self.peek(), Some(Tok::Comma)) {
                self.pos += 1;
            }
        }
        self.expect(Tok::RParen)?;
        Ok(params)
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
                let params = self.params()?;
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
        if self.at_kw("match") {
            return self.match_stmt();
        }
        if self.at_kw("for") {
            self.eat_kw("for")?;
            let k = self.ident()?;
            self.expect(Tok::Comma)?;
            let v = self.ident()?;
            self.eat_kw("in")?;
            let e = self.expr()?;
            let body = self.block()?;
            return Ok(Stmt::For(k, v, e, body));
        }
        if self.at_kw("while") {
            self.eat_kw("while")?;
            let c = self.expr()?;
            let body = self.block()?;
            return Ok(Stmt::While(c, body));
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
            } else if self.at_kw("user") {
                self.pos += 1;
                self.expect(Tok::LParen)?;
                let e = self.expr()?;
                self.expect(Tok::RParen)?;
                CastTarget::User(e)
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

    fn match_stmt(&mut self) -> Result<Stmt, String> {
        self.eat_kw("match")?;
        let subject = self.expr()?;
        self.expect(Tok::LBrace)?;
        let mut arms = Vec::new();
        let mut saw_else = false;
        while !matches!(self.peek(), Some(Tok::RBrace)) {
            if saw_else {
                return Err("match: `else` must be the last arm".into());
            }
            if self.at_kw("else") {
                self.eat_kw("else")?;
                self.expect(Tok::FatArrow)?;
                let body = self.block()?;
                arms.push(MatchArm { tag: None, fields: Vec::new(), body });
                saw_else = true;
            } else {
                let tag = self.ident()?;
                let mut fields = Vec::new();
                if matches!(self.peek(), Some(Tok::LBrace)) {
                    self.pos += 1;
                    while !matches!(self.peek(), Some(Tok::RBrace)) {
                        fields.push(self.ident()?);
                        if matches!(self.peek(), Some(Tok::Comma)) {
                            self.pos += 1;
                        }
                    }
                    self.expect(Tok::RBrace)?;
                }
                self.expect(Tok::FatArrow)?;
                let body = self.block()?;
                arms.push(MatchArm { tag: Some(tag), fields, body });
            }
        }
        self.expect(Tok::RBrace)?;
        Ok(Stmt::Match(subject, arms))
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
            Some(Tok::Ne) => "!=",
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
        // lambda: `fn (params) { ... }`
        if self.at_kw("fn") {
            self.eat_kw("fn")?;
            let params = self.params()?;
            let body = self.block()?;
            return Ok(Expr::Fn(params, body));
        }
        match self.next()? {
            Tok::Num(n) => Ok(Expr::Num(n)),
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::Keyword(k) => Ok(Expr::Str(k)),
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
                // map literal: { name = expr, ... }
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
        // a lambda's captures are whatever its body references
        Expr::Fn(_, body) => refs_stmts(body, out),
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
            Stmt::Match(subject, arms) => {
                refs_expr(subject, out);
                for arm in arms {
                    refs_stmts(&arm.body, out);
                }
            }
            Stmt::For(_, _, e, body) => {
                refs_expr(e, out);
                refs_stmts(body, out);
            }
            Stmt::While(c, body) => {
                refs_expr(c, out);
                refs_stmts(body, out);
            }
            Stmt::Mark(_) => {}
            Stmt::Cast(t, body) => {
                if let CastTarget::Session(e) | CastTarget::User(e) = t {
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

/// Marks are only legal at the top level of a (fn or lambda) body — not in
/// branches, loops, or cast bodies. Lambda bodies are separate flow bodies
/// and are checked when the lambda itself is compiled.
fn check_no_nested_marks(ss: &[Stmt]) -> Result<(), String> {
    let reject = |body: &[Stmt], what: &str| -> Result<(), String> {
        for st in body {
            if matches!(st, Stmt::Mark(_)) {
                return Err(format!("v0: placement marks are not allowed inside {what}"));
            }
        }
        check_no_nested_marks(body)
    };
    for s in ss {
        match s {
            Stmt::If(_, t, e) => {
                reject(t, "branches; marks must be at the top level of a function body")?;
                if let Some(e) = e {
                    reject(e, "branches; marks must be at the top level of a function body")?;
                }
            }
            Stmt::Match(_, arms) => {
                for arm in arms {
                    reject(&arm.body, "match arms; marks must be at the top level of a function body")?;
                }
            }
            Stmt::For(_, _, _, body) | Stmt::While(_, body) => {
                reject(body, "loop bodies; marks must be at the top level of a function body")?
            }
            Stmt::Cast(_, body) => reject(body, "cast bodies")?,
            _ => {}
        }
    }
    Ok(())
}

/// Split a flow body at its top-level marks.
fn split_segments(body: &[Stmt], name: &str) -> Result<Vec<(Side, Vec<Stmt>)>, String> {
    let mut segs: Vec<(Side, Vec<Stmt>)> = vec![(Side::Browser, Vec::new())];
    for s in body {
        if let Stmt::Mark(side) = s {
            let (cur, _) = segs.last().unwrap();
            if *cur == *side {
                return Err(format!("{name}: mark to the side already executing"));
            }
            segs.push((*side, Vec::new()));
        } else {
            segs.last_mut().unwrap().1.push(s.clone());
        }
    }
    Ok(segs)
}

// ---------------------------------------------------------------------------
// Codegen: AST → IR
// ---------------------------------------------------------------------------

#[derive(PartialEq, Eq, Hash)]
enum CKey {
    Str(String),
    Int(i64),
    FloatBits(u64),
}

#[derive(Default)]
struct Cg {
    consts: Vec<Value>,
    const_ix: HashMap<CKey, u32>,
    fns: Vec<Function>,
    hops: HashMap<String, usize>,
    named: HashMap<String, usize>,
    server_lets: Vec<(String, usize)>,
}

impl Cg {
    fn k_str(&mut self, s: &str) -> u32 {
        let key = CKey::Str(s.to_string());
        if let Some(&i) = self.const_ix.get(&key) {
            return i;
        }
        let i = self.consts.len() as u32;
        self.consts.push(Value::str(s));
        self.const_ix.insert(key, i);
        i
    }

    fn k_int(&mut self, n: i64) -> u32 {
        let key = CKey::Int(n);
        if let Some(&i) = self.const_ix.get(&key) {
            return i;
        }
        let i = self.consts.len() as u32;
        self.consts.push(Value::Int(n));
        self.const_ix.insert(key, i);
        i
    }

    fn k_float(&mut self, f: f64) -> u32 {
        let key = CKey::FloatBits(f.to_bits());
        if let Some(&i) = self.const_ix.get(&key) {
            return i;
        }
        let i = self.consts.len() as u32;
        self.consts.push(Value::Float(f));
        self.const_ix.insert(key, i);
        i
    }

    fn add_fn(&mut self, f: Function) -> usize {
        self.fns.push(f);
        self.fns.len() - 1
    }
}

/// Per-named-fn counters for stable `name:cN` / `name:lN` hop ids.
struct Counters {
    fn_name: String,
    cast_n: u32,
    lambda_n: u32,
}

/// One function body being emitted.
struct Fb {
    name: String,
    scope: Vec<(String, u16)>,
    n_locals: u16,
    n_caps: u8,
    n_params: u8,
    code: Vec<Instr>,
}

impl Fb {
    fn new(name: &str) -> Fb {
        Fb {
            name: name.to_string(),
            scope: Vec::new(),
            n_locals: 0,
            n_caps: 0,
            n_params: 0,
            code: Vec::new(),
        }
    }

    fn def_local(&mut self, name: &str) -> u16 {
        let slot = self.n_locals;
        self.n_locals += 1;
        self.scope.push((name.to_string(), slot));
        slot
    }

    fn hidden_local(&mut self) -> u16 {
        let slot = self.n_locals;
        self.n_locals += 1;
        slot
    }

    fn lookup(&self, name: &str) -> Option<u16> {
        self.scope.iter().rev().find(|(n, _)| n == name).map(|(_, s)| *s)
    }

    fn scope_names(&self) -> Vec<String> {
        self.scope.iter().map(|(n, _)| n.clone()).collect()
    }

    fn emit(&mut self, i: Instr) {
        self.code.push(i);
    }

    /// Emit a jump placeholder; returns its index for patching.
    fn jump_placeholder(&mut self, f: fn(i32) -> Instr) -> usize {
        self.code.push(f(0));
        self.code.len() - 1
    }

    /// Patch a placeholder to jump to the current end of code.
    fn patch_to_here(&mut self, at: usize) {
        let d = self.code.len() as i32 - (at as i32 + 1);
        self.code[at] = match self.code[at] {
            Instr::Jump(_) => Instr::Jump(d),
            Instr::JumpIfFalse(_) => Instr::JumpIfFalse(d),
            Instr::IterNext(_) => Instr::IterNext(d),
            _ => unreachable!("not a jump"),
        };
    }

    fn into_function(self) -> Function {
        Function {
            name: self.name,
            n_caps: self.n_caps,
            n_params: self.n_params,
            n_locals: self.n_locals.max((self.n_caps as u16) + (self.n_params as u16)),
            code: self.code,
        }
    }
}

fn emit_expr(cg: &mut Cg, fb: &mut Fb, ctr: &mut Counters, e: &Expr) -> Result<(), String> {
    match e {
        Expr::Num(n) => {
            if n.contains('.') {
                let f: f64 = n.parse().map_err(|_| format!("bad number: {n}"))?;
                let k = cg.k_float(f);
                fb.emit(Instr::Const(k));
            } else {
                let i: i64 = n.parse().map_err(|_| format!("bad number: {n}"))?;
                let k = cg.k_int(i);
                fb.emit(Instr::Const(k));
            }
        }
        Expr::Str(s) => {
            let k = cg.k_str(s);
            fb.emit(Instr::Const(k));
        }
        Expr::Bool(true) => fb.emit(Instr::True),
        Expr::Bool(false) => fb.emit(Instr::False),
        Expr::Nil => fb.emit(Instr::Nil),
        Expr::Ident(n) => match fb.lookup(n) {
            Some(slot) => fb.emit(Instr::LoadLocal(slot)),
            None => {
                let k = cg.k_str(n);
                fb.emit(Instr::LoadGlobal(k));
            }
        },
        Expr::Field(b, f) => {
            emit_expr(cg, fb, ctr, b)?;
            let k = cg.k_str(f);
            fb.emit(Instr::GetField(k));
        }
        Expr::Index(a, b) => {
            emit_expr(cg, fb, ctr, a)?;
            emit_expr(cg, fb, ctr, b)?;
            fb.emit(Instr::GetIndex);
        }
        Expr::Call(f, args) => {
            // session() and user() are the runtime's identity primitives
            if matches!(f.as_ref(), Expr::Ident(n) if n == "session") && args.is_empty() {
                fb.emit(Instr::Session);
                return Ok(());
            }
            if matches!(f.as_ref(), Expr::Ident(n) if n == "user") && args.is_empty() {
                fb.emit(Instr::User);
                return Ok(());
            }
            emit_expr(cg, fb, ctr, f)?;
            for a in args {
                emit_expr(cg, fb, ctr, a)?;
            }
            fb.emit(Instr::Call(args.len() as u8));
        }
        Expr::Unary(op, a) => {
            emit_expr(cg, fb, ctr, a)?;
            fb.emit(Instr::UnOp(match *op {
                "not" => UnOp::Not,
                _ => UnOp::Neg,
            }));
        }
        Expr::Binary("and", a, b) => {
            emit_expr(cg, fb, ctr, a)?;
            fb.emit(Instr::Dup);
            let j = fb.jump_placeholder(Instr::JumpIfFalse);
            fb.emit(Instr::Pop);
            emit_expr(cg, fb, ctr, b)?;
            fb.patch_to_here(j);
        }
        Expr::Binary("or", a, b) => {
            emit_expr(cg, fb, ctr, a)?;
            fb.emit(Instr::Dup);
            fb.emit(Instr::UnOp(UnOp::Not));
            let j = fb.jump_placeholder(Instr::JumpIfFalse);
            fb.emit(Instr::Pop);
            emit_expr(cg, fb, ctr, b)?;
            fb.patch_to_here(j);
        }
        Expr::Binary(op, a, b) => {
            emit_expr(cg, fb, ctr, a)?;
            emit_expr(cg, fb, ctr, b)?;
            fb.emit(Instr::BinOp(match *op {
                "+" => BinOp::Add,
                "-" => BinOp::Sub,
                "*" => BinOp::Mul,
                "/" => BinOp::Div,
                "%" => BinOp::Mod,
                ".." => BinOp::Concat,
                "==" => BinOp::Eq,
                "!=" => BinOp::Ne,
                "<" => BinOp::Lt,
                "<=" => BinOp::Le,
                ">" => BinOp::Gt,
                ">=" => BinOp::Ge,
                other => return Err(format!("unknown operator {other}")),
            }));
        }
        Expr::Table(fs) => {
            for (k, v) in fs {
                let kk = cg.k_str(k);
                fb.emit(Instr::Const(kk));
                emit_expr(cg, fb, ctr, v)?;
            }
            fb.emit(Instr::MakeMap(fs.len() as u16));
        }
        Expr::Array(items) => {
            for v in items {
                emit_expr(cg, fb, ctr, v)?;
            }
            fb.emit(Instr::MakeArray(items.len() as u16));
        }
        Expr::Fn(params, body) => emit_lambda(cg, fb, ctr, params, body)?,
    }
    Ok(())
}

/// A lambda's segment 0 becomes a closure; its captures are the outer
/// locals its body references, copied by value at the `Closure`
/// instruction. The marked remainder ships under `enclosing:lN:i` hop
/// ids like any other segments.
fn emit_lambda(
    cg: &mut Cg,
    outer: &mut Fb,
    ctr: &mut Counters,
    params: &[String],
    body: &[Stmt],
) -> Result<(), String> {
    ctr.lambda_n += 1;
    let prefix = format!("{}:l{}", ctr.fn_name, ctr.lambda_n);

    // captures: referenced by the body, bound in the outer frame, not
    // shadowed by a parameter
    let mut refs = BTreeSet::new();
    refs_stmts(body, &mut refs);
    let caps: Vec<String> = refs
        .into_iter()
        .filter(|n| !params.contains(n) && outer.lookup(n).is_some())
        .collect();

    let mut fb = Fb::new(&prefix);
    fb.n_caps = caps.len() as u8;
    fb.n_params = params.len() as u8;
    for c in &caps {
        fb.def_local(c);
    }
    for p in params {
        fb.def_local(p);
    }
    emit_flow_body(cg, &mut fb, ctr, &prefix, body)?;
    let fn_idx = cg.add_fn(fb.into_function());

    for c in &caps {
        let slot = outer.lookup(c).unwrap();
        outer.emit(Instr::LoadLocal(slot));
    }
    outer.emit(Instr::Closure(fn_idx as u32, caps.len() as u8));
    Ok(())
}

/// Emit the target of a hop: the server, or the flow's origin session.
fn emit_hop_target(cg: &mut Cg, fb: &mut Fb, side: Side) {
    match side {
        Side::Server => {
            let k = cg.k_str("server");
            fb.emit(Instr::Const(k));
        }
        Side::Browser => fb.emit(Instr::Session),
    }
}

/// Build the vars map `{ name = name, ... }` from locals currently in scope.
fn emit_vars_map(cg: &mut Cg, fb: &mut Fb, names: &BTreeSet<String>) -> Result<(), String> {
    for n in names {
        let k = cg.k_str(n);
        fb.emit(Instr::Const(k));
        match fb.lookup(n) {
            Some(slot) => fb.emit(Instr::LoadLocal(slot)),
            None => return Err(format!("{}: shipped var {n} is not in scope", fb.name)),
        }
    }
    fb.emit(Instr::MakeMap(names.len() as u16));
    Ok(())
}

/// Compile a segment (a hop-reachable function): one `__vars` parameter,
/// destructured in the prologue.
fn compile_segment(
    cg: &mut Cg,
    ctr: &mut Counters,
    id: &str,
    ship: &BTreeSet<String>,
    body: &[Stmt],
    chain: Option<(Side, String, BTreeSet<String>)>,
) -> Result<(), String> {
    let mut fb = Fb::new(id);
    fb.n_params = 1;
    let vars_slot = fb.def_local("__vars");
    for v in ship {
        fb.emit(Instr::LoadLocal(vars_slot));
        let k = cg.k_str(v);
        fb.emit(Instr::GetField(k));
        let slot = fb.def_local(v);
        fb.emit(Instr::StoreLocal(slot));
    }
    emit_stmts(cg, &mut fb, ctr, body)?;
    if let Some((side, next_id, next_ship)) = chain {
        emit_hop_target(cg, &mut fb, side);
        emit_vars_map(cg, &mut fb, &next_ship)?;
        let k = cg.k_str(&next_id);
        fb.emit(Instr::At(k));
        fb.emit(Instr::Return);
    }
    let fn_idx = cg.add_fn(fb.into_function());
    cg.hops.insert(id.to_string(), fn_idx);
    Ok(())
}

/// Emit a flow body into `fb` (a named fn or a lambda's segment 0): the
/// segment-0 code plus the chain into segment 1 when marks are present.
/// Later segments are compiled as separate hop functions.
fn emit_flow_body(
    cg: &mut Cg,
    fb: &mut Fb,
    ctr: &mut Counters,
    prefix: &str,
    body: &[Stmt],
) -> Result<(), String> {
    check_no_nested_marks(body)?;
    let segs = split_segments(body, prefix)?;
    let n = segs.len();

    if n == 1 {
        return emit_stmts(cg, fb, ctr, &segs[0].1);
    }

    // ship set for each hop i (into segment i):
    //   refs(segments i..end) ∩ scope at the end of segment i-1
    let base_scope = fb.scope_names();
    let mut ship: Vec<BTreeSet<String>> = vec![BTreeSet::new(); n];
    let mut scope_end: Vec<Vec<String>> = vec![Vec::new(); n];
    scope_end[0] = base_scope;
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

    // segment 0 plus the chain into segment 1
    emit_stmts(cg, fb, ctr, &segs[0].1)?;
    emit_hop_target(cg, fb, segs[1].0);
    emit_vars_map(cg, fb, &ship[1])?;
    let k = cg.k_str(&format!("{prefix}:1"));
    fb.emit(Instr::At(k));
    fb.emit(Instr::Return);

    // segments 1..n-1 as hop functions
    for i in 1..n {
        let chain = if i + 1 < n {
            Some((segs[i + 1].0, format!("{prefix}:{}", i + 1), ship[i + 1].clone()))
        } else {
            None
        };
        compile_segment(cg, ctr, &format!("{prefix}:{i}"), &ship[i], &segs[i].1, chain)?;
    }
    Ok(())
}

fn emit_stmts(cg: &mut Cg, fb: &mut Fb, ctr: &mut Counters, ss: &[Stmt]) -> Result<(), String> {
    for s in ss {
        match s {
            Stmt::Let(n, e) => {
                emit_expr(cg, fb, ctr, e)?;
                let slot = fb.def_local(n);
                fb.emit(Instr::StoreLocal(slot));
            }
            Stmt::Assign(l, r) => match l {
                Expr::Ident(n) => {
                    emit_expr(cg, fb, ctr, r)?;
                    match fb.lookup(n) {
                        Some(slot) => fb.emit(Instr::StoreLocal(slot)),
                        None => {
                            let k = cg.k_str(n);
                            fb.emit(Instr::StoreGlobal(k));
                        }
                    }
                }
                Expr::Field(b, f) => {
                    emit_expr(cg, fb, ctr, b)?;
                    emit_expr(cg, fb, ctr, r)?;
                    let k = cg.k_str(f);
                    fb.emit(Instr::SetField(k));
                }
                Expr::Index(a, i) => {
                    emit_expr(cg, fb, ctr, a)?;
                    emit_expr(cg, fb, ctr, i)?;
                    emit_expr(cg, fb, ctr, r)?;
                    fb.emit(Instr::SetIndex);
                }
                _ => return Err("invalid assignment target".into()),
            },
            Stmt::Return(e) => {
                match e {
                    Some(e) => emit_expr(cg, fb, ctr, e)?,
                    None => fb.emit(Instr::Nil),
                }
                fb.emit(Instr::Return);
            }
            Stmt::If(c, t, els) => {
                emit_expr(cg, fb, ctr, c)?;
                let j_else = fb.jump_placeholder(Instr::JumpIfFalse);
                let mark = fb.scope.len();
                emit_stmts(cg, fb, ctr, t)?;
                fb.scope.truncate(mark);
                match els {
                    Some(els) => {
                        let j_end = fb.jump_placeholder(Instr::Jump);
                        fb.patch_to_here(j_else);
                        let mark = fb.scope.len();
                        emit_stmts(cg, fb, ctr, els)?;
                        fb.scope.truncate(mark);
                        fb.patch_to_here(j_end);
                    }
                    None => fb.patch_to_here(j_else),
                }
            }
            // Lowered to the plain if-chain it replaces: the subject and its
            // `.type` are evaluated once into hidden locals, each arm is an
            // Eq against the tag string + JumpIfFalse to the next arm, and a
            // matching arm loads its destructured fields into fresh locals
            // (scoped to the arm) before its body runs. First match wins;
            // no match and no `else` falls through.
            Stmt::Match(subject, arms) => {
                emit_expr(cg, fb, ctr, subject)?;
                let subj_slot = fb.hidden_local();
                fb.emit(Instr::StoreLocal(subj_slot));
                let type_k = cg.k_str("type");
                fb.emit(Instr::LoadLocal(subj_slot));
                fb.emit(Instr::GetField(type_k));
                let tag_slot = fb.hidden_local();
                fb.emit(Instr::StoreLocal(tag_slot));
                let mut end_jumps = Vec::new();
                for arm in arms {
                    match &arm.tag {
                        Some(tag) => {
                            fb.emit(Instr::LoadLocal(tag_slot));
                            let k = cg.k_str(tag);
                            fb.emit(Instr::Const(k));
                            fb.emit(Instr::BinOp(BinOp::Eq));
                            let j_next = fb.jump_placeholder(Instr::JumpIfFalse);
                            let mark = fb.scope.len();
                            for f in &arm.fields {
                                fb.emit(Instr::LoadLocal(subj_slot));
                                let fk = cg.k_str(f);
                                fb.emit(Instr::GetField(fk));
                                let slot = fb.def_local(f);
                                fb.emit(Instr::StoreLocal(slot));
                            }
                            emit_stmts(cg, fb, ctr, &arm.body)?;
                            fb.scope.truncate(mark);
                            end_jumps.push(fb.jump_placeholder(Instr::Jump));
                            fb.patch_to_here(j_next);
                        }
                        // `else` (parser guarantees it is last): always runs
                        None => {
                            let mark = fb.scope.len();
                            emit_stmts(cg, fb, ctr, &arm.body)?;
                            fb.scope.truncate(mark);
                        }
                    }
                }
                for j in end_jumps {
                    fb.patch_to_here(j);
                }
            }
            Stmt::For(k, v, e, body) => {
                emit_expr(cg, fb, ctr, e)?;
                fb.emit(Instr::IterNew);
                let iter_slot = fb.hidden_local();
                fb.emit(Instr::StoreLocal(iter_slot));
                let zero = cg.k_int(0);
                fb.emit(Instr::Const(zero));
                let idx_slot = fb.hidden_local();
                fb.emit(Instr::StoreLocal(idx_slot));

                let mark = fb.scope.len();
                let k_slot = fb.def_local(k);
                let v_slot = fb.def_local(v);

                let loop_start = fb.code.len();
                fb.emit(Instr::LoadLocal(iter_slot));
                fb.emit(Instr::LoadLocal(idx_slot));
                let j_end = fb.jump_placeholder(Instr::IterNext);
                // stack after IterNext: iter idx' k v
                fb.emit(Instr::StoreLocal(v_slot));
                fb.emit(Instr::StoreLocal(k_slot));
                fb.emit(Instr::StoreLocal(idx_slot));
                fb.emit(Instr::StoreLocal(iter_slot));
                emit_stmts(cg, fb, ctr, body)?;
                fb.scope.truncate(mark + 2); // keep k, v for next round
                let d = loop_start as i32 - (fb.code.len() as i32 + 1);
                fb.emit(Instr::Jump(d));
                fb.patch_to_here(j_end);
                fb.scope.truncate(mark);
            }
            Stmt::While(c, body) => {
                let loop_start = fb.code.len();
                emit_expr(cg, fb, ctr, c)?;
                let j_end = fb.jump_placeholder(Instr::JumpIfFalse);
                let mark = fb.scope.len();
                emit_stmts(cg, fb, ctr, body)?;
                fb.scope.truncate(mark);
                let d = loop_start as i32 - (fb.code.len() as i32 + 1);
                fb.emit(Instr::Jump(d));
                fb.patch_to_here(j_end);
            }
            Stmt::Mark(_) => {
                unreachable!("marks are split before emission; nested marks are rejected")
            }
            Stmt::Cast(target, body) => {
                ctr.cast_n += 1;
                let id = format!("{}:c{}", ctr.fn_name, ctr.cast_n);
                // captured = referenced by body ∩ in scope here
                let mut refs = BTreeSet::new();
                refs_stmts(body, &mut refs);
                let captured: BTreeSet<String> = refs
                    .into_iter()
                    .filter(|n| fb.lookup(n).is_some())
                    .collect();
                compile_segment(cg, ctr, &id, &captured, body, None)?;
                // the send site
                match target {
                    CastTarget::Browsers => {
                        let k = cg.k_str("browsers");
                        fb.emit(Instr::Const(k));
                    }
                    CastTarget::Server => {
                        let k = cg.k_str("server");
                        fb.emit(Instr::Const(k));
                    }
                    CastTarget::Session(e) => emit_expr(cg, fb, ctr, e)?,
                    CastTarget::User(e) => {
                        // the address is "user:" .. uid — routed as a fan-out
                        let k = cg.k_str("user:");
                        fb.emit(Instr::Const(k));
                        emit_expr(cg, fb, ctr, e)?;
                        fb.emit(Instr::BinOp(crate::ir::BinOp::Concat));
                    }
                }
                emit_vars_map(cg, fb, &captured)?;
                let k = cg.k_str(&id);
                fb.emit(Instr::Cast(k));
            }
            Stmt::Spawn(e) => {
                let Expr::Call(f, args) = e else {
                    return Err("spawn expects a function call".into());
                };
                emit_expr(cg, fb, ctr, f)?;
                for a in args {
                    emit_expr(cg, fb, ctr, a)?;
                }
                fb.emit(Instr::MakeArray(args.len() as u16));
                fb.emit(Instr::Spawn);
            }
            Stmt::Expr(e) => {
                emit_expr(cg, fb, ctr, e)?;
                fb.emit(Instr::Pop);
            }
        }
    }
    Ok(())
}

/// Compile `.hop` source to a Hop IR program.
pub fn compile(src: &str) -> Result<Program, String> {
    let toks = lex(src)?;
    let mut parser = Parser { toks, pos: 0 };
    let items = parser.items()?;

    let mut cg = Cg::default();

    for item in &items {
        match item {
            Item::ServerLet(name, e) => {
                let mut ctr = Counters {
                    fn_name: format!("let:{name}"),
                    cast_n: 0,
                    lambda_n: 0,
                };
                let mut fb = Fb::new(&format!("let:{name}"));
                emit_expr(&mut cg, &mut fb, &mut ctr, e)?;
                fb.emit(Instr::Return);
                let fn_idx = cg.add_fn(fb.into_function());
                cg.server_lets.push((name.clone(), fn_idx));
            }
            Item::Fn(name, params, body) => {
                let mut ctr = Counters {
                    fn_name: name.clone(),
                    cast_n: 0,
                    lambda_n: 0,
                };
                let mut fb = Fb::new(name);
                fb.n_params = params.len() as u8;
                for p in params {
                    fb.def_local(p);
                }
                emit_flow_body(&mut cg, &mut fb, &mut ctr, name, body)?;
                let fn_idx = cg.add_fn(fb.into_function());
                cg.named.insert(name.clone(), fn_idx);
            }
        }
    }

    Ok(Program {
        consts: cg.consts,
        fns: cg.fns,
        hops: cg.hops,
        named: cg.named,
        server_lets: cg.server_lets,
    })
}
