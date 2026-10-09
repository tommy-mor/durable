//! Guardian UI suite: the real guardian.hop in a real browser, against a
//! stub slug (and no LLM). Every wound from the 2026-10-06 session is a
//! permanent assertion here:
//!
//! 1. the rank table parses slug's wrapped RPC body `{results:[...]}` —
//!    it once starved on a bare-array guess;
//! 2. the graph iframe points at slug's own replay through hopd's authed
//!    proxy — it was a hand-rolled SVG forgery;
//! 3. GetPair's suggestion drives the vote screen DIRECTLY: the scanner
//!    registers every big file (no top-12 cut), so slug's bridge pair maps
//!    to known local files. There is no local fallback and no adoption —
//!    the old guardian adopted strangers with a wc -l shell-out, and
//!    before that silently ground the same six pairs: 240 votes, six
//!    disconnected islands;
//! 4. the fullscreen prompt view's topbar holds exactly one button —
//!    idiomorph once hoisted the vote screen's `play` button into it, label
//!    and closure id, a button that lied about what it does.
//! 5. a foreign GetPair suggestion (a URL item no scanned file matches)
//!    is refused LOUDLY and never rendered — there is NO shell in the
//!    judging loop at all anymore (files come through hopd's rooted fs
//!    effect), so the old `x/';touch${IFS}/tmp/canary;'` payload has
//!    nothing left to fire through;
//! 6. the play button's whole judging loop — tick, GetPair, fs.read,
//!    llm.stream, verdict, vote POST — runs end to end against a stub
//!    slug/LLM, because it once had no coverage at all.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
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

/// The canned verdict the stub streams as an OpenAI-compatible SSE
/// chat-completion: prose, then the JSON verdict the guardian parses.
const VERDICT_SSE: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"a wins on cohesion grounds\\n\"}}]}\n\n",
    "data: {\"choices\":[{\"delta\":{\"content\":\"{\\\"winner\\\":\\\"a\\\",\\\"why\\\":\\\"stub says a\\\"}\"}}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1234,\"completion_tokens\":56,\"total_tokens\":1290}}\n\n",
    "data: [DONE]\n\n",
);

/// The stub slug: GetPair always offers the scope_rank.rs × reducer.rs
/// bridge, GetGardenRank returns the wrapped batch shape, Post always
/// succeeds, and the replay page is a static HTML shell. It doubles as the
/// LLM: POST /chat/completions gets VERDICT_SSE. Every RPC body is pushed
/// into `recorded` for assertions. Auth is ignored — the stub is not the
/// thing under test.
fn spawn_stub_slug(port: u16, pair_json: String, recorded: Arc<Mutex<Vec<String>>>) {
    thread::spawn(move || {
        let server = tiny_http::Server::http(&format!("127.0.0.1:{port}"))
            .expect("stub slug bind");
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
                recorded.lock().expect("recorded").push(body.clone());
                let canned = if body.contains("GetPair") {
                    &pair_json
                } else if body.contains("GetGardenRank") {
                    &rank_json
                } else {
                    post_json
                };
                respond(req, 200, "application/json", canned.as_bytes());
            } else if *req.method() == tiny_http::Method::Post && url.starts_with("/chat/completions") {
                respond(req, 200, "text/event-stream", VERDICT_SSE.as_bytes());
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
/// slug, no tokens, no real LLM (the stub doubles as the model when a test
/// passes an LlmCfg pointing at it). The fs/git effects read a REAL
/// fixture repo — the suite stubs slug and the model, never the filesystem.
fn guardian_source(slug_port: u16) -> String {
    let src = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("hop/guardian.hop"),
    )
    .expect("read guardian.hop");
    src
        .replace(
            "server let slug_url = \"http://localhost:8080\";",
            &format!("server let slug_url = \"http://127.0.0.1:{slug_port}\";"),
        )
        .replace(
            "server let slug_token = \"\";",
            "server let slug_token = \"stub-token\";",
        )
}

/// A real git repo in a temp dir: each entry a tracked file of N lines, so
/// the fs/git effects do actual disk and git work exactly as in production.
fn fixture_repo(files: &[(&str, usize)]) -> PathBuf {
    let dir = tempfile::tempdir().expect("repo tmpdir");
    let root = dir.path().to_path_buf();
    std::mem::forget(dir);
    for (rel, lines) in files {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().expect("parent")).expect("mkdir");
        std::fs::write(&p, "//! pad\n".repeat(*lines)).expect("write");
    }
    let git = |args: &[&str]| {
        let st = std::process::Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(args)
            .status()
            .expect("git spawn");
        assert!(st.success(), "git {args:?} failed");
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "fixture"]);
    root
}

fn spawn_hopd(
    src: String,
    http: u16,
    ws: u16,
    fs_root: PathBuf,
    llm: Option<hoprt::serve::LlmCfg>,
    trace: Option<PathBuf>,
) {
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
            Some(fs_root),
            llm,
            Vec::new(),
            trace,
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
    spawn_stub_slug(
        STUB_SLUG,
        format!(
            r#"{{"results":[{{"ok":true,"result":{{"Pair":{{
            "left":"{SLUG_ITEM}server/src/scope_rank.rs",
            "right":"{SLUG_ITEM}server/src/reducer.rs",
            "left_body":"stub body","right_body":"stub body","threads":[]
        }}}}}}]}}"#
        ),
        Arc::new(Mutex::new(Vec::new())),
    );
    // the scanner reads a REAL git repo through the fs/git effects: four
    // tracked files over 400 lines, including the two the stub's GetPair
    // will suggest — no top-N cut, no fixture JSON, no sidecar
    let repo = fixture_repo(&[
        ("server/src/dsl.rs", 450),
        ("server/src/api/rpc.rs", 450),
        ("server/src/scope_rank.rs", 450),
        ("server/src/reducer.rs", 450),
    ]);
    spawn_hopd(guardian_source(STUB_SLUG), HOP_HTTP, HOP_WS, repo, None, None);

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

    // regression 3: GetPair's bridge suggestion drives the vote screen
    // DIRECTLY — scope_rank.rs × reducer.rs are scanned locals, the stub
    // offers exactly that pair, and the cards must show slug's choice,
    // not some locally invented pairing
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

// regression 5: the shell slit, sewn shut for good. GetPair can suggest
// any member — including a URL item whose repo path carries a shell
// payload like `x/';touch${IFS}/tmp/guardian_pwned;'` (no literal space or
// colon; ${IFS} expands at shell time). The old guardian shelled out with
// text cut from that URL; the new one has NO shell in the judging loop at
// all — files come through hopd's rooted fs effect, and a suggestion no
// scanned file matches is foreign: refused LOUDLY in the feed, and never
// rendered as a pair.
const STUB_POISON: u16 = 19742;
const HOP_POISON_HTTP: u16 = 19752;
const HOP_POISON_WS: u16 = 19753;

#[tokio::test]
async fn guardian_refuses_poisoned_pair() {
    spawn_stub_slug(
        STUB_POISON,
        format!(
            // {{ and ${{IFS}} are format escapes; the wire sees ${IFS}
            r#"{{"results":[{{"ok":true,"result":{{"Pair":{{
                "left":"{SLUG_ITEM}x/';touch${{IFS}}/tmp/guardian_pwned;'",
                "right":"{SLUG_ITEM}server/src/reducer.rs",
                "left_body":"stub body","right_body":"stub body","threads":[]
            }}}}}}]}}"#
        ),
        Arc::new(Mutex::new(Vec::new())),
    );
    // a real repo with real scanned files — the poisoned suggestion simply
    // matches none of them
    let repo = fixture_repo(&[
        ("server/src/dsl.rs", 450),
        ("server/src/api/rpc.rs", 450),
    ]);
    spawn_hopd(
        guardian_source(STUB_POISON),
        HOP_POISON_HTTP,
        HOP_POISON_WS,
        repo,
        None,
        None,
    );

    let pw = Playwright::launch().await.expect("playwright");
    let browser = pw.chromium().launch().await.expect("chromium");
    let page = browser.new_page().await.expect("page");
    page.goto(
        &format!("http://localhost:{HOP_POISON_HTTP}/"),
        GotoOptions::new().wait_until(WaitUntil::DomContentLoaded),
    )
    .await
    .expect("goto");
    poll_js(&page, r#"document.body.innerText.includes("vote screen") ? "ok" : null"#, 10).await;
    click_button(&page, "scan repo").await;

    // paint asks slug for a pair, slug offers the poisoned item, and the
    // foreign suggestion is refused LOUDLY — the note lands in the feed on
    // the scan's paint
    let refused = poll_js(
        &page,
        r#"document.body.innerText.includes("refused unknown slug-suggested item") ? "ok" : null"#,
        10,
    )
    .await;
    assert_eq!(
        refused, "ok",
        "poisoned pair was not refused loudly in the feed"
    );

    // and the foreign pair never renders: with no fallback and no shell in
    // the loop, both cards stay empty — nothing to judge, nothing to run
    let cards = poll_js(
        &page,
        r#"(() => {
            const cards = Array.from(document.querySelectorAll(".pair-card"))
                .map(c => c.textContent);
            return cards.length === 2 && cards.every(c => c.includes("…")) ? "ok" : null;
        })()"#,
        10,
    )
    .await;
    assert_eq!(cards, "ok", "foreign pair leaked onto the vote screen");
    browser.close().await.ok();
}

// regression 6: the play button's whole loop, proven end to end. A tick
// asks slug for a pair (scope_rank × reducer), fs.read pulls both bodies
// from the fixture repo, the stub LLM streams a verdict over SSE, the
// guardian parses it and POSTs the 2:1 vote back to slug, and the feed
// shows the judgment. Everything is real except the two sockets the suite
// stubs: slug's RPC and the model's SSE — both served by one tiny_http
// stub, with every RPC body recorded for assertion.
const STUB_PLAY: u16 = 19744;
const HOP_PLAY_HTTP: u16 = 19754;
const HOP_PLAY_WS: u16 = 19755;

#[tokio::test]
async fn guardian_judges_end_to_end() {
    let recorded = Arc::new(Mutex::new(Vec::new()));
    spawn_stub_slug(
        STUB_PLAY,
        format!(
            r#"{{"results":[{{"ok":true,"result":{{"Pair":{{
                "left":"{SLUG_ITEM}server/src/scope_rank.rs",
                "right":"{SLUG_ITEM}server/src/reducer.rs",
                "left_body":"stub body","right_body":"stub body","threads":[]
            }}}}}}]}}"#
        ),
        recorded.clone(),
    );
    let repo = fixture_repo(&[
        ("server/src/dsl.rs", 450),
        ("server/src/api/rpc.rs", 450),
        ("server/src/scope_rank.rs", 450),
        ("server/src/reducer.rs", 450),
    ]);
    spawn_hopd(
        guardian_source(STUB_PLAY),
        HOP_PLAY_HTTP,
        HOP_PLAY_WS,
        repo,
        Some(hoprt::serve::LlmCfg {
            key: Some("stub".into()),
            base: format!("http://127.0.0.1:{STUB_PLAY}"),
            model: Some("stub/model".into()),
        }),
        None,
    );

    let pw = Playwright::launch().await.expect("playwright");
    let browser = pw.chromium().launch().await.expect("chromium");
    let page = browser.new_page().await.expect("page");
    page.goto(
        &format!("http://localhost:{HOP_PLAY_HTTP}/"),
        GotoOptions::new().wait_until(WaitUntil::DomContentLoaded),
    )
    .await
    .expect("goto");
    poll_js(&page, r#"document.body.innerText.includes("vote screen") ? "ok" : null"#, 10).await;

    // scan the fixture repo, then wait for the suggested pair to land on
    // the vote screen before pressing play
    click_button(&page, "scan repo").await;
    let pair = poll_js(
        &page,
        r#"(() => {
            const cards = Array.from(document.querySelectorAll(".pair-card"))
                .map(c => c.textContent);
            return cards.some(c => c.includes("server/src/scope_rank.rs")) ? "ok" : null;
        })()"#,
        10,
    )
    .await;
    assert_eq!(pair, "ok", "suggested pair never rendered");

    // PLAY: the judging loop runs — the stub verdict always picks a
    click_button(&page, "play").await;
    let judged = poll_js(
        &page,
        r#"document.body.innerText.includes("guardian: server/src/scope_rank.rs > server/src/reducer.rs") ? "ok" : null"#,
        20,
    )
    .await;
    assert_eq!(
        judged, "ok",
        "no judgment landed in the feed after play"
    );
    click_button(&page, "pause").await;

    // the invoice folded itself onto the status line: at least the stub's
    // 1234 prompt / 56 completion tokens and one call, straight off the
    // tape — billing as a reducer projection, no payment processor
    let billing = poll_js(
        &page,
        r#"(() => {
            const m = document.body.innerText.match(/(\d+) in \/ (\d+) out · (\d+) calls/);
            return m && parseInt(m[1]) >= 1234 && parseInt(m[2]) >= 56 && parseInt(m[3]) >= 1 ? "ok" : null;
        })()"#,
        10,
    )
    .await;
    assert_eq!(billing, "ok", "billing projection missing from the status line");

    // the vote crossed the wire to slug: thread judgments, ratio 2:1,
    // the model's why attached
    let bodies = recorded.lock().expect("recorded");
    assert!(
        bodies
            .iter()
            .any(|b| b.contains("judgments") && b.contains("2:1") && b.contains("stub says a")),
        "no guardian vote recorded at slug: {bodies:?}"
    );
    drop(bodies);

    // and the verbatim prompt was recorded — the exact text the model saw
    click_button(&page, "⤢ prompt").await;
    let prompt = poll_js(
        &page,
        r#"document.body.innerText.includes("code-health guardian") ? "ok" : null"#,
        10,
    )
    .await;
    assert_eq!(prompt, "ok", "verbatim prompt view is empty after a judgment");
    browser.close().await.ok();
}

// The indexer SaaS end to end: paste a repo + an OpenRouter key → real
// git.clone of a local fixture repo → real fs scan → members registered
// with slug (stub records it) → auto-play → the stub LLM streams a verdict
// → the vote lands in slug → the invoice folds onto the status line. The
// user's key never touches the tape (it rides a process global).
const STUB_SAAS: u16 = 19746;
const HOP_SAAS_HTTP: u16 = 19756;
const HOP_SAAS_WS: u16 = 19757;

/// indexer.hop re-pointed at the stub, same trick as guardian_source.
fn indexer_source(slug_port: u16) -> String {
    let src = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("hop/indexer.hop"),
    )
    .expect("read indexer.hop");
    src.replace(
        "server let slug_url = \"http://localhost:8080\";",
        &format!("server let slug_url = \"http://127.0.0.1:{slug_port}\";"),
    )
    .replace(
        "server let slug_token = \"\";",
        "server let slug_token = \"stub-token\";",
    )
}

#[tokio::test]
async fn saas_indexes_and_judges_a_repo() {
    // the repo under test: a real git repo on disk (cloned for real)
    let origin = fixture_repo(&[
        ("server/src/dsl.rs", 450),
        ("server/src/api/rpc.rs", 450),
        ("server/src/scope_rank.rs", 450),
        ("server/src/reducer.rs", 450),
    ]);
    let origin_url = origin.to_string_lossy().to_string();
    // the stub offers the bridge pair using the SaaS's OWN member urls
    // (the clone registers files as {origin}/blob/HEAD/<path>)
    let member = format!("{origin_url}/blob/HEAD/server/src/");
    let recorded = Arc::new(Mutex::new(Vec::new()));
    spawn_stub_slug(
        STUB_SAAS,
        format!(
            r#"{{"results":[{{"ok":true,"result":{{"Pair":{{
                "left":"{member}scope_rank.rs",
                "right":"{member}reducer.rs",
                "left_body":"stub body","right_body":"stub body","threads":[]
            }}}}}}]}}"#
        ),
        recorded.clone(),
    );
    let ws = tempfile::tempdir().expect("workspaces");
    let ws_root = ws.path().to_path_buf();
    std::mem::forget(ws);
    // the lantern: this run traces every assign, hop, and tape event to a
    // file — the SaaS test doubles as the trace smoke test
    let trace_dir = tempfile::tempdir().expect("trace dir");
    let trace_path = trace_dir.path().join("exec.trace");
    std::mem::forget(trace_dir);
    spawn_hopd(
        indexer_source(STUB_SAAS),
        HOP_SAAS_HTTP,
        HOP_SAAS_WS,
        ws_root,
        Some(hoprt::serve::LlmCfg {
            key: Some("stub".into()),
            base: format!("http://127.0.0.1:{STUB_SAAS}"),
            model: Some("stub/model".into()),
        }),
        Some(trace_path.clone()),
    );

    let pw = Playwright::launch().await.expect("playwright");
    let browser = pw.chromium().launch().await.expect("chromium");
    let page = browser.new_page().await.expect("page");
    page.goto(
        &format!("http://localhost:{HOP_SAAS_HTTP}/"),
        GotoOptions::new().wait_until(WaitUntil::DomContentLoaded),
    )
    .await
    .expect("goto");
    poll_js(&page, r#"document.body.innerText.includes("indexer") ? "ok" : null"#, 10).await;

    // paste repo + key, hit the button — the whole self-serve loop fires
    page.evaluate_value(&format!(
        r##"document.querySelector("#rurl").value = {origin_url:?};
           document.querySelector("#rkey").value = "user-secret-key";"##
    ))
    .await
    .expect("fill form");
    click_button(&page, "index my repo").await;

    // clone + scan + registration + auto-play: a judgment lands
    let judged = poll_js(
        &page,
        r#"document.body.innerText.includes("agent: server/src/scope_rank.rs > server/src/reducer.rs") ? "ok" : null"#,
        30,
    )
    .await;
    if judged != "ok" {
        let body = page
            .evaluate_value("document.body.innerText")
            .await
            .unwrap_or_default();
        let tail = &body[body.len().saturating_sub(900)..];
        panic!("no judgment landed after indexing; page tail:\n{tail}");
    }

    // the invoice folded onto the status line from the user's own key's meter
    let billing = poll_js(
        &page,
        r#"(() => {
            const m = document.body.innerText.match(/(\d+) in \/ (\d+) out · (\d+) calls/);
            return m && parseInt(m[1]) >= 1234 && parseInt(m[3]) >= 1 ? "ok" : null;
        })()"#,
        10,
    )
    .await;
    assert_eq!(billing, "ok", "billing line missing after the run");

    // and slug saw the repo: registration + a 2:1 vote, both recorded
    let bodies = recorded.lock().expect("recorded");
    assert!(
        bodies.iter().any(|b| b.contains("\"files\"") && b.contains("scope_rank.rs")),
        "no member registration reached slug: {bodies:?}"
    );
    assert!(
        bodies
            .iter()
            .any(|b| b.contains("judgments") && b.contains("2:1") && b.contains("stub says a")),
        "no vote reached slug: {bodies:?}"
    );
    drop(bodies);

    // the lantern: assigns, hops, and tape events all traced, greppable
    let trace = std::fs::read_to_string(&trace_path).expect("trace file");
    assert!(trace.contains("assign "), "trace has no assigns:\n{trace}");
    assert!(
        // a browser→server mark arrives as the incoming call, ship set
        // attached (the wasm tab's own side of the hop isn't traced — the
        // lantern lives in hopd)
        trace.contains("recv call start_index:1 vars={"),
        "trace has no hop arrival with its ship set:\n{trace}"
    );
    assert!(
        // tape keys serialize alphabetically; "judged" is the event type
        trace.contains("\"type\":\"judged\""),
        "trace has no judged tape event:\n{trace}"
    );
    browser.close().await.ok();
}