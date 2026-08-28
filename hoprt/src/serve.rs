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
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use tungstenite::Message;

use crate::ir::Program;
use crate::rt::{Platform, SideId, Vm};
use crate::store::{self, StoreBinding};
use crate::value::{decode, encode, NativeId, Value};

const INDEX_HTML: &str = "<!doctype html><meta charset=utf-8><title>hop</title>\
<body style=\"font-family:system-ui;max-width:40em;margin:4em auto\">\
<h1>hopd</h1><p>The server VM is running and speaking CBOR over the\
 WebSocket port. The browser backend (hop-web, the wasm build of the Hop\
 interpreter) is the next phase of the native rewrite; until it lands,\
 clients are programs that speak the packet protocol.</p>";

enum Ev {
    Conn(String, mpsc::Sender<Vec<u8>>),
    /// Raw frame bytes — Values are Rc-based and thread-local, so decoding
    /// happens on the VM thread.
    Pkt(String, Vec<u8>),
    Gone(String),
}

fn session_name(n: usize) -> String {
    if n < 26 {
        ((b'A' + n as u8) as char).to_string()
    } else {
        format!("S{n}")
    }
}

fn http_thread(port: u16, ws_port: u16) {
    let server = tiny_http::Server::http(("0.0.0.0", port)).expect("bind http");
    let config = format!(r#"{{"wsPort":{ws_port}}}"#);
    for req in server.incoming_requests() {
        let (content, ctype) = match req.url() {
            "/" | "/index.html" => (INDEX_HTML.to_string(), "text/html; charset=utf-8"),
            "/config.json" => (config.clone(), "application/json"),
            _ => {
                let _ = req.respond(tiny_http::Response::empty(404));
                continue;
            }
        };
        let resp = tiny_http::Response::from_string(content).with_header(
            tiny_http::Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()).unwrap(),
        );
        let _ = req.respond(resp);
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

    fn store_native(
        &mut self,
        id: NativeId,
        args: Vec<Value>,
        _prog: &Rc<Program>,
    ) -> Option<Result<Value, String>> {
        self.store.as_mut().map(|b| b.native(id, args))
    }
}

/// Run the hop server forever. `data_dir` holds the JSONL log and RocksDB
/// projection when the app declares `schema` + `reduce`. `log_packets`
/// dumps every packet in diagnostic notation.
pub fn serve(
    prog: Rc<Program>,
    http_port: u16,
    ws_port: u16,
    data_dir: impl AsRef<Path>,
    log_packets: bool,
) -> Result<(), String> {
    thread::spawn(move || http_thread(http_port, ws_port));
    let (tx, rx) = mpsc::channel::<Ev>();
    {
        let tx = tx.clone();
        thread::spawn(move || accept_thread(ws_port, tx));
    }

    // the server VM lives on this thread
    let mut outbox: VecDeque<Value> = VecDeque::new();
    let mut vm = {
        let mut platform = ServePlatform { outbox: &mut outbox, store: None };
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
                let mut platform = ServePlatform { outbox: &mut outbox, store: binding.as_mut() };
                vm.session_connect(&mut platform, &sid);
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
                let mut platform = ServePlatform { outbox: &mut outbox, store: binding.as_mut() };
                vm.receive(&mut platform, pkt);
            }
            Ev::Gone(sid) => {
                sessions.remove(&sid);
                println!("[hopd] session {sid} disconnected");
            }
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
            route(&sessions, &pkt);
        }
    }
    Ok(())
}
