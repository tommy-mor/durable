//! The hop flow runtime — the Rust port of the old Lua-era runtime.
//!
//! A [`Vm`] is one side of the cluster (the server, or one browser
//! session). It owns the globals, the suspended-flow stacks, and the
//! browser-side hui handler registry. It knows nothing about transports:
//! packets go out through [`Platform::send`] and come back in through
//! [`Vm::receive`]. Packet shapes are the wire grammar of the Lua era,
//! now Value maps encoded as CBOR by the transport:
//!
//! ```text
//! { kind:"call",  flow, to, hop, vars, origin, user, reply_to }
//! { kind:"cast",  flow, to, hop, vars, origin, user }
//! { kind:"reply", flow, to, value }
//! { kind:"error", flow, to, err }
//! ```
//!
//! Reply routing is LIFO per (flow, VM): hops nest like calls. An error in
//! a remote segment unwinds hop by hop to the flow origin. Spawned flows
//! are scheduled and run when the current segment finishes (arguments are
//! evaluated eagerly at the spawn site).

use std::collections::HashMap;
use std::rc::Rc;

use crate::interp::{self, Exec, Globals, Host, NativeOut, Outcome};
use crate::ir::Program;
use crate::value::{NativeId, Value};

/// The pseudo-address effect calls are sent to. Not a VM: the platform
/// runs the effect off-thread and sends the reply itself.
pub const EFFECTS_ADDR: &str = "@effects";

/// What the embedder provides: transport, transcript, DOM (browser), and
/// the durable store (server). See harness.rs and serve.rs.
pub trait Platform {
    fn send(&mut self, pkt: Value);
    /// One transcript line; the platform labels it with the VM identity.
    fn print(&mut self, line: String);
    fn dom_get(&mut self, sel: &str) -> String;
    fn dom_set(&mut self, sel: &str, html: &str);
    fn dom_clear(&mut self, sel: &str);
    fn dom_focus(&mut self, sel: &str);
    /// Store natives (server VMs only). `None` = no store bound.
    fn store_native(
        &mut self,
        id: NativeId,
        args: Vec<Value>,
        prog: &Rc<Program>,
    ) -> Option<Result<Value, String>>;
}

#[derive(Clone, Debug, PartialEq)]
pub enum SideId {
    Server,
    Browser(String),
}

impl SideId {
    pub fn addr(&self) -> &str {
        match self {
            SideId::Server => "server",
            SideId::Browser(s) => s,
        }
    }
}

#[derive(Clone)]
struct FlowCtx {
    flow: String,
    /// origin session (Str) or Nil for server-origin flows
    origin: Value,
    /// the origin session's user (Str) or Nil. Stamped by hopd at the
    /// socket, propagated on every hop of the flow — never client-claimed.
    user: Value,
}

enum Complete {
    Done,
    Reply { to: Value },
}

struct Parked {
    exec: Exec,
    ctx: FlowCtx,
    on_complete: Complete,
}

/// Browser-side hiccup handler registry (hui).
#[derive(Default)]
pub struct HuiState {
    handlers: HashMap<i64, Value>,
    seq: i64,
    root_ids: HashMap<String, Vec<i64>>,
}

pub struct Vm {
    pub prog: Rc<Program>,
    pub globals: Globals,
    pub side: SideId,
    /// Browser VMs: this tab's user identity (from the hello packet).
    /// Server VMs: Nil — a server flow's user is its origin's.
    pub user: Value,
    hui: HuiState,
    stacks: HashMap<String, Vec<Parked>>,
    next_flow: u64,
}

fn mk_map(entries: Vec<(&str, Value)>) -> Value {
    Value::map(
        entries
            .into_iter()
            .filter(|(_, v)| !matches!(v, Value::Nil))
            .map(|(k, v)| (Value::str(k), v))
            .collect(),
    )
}

impl Vm {
    pub fn new(prog: Rc<Program>, side: SideId, platform: &mut dyn Platform) -> Result<Vm, String> {
        let mut globals = Globals::default();

        // language core
        for (name, id) in [
            ("print", NativeId::Print),
            ("error", NativeId::Error),
            ("tostring", NativeId::Tostring),
            ("tonumber", NativeId::Tonumber),
            ("push", NativeId::Push),
            ("len", NativeId::Len),
            ("sort_by", NativeId::SortBy),
            ("floor", NativeId::Floor),
            ("type", NativeId::TypeOf),
        ] {
            globals.set(name, Value::Native(id));
        }
        // the batteries: json, markdown, bash, llm — see modules.rs
        crate::modules::install(&mut globals);

        // the store is one callable: store(path). Its module surface —
        // shape constructors, the tape, navigators — is field access on
        // the native (interp resolves via builtins::store_field). dom and
        // hui stay plain module maps.
        globals.set("store", Value::Native(NativeId::StoreCall));
        let dom_mod = Value::map(
            [
                ("get", Value::Native(NativeId::DomGet)),
                ("set", Value::Native(NativeId::DomSet)),
                ("clear", Value::Native(NativeId::DomClear)),
                ("focus", Value::Native(NativeId::DomFocus)),
            ]
            .into_iter()
            .map(|(k, v)| (Value::str(k), v))
            .collect(),
        );
        globals.set("dom", dom_mod);
        let hui_mod = Value::map(
            [("render", Value::Native(NativeId::HuiRender))]
                .into_iter()
                .map(|(k, v)| (Value::str(k), v))
                .collect(),
        );
        globals.set("hui", hui_mod);

        // named functions
        for (name, fn_idx) in &prog.named {
            globals.set(
                name.clone(),
                Value::Closure(Rc::new(crate::value::ClosureVal {
                    fn_idx: *fn_idx,
                    caps: Vec::new(),
                })),
            );
        }

        let mut vm = Vm {
            prog,
            globals,
            side,
            user: Value::Nil,
            hui: HuiState::default(),
            stacks: HashMap::new(),
            next_flow: 0,
        };

        // `server let` initializers run on the server VM, in order
        if vm.side == SideId::Server {
            let lets: Vec<(String, usize)> = vm.prog.server_lets.clone();
            for (name, fn_idx) in lets {
                let exec = Exec::call(&vm.prog, fn_idx, Vec::new());
                let ctx =
                    FlowCtx { flow: format!("init:{name}"), origin: Value::Nil, user: Value::Nil };
                match vm.run_exec(platform, exec, &ctx) {
                    RunResult::Done(v) => vm.globals.set(name, v),
                    RunResult::Suspended(..) => {
                        return Err(format!("server let {name}: initializer cannot hop"))
                    }
                    RunResult::Failed(e) => return Err(format!("server let {name}: {e}")),
                }
            }
        }
        Ok(vm)
    }

    fn my_addr(&self) -> Value {
        Value::str(self.side.addr())
    }

    /// Fire an event: run global `name` as a new flow (a DOM event, a test
    /// entry point, an hopd connection hook).
    pub fn fire(&mut self, platform: &mut dyn Platform, name: &str, args: Vec<Value>) {
        let callee = self.globals.get(name);
        self.start_flow(platform, callee, args);
    }

    /// Activate a hui handler by id (an event in rendered HTML). `ev` is
    /// the DOM event as a value (a map with e.g. `key` for keyboard
    /// events), or Nil where no event exists (the harness).
    pub fn fire_handler(&mut self, platform: &mut dyn Platform, id: i64, ev: Value) {
        if let Some(h) = self.hui.handlers.get(&id).cloned() {
            self.start_flow(platform, h, vec![ev]);
        }
    }

    /// hopd calls this on the server VM when a session connects.
    pub fn session_connect(&mut self, platform: &mut dyn Platform, sid: &str, user: &str) {
        if !matches!(self.globals.get("on_connect"), Value::Nil) {
            self.fire(platform, "on_connect", vec![Value::str(sid), Value::str(user)]);
        }
    }

    /// hopd calls this on the server VM when a session goes away. The
    /// hook is best-effort presence, not a transaction: a crashed hopd
    /// never fires it.
    pub fn session_disconnect(&mut self, platform: &mut dyn Platform, sid: &str, user: &str) {
        if !matches!(self.globals.get("on_disconnect"), Value::Nil) {
            self.fire(platform, "on_disconnect", vec![Value::str(sid), Value::str(user)]);
        }
    }

    pub fn start_flow(&mut self, platform: &mut dyn Platform, callee: Value, args: Vec<Value>) {
        self.next_flow += 1;
        let flow = format!("{}#{}", self.side.addr(), self.next_flow);
        let origin = match &self.side {
            SideId::Browser(s) => Value::str(s.as_str()),
            SideId::Server => Value::Nil,
        };
        let ctx = FlowCtx { flow, origin, user: self.user.clone() };
        let exec = match Exec::call_value(&self.prog, &callee, args) {
            Ok(e) => e,
            Err(e) => {
                platform.print(format!("!! unhandled flow error: {e}"));
                return;
            }
        };
        self.step(platform, exec, ctx, Complete::Done);
    }

    /// Run a global function to completion, synchronously, returning its
    /// value. It may cast and spawn but not hop (used by the harness for
    /// server-side reads; a server-origin flow with no session).
    pub fn call_sync(
        &mut self,
        platform: &mut dyn Platform,
        name: &str,
        args: Vec<Value>,
    ) -> Result<Value, String> {
        let callee = self.globals.get(name);
        let exec = Exec::call_value(&self.prog, &callee, args)?;
        self.next_flow += 1;
        let ctx = FlowCtx {
            flow: format!("{}#sync{}", self.side.addr(), self.next_flow),
            origin: Value::Nil,
            user: Value::Nil,
        };
        match self.run_exec(platform, exec, &ctx) {
            RunResult::Done(v) => Ok(v),
            RunResult::Suspended(..) => Err(format!("call_sync {name}: cannot hop")),
            RunResult::Failed(e) => Err(e),
        }
    }

    /// Transport delivery point.
    pub fn receive(&mut self, platform: &mut dyn Platform, pkt: Value) {
        let kind = pkt.get_field("kind");
        let kind = kind.as_str().unwrap_or("");
        match kind {
            "call" | "cast" => {
                let hop = pkt.get_field("hop");
                let hop = hop.as_str().unwrap_or("").to_string();
                let flow = pkt.get_field("flow");
                let flow_s = flow.as_str().unwrap_or("?").to_string();
                let Some(&fn_idx) = self.prog.hops.get(&hop) else {
                    if kind == "call" {
                        platform.send(mk_map(vec![
                            ("kind", Value::str("error")),
                            ("flow", flow),
                            ("to", pkt.get_field("reply_to")),
                            ("err", Value::str(format!("unknown hop: {hop}"))),
                        ]));
                    } else {
                        platform.print(format!("!! unknown hop: {hop} (cast dropped)"));
                    }
                    return;
                };
                let exec = Exec::call(&self.prog, fn_idx, vec![pkt.get_field("vars")]);
                let ctx = FlowCtx {
                    flow: flow_s,
                    origin: pkt.get_field("origin"),
                    user: pkt.get_field("user"),
                };
                let on_complete = if kind == "call" {
                    Complete::Reply { to: pkt.get_field("reply_to") }
                } else {
                    Complete::Done
                };
                self.step(platform, exec, ctx, on_complete);
            }
            "reply" | "error" => {
                let flow = pkt.get_field("flow");
                let flow = flow.as_str().unwrap_or("?").to_string();
                let Some(stack) = self.stacks.get_mut(&flow) else {
                    let extra = if kind == "error" {
                        format!(": {}", crate::value::coerce_str(&pkt.get_field("err")))
                    } else {
                        String::new()
                    };
                    platform.print(format!(
                        "!! {kind} for a flow with nothing suspended: {flow}{extra}"
                    ));
                    return;
                };
                let parked = stack.pop().expect("stacks never hold empty vecs");
                if stack.is_empty() {
                    self.stacks.remove(&flow);
                }
                if kind == "reply" {
                    let Parked { mut exec, ctx, on_complete } = parked;
                    if let Some(top) = exec.frames.last_mut() {
                        top.stack.push(pkt.get_field("value"));
                    }
                    self.step(platform, exec, ctx, on_complete);
                } else {
                    // errors unwind: no resume, the failure propagates
                    let err = pkt.get_field("err");
                    let msg = crate::value::coerce_str(&err);
                    self.dispose_error(platform, parked.ctx, parked.on_complete, msg);
                }
            }
            other => platform.print(format!("!! unknown packet kind: {other}")),
        }
    }

    /// True when nothing on this side is suspended awaiting a reply.
    pub fn quiescent(&self) -> Result<(), String> {
        match self.stacks.keys().next() {
            None => Ok(()),
            Some(flow) => Err(format!("suspended flow: {flow}")),
        }
    }

    // -- internals ---------------------------------------------------------

    /// Drive an exec to its next boundary and dispose of the outcome —
    /// the port of the old runtime's step().
    fn step(&mut self, platform: &mut dyn Platform, exec: Exec, ctx: FlowCtx, on_complete: Complete) {
        match self.run_exec(platform, exec, &ctx) {
            RunResult::Done(v) => match on_complete {
                Complete::Reply { to } => {
                    platform.send(mk_map(vec![
                        ("kind", Value::str("reply")),
                        ("flow", Value::str(ctx.flow.as_str())),
                        ("to", to),
                        ("value", v),
                    ]));
                }
                Complete::Done => {}
            },
            RunResult::Suspended(exec, target, hop, vars) => {
                platform.send(mk_map(vec![
                    ("kind", Value::str("call")),
                    ("flow", Value::str(ctx.flow.as_str())),
                    ("to", target),
                    ("hop", Value::str(hop.as_str())),
                    ("vars", vars),
                    ("origin", ctx.origin.clone()),
                    ("user", ctx.user.clone()),
                    ("reply_to", self.my_addr()),
                ]));
                self.stacks
                    .entry(ctx.flow.clone())
                    .or_default()
                    .push(Parked { exec, ctx, on_complete });
            }
            RunResult::Failed(e) => self.dispose_error(platform, ctx, on_complete, e),
        }
    }

    fn dispose_error(
        &mut self,
        platform: &mut dyn Platform,
        ctx: FlowCtx,
        on_complete: Complete,
        msg: String,
    ) {
        match on_complete {
            Complete::Reply { to } => {
                platform.send(mk_map(vec![
                    ("kind", Value::str("error")),
                    ("flow", Value::str(ctx.flow.as_str())),
                    ("to", to),
                    ("err", Value::str(msg)),
                ]));
            }
            Complete::Done => {
                platform.print(format!("!! unhandled flow error: {msg}"));
            }
        }
    }

    /// Run one exec against this VM's globals, then run any flows it
    /// spawned. Returns the exec's own outcome.
    fn run_exec(&mut self, platform: &mut dyn Platform, mut exec: Exec, ctx: &FlowCtx) -> RunResult {
        let prog = self.prog.clone();
        let mut spawned: Vec<(Value, Vec<Value>)> = Vec::new();
        let outcome = {
            let mut host = StepHost {
                side: &self.side,
                ctx,
                vm_user: &self.user,
                hui: &mut self.hui,
                platform,
                spawned: &mut spawned,
                prog: &prog,
            };
            interp::run(&prog, &mut exec, &mut self.globals, &mut host)
        };
        let result = match outcome {
            Outcome::Done(v) => RunResult::Done(v),
            Outcome::Suspend { target, hop, vars } => RunResult::Suspended(exec, target, hop, vars),
            Outcome::Error(e) => RunResult::Failed(e),
        };
        // spawned flows are scheduled: they run when the spawning segment
        // has reached its own boundary
        for (callee, args) in spawned {
            self.start_flow(platform, callee, args);
        }
        result
    }
}

enum RunResult {
    Done(Value),
    Suspended(Exec, Value, String, Value),
    Failed(String),
}

// ---------------------------------------------------------------------------
// The per-step Host: bridges interpreter needs to VM state + platform
// ---------------------------------------------------------------------------

struct StepHost<'a> {
    side: &'a SideId,
    ctx: &'a FlowCtx,
    /// The VM's own user (browser tabs); Nil on the server.
    vm_user: &'a Value,
    hui: &'a mut HuiState,
    platform: &'a mut dyn Platform,
    spawned: &'a mut Vec<(Value, Vec<Value>)>,
    prog: &'a Rc<Program>,
}

impl Host for StepHost<'_> {
    fn print(&mut self, line: String) {
        self.platform.print(line);
    }

    fn cast(&mut self, target: Value, hop: &str, vars: Value) -> Result<(), String> {
        self.platform.send(mk_map(vec![
            ("kind", Value::str("cast")),
            ("flow", Value::str(self.ctx.flow.as_str())),
            ("to", target),
            ("hop", Value::str(hop)),
            ("vars", vars),
            ("origin", self.ctx.origin.clone()),
            ("user", self.ctx.user.clone()),
        ]));
        Ok(())
    }

    fn spawn(&mut self, callee: Value, args: Vec<Value>) -> Result<(), String> {
        self.spawned.push((callee, args));
        Ok(())
    }

    fn session(&mut self) -> Result<Value, String> {
        match self.side {
            SideId::Browser(s) => Ok(Value::str(s.as_str())),
            SideId::Server => match &self.ctx.origin {
                Value::Nil => Err("session() in a server-origin flow with no session".into()),
                v => Ok(v.clone()),
            },
        }
    }

    fn user(&mut self) -> Result<Value, String> {
        // browser: the tab's own identity; server: whatever hopd stamped
        // on the packet that started (or continued) this flow.
        let u = match self.side {
            SideId::Browser(_) => self.vm_user.clone(),
            SideId::Server => self.ctx.user.clone(),
        };
        match u {
            Value::Nil => Err("user() in a flow with no user".into()),
            v => Ok(v),
        }
    }

    fn native(&mut self, id: NativeId, args: Vec<Value>) -> Result<NativeOut, String> {
        match id {
            NativeId::DomGet => {
                let sel = args
                    .first()
                    .and_then(Value::as_str)
                    .ok_or("dom.get(selector)")?;
                Ok(NativeOut::Val(Value::str(self.platform.dom_get(sel))))
            }
            NativeId::DomSet => {
                let sel = args.first().and_then(Value::as_str).ok_or("dom.set(selector, html)")?;
                let html = args.get(1).map(crate::value::coerce_str).unwrap_or_default();
                self.platform.dom_set(sel, &html);
                Ok(NativeOut::Val(Value::Nil))
            }
            NativeId::DomClear => {
                let sel = args.first().and_then(Value::as_str).ok_or("dom.clear(selector)")?;
                self.platform.dom_clear(sel);
                Ok(NativeOut::Val(Value::Nil))
            }
            NativeId::DomFocus => {
                let sel = args.first().and_then(Value::as_str).ok_or("dom.focus(selector)")?;
                self.platform.dom_focus(sel);
                Ok(NativeOut::Val(Value::Nil))
            }
            NativeId::HuiRender => {
                let sel = args
                    .first()
                    .and_then(Value::as_str)
                    .ok_or("hui.render(selector, node)")?
                    .to_string();
                let node = args.get(1).cloned().unwrap_or(Value::Nil);
                let html = crate::builtins::hui_render(self.hui, &sel, &node)?;
                self.platform.dom_set(&sel, &html);
                Ok(NativeOut::Val(Value::Nil))
            }
            // store natives are the platform's (server has one; a browser
            // platform answers None and this errors cleanly)
            other => match self.platform.store_native(other, args, self.prog) {
                Some(r) => r.map(NativeOut::Val),
                None => Err(format!("{other:?} is not available on this side")),
            },
        }
    }

    /// Module effects suspend the flow toward "@effects": the tab never
    /// gets these capabilities (and hopd's VM ignores hop ids it didn't
    /// mint, so a forged packet can't reach them).
    fn effect(&mut self, name: &str, hop: &str, vars: Value) -> Result<NativeOut, String> {
        if !matches!(self.side, SideId::Server) {
            return Err(format!("{name} is server-side only"));
        }
        Ok(NativeOut::Suspend {
            target: Value::str(EFFECTS_ADDR),
            hop: hop.to_string(),
            vars,
        })
    }
}

// hui internals live in builtins.rs but operate on HuiState
impl HuiState {
    pub(crate) fn register(&mut self, sel: &str, h: Value) -> i64 {
        self.seq += 1;
        self.handlers.insert(self.seq, h);
        self.root_ids.entry(sel.to_string()).or_default().push(self.seq);
        self.seq
    }

    pub(crate) fn release_root(&mut self, sel: &str) {
        for id in self.root_ids.remove(sel).unwrap_or_default() {
            self.handlers.remove(&id);
        }
    }
}
