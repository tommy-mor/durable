//! Ordered, prefix-disjoint binary key encoding for typed Rust maps and sets.
//!
//! Every type implementing [`Key`] encodes to bytes whose lexicographic order
//! matches the type's natural [`Ord`] order, and is self-delimiting and
//! prefix-disjoint so keys can be:
//!
//! - concatenated into composite tuple keys `(A, B)`, `(A, B, C)`, `(A, B, C, D)`;
//! - range-scanned in both directions (`range`, `first`, `last`, `page`);
//! - prefix-scanned (`"tom"` on `String`, `("github",)` on `(String, String)`);
//! - used as prefixes of nested subtrees without colliding with siblings
//!   (even for strings containing NUL bytes).
//!
//! Standard types supported out of the box:
//! - `()`, `bool`
//! - `u8`, `u16`, `u32`, `u64`, `u128`
//! - `i8`, `i16`, `i32`, `i64`, `i128`
//! - [`OrderedF64`] (IEEE 754 total-order `f64` for scores/rankings)
//! - `String` (and `&str` / `str` for zero-alloc lookups and prefix scans)
//! - `Vec<u8>` (and `&[u8]` / `[u8]` for zero-alloc lookups and prefix scans)
//! - [`std::cmp::Reverse<K>`] (descending index order)
//! - Tuples `(A, B)`, `(A, B, C)`, `(A, B, C, D)`

use std::cmp::{Ordering, Reverse};
use std::hash::{Hash, Hasher};
use std::ops::{
    Bound, Range, RangeFrom, RangeFull, RangeInclusive, RangeTo, RangeToInclusive,
};

use serde::{Deserialize, Serialize};

use crate::{codec, Error, Result};

/// A wrapper around `f64` with IEEE 754 total ordering (`f64::total_cmp`),
/// usable as a [`Key`] in ordered maps and leaderboards (e.g.
/// `Map<(Reverse<OrderedF64>, ItemId), Leaf<()>>`).
#[derive(Copy, Clone, Debug, Default, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OrderedF64(pub f64);

impl OrderedF64 {
    #[inline]
    pub const fn new(v: f64) -> Self {
        Self(v)
    }

    #[inline]
    pub const fn get(self) -> f64 {
        self.0
    }
}

impl From<f64> for OrderedF64 {
    #[inline]
    fn from(v: f64) -> Self {
        Self(v)
    }
}

impl From<OrderedF64> for f64 {
    #[inline]
    fn from(v: OrderedF64) -> Self {
        v.0
    }
}

impl PartialEq for OrderedF64 {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0) == Ordering::Equal
    }
}

impl Eq for OrderedF64 {}

impl PartialOrd for OrderedF64 {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedF64 {
    #[inline]
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl Hash for OrderedF64 {
    #[inline]
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

/// Encode a key (or borrowed lookup form of a key) into order-preserving bytes.
pub trait KeyEncode {
    fn encode_key(&self, out: &mut Vec<u8>);

    fn to_key_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        self.encode_key(&mut out);
        out
    }
}

/// An owned key type stored in a [`crate::Map`] or [`crate::Set`].
///
/// Encoding must be order-preserving, self-delimiting, and prefix-disjoint.
pub trait Key: KeyEncode + KeyLookup<Self> + Sized {
    /// Decode one key from the front of `bytes`, returning `(key, bytes_consumed)`.
    fn decode_key(bytes: &[u8]) -> Result<(Self, usize)>;

    /// Stable type identifier included in [`crate::Schema::fingerprint`].
    fn key_type_name() -> String;
}

/// Marks types `Self` that encode identically to `K` and can be used for
/// zero-allocation lookups and range bounds against `Map<K, V>` / `Set<K>`.
///
/// For example, `&str` implements `KeyLookup<String>`, and `(&str, i64)`
/// implements `KeyLookup<(String, i64)>`.
pub trait KeyLookup<K: Key>: KeyEncode {}

impl<T: KeyEncode + ?Sized> KeyEncode for &T {
    #[inline]
    fn encode_key(&self, out: &mut Vec<u8>) {
        (**self).encode_key(out);
    }
}

impl<K: Key, Q: KeyLookup<K> + ?Sized> KeyLookup<K> for &Q {}

// ---------------------------------------------------------------------------
// Primitive Key impls
// ---------------------------------------------------------------------------

impl KeyEncode for () {
    #[inline]
    fn encode_key(&self, _out: &mut Vec<u8>) {}
}

impl Key for () {
    #[inline]
    fn decode_key(_bytes: &[u8]) -> Result<(Self, usize)> {
        Ok(((), 0))
    }
    fn key_type_name() -> String {
        "()".into()
    }
}

impl KeyLookup<()> for () {}

impl KeyEncode for bool {
    #[inline]
    fn encode_key(&self, out: &mut Vec<u8>) {
        out.push(u8::from(*self));
    }
}

impl Key for bool {
    #[inline]
    fn decode_key(bytes: &[u8]) -> Result<(Self, usize)> {
        match bytes.first() {
            Some(0) => Ok((false, 1)),
            Some(1) => Ok((true, 1)),
            _ => Err(Error::Corruption("malformed bool key".into())),
        }
    }
    fn key_type_name() -> String {
        "bool".into()
    }
}

impl KeyLookup<bool> for bool {}

macro_rules! impl_unsigned_key {
    ($ty:ty, $size:expr, $name:expr) => {
        impl KeyEncode for $ty {
            #[inline]
            fn encode_key(&self, out: &mut Vec<u8>) {
                out.extend_from_slice(&self.to_be_bytes());
            }
        }
        impl Key for $ty {
            #[inline]
            fn decode_key(bytes: &[u8]) -> Result<(Self, usize)> {
                if bytes.len() < $size {
                    return Err(Error::Corruption(concat!("truncated ", $name, " key").into()));
                }
                let mut buf = [0u8; $size];
                buf.copy_from_slice(&bytes[..$size]);
                Ok((<$ty>::from_be_bytes(buf), $size))
            }
            fn key_type_name() -> String {
                $name.into()
            }
        }
        impl KeyLookup<$ty> for $ty {}
    };
}

impl_unsigned_key!(u8, 1, "u8");
impl_unsigned_key!(u16, 2, "u16");
impl_unsigned_key!(u32, 4, "u32");
impl_unsigned_key!(u64, 8, "u64");
impl_unsigned_key!(u128, 16, "u128");

macro_rules! impl_signed_key {
    ($ty:ty, $uty:ty, $size:expr, $bits:expr, $name:expr) => {
        impl KeyEncode for $ty {
            #[inline]
            fn encode_key(&self, out: &mut Vec<u8>) {
                let flipped = (*self as $uty) ^ (1 as $uty << ($bits - 1));
                out.extend_from_slice(&flipped.to_be_bytes());
            }
        }
        impl Key for $ty {
            #[inline]
            fn decode_key(bytes: &[u8]) -> Result<(Self, usize)> {
                if bytes.len() < $size {
                    return Err(Error::Corruption(concat!("truncated ", $name, " key").into()));
                }
                let mut buf = [0u8; $size];
                buf.copy_from_slice(&bytes[..$size]);
                let raw = <$uty>::from_be_bytes(buf) ^ (1 as $uty << ($bits - 1));
                Ok((raw as $ty, $size))
            }
            fn key_type_name() -> String {
                $name.into()
            }
        }
        impl KeyLookup<$ty> for $ty {}
    };
}

impl_signed_key!(i8, u8, 1, 8, "i8");
impl_signed_key!(i16, u16, 2, 16, "i16");
impl_signed_key!(i32, u32, 4, 32, "i32");
impl_signed_key!(i64, u64, 8, 64, "i64");
impl_signed_key!(i128, u128, 16, 128, "i128");

impl KeyEncode for OrderedF64 {
    #[inline]
    fn encode_key(&self, out: &mut Vec<u8>) {
        let bits = self.0.to_bits();
        let mask = if (bits >> 63) != 0 {
            u64::MAX
        } else {
            1u64 << 63
        };
        out.extend_from_slice(&(bits ^ mask).to_be_bytes());
    }
}

impl Key for OrderedF64 {
    #[inline]
    fn decode_key(bytes: &[u8]) -> Result<(Self, usize)> {
        if bytes.len() < 8 {
            return Err(Error::Corruption("truncated OrderedF64 key".into()));
        }
        let mut buf = [0u8; 8];
        buf.copy_from_slice(&bytes[..8]);
        let enc = u64::from_be_bytes(buf);
        let mask = if (enc >> 63) != 0 {
            1u64 << 63
        } else {
            u64::MAX
        };
        Ok((OrderedF64(f64::from_bits(enc ^ mask)), 8))
    }
    fn key_type_name() -> String {
        "OrderedF64".into()
    }
}

impl KeyLookup<OrderedF64> for OrderedF64 {}

// ---------------------------------------------------------------------------
// String and byte slice keys
// ---------------------------------------------------------------------------

impl KeyEncode for str {
    #[inline]
    fn encode_key(&self, out: &mut Vec<u8>) {
        codec::encode_escaped_bytes(self.as_bytes(), out);
    }
}

impl KeyEncode for String {
    #[inline]
    fn encode_key(&self, out: &mut Vec<u8>) {
        codec::encode_escaped_bytes(self.as_bytes(), out);
    }
}

impl Key for String {
    fn decode_key(bytes: &[u8]) -> Result<(Self, usize)> {
        let (raw, used) = codec::decode_escaped_bytes(bytes)
            .ok_or_else(|| Error::Corruption("malformed String key".into()))?;
        let s = String::from_utf8(raw)
            .map_err(|_| Error::Corruption("invalid UTF-8 in String key".into()))?;
        Ok((s, used))
    }
    fn key_type_name() -> String {
        "String".into()
    }
}

impl KeyLookup<String> for String {}
impl KeyLookup<String> for str {}

impl KeyEncode for [u8] {
    #[inline]
    fn encode_key(&self, out: &mut Vec<u8>) {
        codec::encode_escaped_bytes(self, out);
    }
}

impl KeyEncode for Vec<u8> {
    #[inline]
    fn encode_key(&self, out: &mut Vec<u8>) {
        codec::encode_escaped_bytes(self, out);
    }
}

impl Key for Vec<u8> {
    fn decode_key(bytes: &[u8]) -> Result<(Self, usize)> {
        codec::decode_escaped_bytes(bytes)
            .ok_or_else(|| Error::Corruption("malformed Vec<u8> key".into()))
    }
    fn key_type_name() -> String {
        "Vec<u8>".into()
    }
}

impl KeyLookup<Vec<u8>> for Vec<u8> {}
impl KeyLookup<Vec<u8>> for [u8] {}

// ---------------------------------------------------------------------------
// Reverse<K>
// ---------------------------------------------------------------------------

impl<K: KeyEncode> KeyEncode for Reverse<K> {
    fn encode_key(&self, out: &mut Vec<u8>) {
        let start = out.len();
        self.0.encode_key(out);
        for b in &mut out[start..] {
            *b = !*b;
        }
    }
}

impl<K: Key> Key for Reverse<K> {
    fn decode_key(bytes: &[u8]) -> Result<(Self, usize)> {
        let inverted: Vec<u8> = bytes.iter().map(|b| !*b).collect();
        let (inner, used) = K::decode_key(&inverted)?;
        Ok((Reverse(inner), used))
    }
    fn key_type_name() -> String {
        format!("Reverse<{}>", K::key_type_name())
    }
}

impl<K: Key, Q: KeyLookup<K>> KeyLookup<Reverse<K>> for Reverse<Q> {}

// ---------------------------------------------------------------------------
// Composite tuple keys
// ---------------------------------------------------------------------------

impl<A: KeyEncode, B: KeyEncode> KeyEncode for (A, B) {
    #[inline]
    fn encode_key(&self, out: &mut Vec<u8>) {
        self.0.encode_key(out);
        self.1.encode_key(out);
    }
}

impl<A: Key, B: Key> Key for (A, B) {
    fn decode_key(bytes: &[u8]) -> Result<(Self, usize)> {
        let (a, n1) = A::decode_key(bytes)?;
        let (b, n2) = B::decode_key(&bytes[n1..])?;
        Ok(((a, b), n1 + n2))
    }
    fn key_type_name() -> String {
        format!("({},{})", A::key_type_name(), B::key_type_name())
    }
}

impl<A: Key, B: Key, QA: KeyLookup<A>, QB: KeyLookup<B>> KeyLookup<(A, B)> for (QA, QB) {}

impl<A: KeyEncode, B: KeyEncode, C: KeyEncode> KeyEncode for (A, B, C) {
    #[inline]
    fn encode_key(&self, out: &mut Vec<u8>) {
        self.0.encode_key(out);
        self.1.encode_key(out);
        self.2.encode_key(out);
    }
}

impl<A: Key, B: Key, C: Key> Key for (A, B, C) {
    fn decode_key(bytes: &[u8]) -> Result<(Self, usize)> {
        let (a, n1) = A::decode_key(bytes)?;
        let (b, n2) = B::decode_key(&bytes[n1..])?;
        let (c, n3) = C::decode_key(&bytes[n1 + n2..])?;
        Ok(((a, b, c), n1 + n2 + n3))
    }
    fn key_type_name() -> String {
        format!(
            "({},{},{})",
            A::key_type_name(),
            B::key_type_name(),
            C::key_type_name()
        )
    }
}

impl<A: Key, B: Key, C: Key, QA: KeyLookup<A>, QB: KeyLookup<B>, QC: KeyLookup<C>>
    KeyLookup<(A, B, C)> for (QA, QB, QC)
{
}

impl<A: KeyEncode, B: KeyEncode, C: KeyEncode, D: KeyEncode> KeyEncode for (A, B, C, D) {
    #[inline]
    fn encode_key(&self, out: &mut Vec<u8>) {
        self.0.encode_key(out);
        self.1.encode_key(out);
        self.2.encode_key(out);
        self.3.encode_key(out);
    }
}

impl<A: Key, B: Key, C: Key, D: Key> Key for (A, B, C, D) {
    fn decode_key(bytes: &[u8]) -> Result<(Self, usize)> {
        let (a, n1) = A::decode_key(bytes)?;
        let (b, n2) = B::decode_key(&bytes[n1..])?;
        let (c, n3) = C::decode_key(&bytes[n1 + n2..])?;
        let (d, n4) = D::decode_key(&bytes[n1 + n2 + n3..])?;
        Ok(((a, b, c, d), n1 + n2 + n3 + n4))
    }
    fn key_type_name() -> String {
        format!(
            "({},{},{},{})",
            A::key_type_name(),
            B::key_type_name(),
            C::key_type_name(),
            D::key_type_name()
        )
    }
}

impl<
        A: Key,
        B: Key,
        C: Key,
        D: Key,
        QA: KeyLookup<A>,
        QB: KeyLookup<B>,
        QC: KeyLookup<C>,
        QD: KeyLookup<D>,
    > KeyLookup<(A, B, C, D)> for (QA, QB, QC, QD)
{
}

// ---------------------------------------------------------------------------
// KeyRange<K>: ergonomic range bounds without turbofish on `..`
// ---------------------------------------------------------------------------

/// Physical byte range bounds `(lo, hi)` for a scan over `K` under `scan_prefix`.
pub(crate) type ByteBounds = (Bound<Vec<u8>>, Bound<Vec<u8>>);

/// Range bounds over a [`Key`] type `K`, accepting borrowed lookup forms and
/// unbounded `..` without type annotations.
pub trait KeyRange<K: Key> {
    /// Lower logical bounds onto physical byte bounds under `scan_prefix`.
    /// Returns `None` when the range is guaranteed empty.
    fn to_byte_bounds(&self, scan_prefix: &[u8]) -> Option<ByteBounds>;
}

fn lower_bounds_from_pair<Q: KeyEncode + ?Sized>(
    scan_prefix: &[u8],
    start: Bound<&Q>,
    end: Bound<&Q>,
) -> Option<ByteBounds> {
    let lo = match start {
        Bound::Unbounded => {
            if scan_prefix.is_empty() {
                Bound::Unbounded
            } else {
                Bound::Included(scan_prefix.to_vec())
            }
        }
        Bound::Included(k) => {
            let mut key = scan_prefix.to_vec();
            k.encode_key(&mut key);
            Bound::Included(key)
        }
        Bound::Excluded(k) => {
            // Skip `k` AND all descendants under `k` (needed for nested maps).
            let mut key = scan_prefix.to_vec();
            k.encode_key(&mut key);
            let next = codec::prefix_upper_bound(&key)?;
            // If `next` escaped `scan_prefix`, the range is empty.
            if !scan_prefix.is_empty() && !next.starts_with(scan_prefix) {
                return None;
            }
            Bound::Included(next)
        }
    };
    let hi = match end {
        Bound::Unbounded => match codec::prefix_upper_bound(scan_prefix) {
            Some(u) => Bound::Excluded(u),
            None => Bound::Unbounded,
        },
        Bound::Excluded(k) => {
            let mut key = scan_prefix.to_vec();
            k.encode_key(&mut key);
            Bound::Excluded(key)
        }
        Bound::Included(k) => {
            // Include `k` AND all descendants under `k`.
            let mut key = scan_prefix.to_vec();
            k.encode_key(&mut key);
            match codec::prefix_upper_bound(&key) {
                Some(u) => {
                    if !scan_prefix.is_empty() && !u.starts_with(scan_prefix) {
                        match codec::prefix_upper_bound(scan_prefix) {
                            Some(su) => Bound::Excluded(su),
                            None => Bound::Unbounded,
                        }
                    } else {
                        Bound::Excluded(u)
                    }
                }
                None => Bound::Unbounded,
            }
        }
    };

    // Guard against inverted bounds (redb panics if start > end).
    if let (Bound::Included(l), Bound::Excluded(h)) = (&lo, &hi) {
        if l >= h {
            return None;
        }
    }
    Some((lo, hi))
}

impl<K: Key> KeyRange<K> for RangeFull {
    fn to_byte_bounds(&self, scan_prefix: &[u8]) -> Option<ByteBounds> {
        let lo = if scan_prefix.is_empty() {
            Bound::Unbounded
        } else {
            Bound::Included(scan_prefix.to_vec())
        };
        let hi = match codec::prefix_upper_bound(scan_prefix) {
            Some(u) => Bound::Excluded(u),
            None => Bound::Unbounded,
        };
        Some((lo, hi))
    }
}

impl<K: Key, Q: KeyLookup<K>> KeyRange<K> for Range<Q> {
    fn to_byte_bounds(&self, scan_prefix: &[u8]) -> Option<ByteBounds> {
        lower_bounds_from_pair(
            scan_prefix,
            Bound::Included(&self.start),
            Bound::Excluded(&self.end),
        )
    }
}

impl<K: Key, Q: KeyLookup<K>> KeyRange<K> for RangeInclusive<Q> {
    fn to_byte_bounds(&self, scan_prefix: &[u8]) -> Option<ByteBounds> {
        lower_bounds_from_pair(
            scan_prefix,
            Bound::Included(self.start()),
            Bound::Included(self.end()),
        )
    }
}

impl<K: Key, Q: KeyLookup<K>> KeyRange<K> for RangeFrom<Q> {
    fn to_byte_bounds(&self, scan_prefix: &[u8]) -> Option<ByteBounds> {
        lower_bounds_from_pair(scan_prefix, Bound::Included(&self.start), Bound::Unbounded)
    }
}

impl<K: Key, Q: KeyLookup<K>> KeyRange<K> for RangeTo<Q> {
    fn to_byte_bounds(&self, scan_prefix: &[u8]) -> Option<ByteBounds> {
        lower_bounds_from_pair(scan_prefix, Bound::Unbounded, Bound::Excluded(&self.end))
    }
}

impl<K: Key, Q: KeyLookup<K>> KeyRange<K> for RangeToInclusive<Q> {
    fn to_byte_bounds(&self, scan_prefix: &[u8]) -> Option<ByteBounds> {
        lower_bounds_from_pair(scan_prefix, Bound::Unbounded, Bound::Included(&self.end))
    }
}

impl<K: Key, Q: KeyLookup<K>> KeyRange<K> for (Bound<Q>, Bound<Q>) {
    fn to_byte_bounds(&self, scan_prefix: &[u8]) -> Option<ByteBounds> {
        lower_bounds_from_pair(scan_prefix, self.0.as_ref(), self.1.as_ref())
    }
}

// ---------------------------------------------------------------------------
// PrefixOf<K>: string prefixes and composite tuple prefixes
// ---------------------------------------------------------------------------

/// A prefix that matches a contiguous range of keys of type `K`.
///
/// - For `String`: `&str` / `String` matches every key starting with that string.
/// - For `Vec<u8>`: `&[u8]` / `Vec<u8>` matches every key starting with those bytes.
/// - For tuples `(A, B)`: `(a,)` matches every tuple whose first element is `a`.
/// - For `(A, B, C)`: `(a,)` and `(a, b)` match matching leading elements.
/// - For `(A, B, C, D)`: `(a,)`, `(a, b)`, and `(a, b, c)`.
pub trait PrefixOf<K: Key> {
    fn encode_prefix(&self, out: &mut Vec<u8>);
}

impl PrefixOf<String> for str {
    fn encode_prefix(&self, out: &mut Vec<u8>) {
        codec::encode_escaped_prefix(self.as_bytes(), out);
    }
}

impl PrefixOf<String> for &str {
    fn encode_prefix(&self, out: &mut Vec<u8>) {
        codec::encode_escaped_prefix(self.as_bytes(), out);
    }
}

impl PrefixOf<String> for String {
    fn encode_prefix(&self, out: &mut Vec<u8>) {
        codec::encode_escaped_prefix(self.as_bytes(), out);
    }
}

impl PrefixOf<String> for &String {
    fn encode_prefix(&self, out: &mut Vec<u8>) {
        codec::encode_escaped_prefix(self.as_bytes(), out);
    }
}

impl PrefixOf<Vec<u8>> for [u8] {
    fn encode_prefix(&self, out: &mut Vec<u8>) {
        codec::encode_escaped_prefix(self, out);
    }
}

impl PrefixOf<Vec<u8>> for &[u8] {
    fn encode_prefix(&self, out: &mut Vec<u8>) {
        codec::encode_escaped_prefix(self, out);
    }
}

impl PrefixOf<Vec<u8>> for Vec<u8> {
    fn encode_prefix(&self, out: &mut Vec<u8>) {
        codec::encode_escaped_prefix(self, out);
    }
}

impl PrefixOf<Vec<u8>> for &Vec<u8> {
    fn encode_prefix(&self, out: &mut Vec<u8>) {
        codec::encode_escaped_prefix(self, out);
    }
}

// Tuple leading-element prefixes
impl<A: Key, B: Key, QA: KeyLookup<A>> PrefixOf<(A, B)> for (QA,) {
    fn encode_prefix(&self, out: &mut Vec<u8>) {
        self.0.encode_key(out);
    }
}

impl<A: Key, B: Key, C: Key, QA: KeyLookup<A>> PrefixOf<(A, B, C)> for (QA,) {
    fn encode_prefix(&self, out: &mut Vec<u8>) {
        self.0.encode_key(out);
    }
}

impl<A: Key, B: Key, C: Key, QA: KeyLookup<A>, QB: KeyLookup<B>> PrefixOf<(A, B, C)> for (QA, QB) {
    fn encode_prefix(&self, out: &mut Vec<u8>) {
        self.0.encode_key(out);
        self.1.encode_key(out);
    }
}

impl<A: Key, B: Key, C: Key, D: Key, QA: KeyLookup<A>> PrefixOf<(A, B, C, D)> for (QA,) {
    fn encode_prefix(&self, out: &mut Vec<u8>) {
        self.0.encode_key(out);
    }
}

impl<A: Key, B: Key, C: Key, D: Key, QA: KeyLookup<A>, QB: KeyLookup<B>> PrefixOf<(A, B, C, D)>
    for (QA, QB)
{
    fn encode_prefix(&self, out: &mut Vec<u8>) {
        self.0.encode_key(out);
        self.1.encode_key(out);
    }
}

impl<A: Key, B: Key, C: Key, D: Key, QA: KeyLookup<A>, QB: KeyLookup<B>, QC: KeyLookup<C>>
    PrefixOf<(A, B, C, D)> for (QA, QB, QC)
{
    fn encode_prefix(&self, out: &mut Vec<u8>) {
        self.0.encode_key(out);
        self.1.encode_key(out);
        self.2.encode_key(out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signed_and_unsigned_order() {
        let mut xs = vec![i64::MIN, -1000, -1, 0, 1, 24, 1000, i64::MAX];
        xs.sort();
        for pair in xs.windows(2) {
            assert!(pair[0].to_key_bytes() < pair[1].to_key_bytes());
        }
        for x in xs {
            let b = x.to_key_bytes();
            let (dec, used) = i64::decode_key(&b).unwrap();
            assert_eq!((dec, used), (x, b.len()));
        }
    }

    #[test]
    fn ordered_f64_total_order() {
        let mut xs = vec![
            OrderedF64(f64::NEG_INFINITY),
            OrderedF64(-100.5),
            OrderedF64(-0.0),
            OrderedF64(0.0),
            OrderedF64(1e-10),
            OrderedF64(42.0),
            OrderedF64(f64::INFINITY),
        ];
        xs.sort();
        for pair in xs.windows(2) {
            assert!(pair[0].to_key_bytes() < pair[1].to_key_bytes());
        }
        for x in xs {
            let b = x.to_key_bytes();
            let (dec, used) = OrderedF64::decode_key(&b).unwrap();
            assert_eq!((dec, used), (x, b.len()));
        }
    }

    #[test]
    fn reverse_key_order_and_roundtrip() {
        let vals = ["", "a", "a\0", "ab", "zz"];
        let mut revs: Vec<Reverse<String>> = vals.iter().map(|s| Reverse(s.to_string())).collect();
        revs.sort();
        for pair in revs.windows(2) {
            assert!(pair[0].to_key_bytes() < pair[1].to_key_bytes());
        }
        for r in revs {
            let b = r.to_key_bytes();
            let (dec, used) = Reverse::<String>::decode_key(&b).unwrap();
            assert_eq!((dec, used), (r, b.len()));
        }
    }

    #[test]
    fn tuple_order_and_prefix() {
        let pairs = [
            ("github", "alice"),
            ("github", "bob"),
            ("google", "alice"),
        ];
        let enc: Vec<Vec<u8>> = pairs.iter().map(|p| p.to_key_bytes()).collect();
        assert!(enc[0] < enc[1]);
        assert!(enc[1] < enc[2]);

        let mut pref = Vec::new();
        <(&str,) as PrefixOf<(String, String)>>::encode_prefix(&("github",), &mut pref);
        assert!(enc[0].starts_with(&pref));
        assert!(enc[1].starts_with(&pref));
        assert!(!enc[2].starts_with(&pref));
    }
}
