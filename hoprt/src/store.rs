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

use crate::interp::{self, Exec, Globals, Host, Outcome};
use crate::ir::Program;
use crate::rt::Vm;
use crate::value::{from_cbor, from_json, to_cbor, to_json, NativeId, Value};

pub struct StoreBinding {
    rt: Runtime<serde_json::Value>,
    schema: Shape,
}

/// If the app defined `schema` and `reduce`, open the runtime and bind it.
pub fn bind(vm: &Vm, data_dir: &Path) -> Result<Option<StoreBinding>, String> {
    let schema_v = vm.globals.get("schema");
    if matches!(schema_v, Value::Nil) {
        return Ok(None);
    }
    let schema = value_shape(&schema_v)?;
    let Some(&reduce_idx) = vm.prog.named.get("reduce") else {
        return Err("schema is set but reduce is missing — define fn reduce(tx, event)".into());
    };

    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let db_path = data_dir.join("proj");
    let log_path = data_dir.join("log.jsonl");

    let prog = vm.prog.clone();
    let globals_snapshot = Rc::new(vm.globals.clone());
    let schema_for_reduce = schema.clone();

    // Runtime requires Send + Sync reducers (it is shareable across
    // threads), but this reducer wraps Rc-based interpreter state that
    // never leaves this thread — the server VM and its StoreBinding live
    // on one thread by construction. The wrapper asserts that.
    struct AssertSingleThread<F>(F);
    unsafe impl<F> Send for AssertSingleThread<F> {}
    unsafe impl<F> Sync for AssertSingleThread<F> {}
    let reducer = AssertSingleThread(move |tx: &mut Tx, event: &serde_json::Value| {
        run_reduce(&prog, reduce_idx, &globals_snapshot, &schema_for_reduce, tx, event)
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
    Ok(Some(StoreBinding { rt, schema }))
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

    /// Dispatch a store native (the server platform's store_native).
    pub fn native(&mut self, id: NativeId, args: Vec<Value>) -> Result<Value, String> {
        match id {
            NativeId::StoreAppend => {
                let ev = args.first().ok_or("store.append(event)")?;
                let ev = to_json(ev)?;
                let rec = self.rt.append(ev).map_err(|e| e.to_string())?;
                Ok(Value::Int(rec.seq as i64))
            }
            NativeId::StoreOne => {
                let q = self.path_query(args.first())?;
                let v = self.rt.one(&q).map_err(|e| e.to_string())?;
                match v {
                    Some(v) => from_cbor(&v),
                    None => Ok(Value::Nil),
                }
            }
            NativeId::StoreEntries => {
                let q = self.path_query(args.first())?;
                let pairs = self.rt.entries(&q).map_err(|e| e.to_string())?;
                let mut out = Vec::with_capacity(pairs.len());
                for (k, v) in &pairs {
                    out.push(Value::array(vec![from_cbor(k)?, from_cbor(v)?]));
                }
                Ok(Value::array(out))
            }
            NativeId::StoreItems => {
                // map entries → array of records with the key merged as `id`,
                // ready for `for i, item in items` rendering
                let q = self.path_query(args.first())?;
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
            other => Err(format!("{other:?} is not a store native")),
        }
    }

    fn path_query(&self, path: Option<&Value>) -> Result<Query, String> {
        let steps = path_steps(path)?;
        let navs = dynpath::navs_for(&self.schema, &steps).map_err(|e| e.to_string())?;
        Ok(Query::new(navs))
    }
}

fn path_steps(path: Option<&Value>) -> Result<Vec<ciborium::Value>, String> {
    match path {
        Some(Value::Array(xs)) => xs.borrow().iter().map(to_cbor).collect(),
        Some(other) => Ok(vec![to_cbor(other)?]),
        None => Err("expected a path".into()),
    }
}

// ---------------------------------------------------------------------------
// The reducer: the app's IR `reduce`, run under a restricted host
// ---------------------------------------------------------------------------

fn run_reduce(
    prog: &Rc<Program>,
    reduce_idx: usize,
    globals_snapshot: &Rc<Globals>,
    schema: &Shape,
    tx: &mut Tx,
    event: &serde_json::Value,
) -> Result<(), String> {
    let tx_val = Value::map(
        [
            ("seq", Value::Int(tx.seq() as i64)),
            ("put", Value::Native(NativeId::TxPut)),
            ("peek", Value::Native(NativeId::TxPeek)),
            ("add", Value::Native(NativeId::TxAdd)),
            ("push", Value::Native(NativeId::TxPush)),
            ("delete", Value::Native(NativeId::TxDelete)),
            ("clear", Value::Native(NativeId::TxClear)),
        ]
        .into_iter()
        .map(|(k, v)| (Value::str(k), v))
        .collect(),
    );
    let ev = from_json(event);

    let mut host = ReduceHost {
        schema,
        db: tx.db().clone(),
        writes: Vec::new(),
    };
    // reducers see a frozen globals snapshot; global writes go to a
    // throwaway clone (reducers must be pure — replay depends on it)
    let mut globals = (**globals_snapshot).clone();
    let mut exec = Exec::call(prog, reduce_idx, vec![tx_val, ev]);
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
    db: durable::Db,
    writes: Vec<durable::Write>,
}

impl ReduceHost<'_> {
    fn navs(&self, path: Option<&Value>) -> Result<Vec<durable::Nav>, String> {
        let steps = path_steps(path)?;
        dynpath::navs_for(self.schema, &steps).map_err(|e| e.to_string())
    }
}

impl Host for ReduceHost<'_> {
    fn print(&mut self, line: String) {
        eprintln!("[reduce] {line}");
    }

    fn cast(&mut self, _t: Value, _h: &str, _v: Value) -> Result<(), String> {
        Err("cast is not allowed in a reducer".into())
    }

    fn spawn(&mut self, _c: Value, _a: Vec<Value>) -> Result<(), String> {
        Err("spawn is not allowed in a reducer".into())
    }

    fn session(&mut self) -> Result<Value, String> {
        Err("session() is not allowed in a reducer".into())
    }

    fn native(&mut self, id: NativeId, args: Vec<Value>) -> Result<Value, String> {
        match id {
            NativeId::TxPut => {
                let navs = self.navs(args.first())?;
                let val = to_cbor(args.get(1).unwrap_or(&Value::Nil))?;
                let ws = dynpath::put(self.schema, None, &navs, &val).map_err(|e| e.to_string())?;
                self.writes.extend(ws);
                Ok(Value::Nil)
            }
            NativeId::TxDelete => {
                let navs = self.navs(args.first())?;
                let w = dynpath::delete(self.schema, None, &navs).map_err(|e| e.to_string())?;
                self.writes.push(w);
                Ok(Value::Nil)
            }
            NativeId::TxAdd => {
                let navs = self.navs(args.first())?;
                let val = to_cbor(args.get(1).unwrap_or(&Value::Nil))?;
                let w = dynpath::add(self.schema, None, &navs, &val).map_err(|e| e.to_string())?;
                self.writes.push(w);
                Ok(Value::Nil)
            }
            NativeId::TxPush => {
                let navs = self.navs(args.first())?;
                let val = to_cbor(args.get(1).unwrap_or(&Value::Nil))?;
                let w = dynpath::push(self.schema, None, &navs, &val).map_err(|e| e.to_string())?;
                self.writes.push(w);
                Ok(Value::Nil)
            }
            NativeId::TxClear => {
                let navs = self.navs(args.first())?;
                let w = dynpath::clear(self.schema, None, &navs).map_err(|e| e.to_string())?;
                self.writes.push(w);
                Ok(Value::Nil)
            }
            NativeId::TxPeek => {
                // committed state only: an event's own writes are invisible
                let navs = self.navs(args.first())?;
                let v = dynpath::peek(&self.db, self.schema, None, &navs).map_err(|e| e.to_string())?;
                from_cbor(&v)
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
