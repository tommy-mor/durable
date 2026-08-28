//! Native hui — the hiccup renderer (the Rust port of the old Lua hui).
//!
//! A node is data: `[:li, { class = "done", onclick = fn }, "buy milk"]`
//!
//! - array whose first element is a string: an element — tag, an optional
//!   attrs *map* (unambiguous now that arrays and maps are distinct
//!   kinds), then children
//! - any other array: a fragment — children spliced in place, which is
//!   what a list built with push() renders as
//! - strings and numbers: escaped text; nil renders as nothing
//!
//! Closure-valued attributes are event handlers. They are not serialized:
//! the closure goes into this VM's handler table and the rendered HTML
//! calls back by id (`__hopHandler(id)`). Handler ids are minted per
//! render of a root, released when that root re-renders, never reused.
//! Handlers run as flows — a handler body may hop.

use crate::rt::HuiState;
use crate::value::{coerce_str, Value};

fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub fn hui_render(state: &mut HuiState, sel: &str, node: &Value) -> Result<String, String> {
    state.release_root(sel);
    render_node(state, sel, node)
}

fn render_node(state: &mut HuiState, sel: &str, node: &Value) -> Result<String, String> {
    match node {
        Value::Nil => Ok(String::new()),
        Value::Str(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_) => {
            Ok(esc(&coerce_str(node)))
        }
        Value::Array(items) => {
            let items = items.borrow();
            let is_element = matches!(items.first(), Some(Value::Str(_)));
            if !is_element {
                // fragment
                let mut html = String::new();
                for child in items.iter() {
                    html.push_str(&render_node(state, sel, child)?);
                }
                return Ok(html);
            }

            let tag = match &items[0] {
                Value::Str(s) => s.to_string(),
                _ => unreachable!(),
            };
            let mut attrs = String::new();
            let mut first_child = 1;
            if let Some(Value::Map(m)) = items.get(1) {
                first_child = 2;
                // map iteration is key-ordered: same tree, same HTML
                for (k, v) in m.borrow().iter() {
                    let name = match k {
                        Value::Str(s) => s.to_string(),
                        other => return Err(format!("attr name must be a string, got {}", other.kind())),
                    };
                    match v {
                        Value::Closure(_) => {
                            let id = state.register(sel, v.clone());
                            attrs.push_str(&format!(" {name}=\"__hopHandler({id})\""));
                        }
                        Value::Bool(true) => attrs.push_str(&format!(" {name}")),
                        Value::Bool(false) | Value::Nil => {}
                        other => attrs.push_str(&format!(" {name}=\"{}\"", esc(&coerce_str(other)))),
                    }
                }
            }

            let mut html = format!("<{tag}{attrs}>");
            for child in items.iter().skip(first_child) {
                html.push_str(&render_node(state, sel, child)?);
            }
            html.push_str(&format!("</{tag}>"));
            Ok(html)
        }
        other => Err(format!("cannot render a {} as UI", other.kind())),
    }
}
