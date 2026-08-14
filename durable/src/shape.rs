//! Runtime schema description.
//!
//! Compile-time [`crate::Schema`] types are enough for typed Rust navigators.
//! The query engine — and any language on the other side of a serialization
//! boundary — needs the same information as a value: field names, collection
//! kinds, nesting. [`Shape`] is that value. [`Describe`] produces it.
//!
//! The wire encoding is a small tagged-array CBOR form shared with the Python
//! frontend (see [`Shape::to_cbor`] / [`Shape::from_cbor`]).

use crate::{codec, Error, Result};

/// Runtime description of a durable location's shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Shape {
    Leaf,
    Sum,
    Map { of: Box<Shape> },
    List { of: Box<Shape> },
    Deque { of: Box<Shape> },
    Record { fields: Vec<(String, Shape)> },
}

impl Shape {
    pub fn map(of: Shape) -> Self {
        Self::Map { of: Box::new(of) }
    }

    pub fn list(of: Shape) -> Self {
        Self::List { of: Box::new(of) }
    }

    pub fn deque(of: Shape) -> Self {
        Self::Deque { of: Box::new(of) }
    }

    pub fn record(fields: Vec<(String, Shape)>) -> Self {
        Self::Record { fields }
    }

    /// Look up a record field by name. Returns `(declaration_order_id, shape)`.
    pub fn field(&self, name: &str) -> Result<(u32, &Shape)> {
        match self {
            Shape::Record { fields } => fields
                .iter()
                .enumerate()
                .find(|(_, (n, _))| n == name)
                .map(|(i, (_, s))| (i as u32, s))
                .ok_or_else(|| Error::Query(format!("unknown field {name:?}"))),
            _ => Err(Error::Query(format!(
                "field {name:?} requested on a {}",
                self.kind_name()
            ))),
        }
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Shape::Leaf => "Leaf",
            Shape::Sum => "Sum",
            Shape::Map { .. } => "Map",
            Shape::List { .. } => "List",
            Shape::Deque { .. } => "Deque",
            Shape::Record { .. } => "Record",
        }
    }

    pub fn is_collection(&self) -> bool {
        matches!(self, Shape::Map { .. } | Shape::List { .. } | Shape::Deque { .. })
    }

    pub fn element(&self) -> Result<&Shape> {
        match self {
            Shape::Map { of } | Shape::List { of } | Shape::Deque { of } => Ok(of),
            _ => Err(Error::Query(format!(
                "collection op on a {}",
                self.kind_name()
            ))),
        }
    }

    /// Tagged-array CBOR: `[0]` Leaf, `[1]` Sum, `[2, of]` Map, `[3, of]` List,
    /// `[4, of]` Deque, `[5, [[name, shape], ...]]` Record.
    pub fn to_cbor(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        ciborium::ser::into_writer(&self.to_wire(), &mut bytes)
            .map_err(|e| Error::Serialize(e.to_string()))?;
        Ok(bytes)
    }

    pub fn from_cbor(bytes: &[u8]) -> Result<Self> {
        let wire: Wire =
            ciborium::de::from_reader(bytes).map_err(|e| Error::Deserialize(e.to_string()))?;
        Self::from_wire(&wire)
    }

    fn to_wire(&self) -> Wire {
        match self {
            Shape::Leaf => Wire::Arr(vec![Wire::N(0)]),
            Shape::Sum => Wire::Arr(vec![Wire::N(1)]),
            Shape::Map { of } => Wire::Arr(vec![Wire::N(2), of.to_wire()]),
            Shape::List { of } => Wire::Arr(vec![Wire::N(3), of.to_wire()]),
            Shape::Deque { of } => Wire::Arr(vec![Wire::N(4), of.to_wire()]),
            Shape::Record { fields } => Wire::Arr(vec![
                Wire::N(5),
                Wire::Arr(
                    fields
                        .iter()
                        .map(|(n, s)| Wire::Arr(vec![Wire::T(n.clone()), s.to_wire()]))
                        .collect(),
                ),
            ]),
        }
    }

    fn from_wire(wire: &Wire) -> Result<Self> {
        let Wire::Arr(items) = wire else {
            return Err(Error::Query("shape must be a tagged array".into()));
        };
        let tag = items
            .first()
            .and_then(Wire::as_u64)
            .ok_or_else(|| Error::Query("shape missing tag".into()))?;
        match tag {
            0 => Ok(Shape::Leaf),
            1 => Ok(Shape::Sum),
            2 => {
                let of = items
                    .get(1)
                    .ok_or_else(|| Error::Query("Map shape missing element".into()))?;
                Ok(Shape::map(Self::from_wire(of)?))
            }
            3 => {
                let of = items
                    .get(1)
                    .ok_or_else(|| Error::Query("List shape missing element".into()))?;
                Ok(Shape::list(Self::from_wire(of)?))
            }
            4 => {
                let of = items
                    .get(1)
                    .ok_or_else(|| Error::Query("Deque shape missing element".into()))?;
                Ok(Shape::deque(Self::from_wire(of)?))
            }
            5 => {
                let Wire::Arr(fields) = items
                    .get(1)
                    .ok_or_else(|| Error::Query("Record shape missing fields".into()))?
                else {
                    return Err(Error::Query("Record fields must be an array".into()));
                };
                let mut out = Vec::with_capacity(fields.len());
                for f in fields {
                    let Wire::Arr(pair) = f else {
                        return Err(Error::Query("Record field must be [name, shape]".into()));
                    };
                    let name = match pair.first() {
                        Some(Wire::T(s)) => s.clone(),
                        _ => return Err(Error::Query("Record field name must be text".into())),
                    };
                    let shape = pair
                        .get(1)
                        .ok_or_else(|| Error::Query("Record field missing shape".into()))?;
                    out.push((name, Self::from_wire(shape)?));
                }
                Ok(Shape::record(out))
            }
            other => Err(Error::Query(format!("unknown shape tag {other}"))),
        }
    }
}

/// Minimal CBOR wire value used only for schema/query encodings we control.
#[derive(Debug, Clone)]
enum Wire {
    N(u64),
    T(String),
    Arr(Vec<Wire>),
}

impl Wire {
    fn as_u64(&self) -> Option<u64> {
        match self {
            Wire::N(n) => Some(*n),
            _ => None,
        }
    }
}

impl serde::Serialize for Wire {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            Wire::N(n) => ser.serialize_u64(*n),
            Wire::T(s) => ser.serialize_str(s),
            Wire::Arr(xs) => xs.serialize(ser),
        }
    }
}

impl<'de> serde::Deserialize<'de> for Wire {
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> serde::de::Visitor<'de> for V {
            type Value = Wire;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a query-encoding CBOR value")
            }
            fn visit_u64<E>(self, v: u64) -> std::result::Result<Wire, E> {
                Ok(Wire::N(v))
            }
            fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Wire, E> {
                if v < 0 {
                    return Err(E::custom("negative tag"));
                }
                Ok(Wire::N(v as u64))
            }
            fn visit_str<E>(self, v: &str) -> std::result::Result<Wire, E> {
                Ok(Wire::T(v.to_string()))
            }
            fn visit_string<E>(self, v: String) -> std::result::Result<Wire, E> {
                Ok(Wire::T(v))
            }
            fn visit_seq<A: serde::de::SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Wire, A::Error> {
                let mut xs = Vec::new();
                while let Some(item) = seq.next_element()? {
                    xs.push(item);
                }
                Ok(Wire::Arr(xs))
            }
        }
        de.deserialize_any(V)
    }
}

/// Types that can describe their durable shape at runtime.
pub trait Describe {
    fn shape() -> Shape;
}

impl<T> Describe for crate::Leaf<T> {
    fn shape() -> Shape {
        Shape::Leaf
    }
}

impl<N: crate::Summable> Describe for crate::Sum<N> {
    fn shape() -> Shape {
        Shape::Sum
    }
}

impl<K, V: Describe> Describe for crate::Map<K, V> {
    fn shape() -> Shape {
        Shape::map(V::shape())
    }
}

impl<V: Describe> Describe for crate::List<V> {
    fn shape() -> Shape {
        Shape::list(V::shape())
    }
}

impl<V: Describe> Describe for crate::Deque<V> {
    fn shape() -> Shape {
        Shape::deque(V::shape())
    }
}

/// Encode a record field id the same way typed navigators do.
pub(crate) fn field_segment(field_id: u32) -> Vec<u8> {
    let mut seg = Vec::new();
    codec::put_uvarint(&mut seg, field_id as u64);
    seg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shape_cbor_roundtrip() {
        let shape = Shape::record(vec![
            ("events".into(), Shape::list(Shape::Leaf)),
            ("evidence_by_id".into(), Shape::map(Shape::Leaf)),
            ("count".into(), Shape::Sum),
        ]);
        let bytes = shape.to_cbor().unwrap();
        assert_eq!(Shape::from_cbor(&bytes).unwrap(), shape);
    }
}
