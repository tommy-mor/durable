//! Key encoding for durable paths.
//!
//! Every durable location lowers to a deterministic RocksDB key built from
//! *length-prefixed segments*. A segment is `uvarint(len) ++ bytes`, which makes
//! segment sequences self-delimiting: no segment can be a prefix of a *different*
//! segment, so sibling subtrees never overlap and a parent prefix only ever
//! prefixes its own descendants.
//!
//! Within a location prefix `P` we reserve a one-byte discriminator:
//!
//! - `P` (exact)            → a [`crate::Leaf`] scalar value lives here.
//! - `P ++ [DATA] ++ seg`   → a child (map entry, struct field, list element).
//! - `P ++ [META] ++ seg`   → collection metadata (e.g. a list length).
//!
//! Because children and metadata live *under* `P`, deleting a whole subtree is a
//! single RocksDB range delete over `[P, prefix_upper_bound(P))`.

/// Discriminator for child data living under a location.
pub const DATA: u8 = 0x01;
/// Discriminator for collection metadata living under a location.
pub const META: u8 = 0x00;

/// Append an unsigned LEB128 varint to `out`.
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
/// Returns the value and the number of bytes consumed, or `None` if the input is
/// truncated or overlong.
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

/// Append a length-prefixed segment to `out`.
pub fn put_segment(out: &mut Vec<u8>, bytes: &[u8]) {
    put_uvarint(out, bytes.len() as u64);
    out.extend_from_slice(bytes);
}

/// Read one length-prefixed segment from the front of `bytes`.
///
/// Returns the segment payload and the total number of bytes consumed
/// (including the length prefix).
pub fn read_segment(bytes: &[u8]) -> Option<(&[u8], usize)> {
    let (len, header) = read_uvarint(bytes)?;
    let len = len as usize;
    let end = header.checked_add(len)?;
    if end > bytes.len() {
        return None;
    }
    Some((&bytes[header..end], header + len))
}

/// Build the key for child `seg` under location prefix `parent`.
pub fn child_key(parent: &[u8], seg: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(parent.len() + 2 + seg.len());
    key.extend_from_slice(parent);
    key.push(DATA);
    put_segment(&mut key, seg);
    key
}

/// The prefix under which all of `parent`'s child data lives.
pub fn child_scan_prefix(parent: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(parent.len() + 1);
    key.extend_from_slice(parent);
    key.push(DATA);
    key
}

/// Build a metadata key `name` under location prefix `parent`.
pub fn meta_key(parent: &[u8], name: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(parent.len() + 2 + name.len());
    key.extend_from_slice(parent);
    key.push(META);
    put_segment(&mut key, name);
    key
}

/// Smallest key strictly greater than every key prefixed by `prefix`.
///
/// Returns `None` when `prefix` is empty or all `0xff` (i.e. the range extends to
/// the end of the keyspace), in which case callers must fall back to a scan.
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

/// Order-preserving encoding of an `i64` index (used by [`crate::Deque`]).
///
/// Flipping the sign bit makes the unsigned big-endian byte order match signed
/// numeric order, so negative front indices sort before positive ones.
pub fn order_i64(index: i64) -> [u8; 8] {
    ((index as u64) ^ (1u64 << 63)).to_be_bytes()
}

/// Order-preserving encoding of a `u64` index (used by [`crate::List`]).
pub fn order_u64(index: u64) -> [u8; 8] {
    index.to_be_bytes()
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
    fn segment_roundtrip_and_self_delimiting() {
        let mut buf = Vec::new();
        put_segment(&mut buf, b"alpha");
        put_segment(&mut buf, b"");
        put_segment(&mut buf, &[0x00, 0xff, 0x01]);

        let (a, n1) = read_segment(&buf).unwrap();
        assert_eq!(a, b"alpha");
        let (b, n2) = read_segment(&buf[n1..]).unwrap();
        assert_eq!(b, b"");
        let (c, _) = read_segment(&buf[n1 + n2..]).unwrap();
        assert_eq!(c, &[0x00, 0xff, 0x01]);
    }

    #[test]
    fn segment_no_false_prefix() {
        // seg("a") must not be a byte-prefix of seg("ab"): length-prefixing guards this.
        let mut a = Vec::new();
        put_segment(&mut a, b"a");
        let mut ab = Vec::new();
        put_segment(&mut ab, b"ab");
        assert!(!ab.starts_with(&a));
    }

    #[test]
    fn upper_bound_basics() {
        assert_eq!(prefix_upper_bound(&[1, 2, 3]), Some(vec![1, 2, 4]));
        assert_eq!(prefix_upper_bound(&[1, 2, 0xff]), Some(vec![1, 3]));
        assert_eq!(prefix_upper_bound(&[0xff, 0xff]), None);
        assert_eq!(prefix_upper_bound(&[]), None);
    }

    #[test]
    fn order_i64_is_monotonic() {
        let mut values = [-5i64, -1, 0, 1, 5, i64::MIN, i64::MAX];
        values.sort();
        // Encoded byte order must match signed numeric order.
        for pair in values.windows(2) {
            assert!(order_i64(pair[0]) < order_i64(pair[1]));
        }
    }
}
