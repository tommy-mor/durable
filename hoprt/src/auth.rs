//! Optional gate in front of hopd. When `HOP_PASSWORD` or `PASSWORD` is
//! set, HTTP and the WebSocket handshake require a `hop_auth` cookie
//! minted by POST /login. Unset — the default — leaves hopd open, which
//! is what the tests and local `hopd` runs expect.

use sha2::{Digest, Sha256};

pub const COOKIE: &str = "hop_auth";

pub fn required() -> bool {
    token().is_some()
}

pub fn token() -> Option<String> {
    let password = std::env::var("HOP_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("PASSWORD").ok().filter(|s| !s.is_empty()))?;
    Some(hash_password(&password))
}

pub fn hash_password(password: &str) -> String {
    format!("{:x}", Sha256::digest(format!("hop|{password}").as_bytes()))
}

pub fn cookie_value(header: &str) -> Option<String> {
    header.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == COOKIE && !v.is_empty()).then(|| v.to_string())
    })
}

pub fn is_authed(cookie_header: Option<&str>) -> bool {
    let Some(want) = token() else {
        return true;
    };
    cookie_header
        .and_then(cookie_value)
        .is_some_and(|got| got == want)
}

pub fn verify_password(password: &str) -> bool {
    token().is_some_and(|want| hash_password(password) == want)
}

pub fn set_cookie_header(secure: bool) -> Option<String> {
    let tok = token()?;
    let mut c = format!("{COOKIE}={tok}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000");
    if secure {
        c.push_str("; Secure");
    }
    Some(c)
}

fn login_title() -> String {
    std::env::var("HOP_LOGIN_TITLE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "hop".into())
}

pub fn login_html(error: Option<&str>) -> String {
    let title = login_title();
    let err = error
        .map(|e| format!(r#"<p class="err">{e}</p>"#))
        .unwrap_or_default();
    format!(
        r#"<!doctype html>
<html>
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width,initial-scale=1">
  <title>{title} — login</title>
  <style>
    body {{ margin:0; background:#11151c; color:#e8e4dc; font:16px system-ui,sans-serif;
           display:flex; min-height:100vh; align-items:center; justify-content:center; }}
    form {{ background:#191e27; padding:28px 32px; border-radius:12px; width:min(360px,90vw);
            border:1px solid #303641; }}
    h1 {{ font:italic 32px Georgia,serif; margin:0 0 8px; }}
    p {{ color:#a9afb9; margin:0 0 18px; }}
    .err {{ color:#ff9caa; }}
    input, button {{ width:100%; box-sizing:border-box; padding:10px 12px; border-radius:6px;
                     font:inherit; }}
    input {{ background:#11151c; color:#e8e4dc; border:1px solid #303641; margin-bottom:12px; }}
    button {{ background:#f2b880; color:#111; border:0; cursor:pointer; font-weight:600; }}
  </style>
</head>
<body>
  <form method="post" action="/login">
    <h1>{title}</h1>
    <p>password to enter.</p>
    {err}
    <input type="password" name="password" autofocus autocomplete="current-password">
    <button type="submit">enter</button>
  </form>
</body>
</html>"#
    )
}

pub fn parse_form_password(body: &str) -> Option<String> {
    body.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == "password").then(|| form_decode(v))
    })
}

fn form_decode(s: &str) -> String {
    let s = s.replace('+', " ");
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(byte) =
                u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or(""), 16)
            {
                out.push(byte);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_not_plaintext() {
        let a = hash_password("secret");
        let b = hash_password("secret");
        assert_eq!(a, b);
        assert_ne!(a, "secret");
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn cookie_roundtrip() {
        let h = format!("{COOKIE}=abc123; hop_user=u1");
        assert_eq!(cookie_value(&h).as_deref(), Some("abc123"));
        assert!(cookie_value("hop_user=u1").is_none());
    }

    #[test]
    fn form_password_decodes() {
        assert_eq!(
            parse_form_password("password=hello%20there"),
            Some("hello there".into())
        );
    }
}
