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
use std::sync::{Mutex, OnceLock};

use crate::interp::Globals;
use crate::value::Value;

pub type PureFn = fn(Vec<Value>) -> Result<Value, String>;
/// Shapes call arguments into the `vars` map of an `@effects` packet.
pub type VarsFn = fn(Vec<Value>) -> Result<Value, String>;

#[derive(Clone, Copy)]
pub enum NativeKind {
    /// Runs inside the interpreter, on any side; legal in reducers.
    Pure(PureFn),
    /// Suspends the flow; the platform executes and replies. `hop` names
    /// the executor (the packet's hop field) in hopd and the harness.
    Effect { hop: &'static str, vars: VarsFn },
}

#[derive(Clone)]
pub struct NativeDef {
    /// Surface name and dispatch key: "bash", "json.first", "llm.stream".
    pub name: &'static str,
    pub kind: NativeKind,
}

#[derive(Clone)]
pub struct Module {
    pub name: &'static str,
    pub natives: Vec<NativeDef>,
}

static EXTRA: Mutex<Vec<Module>> = Mutex::new(Vec::new());
static REG: OnceLock<HashMap<&'static str, NativeKind>> = OnceLock::new();

/// The compiled-in battery set. Embedders add more with [`register`]
/// before the first VM is created.
fn builtin_modules() -> Vec<Module> {
    vec![
        json_module(),
        markdown_module(),
        bash_module(),
        llm_module(),
        str_module(),
    ]
}

/// Bind an embedder module (compile-time from the embedder's crate).
/// Must run before `serve` / the first `Vm` — the registry freezes then.
pub fn register(module: Module) {
    if REG.get().is_some() {
        panic!("hoprt::modules::register after the first VM; register natives before serve()");
    }
    EXTRA.lock().expect("modules extra").push(module);
}

/// Name → kind, for call dispatch. Built once per process: builtins plus
/// whatever the embedder registered. hop-web never calls [`register`], so
/// the wasm build stays the battery set only.
pub fn registry() -> &'static HashMap<&'static str, NativeKind> {
    REG.get_or_init(|| {
        let extra = EXTRA.lock().expect("modules extra").clone();
        let mut m = HashMap::new();
        for module in builtin_modules().into_iter().chain(extra) {
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

// ---------------------------------------------------------------------------
// str — small string helpers both VMs share (view code runs in wasm)
// ---------------------------------------------------------------------------

fn str_module() -> Module {
    Module {
        name: "str",
        natives: vec![
            NativeDef { name: "str.has", kind: NativeKind::Pure(str_has) },
            NativeDef { name: "str.before", kind: NativeKind::Pure(str_before) },
            NativeDef { name: "str.cut", kind: NativeKind::Pure(str_cut) },
            NativeDef { name: "str.strip", kind: NativeKind::Pure(str_strip) },
        ],
    }
}

fn two_strings(args: &[Value], name: &str) -> Result<(String, String), String> {
    let hay = args
        .first()
        .and_then(Value::as_str)
        .ok_or_else(|| format!("{name} expects strings"))?;
    let needle = args.get(1).and_then(Value::as_str).unwrap_or("");
    Ok((hay.to_string(), needle.to_string()))
}

fn str_has(args: Vec<Value>) -> Result<Value, String> {
    let (hay, needle) = two_strings(&args, "str.has")?;
    Ok(Value::Bool(!needle.is_empty() && hay.contains(&needle)))
}

fn str_before(args: Vec<Value>) -> Result<Value, String> {
    let (text, needle) = two_strings(&args, "str.before")?;
    if needle.is_empty() {
        return Ok(Value::str(text));
    }
    Ok(Value::str(match text.find(&needle) {
        Some(i) => text[..i].trim().to_string(),
        None => text,
    }))
}

fn str_cut(args: Vec<Value>) -> Result<Value, String> {
    let (text, needle) = two_strings(&args, "str.cut")?;
    if needle.is_empty() {
        return Ok(Value::str(text));
    }
    Ok(Value::str(text.replace(&needle, "").trim().to_string()))
}

fn str_strip(args: Vec<Value>) -> Result<Value, String> {
    let Some(s) = args.first().and_then(Value::as_str) else {
        return Err("str.strip expects a string".into());
    };
    let mut out = s.to_string();
    for tag in ["thought", "response", "message", "declaration"] {
        out = out.replace(&format!("<{tag}>"), "");
        out = out.replace(&format!("</{tag}>"), "");
    }
    Ok(Value::str(out.trim().to_string()))
}
