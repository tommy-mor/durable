//! End-to-end tests for the durable paths-as-data API.

use durable::{Db, Durability, Durable, Deque, Leaf, List, Map, Op, Sum};
use serde::{Deserialize, Serialize};
use tempfile::TempDir;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
struct Vote {
    a: String,
    b: String,
    ratio: i32,
}

/// A scope's local ranking state — the kind of thing that used to be one CBOR
/// blob, now addressable field-by-field and key-by-key.
#[derive(Durable)]
#[allow(dead_code)]
struct GroupState {
    edges: Map<(u32, u32), Sum<f64>>,
    voted_pairs: Map<(u32, u32), Leaf<bool>>,
    recent_votes: Deque<Leaf<Vote>>,
    item_count: Sum<i64>,
}

#[derive(Durable)]
#[allow(dead_code)]
struct Store {
    scopes: Map<String, GroupState>,
    nodes: Map<String, Leaf<String>>,
    log: List<Leaf<u64>>,
}

fn open() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path()).unwrap();
    (dir, db)
}

#[test]
fn leaf_set_get_delete() {
    let (_dir, db) = open();
    let root = Store::root();
    let k = "reddit.com/r/rust".to_string();

    assert_eq!(root.nodes().key(&k).get(&db).unwrap(), None);
    db.run(root.nodes().key(&k).set(&"Rust".to_string()), Durability::SyncWal)
        .unwrap();
    assert_eq!(root.nodes().key(&k).get(&db).unwrap(), Some("Rust".to_string()));

    db.run(root.nodes().key(&k).delete(), Durability::SyncWal).unwrap();
    assert_eq!(root.nodes().key(&k).get(&db).unwrap(), None);
}

#[test]
fn sum_accumulates_with_blind_merges() {
    let (_dir, db) = open();
    let edges = Store::root().scopes().key(&"s".to_string()).edges();
    let e = (3u32, 7u32);

    // Several blind merges in one atomic batch — no reads involved.
    db.apply(
        &[
            edges.key(&e).add(2.0),
            edges.key(&e).add(1.0),
            edges.key(&e).add(0.5),
        ],
        Durability::SyncWal,
    )
    .unwrap();
    assert_eq!(edges.key(&e).get(&db).unwrap(), 3.5);

    // Negative delta decrements; absent key reads as zero.
    db.run(edges.key(&e).add(-1.5), Durability::SyncWal).unwrap();
    assert_eq!(edges.key(&e).get(&db).unwrap(), 2.0);
    assert_eq!(edges.key(&(9, 9)).get(&db).unwrap(), 0.0);
}

#[test]
fn sum_set_then_merge() {
    let (_dir, db) = open();
    let count = Store::root().scopes().key(&"s".to_string()).item_count();
    db.run(count.set(10), Durability::SyncWal).unwrap();
    db.run(count.add(5), Durability::SyncWal).unwrap();
    assert_eq!(count.get(&db).unwrap(), 15);
}

#[test]
fn reified_writes_are_inspectable_data() {
    let edges = Store::root().scopes().key(&"s".to_string()).edges();
    let merge = edges.key(&(1u32, 2u32)).add(1.0);
    assert!(matches!(merge.op(), Op::Merge { .. }));

    let put = Store::root().nodes().key(&"x".to_string()).set(&"y".to_string());
    assert!(matches!(put.op(), Op::Put { .. }));

    let clear = Store::root().scopes().clear();
    assert!(matches!(clear.op(), Op::DeletePrefix { .. }));
}

#[test]
fn point_update_touches_only_its_own_key() {
    let (_dir, db) = open();
    let root = Store::root();
    let rust = root.scopes().key(&"rust".to_string());
    let python = root.scopes().key(&"python".to_string());

    // Populate two scopes with several edges and some recent votes.
    let mut batch = db.batch();
    for j in 0..5u32 {
        batch.write(rust.edges().key(&(0, j)).add(j as f64 + 1.0));
        batch.write(python.edges().key(&(0, j)).add(100.0));
    }
    batch
        .push_back(
            &rust.recent_votes(),
            &Vote { a: "a".into(), b: "b".into(), ratio: 2 },
        )
        .unwrap();
    batch.commit().unwrap();

    // A single precise update to one edge in `rust`.
    db.run(rust.edges().key(&(0, 2)).add(10.0), Durability::SyncWal)
        .unwrap();

    // Only that edge changed.
    assert_eq!(rust.edges().key(&(0, 2)).get(&db).unwrap(), 13.0);
    assert_eq!(rust.edges().key(&(0, 0)).get(&db).unwrap(), 1.0);
    assert_eq!(rust.edges().key(&(0, 4)).get(&db).unwrap(), 5.0);
    // The other scope is entirely untouched.
    for j in 0..5u32 {
        assert_eq!(python.edges().key(&(0, j)).get(&db).unwrap(), 100.0);
    }
    // And the unrelated recent_votes deque is intact.
    assert_eq!(rust.recent_votes().len(&db).unwrap(), 1);
}

#[test]
fn map_keys_len_contains_entries() {
    let (_dir, db) = open();
    let nodes = Store::root().nodes();
    db.apply(
        &[
            nodes.key(&"a".to_string()).set(&"1".to_string()),
            nodes.key(&"b".to_string()).set(&"2".to_string()),
            nodes.key(&"c".to_string()).set(&"3".to_string()),
        ],
        Durability::SyncWal,
    )
    .unwrap();

    let mut keys = nodes.keys(&db).unwrap();
    keys.sort();
    assert_eq!(keys, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
    assert_eq!(nodes.len(&db).unwrap(), 3);
    assert!(nodes.contains(&db, &"b".to_string()).unwrap());
    assert!(!nodes.contains(&db, &"z".to_string()).unwrap());

    let mut pairs = nodes.iter(&db).unwrap();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "2".to_string()),
            ("c".to_string(), "3".to_string()),
        ]
    );
}

#[test]
fn map_keys_dedup_across_nested_subkeys() {
    // A map whose values are nested structs has many physical keys per logical
    // key; `keys()`/`len()` must dedup to distinct logical keys.
    let (_dir, db) = open();
    let scopes = Store::root().scopes();
    let rust = scopes.key(&"rust".to_string());

    let mut batch = db.batch();
    batch.write(rust.edges().key(&(0, 1)).add(1.0));
    batch.write(rust.edges().key(&(0, 2)).add(1.0));
    batch.write(rust.item_count().add(3));
    batch
        .push_back(&rust.recent_votes(), &Vote { a: "a".into(), b: "b".into(), ratio: 1 })
        .unwrap();
    batch.write(scopes.key(&"python".to_string()).item_count().add(1));
    batch.commit().unwrap();

    let mut keys = scopes.keys(&db).unwrap();
    keys.sort();
    assert_eq!(keys, vec!["python".to_string(), "rust".to_string()]);
    assert_eq!(scopes.len(&db).unwrap(), 2);
}

#[test]
fn map_clear_deletes_subtree_only() {
    let (_dir, db) = open();
    let rust = Store::root().scopes().key(&"rust".to_string());

    let mut batch = db.batch();
    batch.write(rust.edges().key(&(0, 1)).add(1.0));
    batch.write(rust.edges().key(&(0, 2)).add(2.0));
    batch.write(rust.item_count().add(5));
    batch.commit().unwrap();

    // Clear only the edges sub-map.
    db.run(rust.edges().clear(), Durability::SyncWal).unwrap();

    assert_eq!(rust.edges().len(&db).unwrap(), 0);
    assert_eq!(rust.edges().key(&(0, 1)).get(&db).unwrap(), 0.0);
    // Sibling field under the same scope is untouched.
    assert_eq!(rust.item_count().get(&db).unwrap(), 5);
}

#[test]
fn transform_values_decays_all_edges_in_one_batch() {
    let (_dir, db) = open();
    let edges = Store::root().scopes().key(&"rust".to_string()).edges();

    let mut batch = db.batch();
    for j in 1..=4u32 {
        batch.write(edges.key(&(0, j)).add(j as f64 * 10.0));
    }
    batch.commit().unwrap();

    // Decay every edge by half, dropping any that fall to/under 5.0 — built as
    // reified writes from one scan, applied atomically.
    let writes = edges
        .transform_values(&db, |_k, w| {
            let decayed = w * 0.5;
            if decayed <= 5.0 {
                None
            } else {
                Some(decayed)
            }
        })
        .unwrap();
    db.apply(&writes, Durability::SyncWal).unwrap();

    assert_eq!(edges.key(&(0, 1)).get(&db).unwrap(), 0.0); // 10*0.5=5.0 -> dropped
    assert_eq!(edges.key(&(0, 2)).get(&db).unwrap(), 10.0);
    assert_eq!(edges.key(&(0, 3)).get(&db).unwrap(), 15.0);
    assert_eq!(edges.key(&(0, 4)).get(&db).unwrap(), 20.0);
    assert_eq!(edges.len(&db).unwrap(), 3);
}

#[test]
fn list_push_pop_iter() {
    let (_dir, db) = open();
    let log = Store::root().log();

    assert!(log.is_empty(&db).unwrap());
    assert_eq!(log.push(&db, &10).unwrap(), 0);
    assert_eq!(log.push(&db, &20).unwrap(), 1);
    assert_eq!(log.push(&db, &30).unwrap(), 2);

    assert_eq!(log.len(&db).unwrap(), 3);
    assert_eq!(log.get(&db, 1).unwrap(), Some(20));
    assert_eq!(log.get(&db, 3).unwrap(), None);
    assert_eq!(log.iter(&db).unwrap(), vec![10, 20, 30]);

    assert_eq!(log.pop(&db).unwrap(), Some(30));
    assert_eq!(log.len(&db).unwrap(), 2);
    assert_eq!(log.iter(&db).unwrap(), vec![10, 20]);
}

#[test]
fn batched_list_pushes_get_contiguous_indices() {
    let (_dir, db) = open();
    let log = Store::root().log();

    let mut batch = db.batch();
    batch.push(&log, &1).unwrap();
    batch.push(&log, &2).unwrap();
    batch.push(&log, &3).unwrap();
    batch.commit().unwrap();
    assert_eq!(log.iter(&db).unwrap(), vec![1, 2, 3]);

    // A second batch continues from the persisted length.
    let mut batch = db.batch();
    batch.push(&log, &4).unwrap();
    batch.push(&log, &5).unwrap();
    batch.commit().unwrap();
    assert_eq!(log.iter(&db).unwrap(), vec![1, 2, 3, 4, 5]);
    assert_eq!(log.len(&db).unwrap(), 5);
}

#[test]
fn deque_behaves_like_a_double_ended_queue() {
    let (_dir, db) = open();
    let dq = Store::root().scopes().key(&"s".to_string()).recent_votes();
    let v = |n: i32| Vote { a: format!("a{n}"), b: format!("b{n}"), ratio: n };

    dq.push_back(&db, &v(1)).unwrap();
    dq.push_back(&db, &v(2)).unwrap();
    dq.push_front(&db, &v(0)).unwrap();

    assert_eq!(dq.len(&db).unwrap(), 3);
    assert_eq!(dq.iter(&db).unwrap(), vec![v(0), v(1), v(2)]);
    assert_eq!(dq.front(&db).unwrap(), Some(v(0)));
    assert_eq!(dq.back(&db).unwrap(), Some(v(2)));

    assert_eq!(dq.pop_front(&db).unwrap(), Some(v(0)));
    assert_eq!(dq.pop_back(&db).unwrap(), Some(v(2)));
    assert_eq!(dq.iter(&db).unwrap(), vec![v(1)]);
    assert_eq!(dq.pop_front(&db).unwrap(), Some(v(1)));
    assert_eq!(dq.pop_front(&db).unwrap(), None);
    assert!(dq.is_empty(&db).unwrap());
}

#[test]
fn deque_supports_capped_recent_window() {
    // The motivating use case: keep only the most recent N votes, O(1) per insert.
    let (_dir, db) = open();
    let dq = Store::root().scopes().key(&"s".to_string()).recent_votes();
    const CAP: u64 = 3;

    for n in 0..10 {
        dq.push_back(&db, &Vote { a: format!("{n}"), b: "x".into(), ratio: n })
            .unwrap();
        while dq.len(&db).unwrap() > CAP {
            dq.pop_front(&db).unwrap();
        }
    }

    let kept = dq.iter(&db).unwrap();
    assert_eq!(kept.len(), 3);
    assert_eq!(kept.iter().map(|v| v.ratio).collect::<Vec<_>>(), vec![7, 8, 9]);
}

#[test]
fn deque_truncate_back_caps_length_keeping_front() {
    let (_dir, db) = open();
    let dq = Store::root().scopes().key(&"s".to_string()).recent_votes();
    for n in 0..10 {
        dq.push_back(&db, &Vote { a: format!("{n}"), b: "x".into(), ratio: n })
            .unwrap();
    }
    // Keep only the 3 oldest at front (drop the back/newest beyond cap).
    dq.truncate_back(&db, 3, Durability::SyncWal).unwrap();
    let kept = dq.iter(&db).unwrap();
    assert_eq!(kept.iter().map(|v| v.ratio).collect::<Vec<_>>(), vec![0, 1, 2]);

    // Truncating to a larger-or-equal cap is a no-op.
    dq.truncate_back(&db, 10, Durability::SyncWal).unwrap();
    assert_eq!(dq.len(&db).unwrap(), 3);
}

#[test]
fn one_batch_commits_all_or_nothing_and_persists() {
    let dir = TempDir::new().unwrap();
    let rust_key = "rust".to_string();
    {
        let db = Db::open(dir.path()).unwrap();
        let rust = Store::root().scopes().key(&rust_key);
        // A "vote" as one atomic batch: two edge merges, a pair flag, a recent
        // vote, and a counter — all distinct keys, one WAL flush.
        let mut batch = db.batch();
        batch.write(rust.edges().key(&(0, 1)).add(2.0));
        batch.write(rust.edges().key(&(1, 0)).add(1.0));
        batch.write(rust.voted_pairs().key(&(0, 1)).set(&true));
        batch
            .push_back(&rust.recent_votes(), &Vote { a: "0".into(), b: "1".into(), ratio: 2 })
            .unwrap();
        batch.write(rust.item_count().add(2));
        batch.commit().unwrap();
    }

    // Reopen: SyncWal data survives.
    let db = Db::open(dir.path()).unwrap();
    let rust = Store::root().scopes().key(&rust_key);
    assert_eq!(rust.edges().key(&(0, 1)).get(&db).unwrap(), 2.0);
    assert_eq!(rust.edges().key(&(1, 0)).get(&db).unwrap(), 1.0);
    assert_eq!(rust.voted_pairs().key(&(0, 1)).get(&db).unwrap(), Some(true));
    assert_eq!(rust.recent_votes().len(&db).unwrap(), 1);
    assert_eq!(rust.item_count().get(&db).unwrap(), 2);
}

#[test]
fn disable_wal_visible_within_session() {
    let (_dir, db) = open();
    let count = Store::root().scopes().key(&"s".to_string()).item_count();
    db.run(count.add(7), Durability::DisableWal).unwrap();
    assert_eq!(count.get(&db).unwrap(), 7);
}

#[test]
fn wal_only_durability_writes() {
    let (_dir, db) = open();
    let node = Store::root().nodes().key(&"k".to_string());
    db.run(node.set(&"v".to_string()), Durability::WalOnly).unwrap();
    assert_eq!(node.get(&db).unwrap(), Some("v".to_string()));
}

#[test]
fn namespaced_roots_do_not_collide() {
    let (_dir, db) = open();
    let a = Store::namespaced("a");
    let b = Store::namespaced("b");
    db.run(a.nodes().key(&"k".to_string()).set(&"av".to_string()), Durability::SyncWal)
        .unwrap();
    db.run(b.nodes().key(&"k".to_string()).set(&"bv".to_string()), Durability::SyncWal)
        .unwrap();

    assert_eq!(a.nodes().key(&"k".to_string()).get(&db).unwrap(), Some("av".to_string()));
    assert_eq!(b.nodes().key(&"k".to_string()).get(&db).unwrap(), Some("bv".to_string()));
}

#[test]
fn persistence_across_reopen_for_all_collection_kinds() {
    let dir = TempDir::new().unwrap();
    {
        let db = Db::open(dir.path()).unwrap();
        let root = Store::root();
        let s = root.scopes().key(&"s".to_string());
        db.run(root.nodes().key(&"n".to_string()).set(&"N".to_string()), Durability::SyncWal)
            .unwrap();
        root.log().push(&db, &42).unwrap();
        db.run(s.edges().key(&(1, 2)).add(9.0), Durability::SyncWal).unwrap();
        s.recent_votes()
            .push_back(&db, &Vote { a: "a".into(), b: "b".into(), ratio: 3 })
            .unwrap();
    }
    let db = Db::open(dir.path()).unwrap();
    let root = Store::root();
    let s = root.scopes().key(&"s".to_string());
    assert_eq!(root.nodes().key(&"n".to_string()).get(&db).unwrap(), Some("N".to_string()));
    assert_eq!(root.log().iter(&db).unwrap(), vec![42]);
    assert_eq!(s.edges().key(&(1, 2)).get(&db).unwrap(), 9.0);
    assert_eq!(
        s.recent_votes().front(&db).unwrap(),
        Some(Vote { a: "a".into(), b: "b".into(), ratio: 3 })
    );
}
