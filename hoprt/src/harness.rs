//! The simulated cluster: one "server" Luau VM plus n "browser" Luau VMs in
//! one process, connected by a queue that carries only serialized packets.
//! Swapping the queue for a WebSocket changes no semantics — that's the
//! claim this harness exists to test.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use mlua::{Function, Lua, LuaSerdeExt, Value as LuaValue};

use crate::store;

type Queue = Rc<RefCell<VecDeque<serde_json::Value>>>;
type Log = Rc<RefCell<Vec<String>>>;

const HOPRT_LUA: &str = include_str!("../lua/hoprt.lua");
const HUI_LUA: &str = include_str!("../lua/hui.lua");

pub struct Host {
    vms: HashMap<String, Lua>,
    queue: Queue,
    log: Log,
    verbose: bool,
    data_dir: PathBuf,
    _keep: Option<tempfile::TempDir>,
}

fn compact(v: &serde_json::Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

fn emit(log: &Log, verbose: bool, line: String) {
    if verbose {
        println!("{line}");
    }
    log.borrow_mut().push(line);
}

impl Host {
    /// Build the cluster. Every VM loads the identical `app_code` program.
    /// The server VM gets a fresh durable data dir (temp).
    pub fn new(sessions: &[&str], app_code: &str, verbose: bool) -> mlua::Result<Self> {
        let keep = tempfile::tempdir().map_err(mlua::Error::external)?;
        let data_dir = keep.path().to_path_buf();
        Self::with_data(sessions, app_code, verbose, data_dir, Some(keep))
    }

    /// Build the cluster against an existing data directory (reopen / persist).
    pub fn with_data_dir(
        sessions: &[&str],
        app_code: &str,
        verbose: bool,
        data_dir: impl AsRef<Path>,
    ) -> mlua::Result<Self> {
        Self::with_data(
            sessions,
            app_code,
            verbose,
            data_dir.as_ref().to_path_buf(),
            None,
        )
    }

    fn with_data(
        sessions: &[&str],
        app_code: &str,
        verbose: bool,
        data_dir: PathBuf,
        keep: Option<tempfile::TempDir>,
    ) -> mlua::Result<Self> {
        let queue: Queue = Rc::new(RefCell::new(VecDeque::new()));
        let log: Log = Rc::new(RefCell::new(Vec::new()));
        let mut vms = HashMap::new();
        let mut addrs = vec!["server".to_string()];
        addrs.extend(sessions.iter().map(|s| s.to_string()));
        for addr in addrs {
            let vm = Self::make_vm(
                &addr,
                app_code,
                queue.clone(),
                log.clone(),
                verbose,
                &data_dir,
            )?;
            vms.insert(addr, vm);
        }
        Ok(Self {
            vms,
            queue,
            log,
            verbose,
            data_dir,
            _keep: keep,
        })
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Evaluate a Lua chunk on the server VM. Used by persistence tests.
    pub fn eval_server<R: mlua::FromLuaMulti>(&self, code: &str) -> mlua::Result<R> {
        self.vms["server"].load(code).set_name("eval-server").call(())
    }

    fn make_vm(
        addr: &str,
        app_code: &str,
        queue: Queue,
        log: Log,
        verbose: bool,
        data_dir: &Path,
    ) -> mlua::Result<Lua> {
        let lua = Lua::new();
        let is_server = addr == "server";
        lua.globals()
            .set("SIDE", if is_server { "server" } else { "browser" })?;
        if !is_server {
            lua.globals().set("SESSION", addr)?;
        }

        let from = addr.to_string();
        let send_log = log.clone();
        let send = lua.create_function(move |lua, pkt: LuaValue| {
            let json: serde_json::Value = lua.from_value(pkt)?;
            let to = json["to"].as_str().unwrap_or("?");
            let kind = json["kind"].as_str().unwrap_or("?");
            let detail = match kind {
                "reply" => format!("value={}", compact(&json["value"])),
                "error" => format!("err={}", compact(&json["err"])),
                _ => format!(
                    "{} vars={}",
                    json["hop"].as_str().unwrap_or("?"),
                    compact(&json["vars"])
                ),
            };
            emit(
                &send_log,
                verbose,
                format!("        ~ wire {from:>7} -> {to:<8} {kind:<5} {detail}"),
            );
            queue.borrow_mut().push_back(json);
            Ok(())
        })?;
        lua.globals().set("__send", send)?;

        let print_log = log.clone();
        let print_fn = lua.create_function(move |_, msg: String| {
            emit(&print_log, verbose, msg);
            Ok(())
        })?;
        lua.globals().set("__print", print_fn)?;

        lua.load(HOPRT_LUA).set_name("hoprt.lua").exec()?;
        lua.load(HUI_LUA).set_name("hui.lua").exec()?;
        if is_server {
            store::install_constructors(&lua)?;
        }
        lua.load(app_code).set_name("app.lua").exec()?;
        if is_server {
            store::bind(&lua, data_dir)?;
        }
        Ok(lua)
    }

    /// Simulate an event: call a global entry point on one VM.
    pub fn fire(&self, addr: &str, entry: &str) -> mlua::Result<()> {
        self.vms[addr].globals().get::<Function>(entry)?.call::<()>(())
    }

    /// Like `fire`, with one argument — e.g. clicking a rendered hui
    /// handler by calling `__handler_fire(id)`.
    pub fn fire_with(&self, addr: &str, entry: &str, arg: i64) -> mlua::Result<()> {
        self.vms[addr].globals().get::<Function>(entry)?.call::<()>(arg)
    }

    fn deliver(&self, addr: &str, pkt: &serde_json::Value) -> mlua::Result<()> {
        let lua = self
            .vms
            .get(addr)
            .unwrap_or_else(|| panic!("no VM at address {addr}"));
        let value = lua.to_value(pkt)?;
        lua.globals().get::<Function>("__receive")?.call::<()>(value)
    }

    /// Drain the queue to quiescence. "browsers" fans out to every browser
    /// VM — enumerating connected sessions at delivery time, per the
    /// design's at-most-once contract.
    pub fn pump(&self) -> mlua::Result<()> {
        loop {
            let pkt = self.queue.borrow_mut().pop_front();
            let Some(pkt) = pkt else { break };
            match pkt["to"].as_str() {
                Some("browsers") => {
                    let mut sessions: Vec<&String> =
                        self.vms.keys().filter(|a| a.as_str() != "server").collect();
                    sessions.sort();
                    for addr in sessions {
                        self.deliver(addr, &pkt)?;
                    }
                }
                Some(addr) => self.deliver(addr, &pkt)?,
                None => panic!("packet without target: {}", compact(&pkt)),
            }
        }
        Ok(())
    }

    /// Every VM must have zero suspended flows once the queue is drained; a
    /// leaked flow means a reply was lost or misrouted.
    pub fn assert_quiescent(&self) -> mlua::Result<()> {
        for (addr, lua) in &self.vms {
            let (ok, flow): (bool, Option<String>) = lua
                .load("return rt.quiescent()")
                .set_name("quiescence-check")
                .call(())?;
            assert!(ok, "VM {addr} leaked a suspended flow: {flow:?}");
        }
        Ok(())
    }

    /// The merged transcript: wire log and every VM's prints, in order.
    pub fn log(&self) -> Vec<String> {
        self.log.borrow().clone()
    }

    pub fn banner(&self, text: &str) {
        if self.verbose {
            println!("{text}");
        }
    }
}
