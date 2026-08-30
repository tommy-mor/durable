//! Hop modules — the library seam.
//!
//! A module is a bag of named natives: pure Rust functions, or effect
//! declarations that suspend the flow and run on the platform. Modules
//! mount into every VM's globals by name ("bash" becomes a global,
//! "json.first" becomes field `first` on global map `json`), and calls
//! dispatch by that name — names, unlike enum discriminants, cannot
//! drift between the server build and the wasm build.
//!
//! `NativeId` remains the language core (control, arrays, store, dom,
//! hui). Modules are the batteries: adding a pure native is one entry
//! here; adding an effect is one entry plus an executor in hopd
//! (serve.rs) and a fake in the harness.
//!
//! The kind carries the policy. Pure natives run in the interpreter on
//! either side and are legal in reducers. Effects are server-side only
//! and never legal in a reducer — the gates fall out of the declaration
//! instead of hand-maintained match arms.

use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;

use crate::interp::Globals;
use crate::value::Value;

pub type PureFn = fn(Vec<Value>) -> Result<Value, String>;
/// Shapes call arguments into the `vars` map of an `@effects` packet.
pub type VarsFn = fn(Vec<Value>) -> Result<Value, String>;

pub enum NativeKind {
    /// Runs inside the interpreter, on any side; legal in reducers.
    Pure(PureFn),
    /// Suspends the flow; the platform executes and replies. `hop` names
    /// the executor (the packet's hop field) in hopd and the harness.
    Effect { hop: &'static str, vars: VarsFn },
}

pub struct NativeDef {
    /// Surface name and dispatch key: "bash", "json.first", "llm.stream".
    pub name: &'static str,
    pub kind: NativeKind,
}

pub struct Module {
    pub name: &'static str,
    pub natives: Vec<NativeDef>,
}

/// The compiled-in battery set.
fn builtin_modules() -> Vec<Module> {
    vec![json_module(), markdown_module(), str_module(), rand_module(), bash_module(), llm_module()]
}

/// Name → kind, for call dispatch. Built once per process; identical in
/// the server and wasm builds because it is the same code.
pub fn registry() -> &'static HashMap<&'static str, NativeKind> {
    static REG: OnceLock<HashMap<&'static str, NativeKind>> = OnceLock::new();
    REG.get_or_init(|| {
        let mut m = HashMap::new();
        for module in builtin_modules() {
            for def in module.natives {
                assert!(
                    m.insert(def.name, def.kind).is_none(),
                    "duplicate native name {}",
                    def.name
                );
            }
        }
        m
    })
}

/// Mount every module's natives into a VM's globals. Dotted names
/// collect into a map under their head: json.first → globals "json".
pub fn install(globals: &mut Globals) {
    let mut mounts: HashMap<&'static str, BTreeMap<Value, Value>> = HashMap::new();
    for name in registry().keys() {
        match name.split_once('.') {
            Some((head, field)) => {
                mounts
                    .entry(head)
                    .or_default()
                    .insert(Value::str(field), Value::Lib(name));
            }
            None => globals.set(*name, Value::Lib(name)),
        }
    }
    for (head, fields) in mounts {
        globals.set(head, Value::map(fields));
    }
}

// ---------------------------------------------------------------------------
// json — encode/decode, and the forgiving scan for a model's tool call
// ---------------------------------------------------------------------------

fn json_module() -> Module {
    Module {
        name: "json",
        natives: vec![
            NativeDef { name: "json.encode", kind: NativeKind::Pure(json_encode) },
            NativeDef { name: "json.decode", kind: NativeKind::Pure(json_decode) },
            NativeDef { name: "json.first", kind: NativeKind::Pure(json_first) },
        ],
    }
}

/// json.encode(v) → string.
fn json_encode(args: Vec<Value>) -> Result<Value, String> {
    let v = args.first().cloned().unwrap_or(Value::Nil);
    let j = crate::value::to_json(&v)?;
    serde_json::to_string(&j).map(Value::str).map_err(|e| e.to_string())
}

/// json.decode(s) → value, nil if unparsable (absence = nil, per the
/// value model — parse failure is data).
fn json_decode(args: Vec<Value>) -> Result<Value, String> {
    match args.first().and_then(Value::as_str) {
        Some(s) => Ok(match serde_json::from_str::<serde_json::Value>(s) {
            Ok(j) => crate::value::from_json(&j),
            Err(_) => Value::Nil,
        }),
        None => Err("json.decode expects a string".into()),
    }
}

/// json.first(s) → the first JSON object embedded anywhere in s (prose,
/// code fences, and trailing objects are ignored), or nil. The forgiving
/// side of the decode family: made for fishing a tool call out of a
/// model's reply.
fn json_first(args: Vec<Value>) -> Result<Value, String> {
    match args.first().and_then(Value::as_str) {
        Some(s) => {
            let mut found = Value::Nil;
            for (i, c) in s.char_indices() {
                if c != '{' {
                    continue;
                }
                let mut it =
                    serde_json::Deserializer::from_str(&s[i..]).into_iter::<serde_json::Value>();
                if let Some(Ok(j)) = it.next() {
                    found = crate::value::from_json(&j);
                    break;
                }
            }
            Ok(found)
        }
        None => Err("json.first expects a string".into()),
    }
}

// ---------------------------------------------------------------------------
// markdown — model output as a hiccup tree (text stays text; hui escapes)
// ---------------------------------------------------------------------------

fn markdown_module() -> Module {
    Module {
        name: "markdown",
        natives: vec![NativeDef { name: "markdown", kind: NativeKind::Pure(markdown) }],
    }
}

fn markdown(args: Vec<Value>) -> Result<Value, String> {
    match args.first().and_then(Value::as_str) {
        Some(s) => Ok(crate::builtins::markdown_hiccup(s)),
        None => Err("markdown(text) expects a string".into()),
    }
}

// ---------------------------------------------------------------------------
// str — the string batteries views keep needing
// ---------------------------------------------------------------------------

fn str_module() -> Module {
    Module {
        name: "str",
        natives: vec![
            NativeDef { name: "str.split", kind: NativeKind::Pure(str_split) },
            NativeDef { name: "str.trim", kind: NativeKind::Pure(str_trim) },
        ],
    }
}

/// str.split(s, sep) → array of pieces (sep is a literal, not a regex).
fn str_split(args: Vec<Value>) -> Result<Value, String> {
    let s = args.first().and_then(Value::as_str).ok_or("str.split(s, sep) expects strings")?;
    let sep = args.get(1).and_then(Value::as_str).ok_or("str.split(s, sep) expects strings")?;
    if sep.is_empty() {
        return Err("str.split separator must be non-empty".into());
    }
    Ok(Value::array(s.split(sep).map(Value::str).collect()))
}

/// str.trim(s) → s without leading/trailing whitespace.
fn str_trim(args: Vec<Value>) -> Result<Value, String> {
    match args.first().and_then(Value::as_str) {
        Some(s) => Ok(Value::str(s.trim())),
        None => Err("str.trim expects a string".into()),
    }
}

// ---------------------------------------------------------------------------
// rand — entropy is an effect: the platform rolls the die, so reducers
// stay deterministic and the harness can fake it
// ---------------------------------------------------------------------------

fn rand_module() -> Module {
    Module {
        name: "rand",
        natives: vec![NativeDef {
            name: "rand",
            kind: NativeKind::Effect { hop: "rand", vars: rand_vars },
        }],
    }
}

/// rand(n) → an int in [0, n). The bound travels in the packet; the
/// platform picks the number.
fn rand_vars(args: Vec<Value>) -> Result<Value, String> {
    let n = match args.first() {
        Some(Value::Int(n)) if *n > 0 => *n,
        _ => return Err("rand(n) expects a positive int bound".into()),
    };
    let mut m = BTreeMap::new();
    m.insert(Value::str("n"), Value::Int(n));
    Ok(Value::map(m))
}

// ---------------------------------------------------------------------------
// bash — one command, suspend until its output comes back
// ---------------------------------------------------------------------------

fn bash_module() -> Module {
    Module {
        name: "bash",
        natives: vec![NativeDef {
            name: "bash",
            kind: NativeKind::Effect { hop: "bash", vars: bash_vars },
        }],
    }
}

/// bash(cmd) or bash(cmd, dir) — dir is the working directory.
fn bash_vars(args: Vec<Value>) -> Result<Value, String> {
    let cmd = args
        .first()
        .and_then(Value::as_str)
        .ok_or("bash(cmd) expects a command string")?;
    let mut m = BTreeMap::new();
    m.insert(Value::str("cmd"), Value::str(cmd));
    if let Some(dir) = args.get(1).and_then(Value::as_str) {
        if !dir.is_empty() {
            m.insert(Value::str("dir"), Value::str(dir));
        }
    }
    Ok(Value::map(m))
}

// ---------------------------------------------------------------------------
// llm — one-shot call, and the streaming trio
// ---------------------------------------------------------------------------

fn llm_module() -> Module {
    Module {
        name: "llm",
        natives: vec![
            NativeDef {
                name: "llm.call",
                kind: NativeKind::Effect { hop: "llm", vars: llm_req_vars },
            },
            NativeDef {
                name: "llm.stream",
                kind: NativeKind::Effect { hop: "llm_start", vars: llm_req_vars },
            },
            NativeDef {
                name: "llm.next",
                kind: NativeKind::Effect { hop: "llm_next", vars: llm_next_vars },
            },
            NativeDef {
                name: "llm.models",
                kind: NativeKind::Effect { hop: "llm_models", vars: llm_no_vars },
            },
        ],
    }
}

fn llm_req_vars(args: Vec<Value>) -> Result<Value, String> {
    let req = args.first().cloned().unwrap_or(Value::Nil);
    if !matches!(req, Value::Map(_)) {
        return Err("llm expects a request map ({ messages = [...] })".into());
    }
    let mut m = BTreeMap::new();
    m.insert(Value::str("req"), req);
    Ok(Value::map(m))
}

fn llm_next_vars(args: Vec<Value>) -> Result<Value, String> {
    let h = args
        .first()
        .and_then(Value::as_str)
        .ok_or("llm.next(handle) expects a stream handle")?;
    let mut m = BTreeMap::new();
    m.insert(Value::str("h"), Value::str(h));
    Ok(Value::map(m))
}

fn llm_no_vars(_args: Vec<Value>) -> Result<Value, String> {
    Ok(Value::empty_map())
}
