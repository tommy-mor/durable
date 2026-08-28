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
    /// closure → ()  (new flow on this VM)
    Spawn,
    /// → session identity (browser: own sid; server: flow origin)
    Session,
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
}

#[derive(Clone, Default)]
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
