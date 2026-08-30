//! The simulated cluster: one server VM plus n browser VMs in one process,
//! connected by a queue that carries only CBOR-encoded packets — every hop
//! round-trips through the codec, which is both the copy-at-hop and a
//! standing test of the encoding. Swapping the queue for a WebSocket
//! changes no semantics — that's the claim this harness exists to test.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use crate::compiler;
use crate::ir::Program;
use crate::rt::{Platform, SideId, Vm};
use crate::store::{self, StoreBinding};
use crate::value::{decode, encode, NativeId, Value};

struct Shared {
    queue: VecDeque<Vec<u8>>,
    log: Vec<String>,
    verbose: bool,
    /// per-browser simulated DOM: addr → selector → contents
    doms: HashMap<String, HashMap<String, String>>,
}

impl Shared {
    fn emit(&mut self, line: String) {
        if self.verbose {
            println!("{line}");
        }
        self.log.push(line);
    }
}

/// The per-VM [`Platform`]: labels transcript lines, logs the wire, backs
/// dom.* with a stored fake DOM, and (server only) carries the store.
struct SimPlatform<'a> {
    label: String,
    is_server: bool,
    shared: &'a mut Shared,
    store: Option<&'a mut StoreBinding>,
}

impl SimPlatform<'_> {
    fn prefix(&self) -> String {
        if self.is_server {
            "[server]".to_string()
        } else {
            format!("[browser {}]", self.label)
        }
    }
}

impl Platform for SimPlatform<'_> {
    fn send(&mut self, pkt: Value) {
        let to = pkt.get_field("to");
        let kind = pkt.get_field("kind");
        let detail = match kind.as_str().unwrap_or("?") {
            "reply" => format!("value={}", pkt.get_field("value")),
            "error" => format!("err={}", pkt.get_field("err")),
            _ => format!(
                "{} vars={}",
                crate::value::coerce_str(&pkt.get_field("hop")),
                pkt.get_field("vars")
            ),
        };
        let line = format!(
            "        ~ wire {:>7} -> {:<8} {:<5} {}",
            self.label,
            crate::value::coerce_str(&to),
            kind.as_str().unwrap_or("?"),
            detail
        );
        self.shared.emit(line);
        match encode(&pkt) {
            Ok(bytes) => self.shared.queue.push_back(bytes),
            Err(e) => self.shared.emit(format!("!! unencodable packet: {e}")),
        }
    }

    fn print(&mut self, line: String) {
        let p = self.prefix();
        self.shared.emit(format!("{p} {line}"));
    }

    fn dom_get(&mut self, sel: &str) -> String {
        self.shared
            .doms
            .get(&self.label)
            .and_then(|d| d.get(sel))
            .cloned()
            .unwrap_or_default()
    }

    fn dom_set(&mut self, sel: &str, html: &str) {
        let p = self.prefix();
        self.shared.emit(format!("{p} [dom] {sel} := {html}"));
        self.shared
            .doms
            .entry(self.label.clone())
            .or_default()
            .insert(sel.to_string(), html.to_string());
    }

    fn dom_clear(&mut self, sel: &str) {
        self.shared
            .doms
            .entry(self.label.clone())
            .or_default()
            .insert(sel.to_string(), String::new());
    }

    fn dom_focus(&mut self, sel: &str) {
        let p = self.prefix();
        self.shared.emit(format!("{p} [dom] focus {sel}"));
    }

    fn store_native(
        &mut self,
        id: NativeId,
        args: Vec<Value>,
        _prog: &Rc<Program>,
    ) -> Option<Result<Value, String>> {
        self.store.as_mut().map(|b| b.native(id, args))
    }
}

pub struct Cluster {
    server: Vm,
    store: Option<StoreBinding>,
    browsers: Vec<(String, Vm)>,
    shared: Shared,
    data_dir: PathBuf,
    _keep: Option<tempfile::TempDir>,
    /// Fake llm streams: handle → (chunks still to deliver, full text,
    /// structured tool calls for the final marker).
    fx_streams: HashMap<String, (VecDeque<String>, String, Vec<(String, String)>)>,
    fx_next: u64,
}

/// The harness's deterministic fake model: "RUN: <cmd>" yields a bash
/// tool call as JSON text (the fallback protocol), "CALL: <cmd>" yields a
/// structured tool call (the native protocol), anything else echoes.
/// Enough to exercise an agent's whole loop — stream, parse, approve,
/// tool, resume — offline. Returns (text, calls) where a call is
/// (id, json-arguments).
fn fake_llm(req: &Value) -> (String, Vec<(String, String)>) {
    let msgs = req.get_field("messages");
    let last = match &msgs {
        Value::Array(a) => a.borrow().last().cloned().unwrap_or(Value::Nil),
        _ => Value::Nil,
    };
    let content = crate::value::coerce_str(&last.get_field("content"));
    let quoted = |cmd: &str| cmd.trim().replace('\\', "\\\\").replace('"', "\\\"");
    if let Some(cmd) = content.strip_prefix("RUN:") {
        return (format!("{{\"tool\":\"bash\",\"cmd\":\"{}\"}}", quoted(cmd)), Vec::new());
    }
    if let Some(cmd) = content.strip_prefix("CALL:") {
        return (
            String::new(),
            vec![("call_fake_1".to_string(), format!("{{\"cmd\":\"{}\"}}", quoted(cmd)))],
        );
    }
    (format!("echo: {content}"), Vec::new())
}

fn fake_tool_calls_value(calls: &[(String, String)]) -> Value {
    Value::array(
        calls
            .iter()
            .map(|(id, args)| {
                Value::map(
                    [
                        (Value::str("id"), Value::str(id.as_str())),
                        (Value::str("name"), Value::str("bash")),
                        (Value::str("args"), Value::str(args.as_str())),
                    ]
                    .into_iter()
                    .collect(),
                )
            })
            .collect(),
    )
}

/// Split into two chunks so streaming loops see more than one delta.
fn fake_chunks(text: &str) -> VecDeque<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() < 2 {
        return VecDeque::from([text.to_string()]);
    }
    let mid = chars.len() / 2;
    VecDeque::from([
        chars[..mid].iter().collect::<String>(),
        chars[mid..].iter().collect::<String>(),
    ])
}

impl Cluster {
    /// Compile `.hop` source and build the cluster. Every VM loads the
    /// identical program. The server VM gets a fresh durable data dir.
    pub fn new(sessions: &[&str], hop_src: &str, verbose: bool) -> Result<Self, String> {
        let keep = tempfile::tempdir().map_err(|e| e.to_string())?;
        let data_dir = keep.path().to_path_buf();
        Self::with_data(sessions, hop_src, verbose, data_dir, Some(keep))
    }

    /// Build the cluster against an existing data directory (reopen / persist).
    pub fn with_data_dir(
        sessions: &[&str],
        hop_src: &str,
        verbose: bool,
        data_dir: impl AsRef<Path>,
    ) -> Result<Self, String> {
        Self::with_data(sessions, hop_src, verbose, data_dir.as_ref().to_path_buf(), None)
    }

    fn with_data(
        sessions: &[&str],
        hop_src: &str,
        verbose: bool,
        data_dir: PathBuf,
        keep: Option<tempfile::TempDir>,
    ) -> Result<Self, String> {
        let prog = Rc::new(compiler::compile(hop_src)?);
        let mut shared = Shared {
            queue: VecDeque::new(),
            log: Vec::new(),
            verbose,
            doms: HashMap::new(),
        };

        let server = {
            let mut platform = SimPlatform {
                label: "server".into(),
                is_server: true,
                shared: &mut shared,
                store: None,
            };
            Vm::new(prog.clone(), SideId::Server, &mut platform)?
        };
        let store = store::bind(&server, &data_dir)?;

        let mut browsers = Vec::new();
        for s in sessions {
            let mut platform = SimPlatform {
                label: s.to_string(),
                is_server: false,
                shared: &mut shared,
                store: None,
            };
            let vm = Vm::new(prog.clone(), SideId::Browser(s.to_string()), &mut platform)?;
            browsers.push((s.to_string(), vm));
        }

        Ok(Self {
            server,
            store,
            browsers,
            shared,
            data_dir,
            _keep: keep,
            fx_streams: HashMap::new(),
            fx_next: 0,
        })
    }

    /// Effects, synchronously: real bash (tests use `echo`), fake llm.
    /// Same reply shapes as hopd's off-thread executor.
    fn handle_effect(&mut self, pkt: &Value) -> Value {
        let flow = crate::value::coerce_str(&pkt.get_field("flow"));
        let vars = pkt.get_field("vars");
        let reply = |value: Value| {
            Value::map(
                [
                    (Value::str("kind"), Value::str("reply")),
                    (Value::str("flow"), Value::str(flow.as_str())),
                    (Value::str("to"), Value::str("server")),
                    (Value::str("value"), value),
                ]
                .into_iter()
                .collect(),
            )
        };
        let result = |entries: Vec<(&str, Value)>| {
            Value::map(entries.into_iter().map(|(k, v)| (Value::str(k), v)).collect())
        };
        match pkt.get_field("hop").as_str().unwrap_or("") {
            "bash" => {
                let cmd = crate::value::coerce_str(&vars.get_field("cmd"));
                let dir = vars.get_field("dir");
                let dir = dir.as_str().unwrap_or("");
                let mut c = std::process::Command::new("bash");
                c.arg("-c").arg(&cmd);
                if !dir.is_empty() {
                    c.current_dir(&dir);
                }
                let value = match c.output() {
                    Ok(o) => result(vec![
                        ("ok", Value::Bool(o.status.success())),
                        ("status", Value::Int(o.status.code().unwrap_or(-1) as i64)),
                        ("stdout", Value::str(String::from_utf8_lossy(&o.stdout).into_owned())),
                        ("stderr", Value::str(String::from_utf8_lossy(&o.stderr).into_owned())),
                    ]),
                    Err(e) => result(vec![
                        ("ok", Value::Bool(false)),
                        ("error", Value::str(e.to_string())),
                    ]),
                };
                reply(value)
            }
            "llm" => {
                let (text, calls) = fake_llm(&vars.get_field("req"));
                let mut fields = vec![("ok", Value::Bool(true)), ("text", Value::str(text))];
                if !calls.is_empty() {
                    fields.push(("tool_calls", fake_tool_calls_value(&calls)));
                }
                reply(result(fields))
            }
            "llm_models" => reply(result(vec![
                ("ok", Value::Bool(true)),
                (
                    "models",
                    Value::array(vec![Value::str("fake/alpha"), Value::str("fake/beta")]),
                ),
            ])),
            "llm_start" => {
                self.fx_next += 1;
                let h = format!("llm:{}", self.fx_next);
                let (text, calls) = fake_llm(&vars.get_field("req"));
                let chunks = if text.is_empty() { VecDeque::new() } else { fake_chunks(&text) };
                self.fx_streams.insert(h.clone(), (chunks, text, calls));
                reply(Value::str(h))
            }
            "llm_next" => {
                let h = crate::value::coerce_str(&vars.get_field("h"));
                let Some((chunks, full, calls)) = self.fx_streams.get_mut(&h) else {
                    return Value::map(
                        [
                            (Value::str("kind"), Value::str("error")),
                            (Value::str("flow"), Value::str(flow.as_str())),
                            (Value::str("to"), Value::str("server")),
                            (Value::str("err"), Value::str(format!("unknown stream {h}"))),
                        ]
                        .into_iter()
                        .collect(),
                    );
                };
                match chunks.pop_front() {
                    Some(delta) => reply(result(vec![("delta", Value::str(delta))])),
                    None => {
                        let full = full.clone();
                        let calls = calls.clone();
                        self.fx_streams.remove(&h);
                        let mut fields = vec![
                            ("done", Value::Bool(true)),
                            ("text", Value::str(full)),
                        ];
                        if !calls.is_empty() {
                            fields.push(("tool_calls", fake_tool_calls_value(&calls)));
                        }
                        reply(result(fields))
                    }
                }
            }
            other => Value::map(
                [
                    (Value::str("kind"), Value::str("error")),
                    (Value::str("flow"), Value::str(flow.as_str())),
                    (Value::str("to"), Value::str("server")),
                    (Value::str("err"), Value::str(format!("unknown effect {other}"))),
                ]
                .into_iter()
                .collect(),
            ),
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    /// Simulate an event: run a global entry point as a flow on one VM.
    pub fn fire(&mut self, addr: &str, entry: &str) {
        self.fire_args(addr, entry, Vec::new());
    }

    pub fn fire_args(&mut self, addr: &str, entry: &str, args: Vec<Value>) {
        if addr == "server" {
            let mut platform = SimPlatform {
                label: "server".into(),
                is_server: true,
                shared: &mut self.shared,
                store: self.store.as_mut(),
            };
            self.server.fire(&mut platform, entry, args);
        } else {
            let (label, vm) = Self::browser(&mut self.browsers, addr);
            let mut platform = SimPlatform {
                label,
                is_server: false,
                shared: &mut self.shared,
                store: None,
            };
            vm.fire(&mut platform, entry, args);
        }
    }

    /// Click a rendered hui handler by id on a browser VM.
    pub fn fire_handler(&mut self, addr: &str, id: i64) {
        self.fire_handler_ev(addr, id, Value::Nil);
    }

    /// Fire a handler with an event value (e.g. a key map).
    pub fn fire_handler_ev(&mut self, addr: &str, id: i64, ev: Value) {
        let (label, vm) = Self::browser(&mut self.browsers, addr);
        let mut platform = SimPlatform {
            label,
            is_server: false,
            shared: &mut self.shared,
            store: None,
        };
        vm.fire_handler(&mut platform, id, ev);
    }

    /// Run a server-side function to completion and return its value —
    /// the replacement for the Lua era's eval_server. It may cast (the
    /// packets queue as usual) but not hop.
    pub fn call_server(&mut self, name: &str, args: Vec<Value>) -> Result<Value, String> {
        let mut platform = SimPlatform {
            label: "server".into(),
            is_server: true,
            shared: &mut self.shared,
            store: self.store.as_mut(),
        };
        self.server.call_sync(&mut platform, name, args)
    }

    fn browser<'b>(browsers: &'b mut [(String, Vm)], addr: &str) -> (String, &'b mut Vm) {
        let (label, vm) = browsers
            .iter_mut()
            .find(|(a, _)| a == addr)
            .unwrap_or_else(|| panic!("no VM at address {addr}"));
        (label.clone(), vm)
    }

    /// Drain the queue to quiescence. "browsers" fans out to every browser
    /// VM — enumerating connected sessions at delivery time, per the
    /// design's at-most-once contract.
    pub fn pump(&mut self) {
        loop {
            let Some(bytes) = self.shared.queue.pop_front() else { break };
            let pkt = decode(&bytes).expect("undecodable packet on the queue");
            let to = pkt.get_field("to");
            let to = crate::value::coerce_str(&to);
            match to.as_str() {
                "@effects" => {
                    let reply = self.handle_effect(&pkt);
                    let line = format!(
                        "        ~ wire @effects -> server   reply value={}",
                        reply.get_field("value")
                    );
                    self.shared.emit(line);
                    match encode(&reply) {
                        Ok(bytes) => self.shared.queue.push_back(bytes),
                        Err(e) => self.shared.emit(format!("!! unencodable effect reply: {e}")),
                    }
                }
                "server" => {
                    let mut platform = SimPlatform {
                        label: "server".into(),
                        is_server: true,
                        shared: &mut self.shared,
                        store: self.store.as_mut(),
                    };
                    self.server.receive(&mut platform, pkt);
                }
                "browsers" => {
                    let mut order: Vec<usize> = (0..self.browsers.len()).collect();
                    order.sort_by(|&a, &b| self.browsers[a].0.cmp(&self.browsers[b].0));
                    for i in order {
                        let (label, vm) = &mut self.browsers[i];
                        let mut platform = SimPlatform {
                            label: label.clone(),
                            is_server: false,
                            shared: &mut self.shared,
                            store: None,
                        };
                        vm.receive(&mut platform, pkt.clone());
                    }
                }
                addr => {
                    let (label, vm) = Self::browser(&mut self.browsers, addr);
                    let mut platform = SimPlatform {
                        label,
                        is_server: false,
                        shared: &mut self.shared,
                        store: None,
                    };
                    vm.receive(&mut platform, pkt);
                }
            }
        }
    }

    /// Every VM must have zero suspended flows once the queue is drained; a
    /// leaked flow means a reply was lost or misrouted.
    pub fn assert_quiescent(&self) {
        if let Err(e) = self.server.quiescent() {
            panic!("server VM leaked: {e}");
        }
        for (addr, vm) in &self.browsers {
            if let Err(e) = vm.quiescent() {
                panic!("VM {addr} leaked: {e}");
            }
        }
    }

    /// The merged transcript: wire log, prints, and dom writes, in order.
    pub fn log(&self) -> Vec<String> {
        self.shared.log.clone()
    }

    /// The last rendered contents of a selector on a browser VM.
    pub fn dom(&self, addr: &str, sel: &str) -> String {
        self.shared
            .doms
            .get(addr)
            .and_then(|d| d.get(sel))
            .cloned()
            .unwrap_or_default()
    }

    /// Set a fake input value (what dom.get will return on that browser).
    pub fn set_dom(&mut self, addr: &str, sel: &str, value: &str) {
        self.shared
            .doms
            .entry(addr.to_string())
            .or_default()
            .insert(sel.to_string(), value.to_string());
    }

    // -- store passthroughs (server-side reads for tests) -------------------

    fn binding(&mut self) -> &mut StoreBinding {
        self.store.as_mut().expect("app has no store bound")
    }

    /// Append an event directly (a server-only append, as if from a
    /// server-origin flow).
    pub fn append(&mut self, event: Value) -> Result<Value, String> {
        self.binding().native(NativeId::StoreAppend, vec![event])
    }

    pub fn store_get(&mut self, path: Value) -> Result<Value, String> {
        self.binding().native(NativeId::StoreCall, vec![path])
    }

    pub fn verify(&mut self) -> Result<(), String> {
        self.binding().verify()
    }

    pub fn rebuild(&mut self) -> Result<(), String> {
        self.binding().rebuild()
    }

    pub fn applied(&mut self) -> Result<u64, String> {
        self.binding().applied()
    }

    pub fn banner(&self, text: &str) {
        if self.shared.verbose {
            println!("{text}");
        }
    }
}
