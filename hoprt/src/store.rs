//! Durable store bindings for the server Luau VM.
//!
//! The hop program declares `schema` (via `store.record` / `store.map` / …)
//! and `fn reduce(tx, event)`. After the app chunk loads, [`bind`] opens a
//! [`durable::Runtime`] on a JSONL log + RocksDB projection and hangs
//! `store.append` / `store.one` / `store.entries` / … on the same table.
//!
//! Browser VMs never load this. A server segment that appends is the only
//! legal state transition — the same line the Rust runtime already drew.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use durable::{cbor_to_json, dynpath, json_to_cbor, Nav, Query, Runtime, Shape, Tx};
use mlua::{Function, Lua, LuaSerdeExt, Table, Value as LuaValue};

const STORE_LUA: &str = include_str!("../lua/store.lua");

/// Load schema constructors (`store.leaf`, `store.record`, …).
pub fn install_constructors(lua: &Lua) -> mlua::Result<()> {
    lua.load(STORE_LUA).set_name("store.lua").exec()
}

/// If the app defined `schema` and `reduce`, open the runtime and bind it.
/// Returns whether a store was bound.
pub fn bind(lua: &Lua, data_dir: &Path) -> mlua::Result<bool> {
    let schema_v: LuaValue = lua.globals().get("schema")?;
    if matches!(schema_v, LuaValue::Nil) {
        return Ok(false);
    }
    let reduce_fn: Function = lua.globals().get("reduce").map_err(|_| {
        mlua::Error::runtime("schema is set but reduce is missing — define fn reduce(tx, event)")
    })?;
    let schema = lua_shape(schema_v)?;

    std::fs::create_dir_all(data_dir).map_err(mlua::Error::external)?;
    let db_path = data_dir.join("proj");
    let log_path = data_dir.join("log.jsonl");

    // Lua is single-threaded here. bind() and every store.* callback run
    // with this VM on the stack; the reducer is only invoked from those
    // paths (append / catch-up during bind / rebuild / verify).
    let reduce_fn = Rc::new(reduce_fn);
    let schema_for_reduce = schema.clone();
    // bind() and every store.* callback run with this VM on the stack; the
    // reducer is only invoked from those paths (append / catch-up during
    // bind / rebuild / verify).
    let lua_slot: Rc<RefCell<Option<*const Lua>>> = Rc::new(RefCell::new(None));
    let slot = lua_slot.clone();
    let reducer = move |tx: &mut Tx, event: &serde_json::Value| {
        let ptr = *slot.borrow();
        // SAFETY: lua_slot is set only while `lua` is on the stack of bind,
        // append, rebuild, or verify — the only callers of this reducer.
        let lua = unsafe {
            let ptr = ptr
                .ok_or_else(|| durable::Error::Reducer("store reducer ran without a Lua VM".into()))?;
            &*ptr
        };
        call_reduce(lua, &reduce_fn, &schema_for_reduce, tx, event)
            .map_err(|e| durable::Error::Reducer(e.to_string()))
    };

    *lua_slot.borrow_mut() = Some(lua as *const Lua);
    let rt = Runtime::open(db_path, log_path, schema.clone(), None, reducer)
        .map_err(mlua::Error::external)?;
    *lua_slot.borrow_mut() = None;

    let rt = Rc::new(RefCell::new(rt));
    let store: Table = lua.globals().get("store")?;

    {
        let rt = rt.clone();
        let lua_slot = lua_slot.clone();
        store.set(
            "append",
            lua.create_function(move |lua, event: LuaValue| {
                let ev: serde_json::Value = lua.from_value(event)?;
                *lua_slot.borrow_mut() = Some(lua as *const Lua);
                let idx = rt.borrow_mut().append(ev).map_err(mlua::Error::external)?;
                *lua_slot.borrow_mut() = None;
                Ok(idx)
            })?,
        )?;
    }
    {
        let rt = rt.clone();
        let schema = schema.clone();
        store.set(
            "one",
            lua.create_function(move |lua, path: LuaValue| {
                let q = path_query(lua, &schema, path)?;
                let v = rt.borrow().one(&q).map_err(mlua::Error::external)?;
                match v {
                    Some(v) => lua.to_value(&cbor_to_json(&v)),
                    None => Ok(LuaValue::Nil),
                }
            })?,
        )?;
    }
    {
        let rt = rt.clone();
        let schema = schema.clone();
        store.set(
            "select",
            lua.create_function(move |lua, path: LuaValue| {
                let q = path_query(lua, &schema, path)?;
                let vs = rt.borrow().select(&q).map_err(mlua::Error::external)?;
                lua.to_value(&serde_json::Value::Array(
                    vs.iter().map(cbor_to_json).collect(),
                ))
            })?,
        )?;
    }
    {
        let rt = rt.clone();
        let schema = schema.clone();
        store.set(
            "subtree",
            lua.create_function(move |lua, path: LuaValue| {
                let q = path_query(lua, &schema, path)?;
                let v = rt.borrow().subtree(&q).map_err(mlua::Error::external)?;
                lua.to_value(&cbor_to_json(&v))
            })?,
        )?;
    }
    {
        let rt = rt.clone();
        let schema = schema.clone();
        store.set(
            "entries",
            lua.create_function(move |lua, path: LuaValue| {
                let q = path_query(lua, &schema, path)?;
                let pairs = rt.borrow().entries(&q).map_err(mlua::Error::external)?;
                let json = serde_json::Value::Array(
                    pairs
                        .iter()
                        .map(|(k, v)| serde_json::json!([cbor_to_json(k), cbor_to_json(v)]))
                        .collect(),
                );
                lua.to_value(&json)
            })?,
        )?;
    }
    {
        let rt = rt.clone();
        let schema = schema.clone();
        store.set(
            "explain",
            lua.create_function(move |lua, path: LuaValue| {
                let q = path_query(lua, &schema, path)?;
                let plan = rt.borrow().explain(&q).map_err(mlua::Error::external)?;
                Ok(plan.to_string())
            })?,
        )?;
    }
    {
        let rt = rt.clone();
        let lua_slot = lua_slot.clone();
        store.set(
            "rebuild",
            lua.create_function(move |lua, ()| {
                *lua_slot.borrow_mut() = Some(lua as *const Lua);
                let r = rt.borrow_mut().rebuild().map_err(mlua::Error::external);
                *lua_slot.borrow_mut() = None;
                r
            })?,
        )?;
    }
    {
        let rt = rt.clone();
        let lua_slot = lua_slot.clone();
        store.set(
            "verify",
            lua.create_function(move |lua, ()| {
                *lua_slot.borrow_mut() = Some(lua as *const Lua);
                let r = rt.borrow().verify().map_err(mlua::Error::external);
                *lua_slot.borrow_mut() = None;
                r
            })?,
        )?;
    }
    {
        let rt = rt.clone();
        store.set(
            "applied",
            lua.create_function(move |_, ()| {
                rt.borrow().applied().map_err(mlua::Error::external)
            })?,
        )?;
    }

    Ok(true)
}

fn call_reduce(
    lua: &Lua,
    reduce: &Function,
    schema: &Shape,
    tx: &mut Tx,
    event: &serde_json::Value,
) -> mlua::Result<()> {
    let db = tx.db().clone();
    let writes: Rc<RefCell<Vec<durable::Write>>> = Rc::new(RefCell::new(Vec::new()));
    let tbl = lua.create_table()?;
    tbl.set("seq", tx.seq())?;

    {
        let writes = writes.clone();
        let schema = schema.clone();
        tbl.set(
            "put",
            lua.create_function(move |lua, (path, value): (LuaValue, LuaValue)| {
                let navs = path_navs(lua, &schema, path)?;
                let val = json_to_cbor(&lua.from_value(value)?);
                let ws = dynpath::put(&schema, None, &navs, &val).map_err(mlua::Error::external)?;
                writes.borrow_mut().extend(ws);
                Ok(())
            })?,
        )?;
    }
    {
        let writes = writes.clone();
        let schema = schema.clone();
        tbl.set(
            "delete",
            lua.create_function(move |lua, path: LuaValue| {
                let navs = path_navs(lua, &schema, path)?;
                let w = dynpath::delete(&schema, None, &navs).map_err(mlua::Error::external)?;
                writes.borrow_mut().push(w);
                Ok(())
            })?,
        )?;
    }
    {
        let writes = writes.clone();
        let schema = schema.clone();
        tbl.set(
            "add",
            lua.create_function(move |lua, (path, delta): (LuaValue, LuaValue)| {
                let navs = path_navs(lua, &schema, path)?;
                let val = json_to_cbor(&lua.from_value(delta)?);
                let w = dynpath::add(&schema, None, &navs, &val).map_err(mlua::Error::external)?;
                writes.borrow_mut().push(w);
                Ok(())
            })?,
        )?;
    }
    {
        let writes = writes.clone();
        let schema = schema.clone();
        tbl.set(
            "push",
            lua.create_function(move |lua, (path, value): (LuaValue, LuaValue)| {
                let navs = path_navs(lua, &schema, path)?;
                let val = json_to_cbor(&lua.from_value(value)?);
                let w = dynpath::push(&schema, None, &navs, &val).map_err(mlua::Error::external)?;
                writes.borrow_mut().push(w);
                Ok(())
            })?,
        )?;
    }
    {
        let writes = writes.clone();
        let schema = schema.clone();
        tbl.set(
            "clear",
            lua.create_function(move |lua, path: LuaValue| {
                let navs = path_navs(lua, &schema, path)?;
                let w = dynpath::clear(&schema, None, &navs).map_err(mlua::Error::external)?;
                writes.borrow_mut().push(w);
                Ok(())
            })?,
        )?;
    }
    {
        let schema = schema.clone();
        tbl.set(
            "peek",
            lua.create_function(move |lua, path: LuaValue| {
                let navs = path_navs(lua, &schema, path)?;
                let v = dynpath::peek(&db, &schema, None, &navs).map_err(mlua::Error::external)?;
                lua.to_value(&cbor_to_json(&v))
            })?,
        )?;
    }

    let event_v = lua.to_value(event)?;
    reduce.call::<()>((tbl, event_v))?;
    for w in writes.borrow_mut().drain(..) {
        tx.write(w);
    }
    Ok(())
}

fn lua_shape(v: LuaValue) -> mlua::Result<Shape> {
    let Some(tbl) = v.as_table() else {
        return Err(mlua::Error::runtime("schema must be a store.* constructor value"));
    };
    let k: String = tbl.get("k")?;
    match k.as_str() {
        "leaf" => Ok(Shape::Leaf),
        "sum" => Ok(Shape::Sum),
        "map" => Ok(Shape::map(lua_shape(tbl.get("of")?)?)),
        "list" => Ok(Shape::list(lua_shape(tbl.get("of")?)?)),
        "deque" => Ok(Shape::deque(lua_shape(tbl.get("of")?)?)),
        "record" => {
            let fields: Table = tbl.get("fields")?;
            let n = fields.raw_len();
            let mut out = Vec::with_capacity(n);
            for i in 1..=n {
                let pair: Table = fields.get(i)?;
                let name: String = pair.get(1)?;
                let shape = lua_shape(pair.get(2)?)?;
                out.push((name, shape));
            }
            Ok(Shape::record(out))
        }
        other => Err(mlua::Error::runtime(format!("unknown shape tag {other:?}"))),
    }
}

fn path_navs(lua: &Lua, schema: &Shape, path: LuaValue) -> mlua::Result<Vec<Nav>> {
    let json: serde_json::Value = lua.from_value(path)?;
    let steps: Vec<_> = match json {
        serde_json::Value::Array(xs) => xs.iter().map(json_to_cbor).collect(),
        other => vec![json_to_cbor(&other)],
    };
    dynpath::navs_for(schema, &steps).map_err(mlua::Error::external)
}

fn path_query(lua: &Lua, schema: &Shape, path: LuaValue) -> mlua::Result<Query> {
    Ok(Query::new(path_navs(lua, schema, path)?))
}
