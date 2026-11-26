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
//! { kind:"cancel", flow, to }        the other half of `flow` died
//! ```
//!
//! Every packet entering the server also carries `from`, stamped by the
//! transport, never by the sender (hopd's socket ingress, the effects
//! executor, the harness router). A reply or error resumes a parked level
//! only if `from` is the party that level awaits — flow ids are guessable,
//! so the flow id alone is no proof.
//!
//! Reply routing is LIFO per (flow, VM): hops nest like calls. An error in
//! a remote segment unwinds hop by hop to the flow origin. Named flows
//! birth and run when the current segment finishes (key, callee, and
//! arguments are evaluated eagerly at the flow site; the callee body
//! never runs in the spawner).

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
    /// Park the current flow in `store.wait`: suspend like an effect hop,
    /// registering (flow, watch prefix, read seq) with the store. The
    /// default refuses — browser platforms never park on the store.
    /// Returns `Some(reply)` when the wait resolved immediately
    /// (a commit slipped in between read and park), `None` to suspend.
    fn store_wait(
        &mut self,
        _path: Value,
        _flow: &str,
        _read_seq: u64,
        _origin: &str,
    ) -> Option<Result<Value, String>> {
        Some(Err("store.wait is server-side only".into()))
    }
    /// Flows the last commit woke. Drained after every store append that
    /// ran on this platform — the courier's delivery queue.
    fn drain_woken(&mut self) -> Vec<String> {
        Vec::new()
    }
    /// Resume a parked waiter with a wake value.
    fn resume_waiter(&mut self, _flow: &str, _value: Value) {}
    /// Drop one parked waiter (cancel by handle). Server platforms drop
    /// her watch; browser platforms never hold any.
    fn cancel_waiter(&mut self, _flow: &str) {}
    /// Drop every parked waiter whose flow belongs to session `sid`
    /// (tab close ends its flows).
    fn cancel_waiters_of_origin(&mut self, _sid: &str) {}
    /// Applied seq as of the last store read on this platform — stamps
    /// the flow's invisible read_seq after every query.
    fn last_read_seq(&self) -> u64 {
        0
    }
    /// Execution trace (hopd --trace): one greppable line per event.
    /// Default off.
    fn tracing(&self) -> bool {
        false
    }
    fn trace(&mut self, _line: String) {}
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
    /// Commit point of the flow's last store read: the applied seq when
    /// it read. Rides invisibly like origin so a write landing between
    /// read and wait wakes at once instead of slipping through.
    read_seq: u64,
    /// birth key (`flow "key" ...`), if born named. Replacement matches
    /// on (origin, key); anonymous births (fire, tests, init) hold None
    /// and are never replaced.
    key: Option<String>,
}

enum Complete {
    Done,
    Reply { to: Value },
}

struct Parked {
    exec: Exec,
    ctx: FlowCtx,
    on_complete: Complete,
    /// Who may resume this level: the address the hop went to (a session,
    /// "server", "@effects"), or "@wait" for a store.wait park (only the
    /// courier wakes those, never a packet). A reply or error from anyone
    /// else is dropped: flow ids are guessable, so without this any tab
    /// could answer another tab's parked flow with a value of her choosing.
    awaiting: Value,
}

/// Browser-side hiccup handler registry (hui). Per-render mint: every
/// render releases the previous generation and registers fresh ids. The
/// morph delivers the new attrs with every paint, so the DOM carries
/// current ids except within a single dispatch — the click-lost-in-transit
/// bug was the megabyte wire flood, not this registry.
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
                let ctx = FlowCtx {
                    flow: format!("init:{name}"),
                    origin: Value::Nil,
                    user: Value::Nil,
                    read_seq: 0,
                    key: None,
                };
                match vm.run_exec(platform, exec, &mut ctx.clone()) {
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
    /// `on_connect` takes no arguments and runs already owned by the tab:
    /// origin and user are stamped on its context (never client-claimed),
    /// so `flow` births inside it land under the tab and `session()` /
    /// `user()` read back what the socket proved.
    pub fn session_connect(&mut self, platform: &mut dyn Platform, sid: &str, user: &str) {
        if !matches!(self.globals.get("on_connect"), Value::Nil) {
            let callee = self.globals.get("on_connect");
            let flow = self.mint_flow();
            self.start_flow_with_id(
                platform,
                flow,
                Value::str(sid),
                Value::str(user),
                None,
                callee,
                Vec::new(),
            );
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

    /// Mint a fresh flow id on this side's namespace without starting anything.
    /// Hands the birther nothing back — the child runs under this id at
    /// the next segment boundary.
    pub fn mint_flow(&mut self) -> String {
        self.next_flow += 1;
        format!("{}#{}", self.side.addr(), self.next_flow)
    }

    pub fn start_flow(&mut self, platform: &mut dyn Platform, callee: Value, args: Vec<Value>) {
        let flow = self.mint_flow();
        let origin = match &self.side {
            SideId::Browser(s) => Value::str(s.as_str()),
            SideId::Server => Value::Nil,
        };
        self.start_flow_with_id(platform, flow, origin, self.user.clone(), None, callee, args);
    }

    /// Start a flow under an already-minted id with explicit ownership.
    /// `flow` births pass the spawner's origin/user so a server-born
    /// child still belongs to the tab that caused it; anonymous births
    /// (fire, tests, init) pass side-derived ownership and no key.
    pub fn start_flow_with_id(
        &mut self,
        platform: &mut dyn Platform,
        flow: String,
        origin: Value,
        user: Value,
        key: Option<String>,
        callee: Value,
        args: Vec<Value>,
    ) {
        let ctx = FlowCtx { flow, origin, user, read_seq: 0, key };
        let exec = match Exec::call_value(&self.prog, &callee, args) {
            Ok(e) => e,
            Err(e) => {
                platform.print(format!("!! unhandled flow error: {e}"));
                return;
            }
        };
        self.step(platform, exec, ctx, Complete::Done);
    }

    /// Kill every flow of `origin` holding `key`, except `except` (the
    /// running spawner never kills herself — the new flow still starts).
    /// Replacement releases far halves, exactly like a death: the other
    /// side drops her half and hears nothing further. Returns how many
    /// flows died.
    pub fn kill_key(
        &mut self,
        platform: &mut dyn Platform,
        origin: &Value,
        key: &str,
        except: &str,
    ) -> usize {
        let doomed: Vec<String> = self
            .stacks
            .iter()
            .filter_map(|(flow, stack)| {
                if flow == except {
                    return None;
                }
                stack.last().and_then(|p| match (&p.ctx.origin, &p.ctx.key) {
                    (o, Some(k)) if o == origin && k == key => Some(flow.clone()),
                    _ => None,
                })
            })
            .collect();
        let n = doomed.len();
        for flow in doomed {
            if platform.tracing() {
                platform.trace(format!("flow {flow} replaced (key {key})"));
            }
            let (_, peers) = self.drop_flow(platform, &flow);
            for to in peers {
                self.send_cancel(platform, &flow, to);
            }
        }
        n
    }

    /// Drop every parked level of `flow` on this VM and her store.wait
    /// watch. Returns whether anything was parked, and the peers she was
    /// bound to — the parties her levels await or would reply to — which
    /// are exactly the VMs holding her other half.
    fn drop_flow(&mut self, platform: &mut dyn Platform, flow: &str) -> (bool, Vec<Value>) {
        platform.cancel_waiter(flow);
        let Some(stack) = self.stacks.remove(flow) else {
            return (false, Vec::new());
        };
        let me = self.my_addr();
        let mut peers: Vec<Value> = Vec::new();
        for level in &stack {
            let reply_to = match &level.on_complete {
                Complete::Reply { to } => Some(to.clone()),
                Complete::Done => None,
            };
            for addr in reply_to.into_iter().chain(std::iter::once(level.awaiting.clone())) {
                let internal = matches!(addr.as_str(), Some(a) if a.starts_with('@'));
                if !internal && addr != me && !matches!(addr, Value::Nil) && !peers.contains(&addr) {
                    peers.push(addr);
                }
            }
        }
        (true, peers)
    }

    fn send_cancel(&self, platform: &mut dyn Platform, flow: &str, to: Value) {
        if platform.tracing() {
            platform.trace(format!("flow {flow} cancel -> {to}"));
        }
        platform.send(mk_map(vec![
            ("kind", Value::str("cancel")),
            ("flow", Value::str(flow)),
            ("to", to),
        ]));
    }

    /// Resume a flow parked in store.wait with a wake value: pop one
    /// parked level (a waiter sleeps exactly one frame deep, at her wait
    /// call site) and push the value as the call's return. No-op when the
    /// flow is gone (cancelled, or never parked).
    pub fn resume_waiter_flow(&mut self, platform: &mut dyn Platform, flow: &str, value: Value) {
        let Some(stack) = self.stacks.get_mut(flow) else {
            return;
        };
        let parked = stack.pop().expect("stacks never hold empty vecs");
        if stack.is_empty() {
            self.stacks.remove(flow);
        }
        let Parked { mut exec, ctx, on_complete, .. } = parked;
        if let Some(top) = exec.frames.last_mut() {
            top.stack.push(value);
        }
        self.step(platform, exec, ctx, on_complete);
    }

    /// Kill every flow whose origin session is `sid`. Server-side only.
    /// One caller: a tab closing (her VM is gone whole — no notify, the
    /// far side hears nothing because there is nobody to hear it).
    /// Re-births use [`Vm::kill_key`]; far-half deaths arrive as
    /// `cancel` packets. Waiters die through the platform hook; parked
    /// stacks drop here. Returns how many flows died.
    pub fn kill_origin(&mut self, platform: &mut dyn Platform, sid: &str, notify: bool) -> usize {
        platform.cancel_waiters_of_origin(sid);
        let doomed: Vec<String> = self
            .stacks
            .iter()
            .filter_map(|(flow, stack)| {
                stack.last().and_then(|p| match &p.ctx.origin {
                    Value::Str(o) if o.as_ref() == sid => Some(flow.clone()),
                    _ => None,
                })
            })
            .collect();
        let n = doomed.len();
        for flow in doomed {
            let (_, peers) = self.drop_flow(platform, &flow);
            if notify {
                for to in peers {
                    self.send_cancel(platform, &flow, to);
                }
            }
        }
        n
    }

    /// Run a global function to completion, synchronously, returning its
    /// value. It may cast and birth flows but not hop (used by the harness for
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
        let mut ctx = FlowCtx {
            flow: format!("{}#sync{}", self.side.addr(), self.next_flow),
            origin: Value::Nil,
            user: Value::Nil,
            read_seq: 0,
            key: None,
        };
        match self.run_exec(platform, exec, &mut ctx) {
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
                    key: pkt.get_field("key").as_str().map(|s| s.to_string()),
                    read_seq: pkt
                        .get_field("read_seq")
                        .as_str()
                        .and_then(|x| x.parse::<u64>().ok())
                        .or_else(|| match pkt.get_field("read_seq") {
                            crate::value::Value::Int(n) => Some(n as u64),
                            _ => None,
                        })
                        .unwrap_or(0),
                };
                let on_complete = if kind == "call" {
                    Complete::Reply { to: pkt.get_field("reply_to") }
                } else {
                    Complete::Done
                };
                if platform.tracing() {
                    platform.trace(format!(
                        "flow {} recv {kind} {hop} vars={}",
                        ctx.flow,
                        pkt.get_field("vars")
                    ));
                }
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
                // Only the party this level awaits may answer it. The
                // transport stamps `from` (hopd at socket ingress, the
                // effects executor, the harness router); the server
                // requires it, a browser checks it when present (her only
                // peer is the server).
                let from = pkt.get_field("from");
                let awaiting = &stack.last().expect("stacks never hold empty vecs").awaiting;
                let authentic = match (&self.side, &from) {
                    (SideId::Server, f) => f == awaiting,
                    (SideId::Browser(_), Value::Nil) => true,
                    (SideId::Browser(_), f) => f == awaiting,
                };
                if !authentic {
                    platform.print(format!(
                        "!! {kind} for flow {flow} from {} dropped: she awaits {}",
                        crate::value::coerce_str(&from),
                        crate::value::coerce_str(awaiting),
                    ));
                    return;
                }
                if platform.tracing() {
                    let detail = if kind == "reply" {
                        format!("value={}", pkt.get_field("value"))
                    } else {
                        format!("err={}", crate::value::coerce_str(&pkt.get_field("err")))
                    };
                    platform.trace(format!("flow {flow} recv {kind} {detail}"));
                }
                let parked = stack.pop().expect("stacks never hold empty vecs");
                if stack.is_empty() {
                    self.stacks.remove(&flow);
                }
                if kind == "reply" {
                    let Parked { mut exec, mut ctx, on_complete, .. } = parked;
                    // Per-flow monotonic commit point: neither side's copy
                    // may go backwards. The browser never reads the store
                    // (its copy stays 0); without this max, her next hop
                    // would clobber the server's stamp and every wait would
                    // wake instantly — an infinite render chain.
                    let incoming = pkt
                        .get_field("read_seq")
                        .as_str()
                        .and_then(|x| x.parse::<u64>().ok())
                        .or_else(|| match pkt.get_field("read_seq") {
                            crate::value::Value::Int(n) => Some(n as u64),
                            _ => None,
                        })
                        .unwrap_or(0);
                    if incoming > ctx.read_seq {
                        ctx.read_seq = incoming;
                    }
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
            "cancel" => {
                // The other half of a flow died; drop ours. On the server
                // only the flow's own tab may cancel it (a tab can't kill
                // another tab's panels by guessing ids); a tab hears
                // cancels only from the server. No echo back to the sender.
                let flow = pkt.get_field("flow");
                let flow = flow.as_str().unwrap_or("?").to_string();
                let from = pkt.get_field("from");
                let allowed = match &self.side {
                    SideId::Server => self
                        .stacks
                        .get(&flow)
                        .map(|stack| stack.iter().all(|l| l.ctx.origin == from))
                        .unwrap_or(false),
                    SideId::Browser(_) => matches!(from, Value::Nil) || from == Value::str("server"),
                };
                if !allowed {
                    if self.stacks.contains_key(&flow) {
                        platform.print(format!(
                            "!! cancel for flow {flow} from {} dropped: not hers",
                            crate::value::coerce_str(&from)
                        ));
                    }
                    return;
                }
                if platform.tracing() {
                    platform.trace(format!("flow {flow} recv cancel"));
                }
                let (_, peers) = self.drop_flow(platform, &flow);
                for to in peers {
                    if to != from {
                        self.send_cancel(platform, &flow, to);
                    }
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

    /// Flow ids parked on this side (hops awaiting replies AND waiters
    /// sleeping in store.wait). A wait-loop panel lives here by design —
    /// parked is her sleeping state, not a leak.
    pub fn parked_flows(&self) -> Vec<String> {
        let mut out: Vec<String> = self.stacks.keys().cloned().collect();
        out.sort();
        out
    }

    // -- internals ---------------------------------------------------------

    /// Drive an exec to its next boundary and dispose of the outcome —
    /// the port of the old runtime's step().
    ///
    /// A hop targeting this VM's own address runs the segment inline,
    /// no wire: a mark to the side you're already on is a no-op stay,
    /// so birth site (tab or server) never changes what a panel does.
    fn step(&mut self, platform: &mut dyn Platform, mut exec: Exec, mut ctx: FlowCtx, mut on_complete: Complete) {
        loop {
            match self.run_exec(platform, exec, &mut ctx) {
                RunResult::Done(v) => {
                    if platform.tracing() {
                        platform.trace(format!("flow {} done value={v}", ctx.flow));
                    }
                    match on_complete {
                        Complete::Reply { to } => {
                            platform.send(mk_map(vec![
                                ("kind", Value::str("reply")),
                                ("flow", Value::str(ctx.flow.as_str())),
                                ("to", to),
                                ("value", v),
                                ("read_seq", Value::Int(ctx.read_seq as i64)),
                            ]));
                        }
                        Complete::Done => {}
                    }
                    return;
                }
                RunResult::Suspended(next_exec, target, hop, vars) => {
                    if target == self.my_addr() {
                        if platform.tracing() {
                            platform.trace(format!(
                                "flow {} self-hop {} vars={}",
                                ctx.flow, hop, vars
                            ));
                        }
                        let prog = self.prog.clone();
                        let Some(&fn_idx) = prog.hops.get(&hop) else {
                            self.dispose_error(platform, ctx, on_complete, format!("unknown hop: {hop}"));
                            return;
                        };
                        exec = Exec::call(&prog, fn_idx, vec![vars]);
                        continue;
                    }
                    // A store.wait park never touches the wire: the flow
                    // sleeps on the Vm itself until the courier wakes her.
                    if target.as_str() == Some("@wait") {
                        if platform.tracing() {
                            platform.trace(format!("flow {} parked in store.wait", ctx.flow));
                        }
                        self.stacks
                            .entry(ctx.flow.clone())
                            .or_default()
                            .push(Parked { exec: next_exec, ctx, on_complete, awaiting: Value::str("@wait") });
                        return;
                    }
                    if platform.tracing() {
                        platform.trace(format!(
                            "flow {} hop -> {} {} vars={}",
                            ctx.flow, target, hop, vars
                        ));
                    }
                    platform.send(mk_map(vec![
                        ("kind", Value::str("call")),
                        ("flow", Value::str(ctx.flow.as_str())),
                        ("to", target.clone()),
                        ("hop", Value::str(hop.as_str())),
                        ("vars", vars),
                        ("origin", ctx.origin.clone()),
                        ("user", ctx.user.clone()),
                        ("read_seq", Value::Int(ctx.read_seq as i64)),
                        (
                            "key",
                            ctx.key.clone().map(Value::str).unwrap_or(Value::Nil),
                        ),
                        ("reply_to", self.my_addr()),
                    ]));
                    self.stacks
                        .entry(ctx.flow.clone())
                        .or_default()
                        .push(Parked { exec: next_exec, ctx, on_complete, awaiting: target });
                    return;
                }
                RunResult::Failed(e) => {
                    self.dispose_error(platform, ctx, on_complete, e);
                    return;
                }
            }
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

    /// Run one exec against this VM's globals, then birth any flows it
    /// named. Returns the exec's own outcome.
    fn run_exec(&mut self, platform: &mut dyn Platform, mut exec: Exec, ctx: &mut FlowCtx) -> RunResult {
        let prog = self.prog.clone();
        let mut flow_ops: Vec<FlowOp> = Vec::new();
        let mut woken: Vec<String> = Vec::new();
        let outcome = {
            let mut host = StepHost {
                side: &self.side,
                ctx,
                vm_user: &self.user,
                hui: &mut self.hui,
                platform,
                flow_ops: &mut flow_ops,
                woken: &mut woken,
                prog: &prog,
            };
            interp::run(&prog, &mut exec, &mut self.globals, &mut host)
        };
        // The courier woke parked waiters mid-step: resume them now that
        // the platform borrow has ended.
        for flow in woken {
            self.resume_waiter_flow(platform, &flow, Value::str("changed"));
        }
        let result = match outcome {
            Outcome::Done(v) => RunResult::Done(v),
            Outcome::Suspend { target, hop, vars } => RunResult::Suspended(exec, target, hop, vars),
            Outcome::Error(e) => RunResult::Failed(e),
        };
        // queued flow births apply in program order when the birthing
        // segment has reached its own boundary — after woken resumes, so
        // a birth later in the program replaces what an earlier one
        // started even if the courier woke her mid-step.
        for op in flow_ops {
            match op {
                FlowOp::Flow { key, origin, user, callee, args } => {
                    self.kill_key(platform, &origin, &key, "");
                    let flow = self.mint_flow();
                    self.start_flow_with_id(platform, flow, origin, user, Some(key), callee, args)
                }
            }
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

/// One queued flow operation: birth the named flow. Queued in program
/// order during the step, applied at the segment boundary with atomic
/// replace — a `flow` later in the program replaces what an earlier one
/// started, which is what room-switch wants.
pub enum FlowOp {
    Flow { key: String, origin: Value, user: Value, callee: Value, args: Vec<Value> },
}

struct StepHost<'a> {
    side: &'a SideId,
    ctx: &'a mut FlowCtx,
    /// The VM's own user (browser tabs); Nil on the server.
    vm_user: &'a Value,
    hui: &'a mut HuiState,
    platform: &'a mut dyn Platform,
    /// Flow operations queued this step, in program order: named births
    /// (with the spawner's origin/user, so a server-born child still
    /// belongs to the tab that caused it). Applied at the segment
    /// boundary after woken resumes — a flow later in the program
    /// replaces what an earlier one birthed under the same key.
    flow_ops: &'a mut Vec<FlowOp>,
    /// Flows the courier woke during this step: resumed on the Vm once
    /// the platform borrow ends.
    woken: &'a mut Vec<String>,
    prog: &'a Rc<Program>,
}

impl Host for StepHost<'_> {
    fn print(&mut self, line: String) {
        self.platform.print(line);
    }

    fn tracing(&self) -> bool {
        self.platform.tracing()
    }

    fn trace(&mut self, line: String) {
        self.platform.trace(line);
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
            ("read_seq", Value::Int(self.ctx.read_seq as i64)),
        ]));
        Ok(())
    }

    fn flow(&mut self, key: String, callee: Value, args: Vec<Value>) -> Result<(), String> {
        // Ownership rides the spawner: the child belongs to whoever owns
        // this flow, wherever this VM runs.
        self.flow_ops.push(FlowOp::Flow {
            key,
            origin: self.ctx.origin.clone(),
            user: self.ctx.user.clone(),
            callee,
            args,
        });
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
            // platform answers None and this errors cleanly). Appends are
            // traced as tape lines — the event log is the whole point.
            NativeId::StoreWait => {
                // A story waits for the world: park like an effect hop.
                // The platform registers (flow, watch prefix, read seq);
                // Some(reply) = a commit slipped in — answer at once.
                // None = suspend toward the courier, wake on overlap.
                let path = args.first().cloned().unwrap_or(Value::Nil);
                let flow = self.ctx.flow.clone();
                let read_seq = self.ctx.read_seq;
                let origin = match &self.ctx.origin {
                    Value::Str(o) => o.to_string(),
                    _ => String::new(),
                };
                match self.platform.store_wait(path, &flow, read_seq, &origin) {
                    // "__parked" is the park signal: suspend locally toward
                    // the courier. Any other value ("changed") answers at
                    // once; an error fails the flow loudly.
                    Some(Ok(v)) if v.as_str() == Some("__parked") => {
                        Ok(NativeOut::Suspend {
                            target: Value::str("@wait"),
                            hop: "wait".to_string(),
                            vars: Value::map(
                                [(Value::str("flow"), Value::str(flow.as_str()))]
                                    .into_iter()
                                    .collect(),
                            ),
                        })
                    }
                    Some(r) => r.map(NativeOut::Val),
                    // No store on this side (a browser platform): a
                    // browser-side wait ships the path as data; for now it
                    // parks like a server wait — the hop carries it home.
                    None => Ok(NativeOut::Suspend {
                        target: Value::str("@wait"),
                        hop: "wait".to_string(),
                        vars: Value::map(
                            [(Value::str("flow"), Value::str(flow.as_str()))]
                                .into_iter()
                                .collect(),
                        ),
                    }),
                }
            }
            other => {
                if matches!(other, NativeId::StoreAppend) && self.platform.tracing() {
                    if let Some(ev) = args.first() {
                        let line = crate::value::to_json(ev)
                            .map(|j| j.to_string())
                            .unwrap_or_else(|_| "<unprintable event>".into());
                        self.platform.trace(format!("tape {line}"));
                    }
                }
                match self.platform.store_native(other, args, self.prog) {
                    Some(r) => {
                        // Every store read stamps the flow's commit point —
                        // the invisible read_seq riding alongside origin.
                        // The platform reports it after serving the read.
                        if matches!(
                            other,
                            NativeId::StoreCall | NativeId::StoreItems | NativeId::StoreHas
                        ) {
                            let seq = self.platform.last_read_seq();
                            if seq > self.ctx.read_seq {
                                self.ctx.read_seq = seq;
                            }
                        }
                        // A commit through this same platform wakes parked
                        // flows whose watches overlap: collect them; the
                        // Vm resumes once the platform borrow ends.
                        if matches!(other, NativeId::StoreAppend) {
                            self.woken.extend(self.platform.drain_woken());
                        }
                        r.map(NativeOut::Val)
                    }
                    None => Err(format!("{other:?} is not available on this side")),
                }
            }
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
