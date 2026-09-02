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
//! Identity rides the connection: whatever a client claims, `origin`,
//! `user`, and `reply_to` are overwritten at ingress with what hopd knows
//! about the socket the packet arrived on.
//!
//! Two identities:
//! - session — one tab, one WebSocket, minted per connection ("A", "B", …).
//! - user — durable across reloads and tabs: a `hop_user` cookie minted by
//!   the HTTP shell and read back from the WebSocket handshake (cookies are
//!   host-scoped, not port-scoped, so the ws port sees it). A client
//!   without the cookie gets a fresh user per connection.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tungstenite::Message;

use crate::auth;
use crate::ir::Program;
use crate::rt::{Platform, SideId, Vm, EFFECTS_ADDR};
use crate::store::{self, StoreBinding};
use crate::value::{decode, encode, NativeId, Value};

fn request_path(url: &str) -> &str {
    url.split('?').next().unwrap_or(url)
}

fn request_cookie(req: &tiny_http::Request) -> Option<String> {
    req.headers()
        .iter()
        .find(|h| h.field.equiv("Cookie"))
        .map(|h| h.value.as_str().to_string())
}

fn request_secure(req: &tiny_http::Request) -> bool {
    req.headers().iter().any(|h| {
        h.field.equiv("X-Forwarded-Proto") && h.value.as_str().eq_ignore_ascii_case("https")
    })
}

fn ws_path_from_env() -> String {
    std::env::var("HOP_WS_PATH")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default()
}

const INDEX_HTML: &str = include_str!("../web/index.html");
const GLUE_JS: &str = include_str!("../web/glue.js");
const IDIOMORPH_JS: &str = include_str!("../web/idiomorph.esm.js");

enum Ev {
    /// (session id, user id, outbound frame queue)
    Conn(String, String, mpsc::Sender<Vec<u8>>),
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
    /// Stream finished: accumulated text + assembled tool calls, or the error.
    LlmDone(String, Result<(String, Vec<ToolCall>), String>),
}

/// A structured tool call from the model, assembled from stream deltas
/// (arguments arrive in fragments) or read whole from a one-shot reply.
#[derive(Default, Clone)]
struct ToolCall {
    id: String,
    name: String,
    args: String,
}

fn tool_calls_value(calls: &[ToolCall]) -> Value {
    Value::array(
        calls
            .iter()
            .map(|c| {
                result_map(vec![
                    ("id", Value::str(c.id.as_str())),
                    ("name", Value::str(c.name.as_str())),
                    ("args", Value::str(c.args.as_str())),
                ])
            })
            .collect(),
    )
}

fn session_name(n: usize) -> String {
    if n < 26 {
        ((b'A' + n as u8) as char).to_string()
    } else {
        format!("S{n}")
    }
}

/// One connected tab.
struct Sess {
    out: mpsc::Sender<Vec<u8>>,
    user: String,
}

/// Mint a user id: an unguessable-enough bearer token for a dev server
/// (hashed clock + pid + counter — swap for a real RNG behind real auth).
fn mint_user() -> String {
    use std::hash::{Hash, Hasher};
    static N: AtomicU64 = AtomicU64::new(0);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut h);
    std::process::id().hash(&mut h);
    N.fetch_add(1, Ordering::Relaxed).hash(&mut h);
    let a = h.finish();
    a.wrapping_mul(0x9e37_79b9_7f4a_7c15).hash(&mut h);
    let b = h.finish();
    format!("u{a:016x}{b:016x}")
}

/// The `hop_user` value out of a Cookie header, if present.
fn hop_user_cookie(cookie_header: &str) -> Option<String> {
    cookie_header.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == "hop_user" && !v.is_empty()).then(|| v.to_string())
    })
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
/// Static files return immediately. A held `/boot.css` used to deadlock
/// boot: HTTP/1.1 will not read the next request on a connection until
/// the current one is answered, and glue waited on those requests
/// before opening the socket the barrier was waiting for.
fn http_thread(
    host: &'static str,
    port: u16,
    ws_port: u16,
    ws_path: String,
    app_src: String,
    pkg_dir: PathBuf,
    tx: mpsc::Sender<Ev>,
    _boot: Arc<AtomicU64>,
) {
    let addr = match host {
        "v6" => SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port),
        _ => SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
    };
    let server = match tiny_http::Server::http(addr) {
        Ok(s) => s,
        Err(e) => {
            // v6 unspecified often dual-stacks on macOS and owns the v4
            // port too; the other family then fails with EADDRINUSE.
            eprintln!("[hopd] http {addr} not bound ({e})");
            return;
        }
    };
    let config = format!(r#"{{"wsPort":{ws_port},"wsPath":"{ws_path}"}}"#);
    for req in server.incoming_requests() {
        let (tx, config, app_src, pkg_dir, ws_path) = (
            tx.clone(),
            config.clone(),
            app_src.clone(),
            pkg_dir.clone(),
            ws_path.clone(),
        );
        thread::spawn(move || {
            let mut req = req;
            let path = request_path(req.url()).to_string();
            let cookie = request_cookie(&req);
            if auth::required() && !auth::is_authed(cookie.as_deref()) {
                if req.method() == &tiny_http::Method::Post && path == "/login" {
                    let secure = request_secure(&req);
                    let mut body = String::new();
                    let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
                    if auth::verify_password(&auth::parse_form_password(&body).unwrap_or_default())
                    {
                        let mut resp = tiny_http::Response::empty(303).with_header(
                            tiny_http::Header::from_bytes(&b"Location"[..], b"/").unwrap(),
                        );
                        if let Some(c) = auth::set_cookie_header(secure) {
                            resp.add_header(
                                tiny_http::Header::from_bytes(&b"Set-Cookie"[..], c.as_bytes())
                                    .unwrap(),
                            );
                        }
                        let _ = req.respond(resp);
                        return;
                    }
                    let page = auth::login_html(Some("wrong password"));
                    let _ = req.respond(
                        tiny_http::Response::from_data(page.into_bytes()).with_header(
                            tiny_http::Header::from_bytes(
                                &b"Content-Type"[..],
                                b"text/html; charset=utf-8",
                            )
                            .unwrap(),
                        ),
                    );
                    return;
                }
                let page = auth::login_html(None);
                let _ = req.respond(
                    tiny_http::Response::from_data(page.into_bytes()).with_header(
                        tiny_http::Header::from_bytes(
                            &b"Content-Type"[..],
                            b"text/html; charset=utf-8",
                        )
                        .unwrap(),
                    ),
                );
                return;
            }

            // the shell page mints the durable user identity: a cookie the
            // ws handshake reads back (host-scoped, so the ws port sees it)
            let has_user = cookie
                .as_deref()
                .and_then(hop_user_cookie)
                .is_some();
            let set_cookie = match path.as_str() {
                "/" | "/index.html" if !has_user => Some(mint_user()),
                _ => None,
            };
            let (content, ctype): (Vec<u8>, &str) = match path.as_str() {
                "/" | "/index.html" => (
                    INDEX_HTML
                        .replace("__HOP_WS_PORT__", &ws_port.to_string())
                        .replace("__HOP_WS_PATH__", &ws_path)
                        .into(),
                    "text/html; charset=utf-8",
                ),
                "/glue.js" => (GLUE_JS.into(), "text/javascript"),
                "/idiomorph.esm.js" => (IDIOMORPH_JS.into(), "text/javascript"),
                "/config.json" => (config.into(), "application/json"),
                "/app.hop" => (app_src.into(), "text/plain; charset=utf-8"),
                "/reset" => {
                    let _ = tx.send(Ev::Reset);
                    (b"ok".to_vec(), "text/plain; charset=utf-8")
                }
                "/boot.css" => (b"/* ok */".to_vec(), "text/css"),
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
            let mut resp = tiny_http::Response::from_data(content).with_header(
                tiny_http::Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()).unwrap(),
            );
            if let Some(u) = set_cookie {
                resp.add_header(
                    tiny_http::Header::from_bytes(
                        &b"Set-Cookie"[..],
                        format!("hop_user={u}; Path=/; Max-Age=31536000; SameSite=Lax").as_bytes(),
                    )
                    .unwrap(),
                );
            }
            // /boot.css holds its socket until the first paint. Close that
            // connection so HTTP/1.1 keepalive cannot queue glue / wasm /
            // idiomorph behind the barrier (a deadlock: barrier waits for
            // ws, ws used to wait for those assets).
            let _ = req.respond(resp);
        });
    }
}

fn accept_thread(port: u16, tx: mpsc::Sender<Ev>) {
    let n = Arc::new(AtomicUsize::new(0));
    let v4 = TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port))
        .expect("bind ws v4");
    {
        let tx = tx.clone();
        let n = n.clone();
        thread::spawn(move || accept_loop(v4, tx, n));
    }
    match TcpListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)) {
        Ok(v6) => {
            thread::spawn(move || accept_loop(v6, tx, n));
        }
        Err(e) => {
            eprintln!("[hopd] ws [::]:{port} not bound ({e}) — localhost over IPv6 will hang");
        }
    }
}

fn accept_loop(listener: TcpListener, tx: mpsc::Sender<Ev>, n: Arc<AtomicUsize>) {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let i = n.fetch_add(1, Ordering::Relaxed);
        let sid = session_name(i);
        let tx = tx.clone();
        thread::spawn(move || conn_thread(stream, sid, tx));
    }
}

fn conn_thread(stream: TcpStream, sid: String, tx: mpsc::Sender<Ev>) {
    // the handshake request carries the browser's cookies; that is where
    // the durable user identity enters the runtime
    let mut user = String::new();
    let mut ws = match tungstenite::accept_hdr(stream, |req: &tungstenite::handshake::server::Request, resp| {
        let cookie = req
            .headers()
            .get("Cookie")
            .and_then(|v| v.to_str().ok());
        if let Some(u) = cookie.and_then(hop_user_cookie) {
            user = u;
        }
        if !auth::is_authed(cookie) {
            let deny = tungstenite::http::Response::builder()
                .status(401)
                .body(Some("unauthorized".into()))
                .expect("401");
            return Err(deny);
        }
        Ok(resp)
    }) {
        Ok(ws) => ws,
        Err(_) => return,
    };
    if user.is_empty() {
        // cookieless client (curl, tests): identity lives one connection
        user = mint_user();
    }
    // after the handshake, timeout reads so this thread can also write
    ws.get_mut()
        .set_read_timeout(Some(Duration::from_millis(50)))
        .ok();

    let (out_tx, out_rx) = mpsc::channel::<Vec<u8>>();
    if tx.send(Ev::Conn(sid.clone(), user, out_tx)).is_err() {
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

fn route(sessions: &HashMap<String, Sess>, pkt: &Value) {
    let Ok(bytes) = encode(pkt) else {
        eprintln!("[hopd] unencodable packet: {pkt}");
        return;
    };
    let to = pkt.get_field("to");
    match to.as_str() {
        Some("browsers") => {
            for s in sessions.values() {
                let _ = s.out.send(bytes.clone());
            }
        }
        Some(addr) => {
            if let Some(uid) = addr.strip_prefix("user:") {
                // every tab of one user
                for s in sessions.values().filter(|s| s.user == uid) {
                    let _ = s.out.send(bytes.clone());
                }
            } else if let Some(s) = sessions.get(addr) {
                let _ = s.out.send(bytes);
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
    done: Option<Result<(String, Vec<ToolCall>), String>>,
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
/// delta. Returns the accumulated text and any tool calls the model made
/// (their arguments arrive as string fragments, keyed by index).
fn llm_read_stream(
    reader: Box<dyn std::io::Read + Send + Sync>,
    mut on_delta: impl FnMut(&str),
) -> Result<(String, Vec<ToolCall>), String> {
    use std::io::BufRead;
    let mut acc = String::new();
    let mut calls: Vec<ToolCall> = Vec::new();
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
        let delta = &j["choices"][0]["delta"];
        if let Some(text) = delta["content"].as_str() {
            if !text.is_empty() {
                acc.push_str(text);
                on_delta(text);
            }
        }
        if let Some(tcs) = delta["tool_calls"].as_array() {
            for tc in tcs {
                let idx = tc["index"].as_u64().unwrap_or(0) as usize;
                while calls.len() <= idx {
                    calls.push(ToolCall::default());
                }
                let c = &mut calls[idx];
                if let Some(id) = tc["id"].as_str() {
                    c.id = id.to_string();
                }
                if let Some(n) = tc["function"]["name"].as_str() {
                    c.name = n.to_string();
                }
                if let Some(a) = tc["function"]["arguments"].as_str() {
                    c.args.push_str(a);
                }
            }
        }
    }
    Ok((acc, calls))
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
                let dir = vars.get_field("dir");
                let dir = dir.as_str().unwrap_or("").to_string();
                let tx = tx.clone();
                thread::spawn(move || {
                    let mut c = std::process::Command::new("bash");
                    c.arg("-c").arg(&cmd);
                    if !dir.is_empty() {
                        c.current_dir(&dir);
                    }
                    let out = c.output();
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
                    let run = || -> Result<(String, Vec<ToolCall>), String> {
                        let mut reader = llm_post(&cfg, &body?)?;
                        let mut s = String::new();
                        std::io::Read::read_to_string(&mut reader, &mut s)
                            .map_err(|e| e.to_string())?;
                        let j: serde_json::Value =
                            serde_json::from_str(&s).map_err(|e| e.to_string())?;
                        let msg = &j["choices"][0]["message"];
                        let text = msg["content"].as_str().unwrap_or_default().to_string();
                        let calls: Vec<ToolCall> = msg["tool_calls"]
                            .as_array()
                            .map(|tcs| {
                                tcs.iter()
                                    .map(|tc| ToolCall {
                                        id: tc["id"].as_str().unwrap_or_default().into(),
                                        name: tc["function"]["name"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .into(),
                                        args: tc["function"]["arguments"]
                                            .as_str()
                                            .unwrap_or_default()
                                            .into(),
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        if text.is_empty() && calls.is_empty() {
                            return Err(format!("no content in response: {s}"));
                        }
                        Ok((text, calls))
                    };
                    let value = match run() {
                        Ok((text, calls)) => {
                            let mut fields = vec![
                                ("ok", Value::Bool(true)),
                                ("text", Value::str(text)),
                            ];
                            if !calls.is_empty() {
                                fields.push(("tool_calls", tool_calls_value(&calls)));
                            }
                            result_map(fields)
                        }
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
            "llm_models" => {
                // GET {base}/models — OpenRouter and OpenAI-compatible
                // servers both answer { data: [{ id, ... }] }
                let cfg = cfg.clone();
                let tx = tx.clone();
                thread::spawn(move || {
                    let run = || -> Result<Vec<String>, String> {
                        let url = format!("{}/models", cfg.base.trim_end_matches('/'));
                        let mut req = ureq::get(&url);
                        if let Some(key) = &cfg.key {
                            req = req.set("Authorization", &format!("Bearer {key}"));
                        }
                        let s = req
                            .call()
                            .map_err(|e| e.to_string())?
                            .into_string()
                            .map_err(|e| e.to_string())?;
                        let j: serde_json::Value =
                            serde_json::from_str(&s).map_err(|e| e.to_string())?;
                        let list = j["data"].as_array().ok_or("no data[] in response")?;
                        Ok(list
                            .iter()
                            .filter_map(|m| m["id"].as_str().map(str::to_string))
                            .collect())
                    };
                    let value = match run() {
                        Ok(ids) => result_map(vec![
                            ("ok", Value::Bool(true)),
                            ("models", Value::array(ids.into_iter().map(Value::str).collect())),
                        ]),
                        Err(e) => result_map(vec![
                            ("ok", Value::Bool(false)),
                            ("error", Value::str(e)),
                        ]),
                    };
                    let _ = tx.send(Ev::Effect(effect_reply(&flow, value)));
                });
            }
            "llm_next" => {
                let h = crate::value::coerce_str(&vars.get_field("h"));
                let Some(stream) = self.streams.get_mut(&h) else {
                    let _ = tx.send(Ev::Effect(effect_error(&flow, &format!("unknown stream {h}"))));
                    return;
                };
                // final = the value about to go out is the done/error
                // marker — decided *before* next_value pops a delta, or a
                // last-buffered-delta-with-done-set removes the stream one
                // reply too early and the flow's next llm.next explodes.
                let is_final = stream.buf.is_empty() && stream.done.is_some();
                match Self::next_value(stream) {
                    Some(v) => {
                        let _ = tx.send(Ev::Effect(effect_reply(&flow, v)));
                        if is_final {
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
    /// `{delta}`, else the `{done, text, tool_calls?}` / `{error}` end marker.
    fn next_value(stream: &mut LlmStream) -> Option<Value> {
        if let Some(delta) = stream.buf.pop_front() {
            return Some(result_map(vec![("delta", Value::str(delta))]));
        }
        match &stream.done {
            Some(Ok((text, calls))) => {
                let mut fields = vec![
                    ("done", Value::Bool(true)),
                    ("text", Value::str(text.as_str())),
                ];
                if !calls.is_empty() {
                    fields.push(("tool_calls", tool_calls_value(calls)));
                }
                Some(result_map(fields))
            }
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

    fn on_done(
        &mut self,
        h: &str,
        res: Result<(String, Vec<ToolCall>), String>,
        tx: &mpsc::Sender<Ev>,
    ) {
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

#[cfg(test)]
mod effects_tests {
    use super::*;

    fn llm_next_pkt(flow: &str, h: &str) -> Value {
        Value::map(
            [
                (Value::str("kind"), Value::str("call")),
                (Value::str("flow"), Value::str(flow)),
                (Value::str("to"), Value::str(EFFECTS_ADDR)),
                (Value::str("hop"), Value::str("llm_next")),
                (
                    Value::str("vars"),
                    Value::map([(Value::str("h"), Value::str(h))].into_iter().collect()),
                ),
            ]
            .into_iter()
            .collect(),
        )
    }

    fn recv_reply(rx: &mpsc::Receiver<Ev>) -> Value {
        match rx.try_recv().expect("a reply should be queued") {
            Ev::Effect(bytes) => decode(&bytes).unwrap().get_field("value"),
            _ => panic!("expected Ev::Effect"),
        }
    }

    /// The race from the field: done arrives while the flow is mid-paint,
    /// so the last delta is popped with done already set. The stream must
    /// survive until the final marker itself has been delivered.
    #[test]
    fn last_delta_with_done_set_does_not_kill_the_stream() {
        let (tx, rx) = mpsc::channel::<Ev>();
        let mut fx = Effects::default();
        fx.streams.insert(
            "llm:1".into(),
            LlmStream {
                buf: VecDeque::from(["tail".to_string()]),
                done: Some(Ok(("full text".into(), Vec::new()))),
                pending: None,
            },
        );

        fx.handle(&llm_next_pkt("f#1", "llm:1"), &tx, &LlmCfg {
            key: None,
            base: String::new(),
            model: None,
        });
        let v = recv_reply(&rx);
        assert_eq!(v.get_field("delta"), Value::str("tail"));
        assert!(fx.streams.contains_key("llm:1"), "stream survives the last delta");

        fx.handle(&llm_next_pkt("f#1", "llm:1"), &tx, &LlmCfg {
            key: None,
            base: String::new(),
            model: None,
        });
        let v = recv_reply(&rx);
        assert_eq!(v.get_field("done"), Value::Bool(true));
        assert_eq!(v.get_field("text"), Value::str("full text"));
        assert!(!fx.streams.contains_key("llm:1"), "stream removed after the final marker");
    }

    /// Streamed tool_calls arrive as fragments keyed by index; the reader
    /// must reassemble ids, names, and argument strings.
    #[test]
    fn sse_reader_assembles_tool_call_fragments() {
        let sse = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"let me look\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_a\",\"type\":\"function\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"cm\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"d\\\":\\\"ls\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_b\",\"function\":{\"name\":\"bash\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: [DONE]\n\n",
        );
        let reader: Box<dyn std::io::Read + Send + Sync> =
            Box::new(std::io::Cursor::new(sse.as_bytes().to_vec()));
        let mut deltas = Vec::new();
        let (text, calls) = llm_read_stream(reader, |d| deltas.push(d.to_string())).unwrap();
        assert_eq!(text, "let me look");
        assert_eq!(deltas, vec!["let me look"]);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].args, r#"{"cmd":"ls"}"#);
        assert_eq!(calls[1].id, "call_b");
    }

    /// A chunk waking a parked llm.next must not finalize either.
    #[test]
    fn wake_on_chunk_keeps_the_stream_until_done_is_delivered() {
        let (tx, rx) = mpsc::channel::<Ev>();
        let mut fx = Effects::default();
        fx.streams.insert(
            "llm:1".into(),
            LlmStream { buf: VecDeque::new(), done: None, pending: Some("f#1".into()) },
        );

        fx.on_chunk("llm:1", "d1".into(), &tx);
        assert_eq!(recv_reply(&rx).get_field("delta"), Value::str("d1"));
        assert!(fx.streams.contains_key("llm:1"));

        fx.streams.get_mut("llm:1").unwrap().pending = Some("f#1".into());
        fx.on_done("llm:1", Ok(("d1".into(), Vec::new())), &tx);
        let v = recv_reply(&rx);
        assert_eq!(v.get_field("done"), Value::Bool(true));
        assert!(!fx.streams.contains_key("llm:1"), "final via wake removes the stream");
    }

    /// Model check: for every interleaving of producer events (chunks,
    /// then done) against a consumer that pipelines llm.next like
    /// run_turn does, the consumer must see every delta in order and then
    /// exactly one final marker — never "unknown stream".
    #[test]
    fn every_interleaving_delivers_all_deltas_then_one_final() {
        let cfg = LlmCfg { key: None, base: String::new(), model: None };
        for n_deltas in 0usize..4 {
            let n_producer = n_deltas + 1; // chunks + done
            // schedule bits: at each step, true = producer moves next
            for sched in 0u32..(1 << (2 * n_producer + n_deltas + 2)) {
                let (tx, rx) = mpsc::channel::<Ev>();
                let mut fx = Effects::default();
                fx.streams.insert(
                    "llm:1".into(),
                    LlmStream { buf: VecDeque::new(), done: None, pending: None },
                );

                let mut produced = 0; // producer events already delivered
                let mut want_next = true; // consumer owes an llm.next
                let mut in_flight = false; // an llm.next awaits its reply
                let mut got: Vec<String> = Vec::new();
                let mut finished = false;
                let mut bit = 0;

                while !finished {
                    let producer_turn = (sched >> bit) & 1 == 1;
                    bit += 1;
                    if producer_turn && produced < n_producer {
                        if produced < n_deltas {
                            fx.on_chunk("llm:1", format!("d{produced}"), &tx);
                        } else {
                            fx.on_done("llm:1", Ok(("all".into(), Vec::new())), &tx);
                        }
                        produced += 1;
                    } else if want_next && !in_flight {
                        fx.handle(&llm_next_pkt("f#1", "llm:1"), &tx, &cfg);
                        want_next = false;
                        in_flight = true;
                    } else if produced < n_producer {
                        // scheduler picked a side with nothing to do; let
                        // the producer move so every schedule terminates
                        if produced < n_deltas {
                            fx.on_chunk("llm:1", format!("d{produced}"), &tx);
                        } else {
                            fx.on_done("llm:1", Ok(("all".into(), Vec::new())), &tx);
                        }
                        produced += 1;
                    }
                    // consumer drains replies as the VM thread would
                    while let Ok(ev) = rx.try_recv() {
                        let Ev::Effect(bytes) = ev else { panic!("expected Ev::Effect") };
                        let pkt = decode(&bytes).unwrap();
                        assert_ne!(
                            pkt.get_field("kind"),
                            Value::str("error"),
                            "n={n_deltas} sched={sched:b}: flow got an error: {}",
                            pkt.get_field("err")
                        );
                        in_flight = false;
                        let v = pkt.get_field("value");
                        if let Value::Str(d) = v.get_field("delta") {
                            got.push(d.to_string());
                            want_next = true; // run_turn loops on deltas
                        } else {
                            assert_eq!(v.get_field("done"), Value::Bool(true));
                            finished = true;
                        }
                    }
                }

                let want: Vec<String> = (0..n_deltas).map(|i| format!("d{i}")).collect();
                assert_eq!(got, want, "n={n_deltas} sched={sched:b}: deltas in order");
                assert!(
                    !fx.streams.contains_key("llm:1"),
                    "n={n_deltas} sched={sched:b}: stream reaped after final"
                );
            }
        }
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
    let ws_path = ws_path_from_env();
    if auth::required() {
        println!("[hopd] password required (HOP_PASSWORD/PASSWORD)");
    }
    if !ws_path.is_empty() {
        println!("[hopd] browsers will connect to {ws_path} on the HTTP host");
    }
    {
        let tx = tx.clone();
        let boot = boot.clone();
        let app_src = app_src.clone();
        let pkg_dir = pkg_dir.clone();
        let ws_path = ws_path.clone();
        thread::spawn(move || {
            http_thread("v4", http_port, ws_port, ws_path, app_src, pkg_dir, tx, boot)
        });
    }
    {
        let tx = tx.clone();
        let boot = boot.clone();
        let ws_path = ws_path.clone();
        thread::spawn(move || {
            http_thread("v6", http_port, ws_port, ws_path, app_src, pkg_dir, tx, boot)
        });
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

    let mut sessions: HashMap<String, Sess> = HashMap::new();
    let mut effects = Effects::default();
    let llm_cfg = LlmCfg::from_env();
    println!("[hopd] serving http://localhost:{http_port}  (ws on :{ws_port}, CBOR binary)");

    for ev in rx {
        match ev {
            Ev::Conn(sid, user, out) => {
                let hello = Value::map(
                    [
                        (Value::str("kind"), Value::str("hello")),
                        (Value::str("session"), Value::str(sid.as_str())),
                        (Value::str("user"), Value::str(user.as_str())),
                    ]
                    .into_iter()
                    .collect(),
                );
                let _ = out.send(encode(&hello).expect("hello encodes"));
                sessions.insert(sid.clone(), Sess { out, user: user.clone() });
                println!("[hopd] session {sid} connected (user {user})");
                let mut platform = ServePlatform {
                    outbox: &mut outbox,
                    store: binding.as_mut(),
                };
                vm.session_connect(&mut platform, &sid, &user);
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
                    let user = sessions.get(&sid).map(|s| s.user.as_str()).unwrap_or("");
                    let _ = pkt.set_field("user", Value::str(user));
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
                if let Some(s) = sessions.remove(&sid) {
                    println!("[hopd] session {sid} disconnected");
                    let mut platform = ServePlatform {
                        outbox: &mut outbox,
                        store: binding.as_mut(),
                    };
                    vm.session_disconnect(&mut platform, &sid, &s.user);
                }
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
