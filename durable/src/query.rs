//! Serializable, Specter-shaped read language.
//!
//! A [`Query`] is a closed algebra: field/key/index steps, collection
//! combinators, and a predicate language. No callables. The whole thing is
//! canonical-enough CBOR, so a Python frontend can build it and this engine
//! can interpret it against a materialized view.
//!
//! Writes do not live here. The only legal state transition is still
//! `Event → reducer → Tx`.

use std::cmp::Ordering;
use std::fmt;

use ciborium::value::Integer;
use ciborium::Value;

use crate::schema::decode_sum;
use crate::shape::field_segment;
use crate::{codec, decode_value, encode_value, read_i64, read_u64, Db, Error, Result, Shape};

/// One navigation step.
#[derive(Debug, Clone, PartialEq)]
pub enum Nav {
    Field(String),
    Key(Value),
    Index(u64),
    All,
    Values,
    Keys,
    Entries,
    Where(Predicate),
    First,
    Last,
    Slice {
        start: Option<u64>,
        end: Option<u64>,
    },
}

/// Boolean test over a focused value. No embedded code.
#[derive(Debug, Clone, PartialEq)]
pub enum Predicate {
    Eq(Expr, Expr),
    Ne(Expr, Expr),
    Lt(Expr, Expr),
    Le(Expr, Expr),
    Gt(Expr, Expr),
    Ge(Expr, Expr),
    And(Vec<Predicate>),
    Or(Vec<Predicate>),
    Not(Box<Predicate>),
    Exists(Vec<Nav>),
}

/// A value in a predicate: a field of the current item, a literal, or a path.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Field(String),
    Lit(Value),
    Path(Vec<Nav>),
}

/// Explicit terminal semantics. A path does not imply how to collect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Terminal {
    One,
    Select,
    Subtree,
    Entries,
}

/// A namespaced navigation. Terminals are chosen by the method that runs it.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    pub namespace: Option<String>,
    pub navs: Vec<Nav>,
}

impl Query {
    pub fn new(navs: impl Into<Vec<Nav>>) -> Self {
        Self {
            namespace: None,
            navs: navs.into(),
        }
    }

    pub fn namespaced(namespace: impl Into<String>, navs: impl Into<Vec<Nav>>) -> Self {
        Self {
            namespace: Some(namespace.into()),
            navs: navs.into(),
        }
    }

    pub fn is_collecting(&self) -> bool {
        self.navs.iter().any(|n| {
            matches!(
                n,
                Nav::All | Nav::Values | Nav::Keys | Nav::Entries | Nav::Where(_) | Nav::Slice { .. }
            )
        })
    }

    pub fn to_cbor(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&query_to_value(self), &mut bytes)
            .map_err(|e| Error::Serialize(e.to_string()))?;
        Ok(bytes)
    }

    pub fn from_cbor(bytes: &[u8]) -> Result<Self> {
        let v: Value =
            ciborium::de::from_reader(bytes).map_err(|e| Error::Deserialize(e.to_string()))?;
        query_from_value(&v)
    }
}

/// Estimated I/O class. Point is not a scan; a scan is never a point get.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostClass {
    Point,
    Bounded,
    Scan,
}

impl CostClass {
    fn raise(self, other: CostClass) -> CostClass {
        use CostClass::*;
        match (self, other) {
            (Scan, _) | (_, Scan) => Scan,
            (Bounded, _) | (_, Bounded) => Bounded,
            (Point, Point) => Point,
        }
    }
}

impl fmt::Display for CostClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CostClass::Point => f.write_str("POINT"),
            CostClass::Bounded => f.write_str("BOUNDED"),
            CostClass::Scan => f.write_str("SCAN"),
        }
    }
}

/// A compiled read plan: the cost class plus a human-readable trace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub class: CostClass,
    pub steps: Vec<String>,
}

impl fmt::Display for Plan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for step in &self.steps {
            writeln!(f, "{step}")?;
        }
        write!(f, "estimated class: {}", self.class)
    }
}

/// Compile `query` against `schema` without touching the database.
pub fn explain(schema: &Shape, query: &Query) -> Result<Plan> {
    let mut shape = schema.clone();
    let mut class = CostClass::Point;
    let mut steps = Vec::new();
    let mut in_value = false;
    let mut path = match &query.namespace {
        Some(ns) => format!("/{ns}"),
        None => String::from("/"),
    };

    for nav in &query.navs {
        if in_value {
            steps.push(format!("  → in-memory {}", nav_label(nav)));
            continue;
        }
        match nav {
            Nav::Field(name) => {
                if matches!(shape, Shape::Leaf) {
                    in_value = true;
                    steps.push(format!("  → Field {name} (leaf interior)"));
                } else {
                    let (_, next) = shape.field(name)?;
                    path = format!("{path}{name}/");
                    steps.push(format!("PointGet {path}"));
                    shape = next.clone();
                }
            }
            Nav::Key(k) => {
                if matches!(shape, Shape::Leaf) {
                    in_value = true;
                    steps.push(format!("  → Key {} (leaf interior)", value_label(k)));
                } else {
                    if !matches!(shape, Shape::Map { .. }) {
                        return Err(Error::Query("Key requires Map".into()));
                    }
                    path = format!("{path}[{}]/", value_label(k));
                    steps.push(format!("PointGet {path}"));
                    shape = shape.element()?.clone();
                }
            }
            Nav::Index(i) => {
                if matches!(shape, Shape::Leaf) {
                    in_value = true;
                    steps.push(format!("  → Index {i} (leaf interior)"));
                } else {
                    if !matches!(shape, Shape::List { .. } | Shape::Deque { .. }) {
                        return Err(Error::Query("Index requires List or Deque".into()));
                    }
                    path = format!("{path}[{i}]/");
                    steps.push(format!("PointGet {path}"));
                    shape = shape.element()?.clone();
                }
            }
            Nav::All | Nav::Values => {
                class = class.raise(CostClass::Scan);
                steps.push(format!("PrefixScan {path}"));
                shape = shape.element()?.clone();
            }
            Nav::Keys | Nav::Entries => {
                class = class.raise(CostClass::Scan);
                steps.push(format!("PrefixScan {path}"));
            }
            Nav::Where(pred) => {
                class = class.raise(CostClass::Scan);
                steps.push(format!("  → Filter {}", pred_label(pred)));
                if let Some(field) = eq_field_hint(pred) {
                    steps.push(format!(
                        "hint: equality on {field:?} is a SCAN; a Map keyed by {field} is POINT"
                    ));
                }
                if shape.is_collection() {
                    shape = shape.element()?.clone();
                }
            }
            Nav::First | Nav::Last => {
                class = class.raise(CostClass::Bounded);
                steps.push(format!("  → {}", nav_label(nav)));
                shape = shape.element()?.clone();
            }
            Nav::Slice { start, end } => {
                class = class.raise(CostClass::Scan);
                steps.push(format!(
                    "  → Slice {}..{}",
                    start.map(|s| s.to_string()).unwrap_or_default(),
                    end.map(|s| s.to_string()).unwrap_or_default()
                ));
                shape = shape.element()?.clone();
            }
        }
    }
    if steps.is_empty() {
        steps.push(format!("PointGet {path}"));
    }
    Ok(Plan { class, steps })
}

/// Execute `one`: a single location. Collection navs are rejected.
pub fn one(db: &Db, schema: &Shape, query: &Query) -> Result<Option<Value>> {
    if query.is_collecting() {
        return Err(Error::Query(
            "one() requires a point path; use select() for All/Where/Slice".into(),
        ));
    }
    let foci = collect_foci(db, schema, query)?;
    match foci.len() {
        0 => Ok(None),
        1 => match materialize_focus(db, &foci[0])? {
            Value::Null => Ok(None),
            v => Ok(Some(v)),
        },
        n => Err(Error::Query(format!("one() matched {n} locations"))),
    }
}

/// Execute `select`: the list of values at every focus.
pub fn select(db: &Db, schema: &Shape, query: &Query) -> Result<Vec<Value>> {
    let foci = collect_foci(db, schema, query)?;
    let mut out = Vec::with_capacity(foci.len());
    for focus in foci {
        out.push(materialize_focus(db, &focus)?);
    }
    Ok(out)
}

/// Execute `subtree`: reconstruct the nested value at a point path.
pub fn subtree(db: &Db, schema: &Shape, query: &Query) -> Result<Value> {
    if query.is_collecting() {
        return Err(Error::Query(
            "subtree() requires a point path; use select() for collections".into(),
        ));
    }
    let foci = collect_foci(db, schema, query)?;
    match foci.len() {
        0 => Ok(Value::Null),
        1 => materialize_focus(db, &foci[0]),
        n => Err(Error::Query(format!("subtree() matched {n} locations"))),
    }
}

/// Execute `entries`: key/value pairs of a map at a point path.
pub fn entries(db: &Db, schema: &Shape, query: &Query) -> Result<Vec<(Value, Value)>> {
    if query.is_collecting() {
        return Err(Error::Query("entries() requires a point path to a Map".into()));
    }
    let foci = collect_foci(db, schema, query)?;
    if foci.len() != 1 {
        return Err(Error::Query("entries() requires exactly one map".into()));
    }
    match &foci[0] {
        Focus::Db { prefix, shape } => {
            if !matches!(shape, Shape::Map { .. }) {
                return Err(Error::Query("entries() requires a Map".into()));
            }
            map_entries(db, prefix, shape)
        }
        Focus::Val(Value::Map(pairs)) => Ok(pairs.clone()),
        Focus::Val(_) => Err(Error::Query("entries() requires a Map".into())),
    }
}

/// Execute `project`: many queries, one snapshot, one nested result map.
pub fn project(db: &Db, schema: &Shape, spec: &[(String, Query)]) -> Result<Value> {
    let mut pairs = Vec::with_capacity(spec.len());
    for (name, query) in spec {
        let value = if query.is_collecting() {
            Value::Array(select(db, schema, query)?)
        } else {
            subtree(db, schema, query)?
        };
        pairs.push((Value::Text(name.clone()), value));
    }
    Ok(Value::Map(pairs))
}

// ---------------------------------------------------------------------------
// Focus machine
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
enum Focus {
    Db { prefix: Vec<u8>, shape: Shape },
    Val(Value),
}

fn root_focus(schema: &Shape, query: &Query) -> Focus {
    let prefix = match &query.namespace {
        Some(ns) => {
            let mut p = Vec::new();
            codec::put_segment(&mut p, ns.as_bytes());
            p
        }
        None => Vec::new(),
    };
    Focus::Db {
        prefix,
        shape: schema.clone(),
    }
}

fn collect_foci(db: &Db, schema: &Shape, query: &Query) -> Result<Vec<Focus>> {
    let mut current = vec![root_focus(schema, query)];
    for nav in &query.navs {
        let mut next = Vec::new();
        for focus in current {
            next.extend(step(db, focus, nav)?);
        }
        current = next;
    }
    Ok(current)
}

fn step(db: &Db, focus: Focus, nav: &Nav) -> Result<Vec<Focus>> {
    match (focus, nav) {
        (Focus::Val(v), nav) => step_value(v, nav),
        (Focus::Db { prefix, shape }, nav) => step_db(db, prefix, shape, nav),
    }
}

fn step_db(db: &Db, prefix: Vec<u8>, shape: Shape, nav: &Nav) -> Result<Vec<Focus>> {
    match nav {
        Nav::Field(name) => match &shape {
            Shape::Leaf => {
                let v = read_leaf(db, &prefix)?;
                step_value(v, nav)
            }
            Shape::Record { .. } => {
                let (id, next) = shape.field(name)?;
                Ok(vec![Focus::Db {
                    prefix: codec::child_key(&prefix, &field_segment(id)),
                    shape: next.clone(),
                }])
            }
            Shape::Map { of } => {
                // Attribute access on a map is Key(name).
                let key = encode_value(&name)?;
                Ok(vec![Focus::Db {
                    prefix: codec::child_key(&prefix, &key),
                    shape: of.as_ref().clone(),
                }])
            }
            _ => Err(Error::Query(format!(
                "field {name:?} on a {}",
                shape.kind_name()
            ))),
        },
        Nav::Key(k) => match &shape {
            Shape::Leaf => {
                let v = read_leaf(db, &prefix)?;
                step_value(v, nav)
            }
            Shape::Map { of } => {
                let key = encode_value(k)?;
                Ok(vec![Focus::Db {
                    prefix: codec::child_key(&prefix, &key),
                    shape: of.as_ref().clone(),
                }])
            }
            Shape::List { .. } | Shape::Deque { .. } => {
                let i = as_u64(k).ok_or_else(|| {
                    Error::Query("integer Key on List/Deque required; use Index".into())
                })?;
                step_db(db, prefix, shape, &Nav::Index(i))
            }
            _ => Err(Error::Query(format!("Key on a {}", shape.kind_name()))),
        },
        Nav::Index(i) => match &shape {
            Shape::Leaf => {
                let v = read_leaf(db, &prefix)?;
                step_value(v, nav)
            }
            Shape::List { of } => Ok(vec![Focus::Db {
                prefix: codec::child_key(&prefix, &codec::order_u64(*i)),
                shape: of.as_ref().clone(),
            }]),
            Shape::Deque { of } => {
                let head = read_i64(db, &codec::meta_key(&prefix, b"head"))?.unwrap_or(0);
                let idx = head + *i as i64;
                Ok(vec![Focus::Db {
                    prefix: codec::child_key(&prefix, &codec::order_i64(idx)),
                    shape: of.as_ref().clone(),
                }])
            }
            _ => Err(Error::Query(format!("Index on a {}", shape.kind_name()))),
        },
        Nav::All | Nav::Values => Ok(collection_foci(db, &prefix, &shape)?
            .into_iter()
            .map(|(_, c)| c)
            .collect()),
        Nav::Keys => Ok(collection_foci(db, &prefix, &shape)?
            .into_iter()
            .filter_map(|(k, _)| k.map(Focus::Val))
            .collect()),
        Nav::Entries => {
            let mut out = Vec::new();
            for (k, child) in collection_foci(db, &prefix, &shape)? {
                let key = k.unwrap_or(Value::Null);
                let val = materialize_focus(db, &child)?;
                out.push(Focus::Val(Value::Array(vec![key, val])));
            }
            Ok(out)
        }
        Nav::Where(pred) => {
            let mut out = Vec::new();
            for (_, child) in collection_foci(db, &prefix, &shape)? {
                if eval_pred(db, &child, pred)? {
                    out.push(child);
                }
            }
            Ok(out)
        }
        Nav::First => Ok(collection_foci(db, &prefix, &shape)?
            .into_iter()
            .next()
            .map(|(_, c)| vec![c])
            .unwrap_or_default()),
        Nav::Last => Ok(collection_foci(db, &prefix, &shape)?
            .into_iter()
            .last()
            .map(|(_, c)| vec![c])
            .unwrap_or_default()),
        Nav::Slice { start, end } => {
            let kids = collection_foci(db, &prefix, &shape)?;
            let start = start.unwrap_or(0) as usize;
            let end = end.map(|e| e as usize).unwrap_or(kids.len());
            Ok(kids
                .into_iter()
                .skip(start)
                .take(end.saturating_sub(start))
                .map(|(_, c)| c)
                .collect())
        }
    }
}

fn step_value(value: Value, nav: &Nav) -> Result<Vec<Focus>> {
    match nav {
        Nav::Field(name) => Ok(vec![Focus::Val(map_get(&value, &Value::Text(name.clone())))]),
        Nav::Key(k) => Ok(vec![Focus::Val(map_get(&value, k))]),
        Nav::Index(i) => Ok(vec![Focus::Val(array_get(&value, *i))]),
        Nav::All | Nav::Values => Ok(value_children(&value)
            .into_iter()
            .map(|(_, v)| Focus::Val(v))
            .collect()),
        Nav::Keys => Ok(value_children(&value)
            .into_iter()
            .filter_map(|(k, _)| k.map(Focus::Val))
            .collect()),
        Nav::Entries => Ok(value_children(&value)
            .into_iter()
            .map(|(k, v)| Focus::Val(Value::Array(vec![k.unwrap_or(Value::Null), v])))
            .collect()),
        Nav::Where(pred) => {
            let mut out = Vec::new();
            for (_, v) in value_children(&value) {
                let focus = Focus::Val(v.clone());
                if eval_pred_value(&focus, pred)? {
                    out.push(focus);
                }
            }
            Ok(out)
        }
        Nav::First => Ok(value_children(&value)
            .into_iter()
            .next()
            .map(|(_, v)| vec![Focus::Val(v)])
            .unwrap_or_default()),
        Nav::Last => Ok(value_children(&value)
            .into_iter()
            .last()
            .map(|(_, v)| vec![Focus::Val(v)])
            .unwrap_or_default()),
        Nav::Slice { start, end } => {
            let kids = value_children(&value);
            let start = start.unwrap_or(0) as usize;
            let end = end.map(|e| e as usize).unwrap_or(kids.len());
            Ok(kids
                .into_iter()
                .skip(start)
                .take(end.saturating_sub(start))
                .map(|(_, v)| Focus::Val(v))
                .collect())
        }
    }
}

fn children(db: &Db, prefix: &[u8], shape: &Shape) -> Result<Vec<(Option<Value>, Focus)>> {
    match shape {
        Shape::Map { of } => {
            let keys = map_keys(db, prefix)?;
            let mut out = Vec::with_capacity(keys.len());
            for key in keys {
                let seg = encode_value(&key)?;
                out.push((
                    Some(key),
                    Focus::Db {
                        prefix: codec::child_key(prefix, &seg),
                        shape: of.as_ref().clone(),
                    },
                ));
            }
            Ok(out)
        }
        Shape::List { of } => {
            let len = read_u64(db, &codec::meta_key(prefix, b"len"))?.unwrap_or(0);
            let mut out = Vec::with_capacity(len as usize);
            for i in 0..len {
                out.push((
                    Some(Value::Integer(Integer::from(i))),
                    Focus::Db {
                        prefix: codec::child_key(prefix, &codec::order_u64(i)),
                        shape: of.as_ref().clone(),
                    },
                ));
            }
            Ok(out)
        }
        Shape::Deque { of } => {
            let head = read_i64(db, &codec::meta_key(prefix, b"head"))?.unwrap_or(0);
            let tail = read_i64(db, &codec::meta_key(prefix, b"tail"))?.unwrap_or(0);
            let mut out = Vec::new();
            let mut logical = 0u64;
            for idx in head..tail {
                out.push((
                    Some(Value::Integer(Integer::from(logical))),
                    Focus::Db {
                        prefix: codec::child_key(prefix, &codec::order_i64(idx)),
                        shape: of.as_ref().clone(),
                    },
                ));
                logical += 1;
            }
            Ok(out)
        }
        Shape::Leaf => {
            let v = read_leaf(db, prefix)?;
            Ok(value_children(&v)
                .into_iter()
                .map(|(k, val)| (k, Focus::Val(val)))
                .collect())
        }
        Shape::Record { fields } => {
            let mut out = Vec::with_capacity(fields.len());
            for (i, (name, child)) in fields.iter().enumerate() {
                out.push((
                    Some(Value::Text(name.clone())),
                    Focus::Db {
                        prefix: codec::child_key(prefix, &field_segment(i as u32)),
                        shape: child.clone(),
                    },
                ));
            }
            Ok(out)
        }
        Shape::Sum => Ok(vec![(None, Focus::Db {
            prefix: prefix.to_vec(),
            shape: Shape::Sum,
        })]),
    }
}

fn is_leaf_collection(shape: &Shape) -> bool {
    match shape {
        Shape::Map { of } | Shape::List { of } | Shape::Deque { of } => {
            matches!(of.as_ref(), Shape::Leaf)
        }
        _ => false,
    }
}

/// Children of a collection. A Map/List/Deque of Leaf is one prefix scan
/// into already-decoded values, not N point gets.
fn collection_foci(db: &Db, prefix: &[u8], shape: &Shape) -> Result<Vec<(Option<Value>, Focus)>> {
    if is_leaf_collection(shape) {
        return Ok(scan_leaf_children(db, prefix, shape)?
            .into_iter()
            .map(|(k, v)| (k, Focus::Val(v)))
            .collect());
    }
    children(db, prefix, shape)
}

fn scan_leaf_children(db: &Db, prefix: &[u8], shape: &Shape) -> Result<Vec<(Option<Value>, Value)>> {
    match shape {
        Shape::Map { .. } => scan_direct_children(db, prefix, ChildKey::Map),
        Shape::List { .. } => scan_direct_children(db, prefix, ChildKey::List),
        Shape::Deque { .. } => scan_direct_children(db, prefix, ChildKey::Deque),
        _ => Ok(Vec::new()),
    }
}

enum ChildKey {
    Map,
    List,
    Deque,
}

fn scan_direct_children(
    db: &Db,
    prefix: &[u8],
    kind: ChildKey,
) -> Result<Vec<(Option<Value>, Value)>> {
    let scan = codec::child_scan_prefix(prefix);
    let iter = db
        .raw()
        .iterator(rocksdb::IteratorMode::From(&scan, rocksdb::Direction::Forward));
    let mut out = Vec::new();
    let mut logical = 0u64;
    for item in iter {
        let (db_key, db_val) = item?;
        if !db_key.starts_with(&scan) {
            break;
        }
        let rest = &db_key[scan.len()..];
        let (key_seg, consumed) = codec::read_segment(rest)
            .ok_or_else(|| Error::Corruption("malformed collection key".into()))?;
        if consumed != rest.len() {
            // Nested key under a non-leaf child. Leaf collections store
            // the value at the child key itself.
            continue;
        }
        let key = match kind {
            ChildKey::Map => Some(decode_value(key_seg)?),
            ChildKey::List => {
                let i = decode_order_u64(key_seg).ok_or_else(|| {
                    Error::Corruption("malformed list index".into())
                })?;
                Some(Value::Integer(Integer::from(i)))
            }
            ChildKey::Deque => {
                let _idx = decode_order_i64(key_seg).ok_or_else(|| {
                    Error::Corruption("malformed deque index".into())
                })?;
                let k = Some(Value::Integer(Integer::from(logical)));
                logical += 1;
                k
            }
        };
        out.push((key, decode_value(&db_val)?));
    }
    Ok(out)
}

fn decode_order_u64(seg: &[u8]) -> Option<u64> {
    let bytes: [u8; 8] = seg.try_into().ok()?;
    Some(u64::from_be_bytes(bytes))
}

fn decode_order_i64(seg: &[u8]) -> Option<i64> {
    let bytes: [u8; 8] = seg.try_into().ok()?;
    Some((u64::from_be_bytes(bytes) ^ (1u64 << 63)) as i64)
}

fn value_children(value: &Value) -> Vec<(Option<Value>, Value)> {
    match value {
        Value::Array(xs) => xs
            .iter()
            .enumerate()
            .map(|(i, v)| (Some(Value::Integer(Integer::from(i as u64))), v.clone()))
            .collect(),
        Value::Map(pairs) => pairs
            .iter()
            .map(|(k, v)| (Some(k.clone()), v.clone()))
            .collect(),
        _ => Vec::new(),
    }
}

fn materialize_focus(db: &Db, focus: &Focus) -> Result<Value> {
    match focus {
        Focus::Val(v) => Ok(v.clone()),
        Focus::Db { prefix, shape } => materialize(db, prefix, shape),
    }
}

fn materialize(db: &Db, prefix: &[u8], shape: &Shape) -> Result<Value> {
    match shape {
        Shape::Leaf => read_leaf(db, prefix),
        Shape::Sum => match db.raw().get(prefix)? {
            Some(bytes) => {
                if let Some(n) = decode_sum::<i64>(&bytes) {
                    return Ok(Value::Integer(Integer::from(n)));
                }
                if let Some(n) = decode_sum::<u64>(&bytes) {
                    return Ok(Value::Integer(Integer::from(n)));
                }
                if let Some(n) = decode_sum::<f64>(&bytes) {
                    return Ok(Value::Float(n));
                }
                Err(Error::Corruption("malformed Sum accumulator".into()))
            }
            None => Ok(Value::Integer(Integer::from(0i64))),
        },
        Shape::Map { .. } => {
            let pairs = map_entries(db, prefix, shape)?;
            Ok(Value::Map(pairs))
        }
        Shape::List { .. } | Shape::Deque { .. } if is_leaf_collection(shape) => {
            Ok(Value::Array(
                scan_leaf_children(db, prefix, shape)?
                    .into_iter()
                    .map(|(_, v)| v)
                    .collect(),
            ))
        }
        Shape::List { .. } | Shape::Deque { .. } => {
            let mut xs = Vec::new();
            for (_, child) in children(db, prefix, shape)? {
                xs.push(materialize_focus(db, &child)?);
            }
            Ok(Value::Array(xs))
        }
        Shape::Record { fields } => {
            let mut pairs = Vec::with_capacity(fields.len());
            for (i, (name, child)) in fields.iter().enumerate() {
                let p = codec::child_key(prefix, &field_segment(i as u32));
                pairs.push((Value::Text(name.clone()), materialize(db, &p, child)?));
            }
            Ok(Value::Map(pairs))
        }
    }
}

fn read_leaf(db: &Db, prefix: &[u8]) -> Result<Value> {
    match db.raw().get(prefix)? {
        Some(bytes) => decode_value(&bytes),
        None => Ok(Value::Null),
    }
}

fn map_keys(db: &Db, prefix: &[u8]) -> Result<Vec<Value>> {
    let scan = codec::child_scan_prefix(prefix);
    let iter = db
        .raw()
        .iterator(rocksdb::IteratorMode::From(&scan, rocksdb::Direction::Forward));
    let mut keys = Vec::new();
    let mut last: Option<Vec<u8>> = None;
    for item in iter {
        let (db_key, _) = item?;
        if !db_key.starts_with(&scan) {
            break;
        }
        let rest = &db_key[scan.len()..];
        let (key_seg, _) = codec::read_segment(rest)
            .ok_or_else(|| Error::Corruption("malformed map entry key".into()))?;
        if last.as_deref() == Some(key_seg) {
            continue;
        }
        last = Some(key_seg.to_vec());
        keys.push(decode_value(key_seg)?);
    }
    Ok(keys)
}

fn map_entries(db: &Db, prefix: &[u8], shape: &Shape) -> Result<Vec<(Value, Value)>> {
    let of = shape.element()?;
    if matches!(of, Shape::Leaf) {
        return Ok(scan_leaf_children(db, prefix, shape)?
            .into_iter()
            .map(|(k, v)| (k.unwrap_or(Value::Null), v))
            .collect());
    }
    let keys = map_keys(db, prefix)?;
    let mut out = Vec::with_capacity(keys.len());
    for key in keys {
        let seg = encode_value(&key)?;
        let child = codec::child_key(prefix, &seg);
        out.push((key, materialize(db, &child, of)?));
    }
    Ok(out)
}

fn map_get(value: &Value, key: &Value) -> Value {
    match value {
        Value::Map(pairs) => pairs
            .iter()
            .find(|(k, _)| values_eq(k, key))
            .map(|(_, v)| v.clone())
            .unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn array_get(value: &Value, index: u64) -> Value {
    match value {
        Value::Array(xs) => xs.get(index as usize).cloned().unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Predicates
// ---------------------------------------------------------------------------

fn eval_pred(db: &Db, focus: &Focus, pred: &Predicate) -> Result<bool> {
    match pred {
        Predicate::Eq(a, b) => Ok(values_eq(&eval_expr(db, focus, a)?, &eval_expr(db, focus, b)?)),
        Predicate::Ne(a, b) => Ok(!values_eq(&eval_expr(db, focus, a)?, &eval_expr(db, focus, b)?)),
        Predicate::Lt(a, b) => Ok(cmp_values(&eval_expr(db, focus, a)?, &eval_expr(db, focus, b)?)
            == Some(Ordering::Less)),
        Predicate::Le(a, b) => {
            Ok(matches!(
                cmp_values(&eval_expr(db, focus, a)?, &eval_expr(db, focus, b)?),
                Some(Ordering::Less | Ordering::Equal)
            ))
        }
        Predicate::Gt(a, b) => Ok(cmp_values(&eval_expr(db, focus, a)?, &eval_expr(db, focus, b)?)
            == Some(Ordering::Greater)),
        Predicate::Ge(a, b) => {
            Ok(matches!(
                cmp_values(&eval_expr(db, focus, a)?, &eval_expr(db, focus, b)?),
                Some(Ordering::Greater | Ordering::Equal)
            ))
        }
        Predicate::And(ps) => {
            for p in ps {
                if !eval_pred(db, focus, p)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Predicate::Or(ps) => {
            for p in ps {
                if eval_pred(db, focus, p)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Predicate::Not(p) => Ok(!eval_pred(db, focus, p)?),
        Predicate::Exists(navs) => {
            let mut current = vec![focus.clone()];
            for nav in navs {
                let mut next = Vec::new();
                for f in current {
                    next.extend(step(db, f, nav)?);
                }
                current = next;
            }
            Ok(current.iter().any(|f| match materialize_focus(db, f) {
                Ok(Value::Null) => false,
                Ok(_) => true,
                Err(_) => false,
            }))
        }
    }
}

fn eval_pred_value(focus: &Focus, pred: &Predicate) -> Result<bool> {
    // Value foci don't need the db; reuse eval_pred with a dummy-safe path.
    // step() on Val never touches db, but Exists/Path might. Pass a dummy? We
    // only have Focus::Val here; eval_expr on Val doesn't read db except Path
    // through step. step on Val is fine. eval_pred still wants &Db for the
    // Exists branch's materialize of Db foci — which won't appear.
    //
    // Callers in step_value don't have a clean unused db. Use a side channel:
    // evaluate expressions directly off the value.
    eval_pred_without_db(focus, pred)
}

fn eval_pred_without_db(focus: &Focus, pred: &Predicate) -> Result<bool> {
    match pred {
        Predicate::Eq(a, b) => Ok(values_eq(&eval_expr_val(focus, a)?, &eval_expr_val(focus, b)?)),
        Predicate::Ne(a, b) => Ok(!values_eq(&eval_expr_val(focus, a)?, &eval_expr_val(focus, b)?)),
        Predicate::Lt(a, b) => {
            Ok(cmp_values(&eval_expr_val(focus, a)?, &eval_expr_val(focus, b)?)
                == Some(Ordering::Less))
        }
        Predicate::Le(a, b) => Ok(matches!(
            cmp_values(&eval_expr_val(focus, a)?, &eval_expr_val(focus, b)?),
            Some(Ordering::Less | Ordering::Equal)
        )),
        Predicate::Gt(a, b) => {
            Ok(cmp_values(&eval_expr_val(focus, a)?, &eval_expr_val(focus, b)?)
                == Some(Ordering::Greater))
        }
        Predicate::Ge(a, b) => Ok(matches!(
            cmp_values(&eval_expr_val(focus, a)?, &eval_expr_val(focus, b)?),
            Some(Ordering::Greater | Ordering::Equal)
        )),
        Predicate::And(ps) => {
            for p in ps {
                if !eval_pred_without_db(focus, p)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Predicate::Or(ps) => {
            for p in ps {
                if eval_pred_without_db(focus, p)? {
                    return Ok(true);
                }
            }
            Ok(false)
        }
        Predicate::Not(p) => Ok(!eval_pred_without_db(focus, p)?),
        Predicate::Exists(navs) => {
            let mut current = vec![focus.clone()];
            for nav in navs {
                let mut next = Vec::new();
                for f in current {
                    match f {
                        Focus::Val(v) => next.extend(step_value(v, nav)?),
                        Focus::Db { .. } => {
                            return Err(Error::Query(
                                "Exists over a db focus requires eval_pred".into(),
                            ))
                        }
                    }
                }
                current = next;
            }
            Ok(current.iter().any(|f| match f {
                Focus::Val(Value::Null) => false,
                Focus::Val(_) => true,
                Focus::Db { .. } => true,
            }))
        }
    }
}

fn eval_expr(db: &Db, focus: &Focus, expr: &Expr) -> Result<Value> {
    match expr {
        Expr::Lit(v) => Ok(v.clone()),
        Expr::Field(name) => match focus {
            Focus::Val(v) => Ok(map_get(v, &Value::Text(name.clone()))),
            Focus::Db { prefix, shape } => match shape {
                Shape::Leaf => Ok(map_get(
                    &read_leaf(db, prefix)?,
                    &Value::Text(name.clone()),
                )),
                Shape::Record { .. } => {
                    let (id, next) = shape.field(name)?;
                    materialize(db, &codec::child_key(prefix, &field_segment(id)), next)
                }
                Shape::Map { of } => {
                    let key = encode_value(&name)?;
                    materialize(db, &codec::child_key(prefix, &key), of)
                }
                _ => Ok(Value::Null),
            },
        },
        Expr::Path(navs) => {
            let mut current = vec![focus.clone()];
            for nav in navs {
                let mut next = Vec::new();
                for f in current {
                    next.extend(step(db, f, nav)?);
                }
                current = next;
            }
            match current.len() {
                0 => Ok(Value::Null),
                1 => materialize_focus(db, &current[0]),
                _ => Ok(Value::Array(
                    current
                        .iter()
                        .map(|f| materialize_focus(db, f))
                        .collect::<Result<Vec<_>>>()?,
                )),
            }
        }
    }
}

fn eval_expr_val(focus: &Focus, expr: &Expr) -> Result<Value> {
    match (focus, expr) {
        (_, Expr::Lit(v)) => Ok(v.clone()),
        (Focus::Val(v), Expr::Field(name)) => Ok(map_get(v, &Value::Text(name.clone()))),
        (Focus::Val(v), Expr::Path(navs)) => {
            let mut current = vec![Focus::Val(v.clone())];
            for nav in navs {
                let mut next = Vec::new();
                for f in current {
                    match f {
                        Focus::Val(val) => next.extend(step_value(val, nav)?),
                        Focus::Db { .. } => {
                            return Err(Error::Query("value path stepped into db".into()))
                        }
                    }
                }
                current = next;
            }
            match current.into_iter().next() {
                Some(Focus::Val(v)) => Ok(v),
                _ => Ok(Value::Null),
            }
        }
        _ => Ok(Value::Null),
    }
}

fn values_eq(a: &Value, b: &Value) -> bool {
    if let (Some(x), Some(y)) = (as_number(a), as_number(b)) {
        return x == y;
    }
    match (a, b) {
        (Value::Text(x), Value::Text(y)) => x == y,
        (Value::Bool(x), Value::Bool(y)) => x == y,
        (Value::Null, Value::Null) => true,
        (Value::Bytes(x), Value::Bytes(y)) => x == y,
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| values_eq(p, q))
        }
        _ => false,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Number {
    Int(i128),
    Float(f64),
}

impl PartialOrd for Number {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Number::Int(a), Number::Int(b)) => a.partial_cmp(b),
            (Number::Float(a), Number::Float(b)) => a.partial_cmp(b),
            (Number::Int(a), Number::Float(b)) => (*a as f64).partial_cmp(b),
            (Number::Float(a), Number::Int(b)) => a.partial_cmp(&(*b as f64)),
        }
    }
}

fn as_number(v: &Value) -> Option<Number> {
    match v {
        Value::Integer(i) => i128::try_from(*i).ok().map(Number::Int),
        Value::Float(f) => Some(Number::Float(*f)),
        _ => None,
    }
}

fn cmp_values(a: &Value, b: &Value) -> Option<Ordering> {
    if let (Some(x), Some(y)) = (as_number(a), as_number(b)) {
        return x.partial_cmp(&y);
    }
    match (a, b) {
        (Value::Text(x), Value::Text(y)) => Some(x.cmp(y)),
        (Value::Bool(x), Value::Bool(y)) => Some(x.cmp(y)),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Wire encoding
// ---------------------------------------------------------------------------

fn query_to_value(q: &Query) -> Value {
    let ns = match &q.namespace {
        Some(s) => Value::Text(s.clone()),
        None => Value::Null,
    };
    Value::Array(vec![
        ns,
        Value::Array(q.navs.iter().map(nav_to_value).collect()),
    ])
}

fn query_from_value(v: &Value) -> Result<Query> {
    let Value::Array(items) = v else {
        return Err(Error::Query("query must be [namespace, navs]".into()));
    };
    if items.len() != 2 {
        return Err(Error::Query("query must be [namespace, navs]".into()));
    }
    let namespace = match &items[0] {
        Value::Null => None,
        Value::Text(s) => Some(s.clone()),
        _ => return Err(Error::Query("namespace must be text or null".into())),
    };
    let Value::Array(navs) = &items[1] else {
        return Err(Error::Query("navs must be an array".into()));
    };
    Ok(Query {
        namespace,
        navs: navs.iter().map(nav_from_value).collect::<Result<_>>()?,
    })
}

fn nav_to_value(nav: &Nav) -> Value {
    let tag = |n: u64, rest: Vec<Value>| {
        let mut xs = vec![Value::Integer(Integer::from(n))];
        xs.extend(rest);
        Value::Array(xs)
    };
    match nav {
        Nav::Field(n) => tag(0, vec![Value::Text(n.clone())]),
        Nav::Key(v) => tag(1, vec![v.clone()]),
        Nav::Index(i) => tag(2, vec![Value::Integer(Integer::from(*i))]),
        Nav::All => tag(3, vec![]),
        Nav::Values => tag(4, vec![]),
        Nav::Keys => tag(5, vec![]),
        Nav::Entries => tag(6, vec![]),
        Nav::Where(p) => tag(7, vec![pred_to_value(p)]),
        Nav::First => tag(8, vec![]),
        Nav::Last => tag(9, vec![]),
        Nav::Slice { start, end } => tag(
            10,
            vec![opt_u64(*start), opt_u64(*end)],
        ),
    }
}

fn nav_from_value(v: &Value) -> Result<Nav> {
    let Value::Array(items) = v else {
        return Err(Error::Query("nav must be a tagged array".into()));
    };
    let tag = items
        .first()
        .and_then(as_u64)
        .ok_or_else(|| Error::Query("nav missing tag".into()))?;
    match tag {
        0 => match items.get(1) {
            Some(Value::Text(s)) => Ok(Nav::Field(s.clone())),
            _ => Err(Error::Query("Field needs a name".into())),
        },
        1 => items
            .get(1)
            .cloned()
            .map(Nav::Key)
            .ok_or_else(|| Error::Query("Key needs a value".into())),
        2 => items
            .get(1)
            .and_then(as_u64)
            .map(Nav::Index)
            .ok_or_else(|| Error::Query("Index needs a u64".into())),
        3 => Ok(Nav::All),
        4 => Ok(Nav::Values),
        5 => Ok(Nav::Keys),
        6 => Ok(Nav::Entries),
        7 => {
            let p = items
                .get(1)
                .ok_or_else(|| Error::Query("Where needs a predicate".into()))?;
            Ok(Nav::Where(pred_from_value(p)?))
        }
        8 => Ok(Nav::First),
        9 => Ok(Nav::Last),
        10 => Ok(Nav::Slice {
            start: items.get(1).and_then(as_opt_u64),
            end: items.get(2).and_then(as_opt_u64),
        }),
        other => Err(Error::Query(format!("unknown nav tag {other}"))),
    }
}

fn pred_to_value(p: &Predicate) -> Value {
    let tag = |n: u64, rest: Vec<Value>| {
        let mut xs = vec![Value::Integer(Integer::from(n))];
        xs.extend(rest);
        Value::Array(xs)
    };
    match p {
        Predicate::Eq(a, b) => tag(0, vec![expr_to_value(a), expr_to_value(b)]),
        Predicate::Ne(a, b) => tag(1, vec![expr_to_value(a), expr_to_value(b)]),
        Predicate::Lt(a, b) => tag(2, vec![expr_to_value(a), expr_to_value(b)]),
        Predicate::Le(a, b) => tag(3, vec![expr_to_value(a), expr_to_value(b)]),
        Predicate::Gt(a, b) => tag(4, vec![expr_to_value(a), expr_to_value(b)]),
        Predicate::Ge(a, b) => tag(5, vec![expr_to_value(a), expr_to_value(b)]),
        Predicate::And(ps) => tag(6, vec![Value::Array(ps.iter().map(pred_to_value).collect())]),
        Predicate::Or(ps) => tag(7, vec![Value::Array(ps.iter().map(pred_to_value).collect())]),
        Predicate::Not(p) => tag(8, vec![pred_to_value(p)]),
        Predicate::Exists(navs) => tag(9, vec![Value::Array(navs.iter().map(nav_to_value).collect())]),
    }
}

fn pred_from_value(v: &Value) -> Result<Predicate> {
    let Value::Array(items) = v else {
        return Err(Error::Query("predicate must be a tagged array".into()));
    };
    let tag = items
        .first()
        .and_then(as_u64)
        .ok_or_else(|| Error::Query("predicate missing tag".into()))?;
    let bin = |ctor: fn(Expr, Expr) -> Predicate| {
        Ok(ctor(
            expr_from_value(items.get(1).ok_or_else(|| Error::Query("missing lhs".into()))?)?,
            expr_from_value(items.get(2).ok_or_else(|| Error::Query("missing rhs".into()))?)?,
        ))
    };
    match tag {
        0 => bin(Predicate::Eq),
        1 => bin(Predicate::Ne),
        2 => bin(Predicate::Lt),
        3 => bin(Predicate::Le),
        4 => bin(Predicate::Gt),
        5 => bin(Predicate::Ge),
        6 => {
            let Value::Array(ps) = items
                .get(1)
                .ok_or_else(|| Error::Query("And needs a list".into()))?
            else {
                return Err(Error::Query("And needs a list".into()));
            };
            Ok(Predicate::And(
                ps.iter().map(pred_from_value).collect::<Result<_>>()?,
            ))
        }
        7 => {
            let Value::Array(ps) = items
                .get(1)
                .ok_or_else(|| Error::Query("Or needs a list".into()))?
            else {
                return Err(Error::Query("Or needs a list".into()));
            };
            Ok(Predicate::Or(
                ps.iter().map(pred_from_value).collect::<Result<_>>()?,
            ))
        }
        8 => {
            let p = items
                .get(1)
                .ok_or_else(|| Error::Query("Not needs a predicate".into()))?;
            Ok(Predicate::Not(Box::new(pred_from_value(p)?)))
        }
        9 => {
            let Value::Array(navs) = items
                .get(1)
                .ok_or_else(|| Error::Query("Exists needs a path".into()))?
            else {
                return Err(Error::Query("Exists needs a path".into()));
            };
            Ok(Predicate::Exists(
                navs.iter().map(nav_from_value).collect::<Result<_>>()?,
            ))
        }
        other => Err(Error::Query(format!("unknown predicate tag {other}"))),
    }
}

fn expr_to_value(e: &Expr) -> Value {
    match e {
        Expr::Field(n) => Value::Array(vec![
            Value::Integer(Integer::from(0u64)),
            Value::Text(n.clone()),
        ]),
        Expr::Lit(v) => Value::Array(vec![Value::Integer(Integer::from(1u64)), v.clone()]),
        Expr::Path(navs) => Value::Array(vec![
            Value::Integer(Integer::from(2u64)),
            Value::Array(navs.iter().map(nav_to_value).collect()),
        ]),
    }
}

fn expr_from_value(v: &Value) -> Result<Expr> {
    let Value::Array(items) = v else {
        return Err(Error::Query("expr must be a tagged array".into()));
    };
    let tag = items
        .first()
        .and_then(as_u64)
        .ok_or_else(|| Error::Query("expr missing tag".into()))?;
    match tag {
        0 => match items.get(1) {
            Some(Value::Text(s)) => Ok(Expr::Field(s.clone())),
            _ => Err(Error::Query("Field expr needs a name".into())),
        },
        1 => items
            .get(1)
            .cloned()
            .map(Expr::Lit)
            .ok_or_else(|| Error::Query("Lit needs a value".into())),
        2 => {
            let Value::Array(navs) = items
                .get(1)
                .ok_or_else(|| Error::Query("Path expr needs navs".into()))?
            else {
                return Err(Error::Query("Path expr needs navs".into()));
            };
            Ok(Expr::Path(
                navs.iter().map(nav_from_value).collect::<Result<_>>()?,
            ))
        }
        other => Err(Error::Query(format!("unknown expr tag {other}"))),
    }
}

fn opt_u64(n: Option<u64>) -> Value {
    match n {
        Some(n) => Value::Integer(Integer::from(n)),
        None => Value::Null,
    }
}

fn as_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Integer(i) => u64::try_from(*i).ok(),
        _ => None,
    }
}

fn as_opt_u64(v: &Value) -> Option<u64> {
    match v {
        Value::Null => None,
        other => as_u64(other),
    }
}

fn nav_label(nav: &Nav) -> String {
    match nav {
        Nav::Field(n) => format!("Field {n}"),
        Nav::Key(v) => format!("Key {}", value_label(v)),
        Nav::Index(i) => format!("Index {i}"),
        Nav::All => "All".into(),
        Nav::Values => "Values".into(),
        Nav::Keys => "Keys".into(),
        Nav::Entries => "Entries".into(),
        Nav::Where(p) => format!("Where {}", pred_label(p)),
        Nav::First => "First".into(),
        Nav::Last => "Last".into(),
        Nav::Slice { start, end } => format!("Slice {start:?}..{end:?}"),
    }
}

fn value_label(v: &Value) -> String {
    match v {
        Value::Text(s) => format!("{s:?}"),
        Value::Integer(i) => format!("{i:?}"),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".into(),
        Value::Float(f) => f.to_string(),
        _ => "<value>".into(),
    }
}

fn eq_field_hint(p: &Predicate) -> Option<&str> {
    match p {
        Predicate::Eq(Expr::Field(f), Expr::Lit(_)) | Predicate::Eq(Expr::Lit(_), Expr::Field(f)) => {
            Some(f.as_str())
        }
        _ => None,
    }
}

fn pred_label(p: &Predicate) -> String {
    match p {
        Predicate::Eq(a, b) => format!("{} == {}", expr_label(a), expr_label(b)),
        Predicate::Ne(a, b) => format!("{} != {}", expr_label(a), expr_label(b)),
        Predicate::Lt(a, b) => format!("{} < {}", expr_label(a), expr_label(b)),
        Predicate::Le(a, b) => format!("{} <= {}", expr_label(a), expr_label(b)),
        Predicate::Gt(a, b) => format!("{} > {}", expr_label(a), expr_label(b)),
        Predicate::Ge(a, b) => format!("{} >= {}", expr_label(a), expr_label(b)),
        Predicate::And(ps) => ps
            .iter()
            .map(pred_label)
            .collect::<Vec<_>>()
            .join(" and "),
        Predicate::Or(ps) => ps.iter().map(pred_label).collect::<Vec<_>>().join(" or "),
        Predicate::Not(p) => format!("not ({})", pred_label(p)),
        Predicate::Exists(navs) => format!(
            "exists {}",
            navs.iter().map(nav_label).collect::<Vec<_>>().join(".")
        ),
    }
}

fn expr_label(e: &Expr) -> String {
    match e {
        Expr::Field(n) => n.clone(),
        Expr::Lit(v) => value_label(v),
        Expr::Path(navs) => navs.iter().map(nav_label).collect::<Vec<_>>().join("."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_cbor_roundtrip() {
        let q = Query::namespaced(
            "constitution-v1",
            vec![
                Nav::Field("emissions".into()),
                Nav::All,
                Nav::Field("distributions".into()),
                Nav::Key(Value::Text("tommy-mor".into())),
            ],
        );
        let bytes = q.to_cbor().unwrap();
        assert_eq!(Query::from_cbor(&bytes).unwrap(), q);
    }

    #[test]
    fn where_cbor_roundtrip() {
        let q = Query::new(vec![Nav::Field("events".into()), Nav::Where(Predicate::Gt(
            Expr::Field("epoch".into()),
            Expr::Lit(Value::Integer(Integer::from(4u64))),
        ))]);
        let bytes = q.to_cbor().unwrap();
        assert_eq!(Query::from_cbor(&bytes).unwrap(), q);
    }
}
