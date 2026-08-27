//! hopd's server: real sockets around the same runtime.
//!
//! Three threads plus one VM:
//! - a tiny_http thread serving the static pieces (index.html, glue.js,
//!   hoprt.lua, and the hopc-compiled app.lua),
//! - a WebSocket accept thread assigning session ids,
//! - one connection thread per tab (50ms read timeout so one thread can
//!   both read the socket and drain its outbound queue),
//! - and the main thread owning the server Luau VM and all routing.
//!
//! Identity rides the connection: whatever a client claims, `origin` and
//! `reply_to` are overwritten with the session id of the socket the packet
//! arrived on.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::net::{TcpListener, TcpStream};
use std::rc::Rc;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use mlua::{Function, Lua, LuaSerdeExt, Value as LuaValue};
use tungstenite::Message;

const INDEX_HTML: &str = include_str!("../web/index.html");
const GLUE_JS: &str = include_str!("../web/glue.js");
const HOPRT_LUA: &str = include_str!("../lua/hoprt.lua");

enum Ev {
    Conn(String, mpsc::Sender<String>),
    Pkt(String, serde_json::Value),
    Gone(String),
}

fn session_name(n: usize) -> String {
    if n < 26 {
        ((b'A' + n as u8) as char).to_string()
    } else {
        format!("S{n}")
    }
}

fn http_thread(port: u16, app_code: String) {
    let server = tiny_http::Server::http(("0.0.0.0", port)).expect("bind http");
    for req in server.incoming_requests() {
        let (content, ctype) = match req.url() {
            "/" | "/index.html" => (INDEX_HTML, "text/html; charset=utf-8"),
            "/glue.js" => (GLUE_JS, "application/javascript"),
            "/hoprt.lua" => (HOPRT_LUA, "text/plain; charset=utf-8"),
            "/app.lua" => (app_code.as_str(), "text/plain; charset=utf-8"),
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

    let (out_tx, out_rx) = mpsc::channel::<String>();
    if tx.send(Ev::Conn(sid.clone(), out_tx)).is_err() {
        return;
    }
    loop {
        while let Ok(msg) = out_rx.try_recv() {
            if ws.send(Message::Text(msg.into())).is_err() {
                let _ = tx.send(Ev::Gone(sid));
                return;
            }
        }
        match ws.read() {
            Ok(Message::Text(t)) => {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(t.as_str()) {
                    let _ = tx.send(Ev::Pkt(sid.clone(), json));
                }
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

fn route(sessions: &HashMap<String, mpsc::Sender<String>>, pkt: &serde_json::Value) {
    let text = pkt.to_string();
    match pkt["to"].as_str() {
        Some("browsers") => {
            for out in sessions.values() {
                let _ = out.send(text.clone());
            }
        }
        Some(addr) => {
            if let Some(out) = sessions.get(addr) {
                let _ = out.send(text);
            }
            // a vanished session is a dropped packet: at-most-once, by design
        }
        None => eprintln!("[hopd] packet without target: {text}"),
    }
}

/// Run the hop server forever: compile-side callers pass the already
/// compiled Lua chunk for the app.
pub fn serve(app_code: String, http_port: u16, ws_port: u16) -> mlua::Result<()> {
    {
        let app = app_code.clone();
        thread::spawn(move || http_thread(http_port, app));
    }
    let (tx, rx) = mpsc::channel::<Ev>();
    {
        let tx = tx.clone();
        thread::spawn(move || accept_thread(ws_port, tx));
    }

    // the server VM lives on this thread
    let lua = Lua::new();
    lua.globals().set("SIDE", "server")?;
    let outbox: Rc<RefCell<VecDeque<serde_json::Value>>> = Rc::new(RefCell::new(VecDeque::new()));
    let ob = outbox.clone();
    let send = lua.create_function(move |lua, pkt: LuaValue| {
        let json: serde_json::Value = lua.from_value(pkt)?;
        ob.borrow_mut().push_back(json);
        Ok(())
    })?;
    lua.globals().set("__send", send)?;
    let print_fn = lua.create_function(|_, msg: String| {
        println!("{msg}");
        Ok(())
    })?;
    lua.globals().set("__print", print_fn)?;
    lua.load(HOPRT_LUA).set_name("hoprt.lua").exec()?;
    lua.load(&app_code).set_name("app.lua").exec()?;

    let mut sessions: HashMap<String, mpsc::Sender<String>> = HashMap::new();
    println!("[hopd] serving http://localhost:{http_port}  (ws on :{ws_port})");

    for ev in rx {
        match ev {
            Ev::Conn(sid, out) => {
                let _ = out.send(format!(r#"{{"kind":"hello","session":"{sid}"}}"#));
                sessions.insert(sid.clone(), out);
                println!("[hopd] session {sid} connected");
                if let Err(e) = lua
                    .load(format!("__session_connect(\"{sid}\")"))
                    .set_name("session-connect")
                    .exec()
                {
                    eprintln!("[hopd] on_connect error: {e}");
                }
            }
            Ev::Pkt(sid, mut pkt) => {
                let kind = pkt["kind"].as_str().unwrap_or("").to_string();
                if kind == "call" || kind == "cast" {
                    // never trust a claimed identity
                    pkt["origin"] = serde_json::Value::String(sid.clone());
                    pkt["reply_to"] = serde_json::Value::String(sid.clone());
                }
                println!(
                    "        ~ wire {sid:>7} -> server   {kind:<5} {}",
                    pkt["hop"].as_str().unwrap_or("·")
                );
                let v = lua.to_value(&pkt)?;
                let f: Function = lua.globals().get("__receive")?;
                if let Err(e) = f.call::<()>(v) {
                    eprintln!("[hopd] receive error: {e}");
                }
            }
            Ev::Gone(sid) => {
                sessions.remove(&sid);
                println!("[hopd] session {sid} disconnected");
            }
        }
        // drain everything the VM emitted in response
        loop {
            let pkt = outbox.borrow_mut().pop_front();
            let Some(pkt) = pkt else { break };
            println!(
                "        ~ wire  server -> {:<8} {:<5} {}",
                pkt["to"].as_str().unwrap_or("?"),
                pkt["kind"].as_str().unwrap_or("?"),
                pkt["hop"].as_str().unwrap_or("·")
            );
            route(&sessions, &pkt);
        }
    }
    Ok(())
}
