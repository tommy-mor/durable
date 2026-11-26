//! Low-level key framing and order-preserving byte primitives.
//!
//! Every key in a `durable` table is built from prefix-disjoint,
//! order-preserving encodings (`crate::key::Key`). Within a non-empty parent
//! prefix `P`, a one-byte discriminator separates child data from metadata:
//!
//! - `P` (exact)          → a [`crate::Leaf`] or [`crate::Sum`] scalar value.
//! - `P ++ [DATA] ++ seg` → a child (map entry, set member, struct field,
//!                          list/deque element).
//! - `P ++ [META] ++ seg` → reserved for collection metadata.
//!
//! At a table's root (`P` empty), child keys sit directly at `seg` with no
//! leading discriminator, so top-level flat collections map 1:1 onto `redb`
//! table entries and inherit O(1) `table.len()` and O(1) `delete_table()`.

/// On-disk projection format version. Bumped whenever key/value framing
/// changes so [`crate::ProjectionStamp`] triggers an automatic rebuild.
pub const FORMAT_VERSION: u32 = 2;

/// Discriminator for child data living under a non-empty parent prefix.
pub const DATA: u8 = 0x01;

/// Reserved discriminator for metadata living under a non-empty parent prefix.
pub const META: u8 = 0x00;

/// Append an unsigned LEB128 varint to `out`.
///
/// Self-delimiting and prefix-disjoint: the terminating byte has bit 7 clear,
/// while every preceding byte has bit 7 set.
pub fn put_uvarint(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let mut byte = (value & 0x7f) as u8;
        value >>= 7;
        if value != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if value == 0 {
            break;
        }
    }
}

/// Decode an unsigned LEB128 varint from the front of `bytes`.
///
/// Returns `(value, bytes_consumed)`, or `None` if truncated or overlong.
pub fn read_uvarint(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    for (i, &byte) in bytes.iter().enumerate() {
        if shift >= 64 {
            return None;
        }
        result |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((result, i + 1));
        }
        shift += 7;
    }
    None
}

/// Encode a `#[derive(Durable)]` struct field id as a self-delimiting segment.
pub fn field_segment(field_id: u32) -> Vec<u8> {
    let mut seg = Vec::with_capacity(5);
    put_uvarint(&mut seg, field_id as u64);
    seg
}

/// Build the key for child `seg` under location prefix `parent`.
///
/// When `parent` is empty (root of a dedicated table), `seg` is the entire key;
/// otherwise `[DATA]` separates `parent` from `seg`.
pub fn child_key(parent: &[u8], seg: &[u8]) -> Vec<u8> {
    if parent.is_empty() {
        return seg.to_vec();
    }
    let mut key = Vec::with_capacity(parent.len() + 1 + seg.len());
    key.extend_from_slice(parent);
    key.push(DATA);
    key.extend_from_slice(seg);
    key
}

/// The prefix under which all of `parent`'s child data lives.
pub fn child_scan_prefix(parent: &[u8]) -> Vec<u8> {
    if parent.is_empty() {
        return Vec::new();
    }
    let mut key = Vec::with_capacity(parent.len() + 1);
    key.extend_from_slice(parent);
    key.push(DATA);
    key
}

/// Build a metadata key `name` under location prefix `parent`.
pub fn meta_key(parent: &[u8], name: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(parent.len() + 1 + name.len() + 2);
    key.extend_from_slice(parent);
    key.push(META);
    encode_escaped_bytes(name, &mut key);
    key
}

/// Smallest key strictly greater than every key prefixed by `prefix`.
///
/// Returns `None` when `prefix` is empty or all `0xff` (the range runs to the
/// end of the keyspace).
pub fn prefix_upper_bound(prefix: &[u8]) -> Option<Vec<u8>> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.last_mut() {
        if *last != 0xff {
            *last += 1;
            return Some(end);
        }
        end.pop();
    }
    None
}

/// Order-preserving encoding of an `i64` (sign bit flipped, big-endian).
#[inline]
pub fn order_i64(index: i64) -> [u8; 8] {
    ((index as u64) ^ (1u64 << 63)).to_be_bytes()
}

/// Inverse of [`order_i64`].
#[inline]
pub fn decode_order_i64(bytes: [u8; 8]) -> i64 {
    (u64::from_be_bytes(bytes) ^ (1u64 << 63)) as i64
}

/// Order-preserving encoding of a `u64` (big-endian).
#[inline]
pub fn order_u64(index: u64) -> [u8; 8] {
    index.to_be_bytes()
}

/// Inverse of [`order_u64`].
#[inline]
pub fn decode_order_u64(bytes: [u8; 8]) -> u64 {
    u64::from_be_bytes(bytes)
}

/// Append a byte slice in lexicographically ordered, prefix-disjoint form:
/// every embedded `0x00` becomes `0x00 0xFF`, and the slice is terminated by
/// `0x00 0x00`.
///
/// Using a two-byte terminator `0x00 0x00` (rather than single `0x00`) ensures
/// that `"a"` (`61 00 00`) is never a byte-prefix of `"a\0"` (`61 00 ff 00 00`),
/// so subtree operations (`DeletePrefix`, `contains`, `watch`) stay sound even
/// when keys contain NUL bytes, while preserving exact byte order:
/// `"a" < "a"/child < "a\0" < "a\x01" < "ab"`.
pub fn encode_escaped_bytes(bytes: &[u8], out: &mut Vec<u8>) {
    out.reserve(bytes.len() + 2);
    for &b in bytes {
        if b == 0x00 {
            out.push(0x00);
            out.push(0xff);
        } else {
            out.push(b);
        }
    }
    out.push(0x00);
    out.push(0x00);
}

/// Append a byte prefix in escaped form WITHOUT the terminating `0x00 0x00`,
/// suitable for prefix range scans (`"tom"` matches `"tom"`, `"tommy"`, `"tom\0"`).
pub fn encode_escaped_prefix(bytes: &[u8], out: &mut Vec<u8>) {
    out.reserve(bytes.len());
    for &b in bytes {
        if b == 0x00 {
            out.push(0x00);
            out.push(0xff);
        } else {
            out.push(b);
        }
    }
}

/// Decode one escaped, `0x00 0x00`-terminated byte slice from the front of
/// `bytes`. Returns `(decoded_bytes, bytes_consumed)`.
pub fn decode_escaped_bytes(bytes: &[u8]) -> Option<(Vec<u8>, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x00 {
            let next = *bytes.get(i + 1)?;
            match next {
                0x00 => return Some((out, i + 2)),
                0xff => {
                    out.push(0x00);
                    i += 2;
                }
                _ => return None,
            }
        } else {
            out.push(b);
            i += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uvarint_roundtrip() {
        for value in [0u64, 1, 127, 128, 300, 16384, u32::MAX as u64, u64::MAX] {
            let mut buf = Vec::new();
            put_uvarint(&mut buf, value);
            let (decoded, used) = read_uvarint(&buf).unwrap();
            assert_eq!(decoded, value);
            assert_eq!(used, buf.len());
        }
    }

    #[test]
    fn read_uvarint_rejects_truncated() {
        assert!(read_uvarint(&[0x80]).is_none());
        assert!(read_uvarint(&[]).is_none());
    }

    #[test]
    fn escaped_bytes_order_and_prefix_disjoint_with_nuls() {
        let samples: &[&[u8]] = &[
            b"",
            b"\x00",
            b"\x00\x00",
            b"\x00a",
            b"\x01",
            b"a",
            b"a\x00",
            b"a\x00b",
            b"a\x01",
            b"ab",
            b"zz",
        ];
        let mut encoded: Vec<Vec<u8>> = Vec::new();
        for s in samples {
            let mut buf = Vec::new();
            encode_escaped_bytes(s, &mut buf);
            let (dec, used) = decode_escaped_bytes(&buf).unwrap();
            assert_eq!(dec, *s);
            assert_eq!(used, buf.len());
            encoded.push(buf);
        }
        // Exact lexicographic order
        for pair in encoded.windows(2) {
            assert!(pair[0] < pair[1], "{:?} not < {:?}", pair[0], pair[1]);
        }
        // Strict prefix disjointness (even "a" vs "a\0")
        for (i, a) in encoded.iter().enumerate() {
            for (j, b) in encoded.iter().enumerate() {
                if i != j {
                    assert!(
                        !b.starts_with(a),
                        "sample {i} ({:?}) is a prefix of sample {j} ({:?})",
                        samples[i],
                        samples[j]
                    );
                }
            }
        }
    }

    #[test]
    fn upper_bound_basics() {
        assert_eq!(prefix_upper_bound(&[1, 2, 3]), Some(vec![1, 2, 4]));
        assert_eq!(prefix_upper_bound(&[1, 2, 0xff]), Some(vec![1, 3]));
        assert_eq!(prefix_upper_bound(&[0xff, 0xff]), None);
        assert_eq!(prefix_upper_bound(&[]), None);
    }

    #[test]
    fn order_i64_is_monotonic_and_roundtrips() {
        let mut values = [-5i64, -1, 0, 1, 5, i64::MIN, i64::MAX];
        values.sort();
        for &v in &values {
            assert_eq!(decode_order_i64(order_i64(v)), v);
        }
        for pair in values.windows(2) {
            assert!(order_i64(pair[0]) < order_i64(pair[1]));
        }
    }
}
