//! hop-web — the Hop browser backend: the same interpreter that runs on
//! the server, compiled to wasm32, with the DOM as its platform.
//!
//! glue.js is a dumb pipe: it opens the WebSocket, hands every binary
//! frame to [`BrowserVm::receive`], and sends whatever the VM's platform
//! emits. All protocol knowledge lives here (and in hoprt): the first
//! frame is the `hello` carrying the session id — the VM is constructed
//! at that moment, and any `on_connect` snapshot cast follows through the
//! same receive path.
//!
//! Rendered hui HTML calls back through `window.__hopHandler(id)`, which
//! glue.js routes to [`BrowserVm::fire_handler`] — closures never left
//! the VM; the ids are minted per render.

use std::rc::Rc;

use hoprt::compiler;
use hoprt::ir::Program;
use hoprt::rt::{Platform, SideId, Vm};
use hoprt::value::{decode, encode, NativeId, Value};
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;

/// The DOM-backed platform. `dom.get` reads an input's value, `dom.set`
/// replaces innerHTML (which is where hui renders land), `dom.clear`
/// empties an input. There is no store on this side.
struct WebPlatform<'a> {
    send: &'a js_sys::Function,
}

fn document() -> Option<web_sys::Document> {
    web_sys::window().and_then(|w| w.document())
}

fn query(sel: &str) -> Option<web_sys::Element> {
    document().and_then(|d| d.query_selector(sel).ok().flatten())
}

impl Platform for WebPlatform<'_> {
    fn send(&mut self, pkt: Value) {
        match encode(&pkt) {
            Ok(bytes) => {
                let arr = js_sys::Uint8Array::from(bytes.as_slice());
                let _ = self.send.call1(&JsValue::NULL, &arr);
            }
            Err(e) => web_sys::console::error_1(&format!("unencodable packet: {e}").into()),
        }
    }

    fn print(&mut self, line: String) {
        web_sys::console::log_1(&line.into());
    }

    fn dom_get(&mut self, sel: &str) -> String {
        let Some(el) = query(sel) else { return String::new() };
        if let Some(input) = el.dyn_ref::<web_sys::HtmlInputElement>() {
            input.value()
        } else if let Some(ta) = el.dyn_ref::<web_sys::HtmlTextAreaElement>() {
            ta.value()
        } else {
            el.text_content().unwrap_or_default()
        }
    }

    fn dom_set(&mut self, sel: &str, html: &str) {
        if let Some(el) = query(sel) {
            el.set_inner_html(html);
        }
    }

    fn dom_clear(&mut self, sel: &str) {
        let Some(el) = query(sel) else { return };
        if let Some(input) = el.dyn_ref::<web_sys::HtmlInputElement>() {
            input.set_value("");
        } else if let Some(ta) = el.dyn_ref::<web_sys::HtmlTextAreaElement>() {
            ta.set_value("");
        } else {
            el.set_inner_html("");
        }
    }

    fn store_native(
        &mut self,
        _id: NativeId,
        _args: Vec<Value>,
        _prog: &Rc<Program>,
    ) -> Option<Result<Value, String>> {
        None
    }
}

#[wasm_bindgen]
pub struct BrowserVm {
    prog: Rc<Program>,
    vm: Option<Vm>,
    send: js_sys::Function,
}

#[wasm_bindgen]
impl BrowserVm {
    /// Compile the `.hop` source (the same file the server compiled — the
    /// wire carries hop ids, so both sides must hold the same program) and
    /// wait for the hello.
    #[wasm_bindgen(constructor)]
    pub fn new(src: &str, send: js_sys::Function) -> Result<BrowserVm, JsValue> {
        let prog = compiler::compile(src).map_err(|e| JsValue::from_str(&e))?;
        Ok(BrowserVm { prog: Rc::new(prog), vm: None, send })
    }

    /// Every binary WebSocket frame lands here. The first is the hello
    /// that names our session and births the VM.
    pub fn receive(&mut self, bytes: &[u8]) {
        let pkt = match decode(bytes) {
            Ok(p) => p,
            Err(e) => {
                web_sys::console::error_1(&format!("undecodable packet: {e}").into());
                return;
            }
        };
        let mut platform = WebPlatform { send: &self.send };
        if pkt.get_field("kind").as_str() == Some("hello") {
            let sid = pkt.get_field("session");
            let sid = sid.as_str().unwrap_or("?").to_string();
            match Vm::new(self.prog.clone(), SideId::Browser(sid), &mut platform) {
                Ok(vm) => self.vm = Some(vm),
                Err(e) => web_sys::console::error_1(&format!("vm boot: {e}").into()),
            }
            return;
        }
        if let Some(vm) = &mut self.vm {
            vm.receive(&mut platform, pkt);
        }
    }

    /// A click on hui-rendered HTML (`__hopHandler(id)`).
    pub fn fire_handler(&mut self, id: f64) {
        if let Some(vm) = &mut self.vm {
            let mut platform = WebPlatform { send: &self.send };
            vm.fire_handler(&mut platform, id as i64);
        }
    }

    /// Fire a named entry point (dev console, tests).
    pub fn fire(&mut self, name: &str) {
        if let Some(vm) = &mut self.vm {
            let mut platform = WebPlatform { send: &self.send };
            vm.fire(&mut platform, name, Vec::new());
        }
    }
}
