//! Discord login over real sockets, against a mock Discord: the authorize
//! redirect carries a signed state, the callback exchanges the code and
//! mints the signed hop_auth cookie, the WebSocket handshake turns that
//! cookie into the durable "d:<id>" identity — and a forged cookie or a
//! doctored state falls back to anonymous.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

use hoprt::value::{decode, Value};
use tungstenite::client::IntoClientRequest;
use tungstenite::stream::MaybeTlsStream;
use tungstenite::Message;

const HTTP_PORT: u16 = 19710;
const WS_PORT: u16 = 19711;
const DISCORD_PORT: u16 = 19712;

/// The mock Discord: token exchanges always succeed for user 42.
fn mock_discord() {
    let listener = TcpListener::bind(("127.0.0.1", DISCORD_PORT)).expect("bind mock discord");
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 1024];
            // read until the header terminator (bodies are tiny; ureq
            // sends Content-Length'd forms that fit one read in practice,
            // and the token route ignores the body anyway)
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        buf.extend_from_slice(&chunk[..n]);
                        if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            let req = String::from_utf8_lossy(&buf);
            let body = if req.starts_with("POST /oauth2/token") {
                r#"{"access_token":"tok-1","token_type":"Bearer"}"#
            } else if req.starts_with("GET /users/@me") {
                r#"{"id":"42","username":"tommy","global_name":"Tommy","avatar":"abc123"}"#
            } else {
                r#"{"error":"unknown route"}"#
            };
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = stream.write_all(resp.as_bytes());
        });
    }
}

/// GET a path on hopd, returning (status, lowercased header map).
fn http_get(path: &str) -> (u16, HashMap<String, String>) {
    for _ in 0..100 {
        if let Ok(mut s) = TcpStream::connect(("127.0.0.1", HTTP_PORT)) {
            let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
            s.write_all(req.as_bytes()).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let mut out = String::new();
            let _ = s.read_to_string(&mut out);
            let head = out.split("\r\n\r\n").next().unwrap_or("");
            let mut lines = head.lines();
            let status: u16 = lines
                .next()
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|c| c.parse().ok())
                .unwrap_or(0);
            let headers = lines
                .filter_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    Some((k.trim().to_lowercase(), v.trim().to_string()))
                })
                .collect();
            return (status, headers);
        }
        thread::sleep(Duration::from_millis(100));
    }
    panic!("hopd did not come up on port {HTTP_PORT}");
}

fn ws_hello(cookie: Option<&str>) -> Value {
    let mut req = format!("ws://127.0.0.1:{WS_PORT}").into_client_request().unwrap();
    if let Some(c) = cookie {
        req.headers_mut().insert("Cookie", c.parse().unwrap());
    }
    let (mut ws, _) = tungstenite::connect(req).expect("ws connect");
    if let MaybeTlsStream::Plain(s) = ws.get_mut() {
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    }
    loop {
        match ws.read().expect("ws read") {
            Message::Binary(b) => return decode(&b).expect("cbor"),
            _ => continue,
        }
    }
}

#[test]
fn discord_login_mints_identity_and_forgeries_fall_back() {
    // one test, one process: env is global
    std::env::set_var("DISCORD_CLIENT_ID", "client-1");
    std::env::set_var("DISCORD_CLIENT_SECRET", "secret-1");
    std::env::set_var("DISCORD_AUTHORIZE_URL", format!("http://127.0.0.1:{DISCORD_PORT}/oauth2/authorize"));
    std::env::set_var("DISCORD_API_BASE", format!("http://127.0.0.1:{DISCORD_PORT}"));
    std::env::set_var("HOP_SESSION_SECRET", "oauth-test-secret");
    std::env::set_var("HOP_ADMIN_DISCORD_IDS", "42");

    thread::spawn(mock_discord);
    let data = tempfile::tempdir().unwrap();
    let data_path = data.path().to_path_buf();
    thread::spawn(move || {
        let src = include_str!("../hop/tournament.hop");
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

    // ── the login redirect carries client_id and a signed state
    let (status, headers) = http_get("/auth/discord");
    assert_eq!(status, 302);
    let loc = headers.get("location").expect("authorize redirect");
    assert!(loc.starts_with(&format!("http://127.0.0.1:{DISCORD_PORT}/oauth2/authorize")), "{loc}");
    assert!(loc.contains("client_id=client-1"), "{loc}");
    let state = loc.split("state=").nth(1).expect("state param").to_string();

    // ── a doctored state is ignored: no cookie minted
    let (status, headers) = http_get("/auth/discord/callback?code=abc&state=1.deadbeef");
    assert_eq!(status, 302);
    assert!(
        !headers.get("set-cookie").map(|c| c.starts_with("hop_auth=")).unwrap_or(false),
        "bad state must not mint a cookie"
    );

    // ── the real callback exchanges the code and sets the signed cookie
    let (status, headers) = http_get(&format!("/auth/discord/callback?code=abc&state={state}"));
    assert_eq!(status, 302);
    let set = headers.get("set-cookie").expect("hop_auth set");
    assert!(set.starts_with("hop_auth="), "{set}");
    let cookie = set.split(';').next().unwrap().to_string();

    // ── the WS handshake reads the cookie: identity is d:42
    let hello = ws_hello(Some(&cookie));
    assert_eq!(hello.get_field("kind"), Value::str("hello"));
    assert_eq!(hello.get_field("user"), Value::str("d:42"));

    // ── a forged cookie (bad signature) falls back to anonymous
    let hello = ws_hello(Some("hop_auth=eyJ1aWQiOiJkOjk5In0.0000000000000000"));
    let user = match hello.get_field("user") {
        Value::Str(s) => s.to_string(),
        other => panic!("user: {other}"),
    };
    assert!(user.starts_with('u'), "forgery must not become d:99 — got {user}");

    // ── no cookie at all: anonymous too
    let hello = ws_hello(None);
    let user = match hello.get_field("user") {
        Value::Str(s) => s.to_string(),
        other => panic!("user: {other}"),
    };
    assert!(user.starts_with('u'), "{user}");
}
