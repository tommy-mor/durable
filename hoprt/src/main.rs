//! hoprt spike host: one "server" Luau VM and two "browser" Luau VMs in one
//! process, connected by a queue that only carries serialized packets.
//!
//! The point: prove the hop runtime's semantics — coroutine suspension at
//! `at`, nested hops, error propagation across the wire, casts and fan-out —
//! with a real serialization boundary between VMs, before any compiler or
//! WebSocket exists. Swapping this queue for a WebSocket changes no
//! semantics; that is the claim being tested.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;

use mlua::{Function, Lua, LuaSerdeExt, Value as LuaValue};

type Queue = Rc<RefCell<VecDeque<serde_json::Value>>>;

fn lua_src(name: &str) -> String {
    let path = format!("{}/lua/{}", env!("CARGO_MANIFEST_DIR"), name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"))
}

fn compact(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// Build one VM: set identity globals, install the transport, load the
/// runtime and the (identical) application program.
fn make_vm(addr: &str, queue: Queue) -> mlua::Result<Lua> {
    let lua = Lua::new();
    let is_server = addr == "server";
    lua.globals().set("SIDE", if is_server { "server" } else { "browser" })?;
    if !is_server {
        lua.globals().set("SESSION", addr)?;
    }

    let from = addr.to_string();
    let send = lua.create_function(move |lua, pkt: LuaValue| {
        let json: serde_json::Value = lua.from_value(pkt)?;
        let to = json["to"].as_str().unwrap_or("?");
        let kind = json["kind"].as_str().unwrap_or("?");
        let detail = match kind {
            "reply" => format!("value={}", compact(&json["value"])),
            "error" => format!("err={}", compact(&json["err"])),
            _ => format!("{} vars={}", json["hop"].as_str().unwrap_or("?"), compact(&json["vars"])),
        };
        println!("        ~ wire {from:>7} -> {to:<8} {kind:<5} {detail}");
        queue.borrow_mut().push_back(json);
        Ok(())
    })?;
    lua.globals().set("__send", send)?;

    let print_fn = lua.create_function(|_, msg: String| {
        println!("{msg}");
        Ok(())
    })?;
    lua.globals().set("__print", print_fn)?;

    lua.load(lua_src("hoprt.lua")).set_name("hoprt.lua").exec()?;
    lua.load(lua_src("app.lua")).set_name("app.lua").exec()?;
    Ok(lua)
}

fn deliver(vms: &HashMap<String, Lua>, addr: &str, pkt: &serde_json::Value) -> mlua::Result<()> {
    let lua = vms
        .get(addr)
        .unwrap_or_else(|| panic!("no VM at address {addr}"));
    let value = lua.to_value(pkt)?;
    lua.globals().get::<Function>("__receive")?.call::<()>(value)
}

/// Drain the queue to quiescence. "browsers" fans out to every browser VM —
/// enumerating connected sessions at delivery time, per the design's
/// at-most-once contract.
fn pump(vms: &HashMap<String, Lua>, queue: &Queue) -> mlua::Result<()> {
    loop {
        let pkt = queue.borrow_mut().pop_front();
        let Some(pkt) = pkt else { break };
        match pkt["to"].as_str() {
            Some("browsers") => {
                let mut sessions: Vec<&String> =
                    vms.keys().filter(|a| a.as_str() != "server").collect();
                sessions.sort();
                for addr in sessions {
                    deliver(vms, addr, &pkt)?;
                }
            }
            Some(addr) => deliver(vms, addr, &pkt)?,
            None => panic!("packet without target: {}", compact(&pkt)),
        }
    }
    Ok(())
}

fn fire(vms: &HashMap<String, Lua>, addr: &str, entry: &str) -> mlua::Result<()> {
    vms[addr].globals().get::<Function>(entry)?.call::<()>(())
}

fn main() -> mlua::Result<()> {
    let queue: Queue = Rc::new(RefCell::new(VecDeque::new()));

    let mut vms = HashMap::new();
    for addr in ["server", "A", "B"] {
        vms.insert(addr.to_string(), make_vm(addr, queue.clone())?);
    }

    println!("== phase 1: sessions join (cast to server) =========================");
    fire(&vms, "A", "demo_join")?;
    fire(&vms, "B", "demo_join")?;
    pump(&vms, &queue)?;

    println!();
    println!("== phase 2: browser A fires four flows =============================");
    println!("   (round trip, round trip, server error caught, nested hops)");
    fire(&vms, "A", "demo_flows")?;
    pump(&vms, &queue)?;

    println!();
    println!("== phase 3: browser B draws; broadcast reaches A and B =============");
    fire(&vms, "B", "demo_stroke")?;
    pump(&vms, &queue)?;

    println!();
    for (addr, lua) in &vms {
        let (ok, flow): (bool, Option<String>) = lua
            .load("return rt.quiescent()")
            .set_name("quiescence-check")
            .call(())?;
        assert!(ok, "VM {addr} leaked a suspended flow: {flow:?}");
    }
    println!("done: queue drained and every VM is quiescent — no leaked flows");
    Ok(())
}
