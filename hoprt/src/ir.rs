//! Hop IR — a stack machine the compiler targets and the interpreter runs.
//!
//! A program is a set of functions. Named functions are `.hop` fns;
//! segment functions carry the stable hop ids (`add_todo:1`, `f:l1:2`,
//! `f:c1`) minted by mark-splitting — the same ids as the Lua era, so the
//! wire grammar is unchanged. All VMs load the identical program; the
//! wire carries ids and data, never code.

use std::collections::HashMap;

use crate::value::Value;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum UnOp {
    Not,
    Neg,
}

#[derive(Clone, Debug)]
pub enum Instr {
    /// Push a constant from the pool.
    Const(u32),
    Nil,
    True,
    False,
    LoadLocal(u16),
    StoreLocal(u16),
    /// Globals are named; the name is a string constant.
    LoadGlobal(u32),
    StoreGlobal(u32),
    /// Pop n values → array (in push order).
    MakeArray(u16),
    /// Pop n (key, value) pairs → map. Scalar-key rule enforced.
    MakeMap(u16),
    /// obj key → value  (arrays: 0-based int; maps: scalar key; nil when absent)
    GetIndex,
    /// obj key value → ()  (append at len grows an array; nil deletes a map key)
    SetIndex,
    /// obj → value  (map field by string constant)
    GetField(u32),
    /// obj value → ()
    SetField(u32),
    BinOp(BinOp),
    UnOp(UnOp),
    Jump(i32),
    JumpIfFalse(i32),
    Dup,
    Pop,
    /// callee a1..an → result
    Call(u8),
    /// caps on stack (ncaps of them) → closure over fns[fn_idx]
    Closure(u32, u8),
    /// iterable → iter
    IterNew,
    /// iter → iter k v, or jump(d) with iter popped when exhausted
    IterNext(i32),
    Return,
    /// target vars → (suspend; reply value is pushed on resume).
    /// The hop id is a string constant.
    At(u32),
    /// target vars → ()  (fire and forget)
    Cast(u32),
    /// key callee args-array → ()  (birth the named flow, replacing same key)
    Flow,
    /// → session identity (browser: own sid; server: flow origin)
    Session,
    /// → user identity (browser: own user; server: origin's user)
    User,
}

#[derive(Clone, Debug)]
pub struct Function {
    pub name: String,
    /// captured values become locals [0..n_caps)
    pub n_caps: u8,
    /// params become locals [n_caps..n_caps+n_params)
    pub n_params: u8,
    pub n_locals: u16,
    pub code: Vec<Instr>,
    /// slot → source name for the runtime trace (blank for hidden slots)
    pub locals_dbg: Vec<String>,
}

/// What crosses one hop, recorded at compile time (hopc --explain):
/// the vars a piece expects coming in, and the locals it replies with
/// when the caller's continuation still needs them.
#[derive(Clone, Debug)]
pub struct HopExplain {
    /// hop id of the piece (`add_todo:1`, `f:c1` for casts)
    pub hop: String,
    /// "server", "browser", or "cast"
    pub side: String,
    /// the vars map the piece expects, in wire order
    pub ship: Vec<String>,
    /// locals the piece replies with (almost always empty)
    pub reply: Vec<String>,
}

/// Store schema as a core language construct (`schema { ... }`).
///
/// This is the compile-time form of what `server let schema =
/// store.record(...)` used to build as runtime data. Same key layout, but
/// field names are identifiers checked by the parser, nesting is grammar
/// instead of nested calls, and the two CBOR-wins rules hold: `record`
/// is a closed map (never a value kind), and blobs describe CBOR kinds
/// (`map` inside, never `record`).
#[derive(Clone, Debug, PartialEq)]
pub enum SchemaDef {
    Leaf,
    Sum,
    Map { key: KeyDef, of: Box<SchemaDef> },
    List(Box<SchemaDef>),
    Deque(Box<SchemaDef>),
    Record(Vec<(String, SchemaDef)>),
    Blob(BlobDef),
}

/// A map's key constraint. `Any` is today's behavior (keys are dynamic
/// scalars); a named kind is validated on write and documents intent.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyDef {
    Any,
    String,
    Int,
    Float,
    Bool,
    Bytes,
}

/// A blob's CBOR shape: the value language inside one opaque key. `Any`
/// is `leaf` spelled long. Closed maps name their known string keys;
/// absent fields read as `nil` either way, extras are tolerated (forward
/// compat — a writer from a newer app must not brick an older reader).
/// `nil` is always an accepted value for any blob type: absence and
/// nil-value are one thing, matching the value model.
#[derive(Clone, Debug, PartialEq)]
pub enum BlobDef {
    Any,
    String,
    Int,
    Float,
    Bool,
    Bytes,
    Array(Box<BlobDef>),
    Map { key: KeyDef, of: Box<BlobDef> },
    Closed(Vec<(String, BlobDef)>),
}

#[derive(Clone, Debug, Default)]
pub struct Program {
    pub consts: Vec<Value>,
    pub fns: Vec<Function>,
    /// hop id → segment function
    pub hops: HashMap<String, usize>,
    /// global name → named function (installed as globals at VM start)
    pub named: HashMap<String, usize>,
    /// `server let` initializers: (global name, thunk fn) — run on the
    /// server VM only, in declaration order.
    pub server_lets: Vec<(String, usize)>,
    /// `schema { ... }` block, if the app uses first-class schema syntax.
    /// Preferred over the legacy `server let schema = store.record(...)`
    /// data literal; `store::bind` uses this when present (and validates
    /// key constraints and blob shapes on write through it).
    pub schema: Option<SchemaDef>,
    /// per-hop shipping, in creation order (hopc --explain)
    pub explain: Vec<HopExplain>,
    /// non-fatal compile warnings (hopc prints them to stderr)
    pub warnings: Vec<String>,
}

impl Program {
    pub fn const_value(&self, k: u32) -> &Value {
        &self.consts[k as usize]
    }

    pub fn const_str(&self, k: u32) -> &str {
        match &self.consts[k as usize] {
            Value::Str(s) => s,
            other => panic!("const {k} is not a string: {other}"),
        }
    }

    /// hopc --explain: what crosses each hop, one line per hop function.
    pub fn explain_listing(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for e in &self.explain {
            let _ = writeln!(
                out,
                "{}  side={}  ship={{{}}}  reply={{{}}}",
                e.hop,
                e.side,
                e.ship.join(", "),
                e.reply.join(", ")
            );
        }
        out
    }

    /// One-line rendering of the declared store schema, if any.
    pub fn schema_listing(&self) -> Option<String> {
        self.schema.as_ref().map(|def| match def {
            // the top level is the `schema { … }` block itself
            SchemaDef::Record(fields) => {
                let inner = fields
                    .iter()
                    .map(|(n, t)| format!("{n}: {}", schema_render(t)))
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("schema {{ {inner} }}")
            }
            other => schema_render(other),
        })
    }

    /// Human-readable listing (hopc --dump, log mode).
    pub fn listing(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        for (i, f) in self.fns.iter().enumerate() {
            let _ = writeln!(
                out,
                "fn {i} {} (caps {}, params {}, locals {})",
                f.name, f.n_caps, f.n_params, f.n_locals
            );
            for (pc, instr) in f.code.iter().enumerate() {
                let note = match instr {
                    Instr::Const(k) => format!("  ; {}", self.consts[*k as usize]),
                    Instr::LoadGlobal(k) | Instr::StoreGlobal(k) => {
                        format!("  ; {}", self.const_str(*k))
                    }
                    Instr::GetField(k) | Instr::SetField(k) => {
                        format!("  ; .{}", self.const_str(*k))
                    }
                    Instr::At(k) | Instr::Cast(k) => format!("  ; {}", self.const_str(*k)),
                    _ => String::new(),
                };
                let _ = writeln!(out, "  {pc:4}  {instr:?}{note}");
            }
        }
        out
    }
}

fn key_render(k: &KeyDef) -> &'static str {
    match k {
        KeyDef::Any => "any",
        KeyDef::String => "string",
        KeyDef::Int => "int",
        KeyDef::Float => "float",
        KeyDef::Bool => "bool",
        KeyDef::Bytes => "bytes",
    }
}

fn blob_render(b: &BlobDef) -> String {
    match b {
        BlobDef::Any => "any".to_string(),
        BlobDef::String => "string".to_string(),
        BlobDef::Int => "int".to_string(),
        BlobDef::Float => "float".to_string(),
        BlobDef::Bool => "bool".to_string(),
        BlobDef::Bytes => "bytes".to_string(),
        BlobDef::Array(of) => format!("array<{}>", blob_render(of)),
        BlobDef::Map { key, of } => {
            if matches!(key, KeyDef::Any) {
                format!("map<{}>", blob_render(of))
            } else {
                format!("map<{}, {}>", key_render(key), blob_render(of))
            }
        }
        BlobDef::Closed(fields) => {
            let inner = fields
                .iter()
                .map(|(n, t)| format!("{n}: {}", blob_render(t)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("map {{ {inner} }}")
        }
    }
}

fn schema_render(def: &SchemaDef) -> String {
    match def {
        SchemaDef::Leaf => "leaf".to_string(),
        SchemaDef::Sum => "sum".to_string(),
        SchemaDef::Map { key, of } => {
            if matches!(key, KeyDef::Any) {
                format!("map<{}>", schema_render(of))
            } else {
                format!("map<{}, {}>", key_render(key), schema_render(of))
            }
        }
        SchemaDef::List(of) => format!("list<{}>", schema_render(of)),
        SchemaDef::Deque(of) => format!("deque<{}>", schema_render(of)),
        SchemaDef::Record(fields) => {
            let inner = fields
                .iter()
                .map(|(n, t)| format!("{n}: {}", schema_render(t)))
                .collect::<Vec<_>>()
                .join(", ");
            format!("record {{ {inner} }}")
        }
        SchemaDef::Blob(b) => format!("blob<{}>", blob_render(b)),
    }
}
