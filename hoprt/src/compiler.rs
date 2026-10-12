//! hopc — compiles `.hop` source to Hop IR (see ir.rs, docs/hop-ir.md).
//!
//! The v0 language is the smallest thing that exercises every runtime
//! semantic:
//!
//! ```text
//! item  := "server" "let" name "=" expr ";"        server-side global
//!        | "schema" "{" field ("," field)* ","? "}"  store schema (core construct)
//!        | "fn" name "(" params ")" block
//! field := name ":" type
//! type  := "leaf" | "sum" | "blob" "<" btype ">"
//!        | "map" "<" type ">" | "map" "<" key "," type ">"
//!        | "list" "<" type ">" | "deque" "<" type ">"
//!        | "record" "{" field ("," field)* ","? "}"
//! key   := "any" | "string" | "int" | "float" | "bool" | "bytes"
//! btype := "any" | "string" | "int" | "float" | "bool" | "bytes"
//!        | "array" "<" btype ">" | "map" "<" btype ">"
//!        | "map" "<" key "," btype ">" | "map" "{" bfield* "}"
//! bfield:= name ":" btype                         (closed CBOR map)
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
//! CBOR wins: `record` lives only outside blobs (a closed map with
//! field-id layout); inside `blob<…>` structs are `map { … }` (text keys,
//! one opaque key). `list`/`map` with angle brackets are keyspace
//! collections; `array`/`map` in blob position are CBOR kinds.
//!
//! Compilation of a marked function: the body lowers to a control-flow
//! graph of basic blocks (see the CFG pipeline below). A placement mark is
//! a call edge — the current piece parks at `At` and resumes when the far
//! piece replies — so marks are legal anywhere a flow body reaches:
//! branches, loops (`while true` included), match arms. A mark back to the
//! side that parked the current piece *closes* it in straight-line code;
//! inside a branch, arm, or loop body the piece itself entered, it opens a
//! nested piece instead, so the body rejoins the piece it started in (its
//! join block or loop header lives there). What crosses each hop is LiveIn of the far
//! piece's entry block, computed by backwards liveness over the graph;
//! what comes back in a reply is the continuation's live vars the far
//! piece defined. Hop functions register under stable hop ids
//! (`name:N` in textual mark order). Cast bodies compile the same way
//! under `name:cN` ids with their own captured-vars set (marks stay
//! illegal inside casts — a fire-and-forget message cannot hop).
//!
//! Lambdas may contain marks. A lambda's piece 0 becomes a closure whose
//! captures are copied from the enclosing frame at the `Closure`
//! instruction — closures never cross the wire. Only the marked remainder
//! ships, under `enclosing:lN:i` hop ids. This is what makes hiccup
//! attributes like `onclick = fn(e) { server!(); ... }` work.
//!
//! Remaining restrictions (deliberate): flows originate on the browser; no
//! try/catch (errors still propagate — they surface at the flow origin).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::ir::{BinOp, BlobDef, Function, HopExplain, Instr, KeyDef, Program, SchemaDef, UnOp};
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
    Colon,
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
                // `:keyword` (no space) is a string literal; a bare `:`
                // followed by space/punctuation is the schema field colon.
                let is_kw = i + 1 < cs.len()
                    && (cs[i + 1].is_ascii_alphanumeric()
                        || cs[i + 1] == '_'
                        || cs[i + 1] == '-');
                if !is_kw {
                    out.push(Tok::Colon);
                    i += 1;
                    continue;
                }
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
    /// `spawn f(args)` — start a flow now, evaluate to its handle (flow id).
    Spawn(Box<Expr>),
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
/// `else` arm (no fields, always matches). Fields are (key, local): the
/// map key to extract and the local it binds — the renamer rewrites the
/// local, never the key.
#[derive(Debug, Clone)]
struct MatchArm {
    tag: Option<String>,
    fields: Vec<(String, String)>,
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
    Schema(SchemaDef),
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

    fn schema_fields(&mut self) -> Result<Vec<(String, SchemaDef)>, String> {
        self.expect(Tok::LBrace)?;
        let mut out = Vec::new();
        while !matches!(self.peek(), Some(Tok::RBrace)) {
            let name = self.ident()?;
            self.expect(Tok::Colon)?;
            let t = self.schema_type()?;
            out.push((name, t));
            if matches!(self.peek(), Some(Tok::Comma)) {
                self.pos += 1;
            }
        }
        self.expect(Tok::RBrace)?;
        Ok(out)
    }

    fn is_key_name(s: &str) -> bool {
        matches!(s, "any" | "string" | "int" | "float" | "bool" | "bytes")
    }

    fn key_def(&mut self) -> Result<KeyDef, String> {
        match self.next()? {
            Tok::Ident(s) => match s.as_str() {
                "any" => Ok(KeyDef::Any),
                "string" => Ok(KeyDef::String),
                "int" => Ok(KeyDef::Int),
                "float" => Ok(KeyDef::Float),
                "bool" => Ok(KeyDef::Bool),
                "bytes" => Ok(KeyDef::Bytes),
                other => Err(format!("unknown map key type {other:?}")),
            },
            t => Err(format!("expected a map key type, got {t:?}")),
        }
    }

    /// After `map <`: a key form iff the next two tokens are a key-type
    /// name followed by a comma; otherwise it is the value form.
    fn map_has_key(&self) -> bool {
        matches!(self.peek(), Some(Tok::Ident(s)) if Self::is_key_name(s))
            && matches!(self.peek2(), Some(Tok::Comma))
    }

    fn schema_type(&mut self) -> Result<SchemaDef, String> {
        match self.next()? {
            Tok::Ident(s) => match s.as_str() {
                "leaf" => Ok(SchemaDef::Leaf),
                "sum" => Ok(SchemaDef::Sum),
                "blob" => {
                    self.expect(Tok::Lt)?;
                    let b = self.blob_type()?;
                    self.expect(Tok::Gt)?;
                    Ok(SchemaDef::Blob(b))
                }
                "map" => {
                    self.expect(Tok::Lt)?;
                    if self.map_has_key() {
                        let key = self.key_def()?;
                        self.expect(Tok::Comma)?;
                        let of = Box::new(self.schema_type()?);
                        self.expect(Tok::Gt)?;
                        Ok(SchemaDef::Map { key, of })
                    } else {
                        let of = Box::new(self.schema_type()?);
                        self.expect(Tok::Gt)?;
                        Ok(SchemaDef::Map { key: KeyDef::Any, of })
                    }
                }
                "list" => {
                    self.expect(Tok::Lt)?;
                    let of = Box::new(self.schema_type()?);
                    self.expect(Tok::Gt)?;
                    Ok(SchemaDef::List(of))
                }
                "deque" => {
                    self.expect(Tok::Lt)?;
                    let of = Box::new(self.schema_type()?);
                    self.expect(Tok::Gt)?;
                    Ok(SchemaDef::Deque(of))
                }
                "record" => {
                    let fields = self.schema_fields()?;
                    Ok(SchemaDef::Record(fields))
                }
                other => Err(format!("unknown schema type {other:?}")),
            },
            t => Err(format!("expected a schema type, got {t:?}")),
        }
    }

    fn blob_type(&mut self) -> Result<BlobDef, String> {
        match self.next()? {
            Tok::Ident(s) => match s.as_str() {
                "any" => Ok(BlobDef::Any),
                "string" => Ok(BlobDef::String),
                "int" => Ok(BlobDef::Int),
                "float" => Ok(BlobDef::Float),
                "bool" => Ok(BlobDef::Bool),
                "bytes" => Ok(BlobDef::Bytes),
                "array" => {
                    self.expect(Tok::Lt)?;
                    let of = Box::new(self.blob_type()?);
                    self.expect(Tok::Gt)?;
                    Ok(BlobDef::Array(of))
                }
                "map" => {
                    if matches!(self.peek(), Some(Tok::LBrace)) {
                        self.pos += 1; // {
                        let mut fields = Vec::new();
                        while !matches!(self.peek(), Some(Tok::RBrace)) {
                            let name = self.ident()?;
                            self.expect(Tok::Colon)?;
                            let t = self.blob_type()?;
                            fields.push((name, t));
                            if matches!(self.peek(), Some(Tok::Comma)) {
                                self.pos += 1;
                            }
                        }
                        self.expect(Tok::RBrace)?;
                        Ok(BlobDef::Closed(fields))
                    } else {
                        self.expect(Tok::Lt)?;
                        if self.map_has_key() {
                            let key = self.key_def()?;
                            self.expect(Tok::Comma)?;
                            let of = Box::new(self.blob_type()?);
                            self.expect(Tok::Gt)?;
                            Ok(BlobDef::Map { key, of })
                        } else {
                            let of = Box::new(self.blob_type()?);
                            self.expect(Tok::Gt)?;
                            Ok(BlobDef::Map { key: KeyDef::Any, of })
                        }
                    }
                }
                "record" => Err("record lives outside blobs; inside blob<…> use map { … }".into()),
                "list" | "deque" => {
                    Err(format!("{s} is keyspace syntax; inside blob<…> use array<…>"))
                }
                "leaf" => Err("leaf is keyspace syntax; inside blob<…> use any (leaf = blob<any>)".into()),
                "sum" => Err("sum lives outside blobs; inside blob<…> use int or float".into()),
                "blob" => Err("blob<…> does not nest; describe the CBOR shape directly".into()),
                other => Err(format!("unknown blob type {other:?}")),
            },
            t => Err(format!("expected a blob type, got {t:?}")),
        }
    }

    fn items(&mut self) -> Result<Vec<Item>, String> {
        let mut out = Vec::new();
        while self.peek().is_some() {
            if self.at_kw("schema") {
                self.eat_kw("schema")?;
                let fields = self.schema_fields()?;
                out.push(Item::Schema(SchemaDef::Record(fields)));
            } else if self.at_kw("server") {
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
                        let f = self.ident()?;
                        fields.push((f.clone(), f));
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
        // spawn expression: `spawn f(args)` evaluates to the child's handle
        if self.at_kw("spawn") {
            self.eat_kw("spawn")?;
            let e = self.expr()?;
            if !matches!(e, Expr::Call(..)) {
                return Err("spawn expects a function call".into());
            }
            return Ok(Expr::Spawn(Box::new(e)));
        }
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
        Expr::Spawn(e) => refs_expr(e, out),
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

/// Marks are legal anywhere a flow body reaches — branches, loops
/// (`while true` included), match arms — EXCEPT cast bodies: a cast is a
/// fire-and-forget message whose body already runs on the target, so a hop
/// inside it has no meaning.

/// Lambda bodies are separate flow bodies, checked when they compile.
fn check_no_cast_marks(ss: &[Stmt]) -> Result<(), String> {
    /// Inside a cast: no mark at any depth.
    fn reject_in(body: &[Stmt]) -> Result<(), String> {
        for st in body {
            if matches!(st, Stmt::Mark(_)) {
                return Err("placement marks are not allowed inside cast bodies".into());
            }
            match st {
                Stmt::If(_, t, e) => {
                    reject_in(t)?;
                    if let Some(e) = e {
                        reject_in(e)?;
                    }
                }
                Stmt::Match(_, arms) => {
                    for a in arms {
                        reject_in(&a.body)?;
                    }
                }
                Stmt::For(_, _, _, b) | Stmt::While(_, b) => reject_in(b)?,
                Stmt::Cast(_, b) => reject_in(b)?,
                _ => {}
            }
        }
        Ok(())
    }
    for s in ss {
        match s {
            Stmt::Cast(_, body) => reject_in(body)?,
            Stmt::If(_, t, e) => {
                check_no_cast_marks(t)?;
                if let Some(e) = e {
                    check_no_cast_marks(e)?;
                }
            }
            Stmt::Match(_, arms) => {
                for a in arms {
                    check_no_cast_marks(&a.body)?;
                }
            }
            Stmt::For(_, _, _, b) | Stmt::While(_, b) => check_no_cast_marks(b)?,
            _ => {}
        }
    }
    Ok(())
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
    explain: Vec<HopExplain>,
    warnings: Vec<String>,
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
    /// slot → source name, append-only, never truncated — the runtime
    /// trace's name table (hidden slots stay blank)
    dbg_names: Vec<String>,
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
            dbg_names: Vec::new(),
        }
    }

    fn def_local(&mut self, name: &str) -> u16 {
        let slot = self.n_locals;
        self.n_locals += 1;
        self.dbg_names.push(name.to_string());
        self.scope.push((name.to_string(), slot));
        slot
    }

    fn hidden_local(&mut self) -> u16 {
        let slot = self.n_locals;
        self.n_locals += 1;
        self.dbg_names.push(String::new());
        slot
    }

    fn lookup(&self, name: &str) -> Option<u16> {
        self.scope.iter().rev().find(|(n, _)| n == name).map(|(_, s)| *s)
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
        self.patch_to(at, self.code.len());
    }

    /// Patch a placeholder to jump to a known code position.
    fn patch_to(&mut self, at: usize, target: usize) {
        let d = target as i32 - (at as i32 + 1);
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
            locals_dbg: self.dbg_names,
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
        Expr::Spawn(e) => {
            // callee + args, then Spawn leaves the child flow id on the stack
            let Expr::Call(f, args) = e.as_ref() else {
                return Err("spawn expects a function call".into());
            };
            emit_expr(cg, fb, ctr, f)?;
            for a in args {
                emit_expr(cg, fb, ctr, a)?;
            }
            fb.emit(Instr::MakeArray(args.len() as u16));
            fb.emit(Instr::Spawn);
        }
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
    cg.explain.push(HopExplain {
        hop: id.to_string(),
        side: "cast".to_string(),
        ship: ship.iter().cloned().collect(),
        reply: Vec::new(),
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// CFG pipeline: AST → basic blocks → liveness → IR
//
// A body compiles to a graph of basic blocks, each tagged with the piece
// (same-side region) it belongs to. A placement mark is a CALL edge: the
// current piece parks at `At` and resumes when the far piece replies, so
// the frame — iterators, hidden slots, everything — never crosses the
// wire. Each far-side piece is a hop function (`prefix:N`) entered with
// exactly LiveIn(entry) as its vars map. A piece's exit replies with the
// caller-continuation's live vars that the piece itself defined (almost
// always none: cross-side state belongs in the store, and a flow's
// locals die with their frame).
//
// Marks alternate sides and nest like calls: a mark to the side of the
// piece that parked us ends the current piece (its reply resumes the
// parked frame); any other mark pushes a new piece. A piece's lexical
// extent is the rest of the enclosing block — when a block ends with
// pieces still open, they exit in LIFO order.
// ---------------------------------------------------------------------------

/// Alpha-rename a body: every binding (params, lets, loop k/v, match
/// field locals, lambda params) gets a unique name, so liveness and scope
/// analysis never confuse shadowed variables. Names are stable: the first
/// binding keeps its source name, later bindings of the same source name
/// get `#2`, `#3`, ... in preorder — programs without shadowing compile
/// to byte-identical wire maps. Match field KEYS are never renamed (they
/// name map fields, not variables).
struct Renamer {
    env: Vec<HashMap<String, String>>,
    seen: HashMap<String, usize>,
}

impl Renamer {
    fn new() -> Renamer {
        Renamer { env: vec![HashMap::new()], seen: HashMap::new() }
    }

    fn bind(&mut self, name: &str) -> String {
        let n = self.seen.entry(name.to_string()).or_insert(0);
        *n += 1;
        let fresh = if *n == 1 { name.to_string() } else { format!("{name}#{n}") };
        self.env
            .last_mut()
            .expect("base scope")
            .insert(name.to_string(), fresh.clone());
        fresh
    }

    fn resolve(&self, name: &str) -> String {
        for scope in self.env.iter().rev() {
            if let Some(f) = scope.get(name) {
                return f.clone();
            }
        }
        name.to_string() // unbound: a global (store, json, hui, ...)
    }

    fn push(&mut self) {
        self.env.push(HashMap::new());
    }

    fn pop(&mut self) {
        self.env.pop();
    }

    fn expr(&mut self, e: &Expr) -> Expr {
        match e {
            Expr::Num(_) | Expr::Str(_) | Expr::Bool(_) | Expr::Nil => e.clone(),
            Expr::Ident(n) => Expr::Ident(self.resolve(n)),
            Expr::Field(b, f) => Expr::Field(Box::new(self.expr(b)), f.clone()),
            Expr::Index(a, i) => Expr::Index(Box::new(self.expr(a)), Box::new(self.expr(i))),
            Expr::Call(f, args) => Expr::Call(
                Box::new(self.expr(f)),
                args.iter().map(|a| self.expr(a)).collect(),
            ),
            Expr::Spawn(e) => Expr::Spawn(Box::new(self.expr(e))),
            Expr::Unary(op, a) => Expr::Unary(op, Box::new(self.expr(a))),
            Expr::Binary(op, a, b) => {
                Expr::Binary(op, Box::new(self.expr(a)), Box::new(self.expr(b)))
            }
            Expr::Table(fs) => Expr::Table(
                fs.iter().map(|(k, v)| (k.clone(), self.expr(v))).collect(),
            ),
            Expr::Array(items) => Expr::Array(items.iter().map(|v| self.expr(v)).collect()),
            Expr::Fn(params, body) => {
                self.push();
                let params: Vec<String> = params.iter().map(|p| self.bind(p)).collect();
                let body = self.stmts(body);
                self.pop();
                Expr::Fn(params, body)
            }
        }
    }

    fn stmts(&mut self, ss: &[Stmt]) -> Vec<Stmt> {
        ss.iter().map(|s| self.stmt(s)).collect()
    }

    fn stmt(&mut self, s: &Stmt) -> Stmt {
        match s {
            Stmt::Let(n, e) => {
                let e = self.expr(e);
                let n = self.bind(n);
                Stmt::Let(n, e)
            }
            Stmt::Assign(l, r) => Stmt::Assign(self.expr(l), self.expr(r)),
            Stmt::Return(e) => Stmt::Return(e.as_ref().map(|e| self.expr(e))),
            Stmt::If(c, t, els) => {
                let c = self.expr(c);
                self.push();
                let t = self.stmts(t);
                self.pop();
                let els = els.as_ref().map(|es| {
                    self.push();
                    let es = self.stmts(es);
                    self.pop();
                    es
                });
                Stmt::If(c, t, els)
            }
            Stmt::Match(subject, arms) => {
                let subject = self.expr(subject);
                let arms = arms
                    .iter()
                    .map(|arm| {
                        self.push();
                        let fields = arm
                            .fields
                            .iter()
                            .map(|(key, _)| (key.clone(), self.bind(key)))
                            .collect();
                        let body = self.stmts(&arm.body);
                        self.pop();
                        MatchArm { tag: arm.tag.clone(), fields, body }
                    })
                    .collect();
                Stmt::Match(subject, arms)
            }
            Stmt::For(k, v, e, body) => {
                let e = self.expr(e);
                self.push();
                let k = self.bind(k);
                let v = self.bind(v);
                let body = self.stmts(body);
                self.pop();
                Stmt::For(k, v, e, body)
            }
            Stmt::While(c, body) => {
                let c = self.expr(c);
                self.push();
                let body = self.stmts(body);
                self.pop();
                Stmt::While(c, body)
            }
            Stmt::Mark(side) => Stmt::Mark(*side),
            Stmt::Cast(target, body) => {
                let target = match target {
                    CastTarget::Browsers => CastTarget::Browsers,
                    CastTarget::Server => CastTarget::Server,
                    CastTarget::Session(e) => CastTarget::Session(self.expr(e)),
                    CastTarget::User(e) => CastTarget::User(self.expr(e)),
                };
                self.push();
                let body = self.stmts(body);
                self.pop();
                Stmt::Cast(target, body)
            }
            Stmt::Spawn(e) => Stmt::Spawn(self.expr(e)),
            Stmt::Expr(e) => Stmt::Expr(self.expr(e)),
        }
    }
}

/// A straight-line statement inside a basic block: no control flow, no
/// marks. Loop/match machinery whose hidden slots must be allocated at
/// emission time is spelled out symbolically by id.
#[derive(Debug, Clone)]
enum Simple {
    Let(String, Expr),
    Assign(Expr, Expr),
    Expr(Expr),
    Spawn(Expr),
    Cast(CastTarget, Vec<Stmt>),
    /// evaluate the collection, IterNew, zero the index (loop id-keyed)
    ForInit(usize, Expr),
    /// evaluate the subject into hidden subj/tag slots (match id-keyed)
    MatchHead(usize, Expr),
    /// destructure an arm's (key, local) fields from the hidden subject
    MatchFields(usize, Vec<(String, String)>),
}

#[derive(Debug, Clone)]
enum Term {
    /// builder's unsealed tail (never emitted: sealed or dead)
    Open,
    Jump(usize),
    Branch(Expr, usize, usize),
    ForNext {
        loop_id: usize,
        k: String,
        v: String,
        body: usize,
        end: usize,
    },
    /// arms are (tag, entry block) in source order; `fall` catches a miss
    Match {
        id: usize,
        arms: Vec<(Option<String>, usize)>,
        fall: usize,
    },
    /// a placement mark: call the far piece, resume at `cont`
    Mark { side: Side, piece: usize, cont: usize },
    Return(Option<Expr>),
    /// piece tail: reply to the parked caller (or end the flow)
    Exit,
}

#[derive(Debug)]
struct Block {
    piece: usize,
    stmts: Vec<Simple>,
    term: Term,
}

#[derive(Debug)]
struct Piece {
    side: Side,
    entry: usize,
    hop_id: String,
}

#[derive(Debug)]
struct Cfg {
    blocks: Vec<Block>,
    pieces: Vec<Piece>, // pieces[0] is the origin function
}

struct CfgBuilder {
    blocks: Vec<Block>,
    pieces: Vec<Piece>,
    prefix: String,
    piece_n: usize,
    loop_n: usize,
    match_n: usize,
    /// (piece, resume block) parked by each open mark, innermost last
    stack: Vec<(usize, usize)>,
    /// stack depth at entry to the innermost lexical statement list. A mark
    /// back to the parked side may only *close* a piece opened inside that
    /// list; below the floor it opens a nested piece instead (see `mark`).
    floor: usize,
}

impl CfgBuilder {
    fn new(prefix: &str) -> CfgBuilder {
        CfgBuilder {
            blocks: vec![Block { piece: 0, stmts: vec![], term: Term::Open }],
            pieces: vec![Piece {
                side: Side::Browser,
                entry: 0,
                hop_id: String::new(),
            }],
            prefix: prefix.to_string(),
            piece_n: 0,
            loop_n: 0,
            match_n: 0,
            stack: vec![],
            floor: 0,
        }
    }

    fn block(&mut self, piece: usize) -> usize {
        self.blocks.push(Block { piece, stmts: vec![], term: Term::Open });
        self.blocks.len() - 1
    }

    fn seal(&mut self, b: usize, t: Term) {
        if matches!(self.blocks[b].term, Term::Open) {
            self.blocks[b].term = t;
        }
    }

    fn build(mut self, body: &[Stmt]) -> Result<Cfg, String> {
        let tail = self.stmts(body, 0, 0)?;
        self.seal(tail, Term::Exit);
        // dead tails (code after `return`) seal Exit too; reachability
        // skips them at emission
        for i in 0..self.blocks.len() {
            self.seal(i, Term::Exit);
        }
        Ok(Cfg { blocks: self.blocks, pieces: self.pieces })
    }

    /// Build stmts into block `cur` (in piece `p`); returns the open tail.
    /// When this lexical block ends, pieces opened inside it exit in LIFO
    /// order and control resumes in the caller's piece.
    fn stmts(&mut self, ss: &[Stmt], cur0: usize, p0: usize) -> Result<usize, String> {
        let depth = self.stack.len();
        let outer_floor = std::mem::replace(&mut self.floor, depth);
        let r = self.stmts_in(ss, cur0, p0, depth);
        self.floor = outer_floor;
        r
    }

    fn stmts_in(&mut self, ss: &[Stmt], cur0: usize, p0: usize, depth: usize) -> Result<usize, String> {
        let mut cur = cur0;
        let mut p = p0;
        for s in ss {
            match s {
                Stmt::Let(n, e) => self.blocks[cur].stmts.push(Simple::Let(n.clone(), e.clone())),
                Stmt::Assign(l, r) => {
                    self.blocks[cur].stmts.push(Simple::Assign(l.clone(), r.clone()))
                }
                Stmt::Expr(e) => self.blocks[cur].stmts.push(Simple::Expr(e.clone())),
                Stmt::Spawn(e) => self.blocks[cur].stmts.push(Simple::Spawn(e.clone())),
                Stmt::Cast(t, body) => {
                    self.blocks[cur].stmts.push(Simple::Cast(t.clone(), body.clone()))
                }
                Stmt::Return(e) => {
                    self.blocks[cur].term = Term::Return(e.clone());
                    cur = self.block(p); // dead continuation, never reached
                }
                Stmt::If(c, t, els) => {
                    let then_b = self.block(p);
                    let join = self.block(p);
                    let else_b = if els.is_some() { self.block(p) } else { join };
                    self.blocks[cur].term = Term::Branch(c.clone(), then_b, else_b);
                    let t = self.stmts(t, then_b, p)?;
                    self.seal(t, Term::Jump(join));
                    if let Some(els) = els {
                        let e = self.stmts(els, else_b, p)?;
                        self.seal(e, Term::Jump(join));
                    }
                    cur = join;
                }
                Stmt::While(c, body) => {
                    let header = self.block(p);
                    self.seal(cur, Term::Jump(header));
                    let body_b = self.block(p);
                    let end = self.block(p);
                    self.blocks[header].term = Term::Branch(c.clone(), body_b, end);
                    let t = self.stmts(body, body_b, p)?;
                    self.seal(t, Term::Jump(header));
                    cur = end;
                }
                Stmt::For(k, v, e, body) => {
                    let loop_id = self.loop_n;
                    self.loop_n += 1;
                    self.blocks[cur].stmts.push(Simple::ForInit(loop_id, e.clone()));
                    let header = self.block(p);
                    self.seal(cur, Term::Jump(header));
                    let body_b = self.block(p);
                    let end = self.block(p);
                    self.blocks[header].term = Term::ForNext {
                        loop_id,
                        k: k.clone(),
                        v: v.clone(),
                        body: body_b,
                        end,
                    };
                    let t = self.stmts(body, body_b, p)?;
                    self.seal(t, Term::Jump(header));
                    cur = end;
                }
                Stmt::Match(subject, arms) => {
                    let id = self.match_n;
                    self.match_n += 1;
                    self.blocks[cur].stmts.push(Simple::MatchHead(id, subject.clone()));
                    let join = self.block(p);
                    let mut arm_entries = Vec::new();
                    for arm in arms {
                        let entry = self.block(p);
                        if !arm.fields.is_empty() {
                            self.blocks[entry]
                                .stmts
                                .push(Simple::MatchFields(id, arm.fields.clone()));
                        }
                        let t = self.stmts(&arm.body, entry, p)?;
                        self.seal(t, Term::Jump(join));
                        arm_entries.push((arm.tag.clone(), entry));
                    }
                    self.blocks[cur].term = Term::Match { id, arms: arm_entries, fall: join };
                    cur = join;
                }
                Stmt::Mark(side) => {
                    let (c2, p2) = self.mark(*side, cur, p)?;
                    cur = c2;
                    p = p2;
                }
            }
        }
        // block boundary: pieces opened inside exit, innermost first
        while self.stack.len() > depth {
            let (_, cont) = self.stack.pop().expect("depth");
            self.seal(cur, Term::Exit);
            cur = cont;
        }
        Ok(cur)
    }

    /// A placement mark. Marks alternate sides: a mark to the side of the
    /// piece that parked us ends this piece (its reply resumes the parked
    /// frame); any other mark parks the current piece and opens a new one.
    ///
    /// Ending a piece is only sound when that piece was opened inside the
    /// current lexical statement list (stack deeper than `floor`). A branch,
    /// arm, or loop body entered *in* piece `p` must rejoin `p` — its join
    /// block or loop header lives there — so a mark back to the parked side
    /// inside such a body opens a nested piece instead: `p` stays parked,
    /// the body runs over there, and when the body ends (or marks back) the
    /// nested piece replies and control is home again in `p`. Closing `p`
    /// there would strand the join/back-edge in a piece that already
    /// replied (the old "no entry found for key" emitter panic).
    fn mark(&mut self, side: Side, cur: usize, p: usize) -> Result<(usize, usize), String> {
        if side == self.pieces[p].side {
            return Err(format!("{}: mark to the side already executing", self.prefix));
        }
        if self.stack.len() > self.floor {
            let &(parent, cont) = self.stack.last().expect("stack deeper than floor");
            if self.pieces[parent].side == side {
                self.stack.pop();
                self.seal(cur, Term::Exit);
                return Ok((cont, parent));
            }
        }
        let cont = self.block(p);
        let idx = self.pieces.len();
        self.blocks[cur].term = Term::Mark { side, piece: idx, cont };
        self.piece_n += 1;
        self.pieces.push(Piece {
            side,
            entry: 0, // fixed up just below
            hop_id: format!("{}:{}", self.prefix, self.piece_n),
        });
        let entry = self.block(idx);
        self.pieces[idx].entry = entry;
        self.stack.push((p, cont));
        Ok((entry, idx))
    }
}

fn term_succs(cfg: &Cfg, t: &Term) -> Vec<usize> {
    match t {
        Term::Open => vec![],
        Term::Jump(t) => vec![*t],
        Term::Branch(_, a, b) => vec![*a, *b],
        Term::ForNext { body, end, .. } => vec![*body, *end],
        Term::Match { arms, fall, .. } => {
            let mut v: Vec<usize> = arms.iter().map(|(_, e)| *e).collect();
            v.push(*fall);
            v
        }
        // the call edge to the far piece AND the resume edge both matter:
        // what the far piece needs must be live here
        Term::Mark { piece, cont, .. } => vec![cfg.pieces[*piece].entry, *cont],
        Term::Return(_) | Term::Exit => vec![],
    }
}

fn preds(cfg: &Cfg) -> Vec<Vec<usize>> {
    let mut p = vec![Vec::new(); cfg.blocks.len()];
    for (i, b) in cfg.blocks.iter().enumerate() {
        for s in term_succs(cfg, &b.term) {
            p[s].push(i);
        }
    }
    p
}

/// Blocks of one piece in reverse postorder from its entry — unreachable
/// (dead) blocks are absent. Deterministic, so hop ids and layouts are
/// stable byte for byte.
fn rpo(cfg: &Cfg, piece: usize) -> Vec<usize> {
    let mut seen = vec![false; cfg.blocks.len()];
    let mut post = Vec::new();
    let mut stack = vec![(cfg.pieces[piece].entry, false)];
    while let Some((b, expanded)) = stack.pop() {
        if expanded {
            post.push(b);
            continue;
        }
        if seen[b] {
            continue;
        }
        seen[b] = true;
        stack.push((b, true));
        for s in term_succs(cfg, &cfg.blocks[b].term) {
            if cfg.blocks[s].piece == piece && !seen[s] {
                stack.push((s, false));
            }
        }
    }
    post.reverse();
    post
}

/// Backwards liveness over the whole graph, pieces included: for any
/// block, the exact set of variables that must exist when it runs.
struct Liveness {
    live_in: Vec<BTreeSet<String>>,
    defs: Vec<BTreeSet<String>>,
}

fn note_uses(e: &Expr, def: &BTreeSet<String>, use_: &mut BTreeSet<String>) {
    let mut r = BTreeSet::new();
    refs_expr(e, &mut r);
    for v in r {
        if !def.contains(&v) {
            use_.insert(v);
        }
    }
}

/// Use = read before any write in the block; Def = written anywhere in
/// the block. Statements walk in order; mutation through a shared
/// reference (push, field/index writes) reads the binding, never rebinds
/// it — it counts as a use. Shipping is call-by-copy, so a mutated copy
/// rides the reply home: the reply set is LiveIn(resume) ∩ (defs ∪ ship).
fn block_use_def(b: &Block, use_: &mut BTreeSet<String>, def: &mut BTreeSet<String>) {
    for st in &b.stmts {
        match st {
            Simple::Let(n, e) => {
                note_uses(e, def, use_);
                def.insert(n.clone());
            }
            Simple::Assign(l, r) => {
                match l {
                    Expr::Ident(_) => {}
                    Expr::Field(base, _) => note_uses(base, def, use_),
                    Expr::Index(a, i) => {
                        note_uses(a, def, use_);
                        note_uses(i, def, use_);
                    }
                    _ => {}
                }
                note_uses(r, def, use_);
                if let Expr::Ident(n) = l {
                    def.insert(n.clone());
                }
            }
            Simple::Expr(e) | Simple::Spawn(e) | Simple::ForInit(_, e) | Simple::MatchHead(_, e) => {
                note_uses(e, def, use_)
            }
            Simple::Cast(t, body) => {
                if let CastTarget::Session(e) | CastTarget::User(e) = t {
                    note_uses(e, def, use_);
                }
                // captured vars are read here, at the send site
                let mut r = BTreeSet::new();
                refs_stmts(body, &mut r);
                for v in r {
                    if !def.contains(&v) {
                        use_.insert(v);
                    }
                }
            }
            Simple::MatchFields(_, fields) => {
                for (_, local) in fields {
                    def.insert(local.clone());
                }
            }
        }
    }
    match &b.term {
        Term::Branch(c, _, _) => note_uses(c, def, use_),
        Term::Return(Some(e)) => note_uses(e, def, use_),
        Term::ForNext { k, v, .. } => {
            def.insert(k.clone());
            def.insert(v.clone());
        }
        _ => {}
    }
}

fn liveness(cfg: &Cfg) -> Liveness {
    let n = cfg.blocks.len();
    let mut uses: Vec<BTreeSet<String>> = vec![BTreeSet::new(); n];
    let mut defs: Vec<BTreeSet<String>> = vec![BTreeSet::new(); n];
    for (i, b) in cfg.blocks.iter().enumerate() {
        block_use_def(b, &mut uses[i], &mut defs[i]);
    }
    let mut live_in: Vec<BTreeSet<String>> = vec![BTreeSet::new(); n];
    let mut live_out: Vec<BTreeSet<String>> = vec![BTreeSet::new(); n];
    loop {
        let mut changed = false;
        for i in (0..n).rev() {
            let mut out = BTreeSet::new();
            for s in term_succs(cfg, &cfg.blocks[i].term) {
                out.extend(live_in[s].iter().cloned());
            }
            let mut inn = uses[i].clone();
            for v in out.difference(&defs[i]) {
                inn.insert(v.clone());
            }
            if inn != live_in[i] || out != live_out[i] {
                live_in[i] = inn;
                live_out[i] = out;
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    Liveness { live_in, defs }
}

/// Per-piece emission state, shared so hop ids and consts stay stable.
struct Px<'a> {
    cg: &'a mut Cg,
    ctr: &'a mut Counters,
    cfg: &'a Cfg,
    live_in: &'a [BTreeSet<String>],
    /// all names defined anywhere in the piece (reply must carry them home)
    piece_defs: Vec<BTreeSet<String>>,
    /// every locally-bound name in the whole function (warning analysis)
    all_defs: BTreeSet<String>,
    /// the function's own variables: params/captures and every `let`,
    /// loop, and match-field binding. Such a name is a local in every
    /// piece — never a VM global — even where it did not ship.
    bound: BTreeSet<String>,
    reply_vars: Vec<BTreeSet<String>>,
    /// pieces that can return: contain a `return`, or mark into a piece
    /// that can. Their replies carry a `ret` tag so a far piece's return
    /// stops the caller instead of resuming it.
    can_ret: Vec<bool>,
    /// vars map each hop piece expects, recorded when its mark is emitted
    ship: Vec<BTreeSet<String>>,
    preds: Vec<Vec<usize>>,
    loop_slots: HashMap<usize, (u16, u16)>,
    match_slots: HashMap<usize, (u16, u16)>,
    /// every local's slot in the piece currently emitting (reply wraps
    /// load by slot, past the arm/branch scopes that defined them)
    slots: HashMap<String, u16>,
    addr: HashMap<usize, usize>,
    fixups: Vec<(usize, usize)>,
}

/// Emit a flow body into `fb` (a named fn or a lambda's piece 0): build
/// the CFG, run liveness, then emit each piece — piece 0 into `fb`, hop
/// pieces as separate functions registered under their hop ids. Pieces
/// emit in creation order, so a piece's vars map is always recorded (at
/// its mark, in the parent) before its prologue runs.
fn emit_flow_body(
    cg: &mut Cg,
    fb: &mut Fb,
    ctr: &mut Counters,
    prefix: &str,
    body: &[Stmt],
) -> Result<(), String> {
    check_no_cast_marks(body)?;
    let cfg = CfgBuilder::new(prefix).build(body)?;
    let la = liveness(&cfg);
    let mut piece_defs: Vec<BTreeSet<String>> = vec![BTreeSet::new(); cfg.pieces.len()];
    let mut all_defs: BTreeSet<String> = BTreeSet::new();
    for (i, b) in cfg.blocks.iter().enumerate() {
        for v in &la.defs[i] {
            piece_defs[b.piece].insert(v.clone());
            all_defs.insert(v.clone());
        }
    }
    let mut bound: BTreeSet<String> = fb.scope.iter().map(|(n, _)| n.clone()).collect();
    for b in &cfg.blocks {
        for st in &b.stmts {
            match st {
                Simple::Let(n, _) => {
                    bound.insert(n.clone());
                }
                Simple::MatchFields(_, fields) => {
                    for (_, local) in fields {
                        bound.insert(local.clone());
                    }
                }
                _ => {}
            }
        }
        if let Term::ForNext { k, v, .. } = &b.term {
            bound.insert(k.clone());
            bound.insert(v.clone());
        }
    }
    // reply vars are computed at each mark, where the ship set exists:
    // LiveIn(resume) ∩ (defs of the piece ∪ what crossed into it).
    //
    // can_ret is computed up front instead: a piece can return if it
    // contains a `return`, or marks into a piece that can. Children
    // always have higher indices than their parents (assigned when the
    // mark is built), so one reverse pass covers nesting.
    let mut can_ret = vec![false; cfg.pieces.len()];
    for pi in (1..cfg.pieces.len()).rev() {
        for b in rpo(&cfg, pi) {
            match &cfg.blocks[b].term {
                Term::Return(_) => can_ret[pi] = true,
                Term::Mark { piece, .. } if can_ret[*piece] => can_ret[pi] = true,
                _ => {}
            }
        }
    }
    let mut px = Px {
        cg,
        ctr,
        cfg: &cfg,
        live_in: &la.live_in,
        piece_defs,
        all_defs,
        bound,
        reply_vars: vec![BTreeSet::new(); cfg.pieces.len()],
        can_ret,
        ship: vec![BTreeSet::new(); cfg.pieces.len()],
        preds: preds(&cfg),
        loop_slots: HashMap::new(),
        match_slots: HashMap::new(),
        slots: HashMap::new(),
        addr: HashMap::new(),
        fixups: Vec::new(),
    };
    for pi in 0..cfg.pieces.len() {
        if pi == 0 {
            emit_piece(&mut px, fb, 0)?;
        } else {
            let hop_id = cfg.pieces[pi].hop_id.clone();
            let mut pfb = Fb::new(&hop_id);
            emit_piece(&mut px, &mut pfb, pi)?;
            let fn_idx = px.cg.add_fn(pfb.into_function());
            px.cg.hops.insert(hop_id, fn_idx);
        }
    }
    // the lamp: record what crosses each hop, in textual mark order
    for (pi, p) in cfg.pieces.iter().enumerate().skip(1) {
        px.cg.explain.push(HopExplain {
            hop: p.hop_id.clone(),
            side: match p.side {
                Side::Server => "server",
                Side::Browser => "browser",
            }
            .to_string(),
            ship: px.ship[pi].iter().cloned().collect(),
            reply: px.reply_vars[pi].iter().cloned().collect(),
        });
    }
    Ok(())
}

fn emit_piece(px: &mut Px, fb: &mut Fb, pi: usize) -> Result<(), String> {
    px.loop_slots.clear();
    px.match_slots.clear();
    px.slots.clear();
    px.addr.clear();
    px.fixups.clear();

    let mut entry_scope: BTreeMap<String, u16> = BTreeMap::new();
    for (n, s) in &fb.scope {
        entry_scope.insert(n.clone(), *s);
        px.slots.insert(n.clone(), *s);
    }
    if pi > 0 {
        // hop function: one `__vars` parameter, destructured in the
        // prologue — exactly what the mark's At shipped
        fb.n_params = 1;
        let vars_slot = fb.def_local("__vars");
        let ship = px.ship[pi].clone();
        for v in &ship {
            fb.emit(Instr::LoadLocal(vars_slot));
            let k = px.cg.k_str(v);
            fb.emit(Instr::GetField(k));
            let slot = fb.def_local(v);
            fb.emit(Instr::StoreLocal(slot));
            entry_scope.insert(v.clone(), slot);
            px.slots.insert(v.clone(), slot);
        }
        // The function's own variables stay locals here even when they
        // didn't ship. A far-side write before any read (`status =
        // "saved"` with nothing reading `status` first) is not live on
        // entry, so it doesn't cross; without a local it would land in a
        // VM global, the reply would carry nothing, and the caller's
        // rebind would clobber her copy with nil.
        let own: Vec<String> = px.piece_defs[pi]
            .iter()
            .filter(|n| px.bound.contains(*n) && !ship.contains(*n))
            .cloned()
            .collect();
        for v in own {
            fb.emit(Instr::Nil);
            let slot = fb.def_local(&v);
            fb.emit(Instr::StoreLocal(slot));
            entry_scope.insert(v.clone(), slot);
            px.slots.insert(v.clone(), slot);
        }
    }

    let mut scope_out: HashMap<usize, BTreeMap<String, u16>> = HashMap::new();
    for b in rpo(px.cfg, pi) {
        // scope in = meet of the piece's emitted forward predecessors;
        // back edges are ignored, which is exactly the old scope-truncate
        // semantics (loop-body lets never reach the header)
        let mut sc: Option<BTreeMap<String, u16>> = None;
        for pred in &px.preds[b] {
            if px.cfg.blocks[*pred].piece != pi {
                continue;
            }
            if let Some(so) = scope_out.get(pred) {
                sc = Some(match sc {
                    None => so.clone(),
                    Some(mut acc) => {
                        acc.retain(|n, s| so.get(n) == Some(s));
                        acc
                    }
                });
            }
        }
        let mut sc = if b == px.cfg.pieces[pi].entry {
            entry_scope.clone()
        } else {
            sc.unwrap_or_default()
        };
        fb.scope = sc.iter().map(|(n, s)| (n.clone(), *s)).collect();
        px.addr.insert(b, fb.code.len());
        let stmts = px.cfg.blocks[b].stmts.clone();
        for st in &stmts {
            emit_simple(px, fb, st, &mut sc)?;
        }
        emit_term(px, fb, b, &mut sc)?;
        scope_out.insert(b, sc);
    }
    let fixups = std::mem::take(&mut px.fixups);
    for (at, target) in fixups {
        let t = px.addr[&target];
        fb.patch_to(at, t);
    }
    Ok(())
}

fn emit_simple(
    px: &mut Px,
    fb: &mut Fb,
    st: &Simple,
    scope: &mut BTreeMap<String, u16>,
) -> Result<(), String> {
    match st {
        Simple::Let(n, e) => {
            emit_expr(px.cg, fb, px.ctr, e)?;
            let slot = fb.def_local(n);
            fb.emit(Instr::StoreLocal(slot));
            scope.insert(n.clone(), slot);
            px.slots.insert(n.clone(), slot);
        }
        Simple::Assign(l, r) => match l {
            Expr::Ident(n) => {
                emit_expr(px.cg, fb, px.ctr, r)?;
                match fb.lookup(n) {
                    Some(slot) => fb.emit(Instr::StoreLocal(slot)),
                    None => {
                        let k = px.cg.k_str(n);
                        fb.emit(Instr::StoreGlobal(k));
                    }
                }
            }
            Expr::Field(b, f) => {
                emit_expr(px.cg, fb, px.ctr, b)?;
                emit_expr(px.cg, fb, px.ctr, r)?;
                let k = px.cg.k_str(f);
                fb.emit(Instr::SetField(k));
            }
            Expr::Index(a, i) => {
                emit_expr(px.cg, fb, px.ctr, a)?;
                emit_expr(px.cg, fb, px.ctr, i)?;
                emit_expr(px.cg, fb, px.ctr, r)?;
                fb.emit(Instr::SetIndex);
            }
            _ => return Err("invalid assignment target".into()),
        },
        Simple::Expr(e) => {
            emit_expr(px.cg, fb, px.ctr, e)?;
            fb.emit(Instr::Pop);
        }
        Simple::Spawn(e) => {
            let Expr::Call(f, args) = e else {
                return Err("spawn expects a function call".into());
            };
            emit_expr(px.cg, fb, px.ctr, f)?;
            for a in args {
                emit_expr(px.cg, fb, px.ctr, a)?;
            }
            fb.emit(Instr::MakeArray(args.len() as u16));
            fb.emit(Instr::Spawn);
            // statement position: discard the child handle
            fb.emit(Instr::Pop);
        }
        Simple::Cast(target, body) => {
            px.ctr.cast_n += 1;
            let id = format!("{}:c{}", px.ctr.fn_name, px.ctr.cast_n);
            // captured = referenced by body ∩ in scope here
            let mut refs = BTreeSet::new();
            refs_stmts(body, &mut refs);
            let captured: BTreeSet<String> = refs
                .into_iter()
                .filter(|n| fb.lookup(n).is_some())
                .collect();
            compile_segment(px.cg, px.ctr, &id, &captured, body, None)?;
            match target {
                CastTarget::Browsers => {
                    let k = px.cg.k_str("browsers");
                    fb.emit(Instr::Const(k));
                }
                CastTarget::Server => {
                    let k = px.cg.k_str("server");
                    fb.emit(Instr::Const(k));
                }
                CastTarget::Session(e) => emit_expr(px.cg, fb, px.ctr, e)?,
                CastTarget::User(e) => {
                    let k = px.cg.k_str("user:");
                    fb.emit(Instr::Const(k));
                    emit_expr(px.cg, fb, px.ctr, e)?;
                    fb.emit(Instr::BinOp(BinOp::Concat));
                }
            }
            emit_vars_map(px.cg, fb, &captured)?;
            let k = px.cg.k_str(&id);
            fb.emit(Instr::Cast(k));
        }
        Simple::ForInit(id, e) => {
            emit_expr(px.cg, fb, px.ctr, e)?;
            fb.emit(Instr::IterNew);
            let iter = fb.hidden_local();
            fb.emit(Instr::StoreLocal(iter));
            let zero = px.cg.k_int(0);
            fb.emit(Instr::Const(zero));
            let idx = fb.hidden_local();
            fb.emit(Instr::StoreLocal(idx));
            px.loop_slots.insert(*id, (iter, idx));
        }
        Simple::MatchHead(id, e) => {
            emit_expr(px.cg, fb, px.ctr, e)?;
            let subj = fb.hidden_local();
            fb.emit(Instr::StoreLocal(subj));
            fb.emit(Instr::LoadLocal(subj));
            let type_k = px.cg.k_str("type");
            fb.emit(Instr::GetField(type_k));
            let tag = fb.hidden_local();
            fb.emit(Instr::StoreLocal(tag));
            px.match_slots.insert(*id, (subj, tag));
        }
        Simple::MatchFields(id, fields) => {
            let (subj, _) = px.match_slots[id];
            for (key, local) in fields {
                fb.emit(Instr::LoadLocal(subj));
                let fk = px.cg.k_str(key);
                fb.emit(Instr::GetField(fk));
                let slot = fb.def_local(local);
                fb.emit(Instr::StoreLocal(slot));
                scope.insert(local.clone(), slot);
                px.slots.insert(local.clone(), slot);
            }
        }
    }
    Ok(())
}

/// The reply of a reply-carrying piece: `{ value, vars }` where vars are
/// the caller-continuation's live locals this piece defined. The value is
/// already on the stack. Pieces that can return tag it `{ ret, value,
/// vars }` instead — always wrapped, even when `vars` is empty — so the
/// mark can tell "the function returned" from "resume me".
fn emit_reply_wrap(px: &mut Px, fb: &mut Fb, pi: usize, ret: bool) -> Result<(), String> {
    let h = fb.hidden_local();
    fb.emit(Instr::StoreLocal(h));
    let mut n = 2;
    if px.can_ret[pi] {
        let k = px.cg.k_str("ret");
        fb.emit(Instr::Const(k));
        fb.emit(if ret { Instr::True } else { Instr::False });
        n = 3;
    }
    let value_k = px.cg.k_str("value");
    fb.emit(Instr::Const(value_k));
    fb.emit(Instr::LoadLocal(h));
    let vars_k = px.cg.k_str("vars");
    fb.emit(Instr::Const(vars_k));
    let rv = px.reply_vars[pi].clone();
    for v in &rv {
        let vk = px.cg.k_str(v);
        fb.emit(Instr::Const(vk));
        match px.slots.get(v) {
            Some(&s) => fb.emit(Instr::LoadLocal(s)),
            // defined on a path that didn't run: absent is nil, per the
            // value model
            None => fb.emit(Instr::Nil),
        }
    }
    fb.emit(Instr::MakeMap(rv.len() as u16));
    fb.emit(Instr::MakeMap(n));
    Ok(())
}

/// Rebind the reply vars from the wrapper already in local `h`.
fn emit_rebind(
    px: &mut Px,
    fb: &mut Fb,
    scope: &mut BTreeMap<String, u16>,
    h: u16,
    rv: &BTreeSet<String>,
) {
    let vars_k = px.cg.k_str("vars");
    for v in rv {
        fb.emit(Instr::LoadLocal(h));
        fb.emit(Instr::GetField(vars_k));
        let vk = px.cg.k_str(v);
        fb.emit(Instr::GetField(vk));
        let slot = match px.slots.get(v) {
            Some(&s) => s,
            None => {
                let s = fb.def_local(v);
                px.slots.insert(v.clone(), s);
                s
            }
        };
        fb.emit(Instr::StoreLocal(slot));
        scope.insert(v.clone(), slot);
    }
}

fn emit_term(
    px: &mut Px,
    fb: &mut Fb,
    b: usize,
    scope: &mut BTreeMap<String, u16>,
) -> Result<(), String> {
    let pi = px.cfg.blocks[b].piece;
    match px.cfg.blocks[b].term.clone() {
        Term::Open => unreachable!("the builder seals every tail"),
        Term::Jump(t) => {
            let j = fb.jump_placeholder(Instr::Jump);
            px.fixups.push((j, t));
        }
        Term::Branch(c, a, bb) => {
            emit_expr(px.cg, fb, px.ctr, &c)?;
            let jf = fb.jump_placeholder(Instr::JumpIfFalse);
            px.fixups.push((jf, bb));
            let j = fb.jump_placeholder(Instr::Jump);
            px.fixups.push((j, a));
        }
        Term::ForNext { loop_id, k, v, body, end } => {
            let (iter, idx) = px.loop_slots[&loop_id];
            let k_slot = fb.def_local(&k);
            let v_slot = fb.def_local(&v);
            scope.insert(k.clone(), k_slot);
            scope.insert(v.clone(), v_slot);
            px.slots.insert(k.clone(), k_slot);
            px.slots.insert(v.clone(), v_slot);
            fb.emit(Instr::LoadLocal(iter));
            fb.emit(Instr::LoadLocal(idx));
            let j = fb.jump_placeholder(Instr::IterNext);
            px.fixups.push((j, end));
            // stack after IterNext: iter idx' k v
            fb.emit(Instr::StoreLocal(v_slot));
            fb.emit(Instr::StoreLocal(k_slot));
            fb.emit(Instr::StoreLocal(idx));
            fb.emit(Instr::StoreLocal(iter));
            let j = fb.jump_placeholder(Instr::Jump);
            px.fixups.push((j, body));
        }
        Term::Match { id, arms, fall } => {
            let (_, tag_slot) = px.match_slots[&id];
            let n_tagged = arms.iter().filter(|(t, _)| t.is_some()).count();
            let else_entry = arms.iter().find(|(t, _)| t.is_none()).map(|(_, e)| *e);
            let mut seen = 0;
            for (tag, entry) in &arms {
                match tag {
                    Some(t) => {
                        seen += 1;
                        fb.emit(Instr::LoadLocal(tag_slot));
                        let k = px.cg.k_str(t);
                        fb.emit(Instr::Const(k));
                        fb.emit(Instr::BinOp(BinOp::Eq));
                        let jf = fb.jump_placeholder(Instr::JumpIfFalse);
                        let j = fb.jump_placeholder(Instr::Jump);
                        px.fixups.push((j, *entry));
                        if seen == n_tagged {
                            px.fixups.push((jf, else_entry.unwrap_or(fall)));
                        } else {
                            fb.patch_to_here(jf);
                        }
                    }
                    None => {
                        if n_tagged == 0 {
                            let j = fb.jump_placeholder(Instr::Jump);
                            px.fixups.push((j, *entry));
                        }
                    }
                }
            }
            if arms.is_empty() {
                let j = fb.jump_placeholder(Instr::Jump);
                px.fixups.push((j, fall));
            }
        }
        Term::Mark { side, piece, cont } => {
            emit_hop_target(px.cg, fb, side);
            let entry = px.cfg.pieces[piece].entry;
            // warn when a locally-bound name is live on the far side but
            // not in scope here: it can't ship, so it reads as nil/global
            // there — a confusing evening, made loud at compile time
            for n in px.live_in[entry].iter() {
                if px.all_defs.contains(n) && fb.lookup(n).is_none() {
                    px.cg.warnings.push(format!(
                        "{}: `{n}` is live on the far side but not in scope at the mark (defined on only some paths); it reads as nil there",
                        px.cfg.pieces[piece].hop_id
                    ));
                }
            }
            let ship: BTreeSet<String> = px.live_in[entry]
                .iter()
                .filter(|n| fb.lookup(n).is_some())
                .cloned()
                .collect();
            emit_vars_map(px.cg, fb, &ship)?;
            // the reply carries what the far piece rebinds and the caller
            // still needs: vars it DEFINED, plus anything that CROSSED —
            // shipping is call-by-copy, so a mutation through a shared
            // reference (push, field/index writes) must ride home
            let rv: BTreeSet<String> = px.live_in[cont]
                .iter()
                .filter(|n| px.piece_defs[piece].contains(*n) || ship.contains(*n))
                .cloned()
                .collect();
            px.reply_vars[piece] = rv.clone();
            px.ship[piece] = ship;
            let k = px.cg.k_str(&px.cfg.pieces[piece].hop_id);
            fb.emit(Instr::At(k));
            if px.can_ret[piece] {
                // the far piece may have returned: check the tag. On
                // `ret`, the origin unwraps the value and returns from the
                // whole function; a nested hop forwards the wrapper
                // verbatim up a level (its parent expects this shape
                // because can_ret propagated to it too). Otherwise resume
                // normally, rebinding from h.vars. The tail-mark shortcut
                // is skipped here: in a nested piece it would forward the
                // child's vars as if they were this piece's own.
                let h = fb.hidden_local();
                fb.emit(Instr::StoreLocal(h));
                // did the far side return?
                fb.emit(Instr::LoadLocal(h));
                let ret_k = px.cg.k_str("ret");
                fb.emit(Instr::GetField(ret_k));
                let j_resume = fb.jump_placeholder(Instr::JumpIfFalse);
                if pi == 0 {
                    // origin: unwrap and return from the whole function
                    fb.emit(Instr::LoadLocal(h));
                    let value_k = px.cg.k_str("value");
                    fb.emit(Instr::GetField(value_k));
                } else {
                    // nested hop: forward the wrapper verbatim, ret = true.
                    fb.emit(Instr::LoadLocal(h));
                }
                fb.emit(Instr::Return);
                fb.patch_to_here(j_resume);
                // normal resume: rebind reply vars from h.vars, then on
                emit_rebind(px, fb, scope, h, &rv);
                let j = fb.jump_placeholder(Instr::Jump);
                px.fixups.push((j, cont));
                return Ok(());
            }
            let cont_trivial = px.cfg.blocks[cont].stmts.is_empty()
                && matches!(px.cfg.blocks[cont].term, Term::Exit);
            let rv = px.reply_vars[piece].clone();
            if cont_trivial {
                // tail mark: the far piece's reply is this piece's
                // result, exactly like the old segment chain
                fb.emit(Instr::Return);
            } else if rv.is_empty() {
                fb.emit(Instr::Pop);
                let j = fb.jump_placeholder(Instr::Jump);
                px.fixups.push((j, cont));
            } else {
                // the reply is `{ value, vars }`: rebind the locals the
                // far piece defined that the continuation still needs
                let h = fb.hidden_local();
                fb.emit(Instr::StoreLocal(h));
                emit_rebind(px, fb, scope, h, &rv);
                let j = fb.jump_placeholder(Instr::Jump);
                px.fixups.push((j, cont));
            }
        }
        Term::Return(e) => {
            match &e {
                Some(e) => emit_expr(px.cg, fb, px.ctr, e)?,
                None => fb.emit(Instr::Nil),
            }
            let wrap = px.can_ret[pi] || !px.reply_vars[pi].is_empty();
            if pi > 0 && wrap {
                emit_reply_wrap(px, fb, pi, true)?;
            }
            fb.emit(Instr::Return);
        }
        Term::Exit => {
            let wrap = px.can_ret[pi] || !px.reply_vars[pi].is_empty();
            if pi > 0 && wrap {
                fb.emit(Instr::Nil);
                emit_reply_wrap(px, fb, pi, false)?;
            } else {
                // falling off = reply nil, same value as the old chain's
                // trailing segment
                fb.emit(Instr::Nil);
            }
            fb.emit(Instr::Return);
        }
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
                            for (key, local) in &arm.fields {
                                fb.emit(Instr::LoadLocal(subj_slot));
                                let fk = cg.k_str(key);
                                fb.emit(Instr::GetField(fk));
                                let slot = fb.def_local(local);
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
                unreachable!("casts reject marks at compile time; flow bodies use the CFG pipeline")
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
                // statement position: discard the child handle
                fb.emit(Instr::Pop);
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
    let mut schema: Option<SchemaDef> = None;

    for item in &items {
        match item {
            Item::Schema(def) => {
                if schema.is_some() {
                    return Err("duplicate schema block — declare schema { … } once".into());
                }
                schema = Some(def.clone());
            }
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
                let mut rn = Renamer::new();
                for p in params {
                    rn.bind(p);
                }
                let body = rn.stmts(body);
                emit_flow_body(&mut cg, &mut fb, &mut ctr, name, &body)?;
                let fn_idx = cg.add_fn(fb.into_function());
                cg.named.insert(name.clone(), fn_idx);
            }
        }
    }

    if schema.is_some() && cg.server_lets.iter().any(|(n, _)| n == "schema") {
        return Err(
            "schema { … } conflicts with `server let schema` — use one or the other".into(),
        );
    }

    Ok(Program {
        consts: cg.consts,
        fns: cg.fns,
        hops: cg.hops,
        named: cg.named,
        server_lets: cg.server_lets,
        schema,
        explain: cg.explain,
        warnings: cg.warnings,
    })
}
