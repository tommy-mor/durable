//! Identity for hopd: Discord OAuth behind an HMAC-signed cookie.
//!
//! The shape mirrors how identity already flows through the runtime — a
//! cookie the HTTP shell sets and the WebSocket handshake reads back.
//! `hop_user` (an anonymous random token) stays as the fallback; this
//! module adds `hop_auth`, a *signed* claim minted by the OAuth callback:
//!
//! ```text
//! hop_auth = base64url(json{uid,name,avatar}) . hex(hmac-sha256)
//! ```
//!
//! A signed uid ("d:<discord id>") is durable across browsers and
//! devices, and is what the admin allowlist matches against. Forged or
//! doctored cookies fail the MAC and fall back to anonymous.
//!
//! Env:
//! - DISCORD_CLIENT_ID / DISCORD_CLIENT_SECRET — the OAuth app.
//! - DISCORD_REDIRECT_URI — defaults to http://localhost:<port>/auth/discord/callback.
//! - DISCORD_AUTHORIZE_URL / DISCORD_API_BASE — overridable for a mock.
//! - HOP_ADMIN_DISCORD_IDS — comma-separated discord ids that may edit.
//!   Unset or empty: open mode, every connection is an admin (dev).
//! - HOP_SESSION_SECRET — cookie-signing key; generated and persisted
//!   under the data dir when absent, so restarts keep sessions valid.

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

/// An authenticated identity, as carried by the hop_auth cookie.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthUser {
    /// "d:<discord id>"
    pub uid: String,
    pub name: String,
    pub avatar: String,
}

// ---------------------------------------------------------------------------
// signed cookie
// ---------------------------------------------------------------------------

fn mac_hex(secret: &[u8], payload: &[u8]) -> String {
    let mut mac = HmacSha256::new_from_slice(secret).expect("hmac accepts any key length");
    mac.update(payload);
    hex(&mac.finalize().into_bytes())
}

pub fn sign_auth(secret: &[u8], user: &AuthUser) -> String {
    let json = serde_json::json!({ "uid": user.uid, "name": user.name, "avatar": user.avatar });
    let payload = b64url(json.to_string().as_bytes());
    let sig = mac_hex(secret, payload.as_bytes());
    format!("{payload}.{sig}")
}

pub fn verify_auth(secret: &[u8], cookie: &str) -> Option<AuthUser> {
    let (payload, sig) = cookie.split_once('.')?;
    // constant-time via Mac::verify_slice
    let mut mac = HmacSha256::new_from_slice(secret).ok()?;
    mac.update(payload.as_bytes());
    let sig_bytes = unhex(sig)?;
    mac.verify_slice(&sig_bytes).ok()?;
    let json = b64url_decode(payload)?;
    let v: serde_json::Value = serde_json::from_slice(&json).ok()?;
    let uid = v["uid"].as_str()?.to_string();
    if uid.is_empty() {
        return None;
    }
    Some(AuthUser {
        uid,
        name: v["name"].as_str().unwrap_or_default().to_string(),
        avatar: v["avatar"].as_str().unwrap_or_default().to_string(),
    })
}

/// The OAuth `state` parameter, stateless: a signed timestamp. Accepted
/// for ten minutes; enough CSRF protection for a login redirect.
pub fn mint_state(secret: &[u8]) -> String {
    let ts = now_secs().to_string();
    let sig = mac_hex(secret, ts.as_bytes());
    format!("{ts}.{sig}")
}

pub fn check_state(secret: &[u8], state: &str) -> bool {
    let Some((ts, sig)) = state.split_once('.') else { return false };
    let Some(sig_bytes) = unhex(sig) else { return false };
    let mut mac = HmacSha256::new_from_slice(secret).expect("any key length");
    mac.update(ts.as_bytes());
    if mac.verify_slice(&sig_bytes).is_err() {
        return false;
    }
    let Ok(then) = ts.parse::<u64>() else { return false };
    now_secs().saturating_sub(then) < 600
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// HOP_SESSION_SECRET, or a key generated once and kept next to the data
/// (restart-stable sessions without configuration).
pub fn session_secret(data_dir: &std::path::Path) -> Vec<u8> {
    if let Ok(s) = std::env::var("HOP_SESSION_SECRET") {
        if !s.is_empty() {
            return s.into_bytes();
        }
    }
    let path = data_dir.join(".hop_secret");
    if let Ok(s) = std::fs::read(&path) {
        if !s.is_empty() {
            return s;
        }
    }
    // no RNG dep: hash clock, pid, and a fresh allocation address
    use std::hash::{Hash, Hasher};
    let mut out = Vec::with_capacity(32);
    let mut h = std::collections::hash_map::DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .hash(&mut h);
    std::process::id().hash(&mut h);
    (&out as *const _ as usize).hash(&mut h);
    for _ in 0..4 {
        let x = h.finish();
        out.extend_from_slice(&x.to_le_bytes());
        x.hash(&mut h);
    }
    let _ = std::fs::create_dir_all(data_dir);
    let _ = std::fs::write(&path, &out);
    out
}

// ---------------------------------------------------------------------------
// admin allowlist
// ---------------------------------------------------------------------------

/// None: open mode (no allowlist configured). Some(ids): uids ("d:…")
/// that may edit.
pub fn admin_uids() -> Option<Vec<String>> {
    let raw = std::env::var("HOP_ADMIN_DISCORD_IDS").ok()?;
    let ids: Vec<String> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| format!("d:{s}"))
        .collect();
    if ids.is_empty() {
        None
    } else {
        Some(ids)
    }
}

pub fn is_admin(admins: &Option<Vec<String>>, uid: &str) -> bool {
    match admins {
        None => true,
        Some(ids) => ids.iter().any(|a| a == uid),
    }
}

// ---------------------------------------------------------------------------
// Discord OAuth (blocking, ureq — matches hopd's effect executor style)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct DiscordCfg {
    pub client_id: String,
    pub client_secret: String,
    pub redirect_uri: String,
    pub authorize_url: String,
    pub api_base: String,
}

impl DiscordCfg {
    /// None when DISCORD_CLIENT_ID/SECRET are absent — login stays off.
    pub fn from_env(http_port: u16) -> Option<Self> {
        let client_id = std::env::var("DISCORD_CLIENT_ID").ok().filter(|s| !s.is_empty())?;
        let client_secret =
            std::env::var("DISCORD_CLIENT_SECRET").ok().filter(|s| !s.is_empty())?;
        Some(Self {
            client_id,
            client_secret,
            redirect_uri: std::env::var("DISCORD_REDIRECT_URI").unwrap_or_else(|_| {
                format!("http://localhost:{http_port}/auth/discord/callback")
            }),
            authorize_url: std::env::var("DISCORD_AUTHORIZE_URL")
                .unwrap_or_else(|_| "https://discord.com/oauth2/authorize".to_string()),
            api_base: std::env::var("DISCORD_API_BASE")
                .unwrap_or_else(|_| "https://discord.com/api".to_string()),
        })
    }

    pub fn authorize_redirect(&self, state: &str) -> String {
        format!(
            "{}?client_id={}&response_type=code&scope=identify&redirect_uri={}&state={}",
            self.authorize_url,
            urlenc(&self.client_id),
            urlenc(&self.redirect_uri),
            urlenc(state),
        )
    }

    /// code → access token → /users/@me → AuthUser.
    pub fn exchange(&self, code: &str) -> Result<AuthUser, String> {
        let body = format!(
            "grant_type=authorization_code&code={}&redirect_uri={}&client_id={}&client_secret={}",
            urlenc(code),
            urlenc(&self.redirect_uri),
            urlenc(&self.client_id),
            urlenc(&self.client_secret),
        );
        let resp = ureq::post(&format!("{}/oauth2/token", self.api_base.trim_end_matches('/')))
            .set("Content-Type", "application/x-www-form-urlencoded")
            .send_string(&body)
            .map_err(|e| format!("token: {e}"))?
            .into_string()
            .map_err(|e| e.to_string())?;
        let tok: serde_json::Value =
            serde_json::from_str(&resp).map_err(|e| format!("token json: {e}"))?;
        let access = tok["access_token"].as_str().ok_or("no access_token")?;

        let me = ureq::get(&format!("{}/users/@me", self.api_base.trim_end_matches('/')))
            .set("Authorization", &format!("Bearer {access}"))
            .call()
            .map_err(|e| format!("me: {e}"))?
            .into_string()
            .map_err(|e| e.to_string())?;
        let u: serde_json::Value = serde_json::from_str(&me).map_err(|e| format!("me json: {e}"))?;
        let id = u["id"].as_str().ok_or("no user id")?;
        let name = u["global_name"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| u["username"].as_str())
            .unwrap_or("discord user");
        let avatar = match u["avatar"].as_str() {
            Some(hash) if !hash.is_empty() => {
                format!("https://cdn.discordapp.com/avatars/{id}/{hash}.png")
            }
            _ => String::new(),
        };
        Ok(AuthUser { uid: format!("d:{id}"), name: name.to_string(), avatar })
    }
}

// ---------------------------------------------------------------------------
// small codecs (no extra deps)
// ---------------------------------------------------------------------------

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

fn b64url(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(B64[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(B64[n as usize & 63] as char);
        }
    }
    out
}

fn b64url_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            _ => return None,
        } as u32)
    };
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        if chunk.len() < 2 {
            return None;
        }
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Some(out)
}

/// Percent-encode for a query component.
fn urlenc(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user() -> AuthUser {
        AuthUser { uid: "d:123".into(), name: "tommy".into(), avatar: "http://a/x.png".into() }
    }

    #[test]
    fn sign_and_verify_round_trip() {
        let secret = b"test-secret";
        let cookie = sign_auth(secret, &user());
        assert_eq!(verify_auth(secret, &cookie), Some(user()));
    }

    #[test]
    fn forged_and_doctored_cookies_fail() {
        let secret = b"test-secret";
        let cookie = sign_auth(secret, &user());
        // wrong key
        assert_eq!(verify_auth(b"other-secret", &cookie), None);
        // doctored payload (claim a different uid, keep the signature)
        let (_, sig) = cookie.split_once('.').unwrap();
        let forged_payload = b64url(br#"{"uid":"d:999","name":"mallory","avatar":""}"#);
        assert_eq!(verify_auth(secret, &format!("{forged_payload}.{sig}")), None);
        // garbage
        assert_eq!(verify_auth(secret, "not-a-cookie"), None);
        assert_eq!(verify_auth(secret, ""), None);
    }

    #[test]
    fn state_expires_and_rejects_tampering() {
        let secret = b"s";
        let state = mint_state(secret);
        assert!(check_state(secret, &state));
        assert!(!check_state(b"other", &state));
        let old = format!("1.{}", {
            let mut mac = HmacSha256::new_from_slice(secret).unwrap();
            mac.update(b"1");
            hex(&mac.finalize().into_bytes())
        });
        assert!(!check_state(secret, &old), "ten-minute window");
    }

    #[test]
    fn b64_round_trips() {
        for s in ["", "a", "ab", "abc", "abcd", "hello world! {\"json\":1}"] {
            assert_eq!(b64url_decode(&b64url(s.as_bytes())), Some(s.as_bytes().to_vec()));
        }
    }

    #[test]
    fn admin_allowlist_matches_uids() {
        assert!(is_admin(&None, "anyone"));
        let list = Some(vec!["d:1".to_string(), "d:2".to_string()]);
        assert!(is_admin(&list, "d:1"));
        assert!(!is_admin(&list, "d:3"));
        assert!(!is_admin(&list, "u:anonymous"));
    }
}
