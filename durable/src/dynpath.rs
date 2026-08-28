//! Untyped path algebra: the write-side counterpart of [`crate::query`].
//!
//! Typed [`crate::Path<S>`] navigators are for Rust reducers that know the
//! schema at compile time. Clients — Lua, hopc, a REPL — speak paths as data.
//! This module lowers a [`Shape`] plus a list of [`Nav`]s to the same RocksDB
//! keys the typed navigators would have produced, and emits reified [`Write`]s.
//!
//! A path does not become a mutation by itself. Terminals are explicit:
//! [`put`], [`delete`], [`add`], [`push`], [`clear`].

use ciborium::value::Integer;
use ciborium::Value;

use crate::query::{one, subtree, Nav, Query};
use crate::schema::encode_sum;
use crate::shape::field_segment;
use crate::{codec, encode_value, Db, Error, Op, Result, Shape, Write};

/// A resolved location: the lowered key prefix and the shape that lives there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub prefix: Vec<u8>,
    pub shape: Shape,
}

/// Walk `schema` along `navs` (optionally under `namespace`) to a location.
///
/// Collection combinators (`All`, `Where`, …) are rejected: a write names
/// exactly one place.
pub fn resolve(schema: &Shape, namespace: Option<&str>, navs: &[Nav]) -> Result<Location> {
    let mut prefix = match namespace {
        Some(ns) => {
            let mut p = Vec::new();
            codec::put_segment(&mut p, ns.as_bytes());
            p
        }
        None => Vec::new(),
    };
    let mut shape = schema.clone();
    for nav in navs {
        match nav {
            Nav::Field(name) => match &shape {
                Shape::Record { .. } => {
                    let (id, next) = shape.field(name)?;
                    prefix = codec::child_key(&prefix, &field_segment(id));
                    shape = next.clone();
                }
                Shape::Map { of } => {
                    let key = encode_value(&Value::Text(name.clone()))?;
                    prefix = codec::child_key(&prefix, &key);
                    shape = of.as_ref().clone();
                }
                Shape::Leaf => {
                    return Err(Error::Query(format!(
                        "write path entered a Leaf at field {name:?}; put the whole leaf"
                    )));
                }
                other => {
                    return Err(Error::Query(format!(
                        "field {name:?} on a {}",
                        other.kind_name()
                    )));
                }
            },
            Nav::Key(k) => match &shape {
                Shape::Map { of } => {
                    let key = encode_value(k)?;
                    prefix = codec::child_key(&prefix, &key);
                    shape = of.as_ref().clone();
                }
                Shape::List { of } => {
                    let i = as_u64(k).ok_or_else(|| {
                        Error::Query("integer Key on List required; use Index".into())
                    })?;
                    prefix = codec::child_key(&prefix, &codec::order_u64(i));
                    shape = of.as_ref().clone();
                }
                Shape::Deque { .. } => {
                    return Err(Error::Query(
                        "Deque writes use Index (logical) or push/clear".into(),
                    ));
                }
                other => {
                    return Err(Error::Query(format!("Key on a {}", other.kind_name())));
                }
            },
            Nav::Index(i) => match &shape {
                Shape::List { of } => {
                    prefix = codec::child_key(&prefix, &codec::order_u64(*i));
                    shape = of.as_ref().clone();
                }
                Shape::Deque { .. } => {
                    return Err(Error::Query(
                        "absolute Deque Index is not a write address; use push".into(),
                    ));
                }
                other => {
                    return Err(Error::Query(format!("Index on a {}", other.kind_name())));
                }
            },
            other => {
                return Err(Error::Query(format!(
                    "write path cannot contain {}",
                    nav_kind(other)
                )));
            }
        }
    }
    Ok(Location { prefix, shape })
}

/// Recognize a tagged navigation combinator: CBOR tag 27 carrying
/// `["nav", name]`. This is how untyped frontends spell the collecting
/// steps of the query algebra — `all`, `keys`, `vals`, `first`, `last` —
/// inside an ordinary path array.
fn tagged_nav(step: &Value) -> Result<Option<Nav>> {
    let Value::Tag(27, inner) = step else {
        return Ok(None);
    };
    let Value::Array(parts) = inner.as_ref() else {
        return Ok(None);
    };
    match (parts.first(), parts.get(1)) {
        (Some(Value::Text(t)), Some(Value::Text(name))) if t == "nav" => match name.as_str() {
            "all" => Ok(Some(Nav::All)),
            "keys" => Ok(Some(Nav::Keys)),
            "vals" => Ok(Some(Nav::Values)),
            "entries" => Ok(Some(Nav::Entries)),
            "first" => Ok(Some(Nav::First)),
            "last" => Ok(Some(Nav::Last)),
            other => Err(Error::Query(format!("unknown nav combinator {other:?}"))),
        },
        _ => Ok(None),
    }
}

/// Turn a list of CBOR values into navs, choosing Field / Key / Index from
/// the shape underfoot. Strings against a Record (or a Leaf interior) are
/// fields; anything against a Map is a key; integers against a List are
/// indices. Tagged `#nav` steps become the collecting combinators.
pub fn navs_for(schema: &Shape, steps: &[Value]) -> Result<Vec<Nav>> {
    let mut shape = schema.clone();
    let mut navs = Vec::with_capacity(steps.len());
    for step in steps {
        if let Some(nav) = tagged_nav(step)? {
            shape = match (&nav, &shape) {
                (Nav::Keys, Shape::Map { .. }) => Shape::Leaf,
                (_, Shape::Map { of } | Shape::List { of } | Shape::Deque { of }) => {
                    of.as_ref().clone()
                }
                (_, other) => {
                    return Err(Error::Query(format!(
                        "nav combinator on a {}",
                        other.kind_name()
                    )));
                }
            };
            navs.push(nav);
            continue;
        }
        let nav = match (&shape, step) {
            (Shape::Record { .. }, Value::Text(name)) => Nav::Field(name.clone()),
            (Shape::Map { .. }, v) => Nav::Key(v.clone()),
            (Shape::List { .. } | Shape::Deque { .. }, v) => {
                if let Some(i) = as_u64(v) {
                    Nav::Index(i)
                } else {
                    Nav::Key(v.clone())
                }
            }
            (Shape::Leaf, Value::Text(name)) => Nav::Field(name.clone()),
            (Shape::Leaf, v) => Nav::Key(v.clone()),
            (_, Value::Text(name)) => Nav::Field(name.clone()),
            (_, v) => Nav::Key(v.clone()),
        };
        // Advance the shape so the next step sees the child.
        shape = match &nav {
            Nav::Field(name) => match &shape {
                Shape::Record { .. } => shape.field(name)?.1.clone(),
                Shape::Map { of } | Shape::List { of } | Shape::Deque { of } => of.as_ref().clone(),
                Shape::Leaf => Shape::Leaf,
                other => {
                    return Err(Error::Query(format!(
                        "field {name:?} on a {}",
                        other.kind_name()
                    )));
                }
            },
            Nav::Key(_) | Nav::Index(_) => match &shape {
                Shape::Map { of } | Shape::List { of } | Shape::Deque { of } => of.as_ref().clone(),
                Shape::Leaf => Shape::Leaf,
                other => {
                    return Err(Error::Query(format!(
                        "key/index on a {}",
                        other.kind_name()
                    )));
                }
            },
            _ => shape,
        };
        navs.push(nav);
    }
    Ok(navs)
}

/// Put `value` at `navs`. A Record put with a map value expands into one
/// put per field. A Sum put sets the accumulator exactly.
pub fn put(schema: &Shape, namespace: Option<&str>, navs: &[Nav], value: &Value) -> Result<Vec<Write>> {
    let loc = resolve(schema, namespace, navs)?;
    put_at(&loc.prefix, &loc.shape, value)
}

fn put_at(prefix: &[u8], shape: &Shape, value: &Value) -> Result<Vec<Write>> {
    match shape {
        Shape::Leaf => Ok(vec![Write::new(Op::Put {
            key: prefix.to_vec(),
            value: encode_value(value)?,
        })]),
        Shape::Sum => Ok(vec![Write::new(Op::Put {
            key: prefix.to_vec(),
            value: encode_sum_value(value)?,
        })]),
        Shape::Record { fields } => {
            let mut out = Vec::new();
            for (i, (name, child)) in fields.iter().enumerate() {
                let child_val = map_get(value, name).unwrap_or(Value::Null);
                if matches!(child_val, Value::Null) && !matches!(child, Shape::Leaf) {
                    continue;
                }
                let p = codec::child_key(prefix, &field_segment(i as u32));
                out.extend(put_at(&p, child, &child_val)?);
            }
            Ok(out)
        }
        other => Err(Error::Query(format!(
            "put on a {} — put each entry, or clear + push",
            other.kind_name()
        ))),
    }
}

/// Delete a Leaf/Sum, or range-delete a collection/record subtree.
pub fn delete(schema: &Shape, namespace: Option<&str>, navs: &[Nav]) -> Result<Write> {
    let loc = resolve(schema, namespace, navs)?;
    match loc.shape {
        Shape::Leaf | Shape::Sum => Ok(Write::new(Op::Delete { key: loc.prefix })),
        _ => Ok(Write::new(Op::DeletePrefix { prefix: loc.prefix })),
    }
}

/// Blind merge-add on a Sum.
pub fn add(schema: &Shape, namespace: Option<&str>, navs: &[Nav], delta: &Value) -> Result<Write> {
    let loc = resolve(schema, namespace, navs)?;
    if !matches!(loc.shape, Shape::Sum) {
        return Err(Error::Query(format!(
            "add requires Sum, got {}",
            loc.shape.kind_name()
        )));
    }
    Ok(Write::new(Op::Merge {
        key: loc.prefix,
        value: encode_sum_value(delta)?,
    }))
}

/// Append to a List of Leaf (resolved against length at commit).
pub fn push(schema: &Shape, namespace: Option<&str>, navs: &[Nav], value: &Value) -> Result<Write> {
    let loc = resolve(schema, namespace, navs)?;
    match loc.shape {
        Shape::List { of } if matches!(of.as_ref(), Shape::Leaf) => Ok(Write::new(Op::ListPush {
            prefix: loc.prefix,
            value: encode_value(value)?,
        })),
        Shape::Deque { of } if matches!(of.as_ref(), Shape::Leaf) => {
            Ok(Write::new(Op::DequePushBack {
                prefix: loc.prefix,
                value: encode_value(value)?,
            }))
        }
        other => Err(Error::Query(format!(
            "push requires List<Leaf> or Deque<Leaf>, got {}",
            other.kind_name()
        ))),
    }
}

/// Range-delete a collection or record.
pub fn clear(schema: &Shape, namespace: Option<&str>, navs: &[Nav]) -> Result<Write> {
    let loc = resolve(schema, namespace, navs)?;
    Ok(Write::new(Op::DeletePrefix { prefix: loc.prefix }))
}

/// Point-read the value at `navs` from committed state.
pub fn peek(db: &Db, schema: &Shape, namespace: Option<&str>, navs: &[Nav]) -> Result<Value> {
    let mut q = Query::new(navs.to_vec());
    q.namespace = namespace.map(|s| s.to_string());
    if q.is_collecting() {
        return Err(Error::Query("peek requires a point path".into()));
    }
    // Prefer `one` for scalars so a missing leaf is Null, not an empty map.
    match one(db, schema, &q)? {
        Some(v) => Ok(v),
        None => subtree(db, schema, &q),
    }
}

fn encode_sum_value(value: &Value) -> Result<Vec<u8>> {
    if let Some(i) = as_i64(value) {
        return Ok(encode_sum::<i64>(i));
    }
    if let Some(f) = as_f64(value) {
        return Ok(encode_sum::<f64>(f));
    }
    Err(Error::Query(format!("Sum value must be numeric, got {value:?}")))
}

fn map_get<'a>(value: &'a Value, name: &str) -> Option<Value> {
    match value {
        Value::Map(pairs) => pairs.iter().find_map(|(k, v)| match k {
            Value::Text(s) if s == name => Some(v.clone()),
            _ => None,
        }),
        _ => None,
    }
}

fn as_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Integer(i) => u64::try_from(*i).ok(),
        Value::Float(f) if *f >= 0.0 && f.fract() == 0.0 && *f <= u64::MAX as f64 => {
            Some(*f as u64)
        }
        _ => None,
    }
}

fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Integer(i) => i64::try_from(*i).ok(),
        Value::Float(f) if f.fract() == 0.0 && *f >= i64::MIN as f64 && *f <= i64::MAX as f64 => {
            Some(*f as i64)
        }
        _ => None,
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    match v {
        Value::Float(f) => Some(*f),
        Value::Integer(i) => i64::try_from(*i).ok().map(|n| n as f64),
        _ => None,
    }
}

fn nav_kind(nav: &Nav) -> &'static str {
    match nav {
        Nav::Field(_) => "Field",
        Nav::Key(_) => "Key",
        Nav::Index(_) => "Index",
        Nav::All => "All",
        Nav::Values => "Values",
        Nav::Keys => "Keys",
        Nav::Entries => "Entries",
        Nav::Where(_) => "Where",
        Nav::First => "First",
        Nav::Last => "Last",
        Nav::Slice { .. } => "Slice",
    }
}

/// Convert a serde_json value to the CBOR value the engine speaks.
pub fn json_to_cbor(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Integer(Integer::from(i))
            } else if let Some(u) = n.as_u64() {
                Value::Integer(Integer::from(u))
            } else if let Some(f) = n.as_f64() {
                Value::Float(f)
            } else {
                Value::Null
            }
        }
        serde_json::Value::String(s) => Value::Text(s.clone()),
        serde_json::Value::Array(xs) => Value::Array(xs.iter().map(json_to_cbor).collect()),
        serde_json::Value::Object(map) => Value::Map(
            map.iter()
                .map(|(k, v)| (Value::Text(k.clone()), json_to_cbor(v)))
                .collect(),
        ),
    }
}

/// Convert an engine value to JSON for language frontends.
pub fn cbor_to_json(v: &Value) -> serde_json::Value {
    match v {
        Value::Null => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Integer(i) => {
            if let Ok(n) = i64::try_from(*i) {
                serde_json::Value::Number(n.into())
            } else if let Ok(n) = u64::try_from(*i) {
                serde_json::Value::Number(n.into())
            } else {
                serde_json::Value::Null
            }
        }
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Value::Text(s) => serde_json::Value::String(s.clone()),
        Value::Bytes(b) => serde_json::Value::Array(
            b.iter()
                .map(|x| serde_json::Value::Number((*x).into()))
                .collect(),
        ),
        Value::Array(xs) => serde_json::Value::Array(xs.iter().map(cbor_to_json).collect()),
        Value::Map(pairs) => {
            let all_text = pairs.iter().all(|(k, _)| matches!(k, Value::Text(_)));
            if all_text {
                let mut obj = serde_json::Map::new();
                for (k, v) in pairs {
                    if let Value::Text(s) = k {
                        obj.insert(s.clone(), cbor_to_json(v));
                    }
                }
                serde_json::Value::Object(obj)
            } else {
                serde_json::Value::Array(
                    pairs
                        .iter()
                        .map(|(k, v)| serde_json::json!([cbor_to_json(k), cbor_to_json(v)]))
                        .collect(),
                )
            }
        }
        Value::Tag(_, inner) => cbor_to_json(inner),
        _ => serde_json::Value::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Db, Durability};

    fn schema() -> Shape {
        Shape::record(vec![
            (
                "todos".into(),
                Shape::map(Shape::record(vec![
                    ("text".into(), Shape::Leaf),
                    ("done".into(), Shape::Leaf),
                ])),
            ),
            (
                "stats".into(),
                Shape::record(vec![
                    ("created".into(), Shape::Sum),
                    ("completed".into(), Shape::Sum),
                ]),
            ),
            ("title".into(), Shape::Leaf),
        ])
    }

    #[test]
    fn put_record_expands_and_peek_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let s = schema();
        let navs = navs_for(
            &s,
            &[
                Value::Text("todos".into()),
                Value::Integer(Integer::from(0i64)),
            ],
        )
        .unwrap();
        let writes = put(
            &s,
            None,
            &navs,
            &Value::Map(vec![
                (Value::Text("text".into()), Value::Text("milk".into())),
                (Value::Text("done".into()), Value::Bool(false)),
            ]),
        )
        .unwrap();
        assert_eq!(writes.len(), 2);
        db.apply(&writes, Durability::DisableWal).unwrap();

        let text = peek(
            &db,
            &s,
            None,
            &navs_for(
                &s,
                &[
                    Value::Text("todos".into()),
                    Value::Integer(Integer::from(0i64)),
                    Value::Text("text".into()),
                ],
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(text, Value::Text("milk".into()));
    }

    #[test]
    fn tagged_nav_steps_build_collecting_queries() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let s = schema();
        for (i, text) in ["milk", "eggs"].iter().enumerate() {
            let navs = navs_for(
                &s,
                &[
                    Value::Text("todos".into()),
                    Value::Integer(Integer::from(i as i64)),
                    Value::Text("text".into()),
                ],
            )
            .unwrap();
            db.apply(
                &put(&s, None, &navs, &Value::Text(text.to_string())).unwrap(),
                Durability::DisableWal,
            )
            .unwrap();
        }

        let nav_all = Value::Tag(
            27,
            Box::new(Value::Array(vec![
                Value::Text("nav".into()),
                Value::Text("all".into()),
            ])),
        );
        let navs = navs_for(
            &s,
            &[
                Value::Text("todos".into()),
                nav_all,
                Value::Text("text".into()),
            ],
        )
        .unwrap();
        assert_eq!(navs[1], Nav::All);
        let texts = crate::query::select(&db, &s, &Query::new(navs)).unwrap();
        assert_eq!(
            texts,
            vec![Value::Text("milk".into()), Value::Text("eggs".into())]
        );

        // a collecting nav is still rejected on the write side
        let nav_all = Value::Tag(
            27,
            Box::new(Value::Array(vec![
                Value::Text("nav".into()),
                Value::Text("all".into()),
            ])),
        );
        let navs = navs_for(&s, &[Value::Text("todos".into()), nav_all]).unwrap();
        assert!(put(&s, None, &navs, &Value::Bool(true)).is_err());
    }

    #[test]
    fn add_is_blind_sum() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(dir.path()).unwrap();
        let s = schema();
        let navs = navs_for(
            &s,
            &[Value::Text("stats".into()), Value::Text("created".into())],
        )
        .unwrap();
        db.apply(
            &[
                add(&s, None, &navs, &Value::Integer(Integer::from(1i64))).unwrap(),
                add(&s, None, &navs, &Value::Integer(Integer::from(2i64))).unwrap(),
            ],
            Durability::DisableWal,
        )
        .unwrap();
        let n = peek(&db, &s, None, &navs).unwrap();
        assert_eq!(n, Value::Integer(Integer::from(3i64)));
    }
}
