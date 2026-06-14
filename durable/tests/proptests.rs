//! Property tests: durable collections must behave like their std analogues.

use std::collections::{BTreeMap, VecDeque};

use durable::{Db, Durability, Durable, Deque, Leaf, List, Map, Sum};
use proptest::prelude::*;
use tempfile::TempDir;

#[derive(Durable)]
#[allow(dead_code)]
struct Bag {
    map: Map<String, Leaf<i64>>,
    list: List<Leaf<i64>>,
    deque: Deque<Leaf<i64>>,
    total: Sum<i64>,
}

fn open() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path()).unwrap();
    (dir, db)
}

proptest! {
    #[test]
    fn map_matches_btreemap(entries in proptest::collection::vec((".*", any::<i64>()), 0..40)) {
        let (_dir, db) = open();
        let map = Bag::root().map();
        let mut model = BTreeMap::new();

        let mut batch = db.batch();
        for (k, v) in &entries {
            batch.write(map.key(k).set(v));
            model.insert(k.clone(), *v);
        }
        batch.commit_with(Durability::WalOnly).unwrap();

        prop_assert_eq!(map.len(&db).unwrap(), model.len());
        for (k, v) in &model {
            prop_assert_eq!(map.get(&db, k).unwrap(), Some(*v));
        }
        let mut got = map.iter(&db).unwrap();
        got.sort();
        let mut want: Vec<(String, i64)> = model.into_iter().collect();
        want.sort();
        prop_assert_eq!(got, want);
    }

    #[test]
    fn list_roundtrips_in_order(values in proptest::collection::vec(any::<i64>(), 0..50)) {
        let (_dir, db) = open();
        let list = Bag::root().list();
        let mut batch = db.batch();
        for v in &values {
            batch.push(&list, v).unwrap();
        }
        batch.commit_with(Durability::WalOnly).unwrap();

        prop_assert_eq!(list.len(&db).unwrap(), values.len() as u64);
        prop_assert_eq!(list.iter(&db).unwrap(), values);
    }

    #[test]
    fn deque_matches_vecdeque(ops in proptest::collection::vec(any::<(bool, i64)>(), 0..60)) {
        let (_dir, db) = open();
        let dq = Bag::root().deque();
        let mut model: VecDeque<i64> = VecDeque::new();

        for (front, v) in &ops {
            if *front {
                dq.push_front(&db, v).unwrap();
                model.push_front(*v);
            } else {
                dq.push_back(&db, v).unwrap();
                model.push_back(*v);
            }
        }
        prop_assert_eq!(dq.len(&db).unwrap(), model.len() as u64);
        prop_assert_eq!(dq.iter(&db).unwrap(), Vec::from(model.clone()));

        // Drain alternately from both ends.
        let mut toggle = true;
        while !model.is_empty() {
            if toggle {
                prop_assert_eq!(dq.pop_front(&db).unwrap(), model.pop_front());
            } else {
                prop_assert_eq!(dq.pop_back(&db).unwrap(), model.pop_back());
            }
            toggle = !toggle;
        }
        prop_assert!(dq.is_empty(&db).unwrap());
        prop_assert_eq!(dq.pop_front(&db).unwrap(), None);
    }

    #[test]
    fn sum_equals_total_of_deltas(deltas in proptest::collection::vec(-1000i64..1000, 0..50)) {
        let (_dir, db) = open();
        let total = Bag::root().total();
        let writes: Vec<_> = deltas.iter().map(|d| total.add(*d)).collect();
        db.apply(&writes, Durability::WalOnly).unwrap();
        prop_assert_eq!(total.get(&db).unwrap(), deltas.iter().sum::<i64>());
    }
}
