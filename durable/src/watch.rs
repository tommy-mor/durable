//! The prefix-invalidation bridge: the load-bearing claim behind `store.wait`.
//!
//! A commit's write set and a watched path's key range overlap whenever the
//! write could change what that path reads. The schema *is* the dependency
//! graph; prefix overlap just reads it.
//!
//! Rules (hop-wait.md, proven in `tests/invalidation_lab.rs`):
//! 1. Watch whole subtrees, never the rows you saw — stop at the first
//!    collecting step (`where`, `first`, `slice`, `keys`, …), at a deque
//!    index, or on entering a leaf blob's interior.
//! 2. Check overlap in both directions — a write *above* the watcher
//!    (deleting `users/bob` while watching `users/bob/banned`) still hits.
//! 3. Only RocksDB is visible — server lets, fs, http never wake anyone.
//!
//! This module is the shipped form of the lab's strategy A. The lab keeps
//! proving it; this module is what the courier calls.

use crate::{dynpath, Nav, Op, Shape, Write};

/// Half-open key interval `[lo, hi)`; `hi = None` means "to the end".
#[derive(Clone, Debug)]
pub struct WatchRange {
    lo: Vec<u8>,
    hi: Option<Vec<u8>>,
}

fn upper(p: &[u8]) -> Option<Vec<u8>> {
    let mut e = p.to_vec();
    while let Some(last) = e.last_mut() {
        if *last != 0xff {
            *last += 1;
            return Some(e);
        }
        e.pop();
    }
    None
}

impl WatchRange {
    /// Every key under `p` (a whole subtree).
    pub fn prefix(p: &[u8]) -> Self {
        Self { lo: p.to_vec(), hi: upper(p) }
    }

    /// Exactly one key.
    pub fn point(key: &[u8]) -> Self {
        let mut hi = key.to_vec();
        hi.push(0);
        Self { lo: key.to_vec(), hi: Some(hi) }
    }

    /// Interval overlap — both directions. For two *prefix* ranges this is
    /// exactly `a.starts_with(b) || b.starts_with(a)`: prefix ranges either
    /// nest or are disjoint.
    pub fn overlaps(&self, o: &WatchRange) -> bool {
        let a_before_o_end = o.hi.as_ref().map_or(true, |h| self.lo < *h);
        let o_before_a_end = self.hi.as_ref().map_or(true, |h| o.lo < *h);
        a_before_o_end && o_before_a_end
    }
}

/// Two prefix ranges either nest or are disjoint — so this one-liner *is*
/// interval overlap for the prefix-prefix case.
pub fn prefix_overlap(a: &[u8], b: &[u8]) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// The longest *point* prefix of a query's path: walk `Field`/`Key`/`Index`
/// steps while they resolve, stop at the first collecting/positional nav,
/// at anything that fails to resolve (deque index, leaf interior), and
/// watch everything under that point. Returns the prefix bytes (empty =
/// the root, which overlaps every write).
pub fn watch_prefix(schema: &Shape, namespace: Option<&str>, navs: &[Nav]) -> Vec<u8> {
    let mut best = match namespace {
        Some(ns) => {
            let mut p = Vec::new();
            crate::codec::put_segment(&mut p, ns.as_bytes());
            p
        }
        None => Vec::new(),
    };
    for n in 0..navs.len() {
        match &navs[n] {
            Nav::Field(_) | Nav::Key(_) | Nav::Index(_) => {}
            _ => break,
        }
        match dynpath::resolve(schema, namespace, &navs[..=n]) {
            Ok(loc) => best = loc.prefix,
            Err(_) => break,
        }
    }
    best
}

/// What one committed write touched, as a key range. Point ops
/// (`Put`/`Delete`/`Merge`) are exact keys; subtree ops (`DeletePrefix`,
/// collection pushes) are whole-prefix ranges — conservative, never missed.
pub fn write_range(w: &Write) -> WatchRange {
    match w.op() {
        Op::Put { key, .. } | Op::Delete { key } | Op::Merge { key, .. } => {
            WatchRange::point(key)
        }
        Op::DeletePrefix { prefix }
        | Op::ListPush { prefix, .. }
        | Op::DequePushBack { prefix, .. }
        | Op::DequePushFront { prefix, .. } => WatchRange::prefix(prefix),
    }
}

/// Did any write in this commit touch anything under the watched prefix?
/// Both directions checked — a write above the watcher still wakes her.
pub fn watch_hit(watch: &[u8], writes: &[Write]) -> bool {
    let read = WatchRange::prefix(watch);
    writes.iter().any(|w| write_range(w).overlaps(&read))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ciborium::Value;

    fn shape() -> Shape {
        Shape::record(vec![
            (
                "users".into(),
                Shape::map(Shape::record(vec![
                    ("name".into(), Shape::Leaf),
                    ("banned".into(), Shape::Leaf),
                ])),
            ),
            ("count".into(), Shape::Sum),
            ("title".into(), Shape::Leaf),
        ])
    }

    fn t(s: &str) -> Value {
        Value::Text(s.into())
    }

    #[test]
    fn delete_parent_hits_child_watch() {
        let s = shape();
        let watch_navs = dynpath::navs_for(&s, &[t("users"), t("bob"), t("banned")]).unwrap();
        let watch = watch_prefix(&s, None, &watch_navs);
        let del_navs = dynpath::navs_for(&s, &[t("users"), t("bob")]).unwrap();
        let w = dynpath::delete(&s, None, &del_navs).unwrap();
        assert!(watch_hit(&watch, std::slice::from_ref(&w)));
    }

    #[test]
    fn sibling_stays_disjoint() {
        let s = shape();
        let watch_navs = dynpath::navs_for(&s, &[t("users"), t("bob"), t("name")]).unwrap();
        let watch = watch_prefix(&s, None, &watch_navs);
        let navs = dynpath::navs_for(&s, &[t("users"), t("bobby"), t("banned")]).unwrap();
        let ws = dynpath::put(&s, None, &navs, &Value::Bool(true)).unwrap();
        assert!(!watch_hit(&watch, &ws));
    }

    #[test]
    fn collecting_watch_covers_whole_collection() {
        use crate::{Expr, Predicate};
        let s = shape();
        let q = vec![
            Nav::Field("users".into()),
            Nav::Where(Predicate::Eq(
                Expr::Field("banned".into()),
                Expr::Lit(Value::Bool(true)),
            )),
            Nav::Field("name".into()),
        ];
        let watch = watch_prefix(&s, None, &q);
        // stops at the Where: watches the whole users map
        let whole = dynpath::resolve(&s, None, &[Nav::Field("users".into())]).unwrap().prefix;
        assert_eq!(watch, whole);
        // a new banned user wakes her
        let navs = dynpath::navs_for(&s, &[t("users"), t("carol"), t("banned")]).unwrap();
        let ws = dynpath::put(&s, None, &navs, &Value::Bool(true)).unwrap();
        assert!(watch_hit(&watch, &ws));
    }

    #[test]
    fn sum_merge_hits_point_watch_not_sibling() {
        let s = shape();
        let navs = dynpath::navs_for(&s, &[t("count")]).unwrap();
        let watch = watch_prefix(&s, None, &navs);
        let w = dynpath::add(&s, None, &navs, &ciborium::Value::Integer(1.into())).unwrap();
        assert!(watch_hit(&watch, std::slice::from_ref(&w)));
        let other = dynpath::navs_for(&s, &[t("title")]).unwrap();
        let other_watch = watch_prefix(&s, None, &other);
        assert!(!watch_hit(&other_watch, std::slice::from_ref(&w)));
    }

    #[test]
    fn root_watches_everything() {
        let s = shape();
        let navs = dynpath::navs_for(&s, &[t("title")]).unwrap();
        let ws = dynpath::put(&s, None, &navs, &t("hi")).unwrap();
        assert!(watch_hit(&[], &ws));
        assert!(prefix_overlap(&[], &[0x01]));
    }
}
