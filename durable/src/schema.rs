//! Type-level schema markers and structural fingerprinting.
//!
//! A *schema* describes the shape of a durable location at the type level. It is
//! never instantiated; it parameterizes a [`crate::Path`] so the compiler knows
//! which navigation steps and operations are legal, and provides a deterministic
//! [`Schema::fingerprint`] so [`crate::Runtime::open_with`] detects schema
//! changes automatically.
//!
//! - [`Leaf<T>`] — a single CBOR-encoded scalar value.
//! - [`Sum<N>`] — a numeric accumulator (`i64`, `u64`, `f64`).
//! - [`Map<K, V>`] — ordered map from [`crate::Key`] `K` to sub-schema `V`.
//! - [`Set<K>`] — ordered set of [`crate::Key`] `K` (0-byte values).
//! - [`List<V>`] — index-addressed sequence of sub-schema `V`.
//! - [`Deque<V>`] — double-ended queue of sub-schema `V` (O(1) ends).
//! - any `#[derive(Durable)]` struct — fixed named fields (top-level fields map
//!   to dedicated `redb` tables; nested structs live inline under their parent's
//!   table prefix).

use std::marker::PhantomData;

use sha2::{Digest as _, Sha256};

use crate::codec::FORMAT_VERSION;
use crate::key::Key;

/// Marker trait implemented by every durable schema.
pub trait Schema {
    /// `true` for scalar schemas (`Leaf`, `Sum`) that live in `"__scalars"`
    /// when placed at the root of a store rather than getting their own table.
    const IS_SCALAR: bool = false;

    /// `true` when every entry occupies exactly one physical key (no child
    /// sub-keys). Enables O(1) `table.len()` and non-deduping range scans on
    /// `Map<K, V>` / `List<V>` / `Deque<V>`.
    const IS_FLAT: bool = false;

    /// Append a canonical structural description of this schema to `out`.
    fn encode_schema(out: &mut Vec<u8>);

    /// Stable 64-bit structural fingerprint of this schema (including
    /// [`FORMAT_VERSION`], key types, field names, and leaf type names).
    fn fingerprint() -> u64 {
        let mut bytes = b"durable.schema.v2\0".to_vec();
        bytes.extend_from_slice(&FORMAT_VERSION.to_be_bytes());
        Self::encode_schema(&mut bytes);
        let digest = Sha256::digest(&bytes);
        u64::from_be_bytes(digest[..8].try_into().expect("8 bytes"))
    }
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u64).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

/// A single CBOR-encoded scalar value of type `T`.
pub struct Leaf<T>(PhantomData<T>);
impl<T: 'static> Schema for Leaf<T> {
    const IS_SCALAR: bool = true;
    const IS_FLAT: bool = true;

    fn encode_schema(out: &mut Vec<u8>) {
        out.push(0);
        put_str(out, std::any::type_name::<T>());
    }
}

/// A numeric accumulator (`i64`, `u64`, `f64`).
pub struct Sum<N>(PhantomData<N>);
impl<N: Summable> Schema for Sum<N> {
    const IS_SCALAR: bool = true;
    const IS_FLAT: bool = true;

    fn encode_schema(out: &mut Vec<u8>) {
        out.push(1);
        out.push(N::TAG);
    }
}

/// An ordered map from keys of type `K` to sub-schema `V`.
pub struct Map<K, V>(PhantomData<(K, V)>);
impl<K: Key, V: Schema> Schema for Map<K, V> {
    fn encode_schema(out: &mut Vec<u8>) {
        out.push(2);
        put_str(out, &K::key_type_name());
        V::encode_schema(out);
    }
}

/// An ordered set of keys of type `K` (stored with 0-byte values).
pub struct Set<K>(PhantomData<K>);
impl<K: Key> Schema for Set<K> {
    fn encode_schema(out: &mut Vec<u8>) {
        out.push(6);
        put_str(out, &K::key_type_name());
    }
}

/// An index-addressed growable sequence of sub-schema `V`.
pub struct List<V>(PhantomData<V>);
impl<V: Schema> Schema for List<V> {
    fn encode_schema(out: &mut Vec<u8>) {
        out.push(3);
        V::encode_schema(out);
    }
}

/// A double-ended queue of sub-schema `V` with O(1) push/pop at both ends.
pub struct Deque<V>(PhantomData<V>);
impl<V: Schema> Schema for Deque<V> {
    fn encode_schema(out: &mut Vec<u8>) {
        out.push(4);
        V::encode_schema(out);
    }
}

/// Numbers that can back a [`Sum`] accumulator.
///
/// Stored on disk as `[TAG, b0..b7]`: a one-byte type tag plus the 8-byte
/// little-endian payload.
pub trait Summable: Copy + 'static {
    const TAG: u8;
    fn zero() -> Self;
    fn combine(self, other: Self) -> Self;
    fn to_le_payload(self) -> [u8; 8];
    fn from_le_payload(bytes: [u8; 8]) -> Self;
}

impl Summable for f64 {
    const TAG: u8 = 0;
    fn zero() -> Self {
        0.0
    }
    fn combine(self, other: Self) -> Self {
        self + other
    }
    fn to_le_payload(self) -> [u8; 8] {
        self.to_le_bytes()
    }
    fn from_le_payload(bytes: [u8; 8]) -> Self {
        f64::from_le_bytes(bytes)
    }
}

impl Summable for i64 {
    const TAG: u8 = 1;
    fn zero() -> Self {
        0
    }
    fn combine(self, other: Self) -> Self {
        self.wrapping_add(other)
    }
    fn to_le_payload(self) -> [u8; 8] {
        self.to_le_bytes()
    }
    fn from_le_payload(bytes: [u8; 8]) -> Self {
        i64::from_le_bytes(bytes)
    }
}

impl Summable for u64 {
    const TAG: u8 = 2;
    fn zero() -> Self {
        0
    }
    fn combine(self, other: Self) -> Self {
        self.wrapping_add(other)
    }
    fn to_le_payload(self) -> [u8; 8] {
        self.to_le_bytes()
    }
    fn from_le_payload(bytes: [u8; 8]) -> Self {
        u64::from_le_bytes(bytes)
    }
}

/// Encode a `Summable` to its tagged on-disk form `[TAG, b0..b7]`.
pub(crate) fn encode_sum<N: Summable>(value: N) -> [u8; 9] {
    let mut out = [0u8; 9];
    out[0] = N::TAG;
    out[1..9].copy_from_slice(&value.to_le_payload());
    out
}

/// Decode a tagged accumulator payload back to `N`, validating the tag.
pub(crate) fn decode_sum<N: Summable>(bytes: &[u8]) -> Option<N> {
    if bytes.len() != 9 || bytes[0] != N::TAG {
        return None;
    }
    let mut payload = [0u8; 8];
    payload.copy_from_slice(&bytes[1..9]);
    Some(N::from_le_payload(payload))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_roundtrip_tagged() {
        assert_eq!(decode_sum::<f64>(&encode_sum(2.5f64)), Some(2.5));
        assert_eq!(decode_sum::<i64>(&encode_sum(-7i64)), Some(-7));
        assert_eq!(decode_sum::<u64>(&encode_sum(9u64)), Some(9));
        assert_eq!(decode_sum::<i64>(&encode_sum(2.5f64)), None);
    }

    #[test]
    fn fingerprint_distinguishes_key_and_leaf_and_sum_types() {
        assert_ne!(
            <Map<String, Leaf<i64>> as Schema>::fingerprint(),
            <Map<i64, Leaf<i64>> as Schema>::fingerprint(),
        );
        assert_ne!(
            <Map<String, Leaf<i64>> as Schema>::fingerprint(),
            <Map<String, Leaf<String>> as Schema>::fingerprint(),
        );
        assert_ne!(
            <Sum<i64> as Schema>::fingerprint(),
            <Sum<f64> as Schema>::fingerprint(),
        );
        assert_ne!(
            <Set<String> as Schema>::fingerprint(),
            <Map<String, Leaf<()>> as Schema>::fingerprint(),
        );
    }
}
