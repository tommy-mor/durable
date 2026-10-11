//! A fake browser over real sockets: connect to hopd via WebSocket and
//! speak exactly the CBOR packets a browser's origin segments would send.
//! This validates the transport half without a browser: session hello,
//! on_connect panel boot, a server-side append, the panel's re-render data
//! riding hop replies (not broadcasts), and identity stamping — all across
//! real TCP, all binary.
//!
//! Wire shape of the wait world: the tab sends calls, the server answers
//! with replies whose vars carry the render data; writers get bare replies;
//! chain links arrive as casts the tab answers by sending the next
//! iteration's first hop.

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

/// Snapshot array out of a panel reply's vars.
fn snap_of(pkt: &Value) -> Vec<Value> {
    match pkt.get_field("value").get_field("vars").get_field("snapshot") {
        Value::Array(a) => a.borrow().clone(),
        other => panic!("snapshot is {other}: {pkt}"),
    }
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

    // ── tab A connects: hello, then the on_connect cast that boots her board
    let mut a = connect_with_retry();
    let hello = recv(&mut a);
    assert_eq!(hello.get_field("kind"), s("hello"));
    assert_eq!(hello.get_field("session"), s("A"));
    let pkt = recv(&mut a);
    assert_eq!(pkt.get_field("kind"), s("cast"));
    assert_eq!(pkt.get_field("hop"), s("on_connect:c1"));

    // ── emulate the tab VM: open_board hits server!() …
    send(&mut a, &call("A#1", "open_board:1", &[]));
    let pkt = recv(&mut a);
    assert_eq!(pkt.get_field("kind"), s("reply"));
    assert_eq!(pkt.get_field("flow"), s("A#1"));

    // … the spawn runs panel browser-side: her first server hop …
    send(&mut a, &call("A#2", "todo_panel:1", &[]));
    // … answered with the render data: the empty board
    let pkt = recv(&mut a);
    assert_eq!(pkt.get_field("flow"), s("A#2"));
    assert_eq!(snap_of(&pkt).len(), 0, "empty board renders: {pkt}");

    // ── the browser piece "renders", then hops back to wait …
    let me = pkt
        .get_field("value")
        .get_field("vars")
        .get_field("me")
        .as_str()
        .unwrap()
        .to_string();
    send(&mut a, &call("A#2", "todo_panel:2", &[("me", s(me.as_str()))]));
    // … and nothing comes back: she parked in store.wait (read timeout
    // would fire — so send the write on a NEW flow instead of reading)
    // ── A "types a todo": the writer appends and gets a bare reply. Three
    // packets land in one server pass, any order: the writer reply (A#3),
    // the woken panel's done-reply (A#2 returns after chaining), and the
    // chain cast spawning iteration 2.
    send(&mut a, &call("A#3", "add_todo:1", &[("text", s("buy milk"))]));
    let (mut replies, mut casts) = (Vec::new(), Vec::new());
    for _ in 0..3 {
        let pkt = recv(&mut a);
        match pkt.get_field("kind").as_str() {
            Some("reply") => replies.push(pkt),
            Some("cast") => casts.push(pkt),
            other => panic!("unexpected packet: {other:?} {pkt}"),
        }
    }
    assert!(replies.iter().any(|p| p.get_field("flow") == s("A#3")), "writer reply");
    // ── the woken panel chains: a cast arrives spawning the next iteration …
    assert_eq!(casts.len(), 1, "one chain cast");
    assert_eq!(casts[0].get_field("hop"), s("todo_panel:c1"));
    // … the tab spawns her browser-side: first server hop of iteration 2 …
    send(&mut a, &call("A#4", "todo_panel:1", &[]));
    let pkt = recv(&mut a);
    assert_eq!(pkt.get_field("flow"), s("A#4"));
    let items = snap_of(&pkt);
    assert_eq!(items.len(), 1, "woken panel re-reads: {pkt}");
    assert_eq!(items[0].get_field("text"), s("buy milk"));
    let me = pkt
        .get_field("value")
        .get_field("vars")
        .get_field("me")
        .as_str()
        .unwrap()
        .to_string();
    send(&mut a, &call("A#4", "todo_panel:2", &[("me", s(me.as_str()))]));
    // parked again — silence. On to tab B.

    // ── tab B joins late: hello, boot cast, same dance, but her panel
    // reads the existing board (replay from the tape, not a push)
    let mut b = connect_with_retry();
    let hello = recv(&mut b);
    assert_eq!(hello.get_field("session"), s("B"));
    let pkt = recv(&mut b);
    assert_eq!(pkt.get_field("hop"), s("on_connect:c1"));
    send(&mut b, &call("B#1", "open_board:1", &[]));
    let pkt = recv(&mut b);
    assert_eq!(pkt.get_field("flow"), s("B#1"));
    send(&mut b, &call("B#2", "todo_panel:1", &[]));
    let pkt = recv(&mut b);
    assert_eq!(snap_of(&pkt).len(), 1, "late tab sees the board: {pkt}");
    let me = pkt
        .get_field("value")
        .get_field("vars")
        .get_field("me")
        .as_str()
        .unwrap()
        .to_string();
    send(&mut b, &call("B#2", "todo_panel:2", &[("me", s(me.as_str()))]));

    // ── B toggles item 0 (forged origin: hopd stamps the connection's id).
    // The writer's bare reply goes to B, not the forged A; then BOTH tabs
    // get chain casts and re-render with done=true.
    send(
        &mut b,
        &map(&[
            ("kind", s("call")),
            ("flow", s("B#3")),
            ("to", s("server")),
            ("hop", s("toggle_todo:1")),
            ("vars", map(&[("id", Value::Int(0))])),
            ("origin", s("A")),
            ("reply_to", s("A")),
        ]),
    );
    // B's socket gets three packets, any order: her writer reply (B#3),
    // her panel's done-reply, and her chain cast.
    let (mut b_replies, mut b_cast) = (Vec::new(), None);
    for _ in 0..3 {
        let pkt = recv(&mut b);
        match pkt.get_field("kind").as_str() {
            Some("reply") => b_replies.push(pkt),
            Some("cast") => b_cast = Some(pkt),
            other => panic!("unexpected packet: {other:?} {pkt}"),
        }
    }
    assert!(b_replies.iter().any(|p| p.get_field("flow") == s("B#3")), "writer reply to B");
    assert_eq!(b_cast.as_ref().expect("chain cast to B").get_field("hop"), s("todo_panel:c1"));

    // A's socket gets two packets, either order: her panel's done-reply
    // and her chain cast.
    let (mut a_replies, mut a_cast) = (Vec::new(), None);
    for _ in 0..2 {
        let pkt = recv(&mut a);
        match pkt.get_field("kind").as_str() {
            Some("reply") => a_replies.push(pkt),
            Some("cast") => a_cast = Some(pkt),
            other => panic!("unexpected packet: {other:?} {pkt}"),
        }
    }
    assert_eq!(a_cast.as_ref().expect("chain cast to A").get_field("hop"), s("todo_panel:c1"));

    // Each tab spawns her next iteration and re-reads done=true.
    for (tab, ws, flow) in [("B", &mut b, "B#4"), ("A", &mut a, "A#5")] {
        send(ws, &call(flow, "todo_panel:1", &[]));
        let pkt = recv(ws);
        let items = snap_of(&pkt);
        assert_eq!(items.len(), 1, "{tab} re-reads");
        assert_eq!(items[0].get_field("done"), Value::Bool(true), "{tab} sees the toggle");
        let me = pkt
            .get_field("value")
            .get_field("vars")
            .get_field("me")
            .as_str()
            .unwrap()
            .to_string();
        send(ws, &call(flow, "todo_panel:2", &[("me", s(me.as_str()))]));
    }
}
