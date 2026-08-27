//! A fake browser over real sockets: connect to hopd via WebSocket and
//! speak exactly the packets a browser's origin segments would send. This
//! validates the transport half without a browser: session hello,
//! on_connect snapshot, server-side state mutation, broadcast, and the
//! at-reply — all across real TCP.

use std::net::TcpStream;
use std::thread;
use std::time::Duration;

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

fn recv_json(ws: &mut Ws) -> serde_json::Value {
    loop {
        match ws.read().expect("ws read") {
            Message::Text(t) => return serde_json::from_str(t.as_str()).expect("json"),
            _ => continue,
        }
    }
}

#[test]
fn hopd_serves_the_todo_app_over_real_sockets() {
    let src = include_str!("../hop/todo.hop");
    let lua = hoprt::compiler::compile(src).expect("hopc compile");
    thread::spawn(move || {
        let _ = hoprt::serve::serve(lua, HTTP_PORT, WS_PORT);
    });

    // ── tab A connects: hello, then the on_connect snapshot (empty board)
    let mut a = connect_with_retry();
    let hello = recv_json(&mut a);
    assert_eq!(hello["kind"], "hello");
    assert_eq!(hello["session"], "A");
    let pkt = recv_json(&mut a);
    assert_eq!(pkt["kind"], "cast");
    assert_eq!(pkt["hop"], "on_connect:c1");

    // ── A "types a todo": send what add_todo's origin segment sends
    let call = serde_json::json!({
        "kind": "call", "flow": "A#1", "to": "server", "hop": "add_todo:1",
        "vars": { "text": "buy milk" }, "origin": "A", "reply_to": "A",
    });
    a.send(Message::Text(call.to_string().into())).unwrap();

    // broadcast render with the new item, then the reply completing the flow
    let cast = recv_json(&mut a);
    assert_eq!(cast["kind"], "cast");
    assert_eq!(cast["hop"], "add_todo:c1");
    assert_eq!(cast["vars"]["snapshot"][0]["text"], "buy milk");
    assert_eq!(cast["vars"]["snapshot"][0]["done"], false);
    let reply = recv_json(&mut a);
    assert_eq!(reply["kind"], "reply");
    assert_eq!(reply["flow"], "A#1");

    // ── tab B joins late and is brought up to date by on_connect
    let mut b = connect_with_retry();
    let hello = recv_json(&mut b);
    assert_eq!(hello["session"], "B");
    let pkt = recv_json(&mut b);
    assert_eq!(pkt["hop"], "on_connect:c1");
    assert_eq!(pkt["vars"]["snapshot"][0]["text"], "buy milk");

    // ── B toggles item 1 by invoking the onclick lambda's server segment
    // (in a real tab, clicking the <li> runs the closure, whose first act
    // is exactly this packet); the broadcast reaches BOTH tabs
    let call = serde_json::json!({
        "kind": "call", "flow": "B#1", "to": "server", "hop": "todo_view:l1:1",
        "vars": { "i": 1 },
        // a forged origin: hopd must overwrite it with the connection's id
        "origin": "A", "reply_to": "A",
    });
    b.send(Message::Text(call.to_string().into())).unwrap();

    let cast_b = recv_json(&mut b);
    assert_eq!(cast_b["hop"], "todo_view:c1");
    assert_eq!(cast_b["vars"]["snapshot"][0]["done"], true);
    let cast_a = recv_json(&mut a);
    assert_eq!(cast_a["hop"], "todo_view:c1");
    assert_eq!(cast_a["vars"]["snapshot"][0]["done"], true);

    // identity rode the connection: the reply went to B, not the forged A
    let reply = recv_json(&mut b);
    assert_eq!(reply["kind"], "reply");
    assert_eq!(reply["flow"], "B#1");
}
