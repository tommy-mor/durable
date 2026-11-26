//! Property tests: durable collections, ordered keys, and range/prefix/page
//! queries must match std `BTreeMap` / `BTreeSet` / `VecDeque` identically.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ops::Bound;

use durable::{
    Db, Deque, Dir, Durability, Durable, Key, KeyEncode, Leaf, List, Map, OrderedF64, Reverse, Set,
    Sum,
};
use proptest::prelude::*;

#[derive(Durable)]
#[allow(dead_code)]
struct Bag {
    str_map: Map<String, Leaf<i64>>,
    int_map: Map<i64, Leaf<i64>>,
    nested_map: Map<String, Map<i64, Leaf<bool>>>,
    tuple_map: Map<(String, i64), Leaf<i64>>,
    rev_scores: Map<(Reverse<OrderedF64>, String), Leaf<()>>,
    set: Set<String>,
    list: List<Leaf<i64>>,
    deque: Deque<Leaf<i64>>,
    total: Sum<i64>,
}

fn str_with_nuls() -> impl Strategy<Value = String> {
    prop_oneof![
        "[a-z]{0,5}",
        Just(String::new()),
        Just("\0".to_string()),
        Just("a\0".to_string()),
        Just("a\0b".to_string()),
        Just("a\x01".to_string()),
        Just("a".to_string()),
        Just("ab".to_string()),
        Just("zz".to_string()),
        Just("aaa".to_string()),
    ]
}

fn bound_strategy<T: Strategy>(inner: T) -> impl Strategy<Value = Bound<T::Value>>
where
    T::Value: std::fmt::Debug + Clone,
{
    (0u8..3, inner).prop_map(|(kind, v)| match kind {
        0 => Bound::Unbounded,
        1 => Bound::Included(v),
        _ => Bound::Excluded(v),
    })
}

proptest! {
    #[test]
    fn string_map_order_range_and_rev_match_btreemap(
        entries in proptest::collection::vec((str_with_nuls(), any::<i64>()), 0..40),
        lo in bound_strategy(str_with_nuls()),
        hi in bound_strategy(str_with_nuls()),
    ) {
        let db = Db::open_memory().unwrap();
        let map = Bag::root().str_map();
        let mut model = BTreeMap::new();

        db.txn(Durability::DisableWal, |tx| {
            for (k, v) in &entries {
                map.set(tx, k, v)?;
                model.insert(k.clone(), *v);
            }
            Ok(())
        }).unwrap();

        let r = db.read().unwrap();
        prop_assert_eq!(map.len(&r).unwrap(), model.len() as u64);
        // Full order must match BTreeMap EXACTLY without sorting!
        let want_all: Vec<(String, i64)> = model.iter().map(|(k, v)| (k.clone(), *v)).collect();
        prop_assert_eq!(map.iter(&r).unwrap(), want_all);
        prop_assert_eq!(map.first(&r).unwrap(), model.keys().next().cloned());
        prop_assert_eq!(map.last(&r).unwrap(), model.keys().next_back().cloned());

        // Compare range forward and reverse against BTreeMap::range.
        let inverted = match (&lo, &hi) {
            (Bound::Included(a), Bound::Included(b)) => a > b,
            (Bound::Included(a), Bound::Excluded(b))
            | (Bound::Excluded(a), Bound::Included(b))
            | (Bound::Excluded(a), Bound::Excluded(b)) => a >= b,
            _ => false,
        };
        let want: Vec<(String, i64)> = if inverted {
            Vec::new()
        } else {
            model
                .range::<String, _>((lo.as_ref(), hi.as_ref()))
                .map(|(k, v)| (k.clone(), *v))
                .collect()
        };
        let got_fwd: Vec<(String, i64)> = map
            .range(&r, (lo.clone(), hi.clone()))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let got_rev: Vec<(String, i64)> = map
            .range(&r, (lo.clone(), hi.clone()))
            .unwrap()
            .rev()
            .collect::<Result<_, _>>()
            .unwrap();
        prop_assert_eq!(&got_fwd, &want);
        let mut want_rev = want.clone();
        want_rev.reverse();
        prop_assert_eq!(got_rev, want_rev);
        prop_assert_eq!(map.count_range(&r, (lo, hi)).unwrap(), want.len() as u64);
    }

    #[test]
    fn signed_i64_map_order_and_bounds_match_btreemap(
        entries in proptest::collection::vec((any::<i64>(), any::<i64>()), 0..40),
        lo in bound_strategy(any::<i64>()),
        hi in bound_strategy(any::<i64>()),
    ) {
        let db = Db::open_memory().unwrap();
        let map = Bag::root().int_map();
        let mut model = BTreeMap::new();

        db.txn(Durability::DisableWal, |tx| {
            for (k, v) in &entries {
                map.set(tx, *k, v)?;
                model.insert(*k, *v);
            }
            Ok(())
        }).unwrap();

        let r = db.read().unwrap();
        let want_all: Vec<(i64, i64)> = model.iter().map(|(&k, &v)| (k, v)).collect();
        prop_assert_eq!(map.iter(&r).unwrap(), want_all);

        let inverted = match (&lo, &hi) {
            (Bound::Included(a), Bound::Included(b)) => a > b,
            (Bound::Included(a), Bound::Excluded(b))
            | (Bound::Excluded(a), Bound::Included(b))
            | (Bound::Excluded(a), Bound::Excluded(b)) => a >= b,
            _ => false,
        };
        let want: Vec<(i64, i64)> = if inverted {
            Vec::new()
        } else {
            model.range((lo, hi)).map(|(&k, &v)| (k, v)).collect()
        };
        let got: Vec<(i64, i64)> = map
            .range(&r, (lo, hi))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        prop_assert_eq!(got, want);
    }

    #[test]
    fn nested_map_range_dedup_and_remove_stay_prefix_disjoint(
        entries in proptest::collection::vec((str_with_nuls(), -10i64..10), 1..40),
        lo in bound_strategy(str_with_nuls()),
        hi in bound_strategy(str_with_nuls()),
    ) {
        let db = Db::open_memory().unwrap();
        let nested = Bag::root().nested_map();
        let mut model: BTreeMap<String, BTreeSet<i64>> = BTreeMap::new();

        db.txn(Durability::DisableWal, |tx| {
            for (u, vid) in &entries {
                nested.key(u).set(tx, *vid, &true)?;
                model.entry(u.clone()).or_default().insert(*vid);
            }
            Ok(())
        }).unwrap();

        let r = db.read().unwrap();
        let want_keys: Vec<String> = model.keys().cloned().collect();
        prop_assert_eq!(nested.keys(&r).unwrap(), want_keys);
        prop_assert_eq!(nested.len(&r).unwrap(), model.len() as u64);

        // Nested range on outer keys (including NUL-containing strings!)
        let inverted = match (&lo, &hi) {
            (Bound::Included(a), Bound::Included(b)) => a > b,
            (Bound::Included(a), Bound::Excluded(b))
            | (Bound::Excluded(a), Bound::Included(b))
            | (Bound::Excluded(a), Bound::Excluded(b)) => a >= b,
            _ => false,
        };
        let want_range: Vec<String> = if inverted {
            Vec::new()
        } else {
            model
                .range::<String, _>((lo.as_ref(), hi.as_ref()))
                .map(|(k, _)| k.clone())
                .collect()
        };
        let got_fwd: Vec<String> = nested
            .keys_range(&r, (lo.clone(), hi.clone()))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let got_rev: Vec<String> = nested
            .keys_range(&r, (lo, hi))
            .unwrap()
            .rev()
            .collect::<Result<_, _>>()
            .unwrap();
        prop_assert_eq!(&got_fwd, &want_range);
        let mut want_rev = want_range;
        want_rev.reverse();
        prop_assert_eq!(got_rev, want_rev);

        // Removing one outer key must never touch sibling keys (even "a" vs "a\0")!
        let victim = model.keys().next().unwrap().clone();
        model.remove(&victim);
        db.txn(Durability::DisableWal, |tx| nested.remove(tx, &victim)).unwrap();
        let r2 = db.read().unwrap();
        for (k, vs) in &model {
            let got_inner: Vec<i64> = nested.key(k).keys(&r2).unwrap();
            let want_inner: Vec<i64> = vs.iter().copied().collect();
            prop_assert_eq!(got_inner, want_inner);
        }
        prop_assert!(!nested.contains(&r2, &victim).unwrap());
    }

    #[test]
    fn cursor_pagination_covers_every_entry_once_in_both_directions(
        entries in proptest::collection::vec((any::<i64>(), any::<i64>()), 0..45),
        page_size in 1usize..10,
    ) {
        let db = Db::open_memory().unwrap();
        let map = Bag::root().int_map();
        let mut model = BTreeMap::new();
        db.txn(Durability::DisableWal, |tx| {
            for (k, v) in &entries {
                map.set(tx, *k, v)?;
                model.insert(*k, *v);
            }
            Ok(())
        }).unwrap();

        let r = db.read().unwrap();
        // Ascending full walk via cursor pagination
        let mut asc = Vec::new();
        let mut cursor: Option<i64> = None;
        loop {
            let (page, next) = map.page(&r, cursor, page_size, Dir::Asc).unwrap();
            asc.extend(page);
            match next {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        let want_asc: Vec<(i64, i64)> = model.iter().map(|(&k, &v)| (k, v)).collect();
        prop_assert_eq!(&asc, &want_asc);

        // Descending full walk via cursor pagination
        let mut desc = Vec::new();
        let mut cursor: Option<i64> = None;
        loop {
            let (page, next) = map.page(&r, cursor, page_size, Dir::Desc).unwrap();
            desc.extend(page);
            match next {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        let mut want_desc = want_asc;
        want_desc.reverse();
        prop_assert_eq!(desc, want_desc);
    }

    #[test]
    fn string_prefix_and_tuple_prefix_match_filter(
        entries in proptest::collection::vec((str_with_nuls(), -20i64..20, any::<i64>()), 0..40),
        pref in str_with_nuls(),
    ) {
        let db = Db::open_memory().unwrap();
        let root = Bag::root();
        let mut str_model = BTreeSet::new();
        let mut tup_model = BTreeMap::new();

        db.txn(Durability::DisableWal, |tx| {
            for (s, n, v) in &entries {
                root.set().insert(tx, s)?;
                root.tuple_map().set(tx, (s.as_str(), *n), v)?;
                str_model.insert(s.clone());
                tup_model.insert((s.clone(), *n), *v);
            }
            Ok(())
        }).unwrap();

        let r = db.read().unwrap();
        let got_str: Vec<String> = root.set().prefix(&r, &pref).unwrap().collect::<Result<_, _>>().unwrap();
        let want_str: Vec<String> = str_model.iter().filter(|s| s.starts_with(&pref)).cloned().collect();
        prop_assert_eq!(got_str, want_str);

        let got_tup: Vec<((String, i64), i64)> = root
            .tuple_map()
            .prefix(&r, (pref.as_str(),))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let want_tup: Vec<((String, i64), i64)> = tup_model
            .iter()
            .filter(|((s, _), _)| s == &pref)
            .map(|(k, &v)| (k.clone(), v))
            .collect();
        prop_assert_eq!(got_tup, want_tup);
    }

    #[test]
    fn ordered_f64_reverse_leaderboard_order(
        items in proptest::collection::vec((any::<f64>(), "[a-z]{1,4}"), 0..40),
    ) {
        let db = Db::open_memory().unwrap();
        let lb = Bag::root().rev_scores();
        let mut model = BTreeSet::new();
        db.txn(Durability::DisableWal, |tx| {
            for (score, name) in &items {
                if score.is_nan() {
                    continue;
                }
                let k = (Reverse(OrderedF64(*score)), name.clone());
                lb.set(tx, &k, &())?;
                model.insert(k);
            }
            Ok(())
        }).unwrap();

        let r = db.read().unwrap();
        let got: Vec<(Reverse<OrderedF64>, String)> = lb.keys(&r).unwrap();
        let want: Vec<(Reverse<OrderedF64>, String)> = model.into_iter().collect();
        prop_assert_eq!(got, want);
    }

    #[test]
    fn list_roundtrips_in_order(values in proptest::collection::vec(any::<i64>(), 0..50)) {
        let db = Db::open_memory().unwrap();
        let list = Bag::root().list();
        db.txn(Durability::DisableWal, |tx| {
            for v in &values {
                list.push(tx, v)?;
            }
            Ok(())
        }).unwrap();

        let r = db.read().unwrap();
        prop_assert_eq!(list.len(&r).unwrap(), values.len() as u64);
        prop_assert_eq!(list.iter(&r).unwrap(), values);
    }

    #[test]
    fn deque_matches_vecdeque(ops in proptest::collection::vec(any::<(bool, i64)>(), 0..60)) {
        let db = Db::open_memory().unwrap();
        let dq = Bag::root().deque();
        let mut model: VecDeque<i64> = VecDeque::new();

        db.txn(Durability::DisableWal, |tx| {
            for (front, v) in &ops {
                if *front {
                    dq.push_front(tx, v)?;
                    model.push_front(*v);
                } else {
                    dq.push_back(tx, v)?;
                    model.push_back(*v);
                }
            }
            Ok(())
        }).unwrap();

        let r = db.read().unwrap();
        prop_assert_eq!(dq.len(&r).unwrap(), model.len() as u64);
        prop_assert_eq!(dq.iter(&r).unwrap(), Vec::from(model.clone()));

        db.txn(Durability::DisableWal, |tx| {
            let mut toggle = true;
            while !model.is_empty() {
                if toggle {
                    assert_eq!(dq.pop_front(tx)?, model.pop_front());
                } else {
                    assert_eq!(dq.pop_back(tx)?, model.pop_back());
                }
                toggle = !toggle;
            }
            assert!(dq.is_empty(tx)?);
            assert_eq!(dq.pop_front(tx)?, None);
            Ok(())
        }).unwrap();
    }

    #[test]
    fn sum_equals_total_of_deltas(deltas in proptest::collection::vec(-1000i64..1000, 0..50)) {
        let db = Db::open_memory().unwrap();
        let total = Bag::root().total();
        db.txn(Durability::DisableWal, |tx| {
            for d in &deltas {
                total.add(tx, *d)?;
            }
            Ok(())
        }).unwrap();
        prop_assert_eq!(total.get(&db).unwrap(), deltas.iter().sum::<i64>());
    }

    #[test]
    fn key_encoding_roundtrip_and_prefix_disjoint(a in str_with_nuls(), b in str_with_nuls()) {
        let ea = a.to_key_bytes();
        let eb = b.to_key_bytes();
        let (da, ua) = String::decode_key(&ea).unwrap();
        prop_assert_eq!((da, ua), (a.clone(), ea.len()));
        prop_assert_eq!(a.cmp(&b), ea.cmp(&eb));
        if a != b {
            prop_assert!(!eb.starts_with(&ea));
            prop_assert!(!ea.starts_with(&eb));
        }
    }
}
