//! A fake browser over real sockets: connect to hopd via WebSocket and
//! speak exactly the CBOR packets a browser's origin segments would send.
//! This validates the transport half without a browser: session hello,
//! on_connect snapshot, server-side state mutation, broadcast, and the
//! at-reply — all across real TCP, all binary.

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

/// vars.snapshot[i].field
fn snap_field(pkt: &Value, i: i64, field: &str) -> Value {
    let snapshot = pkt.get_field("vars").get_field("snapshot");
    match &snapshot {
        Value::Array(a) => a.borrow()[i as usize].get_field(field),
        other => panic!("snapshot is {other}"),
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
            false,
        );
    });

    // ── tab A connects: hello, then the on_connect snapshot (empty board)
    let mut a = connect_with_retry();
    let hello = recv(&mut a);
    assert_eq!(hello.get_field("kind"), s("hello"));
    assert_eq!(hello.get_field("session"), s("A"));
    let pkt = recv(&mut a);
    assert_eq!(pkt.get_field("kind"), s("cast"));
    assert_eq!(pkt.get_field("hop"), s("on_connect:c1"));

    // ── A "types a todo": send what add_todo's origin segment sends
    send(
        &mut a,
        &map(&[
            ("kind", s("call")),
            ("flow", s("A#1")),
            ("to", s("server")),
            ("hop", s("add_todo:1")),
            ("vars", map(&[("text", s("buy milk"))])),
            ("origin", s("A")),
            ("reply_to", s("A")),
        ]),
    );

    // broadcast render with the new item, then the reply completing the flow
    let cast = recv(&mut a);
    assert_eq!(cast.get_field("kind"), s("cast"));
    assert_eq!(cast.get_field("hop"), s("add_todo:c1"));
    assert_eq!(snap_field(&cast, 0, "text"), s("buy milk"));
    assert_eq!(snap_field(&cast, 0, "done"), Value::Bool(false));
    let reply = recv(&mut a);
    assert_eq!(reply.get_field("kind"), s("reply"));
    assert_eq!(reply.get_field("flow"), s("A#1"));

    // ── tab B joins late and is brought up to date by on_connect
    let mut b = connect_with_retry();
    let hello = recv(&mut b);
    assert_eq!(hello.get_field("session"), s("B"));
    let pkt = recv(&mut b);
    assert_eq!(pkt.get_field("hop"), s("on_connect:c1"));
    assert_eq!(snap_field(&pkt, 0, "text"), s("buy milk"));

    // ── B toggles item 0 by invoking the onclick lambda's server segment
    // (in a real tab, clicking the <li> runs the closure, whose first act
    // is exactly this packet); the broadcast reaches BOTH tabs
    send(
        &mut b,
        &map(&[
            ("kind", s("call")),
            ("flow", s("B#1")),
            ("to", s("server")),
            ("hop", s("todo_view:l1:1")),
            ("vars", map(&[("id", Value::Int(0))])),
            // a forged origin: hopd must overwrite it with the connection's id
            ("origin", s("A")),
            ("reply_to", s("A")),
        ]),
    );

    let cast_b = recv(&mut b);
    assert_eq!(cast_b.get_field("hop"), s("todo_view:c1"));
    assert_eq!(snap_field(&cast_b, 0, "done"), Value::Bool(true));
    let cast_a = recv(&mut a);
    assert_eq!(cast_a.get_field("hop"), s("todo_view:c1"));
    assert_eq!(snap_field(&cast_a, 0, "done"), Value::Bool(true));

    // identity rode the connection: the reply went to B, not the forged A
    let reply = recv(&mut b);
    assert_eq!(reply.get_field("kind"), s("reply"));
    assert_eq!(reply.get_field("flow"), s("B#1"));
}
