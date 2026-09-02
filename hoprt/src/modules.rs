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
    vec![
        json_module(),
        markdown_module(),
        bash_module(),
        llm_module(),
        rank_module(),
    ]
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
// rank — centrality, compaction plan, being projection (pure)
// ---------------------------------------------------------------------------

fn rank_module() -> Module {
    Module {
        name: "rank",
        natives: vec![
            NativeDef { name: "rank.score", kind: NativeKind::Pure(rank_score) },
            NativeDef { name: "rank.components", kind: NativeKind::Pure(rank_components) },
            NativeDef { name: "rank.plan", kind: NativeKind::Pure(rank_plan) },
            NativeDef { name: "rank.finalize", kind: NativeKind::Pure(rank_finalize) },
            NativeDef { name: "rank.project", kind: NativeKind::Pure(rank_project) },
            NativeDef { name: "rank.parse_score", kind: NativeKind::Pure(rank_parse_score) },
            NativeDef { name: "rank.has", kind: NativeKind::Pure(rank_has) },
            NativeDef { name: "rank.strip", kind: NativeKind::Pure(rank_strip) },
            NativeDef { name: "rank.cut", kind: NativeKind::Pure(rank_cut) },
            NativeDef { name: "rank.before", kind: NativeKind::Pure(rank_before) },
        ],
    }
}

fn value_ids(v: &Value) -> Result<Vec<String>, String> {
    match v {
        Value::Array(a) => Ok(a
            .borrow()
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect()),
        Value::Nil => Ok(vec![]),
        other => Err(format!("rank expects an id array, got {}", other.kind())),
    }
}

fn value_num_i32(v: &Value) -> Option<i32> {
    match v {
        Value::Int(i) => Some(*i as i32),
        Value::Float(f) => Some(*f as i32),
        _ => None,
    }
}

fn value_u64(v: &Value) -> u64 {
    match v {
        Value::Int(i) if *i >= 0 => *i as u64,
        Value::Int(i) => i.unsigned_abs(),
        Value::Float(f) if *f >= 0.0 => *f as u64,
        _ => 0,
    }
}

fn value_comparisons(v: &Value) -> Result<Vec<rank::Comparison>, String> {
    let items: Vec<Value> = match v {
        Value::Array(a) => a.borrow().clone(),
        Value::Map(m) => m.borrow().values().cloned().collect(),
        Value::Nil => return Ok(vec![]),
        other => return Err(format!("rank expects comparisons, got {}", other.kind())),
    };
    let mut out = Vec::new();
    for c in items {
        let a = c.get_field("a");
        let a = if a.as_str().is_some() {
            a
        } else {
            c.get_field("a_id")
        };
        let b = c.get_field("b");
        let b = if b.as_str().is_some() {
            b
        } else {
            c.get_field("b_id")
        };
        let score = c.get_field("score");
        let score = if matches!(score, Value::Nil) {
            c.get_field("vote_score")
        } else {
            score
        };
        let (Some(a), Some(b), Some(score)) = (a.as_str(), b.as_str(), value_num_i32(&score)) else {
            continue;
        };
        out.push(rank::Comparison {
            a_id: a.to_string(),
            b_id: b.to_string(),
            score,
        });
    }
    Ok(out)
}

/// rank.score(ids, comparisons) → [{id, score}, ...]
fn rank_score(args: Vec<Value>) -> Result<Value, String> {
    let ids = value_ids(args.first().unwrap_or(&Value::Nil))?;
    let comps = value_comparisons(args.get(1).unwrap_or(&Value::Nil))?;
    let ranked = rank::ranked_items_from_comparisons(&ids, &comps, 100_000, 1e-8);
    let items: Result<Vec<Value>, String> = ranked
        .into_iter()
        .map(|r| {
            Ok(Value::map(
                [
                    (Value::str("id"), Value::str(r.item)),
                    (Value::str("score"), Value::float(r.score)?),
                ]
                .into_iter()
                .collect(),
            ))
        })
        .collect();
    Ok(Value::array(items?))
}

/// rank.components(ids, comparisons) → { groups = [[id…]], isolates = [id…] }
fn rank_components(args: Vec<Value>) -> Result<Value, String> {
    let ids = value_ids(args.first().unwrap_or(&Value::Nil))?;
    let comps = value_comparisons(args.get(1).unwrap_or(&Value::Nil))?;
    let (groups, isolates) = rank::connected_components(&ids, &comps);
    let groups = Value::array(
        groups
            .into_iter()
            .map(|g| {
                Value::array(
                    g.into_iter()
                        .filter_map(|i| ids.get(i).map(|s| Value::str(s.as_str())))
                        .collect(),
                )
            })
            .collect(),
    );
    let isolates = Value::array(
        isolates
            .into_iter()
            .filter_map(|i| ids.get(i).map(|s| Value::str(s.as_str())))
            .collect(),
    );
    Ok(Value::map(
        [
            (Value::str("groups"), groups),
            (Value::str("isolates"), isolates),
        ]
        .into_iter()
        .collect(),
    ))
}

/// rank.plan(ids, comparisons, seed) → { pairs = [[a, b], …] }
fn rank_plan(args: Vec<Value>) -> Result<Value, String> {
    let ids = value_ids(args.first().unwrap_or(&Value::Nil))?;
    let comps = value_comparisons(args.get(1).unwrap_or(&Value::Nil))?;
    let seed = value_u64(args.get(2).unwrap_or(&Value::Nil));
    let plan = rank::plan_pairs(&ids, &comps, seed);
    let pairs = Value::array(
        plan.pairs
            .into_iter()
            .map(|(a, b)| Value::array(vec![Value::str(a), Value::str(b)]))
            .collect(),
    );
    Ok(Value::map([(Value::str("pairs"), pairs)].into_iter().collect()))
}

/// rank.finalize(all_ids, current_ids, comparisons, budget) → { kept, released }
fn rank_finalize(args: Vec<Value>) -> Result<Value, String> {
    let all = value_ids(args.first().unwrap_or(&Value::Nil))?;
    let current = value_ids(args.get(1).unwrap_or(&Value::Nil))?;
    let comps = value_comparisons(args.get(2).unwrap_or(&Value::Nil))?;
    let budget = value_u64(args.get(3).unwrap_or(&Value::Nil)) as usize;
    if budget == 0 {
        return Err("compaction budget must be at least 1".into());
    }
    if rank::nothing_to_compact(all.len(), current.len(), budget) {
        return Ok(Value::map(
            [
                (Value::str("kept"), Value::array(vec![])),
                (Value::str("released"), Value::array(vec![])),
                (Value::str("skip"), Value::Bool(true)),
            ]
            .into_iter()
            .collect(),
        ));
    }
    let out = rank::finalize(&all, &current, &comps, budget);
    Ok(Value::map(
        [
            (
                Value::str("kept"),
                Value::array(out.kept.into_iter().map(Value::str).collect()),
            ),
            (
                Value::str("released"),
                Value::array(out.released.into_iter().map(Value::str).collect()),
            ),
            (Value::str("skip"), Value::Bool(false)),
        ]
        .into_iter()
        .collect(),
    ))
}

/// rank.project(events) → { all, current, votes, declaration, capacity, … }
fn rank_project(args: Vec<Value>) -> Result<Value, String> {
    let events = args.first().cloned().unwrap_or(Value::Nil);
    let j = crate::value::to_json(&events)?;
    let arr = match j {
        serde_json::Value::Array(xs) => xs,
        serde_json::Value::Null => vec![],
        other => return Err(format!("rank.project expects an event array, got {other}")),
    };
    Ok(crate::value::from_json(&rank::project_json(&arr)))
}

/// rank.parse_score(text) → int | nil
fn rank_parse_score(args: Vec<Value>) -> Result<Value, String> {
    match args.first().and_then(Value::as_str) {
        Some(s) => Ok(match rank::parse_score(s) {
            Some(n) => Value::Int(n as i64),
            None => Value::Nil,
        }),
        None => Err("rank.parse_score expects a string".into()),
    }
}

/// rank.has(hay, needle) → bool
fn rank_has(args: Vec<Value>) -> Result<Value, String> {
    let hay = args
        .first()
        .and_then(Value::as_str)
        .ok_or("rank.has(hay, needle) expects strings")?;
    let needle = args.get(1).and_then(Value::as_str).unwrap_or("");
    Ok(Value::Bool(rank::has_substr(hay, needle)))
}

/// rank.strip(text) → string without <thought>|… tags
fn rank_strip(args: Vec<Value>) -> Result<Value, String> {
    match args.first().and_then(Value::as_str) {
        Some(s) => Ok(Value::str(rank::strip_tags(s))),
        None => Err("rank.strip expects a string".into()),
    }
}

/// rank.before(text, needle) → text up to the first needle, or all of it
fn rank_before(args: Vec<Value>) -> Result<Value, String> {
    let s = args
        .first()
        .and_then(Value::as_str)
        .ok_or("rank.before(text, needle) expects strings")?;
    let n = args.get(1).and_then(Value::as_str).unwrap_or("");
    Ok(Value::str(rank::before(s, n)))
}

/// rank.cut(text, needle) → text with needle removed
fn rank_cut(args: Vec<Value>) -> Result<Value, String> {
    let s = args
        .first()
        .and_then(Value::as_str)
        .ok_or("rank.cut(text, needle) expects strings")?;
    let n = args.get(1).and_then(Value::as_str).unwrap_or("");
    Ok(Value::str(rank::cut(s, n)))
}
