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

#[wasm_bindgen]
extern "C" {
    /// glue.js: idiomorph merge into the live tree, innerHTML fallback.
    #[wasm_bindgen(js_name = __hopMorph)]
    fn hop_morph(sel: &str, html: &str);
    /// glue.js: unhandled flow errors land here so they are visible
    /// in the page, not only as a binary WS frame / console line.
    #[wasm_bindgen(js_name = __hopError)]
    fn hop_error(msg: &str);
}

/// The DOM-backed platform. `dom.get` reads an input's value, `dom.set`
/// morphs new markup into the element (which is where hui renders land —
/// idiomorph keeps focus, scroll, and in-progress typing alive across
/// re-renders), `dom.clear` empties an input. There is no store here.
struct WebPlatform<'a> {
    send: &'a js_sys::Function,
}

fn document() -> Option<web_sys::Document> {
    web_sys::window().and_then(|w| w.document())
}

fn query(sel: &str) -> Option<web_sys::Element> {
    document().and_then(|d| d.query_selector(sel).ok().flatten())
}

/// DOM event → hop value: a map of the fields handlers care about.
/// Absent fields (a click has no `key`) are simply omitted.
fn event_value(ev: &JsValue) -> Value {
    if ev.is_undefined() || ev.is_null() {
        return Value::Nil;
    }
    let mut entries = std::collections::BTreeMap::new();
    if let Ok(k) = js_sys::Reflect::get(ev, &JsValue::from_str("key")) {
        if let Some(s) = k.as_string() {
            entries.insert(Value::str("key"), Value::str(s));
        }
    }
    Value::map(entries)
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
        if line.starts_with("!!") {
            hop_error(&line);
            web_sys::console::error_1(&line.clone().into());
        } else {
            web_sys::console::log_1(&line.into());
        }
    }

    fn dom_get(&mut self, sel: &str) -> String {
        let Some(el) = query(sel) else { return String::new() };
        if let Some(input) = el.dyn_ref::<web_sys::HtmlInputElement>() {
            input.value()
        } else if let Some(ta) = el.dyn_ref::<web_sys::HtmlTextAreaElement>() {
            ta.value()
        } else if let Some(select) = el.dyn_ref::<web_sys::HtmlSelectElement>() {
            select.value()
        } else {
            el.text_content().unwrap_or_default()
        }
    }

    fn dom_set(&mut self, sel: &str, html: &str) {
        hop_morph(sel, html);
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

    fn dom_focus(&mut self, sel: &str) {
        let Some(el) = query(sel) else { return };
        if let Some(html) = el.dyn_ref::<web_sys::HtmlElement>() {
            let _ = html.focus();
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
    /// Frames that arrived before the hello. A localhost handshake can
    /// deliver the first-render cast in the same burst; dropping it
    /// leaves the tab on "connected" with an empty #app.
    pending: Vec<Value>,
}

#[wasm_bindgen]
impl BrowserVm {
    /// Compile the `.hop` source (the same file the server compiled — the
    /// wire carries hop ids, so both sides must hold the same program) and
    /// wait for the hello.
    #[wasm_bindgen(constructor)]
    pub fn new(src: &str, send: js_sys::Function) -> Result<BrowserVm, JsValue> {
        let prog = compiler::compile(src).map_err(|e| JsValue::from_str(&e))?;
        Ok(BrowserVm {
            prog: Rc::new(prog),
            vm: None,
            send,
            pending: Vec::new(),
        })
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
        if pkt.get_field("kind").as_str() == Some("hello") {
            let sid = pkt.get_field("session");
            let sid = sid.as_str().unwrap_or("?").to_string();
            let mut platform = WebPlatform { send: &self.send };
            match Vm::new(self.prog.clone(), SideId::Browser(sid), &mut platform) {
                Ok(mut vm) => {
                    // this tab's durable identity, minted by hopd's cookie
                    vm.user = pkt.get_field("user");
                    self.vm = Some(vm);
                }
                Err(e) => web_sys::console::error_1(&format!("vm boot: {e}").into()),
            }
            let queued = std::mem::take(&mut self.pending);
            if let Some(vm) = &mut self.vm {
                let mut platform = WebPlatform { send: &self.send };
                for p in queued {
                    vm.receive(&mut platform, p);
                }
            }
            return;
        }
        if let Some(vm) = &mut self.vm {
            let mut platform = WebPlatform { send: &self.send };
            vm.receive(&mut platform, pkt);
        } else {
            web_sys::console::log_1(&"hop: queued packet before hello".into());
            self.pending.push(pkt);
        }
    }

    /// An event on hui-rendered HTML (`__hopHandler(id, event)`). The DOM
    /// event crosses as a small map value — just `key` for now, which is
    /// what keyboard handlers need.
    pub fn fire_handler(&mut self, id: f64, ev: JsValue) {
        if let Some(vm) = &mut self.vm {
            let mut platform = WebPlatform { send: &self.send };
            vm.fire_handler(&mut platform, id as i64, event_value(&ev));
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
