//! Guardian UI suite: the real guardian.hop in a real browser, against a
//! stub slug (and no LLM). Every wound from the 2026-10-06 session is a
//! permanent assertion here:
//!
//! 1. the rank table parses slug's wrapped RPC body `{results:[...]}` —
//!    it once starved on a bare-array guess;
//! 2. the graph iframe points at slug's own replay through hopd's authed
//!    proxy — it was a hand-rolled SVG forgery;
//! 3. GetPair's bridge suggestions get ADOPTED: slug suggested scope_rank.rs
//!    (13th by line count, unknown to the top-12 scanner) and the guardian
//!    silently fell back to grinding the same six pairs — 240 votes, six
//!    disconnected islands;
//! 4. the fullscreen prompt view's topbar holds exactly one button —
//!    idiomorph once hoisted the vote screen's `play` button into it, label
//!    and closure id, a button that lied about what it does.

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use playwright_rs::{Playwright, GotoOptions, WaitUntil};

const STUB_SLUG: u16 = 19740;
const HOP_HTTP: u16 = 19750;
const HOP_WS: u16 = 19751;

const ROOM_WEB: &str = "gt7dc9ycode-review";
const SLUG_ITEM: &str = "https://slug.social/r/gt7dc9ycode-review/-/https://github.com/sortersocial/slug/blob/main/";

fn pkg_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hop-web/pkg")
}

/// The stub slug: GetPair always offers the scope_rank.rs bridge (the exact
/// suggestion the top-12 scanner could not answer), GetGardenRank returns
/// the wrapped batch shape, Post always succeeds, and the replay page is a
/// static HTML shell. Auth is ignored — the stub is not the thing under
/// test.
fn spawn_stub_slug() {
    thread::spawn(move || {
        let server = tiny_http::Server::http(&format!("127.0.0.1:{STUB_SLUG}"))
            .expect("stub slug bind");
        let pair_json = format!(
            r#"{{"results":[{{"ok":true,"result":{{"Pair":{{
                "left":"{SLUG_ITEM}server/src/scope_rank.rs",
                "right":"{SLUG_ITEM}server/src/reducer.rs",
                "left_body":"stub body","right_body":"stub body","threads":[]
            }}}}}}]}}"#
        );
        let rank_json = format!(
            r#"{{"results":[{{"ok":true,"result":{{"GardenRank":{{"components":[
                {{"pairs":2,"ranking":[
                    {{"item":"{SLUG_ITEM}server/src/dsl.rs","score":0.65}},
                    {{"item":"{SLUG_ITEM}server/src/api/rpc.rs","score":0.35}}]}},
                {{"pairs":1,"ranking":[
                    {{"item":"{SLUG_ITEM}server/src/scope_rank.rs","score":0.6}},
                    {{"item":"{SLUG_ITEM}server/src/reducer.rs","score":0.4}}]}}
            ],"unranked_items":[]}}}}}}]}}"#
        );
        let post_json = r#"{"results":[{"ok":true,"result":{"PostOk":{"events_appended":1}}}]}"#;
        for mut req in server.incoming_requests() {
            let url = req.url().to_string();
            if *req.method() == tiny_http::Method::Post && url.starts_with("/api/v0/rpc") {
                let mut body = String::new();
                let _ = std::io::Read::read_to_string(req.as_reader(), &mut body);
                let canned = if body.contains("GetPair") {
                    &pair_json
                } else if body.contains("GetGardenRank") {
                    &rank_json
                } else {
                    post_json
                };
                respond(req, 200, "application/json", canned.as_bytes());
            } else if url.starts_with("/r/") {
                if url.contains("v=history.json") {
                    respond(req, 200, "application/json", br#"{"history":[]}"#);
                } else {
                    respond(
                        req,
                        200,
                        "text/html; charset=utf-8",
                        b"<html><title>stub replay</title><body>stub graph</body></html>",
                    );
                }
            } else {
                respond(req, 404, "text/plain", b"stub: no route");
            }
        }
    });
}

fn respond(req: tiny_http::Request, code: u16, ctype: &str, body: &[u8]) {
    let _ = req.respond(
        tiny_http::Response::from_data(body.to_vec())
            .with_status_code(code)
            .with_header(
                tiny_http::Header::from_bytes(&b"Content-Type"[..], ctype.as_bytes()).unwrap(),
            ),
    );
}

/// The guardian source with its server lets re-pointed at the stub: no live
/// slug, no tokens, no real LLM (play stays unpressed in this suite; the
/// judge flow's stub-LLM path is a future assertion).
fn guardian_source() -> String {
    let src = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("hop/guardian.hop"),
    )
    .expect("read guardian.hop");
    src
        .replace(
            "server let slug_url = \"http://localhost:8080\";",
            &format!("server let slug_url = \"http://127.0.0.1:{STUB_SLUG}\";"),
        )
        .replace(
            "server let slug_token = \"slug_erosdev_erosdevsecret\";",
            "server let slug_token = \"stub-token\";",
        )
        .replace(
            "server let repo_scanner = \"python3 /Users/tommy/eros/scan_slug_repo.py\";",
            "server let repo_scanner = \"cat /tmp/guardian_ui_scan.json\";",
        )
}

fn spawn_hopd(src: String, http: u16, ws: u16) {
    let data = tempfile::tempdir().expect("tmpdir");
    let data_path = data.path().to_path_buf();
    std::mem::forget(data);
    let pkg = pkg_dir();
    thread::spawn(move || {
        let prog = hoprt::compiler::compile(&src).expect("compile");
        let _ = hoprt::serve::serve(
            std::rc::Rc::new(prog),
            src,
            http,
            ws,
            data_path,
            pkg,
            false,
        );
    });
    for _ in 0..100 {
        if ureq::get(&format!("http://127.0.0.1:{http}/config.json"))
            .timeout(Duration::from_millis(200))
            .call()
            .is_ok()
        {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!("hopd did not come up on port {http}");
}

/// Poll a JS expression until it returns JSON truthy for `pred`, with a
/// deadline. Returns the last value.
async fn poll_js(page: &playwright_rs::Page, expr: &str, secs: u64) -> String {
    let deadline = Instant::now() + Duration::from_secs(secs);
    let mut last = String::new();
    while Instant::now() < deadline {
        last = page
            .evaluate_value(expr)
            .await
            .unwrap_or_else(|e| format!("{{\"err\":\"{e}\"}}"));
        if last != "null" && last != "undefined" && !last.is_empty() {
            return last;
        }
        thread::sleep(Duration::from_millis(250));
    }
    last
}

async fn click_button(page: &playwright_rs::Page, label: &str) {
    let r = page
        .evaluate_value(&format!(
            r#"(() => {{
            const b = Array.from(document.querySelectorAll("button"))
                .find(x => x.textContent.trim() === "{label}");
            if (b) {{ b.click(); return "clicked"; }}
            return "missing";
        }})()"#
        ))
        .await
        .expect("click evaluate");
    assert_eq!(r, "clicked", "button {label:?} not found on the page");
}

#[tokio::test]
async fn guardian_surface_holds() {
    spawn_stub_slug();
    std::fs::write(
        "/tmp/guardian_ui_scan.json",
        r#"[{"path":"server/src/dsl.rs","lines":2406},{"path":"server/src/api/rpc.rs","lines":1897}]"#,
    )
    .expect("write scan fixture");
    spawn_hopd(guardian_source(), HOP_HTTP, HOP_WS);

    let pw = Playwright::launch().await.expect("playwright");
    let browser = pw.chromium().launch().await.expect("chromium");
    let page = browser.new_page().await.expect("page");
    page.goto(
        &format!("http://localhost:{HOP_HTTP}/"),
        GotoOptions::new().wait_until(WaitUntil::DomContentLoaded),
    )
    .await
    .expect("goto");

    // paint: the app renders and scan registered the fixture files
    poll_js(
        &page,
        r#"document.body.innerText.includes("vote screen") ? "ok" : null"#,
        10,
    )
    .await;

    click_button(&page, "scan repo").await;
    let cards = poll_js(
        &page,
        r#"(() => {
            const t = document.body.innerText;
            return t.includes("server/src/dsl.rs") && t.includes("server/src/api/rpc.rs") ? "ok" : null;
        })()"#,
        10,
    )
    .await;
    assert_eq!(cards, "ok", "vote screen did not show the scanned pair");

    // regression 1: the rank table parses the WRAPPED RPC body and shows
    // every component's rows (it once starved on a bare-array guess)
    let rows = poll_js(
        &page,
        r##"(() => {
            const cells = document.querySelectorAll("#app td.mono");
            return cells.length >= 4 ? String(cells.length) : null;
        })()"##,
        10,
    )
    .await;
    assert!(
        rows.parse::<i64>().unwrap_or(0) >= 4,
        "rank table did not render the stub's 4 rows across 2 components; got {rows}"
    );

    // regression 2: the graph iframe points at slug's own replay
    let src = poll_js(
        &page,
        r#"(() => {
            const f = document.querySelector("iframe.chart-frame");
            return f ? f.getAttribute("src") : null;
        })()"#,
        10,
    )
    .await;
    assert!(
        src.ends_with("?v=history") && src.contains(ROOM_WEB),
        "chart iframe src wrong: {src}"
    );

    // regression 3: GetPair's bridge suggestion is ADOPTED — scope_rank.rs
    // is not in the scanned fixture, the stub keeps offering it, and the
    // vote screen must show the suggested bridge, not a local grind pair
    let bridge = poll_js(
        &page,
        r#"(() => {
            // the bridge must appear as the SUGGESTED PAIR on the vote
            // screen — the rank table's raw URL labels also contain file
            // names and once let a loose assertion pass on a silent fallback
            const cards = Array.from(document.querySelectorAll(".pair-card"))
                .map(c => c.textContent);
            const scope = cards.filter(c => c.includes("server/src/scope_rank.rs"));
            return scope.length ? "ok" : null;
        })()"#,
        15,
    )
    .await;
    assert_eq!(
        bridge, "ok",
        "slug's scope_rank.rs bridge suggestion was not adopted onto the vote screen"
    );

    // regression 4: the prompt view's topbar holds exactly one button and
    // it is the back button — idiomorph once hoisted the vote screen's
    // `play` button in here, label and closure id
    click_button(&page, "⤢ prompt").await;
    let prompt = poll_js(
        &page,
        r#"(() => {
            const btns = Array.from(document.querySelectorAll(".log-topbar button"))
                .map(b => b.textContent.trim());
            // the prompt view renders .prompt-pre only once a judgment has
            // run; with no LLM in this suite the empty branch shows instead —
            // detect the VIEW by its topbar, not by the element
            if (!document.body.innerText.includes("verbatim prompt sent to the model"))
                return null;
            return JSON.stringify({
                count: btns.length,
                back: btns[0] === "← back",
                empty: document.body.innerText.includes("no prompt recorded yet")
            });
        })()"#,
        10,
    )
    .await;
    let parsed: serde_json::Value = match serde_json::from_str::<serde_json::Value>(&prompt) {
        Ok(v) if !v.is_null() => v,
        _ => {
            let body = page
                .evaluate_value("document.body.innerText.slice(0, 400)")
                .await
                .unwrap_or_default();
            panic!("prompt view never rendered (poll returned {prompt})\npage: {body}");
        }
    };
    assert_eq!(
        parsed["count"].as_i64(),
        Some(1),
        "prompt topbar must hold exactly one button; got {prompt}"
    );
    assert_eq!(parsed["back"].as_bool(), Some(true), "topbar button is not ← back: {prompt}");
    assert_eq!(
        parsed["empty"].as_bool(),
        Some(true),
        "prompt view should report no prompt yet (no LLM in this suite): {prompt}"
    );

    // and back home again, buttons honest
    click_button(&page, "← back").await;
    let home = poll_js(
        &page,
        r#"(() => {
            const labels = Array.from(document.querySelectorAll("button"))
                .map(b => b.textContent.trim());
            return labels.includes("⤢ prompt") && labels.includes("scan repo") ? "ok" : null;
        })()"#,
        10,
    )
    .await;
    assert_eq!(home, "ok", "back button did not return to the vote screen");
}