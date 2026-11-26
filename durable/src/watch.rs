//! Typed key-range invalidation for reactive subscribers.
//!
//! Every [`crate::Path`] lowers to a [`WatchRange`] naming its table and key
//! interval, and every [`crate::Tx`] records the [`WatchRange`]s touched by its
//! mutations. Checking whether a commit could affect a watched path is a pure
//! interval overlap check in both directions (a write *above* a watched path —
//! e.g. deleting a parent user record while watching one of its fields — still
//! hits).

use crate::codec;

/// A watched or modified region of the database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WatchRange {
    /// Every table in the database (database root).
    All,
    /// Every table under a namespace (`"{ns}/..."`).
    Namespace(String),
    /// Half-open key interval `[lo, hi)` within a single table (`hi = None`
    /// runs to the end of the table).
    Range {
        table: String,
        lo: Vec<u8>,
        hi: Option<Vec<u8>>,
    },
}

impl WatchRange {
    /// Matches every table in the database.
    pub fn all() -> Self {
        Self::All
    }

    /// Matches every table belonging to `namespace`.
    pub fn namespace(ns: impl Into<String>) -> Self {
        Self::Namespace(ns.into())
    }

    /// Matches an entire table.
    pub fn table(table: impl Into<String>) -> Self {
        Self::Range {
            table: table.into(),
            lo: Vec::new(),
            hi: None,
        }
    }

    /// Matches every key prefixed by `prefix` within `table`.
    pub fn prefix(table: impl Into<String>, prefix: &[u8]) -> Self {
        let hi = codec::prefix_upper_bound(prefix);
        Self::Range {
            table: table.into(),
            lo: prefix.to_vec(),
            hi,
        }
    }

    /// Matches one exact key within `table`.
    pub fn point(table: impl Into<String>, key: &[u8]) -> Self {
        let mut hi = key.to_vec();
        hi.push(0);
        Self::Range {
            table: table.into(),
            lo: key.to_vec(),
            hi: Some(hi),
        }
    }

    /// Matches a custom half-open byte interval `[lo, hi)` within `table`.
    pub fn interval(table: impl Into<String>, lo: Vec<u8>, hi: Option<Vec<u8>>) -> Self {
        Self::Range {
            table: table.into(),
            lo,
            hi,
        }
    }

    /// Symmetric overlap check: returns `true` iff there exists at least one
    /// `(table, key)` that belongs to both ranges.
    pub fn overlaps(&self, other: &WatchRange) -> bool {
        match (self, other) {
            (WatchRange::All, _) | (_, WatchRange::All) => true,
            (WatchRange::Namespace(a), WatchRange::Namespace(b)) => a == b,
            (WatchRange::Namespace(ns), WatchRange::Range { table, .. })
            | (WatchRange::Range { table, .. }, WatchRange::Namespace(ns)) => {
                table_in_namespace(table, ns)
            }
            (
                WatchRange::Range {
                    table: t1,
                    lo: lo1,
                    hi: hi1,
                },
                WatchRange::Range {
                    table: t2,
                    lo: lo2,
                    hi: hi2,
                },
            ) => {
                if t1 != t2 {
                    return false;
                }
                let a_before_b_end = hi2.as_ref().map_or(true, |h| lo1 < h);
                let b_before_a_end = hi1.as_ref().map_or(true, |h| lo2 < h);
                a_before_b_end && b_before_a_end
            }
        }
    }
}

pub(crate) fn table_in_namespace(table: &str, ns: &str) -> bool {
    table
        .strip_prefix(ns)
        .is_some_and(|rest| rest.starts_with('/'))
}

/// Did any touched range in a commit overlap `watch`?
pub fn watch_hit(watch: &WatchRange, touched: &[WatchRange]) -> bool {
    touched.iter().any(|w| w.overlaps(watch))
}

/// Two prefix ranges either nest or are disjoint — pure byte-prefix overlap.
pub fn prefix_overlap(a: &[u8], b: &[u8]) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_and_prefix_overlap_both_directions() {
        let user_bob = WatchRange::prefix("users", b"bob\0\0");
        let bob_banned = WatchRange::point("users", b"bob\0\0\x01\x01");
        let bobby_banned = WatchRange::point("users", b"bobby\0\0\x01\x01");
        let other_table = WatchRange::point("votes", b"bob\0\0");

        assert!(user_bob.overlaps(&bob_banned));
        assert!(bob_banned.overlaps(&user_bob));
        assert!(!user_bob.overlaps(&bobby_banned));
        assert!(!user_bob.overlaps(&other_table));
    }

    #[test]
    fn namespace_and_all_overlap() {
        let ns = WatchRange::namespace("tenant_a");
        let a_users = WatchRange::point("tenant_a/users", b"alice\0\0");
        let b_users = WatchRange::point("tenant_b/users", b"alice\0\0");
        assert!(ns.overlaps(&a_users));
        assert!(!ns.overlaps(&b_users));
        assert!(WatchRange::all().overlaps(&b_users));
    }
}
