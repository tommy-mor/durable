//! Real-browser boot: hopd + chromium, via playwright-rs.
//!
//! The harness cannot prove this: the wasm tab must receive hello and the
//! first-render cast, and `#app` must not stay empty. Two bugs hid here:
//!
//! 1. A localhost handshake that finished during `new BrowserVm(src)`
//!    dropped those frames (status "connected", blank page).
//! 2. glue awaited `import('/idiomorph.esm.js')` before opening the
//!    socket, while `/boot.css` held its HTTP/1.1 connection until that
//!    socket connected — a deadlock that looked like "idiomorph pending"
//!    for the boot-css timeout (and longer on `localhost` → IPv6).

use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use playwright_rs::{expect, Browser, GotoOptions, Page, Playwright, WaitUntil};

const TODO_HTTP: u16 = 19710;
const TODO_WS: u16 = 19711;
const AGENT_HTTP: u16 = 19712;
const AGENT_WS: u16 = 19713;
const EMBER_HTTP: u16 = 19714;
const EMBER_WS: u16 = 19715;
const ERR_HTTP: u16 = 19716;
const ERR_WS: u16 = 19717;

fn pkg_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../hop-web/pkg")
}

fn spawn_hopd(src: impl Into<String>, http: u16, ws: u16) {
    let src = src.into();
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

async fn launch_chromium() -> (Playwright, Browser) {
    let try_launch = || async {
        let pw = Playwright::launch().await?;
        let browser = pw.chromium().launch().await?;
        playwright_rs::Result::Ok((pw, browser))
    };
    match try_launch().await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("chromium launch failed ({e}); installing browsers");
            playwright_rs::install_browsers(Some(&["chromium"]))
                .await
                .expect("install chromium");
            try_launch().await.expect("chromium after install")
        }
    }
}

async fn dump(page: &Page) -> String {
    page.evaluate_value(
        "JSON.stringify({
            status: document.getElementById('status') && document.getElementById('status').textContent,
            app: document.getElementById('app') && document.getElementById('app').innerHTML.slice(0, 800)
        })",
    )
    .await
    .unwrap_or_else(|e| format!("evaluate failed: {e}"))
}

/// Resource-timing duration for a URL substring, or -1 if not recorded.
async fn resource_ms(page: &Page, needle: &str) -> f64 {
    let expr = format!(
        "(() => {{
            const e = performance.getEntriesByType('resource')
                .find(r => r.name.indexOf('{needle}') >= 0);
            return e ? String(e.duration) : '-1';
        }})()"
    );
    page.evaluate_value(&expr)
        .await
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(-1.0)
}

async fn assert_fast_first_paint(page: &Page, http: u16, needle: &str) {
    let t0 = Instant::now();
    // localhost, not 127.0.0.1: that is the hostname that prefers IPv6
    // and used to hang for ~20s against an IPv4-only hopd.
    page.goto(
        &format!("http://localhost:{http}/"),
        GotoOptions::new().wait_until(WaitUntil::DomContentLoaded),
    )
    .await
    .expect("goto");

    let app = page.locator("#app");
    if let Err(e) = expect(app.clone())
        .with_timeout(Duration::from_secs(8))
        .to_contain_text(needle)
        .await
    {
        panic!(
            "first paint failed: {e}\npage: {}\nconfig_ms={} boot_css_ms={}",
            dump(page).await,
            resource_ms(page, "config.json").await,
            resource_ms(page, "boot.css").await
        );
    }

    let elapsed = t0.elapsed();
    assert!(
        elapsed < Duration::from_secs(5),
        "boot took {elapsed:?} (held HTTP request / IPv6 hang)\npage: {}\nconfig_ms={} boot_css_ms={}",
        dump(page).await,
        resource_ms(page, "config.json").await,
        resource_ms(page, "boot.css").await
    );

    let status = page
        .locator("#status")
        .text_content()
        .await
        .ok()
        .flatten()
        .unwrap_or_default();
    assert!(
        !status.contains("loading hop"),
        "still on 'loading hop' after paint — idiomorph is still on the critical path\n{status:?}"
    );

    for name in ["config.json", "boot.css", "idiomorph"] {
        let ms = resource_ms(page, name).await;
        if ms > 500.0 {
            panic!("{name} took {ms:.0}ms — hopd is holding a boot asset again");
        }
    }
}

#[tokio::test]
async fn todo_first_paint_is_not_blank() {
    spawn_hopd(include_str!("../hop/todo.hop"), TODO_HTTP, TODO_WS);
    let (_pw, browser) = launch_chromium().await;
    let page = browser.new_page().await.expect("page");
    assert_fast_first_paint(&page, TODO_HTTP, "todos").await;
    browser.close().await.ok();
}

#[tokio::test]
async fn agent_first_paint_is_not_blank() {
    spawn_hopd(include_str!("../hop/agent.hop"), AGENT_HTTP, AGENT_WS);
    let (_pw, browser) = launch_chromium().await;
    let page = browser.new_page().await.expect("page");
    assert_fast_first_paint(&page, AGENT_HTTP, "agent").await;
    browser.close().await.ok();
}

#[tokio::test]
async fn ember_first_paint_is_not_blank() {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../ember2/ember.hop");
    let src = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!("need {} for this test: {e}", path.display())
    });
    spawn_hopd(src, EMBER_HTTP, EMBER_WS);
    let (_pw, browser) = launch_chromium().await;
    let page = browser.new_page().await.expect("page");
    assert_fast_first_paint(&page, EMBER_HTTP, "A being, in time.").await;
    if let Err(e) = expect(page.locator("#app"))
        .to_contain_text("newest first")
        .await
    {
        panic!("pager missing: {e}\npage: {}", dump(&page).await);
    }
    if let Err(e) = expect(page.locator("#draft"))
        .with_timeout(Duration::from_secs(3))
        .to_be_visible()
        .await
    {
        panic!("compose box missing: {e}\npage: {}", dump(&page).await);
    }
    if let Err(e) = expect(page.locator("#budget"))
        .with_timeout(Duration::from_secs(3))
        .to_be_visible()
        .await
    {
        panic!("compact budget missing: {e}\npage: {}", dump(&page).await);
    }
    if let Err(e) = expect(page.locator("#compact"))
        .to_contain_text("compact")
        .await
    {
        panic!("compact button missing: {e}\npage: {}", dump(&page).await);
    }
    browser.close().await.ok();
}

#[tokio::test]
async fn unhandled_error_is_visible_in_the_page() {
    spawn_hopd(
        r##"
fn boom() {
  server!();
  error("test-error-surface");
}
fn on_connect(sid) {
  cast session(sid) {
    hui.render("#app", [:button, { id = "boom", onclick = fn(e) { boom(); } }, "boom"]);
  }
}
"##,
        ERR_HTTP,
        ERR_WS,
    );
    let (_pw, browser) = launch_chromium().await;
    let page = browser.new_page().await.expect("page");
    assert_fast_first_paint(&page, ERR_HTTP, "boom").await;
    page.locator("#boom").click(None).await.expect("click boom");
    if let Err(e) = expect(page.locator("#hop-err"))
        .with_timeout(Duration::from_secs(5))
        .to_contain_text("test-error-surface")
        .await
    {
        panic!("error not surfaced: {e}\npage: {}", dump(&page).await);
    }
    browser.close().await.ok();
}
