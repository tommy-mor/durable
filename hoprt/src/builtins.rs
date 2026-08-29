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
//! calls back by id (`__hopHandler(id, event)` — the DOM event rides
//! along, so keyboard handlers can inspect `e.key`). Handler ids are
//! minted per render of a root, released when that root re-renders,
//! never reused. Handlers run as flows — a handler body may hop.

use crate::rt::HuiState;
use crate::value::{coerce_str, NativeId, Value};

/// The store module: fields resolved on the callable `store` native.
/// Shape constructors, the tape, view sugar, and the path vocabulary —
/// collecting navigators (`#nav`) and terminal navigators (`#term`).
pub fn store_field(name: &str) -> Option<Value> {
    let shape_const = |k: &str| {
        Value::map(
            [(Value::str("k"), Value::str(k))].into_iter().collect(),
        )
    };
    Some(match name {
        // schema shapes
        "leaf" => shape_const("leaf"),
        "sum" => shape_const("sum"),
        "map" => Value::Native(NativeId::ShapeMap),
        "list" => Value::Native(NativeId::ShapeList),
        "deque" => Value::Native(NativeId::ShapeDeque),
        "record" => Value::Native(NativeId::ShapeRecord),
        // the tape, view sugar, replay check
        "append" => Value::Native(NativeId::StoreAppend),
        "items" => Value::Native(NativeId::StoreItems),
        "verify" => Value::Native(NativeId::StoreVerify),
        // collecting navigators: a path containing one is a select
        "all" | "keys" | "vals" | "first" | "last" | "entries" => {
            Value::tagged("nav", Value::str(name))
        }
        // parametrized collecting navigators
        "where" => Value::Native(NativeId::NavWhere),
        "slice" => Value::Native(NativeId::NavSlice),
        // terminal navigators: a path ending in one is a mutation
        "set" => Value::Native(NativeId::NavSet),
        "add" => Value::Native(NativeId::NavAdd),
        "push" => Value::Native(NativeId::NavPush),
        "del" => Value::tagged("term", Value::array(vec![Value::str("del")])),
        _ => return None,
    })
}

/// markdown → hiccup. The tree goes through render_node like any
/// hand-written UI: every text run is escaped there, so this is the safe
/// way to show model output. Raw HTML in the source is demoted to text.
pub fn markdown_hiccup(src: &str) -> Value {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};

    struct Node {
        tag: &'static str,
        attrs: Vec<(Value, Value)>,
        children: Vec<Value>,
    }
    fn seal(n: Node) -> Value {
        let mut items = vec![Value::str(n.tag), Value::map(n.attrs.into_iter().collect())];
        items.extend(n.children);
        Value::array(items)
    }

    let mut stack = vec![Node {
        tag: "div",
        attrs: vec![(Value::str("class"), Value::str("md"))],
        children: Vec::new(),
    }];
    let push_leaf = |stack: &mut Vec<Node>, v: Value| {
        stack.last_mut().expect("root never pops").children.push(v);
    };

    let opts = Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TABLES;
    for ev in Parser::new_ext(src, opts) {
        match ev {
            Event::Start(tag) => {
                let (t, attrs): (&'static str, Vec<(Value, Value)>) = match tag {
                    Tag::Paragraph => ("p", vec![]),
                    Tag::Heading { level, .. } => (
                        match level as usize {
                            1 => "h1",
                            2 => "h2",
                            3 => "h3",
                            4 => "h4",
                            5 => "h5",
                            _ => "h6",
                        },
                        vec![],
                    ),
                    Tag::BlockQuote(_) => ("blockquote", vec![]),
                    Tag::CodeBlock(kind) => {
                        let attrs = match kind {
                            CodeBlockKind::Fenced(lang) if !lang.is_empty() => {
                                vec![(Value::str("class"), Value::str(format!("lang-{lang}")))]
                            }
                            _ => vec![],
                        };
                        ("pre", attrs)
                    }
                    Tag::List(Some(_)) => ("ol", vec![]),
                    Tag::List(None) => ("ul", vec![]),
                    Tag::Item => ("li", vec![]),
                    Tag::Emphasis => ("em", vec![]),
                    Tag::Strong => ("strong", vec![]),
                    Tag::Strikethrough => ("s", vec![]),
                    Tag::Link { dest_url, .. } => (
                        "a",
                        vec![(Value::str("href"), Value::str(dest_url.as_ref()))],
                    ),
                    // images as links: the transcript stays text-only
                    Tag::Image { dest_url, .. } => (
                        "a",
                        vec![(Value::str("href"), Value::str(dest_url.as_ref()))],
                    ),
                    Tag::Table(_) => ("table", vec![]),
                    Tag::TableHead => ("tr", vec![]),
                    Tag::TableRow => ("tr", vec![]),
                    Tag::TableCell => ("td", vec![]),
                    _ => ("span", vec![]),
                };
                stack.push(Node { tag: t, attrs, children: Vec::new() });
            }
            Event::End(_) => {
                let done = stack.pop().expect("balanced events");
                push_leaf(&mut stack, seal(done));
            }
            Event::Text(t) => push_leaf(&mut stack, Value::str(t.as_ref())),
            Event::Code(t) => push_leaf(
                &mut stack,
                Value::array(vec![Value::str("code"), Value::map(Default::default()), Value::str(t.as_ref())]),
            ),
            // raw HTML is shown, not interpreted
            Event::Html(t) | Event::InlineHtml(t) => push_leaf(&mut stack, Value::str(t.as_ref())),
            Event::SoftBreak => push_leaf(&mut stack, Value::str(" ")),
            Event::HardBreak => push_leaf(
                &mut stack,
                Value::array(vec![Value::str("br"), Value::map(Default::default())]),
            ),
            Event::Rule => push_leaf(
                &mut stack,
                Value::array(vec![Value::str("hr"), Value::map(Default::default())]),
            ),
            _ => {}
        }
    }
    // unbalanced input can't happen (pulldown guarantees it), but a
    // truncated iteration would just seal into the root anyway
    while stack.len() > 1 {
        let done = stack.pop().expect("len > 1");
        push_leaf(&mut stack, seal(done));
    }
    seal(stack.pop().expect("root"))
}

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
                            attrs.push_str(&format!(" {name}=\"__hopHandler({id}, event)\""));
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
