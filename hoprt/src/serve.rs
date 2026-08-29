//! hopd's server: real sockets around the same runtime. The wire is CBOR
//! binary; readable packet dumps come from log mode (`--log`), rendered in
//! diagnostic notation by the value model.
//!
//! Threads:
//! - a tiny_http thread (a placeholder page until the hop-web wasm browser
//!   backend lands — fake-browser clients speak the ws protocol today),
//! - a WebSocket accept thread assigning session ids,
//! - one connection thread per client (50ms read timeout so one thread can
//!   both read the socket and drain its outbound queue),
//! - and the main thread owning the server VM and all routing.
//!
//! Identity rides the connection: whatever a client claims, `origin` and
//! `reply_to` are overwritten with the session id of the socket the packet
//! arrived on.

use std::collections::{HashMap, VecDeque};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tungstenite::Message;

use crate::ir::Program;
use crate::rt::{Platform, SideId, Vm, EFFECTS_ADDR};
use crate::store::{self, StoreBinding};
use crate::value::{decode, encode, NativeId, Value};

const INDEX_HTML: &str = include_str!("../web/index.html");
const GLUE_JS: &str = include_str!("../web/glue.js");

enum Ev {
    Conn(String, mpsc::Sender<Vec<u8>>),
    /// Raw frame bytes — Values are Rc-based and thread-local, so decoding
    /// happens on the VM thread.
    Pkt(String, Vec<u8>),
    Gone(String),
    /// GET /reset — the app's `on_reset` hook decides what reset means
    /// (typically: append a reset event to the log, re-render browsers).
    Reset,
    /// An effect reply (encoded packet) minted by the effects executor —
    /// delivered straight to the server VM, never identity-stamped.
    Effect(Vec<u8>),
    /// One streamed LLM delta for a stream handle.
    LlmChunk(String, String),
    /// Stream finished: full accumulated text, or the error.
    LlmDone(String, Result<String, String>),
}

fn session_name(n: usize) -> String {
    if n < 26 {
        ((b'A' + n as u8) as char).to_string()
    } else {
        format!("S{n}")
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// Static assets: the shell page and glue.js are compiled in; the app's
/// .hop source ships to the browser (both sides must compile the same
/// program — the wire carries hop ids, not code); the wasm interpreter is
/// served from `pkg_dir` (a `wasm-pack build --target web` output).
///
/// `/boot.css` is a boot barrier: the shell page references it as a
/// stylesheet, so the window load event (what navigation waiters key on)
/// holds until a ws session that began after this request has connected
/// and received its first cast — i.e. until the first render has landed.
/// Each request is handled on its own thread so the barrier never blocks
/// the boot assets themselves.
fn http_thread(
    port: u16,
    ws_port: u16,
    app_src: String,
    pkg_dir: PathBuf,
    tx: mpsc::Sender<Ev>,
    boot: Arc<AtomicU64>,
) {
    let server = tiny_http::Server::http(("0.0.0.0", port)).expect("bind http");
    let config = format!(r#"{{"wsPort":{ws_port}}}"#);
    for req in server.incoming_requests() {
        let (tx, boot, config, app_src, pkg_dir) =
            (tx.clone(), boot.clone(), config.clone(), app_src.clone(), pkg_dir.clone());
        thread::spawn(move || {
            let (content, ctype): (Vec<u8>, &str) = match req.url() {
                "/" | "/index.html" => (INDEX_HTML.into(), "text/html; charset=utf-8"),
                "/glue.js" => (GLUE_JS.into(), "text/javascript"),
                "/config.json" => (config.into(), "application/json"),
                "/app.hop" => (app_src.into(), "text/plain; charset=utf-8"),
                "/reset" => {
                    let _ = tx.send(Ev::Reset);
                    (b"ok".to_vec(), "text/plain; charset=utf-8")
                }
                "/boot.css" => {
                    let start = now_millis();
                    let deadline = start + 8000;
                    while now_millis() < deadline {
                        if boot.load(Ordering::SeqCst) > start {
                            break;
                        }
                        thread::sleep(Duration::from_millis(10));
                    }
                    // grace: the hello + first cast are in flight; let the
                    // browser's wasm VM render before load fires
                    thread::sleep(Duration::from_millis(75));
                    (b"/* boot barrier */".to_vec(), "text/css")
                }
                url => {
                    // /pkg/<file> — flat, no traversal: the last path component only
                    let served = url.strip_prefix("/pkg/").and_then(|f| {
                        let name = Path::new(f).file_name()?;
                        std::fs::read(pkg_dir.join(name)).ok().map(|bytes| {
                            let ctype = if f.ends_with(".wasm") {
                                "application/wasm"
                            } else if f.ends_with(".js") {
                                "text/javascript"
                            } else {
                                "application/octet-stream"
                            };
                            (bytes, ctype)
                        })
                    });
                    match served {
                        Some(x) => x,
                        None => {
                            let _ = req.respond(tiny_http::Response::empty(404));
                            return;
                        }
                    }
                }
            };
            let resp = tiny_http::Response::from_data(content).with_header(
                tiny_http::Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()).unwrap(),
            );
            let _ = req.respond(resp);
        });
    }
}

fn accept_thread(port: u16, tx: mpsc::Sender<Ev>) {
    let listener = TcpListener::bind(("0.0.0.0", port)).expect("bind ws");
    let mut n = 0usize;
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let sid = session_name(n);
        n += 1;
        let tx = tx.clone();
        thread::spawn(move || conn_thread(stream, sid, tx));
    }
}

fn conn_thread(stream: TcpStream, sid: String, tx: mpsc::Sender<Ev>) {
    let mut ws = match tungstenite::accept(stream) {
        Ok(ws) => ws,
        Err(_) => return,
    };
    // after the handshake, timeout reads so this thread can also write
    ws.get_mut()
        .set_read_timeout(Some(Duration::from_millis(50)))
        .ok();

    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    if tx.send(Ev::Conn(sid.clone(), out_tx)).is_err() {
        return;
    }
    loop {
        while let Ok(bytes) = out_rx.try_recv() {
            if ws.send(Message::Binary(bytes.into())).is_err() {
                let _ = tx.send(Ev::Gone(sid));
                return;
            }
        }
        match ws.read() {
            Ok(Message::Binary(b)) => {
                let _ = tx.send(Ev::Pkt(sid.clone(), b.to_vec()));
            }
            Ok(Message::Close(_)) => {
                let _ = tx.send(Ev::Gone(sid));
                return;
            }
            Ok(_) => {}
            Err(tungstenite::Error::Io(e))
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            Err(_) => {
                let _ = tx.send(Ev::Gone(sid));
                return;
            }
        }
    }
}

fn route(sessions: &HashMap<String, mpsc::Sender<Vec<u8>>>, pkt: &Value) {
    let Ok(bytes) = encode(pkt) else {
        eprintln!("[hopd] unencodable packet: {pkt}");
        return;
    };
    let to = pkt.get_field("to");
    match to.as_str() {
        Some("browsers") => {
            for out in sessions.values() {
                let _ = out.send(bytes.clone());
            }
        }
        Some(addr) => {
            if let Some(out) = sessions.get(addr) {
                let _ = out.send(bytes);
            }
            // a vanished session is a dropped packet: at-most-once, by design
        }
        None => eprintln!("[hopd] packet without target: {pkt}"),
    }
}

/// The server VM's platform: packets queue for routing; prints go to
/// stdout; there is no DOM on this side.
struct ServePlatform<'a> {
    outbox: &'a mut VecDeque<Value>,
    store: Option<&'a mut StoreBinding>,
}

impl Platform for ServePlatform<'_> {
    fn send(&mut self, pkt: Value) {
        self.outbox.push_back(pkt);
    }

    fn print(&mut self, line: String) {
        println!("{line}");
    }

    fn dom_get(&mut self, _sel: &str) -> String {
        String::new()
    }

    fn dom_set(&mut self, _sel: &str, _html: &str) {}

    fn dom_clear(&mut self, _sel: &str) {}

    fn dom_focus(&mut self, _sel: &str) {}

    fn store_native(
        &mut self,
        id: NativeId,
        args: Vec<Value>,
        _prog: &Rc<Program>,
    ) -> Option<Result<Value, String>> {
        self.store.as_mut().map(|b| b.native(id, args))
    }
}

// ---------------------------------------------------------------------------
// Effects: bash + llm run off-thread; the flow stays suspended, the VM
// stays live. A call packet addressed to "@effects" comes only from the
// server VM's own outbox — a socket packet is delivered to the VM (which
// ignores hop ids it didn't mint), so tabs cannot reach these.
// ---------------------------------------------------------------------------

/// Env-configured LLM endpoint (OpenAI-compatible chat completions).
#[derive(Clone)]
struct LlmCfg {
    key: Option<String>,
    base: String,
    model: Option<String>,
}

impl LlmCfg {
    fn from_env() -> LlmCfg {
        LlmCfg {
            key: std::env::var("OPENAI_API_KEY").ok(),
            base: std::env::var("OPENAI_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".into()),
            model: std::env::var("HOP_LLM_MODEL").ok(),
        }
    }
}

struct LlmStream {
    buf: VecDeque<String>,
    done: Option<Result<String, String>>,
    /// A parked llm.next waiting for the next chunk: its flow id.
    pending: Option<String>,
}

#[derive(Default)]
struct Effects {
    streams: HashMap<String, LlmStream>,
    next_id: u64,
}

/// `{ kind:"reply", flow, to:"server", value }` — resumes the parked flow.
fn effect_reply(flow: &str, value: Value) -> Vec<u8> {
    let pkt = Value::map(
        [
            (Value::str("kind"), Value::str("reply")),
            (Value::str("flow"), Value::str(flow)),
            (Value::str("to"), Value::str("server")),
            (Value::str("value"), value),
        ]
        .into_iter()
        .collect(),
    );
    encode(&pkt).expect("effect reply encodes")
}

fn effect_error(flow: &str, err: &str) -> Vec<u8> {
    let pkt = Value::map(
        [
            (Value::str("kind"), Value::str("error")),
            (Value::str("flow"), Value::str(flow)),
            (Value::str("to"), Value::str("server")),
            (Value::str("err"), Value::str(err)),
        ]
        .into_iter()
        .collect(),
    );
    encode(&pkt).expect("effect error encodes")
}

fn result_map(entries: Vec<(&str, Value)>) -> Value {
    Value::map(
        entries
            .into_iter()
            .filter(|(_, v)| !matches!(v, Value::Nil))
            .map(|(k, v)| (Value::str(k), v))
            .collect(),
    )
}

/// Build the chat-completions request body from the hop request map.
/// `stream` is forced; `model` defaults from the env when absent.
fn llm_body(req: &Value, cfg: &LlmCfg, stream: bool) -> Result<String, String> {
    let mut body = crate::value::to_json(req)?;
    let obj = body.as_object_mut().ok_or("llm request must be a map")?;
    if !obj.contains_key("model") {
        match &cfg.model {
            Some(m) => {
                obj.insert("model".into(), serde_json::Value::String(m.clone()));
            }
            None => return Err("no model: pass req.model or set HOP_LLM_MODEL".into()),
        }
    }
    obj.insert("stream".into(), serde_json::Value::Bool(stream));
    serde_json::to_string(&body).map_err(|e| e.to_string())
}

/// POST the request. Returns the response reader, or a printable error
/// (HTTP status errors include the body — that's where the API explains).
fn llm_post(cfg: &LlmCfg, body: &str) -> Result<Box<dyn std::io::Read + Send + Sync>, String> {
    let key = cfg.key.as_deref().ok_or("OPENAI_API_KEY is not set")?;
    let url = format!("{}/chat/completions", cfg.base.trim_end_matches('/'));
    let req = ureq::post(&url)
        .set("Authorization", &format!("Bearer {key}"))
        .set("Content-Type", "application/json");
    match req.send_string(body) {
        Ok(resp) => Ok(resp.into_reader()),
        Err(ureq::Error::Status(code, resp)) => {
            let detail = resp.into_string().unwrap_or_default();
            Err(format!("HTTP {code}: {}", detail.chars().take(400).collect::<String>()))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Read an SSE chat-completions stream, calling `on_delta` per content
/// delta. Returns the accumulated text.
fn llm_read_stream(
    reader: Box<dyn std::io::Read + Send + Sync>,
    mut on_delta: impl FnMut(&str),
) -> Result<String, String> {
    use std::io::BufRead;
    let mut acc = String::new();
    for line in std::io::BufReader::new(reader).lines() {
        let line = line.map_err(|e| e.to_string())?;
        let Some(data) = line.strip_prefix("data:") else { continue };
        let data = data.trim();
        if data == "[DONE]" {
            break;
        }
        let j: serde_json::Value = match serde_json::from_str(data) {
            Ok(j) => j,
            Err(_) => continue,
        };
        if let Some(err) = j.get("error") {
            return Err(err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("llm stream error")
                .to_string());
        }
        if let Some(delta) = j["choices"][0]["delta"]["content"].as_str() {
            if !delta.is_empty() {
                acc.push_str(delta);
                on_delta(delta);
            }
        }
    }
    Ok(acc)
}

impl Effects {
    /// Handle a call packet addressed to "@effects": spawn the work,
    /// reply through the Ev channel. Never blocks the VM thread.
    fn handle(&mut self, pkt: &Value, tx: &mpsc::Sender<Ev>, cfg: &LlmCfg) {
        let flow = crate::value::coerce_str(&pkt.get_field("flow"));
        let hop = pkt.get_field("hop");
        let vars = pkt.get_field("vars");
        match hop.as_str().unwrap_or("") {
            "bash" => {
                let cmd = crate::value::coerce_str(&vars.get_field("cmd"));
                let tx = tx.clone();
                thread::spawn(move || {
                    let out = std::process::Command::new("bash").arg("-c").arg(&cmd).output();
                    let value = match out {
                        Ok(o) => result_map(vec![
                            ("ok", Value::Bool(o.status.success())),
                            ("status", Value::Int(o.status.code().unwrap_or(-1) as i64)),
                            ("stdout", Value::str(String::from_utf8_lossy(&o.stdout).into_owned())),
                            ("stderr", Value::str(String::from_utf8_lossy(&o.stderr).into_owned())),
                        ]),
                        Err(e) => result_map(vec![
                            ("ok", Value::Bool(false)),
                            ("error", Value::str(e.to_string())),
                        ]),
                    };
                    let _ = tx.send(Ev::Effect(effect_reply(&flow, value)));
                });
            }
            "llm" => {
                // one-shot completion: the whole turn in one reply
                let body = llm_body(&vars.get_field("req"), cfg, false);
                let cfg = cfg.clone();
                let tx = tx.clone();
                thread::spawn(move || {
                    let run = || -> Result<String, String> {
                        let mut reader = llm_post(&cfg, &body?)?;
                        let mut s = String::new();
                        std::io::Read::read_to_string(&mut reader, &mut s)
                            .map_err(|e| e.to_string())?;
                        let j: serde_json::Value =
                            serde_json::from_str(&s).map_err(|e| e.to_string())?;
                        j["choices"][0]["message"]["content"]
                            .as_str()
                            .map(str::to_string)
                            .ok_or_else(|| format!("no content in response: {s}"))
                    };
                    let value = match run() {
                        Ok(text) => result_map(vec![
                            ("ok", Value::Bool(true)),
                            ("text", Value::str(text)),
                        ]),
                        Err(e) => result_map(vec![
                            ("ok", Value::Bool(false)),
                            ("error", Value::str(e)),
                        ]),
                    };
                    let _ = tx.send(Ev::Effect(effect_reply(&flow, value)));
                });
            }
            "llm_start" => {
                self.next_id += 1;
                let h = format!("llm:{}", self.next_id);
                self.streams.insert(
                    h.clone(),
                    LlmStream { buf: VecDeque::new(), done: None, pending: None },
                );
                let body = llm_body(&vars.get_field("req"), cfg, true);
                let cfg = cfg.clone();
                let tx_stream = tx.clone();
                let handle = h.clone();
                thread::spawn(move || {
                    let res = body.and_then(|b| llm_post(&cfg, &b)).and_then(|reader| {
                        llm_read_stream(reader, |delta| {
                            let _ = tx_stream
                                .send(Ev::LlmChunk(handle.clone(), delta.to_string()));
                        })
                    });
                    let _ = tx_stream.send(Ev::LlmDone(handle, res));
                });
                let _ = tx.send(Ev::Effect(effect_reply(&flow, Value::str(h))));
            }
            "llm_next" => {
                let h = crate::value::coerce_str(&vars.get_field("h"));
                let Some(stream) = self.streams.get_mut(&h) else {
                    let _ = tx.send(Ev::Effect(effect_error(&flow, &format!("unknown stream {h}"))));
                    return;
                };
                match Self::next_value(stream) {
                    Some(v) => {
                        let finished = stream.buf.is_empty() && stream.done.is_some();
                        let _ = tx.send(Ev::Effect(effect_reply(&flow, v)));
                        if finished {
                            self.streams.remove(&h);
                        }
                    }
                    None => stream.pending = Some(flow),
                }
            }
            other => {
                let _ = tx.send(Ev::Effect(effect_error(&flow, &format!("unknown effect {other}"))));
            }
        }
    }

    /// The next llm.next reply for a stream, if one is ready: a buffered
    /// `{delta}`, else the `{done, text}` / `{error}` end marker.
    fn next_value(stream: &mut LlmStream) -> Option<Value> {
        if let Some(delta) = stream.buf.pop_front() {
            return Some(result_map(vec![("delta", Value::str(delta))]));
        }
        match &stream.done {
            Some(Ok(text)) => Some(result_map(vec![
                ("done", Value::Bool(true)),
                ("text", Value::str(text.as_str())),
            ])),
            Some(Err(e)) => Some(result_map(vec![("error", Value::str(e.as_str()))])),
            None => None,
        }
    }

    /// A delta arrived from the stream thread; wake a parked llm.next.
    fn on_chunk(&mut self, h: &str, delta: String, tx: &mpsc::Sender<Ev>) {
        let Some(stream) = self.streams.get_mut(h) else { return };
        stream.buf.push_back(delta);
        if Self::wake(stream, tx) {
            self.streams.remove(h);
        }
    }

    fn on_done(&mut self, h: &str, res: Result<String, String>, tx: &mpsc::Sender<Ev>) {
        let Some(stream) = self.streams.get_mut(h) else { return };
        stream.done = Some(res);
        if Self::wake(stream, tx) {
            self.streams.remove(h);
        }
    }

    /// Deliver a reply to a parked llm.next if one is ready. True when
    /// the final marker went out — the stream record is spent.
    fn wake(stream: &mut LlmStream, tx: &mpsc::Sender<Ev>) -> bool {
        if stream.pending.is_none() {
            return false;
        }
        let is_final = stream.buf.is_empty() && stream.done.is_some();
        if let Some(v) = Self::next_value(stream) {
            let flow = stream.pending.take().unwrap();
            let _ = tx.send(Ev::Effect(effect_reply(&flow, v)));
            return is_final;
        }
        false
    }
}

/// Run the hop server forever. `app_src` is the .hop source, served to
/// browsers (they compile it with the same compiler, in wasm). `pkg_dir`
/// is the hop-web wasm-pack output. `data_dir` holds the JSONL log and
/// RocksDB projection when the app declares `schema` + `reduce`.
/// `log_packets` dumps every packet in diagnostic notation.
pub fn serve(
    prog: Rc<Program>,
    app_src: String,
    http_port: u16,
    ws_port: u16,
    data_dir: impl AsRef<Path>,
    pkg_dir: PathBuf,
    log_packets: bool,
) -> Result<(), String> {
    if !pkg_dir.join("hop_web.js").exists() {
        eprintln!(
            "[hopd] warning: {} has no hop_web.js — browsers won't boot.\n\
             [hopd]          build it with: wasm-pack build hop-web --target web",
            pkg_dir.display()
        );
    }
    let (tx, rx) = mpsc::channel::<Ev>();
    // millis of the latest session connect (hello + first cast sent) —
    // the /boot.css barrier waits on this
    let boot = Arc::new(AtomicU64::new(0));
    {
        let tx = tx.clone();
        let boot = boot.clone();
        thread::spawn(move || http_thread(http_port, ws_port, app_src, pkg_dir, tx, boot));
    }
    {
        let tx = tx.clone();
        thread::spawn(move || accept_thread(ws_port, tx));
    }

    // the server VM lives on this thread
    let mut outbox: VecDeque<Value> = VecDeque::new();
    let mut vm = {
        let mut platform = ServePlatform {
            outbox: &mut outbox,
            store: None,
        };
        Vm::new(prog, SideId::Server, &mut platform)?
    };
    let mut binding = store::bind(&vm, data_dir.as_ref())?;
    if binding.is_some() {
        println!(
            "[hopd] durable store at {}  (log.jsonl + proj/)",
            data_dir.as_ref().display()
        );
    }

    let mut sessions: HashMap<String, mpsc::Sender<Vec<u8>>> = HashMap::new();
    let mut effects = Effects::default();
    let llm_cfg = LlmCfg::from_env();
    println!("[hopd] serving http://localhost:{http_port}  (ws on :{ws_port}, CBOR binary)");

    for ev in rx {
        match ev {
            Ev::Conn(sid, out) => {
                let hello = Value::map(
                    [
                        (Value::str("kind"), Value::str("hello")),
                        (Value::str("session"), Value::str(sid.as_str())),
                    ]
                    .into_iter()
                    .collect(),
                );
                let _ = out.send(encode(&hello).expect("hello encodes"));
                sessions.insert(sid.clone(), out);
                println!("[hopd] session {sid} connected");
                let mut platform = ServePlatform {
                    outbox: &mut outbox,
                    store: binding.as_mut(),
                };
                vm.session_connect(&mut platform, &sid);
                boot.store(now_millis(), Ordering::SeqCst);
            }
            Ev::Pkt(sid, bytes) => {
                let pkt = match decode(&bytes) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[hopd] undecodable packet from {sid}: {e}");
                        continue;
                    }
                };
                let kind = pkt.get_field("kind");
                let kind = kind.as_str().unwrap_or("");
                if kind == "call" || kind == "cast" {
                    // never trust a claimed identity
                    let _ = pkt.set_field("origin", Value::str(sid.as_str()));
                    let _ = pkt.set_field("reply_to", Value::str(sid.as_str()));
                }
                if log_packets {
                    println!("        ~ wire {sid:>7} -> server   {pkt}");
                } else {
                    println!(
                        "        ~ wire {sid:>7} -> server   {kind:<5} {}",
                        crate::value::coerce_str(&pkt.get_field("hop"))
                    );
                }
                let mut platform = ServePlatform {
                    outbox: &mut outbox,
                    store: binding.as_mut(),
                };
                vm.receive(&mut platform, pkt);
            }
            Ev::Gone(sid) => {
                sessions.remove(&sid);
                println!("[hopd] session {sid} disconnected");
            }
            Ev::Reset => {
                println!("[hopd] reset");
                let mut platform = ServePlatform {
                    outbox: &mut outbox,
                    store: binding.as_mut(),
                };
                if !matches!(vm.globals.get("on_reset"), Value::Nil) {
                    vm.fire(&mut platform, "on_reset", Vec::new());
                }
            }
            Ev::Effect(bytes) => {
                let pkt = match decode(&bytes) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("[hopd] undecodable effect reply: {e}");
                        continue;
                    }
                };
                if log_packets {
                    println!("        ~ wire @effects -> server   {pkt}");
                }
                let mut platform = ServePlatform {
                    outbox: &mut outbox,
                    store: binding.as_mut(),
                };
                vm.receive(&mut platform, pkt);
            }
            Ev::LlmChunk(h, delta) => effects.on_chunk(&h, delta, &tx),
            Ev::LlmDone(h, res) => effects.on_done(&h, res, &tx),
        }
        // drain everything the VM emitted in response
        while let Some(pkt) = outbox.pop_front() {
            if log_packets {
                println!("        ~ wire  server -> {pkt}");
            } else {
                println!(
                    "        ~ wire  server -> {:<8} {:<5} {}",
                    crate::value::coerce_str(&pkt.get_field("to")),
                    crate::value::coerce_str(&pkt.get_field("kind")),
                    crate::value::coerce_str(&pkt.get_field("hop")),
                );
            }
            if pkt.get_field("to").as_str() == Some(EFFECTS_ADDR) {
                effects.handle(&pkt, &tx, &llm_cfg);
            } else {
                route(&sessions, &pkt);
            }
        }
    }
    Ok(())
}
