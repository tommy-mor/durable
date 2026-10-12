//! Durable store bindings for the server VM — native, no host language in
//! the middle.
//!
//! The hop program declares `schema` (via `store.record` / `store.map` / …,
//! pure shape-constructor natives) and `fn reduce(tx, event)`. After the
//! server VM boots, [`bind`] opens a [`durable::Runtime`] on a JSONL log +
//! RocksDB projection. The reducer the runtime calls is the app's IR
//! `reduce`, run by the same interpreter with a reducer-restricted host:
//! tx natives and print, no hops, no casts, no store queries. It sees a
//! globals snapshot frozen at bind time — reducers are replayed, so they
//! must not read mutable state.
//!
//! Browser VMs never get a binding. A server segment that appends is the
//! only legal state transition — the same line the Lua era drew.

use std::path::Path;
use std::rc::Rc;

use durable::{dynpath, Query, Runtime, Shape, Tx};

use crate::interp::{self, Exec, Globals, Host, NativeOut, Outcome};
use crate::ir::{BlobDef, KeyDef, Program, SchemaDef};
use crate::rt::Vm;
use crate::value::{from_cbor, from_json, to_cbor, to_json, NativeId, Value};

pub struct StoreBinding {
    rt: Runtime<serde_json::Value>,
    schema: Shape,
    /// First-class `schema { … }` declaration, when the app uses it. The
    /// layout (`schema`) is all durable needs; this drives write-time
    /// validation of map key kinds and blob shapes.
    validation: Option<SchemaDef>,
    /// Flows parked in `store.wait(path)`: (flow id, watched prefix,
    /// read seq). Woken when a commit's writes overlap the prefix, or
    /// when any commit lands past the read seq under that prefix.
    waiters: Vec<Waiter>,
    /// Flow ids the last commit woke — drained by the platform after
    /// every append that ran on it.
    woken: Vec<String>,
}

#[derive(Clone)]
struct Waiter {
    flow: String,
    watch: Vec<u8>,
    /// Applied seq she had read when she parked. Kept so future
    /// precision work can narrow re-checks; waking today is pure overlap.
    #[allow(dead_code)]
    read_seq: u64,
    /// Session that owns the flow (browser-origin flows). Tab close
    /// kills every waiter of a dead session.
    origin: String,
}

/// Lower a first-class schema to its durable key layout. Key constraints
/// vanish (today's maps stay dynamic) and blobs are opaque leaves — one
/// CBOR value under one key.
fn schema_from_def(def: &SchemaDef) -> Shape {
    match def {
        SchemaDef::Leaf => Shape::Leaf,
        SchemaDef::Sum => Shape::Sum,
        SchemaDef::Map { of, .. } => Shape::map(schema_from_def(of)),
        SchemaDef::List(of) => Shape::list(schema_from_def(of)),
        SchemaDef::Deque(of) => Shape::deque(schema_from_def(of)),
        SchemaDef::Record(fields) => Shape::record(
            fields
                .iter()
                .map(|(n, t)| (n.clone(), schema_from_def(t)))
                .collect(),
        ),
        SchemaDef::Blob(_) => Shape::Leaf,
    }
}

/// If the app defined `schema` and `reduce`, open the runtime and bind it.
pub fn bind(vm: &Vm, data_dir: &Path) -> Result<Option<StoreBinding>, String> {
    let (schema, validation) = if let Some(def) = &vm.prog.schema {
        (schema_from_def(def), Some(def.clone()))
    } else {
        let schema_v = vm.globals.get("schema");
        if matches!(schema_v, Value::Nil) {
            return Ok(None);
        }
        // Legacy `server let schema = store.record(…)`: same layout, no
        // key/blob validation — old apps keep old semantics.
        (value_shape(&schema_v)?, None)
    };
    let Some(&reduce_idx) = vm.prog.named.get("reduce") else {
        return Err("schema is set but reduce is missing — define fn reduce(tx, event)".into());
    };

    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let db_path = data_dir.join("proj");
    let log_path = data_dir.join("log.jsonl");

    let prog = vm.prog.clone();
    let globals_snapshot = Rc::new(vm.globals.clone());
    let schema_for_reduce = schema.clone();
    let validation_for_reduce = validation.clone();

    // Runtime requires Send + Sync reducers (it is shareable across
    // threads), but this reducer wraps Rc-based interpreter state that
    // never leaves this thread — the server VM and its StoreBinding live
    // on one thread by construction. The wrapper asserts that.
    struct AssertSingleThread<F>(F);
    unsafe impl<F> Send for AssertSingleThread<F> {}
    unsafe impl<F> Sync for AssertSingleThread<F> {}
    let reducer = AssertSingleThread(move |tx: &mut Tx, event: &serde_json::Value| {
        run_reduce(
            &prog,
            reduce_idx,
            &globals_snapshot,
            &schema_for_reduce,
            &validation_for_reduce,
            tx,
            event,
        )
        .map_err(durable::Error::Reducer)
    });
    // capture the whole wrapper (not the `.0` field, which 2021 precise
    // capture would otherwise reach through, defeating the assertion)
    let reducer = move |tx: &mut Tx, event: &serde_json::Value| {
        let wrapper = &reducer;
        (wrapper.0)(tx, event)
    };

    let rt = Runtime::open(db_path, log_path, schema.clone(), None, reducer)
        .map_err(|e| e.to_string())?;
    Ok(Some(StoreBinding { rt, schema, validation, waiters: Vec::new(), woken: Vec::new() }))
}

impl StoreBinding {
    pub fn verify(&self) -> Result<(), String> {
        self.rt.verify().map_err(|e| e.to_string())
    }

    pub fn rebuild(&self) -> Result<(), String> {
        self.rt.rebuild().map_err(|e| e.to_string())
    }

    pub fn applied(&self) -> Result<u64, String> {
        self.rt.applied().map_err(|e| e.to_string())
    }

    /// Park a flow in `store.wait`: resolve the path to its watch prefix
    /// and register (flow, prefix, read seq). Returns `Ok(Woken)` when a
    /// commit landed between her last read and this park (wake at once),
    /// `Err(Parked)` to suspend toward the courier.
    pub fn wait(
        &mut self,
        path: Value,
        flow: &str,
        read_seq: u64,
        origin: &str,
    ) -> Result<Value, String> {
        let (steps, term) = split_terminal(Some(&path))?;
        if term.is_some() {
            return Err("store.wait takes a plain path".into());
        }
        let q = self.path_query(&steps)?;
        let watch = durable::watch_prefix(&self.schema, None, &q.navs);
        let applied = self.rt.applied().map_err(|e| e.to_string())?;
        if applied > read_seq {
            return Ok(Value::str("changed"));
        }
        self.waiters.push(Waiter {
            flow: flow.to_string(),
            watch,
            read_seq,
            origin: origin.to_string(),
        });
        Ok(Value::str("__parked"))
    }

    /// Cancel one parked waiter by handle. Returns true when she existed.
    pub fn cancel_waiter(&mut self, flow: &str) -> bool {
        let n = self.waiters.len();
        self.waiters.retain(|w| w.flow != flow);
        n != self.waiters.len()
    }

    /// Cancel every waiter owned by a dead session. Returns how many died.
    pub fn cancel_waiters_of_origin(&mut self, sid: &str) -> usize {
        let n = self.waiters.len();
        self.waiters.retain(|w| w.origin != sid);
        n - self.waiters.len()
    }

    /// Flows the last commit woke. Drained by the platform after every
    /// append that ran on it — the courier's delivery queue.
    pub fn drain_woken(&mut self) -> Vec<String> {
        std::mem::take(&mut self.woken)
    }

    /// Which parked flows does this commit wake? Called from the
    /// runtime's on_commit listener while still holding the ingest lock.
    pub fn wake_with(&mut self, writes: &[durable::Write]) {
        let mut woken = Vec::new();
        self.waiters.retain(|w| {
            let hit = writes.iter().any(|wr| {
                durable::write_range(wr).overlaps(&durable::WatchRange::prefix(&w.watch))
            });
            if hit {
                woken.push(w.flow.clone());
                false
            } else {
                true
            }
        });
        self.woken.extend(woken);
    }

    /// Dispatch a store native (the server platform's store_native).
    pub fn native(&mut self, id: NativeId, args: Vec<Value>) -> Result<Value, String> {
        match id {
            NativeId::StoreCall => {
                // one function: a path that only navigates is a query
                let (steps, term) = split_terminal(args.first())?;
                if term.is_some() {
                    return Err(
                        "a path with a terminal navigator is a mutation — those happen in fn reduce"
                            .into(),
                    );
                }
                let q = self.path_query(&steps)?;
                if q.is_collecting() {
                    let vs = self.rt.select(&q).map_err(|e| e.to_string())?;
                    let mut out = Vec::with_capacity(vs.len());
                    for v in &vs {
                        out.push(from_cbor(v)?);
                    }
                    Ok(Value::array(out))
                } else {
                    match self.rt.one(&q).map_err(|e| e.to_string())? {
                        Some(v) => from_cbor(&v),
                        None => from_cbor(&self.rt.subtree(&q).map_err(|e| e.to_string())?),
                    }
                }
            }
            NativeId::StoreAppend => {
                let ev = args.first().ok_or("store.append(event)")?;
                let ev = to_json(ev)?;
                let rec = self.rt.append(ev).map_err(|e| e.to_string())?;
                // The courier wakes here: the commit's waking set lands in
                // last_writes under the ingest lock; check every parked
                // watch against it before the next flow runs.
                let writes = self.rt.last_writes();
                self.wake_with(&writes);
                Ok(Value::Int(rec.seq as i64))
            }
            NativeId::StoreItems => {
                // map entries → array of records with the key merged as `id`,
                // ready for `for i, item in items` rendering
                let (steps, term) = split_terminal(args.first())?;
                if term.is_some() {
                    return Err("store.items takes a plain path".into());
                }
                let q = self.path_query(&steps)?;
                let pairs = self.rt.entries(&q).map_err(|e| e.to_string())?;
                let mut out = Vec::with_capacity(pairs.len());
                for (k, v) in &pairs {
                    let rec = from_cbor(v)?;
                    if matches!(rec, Value::Map(_)) {
                        rec.set_field("id", from_cbor(k)?)?;
                    }
                    out.push(rec);
                }
                Ok(Value::array(out))
            }
            NativeId::StoreVerify => {
                self.rt.verify().map_err(|e| e.to_string())?;
                Ok(Value::Bool(true))
            }
            NativeId::StoreHas => {
                // Presence in a parent-creating keyspace is semantic, not
                // structural: probed in the raw keyspace (durable::query::has),
                // so a missing record's empty-map read and a missing sum's
                // zero default cannot masquerade as existence.
                let (steps, term) = split_terminal(args.first())?;
                if term.is_some() {
                    return Err("store.has takes a plain path".into());
                }
                let q = self.path_query(&steps)?;
                Ok(Value::Bool(self.rt.has(&q).map_err(|e| e.to_string())?))
            }
            other => Err(format!("{other:?} is not a store native")),
        }
    }

    fn path_query(&self, steps: &[ciborium::Value]) -> Result<Query, String> {
        let navs = dynpath::navs_for(&self.schema, steps).map_err(|e| e.to_string())?;
        Ok(Query::new(navs))
    }
}
/// A terminal navigator, parsed off the end of a path.
enum Term {
    Set(Value),
    Add(Value),
    Push(Value),
    Del,
}

fn parse_term(v: &Value) -> Option<Result<Term, String>> {
    let Value::Tagged(t) = v else { return None };
    if t.0 != "term" {
        return None;
    }
    let Value::Array(parts) = &t.1 else {
        return Some(Err("malformed #term".into()));
    };
    let parts = parts.borrow();
    let op = parts.first().and_then(Value::as_str).unwrap_or("");
    let payload = parts.get(1).cloned().unwrap_or(Value::Nil);
    Some(Ok(match op {
        "set" => Term::Set(payload),
        "add" => Term::Add(payload),
        "push" => Term::Push(payload),
        "del" => Term::Del,
        other => return Some(Err(format!("unknown terminal navigator {other:?}"))),
    }))
}

/// Split a hop path into navigation steps (as CBOR, for durable) and an
/// optional terminal. A terminal anywhere but last is an error.
fn split_terminal(path: Option<&Value>) -> Result<(Vec<ciborium::Value>, Option<Term>), String> {
    let Some(Value::Array(xs)) = path else {
        return Err("expected a path array".into());
    };
    let xs = xs.borrow();
    let mut steps = Vec::with_capacity(xs.len());
    let mut term = None;
    for (i, x) in xs.iter().enumerate() {
        match parse_term(x) {
            Some(t) => {
                if i + 1 != xs.len() {
                    return Err("a terminal navigator must be the last path element".into());
                }
                term = Some(t?);
            }
            None => steps.push(to_cbor(x)?),
        }
    }
    Ok((steps, term))
}

// ---------------------------------------------------------------------------
// The reducer: the app's IR `reduce`, run under a restricted host
// ---------------------------------------------------------------------------

fn run_reduce(
    prog: &Rc<Program>,
    reduce_idx: usize,
    globals_snapshot: &Rc<Globals>,
    schema: &Shape,
    validation: &Option<SchemaDef>,
    tx: &mut Tx,
    event: &serde_json::Value,
) -> Result<(), String> {
    // the event carries its own seq and tape time — there is no tx handle in hop
    let ev = from_json(event);
    if matches!(ev, Value::Map(_)) {
        ev.set_field("seq", Value::Int(tx.seq() as i64))?;
        ev.set_field("ts_ms", Value::Int(tx.meta().ts_ms as i64))?;
    }

    let mut host = ReduceHost {
        schema,
        validation,
        db: tx.db().clone(),
        writes: Vec::new(),
    };
    // reducers see a frozen globals snapshot; global writes go to a
    // throwaway clone (reducers must be pure — replay depends on it)
    let mut globals = (**globals_snapshot).clone();
    let mut exec = Exec::call(prog, reduce_idx, vec![ev]);
    match interp::run(prog, &mut exec, &mut globals, &mut host) {
        Outcome::Done(_) => {
            for w in host.writes {
                tx.write(w);
            }
            Ok(())
        }
        Outcome::Suspend { hop, .. } => Err(format!("reducer cannot hop (at {hop})")),
        Outcome::Error(e) => Err(e),
    }
}

struct ReduceHost<'a> {
    schema: &'a Shape,
    validation: &'a Option<SchemaDef>,
    db: durable::Db,
    writes: Vec<durable::Write>,
}

impl Host for ReduceHost<'_> {
    fn print(&mut self, line: String) {
        eprintln!("[reduce] {line}");
    }

    fn cast(&mut self, _t: Value, _h: &str, _v: Value) -> Result<(), String> {
        Err("cast is not allowed in a reducer".into())
    }

    fn spawn(&mut self, _c: Value, _a: Vec<Value>) -> Result<Value, String> {
        Err("spawn is not allowed in a reducer".into())
    }

    fn session(&mut self) -> Result<Value, String> {
        Err("session() is not allowed in a reducer".into())
    }

    fn native(&mut self, id: NativeId, args: Vec<Value>) -> Result<NativeOut, String> {
        self.native_value(id, args).map(NativeOut::Val)
    }

    /// Effects would make replay nondeterministic; the reducer is a
    /// function of (committed state, event) and nothing else.
    fn effect(&mut self, name: &str, _hop: &str, _vars: Value) -> Result<NativeOut, String> {
        Err(format!("effects are not allowed in a reducer ({name})"))
    }
}

impl ReduceHost<'_> {
    /// Validate a write path's map keys against the declared key kinds.
    /// Returns the schema at the path's target for value validation.
    /// `None` means a legacy app (no `schema { … }`): skip everything.
    /// Reads never call this — they stay tolerant.
    fn check_path(&self, navs: &[durable::Nav]) -> Result<Option<&SchemaDef>, String> {
        let Some(def) = self.validation.as_ref() else {
            return Ok(None);
        };
        Ok(Some(resolve_def(def, navs)?))
    }
}

// ---------------------------------------------------------------------------
// Write-time validation against `schema { … }`
//
// The layout (`Shape`) is what durable needs; this walks the richer
// `SchemaDef` alongside the write's navs and checks what the layout
// cannot say: map key kinds, and blob (opaque-leaf) shapes. Reads stay
// tolerant — only reducer writes fail. Anything this accepts, durable
// still checks (shapes, sums, indices); anything durable rejects, this
// never rescues.
// ---------------------------------------------------------------------------

/// Walk a schema along write navs to the target, checking map keys on
/// the way down. Collecting combinators cannot appear in a write path.
fn resolve_def<'a>(def: &'a SchemaDef, navs: &[durable::Nav]) -> Result<&'a SchemaDef, String> {
    let mut cur = def;
    for nav in navs {
        cur = match (cur, nav) {
            (SchemaDef::Record(fields), durable::Nav::Field(name)) => fields
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, t)| t)
                .ok_or_else(|| format!("record has no field {name:?}"))?,
            (SchemaDef::Record(_), durable::Nav::Key(_)) => {
                return Err("key/index on a record — address fields by name".into());
            }
            (
                SchemaDef::Map { key, of },
                durable::Nav::Key(k),
            ) => {
                let k = from_cbor(k).map_err(|e| format!("bad map key: {e}"))?;
                check_key(&k, key)?;
                of
            }
            (SchemaDef::Map { key, of }, durable::Nav::Field(name)) => {
                check_key(&Value::str(name.as_str()), key)?;
                of
            }
            (SchemaDef::List(of) | SchemaDef::Deque(of), durable::Nav::Index(_)) => of,
            (SchemaDef::List(_), durable::Nav::Key(_)) => {
                return Err("integer Key on List required; use Index".into());
            }
            (SchemaDef::Deque(_), durable::Nav::Key(_)) => {
                return Err("Deque writes use Index (logical) or push/clear".into());
            }
            (SchemaDef::Blob(_), _) => {
                return Err("write path entered a blob<…>; put the whole blob".into());
            }
            (SchemaDef::Leaf, _) => cur,
            (SchemaDef::Sum, _) => {
                return Err("cannot navigate into a sum".into());
            }
            (def, nav) => {
                return Err(format!(
                    "cannot navigate {nav:?} on a {}",
                    schema_kind(def)
                ));
            }
        };
    }
    Ok(cur)
}

fn schema_kind(def: &SchemaDef) -> &'static str {
    match def {
        SchemaDef::Leaf => "leaf",
        SchemaDef::Sum => "sum",
        SchemaDef::Map { .. } => "map",
        SchemaDef::List(_) => "list",
        SchemaDef::Deque(_) => "deque",
        SchemaDef::Record(_) => "record",
        SchemaDef::Blob(_) => "blob",
    }
}

/// Unwrap `#tag` envelopes: a tagged key counts by its payload kind —
/// tags travel on the value, not in the kind the schema constrains.
fn strip_tag(mut v: &Value) -> &Value {
    while let Value::Tagged(t) = v {
        v = &t.1;
    }
    v
}

fn check_key(key: &Value, def: &KeyDef) -> Result<(), String> {
    match def {
        KeyDef::Any => Ok(()),
        KeyDef::String => match strip_tag(key) {
            Value::Str(_) => Ok(()),
            other => Err(format!("map key must be string, got {}", other.kind())),
        },
        KeyDef::Int => match strip_tag(key) {
            Value::Int(_) => Ok(()),
            other => Err(format!("map key must be int, got {}", other.kind())),
        },
        KeyDef::Float => match strip_tag(key) {
            Value::Float(_) => Ok(()),
            other => Err(format!("map key must be float, got {}", other.kind())),
        },
        KeyDef::Bool => match strip_tag(key) {
            Value::Bool(_) => Ok(()),
            other => Err(format!("map key must be bool, got {}", other.kind())),
        },
        KeyDef::Bytes => match strip_tag(key) {
            Value::Bytes(_) => Ok(()),
            other => Err(format!("map key must be bytes, got {}", other.kind())),
        },
    }
}

/// Validate a `store.set` value against its target: record puts expand
/// per field exactly like `dynpath::put_at` (absent fields read as nil
/// either way, extras are ignored there too), and blob leaves check
/// their CBOR shape. Whole-collection puts are durable's error.
fn check_set(v: &Value, def: &SchemaDef) -> Result<(), String> {
    match def {
        SchemaDef::Leaf => Ok(()),
        SchemaDef::Sum => match strip_tag(v) {
            Value::Int(_) | Value::Float(_) => Ok(()),
            other => Err(format!("sum value must be numeric, got {}", other.kind())),
        },
        SchemaDef::Map { .. } | SchemaDef::List(_) | SchemaDef::Deque(_) => Ok(()),
        SchemaDef::Record(fields) => {
            if matches!(v, Value::Nil) {
                return Ok(());
            }
            let Value::Map(_) = v else {
                return Err(format!("record value must be a map, got {}", v.kind()));
            };
            for (name, child) in fields {
                let child_val = v.get_field(name);
                if matches!(child_val, Value::Nil)
                    && !matches!(child, SchemaDef::Leaf | SchemaDef::Blob(_))
                {
                    continue;
                }
                check_set(&child_val, child)
                    .map_err(|e| format!("field {name:?}: {e}"))?;
            }
            Ok(())
        }
        SchemaDef::Blob(b) => check_blob(v, b),
    }
}

/// Validate a `store.push` element: the list's element type, same value
/// rules as `store.set` at that type.
fn check_push(v: &Value, def: &SchemaDef) -> Result<(), String> {
    match def {
        SchemaDef::List(of) | SchemaDef::Deque(of) => check_set(v, of),
        _ => Ok(()),
    }
}

/// Validate one opaque value against its declared CBOR shape. `nil` is
/// always accepted: absence and nil-value are one thing, matching the
/// value model. Closed maps tolerate extras (a newer writer must not
/// brick an older reader) and absent fields (they read as nil).
fn check_blob(v: &Value, b: &BlobDef) -> Result<(), String> {
    if matches!(v, Value::Nil) {
        return Ok(());
    }
    match b {
        BlobDef::Any => Ok(()),
        BlobDef::String => match v {
            Value::Str(_) => Ok(()),
            other => Err(format!("blob must be string, got {}", other.kind())),
        },
        BlobDef::Int => match v {
            Value::Int(_) => Ok(()),
            other => Err(format!("blob must be int, got {}", other.kind())),
        },
        BlobDef::Float => match v {
            Value::Float(_) => Ok(()),
            other => Err(format!("blob must be float, got {}", other.kind())),
        },
        BlobDef::Bool => match v {
            Value::Bool(_) => Ok(()),
            other => Err(format!("blob must be bool, got {}", other.kind())),
        },
        BlobDef::Bytes => match v {
            Value::Bytes(_) => Ok(()),
            other => Err(format!("blob must be bytes, got {}", other.kind())),
        },
        BlobDef::Array(of) => {
            let Value::Array(items) = v else {
                return Err(format!("blob must be array, got {}", v.kind()));
            };
            for item in items.borrow().iter() {
                check_blob(item, of)?;
            }
            Ok(())
        }
        BlobDef::Map { key, of } => {
            let Value::Map(entries) = v else {
                return Err(format!("blob must be map, got {}", v.kind()));
            };
            for (k, val) in entries.borrow().iter() {
                check_key(k, key)?;
                check_blob(val, of).map_err(|e| format!("key {k}: {e}"))?;
            }
            Ok(())
        }
        BlobDef::Closed(fields) => {
            let Value::Map(_) = v else {
                return Err(format!("blob must be map, got {}", v.kind()));
            };
            for (name, child) in fields {
                let child_val = v.get_field(name);
                if matches!(child_val, Value::Nil) {
                    continue;
                }
                check_blob(&child_val, child)
                    .map_err(|e| format!("field {name:?}: {e}"))?;
            }
            Ok(())
        }
    }
}

impl ReduceHost<'_> {
    fn native_value(&mut self, id: NativeId, args: Vec<Value>) -> Result<Value, String> {
        match id {
            // the same one function. Here a terminal navigator is legal:
            // it reifies as a Write against committed state.
            NativeId::StoreCall => {
                let (steps, term) = split_terminal(args.first())?;
                let navs =
                    dynpath::navs_for(self.schema, &steps).map_err(|e| e.to_string())?;
                match term {
                    None => {
                        let q = Query::new(navs);
                        if q.is_collecting() {
                            let vs = durable::query::select(&self.db, self.schema, &q)
                                .map_err(|e| e.to_string())?;
                            let mut out = Vec::with_capacity(vs.len());
                            for v in &vs {
                                out.push(from_cbor(v)?);
                            }
                            Ok(Value::array(out))
                        } else {
                            // committed state: an event's own writes are invisible
                            let v = dynpath::peek(&self.db, self.schema, None, &q.navs)
                                .map_err(|e| e.to_string())?;
                            from_cbor(&v)
                        }
                    }
                    Some(Term::Set(v)) => {
                        let target = self.check_path(&navs)?;
                        if let Some(def) = target {
                            check_set(&v, def)?;
                        }
                        let val = to_cbor(&v)?;
                        let ws = dynpath::put(self.schema, None, &navs, &val)
                            .map_err(|e| e.to_string())?;
                        self.writes.extend(ws);
                        Ok(Value::Nil)
                    }
                    Some(Term::Add(v)) => {
                        self.check_path(&navs)?;
                        let val = to_cbor(&v)?;
                        let w = dynpath::add(self.schema, None, &navs, &val)
                            .map_err(|e| e.to_string())?;
                        self.writes.push(w);
                        Ok(Value::Nil)
                    }
                    Some(Term::Push(v)) => {
                        let target = self.check_path(&navs)?;
                        if let Some(def) = target {
                            check_push(&v, def)?;
                        }
                        let val = to_cbor(&v)?;
                        let w = dynpath::push(self.schema, None, &navs, &val)
                            .map_err(|e| e.to_string())?;
                        self.writes.push(w);
                        Ok(Value::Nil)
                    }
                    Some(Term::Del) => {
                        self.check_path(&navs)?;
                        let w = dynpath::delete(self.schema, None, &navs)
                            .map_err(|e| e.to_string())?;
                        self.writes.push(w);
                        Ok(Value::Nil)
                    }
                }
            }
            other => Err(format!("{other:?} is not available in a reducer")),
        }
    }
}

// ---------------------------------------------------------------------------
// Schema: shape-constructor values → durable::Shape
// ---------------------------------------------------------------------------

fn value_shape(v: &Value) -> Result<Shape, String> {
    let k = v.get_field("k");
    let k = k.as_str().ok_or("schema must be a store.* constructor value")?;
    match k {
        "leaf" => Ok(Shape::Leaf),
        "sum" => Ok(Shape::Sum),
        "map" => Ok(Shape::map(value_shape(&v.get_field("of"))?)),
        "list" => Ok(Shape::list(value_shape(&v.get_field("of"))?)),
        "deque" => Ok(Shape::deque(value_shape(&v.get_field("of"))?)),
        "record" => {
            let fields = v.get_field("fields");
            let Value::Array(fs) = &fields else {
                return Err("record fields must be an array of [name, shape]".into());
            };
            let mut out = Vec::new();
            for pair in fs.borrow().iter() {
                let Value::Array(p) = pair else {
                    return Err("record field must be [name, shape]".into());
                };
                let p = p.borrow();
                let name = p
                    .first()
                    .and_then(Value::as_str)
                    .ok_or("record field name must be a string")?
                    .to_string();
                let shape = value_shape(p.get(1).unwrap_or(&Value::Nil))?;
                out.push((name, shape));
            }
            Ok(Shape::record(out))
        }
        other => Err(format!("unknown shape tag {other:?}")),
    }
}
