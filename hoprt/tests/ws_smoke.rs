//! A fake browser over real sockets: connect to hopd via WebSocket and
//! speak exactly the CBOR packets a browser's origin segments would send.
//! This validates the transport half without a browser: session hello,
//! on_connect panel boot, a server-side append, the panel's re-render data
//! riding hop replies (not broadcasts), and identity stamping — all across
//! real TCP, all binary.
//!
//! Wire shape of the wait world: connecting births the tab's panel under
//! her key (no boot cast — on_connect runs owned by the tab on the
//! server); from then on the server *calls her* with each fresh render
//! (the panel loop hops to the browser), she answers, and the panel
//! sleeps again. Writers get bare replies. Forged answers and cancels
//! from another tab are dropped at the server.

use std::net::TcpStream;
use std::thread;
use std::time::Duration;

use hoprt::value::{decode, encode, Value};
use tungstenite::stream::MaybeTlsStream;
use tungstenite::{Message, WebSocket};

type Ws = WebSocket<MaybeTlsStream<TcpStream>>;

const HTTP_PORT: u16 = 19700;
const WS_PORT: u16 = 19701;

fn connect_with_retry() -> Ws {
    for _ in 0..100 {
        if let Ok((mut ws, _)) = tungstenite::connect(format!("ws://127.0.0.1:{WS_PORT}")) {
            if let MaybeTlsStream::Plain(s) = ws.get_mut() {
                s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            }
            return ws;
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("hopd did not come up on port {WS_PORT}");
}

fn recv(ws: &mut Ws) -> Value {
    loop {
        match ws.read().expect("ws read") {
            Message::Binary(b) => return decode(&b).expect("cbor packet"),
            _ => continue,
        }
    }
}

fn send(ws: &mut Ws, pkt: &Value) {
    ws.send(Message::Binary(encode(pkt).expect("encode").into())).unwrap();
}

fn s(x: &str) -> Value {
    Value::str(x)
}

fn map(entries: &[(&str, Value)]) -> Value {
    Value::map(
        entries
            .iter()
            .map(|(k, v)| (Value::str(*k), v.clone()))
            .collect(),
    )
}

fn call(flow: &str, hop: &str, vars: &[(&str, Value)]) -> Value {
    map(&[
        ("kind", s("call")),
        ("flow", s(flow)),
        ("to", s("server")),
        ("hop", s(hop)),
        ("vars", map(vars)),
        ("origin", s("FAKE")),
        ("reply_to", s("FAKE")),
    ])
}

fn reply(flow: &str, value: Value) -> Value {
    map(&[
        ("kind", s("reply")),
        ("flow", s(flow)),
        ("to", s("server")),
        ("value", value),
    ])
}

/// Snapshot array out of a panel render call's vars.
fn snap_of(pkt: &Value) -> Vec<Value> {
    assert_eq!(pkt.get_field("kind"), s("call"), "a render is a call: {pkt}");
    assert_eq!(pkt.get_field("hop"), s("todo_panel:2"), "the panel's browser piece: {pkt}");
    match pkt.get_field("vars").get_field("snapshot") {
        Value::Array(a) => a.borrow().clone(),
        other => panic!("snapshot is {other}: {pkt}"),
    }
}

/// Read packets until one matches, collecting the rest (order across a
/// server pass is not part of the contract).
fn recv_n(ws: &mut Ws, n: usize) -> Vec<Value> {
    (0..n).map(|_| recv(ws)).collect()
}

fn find<'a>(pkts: &'a [Value], kind: &str, flow: &str) -> &'a Value {
    pkts.iter()
        .find(|p| p.get_field("kind") == s(kind) && p.get_field("flow") == s(flow))
        .unwrap_or_else(|| panic!("no {kind} for {flow} in {pkts:?}"))
}

fn flow_of(pkt: &Value) -> String {
    pkt.get_field("flow").as_str().unwrap_or("?").to_string()
}

#[test]
fn hopd_serves_the_todo_app_over_real_sockets() {
    let data = tempfile::tempdir().unwrap();
    let data_path = data.path().to_path_buf();
    thread::spawn(move || {
        // compile inside the thread: programs hold Rc values and are
        // thread-local, like everything else in a VM
        let src = include_str!("../hop/todo.hop");
        let prog = hoprt::compiler::compile(src).expect("compile");
        let _ = hoprt::serve::serve(
            std::rc::Rc::new(prog),
            src.to_string(),
            HTTP_PORT,
            WS_PORT,
            data_path,
            std::path::PathBuf::from("../hop-web/pkg"),
            None,
            None,
            Vec::new(),
            None,
            false,
        );
    });

    // ── tab A connects: hello, then the server calls her with the first
    // render (on_connect birthed her panel under "board", server-side).
    let mut a = connect_with_retry();
    let hello = recv(&mut a);
    assert_eq!(hello.get_field("kind"), s("hello"));
    assert_eq!(hello.get_field("session"), s("A"));
    let render = recv(&mut a);
    let panel_a = flow_of(&render);
    assert_eq!(snap_of(&render).len(), 0, "empty board renders: {render}");
    // The tab "renders" and answers; the panel goes to sleep in store.wait.
    send(&mut a, &reply(&panel_a, Value::Nil));

    // ── A "types a todo": the writer gets a bare reply, and the woken panel
    // calls A with the new board. One server pass, either order.
    send(&mut a, &call("A#1", "add_todo:1", &[("text", s("buy milk"))]));
    let pkts = recv_n(&mut a, 2);
    find(&pkts, "reply", "A#1");
    let items = snap_of(find(&pkts, "call", &panel_a));
    assert_eq!(items.len(), 1, "woken panel re-reads");
    assert_eq!(items[0].get_field("text"), s("buy milk"));
    send(&mut a, &reply(&panel_a, Value::Nil));

    // ── tab B joins late: hello, first render reads the existing board
    // (replay from the tape, not a push)
    let mut b = connect_with_retry();
    let hello = recv(&mut b);
    assert_eq!(hello.get_field("session"), s("B"));
    let render = recv(&mut b);
    let panel_b = flow_of(&render);
    assert_ne!(panel_b, panel_a, "each tab owns her panel");
    assert_eq!(snap_of(&render).len(), 1, "late tab sees the board: {render}");
    send(&mut b, &reply(&panel_b, Value::Nil));

    // ── B attacks A's sleeping panel: a forged answer, a forged error, and
    // a forged cancel, all naming A's panel flow. hopd stamps `from: "B"`;
    // the server drops all three. If any landed, A's panel would be dead
    // or resumed with junk, and the toggle below wouldn't reach her.
    send(&mut b, &reply(&panel_a, s("pwned")));
    send(
        &mut b,
        &map(&[("kind", s("error")), ("flow", s(&panel_a)), ("to", s("server")), ("err", s("pwned"))]),
    );
    send(&mut b, &map(&[("kind", s("cancel")), ("flow", s(&panel_a)), ("to", s("server"))]));

    // ── B toggles item 0 (forged origin: hopd stamps the connection's id).
    // The writer's bare reply goes to B, not the forged A; both panels wake
    // and re-render with done=true.
    send(
        &mut b,
        &map(&[
            ("kind", s("call")),
            ("flow", s("B#1")),
            ("to", s("server")),
            ("hop", s("toggle_todo:1")),
            ("vars", map(&[("id", Value::Int(0))])),
            ("origin", s("A")),
            ("reply_to", s("A")),
        ]),
    );
    let pkts = recv_n(&mut b, 2);
    find(&pkts, "reply", "B#1");
    let items = snap_of(find(&pkts, "call", &panel_b));
    assert_eq!(items[0].get_field("done"), Value::Bool(true), "B sees the toggle");
    send(&mut b, &reply(&panel_b, Value::Nil));

    let render = recv(&mut a);
    assert_eq!(render.get_field("flow"), s(&panel_a), "A's panel survived B's forgeries: {render}");
    let items = snap_of(&render);
    assert_eq!(items[0].get_field("done"), Value::Bool(true), "A sees the toggle");
    send(&mut a, &reply(&panel_a, Value::Nil));
}
