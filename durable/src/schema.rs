//! Type-level schema markers.
//!
//! A *schema* describes the shape of a durable location at the type level. It is
//! never instantiated; it only parameterizes a [`crate::Path`] so the compiler
//! knows which navigation steps and terminal operations are legal.
//!
//! - [`Leaf<T>`] — a single CBOR-encoded scalar value.
//! - [`Map<K, V>`] — keys of type `K` to sub-schema `V`.
//! - [`List<V>`] — an index-addressed sequence of sub-schema `V`.
//! - [`Deque<V>`] — a double-ended queue of sub-schema `V` (O(1) ends).
//! - [`Sum<N>`] — a numeric accumulator updated with blind merge writes.
//! - any `#[derive(Durable)]` struct — a fixed set of named fields.

use std::marker::PhantomData;

/// Marker trait implemented by every durable schema.
///
/// Implemented for [`Leaf`], [`Map`], [`List`], [`Deque`], [`Sum`], and by
/// `#[derive(Durable)]` for user structs. It is intentionally minimal; behaviour
/// lives on `Path<S>` impls keyed by the concrete schema.
pub trait Schema {}

/// A single CBOR-encoded scalar value of type `T`.
pub struct Leaf<T>(PhantomData<T>);
impl<T> Schema for Leaf<T> {}

/// A map from keys of type `K` to sub-schema `V`.
pub struct Map<K, V>(PhantomData<(K, V)>);
impl<K, V: Schema> Schema for Map<K, V> {}

/// An index-addressed growable sequence of sub-schema `V`.
pub struct List<V>(PhantomData<V>);
impl<V: Schema> Schema for List<V> {}

/// A double-ended queue of sub-schema `V` with O(1) push/pop at both ends.
pub struct Deque<V>(PhantomData<V>);
impl<V: Schema> Schema for Deque<V> {}

/// A numeric accumulator. Updated with blind, associative merge writes so
/// incrementing is O(1) and never reads the current value.
pub struct Sum<N>(PhantomData<N>);
impl<N: Summable> Schema for Sum<N> {}

/// Numbers that can back a [`Sum`] accumulator.
///
/// Stored on disk as `[TAG, b0..b7]`: a one-byte type tag plus the 8-byte
/// little-endian payload. The tag lets a single RocksDB merge operator fold
/// `f64` and `i64` accumulators correctly.
pub trait Summable: Copy + 'static {
    /// Disk type tag, unique per numeric type.
    const TAG: u8;
    /// Additive identity.
    fn zero() -> Self;
    /// Combine two values (sum).
    fn combine(self, other: Self) -> Self;
    /// Little-endian 8-byte payload.
    fn to_le_payload(self) -> [u8; 8];
    /// Decode from a little-endian 8-byte payload.
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
pub(crate) fn encode_sum<N: Summable>(value: N) -> Vec<u8> {
    let mut out = Vec::with_capacity(9);
    out.push(N::TAG);
    out.extend_from_slice(&value.to_le_payload());
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

/// Fold one tagged operand into a running tagged accumulator.
///
/// Used by the RocksDB merge operator. Operands of mismatched tags are skipped
/// rather than panicking, keeping compaction resilient to stray bytes.
fn fold_tagged(acc: &mut Option<[u8; 9]>, operand: &[u8]) {
    if operand.len() != 9 {
        return;
    }
    let tag = operand[0];
    let mut op_payload = [0u8; 8];
    op_payload.copy_from_slice(&operand[1..9]);

    match acc {
        Some(existing) if existing[0] == tag => {
            let mut acc_payload = [0u8; 8];
            acc_payload.copy_from_slice(&existing[1..9]);
            let combined = match tag {
                0 => f64::from_le_bytes(acc_payload)
                    .combine(f64::from_le_bytes(op_payload))
                    .to_le_payload(),
                1 => i64::from_le_bytes(acc_payload)
                    .combine(i64::from_le_bytes(op_payload))
                    .to_le_payload(),
                2 => u64::from_le_bytes(acc_payload)
                    .combine(u64::from_le_bytes(op_payload))
                    .to_le_payload(),
                _ => return,
            };
            existing[1..9].copy_from_slice(&combined);
        }
        Some(_) => {} // tag mismatch: ignore stray operand
        None => {
            let mut start = [0u8; 9];
            start[0] = tag;
            start[1..9].copy_from_slice(&op_payload);
            *acc = Some(start);
        }
    }
}

/// Associative merge operator registered on every durable database so that
/// [`Sum`] accumulators can be incremented with blind `merge` writes.
pub(crate) fn sum_merge(
    _key: &[u8],
    existing: Option<&[u8]>,
    operands: &rocksdb::MergeOperands,
) -> Option<Vec<u8>> {
    let mut acc: Option<[u8; 9]> = None;
    if let Some(existing) = existing {
        if existing.len() == 9 {
            let mut start = [0u8; 9];
            start.copy_from_slice(existing);
            acc = Some(start);
        }
    }
    for operand in operands.iter() {
        fold_tagged(&mut acc, operand);
    }
    acc.map(|bytes| bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sum_roundtrip_tagged() {
        assert_eq!(decode_sum::<f64>(&encode_sum(2.5f64)), Some(2.5));
        assert_eq!(decode_sum::<i64>(&encode_sum(-7i64)), Some(-7));
        assert_eq!(decode_sum::<u64>(&encode_sum(9u64)), Some(9));
        // Tag mismatch is rejected.
        assert_eq!(decode_sum::<i64>(&encode_sum(2.5f64)), None);
    }

    #[test]
    fn fold_accumulates_same_tag() {
        let mut acc = None;
        fold_tagged(&mut acc, &encode_sum(1.5f64));
        fold_tagged(&mut acc, &encode_sum(2.0f64));
        let bytes = acc.unwrap();
        assert_eq!(decode_sum::<f64>(&bytes), Some(3.5));
    }

    #[test]
    fn fold_skips_mismatched_tag() {
        let mut acc = None;
        fold_tagged(&mut acc, &encode_sum(5i64));
        fold_tagged(&mut acc, &encode_sum(1.0f64)); // ignored
        let bytes = acc.unwrap();
        assert_eq!(decode_sum::<i64>(&bytes), Some(5));
    }
}
