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
    TypeOf,
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
    StoreHas,
    // pure: terminal navigator constructors (store.set(v), store.add(n), …)
    NavSet,
    NavAdd,
    NavPush,
    // pure: parametrized collecting navigators
    NavWhere,
    NavSlice,
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
    /// A module native (modules.rs), named for dispatch and mounting.
    /// Like Native: VM-local, not data. Both builds compile the same
    /// module set, so a name means the same thing on every side.
    Lib(&'static str),
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
            Value::Native(_) | Value::Lib(_) => "native",
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
            Value::Lib(_) => 11,
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
            (Value::Lib(a), Value::Lib(b)) => a.cmp(b),
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
            Value::Lib(name) => write!(f, "#native({name})"),
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn map_of(entries: Vec<(&str, Value)>) -> Value {
        Value::map(
            entries
                .into_iter()
                .map(|(k, v)| (Value::str(k), v))
                .collect::<BTreeMap<_, _>>(),
        )
    }

    fn roundtrip(v: &Value) -> Value {
        let bytes = encode(v).expect("encodable");
        decode(&bytes).expect("decodable")
    }

    /// encode ∘ decode = id over every data kind. This is the isomorphism
    /// the wire, the tape boundary, and copy-at-hop all stand on: if it
    /// breaks, packets and stored leaves stop meaning the same thing.
    #[test]
    fn roundtrip_every_data_kind() {
        let vs = vec![
            Value::Nil,
            Value::Bool(true),
            Value::Bool(false),
            Value::Int(-7),
            Value::Int(0),
            Value::Int(i64::MAX),
            Value::Float(2.5),
            Value::str("hi"),
            Value::str(""),
            Value::Bytes(Rc::from(b"\x00\xffhi".as_slice())),
            Value::array(vec![Value::Int(1), Value::str("x"), Value::Nil]),
            Value::array(vec![]),
            map_of(vec![("b", Value::Int(1)), ("a", Value::array(vec![Value::Bool(true)]))]),
            Value::map(BTreeMap::new()),
            Value::tagged("inst", Value::Int(1756345678123)),
            Value::tagged("tournament/match", map_of(vec![("w", Value::Nil)])),
        ];
        for v in &vs {
            assert_eq!(&roundtrip(v), v, "roundtrip failed for {v}");
        }
    }

    /// int and float are distinct kinds on the wire: `1` and `1.0` encode
    /// differently and decode distinctly. Mixed numeric comparison is a
    /// runtime semantic, never an encoding collapse.
    #[test]
    fn int_and_float_stay_distinct_on_the_wire() {
        let i = encode(&Value::Int(1)).unwrap();
        let f = encode(&Value::Float(1.0)).unwrap();
        assert_ne!(i, f);
        assert_eq!(decode(&i).unwrap(), Value::Int(1));
        assert_eq!(decode(&f).unwrap(), Value::Float(1.0));
    }

    /// Canonical bytes: the same logical map encodes identically however
    /// it was built, and decoding wire-ordered keys normalizes to sorted
    /// order. Equal values ⇒ equal bytes is what makes the wire a copy
    /// (not a serialization of a struct) and iteration deterministic.
    #[test]
    fn encoding_is_canonical_over_key_order() {
        let a = map_of(vec![("a", Value::Int(2)), ("b", Value::Int(1))]);
        let b = map_of(vec![("b", Value::Int(1)), ("a", Value::Int(2))]);
        assert_eq!(a, b);
        assert_eq!(encode(&a).unwrap(), encode(&b).unwrap());

        // wire order {"b":1,"a":2} → canonical {"a":2,"b":1}
        let wire = [0xA2, 0x61, 0x62, 0x01, 0x61, 0x61, 0x02];
        let v = decode(&wire).expect("wire map decodes");
        assert_eq!(v, a);
        assert_eq!(
            encode(&v).unwrap(),
            vec![0xA2, 0x61, 0x61, 0x02, 0x61, 0x62, 0x01]
        );
    }

    /// NaN is not a value: rejected at construction and at decode.
    #[test]
    fn nan_is_rejected_at_construction_and_decode() {
        assert!(Value::float(f64::NAN).is_err());
        // 0xFB 7FF8000000000000: f64 NaN on the wire
        let nan = [0xFB, 0x7F, 0xF8, 0, 0, 0, 0, 0, 0];
        assert!(decode(&nan).is_err());
    }

    /// Integers wider than i64 are not Hop ints.
    #[test]
    fn integers_outside_i64_are_rejected() {
        // 0x1B + eight 0xFF: u64::MAX on the wire
        let big = [0x1B, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        assert!(decode(&big).is_err());
    }

    /// Map keys must be scalar; aggregate keys are a decode error.
    #[test]
    fn non_scalar_map_keys_are_rejected() {
        let bad = ciborium::Value::Map(vec![(
            ciborium::Value::Array(vec![ciborium::Value::Integer(1.into())]),
            ciborium::Value::Null,
        )]);
        assert!(from_cbor(&bad).is_err());
    }

    /// Only tag 27 (the generic extension envelope) is understood; native
    /// CBOR tags are a future interop translation, not today's wire.
    #[test]
    fn unknown_cbor_tags_are_rejected() {
        let dated = ciborium::Value::Tag(0, Box::new(ciborium::Value::Text("x".into())));
        assert!(from_cbor(&dated).is_err());
    }

    /// Closures and natives are VM-local: never data, never encodable.
    /// This is the type-level side of "closures never cross the wire."
    #[test]
    fn vm_local_values_do_not_encode() {
        let closure = Value::Closure(Rc::new(ClosureVal { fn_idx: 0, caps: vec![] }));
        assert!(to_cbor(&closure).is_err());
        assert!(to_cbor(&Value::Native(NativeId::Print)).is_err());
        assert!(to_cbor(&Value::Lib("json.encode")).is_err());
    }
}
