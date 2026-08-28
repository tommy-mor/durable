//! Hop values — the normative model is durable/docs/hop-values.md.
//!
//! One representation in memory, on the wire, and at the store boundary.
//! Closed set of kinds; `tagged` is the only extension mechanism; closures
//! are VM-local and never data. NaN is not a value.

use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::rc::Rc;

/// Built-in native functions. Pure ones are dispatched inside the
/// interpreter; contextual ones (store, dom, hui) go to the Host.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NativeId {
    // pure
    Print,
    Error,
    Tostring,
    Tonumber,
    Push,
    Len,
    SortBy,
    Floor,
    // pure: schema shape constructors (store.map(of), store.record(fields), …)
    ShapeMap,
    ShapeList,
    ShapeDeque,
    ShapeRecord,
    // context: the store — one callable. store(path) is a query when the
    // path only navigates, a mutation when it ends in a terminal navigator
    // (reducers only). Everything else on the module is a constant or a
    // pure constructor resolved by field access on this native.
    StoreCall,
    StoreAppend,
    StoreItems,
    StoreVerify,
    // pure: terminal navigator constructors (store.set(v), store.add(n), …)
    NavSet,
    NavAdd,
    NavPush,
    // context: browser side
    DomGet,
    DomSet,
    DomClear,
    DomFocus,
    HuiRender,
}

#[derive(Clone)]
pub struct ClosureVal {
    pub fn_idx: usize,
    pub caps: Vec<Value>,
}

#[derive(Clone)]
pub enum Value {
    Nil,
    Bool(bool),
    Int(i64),
    /// Never NaN — constructors and decoders reject it.
    Float(f64),
    Str(Rc<str>),
    Bytes(Rc<[u8]>),
    Array(Rc<RefCell<Vec<Value>>>),
    Map(Rc<RefCell<BTreeMap<Value, Value>>>),
    Tagged(Rc<(String, Value)>),
    /// VM-local, not data: identity equality, unordered, unserializable.
    Closure(Rc<ClosureVal>),
    Native(NativeId),
}

impl Value {
    pub fn str(s: impl Into<Rc<str>>) -> Value {
        Value::Str(s.into())
    }

    pub fn float(f: f64) -> Result<Value, String> {
        if f.is_nan() {
            Err("NaN is not a value".into())
        } else {
            Ok(Value::Float(f))
        }
    }

    pub fn array(items: Vec<Value>) -> Value {
        Value::Array(Rc::new(RefCell::new(items)))
    }

    pub fn map(entries: BTreeMap<Value, Value>) -> Value {
        Value::Map(Rc::new(RefCell::new(entries)))
    }

    pub fn empty_map() -> Value {
        Value::map(BTreeMap::new())
    }

    pub fn tagged(tag: impl Into<String>, payload: Value) -> Value {
        Value::Tagged(Rc::new((tag.into(), payload)))
    }

    pub fn truthy(&self) -> bool {
        !matches!(self, Value::Nil | Value::Bool(false))
    }

    /// Scalar = permitted as a map key.
    pub fn is_scalar(&self) -> bool {
        match self {
            Value::Nil
            | Value::Bool(_)
            | Value::Int(_)
            | Value::Float(_)
            | Value::Str(_)
            | Value::Bytes(_) => true,
            Value::Tagged(t) => t.1.is_scalar(),
            _ => false,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Value::Nil => "nil",
            Value::Bool(_) => "bool",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Str(_) => "string",
            Value::Bytes(_) => "bytes",
            Value::Array(_) => "array",
            Value::Map(_) => "map",
            Value::Tagged(_) => "tagged",
            Value::Closure(_) => "closure",
            Value::Native(_) => "native",
        }
    }

    fn kind_rank(&self) -> u8 {
        match self {
            Value::Nil => 0,
            Value::Bool(_) => 1,
            Value::Int(_) => 2,
            Value::Float(_) => 3,
            Value::Str(_) => 4,
            Value::Bytes(_) => 5,
            Value::Array(_) => 6,
            Value::Map(_) => 7,
            Value::Tagged(_) => 8,
            Value::Closure(_) => 9,
            Value::Native(_) => 10,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Map field access by string key; Nil when absent or not a map.
    pub fn get_field(&self, name: &str) -> Value {
        match self {
            Value::Map(m) => m
                .borrow()
                .get(&Value::str(name))
                .cloned()
                .unwrap_or(Value::Nil),
            _ => Value::Nil,
        }
    }

    pub fn set_field(&self, name: &str, v: Value) -> Result<(), String> {
        match self {
            Value::Map(m) => {
                if matches!(v, Value::Nil) {
                    m.borrow_mut().remove(&Value::str(name));
                } else {
                    m.borrow_mut().insert(Value::str(name), v);
                }
                Ok(())
            }
            other => Err(format!("cannot set field on {}", other.kind())),
        }
    }
}

/// Total order per hop-values.md: kind rank, then within-kind. Closures
/// and natives order by identity — they cannot be map keys (enforced at
/// insertion), so this never affects observable determinism.
impl Ord for Value {
    fn cmp(&self, other: &Self) -> Ordering {
        let (ra, rb) = (self.kind_rank(), other.kind_rank());
        if ra != rb {
            return ra.cmp(&rb);
        }
        match (self, other) {
            (Value::Nil, Value::Nil) => Ordering::Equal,
            (Value::Bool(a), Value::Bool(b)) => a.cmp(b),
            (Value::Int(a), Value::Int(b)) => a.cmp(b),
            // no NaN: partial_cmp is total here
            (Value::Float(a), Value::Float(b)) => a.partial_cmp(b).unwrap(),
            (Value::Str(a), Value::Str(b)) => a.cmp(b),
            (Value::Bytes(a), Value::Bytes(b)) => a.cmp(b),
            (Value::Array(a), Value::Array(b)) => {
                if Rc::ptr_eq(a, b) {
                    return Ordering::Equal;
                }
                a.borrow().iter().cmp(b.borrow().iter())
            }
            (Value::Map(a), Value::Map(b)) => {
                if Rc::ptr_eq(a, b) {
                    return Ordering::Equal;
                }
                a.borrow().iter().cmp(b.borrow().iter())
            }
            (Value::Tagged(a), Value::Tagged(b)) => {
                a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1))
            }
            (Value::Closure(a), Value::Closure(b)) => {
                (Rc::as_ptr(a) as usize).cmp(&(Rc::as_ptr(b) as usize))
            }
            (Value::Native(a), Value::Native(b)) => format!("{a:?}").cmp(&format!("{b:?}")),
            _ => unreachable!("kind ranks matched"),
        }
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Value {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Value {}

/// Deep copy — what a placement boundary does (copy-at-hop).
pub fn deep_copy(v: &Value) -> Value {
    match v {
        Value::Array(a) => Value::array(a.borrow().iter().map(deep_copy).collect()),
        Value::Map(m) => Value::map(
            m.borrow()
                .iter()
                .map(|(k, v)| (deep_copy(k), deep_copy(v)))
                .collect(),
        ),
        Value::Tagged(t) => Value::tagged(t.0.clone(), deep_copy(&t.1)),
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Diagnostics (log mode) — EDN-ish, deterministic
// ---------------------------------------------------------------------------

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self}")
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Nil => write!(f, "nil"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => {
                // always show a float as a float in diagnostics
                if x.fract() == 0.0 && x.is_finite() {
                    write!(f, "{x:.1}")
                } else {
                    write!(f, "{x}")
                }
            }
            Value::Str(s) => write!(f, "{:?}", &**s),
            Value::Bytes(b) => write!(f, "#bytes({})", b.len()),
            Value::Array(a) => {
                write!(f, "[")?;
                for (i, v) in a.borrow().iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{v}")?;
                }
                write!(f, "]")
            }
            Value::Map(m) => {
                write!(f, "{{")?;
                for (i, (k, v)) in m.borrow().iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    match k {
                        Value::Str(s) => write!(f, "{}: {v}", &**s)?,
                        other => write!(f, "{other}: {v}")?,
                    }
                }
                write!(f, "}}")
            }
            Value::Tagged(t) => write!(f, "#{} {}", t.0, t.1),
            Value::Closure(c) => write!(f, "#closure(fn {})", c.fn_idx),
            Value::Native(id) => write!(f, "#native({id:?})"),
        }
    }
}

/// The string coercion used by `tostring` and `..`: unquoted strings,
/// otherwise diagnostic notation. Deterministic across platforms.
pub fn coerce_str(v: &Value) -> String {
    match v {
        Value::Str(s) => s.to_string(),
        other => format!("{other}"),
    }
}

// ---------------------------------------------------------------------------
// CBOR — the one encoding (wire; store leaves are CBOR via durable)
// ---------------------------------------------------------------------------

const TAG_GENERIC: u64 = 27; // RFC 8949: [type-name, args...]

pub fn to_cbor(v: &Value) -> Result<ciborium::Value, String> {
    Ok(match v {
        Value::Nil => ciborium::Value::Null,
        Value::Bool(b) => ciborium::Value::Bool(*b),
        Value::Int(i) => ciborium::Value::Integer((*i).into()),
        Value::Float(f) => ciborium::Value::Float(*f),
        Value::Str(s) => ciborium::Value::Text(s.to_string()),
        Value::Bytes(b) => ciborium::Value::Bytes(b.to_vec()),
        Value::Array(a) => ciborium::Value::Array(
            a.borrow().iter().map(to_cbor).collect::<Result<_, _>>()?,
        ),
        Value::Map(m) => ciborium::Value::Map(
            m.borrow()
                .iter()
                .map(|(k, v)| Ok((to_cbor(k)?, to_cbor(v)?)))
                .collect::<Result<_, String>>()?,
        ),
        Value::Tagged(t) => ciborium::Value::Tag(
            TAG_GENERIC,
            Box::new(ciborium::Value::Array(vec![
                ciborium::Value::Text(t.0.clone()),
                to_cbor(&t.1)?,
            ])),
        ),
        other => return Err(format!("{} is not data and cannot be encoded", other.kind())),
    })
}

pub fn from_cbor(v: &ciborium::Value) -> Result<Value, String> {
    Ok(match v {
        ciborium::Value::Null => Value::Nil,
        ciborium::Value::Bool(b) => Value::Bool(*b),
        ciborium::Value::Integer(i) => {
            Value::Int(i128::from(*i).try_into().map_err(|_| "integer out of i64 range")?)
        }
        ciborium::Value::Float(f) => Value::float(*f)?,
        ciborium::Value::Text(s) => Value::str(s.as_str()),
        ciborium::Value::Bytes(b) => Value::Bytes(Rc::from(b.as_slice())),
        ciborium::Value::Array(xs) => {
            Value::array(xs.iter().map(from_cbor).collect::<Result<_, _>>()?)
        }
        ciborium::Value::Map(entries) => {
            let mut m = BTreeMap::new();
            for (k, v) in entries {
                let k = from_cbor(k)?;
                if !k.is_scalar() {
                    return Err(format!("map key must be scalar, got {}", k.kind()));
                }
                m.insert(k, from_cbor(v)?);
            }
            Value::map(m)
        }
        ciborium::Value::Tag(TAG_GENERIC, inner) => match inner.as_ref() {
            ciborium::Value::Array(parts) if parts.len() == 2 => {
                let tag = match &parts[0] {
                    ciborium::Value::Text(t) => t.clone(),
                    _ => return Err("tag name must be text".into()),
                };
                Value::tagged(tag, from_cbor(&parts[1])?)
            }
            _ => return Err("tag 27 payload must be [name, value]".into()),
        },
        ciborium::Value::Tag(n, _) => return Err(format!("unknown CBOR tag {n}")),
        other => return Err(format!("unsupported CBOR item: {other:?}")),
    })
}

pub fn encode(v: &Value) -> Result<Vec<u8>, String> {
    let c = to_cbor(v)?;
    let mut out = Vec::new();
    ciborium::into_writer(&c, &mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

pub fn decode(bytes: &[u8]) -> Result<Value, String> {
    let c: ciborium::Value = ciborium::from_reader(bytes).map_err(|e| e.to_string())?;
    from_cbor(&c)
}

// ---------------------------------------------------------------------------
// JSON boundary — the tape is JSONL; events are the JSON-safe subset
// ---------------------------------------------------------------------------

pub fn to_json(v: &Value) -> Result<serde_json::Value, String> {
    Ok(match v {
        Value::Nil => serde_json::Value::Null,
        Value::Bool(b) => serde_json::Value::Bool(*b),
        Value::Int(i) => serde_json::Value::Number((*i).into()),
        Value::Float(f) => serde_json::Number::from_f64(*f)
            .map(serde_json::Value::Number)
            .ok_or("non-finite float in JSON boundary")?,
        Value::Str(s) => serde_json::Value::String(s.to_string()),
        Value::Array(a) => serde_json::Value::Array(
            a.borrow().iter().map(to_json).collect::<Result<_, _>>()?,
        ),
        Value::Map(m) => {
            let mut o = serde_json::Map::new();
            for (k, v) in m.borrow().iter() {
                let key = match k {
                    Value::Str(s) => s.to_string(),
                    Value::Int(i) => i.to_string(),
                    other => return Err(format!("JSON map key must be string/int, got {}", other.kind())),
                };
                o.insert(key, to_json(v)?);
            }
            serde_json::Value::Object(o)
        }
        other => return Err(format!("{} cannot cross the JSON boundary", other.kind())),
    })
}

pub fn from_json(v: &serde_json::Value) -> Value {
    match v {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Value::Int(i)
            } else {
                Value::Float(n.as_f64().unwrap_or(0.0))
            }
        }
        serde_json::Value::String(s) => Value::str(s.as_str()),
        serde_json::Value::Array(xs) => Value::array(xs.iter().map(from_json).collect()),
        serde_json::Value::Object(o) => Value::map(
            o.iter()
                .map(|(k, v)| (Value::str(k.as_str()), from_json(v)))
                .collect(),
        ),
    }
}
